//! The Studio clipboard: `copy` and `paste` (copy.json, paste.json).
//!
//! `copy` has Studio serialize arbitrary native instance trees
//! (SerializationService preserves types, descendants, properties,
//! attributes, tags, scripts, and intra-copy references), pulls the payload
//! down in bounded chunks, verifies its SHA-256, and *atomically replaces*
//! the private cross-project clipboard in the WSync state directory:
//! `clipboard.rbxm` plus a `clipboard.json` sidecar recording `{sha256,
//! bytes, roots, copiedAt, sourceProject}`. Both are written temp+rename with
//! `0600` permissions, sidecar last — so the sidecar on disk always describes
//! a payload that finished writing, and a digest mismatch between the two is
//! detectable as a torn state rather than pasted as garbage.
//!
//! `paste` streams the stored payload back (begin → chunks → commit) into
//! whichever project's Studio is connected — the clipboard is deliberately
//! shared across every WSync project — and prints the created root paths.
//! Paste never consumes the clipboard.

use anyhow::{bail, Context, Result};
use clap::Parser;
use colored::Colorize;
use serde_json::{json, Value};
use std::{
	fs,
	path::{Path, PathBuf},
	process,
};

use crate::{
	cli::client::{field, print_json, print_ok, Client, Target, Targeting},
	cli::live::transfer::{self, human_size, sha256_hex},
	ext::PathExt,
	wsync_info,
};

/// Root-count bound per copy (copy.json)
const MAX_ROOTS: usize = 256;

/// Payload bound per copy (copy.json)
const MAX_BYTES: u64 = 128 * 1024 * 1024;

/// Deadline for the heavy end ops — Studio-side serialization
/// (`clipboard_copy`) and apply (`clipboard_paste_commit`) legitimately
/// outlast the 5 s default on large trees. `--timeout` overrides
const HEAVY_TIMEOUT_MS: u64 = 60_000;

const CLIPBOARD_FILE: &str = "clipboard.rbxm";
const SIDECAR_FILE: &str = "clipboard.json";

/// Copy native Studio instance trees into WSync's private cross-project
/// clipboard
#[derive(Parser)]
pub struct Copy {
	#[command(flatten)]
	targeting: Targeting,

	/// Studio paths of the roots to copy (defaults to the current Studio
	/// selection)
	#[arg(value_name = "STUDIO-PATH")]
	paths: Vec<String>,

	/// Additional root to copy (repeatable)
	#[arg(long = "path", value_name = "STUDIO-PATH")]
	flagged: Vec<String>,

	/// Seconds to wait for Studio to serialize the roots (default 60)
	#[arg(long, value_name = "SECONDS", value_parser = clap::value_parser!(u64).range(1..=600))]
	timeout: Option<u64>,

	/// Print machine-readable JSON
	#[arg(long)]
	raw: bool,
}

