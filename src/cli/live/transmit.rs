//! `transmit` — read EditableImage/ImageLabel/ImageButton/MeshPart texture
//! pixels out of Studio and write local, verified PNG files (transmit.json).
//!
//! One `transmit_prepare` op runs in exactly one mode — `source` (a render
//! script executed in the plugin's real Studio environment, eval-trust) or
//! `paths` (named image-bearing instances) — and answers
//! `{items, failures}`, both arrays always present, at most 16 items per
//! prepare. Each item is a standard capture session: the CLI re-checks its
//! metadata against the capture limits, pumps it through the shared
//! `capture_read` chunk pump with SHA-256 verification, writes a locally
//! decoded-back PNG, and closes it with `capture_close` whether or not the
//! pump succeeded. Pixels never touch stdout.
//!
//! `--from` is resolved client-side: the subtree is searched for image-like
//! instances through the `find` op *after* any `--source` batch ran (the
//! render script may populate that subtree), and the results join the
//! explicit `--path` list in a second, sequential `paths` batch. Batches are
//! never concurrent, and path batches are chunked at the 16-item allowance.
//!
//! Failures are per-item: the batch continues, every failure is reported,
//! and the exit is non-zero only when nothing was written at all.

use anyhow::{bail, Context, Result};
use clap::Parser;
use colored::Colorize;
use serde_json::{json, Value};
use std::{collections::HashSet, fs, path::PathBuf};

use crate::{
	cli::client::{print_json, Client, Targeting},
	cli::live::capture,
	cli::live::transfer::human_size,
	ext::PathExt,
	wsync_info, wsync_warn,
};

/// The plugin's per-prepare item allowance — path batches are chunked here
const MAX_ITEMS_PER_PREPARE: usize = 16;

/// Default prepare deadline: a render script plus up to 16 image reads
const PREPARE_TIMEOUT_MS: u64 = 60_000;

/// Classes the `--from` walk captures — the image-like set transmit.json
/// documents
const IMAGE_CLASSES: [&str; 4] = ["EditableImage", "ImageLabel", "ImageButton", "MeshPart"];

/// Run an optional Studio render script and write image pixels from Studio
/// as local, verified PNG files
#[derive(Parser)]
pub struct Transmit {
	#[command(flatten)]
	targeting: Targeting,

	/// Luau render source executed in Studio (eval trust — inspect scripts
	/// first)
	#[arg(long, value_name = "LUAU", conflicts_with = "source_file")]
	source: Option<String>,

	/// Read the render source from a file
	#[arg(long = "source-file", value_name = "FILE", conflicts_with = "source")]
	source_file: Option<PathBuf>,

	/// Studio subtree walked for image-like instances after the source runs
	#[arg(long, value_name = "STUDIO-PATH")]
	from: Option<String>,

	/// Studio path of an image-bearing instance (repeatable)
	#[arg(long = "path", value_name = "STUDIO-PATH")]
	paths: Vec<String>,

	/// Output file (single image) or directory (multiple images)
	#[arg(short, long, value_name = "FILE-OR-DIR")]
	output: PathBuf,

	/// Seconds each prepare may run (default 60)
	#[arg(long, value_name = "SECONDS", value_parser = clap::value_parser!(u64).range(1..=600))]
	timeout: Option<u64>,

	/// Print machine-readable JSON (never pixel data)
	#[arg(long)]
	raw: bool,
}

/// One item's final report line
struct ItemReport {
	ok: bool,
	name: String,
	studio_path: Option<String>,
	file: Option<PathBuf>,
	width: u64,
	height: u64,
	bytes: u64,
	sha256: Option<String>,
	error: Option<String>,
}

impl ItemReport {
	fn to_json(&self) -> Value {
		json!({
			"ok": self.ok,
			"name": self.name,
			"path": self.studio_path,
			"file": self.file.as_ref().map(|file| file.to_string()),
			"width": self.width,
			"height": self.height,
			"bytes": self.bytes,
			"sha256": self.sha256,
			"error": self.error,
		})
	}
}

