//! `backlog` — the disk content that lost to Studio.
//!
//! WSync is Studio-first and never stops to ask: a connect applies Studio over
//! the project and a mid-session clash resolves the same way. The disk bytes
//! that lost are not discarded, they are moved here, and this command is the
//! terminal half of the app's backlog view — list what is waiting, put an entry
//! back, or drop it.
//!
//! Entries expire a day after capture (`backlog_ttl_hours`) unless pinned. The
//! backlog is a safety net for an edit you have not noticed losing, not a
//! version history: `diff` shows an entry against the file there now, and a
//! path that lost more than once lists its newest entry first.

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use colored::Colorize;
use serde_json::{json, Value};

use crate::{
	cli::client::{print_json, print_line, Client, Targeting},
	wsync_info,
};

/// Review, restore, or drop disk content that lost to Studio
#[derive(Parser)]
pub struct Backlog {
	#[command(subcommand)]
	command: Option<BacklogCommand>,

	/// Project path (defaults to the current directory)
	#[arg(long, global = true, value_name = "PATH")]
	project: Option<std::path::PathBuf>,

	/// Daemon port
	#[arg(short = 'P', long, global = true)]
	port: Option<u16>,

	/// Print machine-readable JSON
	#[arg(long, global = true)]
	raw: bool,
}

#[derive(Subcommand)]
enum BacklogCommand {
	/// List what is waiting and how long each entry has left (the default)
	List,
	/// Put an entry back on disk and push it to Studio
	Restore {
		/// Entry id from `wsync backlog`
		#[arg(value_name = "ID")]
		id: String,
	},
	/// Forget an entry without restoring it
	Drop {
		/// Entry id; omit with --all
		#[arg(value_name = "ID")]
		id: Option<String>,

		/// Forget every entry
		#[arg(long)]
		all: bool,
	},
	/// Show an entry against the file at its path now
	Diff {
		/// Entry id from `wsync backlog`
		#[arg(value_name = "ID")]
		id: String,
	},
	/// Keep an entry until it is dropped or restored (it never expires)
	Pin {
		/// Entry id from `wsync backlog`
		#[arg(value_name = "ID")]
		id: String,
	},
	/// Let a pinned entry expire again on its original schedule
	Unpin {
		/// Entry id from `wsync backlog`
		#[arg(value_name = "ID")]
		id: String,
	},
}

impl Backlog {
	pub fn main(self) -> Result<()> {
		if let Some(BacklogCommand::Drop { id, all }) = &self.command {
			if id.is_some() == *all {
				bail!("Pass exactly one of an entry id or --all");
			}
		}
		let client = Client::connect(&Targeting {
			project: self.project.clone(),
			port: self.port,
		})?;

		let Backlog { command, raw, .. } = self;

		match command {
			None | Some(BacklogCommand::List) => list(&client, raw),
			Some(BacklogCommand::Restore { id }) => restore(&client, &id, raw),
			Some(BacklogCommand::Drop { id, all }) => drop_entry(&client, id, all, raw),
			Some(BacklogCommand::Diff { id }) => diff(&client, &id, raw),
			Some(BacklogCommand::Pin { id }) => pin(&client, &id, true, raw),
			Some(BacklogCommand::Unpin { id }) => pin(&client, &id, false, raw),
		}
	}
}

fn list(client: &Client, raw: bool) -> Result<()> {
	let endpoint = client.get_endpoint("/backlog")?;
	let value = endpoint.json("/backlog")?.clone();

	if raw {
		print_json(&value);

		return Ok(());
	}

	let empty = Vec::new();
	let entries = value.get("entries").and_then(Value::as_array).unwrap_or(&empty);

	if entries.is_empty() {
		wsync_info!("The backlog is empty — nothing on disk has lost to Studio");

		return Ok(());
	}

	// Newest first within a path, and a path's older versions indented under
	// it: two entries for one file is one file that lost twice
	let mut ordered: Vec<&Value> = entries.iter().collect();

	ordered.sort_by(|a, b| {
		let path = |entry: &Value| entry.get("path").and_then(Value::as_str).unwrap_or("").to_owned();
		let captured = |entry: &Value| entry.get("capturedAt").and_then(Value::as_u64).unwrap_or(0);

		path(a).cmp(&path(b)).then(captured(b).cmp(&captured(a)))
	});

	let mut previous_path: Option<&str> = None;

	for entry in ordered {
		let path = entry.get("path").and_then(Value::as_str).unwrap_or("?");
		let older = previous_path == Some(path);

		let expiry = if entry.get("pinned").and_then(Value::as_bool).unwrap_or(false) {
			"pinned".bold().to_string()
		} else {
			format!(
				"expires in {}",
				human_remaining(entry.get("secondsRemaining").and_then(Value::as_u64).unwrap_or(0))
			)
		};

		let captures = entry.get("captures").and_then(Value::as_u64).unwrap_or(1);
		let repeated = if captures > 1 {
			format!("  (lost {captures} times)")
		} else {
			String::new()
		};

		print_line(&format!(
			"{}{}  {}  {}  {}{}",
			if older { "  └ older: " } else { "" },
			entry.get("id").and_then(Value::as_str).unwrap_or("?").dimmed(),
			if older { path.normal() } else { path.bold() },
			entry.get("reason").and_then(Value::as_str).unwrap_or("?"),
			expiry,
			repeated,
		));

		previous_path = Some(path);
	}

	wsync_info!(
		"{} entr(ies) waiting — `wsync backlog diff <id>` compares one with the file now, `restore <id>` puts it back, `pin <id>` keeps it",
		entries.len()
	);

	Ok(())
}