impl Copy {
	pub fn main(self) -> Result<()> {
		let mut roots = self.paths.clone();
		roots.extend(self.flagged.iter().cloned());

		// Bounded before any socket is opened (copy.json)
		if roots.len() > MAX_ROOTS {
			bail!(
				"A copy is bounded at {MAX_ROOTS} roots ({} requested) — copy a common ancestor instead",
				roots.len()
			);
		}

		let client = Client::connect(&self.targeting)?;

		let args = if roots.is_empty() {
			json!({})
		} else {
			json!({ "paths": roots })
		};

		let timeout_ms = self.timeout.map_or(HEAVY_TIMEOUT_MS, |seconds| seconds * 1000);
		let prepared = client
			.request_with_timeout("clipboard_copy", args, timeout_ms)?
			.into_value(self.raw)?;

		let clip_id = prepared
			.get("clipId")
			.and_then(Value::as_str)
			.context("The plugin answered `clipboard_copy` without a clipId")?;
		let bytes = prepared.get("bytes").and_then(Value::as_u64).unwrap_or(0);
		let sha = prepared.get("sha256").and_then(Value::as_str).unwrap_or_default();

		if bytes > MAX_BYTES {
			bail!(
				"The serialized copy is {} — the clipboard is bounded at {} per copy",
				human_size(bytes),
				human_size(MAX_BYTES)
			);
		}

		if sha.is_empty() {
			bail!("The plugin answered `clipboard_copy` without a SHA-256 digest");
		}

		let payload = transfer::pull(
			&client,
			&transfer::Prepared {
				op: "clipboard_read",
				id_key: "clipId",
				id: clip_id,
				bytes,
				sha256: sha,
				label: "clipboard",
			},
			self.raw,
		)?;

		// Only a fully verified payload replaces the stored clipboard — a
		// failed pull above leaves whatever was there before untouched
		let copied_roots = prepared.get("roots").cloned().unwrap_or_else(|| json!([]));
		let source_project = client
			.target
			.canonical
			.clone()
			.unwrap_or_else(|| client.target.project_path.to_string());
		let copied_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

		let sidecar = json!({
			"sha256": sha,
			"bytes": bytes,
			"roots": copied_roots,
			"copiedAt": copied_at,
			"sourceProject": source_project,
		});

		let clipboard_path = store_clipboard(&client.target.state_dir, &payload, &sidecar)?;

		if self.raw {
			print_json(&json!({
				"ok": true,
				"path": clipboard_path.to_string(),
				"bytes": bytes,
				"sha256": sha,
				"roots": sidecar["roots"],
				"copiedAt": sidecar["copiedAt"],
				"sourceProject": sidecar["sourceProject"],
			}));

			return Ok(());
		}

		let empty = Vec::new();
		let listed = sidecar["roots"].as_array().unwrap_or(&empty);

		for root in listed {
			println!("{:<20} {}", field(root, "class"), field(root, "path"));
		}

		wsync_info!(
			"Copied {} root(s) to the WSync clipboard ({}) — paste into any project with `{}`",
			listed.len().to_string().bold(),
			human_size(bytes),
			"wsync paste".bold()
		);

		Ok(())
	}
}

/// Paste the private clipboard into the connected Studio as one undoable
/// change
#[derive(Parser)]
pub struct Paste {
	#[command(flatten)]
	targeting: Targeting,

	/// Parent Studio path for every pasted root (defaults to each root's
	/// recorded parent route)
	#[arg(long, alias = "parent", value_name = "PARENT-PATH")]
	to: Option<String>,

	/// Do not select the pasted roots in Studio
	#[arg(long = "no-select")]
	no_select: bool,

	/// Seconds to wait for Studio to apply the paste (default 60)
	#[arg(long, value_name = "SECONDS", value_parser = clap::value_parser!(u64).range(1..=600))]
	timeout: Option<u64>,

	/// Print machine-readable JSON
	#[arg(long)]
	raw: bool,
}

impl Paste {
	pub fn main(self) -> Result<()> {
		// The clipboard is read and verified before any daemon is contacted:
		// "nothing to paste" and "torn clipboard" are local facts that should
		// not cost a connection to diagnose
		let target = Target::resolve(&self.targeting)?;
		let (payload, sidecar) = load_clipboard(&target.state_dir)?;

		let bytes = payload.len() as u64;
		let sha = sha256_hex(&payload);

		let client = Client::connect(&self.targeting)?;

		let begun = client
			.request("clipboard_paste_begin", json!({ "bytes": bytes, "sha256": sha }))?
			.into_value(self.raw)?;

		let clip_id = begun
			.get("clipId")
			.and_then(Value::as_str)
			.context("The plugin answered `clipboard_paste_begin` without a clipId")?;

		transfer::push(
			&client,
			"clipboard_paste_chunk",
			"clipId",
			clip_id,
			&payload,
			"clipboard",
			self.raw,
		)?;

		let mut commit = json!({ "clipId": clip_id });

		if let Some(to) = &self.to {
			commit["to"] = json!(to);
		}

		if self.no_select {
			commit["noSelect"] = json!(true);
		}

		let timeout_ms = self.timeout.map_or(HEAVY_TIMEOUT_MS, |seconds| seconds * 1000);
		let value = client
			.request_with_timeout("clipboard_paste_commit", commit, timeout_ms)?
			.into_value(self.raw)?;

		if self.raw {
			print_ok(&value);

			return Ok(());
		}

		let empty = Vec::new();
		let roots = value.get("roots").and_then(Value::as_array).unwrap_or(&empty);

		// The created paths are what every follow-up read or write addresses
		for root in roots {
			println!("{}", field(root, "path"));
		}

		wsync_info!(
			"Pasted {} root(s) from {} — one Studio undo removes the paste",
			roots.len().to_string().bold(),
			sidecar
				.get("sourceProject")
				.and_then(Value::as_str)
				.unwrap_or("the WSync clipboard")
				.bold()
		);

		Ok(())
	}
}