impl Transmit {
	pub fn main(self) -> Result<()> {
		let source = match (&self.source, &self.source_file) {
			(Some(source), None) => Some(source.clone()),
			(None, Some(path)) => Some(
				fs::read_to_string(path)
					.with_context(|| format!("Failed to read the --source-file {}", path.display()))?,
			),
			(None, None) => None,
			(Some(_), Some(_)) => unreachable!("clap enforces the conflict"),
		};

		if source.is_none() && self.from.is_none() && self.paths.is_empty() {
			bail!("Nothing to transmit — pass --source/--source-file, --from, or --path");
		}

		let client = Client::connect(&self.targeting)?;
		let timeout_ms = self.timeout.map_or(PREPARE_TIMEOUT_MS, |seconds| seconds * 1000);

		let mut reports: Vec<ItemReport> = Vec::new();
		let mut used_names: HashSet<String> = HashSet::new();
		// Multi-item layout is settled once the total item count is known;
		// items stream into it batch by batch
		let mut layout = Layout::Undecided;

		// Batch 1: the render script (its items are pumped and closed before
		// anything else happens, so two batches never hold sessions at once)
		if let Some(source) = &source {
			let prepared = client
				.request_with_timeout(
					"transmit_prepare",
					json!({ "source": source, "timeoutMs": timeout_ms }),
					timeout_ms,
				)?
				.into_value(self.raw)?;

			// A pending --from walk or --path list may add items later, so a
			// single source item cannot claim a single-file layout yet
			let may_follow = self.from.is_some() || !self.paths.is_empty();

			self.consume_batch(
				&client,
				&prepared,
				may_follow,
				&mut reports,
				&mut used_names,
				&mut layout,
			)?;
		}

		// The `--from` walk runs after the source, because the script may
		// have populated that subtree
		let mut path_list: Vec<String> = Vec::new();
		let mut seen: HashSet<String> = HashSet::new();

		for path in &self.paths {
			if seen.insert(path.clone()) {
				path_list.push(path.clone());
			}
		}

		if let Some(from) = &self.from {
			for path in self.walk_from(&client, from)? {
				if seen.insert(path.clone()) {
					path_list.push(path);
				}
			}

			if path_list.is_empty() && source.is_none() {
				bail!(
					"No image-like instances ({}) found under {}",
					IMAGE_CLASSES.join("/"),
					from.bold()
				);
			}
		}

		// Batch 2..n: named instances, chunked at the plugin's allowance
		let chunks: Vec<&[String]> = path_list.chunks(MAX_ITEMS_PER_PREPARE).collect();

		for (index, chunk) in chunks.iter().enumerate() {
			let prepared = client
				.request_with_timeout(
					"transmit_prepare",
					json!({ "paths": chunk, "timeoutMs": timeout_ms }),
					timeout_ms,
				)?
				.into_value(self.raw)?;

			let may_follow = index + 1 < chunks.len();

			self.consume_batch(
				&client,
				&prepared,
				may_follow,
				&mut reports,
				&mut used_names,
				&mut layout,
			)?;
		}

		let written = reports.iter().filter(|report| report.ok).count();
		let failed = reports.len() - written;

		if self.raw {
			print_json(&json!({
				"ok": written > 0,
				"written": written,
				"failed": failed,
				"output": self.output.resolve()?.to_string(),
				"items": reports.iter().map(ItemReport::to_json).collect::<Vec<Value>>(),
			}));
		} else {
			for report in &reports {
				match (&report.file, &report.error) {
					(Some(file), None) => wsync_info!(
						"Transmitted {} {}x{} → {} ({})",
						report.name.bold(),
						report.width,
						report.height,
						file.to_string().bold(),
						human_size(report.bytes)
					),
					_ => wsync_warn!(
						"Failed {}{} — {}",
						report.name.bold(),
						report
							.studio_path
							.as_ref()
							.map(|path| format!(" ({path})"))
							.unwrap_or_default(),
						report.error.as_deref().unwrap_or("unknown error")
					),
				}
			}

			wsync_info!("{} image(s) written, {} failed", written.to_string().bold(), failed);
		}

		if reports.is_empty() {
			bail!("The transmit produced no images — the source returned none and no paths resolved");
		}

		// Per-item tolerance: partial success is a success with failures
		// listed; only a fully failed transmit exits non-zero
		if written == 0 {
			bail!("All {failed} transmit item(s) failed");
		}

		Ok(())
	}