fn restore(client: &Client, id: &str, raw: bool) -> Result<()> {
	let endpoint = client.post_endpoint("/backlog/restore", &json!({ "id": id }))?;
	let value = endpoint.json("/backlog/restore")?.clone();

	if raw {
		print_json(&value);

		return Ok(());
	}

	wsync_info!(
		"Restored {} — it is back on disk and on its way to Studio",
		value.get("path").and_then(Value::as_str).unwrap_or(id).bold()
	);

	Ok(())
}

fn drop_entry(client: &Client, id: Option<String>, all: bool, raw: bool) -> Result<()> {
	let body = match (id, all) {
		(Some(id), false) => json!({ "id": id }),
		(None, true) => json!({ "all": true }),
		_ => bail!("Pass exactly one of an entry id or --all"),
	};

	let endpoint = client.post_endpoint("/backlog/drop", &body)?;
	let value = endpoint.json("/backlog/drop")?.clone();

	if raw {
		print_json(&value);

		return Ok(());
	}

	wsync_info!(
		"Dropped {} backlog entr(ies)",
		value.get("dropped").and_then(Value::as_u64).unwrap_or(0)
	);

	Ok(())
}

fn pin(client: &Client, id: &str, pinned: bool, raw: bool) -> Result<()> {
	let endpoint = client.post_endpoint("/backlog/pin", &json!({ "id": id, "pinned": pinned }))?;
	let value = endpoint.json("/backlog/pin")?.clone();

	if raw {
		print_json(&value);

		return Ok(());
	}

	let path = value.get("path").and_then(Value::as_str).unwrap_or(id).bold();

	if pinned {
		wsync_info!("Pinned {path} — it stays until you restore or drop it");
	} else {
		wsync_info!("Unpinned {path} — it expires on its original schedule");
	}

	Ok(())
}

fn diff(client: &Client, id: &str, raw: bool) -> Result<()> {
	// Entry ids are UUIDs; anything else never names an entry and must not
	// reach the query string unescaped
	if id.is_empty() || !id.chars().all(|char| char.is_ascii_alphanumeric() || char == '-') {
		bail!("`{id}` is not a backlog entry id — see `wsync backlog`");
	}

	let route = format!("/backlog/diff?id={id}");
	let endpoint = client.get_endpoint(&route)?;
	let value = endpoint.json("/backlog/diff")?.clone();

	if raw {
		print_json(&value);

		return Ok(());
	}

	let path = value.get("path").and_then(Value::as_str).unwrap_or(id);
	let stored = &value["stored"];
	let current = &value["current"];

	let describe = |side: &Value| -> Option<&'static str> {
		if side.get("exists").and_then(Value::as_bool) != Some(true) {
			Some("absent")
		} else if side.get("directory").and_then(Value::as_bool) == Some(true) {
			Some("a directory")
		} else if side.get("binary").and_then(Value::as_bool) == Some(true) {
			Some("binary")
		} else if side.get("tooLarge").and_then(Value::as_bool) == Some(true) {
			Some("too large to diff")
		} else {
			None
		}
	};

	let stored_text = stored.get("text").and_then(Value::as_str);
	let current_text = current.get("text").and_then(Value::as_str);

	match (stored_text, current_text, describe(current)) {
		(Some(stored), Some(current), _) => {
			if stored == current {
				wsync_info!("{} — the backlog entry matches the file there now", path.bold());

				return Ok(());
			}

			print_line(&format!("--- backlog {id}\n+++ {path} (now)"));

			for line in unified_diff(stored, current, 3) {
				print_line(&line);
			}
		}
		(Some(stored), None, Some("absent")) => {
			print_line(&format!("--- backlog {id}\n+++ {path} (now: absent)"));

			for line in stored.lines() {
				print_line(&format!("-{line}").red().to_string());
			}
		}
		_ => {
			wsync_info!(
				"{} — backlog entry: {}, file now: {}",
				path.bold(),
				describe(stored).unwrap_or("text"),
				describe(current).unwrap_or("text")
			);
		}
	}

	Ok(())
}