// ---------------------------------------------------------------------------
// The on-disk clipboard
// ---------------------------------------------------------------------------

/// Atomically replaces the stored clipboard: payload first, sidecar second,
/// each temp+rename with `0600`. Returns the payload path
fn store_clipboard(state_dir: &Path, payload: &[u8], sidecar: &Value) -> Result<PathBuf> {
	fs::create_dir_all(state_dir)
		.with_context(|| format!("Failed to create the WSync state directory {}", state_dir.to_string()))?;

	let clipboard = state_dir.join(CLIPBOARD_FILE);
	let sidecar_path = state_dir.join(SIDECAR_FILE);

	write_private(&clipboard, payload)?;
	write_private(&sidecar_path, format!("{sidecar:#}\n").as_bytes())?;

	Ok(clipboard)
}

/// One private file, atomically: temp in the same directory, `0600`, rename
/// over the destination
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
	let temp = path.with_file_name(format!(".{}.tmp-{}", path.get_name(), process::id()));

	let written = (|| {
		fs::write(&temp, bytes).with_context(|| format!("Failed to write {}", temp.to_string()))?;

		#[cfg(unix)]
		{
			use std::os::unix::fs::PermissionsExt;

			fs::set_permissions(&temp, fs::Permissions::from_mode(0o600))
				.with_context(|| format!("Failed to restrict permissions on {}", temp.to_string()))?;
		}

		fs::rename(&temp, path).with_context(|| format!("Failed to move {} into place", path.to_string()))
	})();

	if written.is_err() {
		fs::remove_file(&temp).ok();
	}

	written
}

/// Loads and verifies the stored clipboard. A missing clipboard is a clear
/// instruction; a payload/sidecar digest mismatch is reported as the torn
/// state it is instead of being pasted
fn load_clipboard(state_dir: &Path) -> Result<(Vec<u8>, Value)> {
	let clipboard = state_dir.join(CLIPBOARD_FILE);
	let sidecar_path = state_dir.join(SIDECAR_FILE);

	if !clipboard.exists() || !sidecar_path.exists() {
		bail!(
			"The WSync clipboard is empty — copy something first with `{}` (the clipboard is shared across every \
			 WSync project)",
			"wsync copy".bold()
		);
	}

	let sidecar: Value = serde_json::from_str(
		&fs::read_to_string(&sidecar_path)
			.with_context(|| format!("Failed to read the clipboard sidecar {}", sidecar_path.to_string()))?,
	)
	.with_context(|| format!("The clipboard sidecar {} is not valid JSON", sidecar_path.to_string()))?;

	let payload =
		fs::read(&clipboard).with_context(|| format!("Failed to read the clipboard {}", clipboard.to_string()))?;

	let declared_bytes = sidecar.get("bytes").and_then(Value::as_u64).unwrap_or(0);
	let declared_sha = sidecar.get("sha256").and_then(Value::as_str).unwrap_or_default();
	let digest = sha256_hex(&payload);

	if payload.len() as u64 != declared_bytes || !digest.eq_ignore_ascii_case(declared_sha) {
		bail!(
			"The stored clipboard does not match its sidecar (an interrupted copy?) — run `{}` again to replace it",
			"wsync copy".bold()
		);
	}

	Ok((payload, sidecar))
}