	/// Pumps every item of one prepare answer into the output layout and
	/// folds the prepare-side failures into the report. Every item with a
	/// captureId is closed, pumped or not
	fn consume_batch(
		&self,
		client: &Client,
		prepared: &Value,
		may_follow: bool,
		reports: &mut Vec<ItemReport>,
		used_names: &mut HashSet<String>,
		layout: &mut Layout,
	) -> Result<()> {
		let empty = Vec::new();
		let items = prepared.get("items").and_then(Value::as_array).unwrap_or(&empty);
		let failures = prepared.get("failures").and_then(Value::as_array).unwrap_or(&empty);

		// The layout is decided on the first batch that carries items; a
		// refused layout must still release every prepared session
		let total_items = reports.iter().filter(|report| report.ok).count() + items.len();

		if let Err(err) = layout.grow(self, total_items, may_follow) {
			for item in items {
				if let Some(capture_id) = item.get("captureId").and_then(Value::as_str) {
					client.request("capture_close", json!({ "captureId": capture_id })).ok();
				}
			}

			return Err(err);
		}

		for item in items {
			let name = item.get("name").and_then(Value::as_str).unwrap_or("item").to_owned();
			let studio_path = item.get("path").and_then(Value::as_str).map(str::to_owned);
			let capture_id = item.get("captureId").and_then(Value::as_str).map(str::to_owned);
			let width = item.get("width").and_then(Value::as_u64).unwrap_or(0);
			let height = item.get("height").and_then(Value::as_u64).unwrap_or(0);

			let outcome = self.pull_item(client, item, &name, used_names, layout);

			// The plugin holds the pixels until told otherwise — close even
			// when the pump or the metadata check failed
			if let Some(capture_id) = &capture_id {
				client.request("capture_close", json!({ "captureId": capture_id })).ok();
			}

			match outcome {
				Ok((file, bytes, sha256)) => reports.push(ItemReport {
					ok: true,
					name,
					studio_path,
					file: Some(file),
					width,
					height,
					bytes,
					sha256: Some(sha256),
					error: None,
				}),
				Err(err) => reports.push(ItemReport {
					ok: false,
					name,
					studio_path,
					file: None,
					width,
					height,
					bytes: 0,
					sha256: None,
					error: Some(err.to_string()),
				}),
			}
		}

		for failure in failures {
			reports.push(ItemReport {
				ok: false,
				name: failure.get("name").and_then(Value::as_str).unwrap_or("item").to_owned(),
				studio_path: failure.get("path").and_then(Value::as_str).map(str::to_owned),
				file: None,
				width: 0,
				height: 0,
				bytes: 0,
				sha256: None,
				error: Some(
					failure
						.pointer("/error/message")
						.and_then(Value::as_str)
						.unwrap_or("the plugin reported failure without detail")
						.to_owned(),
				),
			});
		}

		Ok(())
	}

	/// One item: metadata check, chunk pump, verified PNG write
	fn pull_item(
		&self,
		client: &Client,
		item: &Value,
		name: &str,
		used_names: &mut HashSet<String>,
		layout: &mut Layout,
	) -> Result<(PathBuf, u64, String)> {
		let file = layout.file_for(name, used_names)?;

		let (bytes, sha256, _, _) = capture::pull_and_write(client, item, &file, self.raw)?;

		Ok((file, bytes, sha256))
	}

	/// The image-like instances under `--from`, resolved client-side through
	/// the plugin's `find` op, one class at a time (`find` matches
	/// subclasses, so the four classes cover the documented surface)
	fn walk_from(&self, client: &Client, from: &str) -> Result<Vec<String>> {
		let mut paths: Vec<String> = Vec::new();
		let mut seen: HashSet<String> = HashSet::new();

		for class in IMAGE_CLASSES {
			let value = client
				.request("find", json!({ "under": from, "class": class }))?
				.into_value(self.raw)
				.with_context(|| format!("Searching {from} for {class} instances failed"))?;

			if value.get("truncated").and_then(Value::as_bool) == Some(true) {
				wsync_warn!("The {class} search under {from} was truncated — some images may be missed");
			}

			let empty = Vec::new();

			for entry in value.get("matches").and_then(Value::as_array).unwrap_or(&empty) {
				if let Some(path) = entry.get("path").and_then(Value::as_str) {
					if seen.insert(path.to_owned()) {
						paths.push(path.to_owned());
					}
				}
			}
		}

		Ok(paths)
	}
}

/// Where item PNGs land. Single item + file output → exactly that file;
/// anything else → a directory of `<name>.png` files
enum Layout {
	Undecided,
	File(PathBuf),
	Directory(PathBuf),
}