/// A unified diff of two texts, line based, with `context` lines around each
/// change. The common prefix and suffix are trimmed first so a typical edit
/// only runs the quadratic LCS over the region that actually changed
fn unified_diff(old: &str, new: &str, context: usize) -> Vec<String> {
	const MAX_CELLS: usize = 4_000_000;

	let old: Vec<&str> = old.lines().collect();
	let new: Vec<&str> = new.lines().collect();

	let prefix = old.iter().zip(&new).take_while(|(a, b)| a == b).count();
	let suffix = old[prefix..]
		.iter()
		.rev()
		.zip(new[prefix..].iter().rev())
		.take_while(|(a, b)| a == b)
		.count();

	let old_mid = &old[prefix..old.len() - suffix];
	let new_mid = &new[prefix..new.len() - suffix];

	// (tag, line): ' ' keep, '-' old only, '+' new only
	let mut ops: Vec<(char, &str)> = old[..prefix].iter().map(|line| (' ', *line)).collect();

	if old_mid.len().saturating_mul(new_mid.len()) > MAX_CELLS {
		ops.extend(old_mid.iter().map(|line| ('-', *line)));
		ops.extend(new_mid.iter().map(|line| ('+', *line)));
	} else {
		let (n, m) = (old_mid.len(), new_mid.len());
		let mut lcs = vec![0u32; (n + 1) * (m + 1)];

		for i in (0..n).rev() {
			for j in (0..m).rev() {
				lcs[i * (m + 1) + j] = if old_mid[i] == new_mid[j] {
					lcs[(i + 1) * (m + 1) + j + 1] + 1
				} else {
					lcs[(i + 1) * (m + 1) + j].max(lcs[i * (m + 1) + j + 1])
				};
			}
		}

		let (mut i, mut j) = (0, 0);

		while i < n || j < m {
			if i < n && j < m && old_mid[i] == new_mid[j] {
				ops.push((' ', old_mid[i]));
				i += 1;
				j += 1;
			} else if i < n && (j == m || lcs[(i + 1) * (m + 1) + j] >= lcs[i * (m + 1) + j + 1]) {
				// Removals before additions, as unified diffs read
				ops.push(('-', old_mid[i]));
				i += 1;
			} else {
				ops.push(('+', new_mid[j]));
				j += 1;
			}
		}
	}

	ops.extend(old[old.len() - suffix..].iter().map(|line| (' ', *line)));

	// Hunks: runs of changes with `context` unchanged lines around them
	let changed: Vec<usize> = ops
		.iter()
		.enumerate()
		.filter(|(_, (tag, _))| *tag != ' ')
		.map(|(index, _)| index)
		.collect();

	let mut lines = Vec::new();
	let mut index = 0;

	while index < changed.len() {
		let start = changed[index].saturating_sub(context);
		let mut end = changed[index];

		while index < changed.len() && changed[index] <= end + 2 * context + 1 {
			end = changed[index];
			index += 1;
		}

		let end = (end + context + 1).min(ops.len());

		let old_start = ops[..start].iter().filter(|(tag, _)| *tag != '+').count() + 1;
		let new_start = ops[..start].iter().filter(|(tag, _)| *tag != '-').count() + 1;
		let old_count = ops[start..end].iter().filter(|(tag, _)| *tag != '+').count();
		let new_count = ops[start..end].iter().filter(|(tag, _)| *tag != '-').count();

		lines.push(
			format!("@@ -{old_start},{old_count} +{new_start},{new_count} @@")
				.cyan()
				.to_string(),
		);

		for (tag, line) in &ops[start..end] {
			let rendered = format!("{tag}{line}");

			lines.push(match tag {
				'-' => rendered.red().to_string(),
				'+' => rendered.green().to_string(),
				_ => rendered,
			});
		}
	}

	lines
}

fn human_remaining(seconds: u64) -> String {
	if seconds >= 3600 {
		return format!("{}h", seconds / 3600);
	}

	if seconds >= 60 {
		return format!("{}m", seconds / 60);
	}

	format!("{seconds}s")
}

#[cfg(test)]
mod tests {
	use super::unified_diff;

	#[test]
	fn a_one_line_edit_is_one_small_hunk() {
		colored::control::set_override(false);

		let old = "a\nb\nc\nd\ne\nf\ng\nh\n";
		let new = "a\nb\nc\nd\nE\nf\ng\nh\n";
		let diff = unified_diff(old, new, 1);

		assert_eq!(diff, vec!["@@ -4,3 +4,3 @@", " d", "-e", "+E", " f"]);
	}

	#[test]
	fn identical_texts_have_no_hunks() {
		assert!(unified_diff("same\n", "same\n", 3).is_empty());
	}
}