impl Layout {
	/// Settles the layout on the first batch that carries items. The
	/// single-file layout is only chosen when this batch is the last one
	/// that can produce items (`!may_follow`) and it carries exactly one —
	/// otherwise the run may grow, and only a directory can hold it. A
	/// `File` layout can never see a second batch: it required
	/// `!may_follow`, and batches are sequential
	fn grow(&mut self, transmit: &Transmit, total_items: usize, may_follow: bool) -> Result<()> {
		if total_items == 0 || matches!(self, Layout::Directory(_) | Layout::File(_)) {
			return Ok(());
		}

		let output = transmit.output.resolve()?;

		if total_items == 1 && !may_follow && !output.is_dir() && !ends_with_separator(&transmit.output) {
			*self = Layout::File(output);

			return Ok(());
		}

		if output.is_file() {
			bail!(
				"--output {} is an existing file, but this transmit can produce multiple images — pass a directory",
				output.to_string().bold()
			);
		}

		if output.get_ext().eq_ignore_ascii_case("png") {
			bail!(
				"--output {} looks like a single PNG, but this transmit can produce multiple images — pass a directory",
				output.to_string().bold()
			);
		}

		fs::create_dir_all(&output)
			.with_context(|| format!("Failed to create the output directory {}", output.to_string()))?;

		*self = Layout::Directory(output);

		Ok(())
	}

	/// The output file for one item. Directory items are `<name>.png` with
	/// a defensive sanitization pass — the plugin pre-sanitizes and
	/// de-duplicates names, so on a well-behaved plugin this is a no-op,
	/// but a hostile name must never escape the directory
	fn file_for(&mut self, name: &str, used_names: &mut HashSet<String>) -> Result<PathBuf> {
		match self {
			Layout::Undecided => bail!("internal: the output layout was not settled before an item arrived"),
			Layout::File(file) => {
				if let Some(parent) = file.parent() {
					fs::create_dir_all(parent)
						.with_context(|| format!("Failed to create the output directory {}", parent.to_string()))?;
				}

				Ok(file.clone())
			}
			Layout::Directory(dir) => {
				let base = sanitize_name(name);
				let mut candidate = format!("{base}.png");
				let mut counter = 2;

				while !used_names.insert(candidate.clone()) {
					candidate = format!("{base}-{counter}.png");
					counter += 1;
				}

				Ok(dir.join(candidate))
			}
		}
	}
}

fn ends_with_separator(path: &std::path::Path) -> bool {
	path.to_string_lossy().ends_with(['/', '\\'])
}

/// Filesystem-safe item name: separators and control characters replaced,
/// leading dots stripped, a trailing `.png` (any case) removed so the
/// appended extension never doubles, empty names replaced
fn sanitize_name(name: &str) -> String {
	let mut cleaned: String = name
		.chars()
		.map(|character| match character {
			'/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '-',
			character if character.is_control() => '-',
			character => character,
		})
		.collect();

	if cleaned.to_ascii_lowercase().ends_with(".png") {
		cleaned.truncate(cleaned.len() - 4);
	}

	let cleaned = cleaned.trim().trim_start_matches('.').trim();

	if cleaned.is_empty() {
		"item".to_owned()
	} else {
		cleaned.to_owned()
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn names_sanitize_defensively() {
		assert_eq!(sanitize_name("IconA"), "IconA");
		assert_eq!(sanitize_name("Icon B/2"), "Icon B-2");
		// Separators become dashes, then leading dots are stripped
		assert_eq!(sanitize_name("../evil"), "-evil");
		assert_eq!(sanitize_name("shot.png"), "shot");
		assert_eq!(sanitize_name("Shot.PNG"), "Shot");
		assert_eq!(sanitize_name(""), "item");
		assert_eq!(sanitize_name("..."), "item");
		assert_eq!(sanitize_name("a:b*c"), "a-b-c");
	}

	#[test]
	fn duplicate_names_get_numbered() {
		let mut layout = Layout::Directory(PathBuf::from("/tmp/out"));
		let mut used = HashSet::new();

		let first = layout.file_for("Shot", &mut used).unwrap();
		let second = layout.file_for("Shot", &mut used).unwrap();
		let third = layout.file_for("Shot", &mut used).unwrap();

		assert_eq!(first.get_name(), "Shot.png");
		assert_eq!(second.get_name(), "Shot-2.png");
		assert_eq!(third.get_name(), "Shot-3.png");
	}
}
