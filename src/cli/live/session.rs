//! Studio session state: `select get|set`, `logs`, `tail`, and `open`
//! (select.json, logs.json, tail.json, open.json).
//!
//! `select get` is read-only; `select set` is the one whitelisted Studio
//! state change in this slice (it changes the Selection, nothing in the
//! DataModel). `logs --tail` pages forward with the plugin's `sinceSeq`
//! cursor, so a poll never re-prints an entry it already showed. `tail` is
//! that same loop as its own command — it delegates to [`Logs`] with the
//! tail flag pinned on, so the two can never drift apart. `open` is the same
//! delegation shape over `select set`: positional Studio paths instead of a
//! JSON array, one shared implementation.

use anyhow::{bail, Context, Result};
use chrono::{Local, TimeZone};
use clap::{Parser, Subcommand};
use colored::Colorize;
use serde_json::{json, Value};
use std::{thread, time::Duration};

use crate::{
	cli::client::{kind_of, print_json, print_line, Client, Targeting},
	wsync_info, wsync_warn,
};

/// How long `--tail` waits between polls
const TAIL_INTERVAL: Duration = Duration::from_millis(500);

/// Read or replace the current Studio Selection
#[derive(Parser)]
pub struct Select {
	#[command(subcommand)]
	command: SelectCommand,
}

#[derive(Subcommand)]
enum SelectCommand {
	/// Print the current Studio selection
	Get(SelectGet),
	/// Replace the Studio selection
	Set(SelectSet),
}

#[derive(Parser)]
struct SelectGet {
	#[command(flatten)]
	targeting: Targeting,

	/// Print machine-readable JSON
	#[arg(long)]
	raw: bool,
}

#[derive(Parser)]
struct SelectSet {
	#[command(flatten)]
	targeting: Targeting,

	/// JSON array of Studio paths, e.g. `["Workspace/Box"]`
	#[arg(long, value_name = "JSON-ARRAY")]
	paths: String,

	/// Print machine-readable JSON
	#[arg(long)]
	raw: bool,
}

impl Select {
	pub fn main(self) -> Result<()> {
		match self.command {
			SelectCommand::Get(command) => command.main(),
			SelectCommand::Set(command) => command.main(),
		}
	}
}

impl SelectGet {
	fn main(self) -> Result<()> {
		let client = Client::connect(&self.targeting)?;
		let value = client.value("select_get", json!({}), self.raw)?;

		if self.raw {
			print_json(&value);

			return Ok(());
		}

		let empty = Vec::new();
		let paths = value.get("paths").and_then(Value::as_array).unwrap_or(&empty);

		for path in paths {
			println!("{}", path.as_str().unwrap_or_default());
		}

		println!("\n{} instance(s) selected", paths.len());

		Ok(())
	}
}

impl SelectSet {
	fn main(self) -> Result<()> {
		// Validated before the request: a malformed `--paths` never reaches
		// Studio, so the current selection is never touched by a typo
		let parsed: Value = serde_json::from_str(&self.paths)
			.context("--paths must be a JSON array of Studio paths, e.g. '[\"Workspace/Box\"]'")?;

		let Some(items) = parsed.as_array() else {
			bail!("--paths must be a JSON array, not {}", kind_of(&parsed));
		};

		for (index, item) in items.iter().enumerate() {
			if !item.is_string() {
				bail!("--paths[{index}] must be a string, not {}", kind_of(item));
			}
		}

		let client = Client::connect(&self.targeting)?;
		let value = client.value("select_set", json!({ "paths": parsed }), self.raw)?;

		if self.raw {
			print_json(&value);

			return Ok(());
		}

		wsync_info!(
			"Selected {} instance(s) in Studio",
			value
				.get("count")
				.and_then(Value::as_u64)
				.unwrap_or(items.len() as u64)
				.to_string()
				.bold()
		);

		Ok(())
	}
}

/// Select one or more Studio instances by path
///
/// Shorthand over `wsync select set --paths '[…]'` (open.json). With the
/// `OpenInEditor` plugin setting on, selecting a script also opens it in the
/// Studio script editor.
#[derive(Parser)]
pub struct Open {
	#[command(flatten)]
	targeting: Targeting,

	/// Studio path(s) to select, `/`-separated
	#[arg(value_name = "STUDIO-PATH", required = true)]
	paths: Vec<String>,

	/// Print machine-readable JSON
	#[arg(long)]
	raw: bool,
}

impl Open {
	pub fn main(self) -> Result<()> {
		SelectSet {
			targeting: self.targeting,
			paths: serde_json::to_string(&self.paths)?,
			raw: self.raw,
		}
		.main()
	}
}

/// Read recent Studio output, warning, and error messages
#[derive(Parser)]
pub struct Logs {
	#[command(flatten)]
	targeting: Targeting,

	/// Only entries newer than this (e.g. `30s`, `5m`, `2h`; bare numbers are
	/// seconds)
	#[arg(long, value_name = "DURATION")]
	since: Option<String>,

	/// Minimum level to report
	#[arg(long, value_name = "LEVEL", value_parser = ["info", "warn", "error"])]
	level: Option<String>,

	/// Maximum entries to return
	#[arg(long, value_name = "N")]
	limit: Option<u32>,

	/// Keep polling and print new entries until interrupted
	#[arg(long)]
	tail: bool,

	/// Print NDJSON, one log entry per line
	#[arg(long)]
	raw: bool,
}

impl Logs {
	pub fn main(self) -> Result<()> {
		// Parsed before connecting: a bad duration is a usage error, not a
		// daemon round trip
		let since_seconds = match &self.since {
			Some(text) => Some(parse_duration(text)?),
			None => None,
		};

		let client = Client::connect(&self.targeting)?;

		let mut cursor: Option<u64> = None;
		let mut first = true;

		loop {
			let mut args = json!({});

			if let Some(level) = &self.level {
				args["level"] = json!(level);
			}

			if let Some(limit) = self.limit {
				args["limit"] = json!(limit);
			}

			match cursor {
				// Subsequent polls page forward from the cursor; `since` only
				// bounds the first read
				Some(cursor) => args["sinceSeq"] = json!(cursor),
				None => {
					if let Some(seconds) = since_seconds {
						args["sinceSeconds"] = json!(seconds);
					}
				}
			}

			let value = client.value("logs", args, self.raw)?;
			let empty = Vec::new();
			let entries = value.get("entries").and_then(Value::as_array).unwrap_or(&empty);

			// `t` is plugin-clock relative; `now`/`wall` convert it to a real
			// timestamp the CLI can print and machines can sort on
			let now = value.get("now").and_then(Value::as_f64).unwrap_or(0.0);
			let wall = value.get("wall").and_then(Value::as_f64).unwrap_or(0.0);

			for entry in entries {
				let timestamp = entry.get("t").and_then(Value::as_f64).map(|clock| wall - (now - clock));

				if self.raw {
					let mut record = entry.clone();

					if let (Some(object), Some(timestamp)) = (record.as_object_mut(), timestamp) {
						object.insert("wall".to_owned(), json!(timestamp));
					}

					print_line(&record.to_string());
				} else {
					print_line(&format_entry(entry, timestamp));
				}
			}

			cursor = value
				.pointer("/buffer/newestSeq")
				.and_then(Value::as_u64)
				.or_else(|| entries.iter().filter_map(|entry| entry["seq"].as_u64()).max())
				.or(cursor);

			if first && !self.raw {
				if entries.is_empty() {
					wsync_warn!("The plugin log buffer has no matching entries");
				}

				if self.tail {
					wsync_info!("Tailing Studio output (Ctrl+C to stop)");
				}
			}

			first = false;

			if !self.tail {
				return Ok(());
			}

			thread::sleep(TAIL_INTERVAL);
		}
	}
}

/// Stream Studio output, warning, and error messages until interrupted
/// (equivalent to `wsync logs --tail`)
#[derive(Parser)]
pub struct Tail {
	#[command(flatten)]
	targeting: Targeting,

	/// Only entries newer than this (e.g. `30s`, `5m`, `2h`; bare numbers are
	/// seconds)
	#[arg(long, value_name = "DURATION")]
	since: Option<String>,

	/// Minimum level to report
	#[arg(long, value_name = "LEVEL", value_parser = ["info", "warn", "error"])]
	level: Option<String>,

	/// Maximum entries per poll
	#[arg(long, value_name = "N")]
	limit: Option<u32>,

	/// Print NDJSON, one log entry per line
	#[arg(long)]
	raw: bool,
}

impl Tail {
	pub fn main(self) -> Result<()> {
		Logs {
			targeting: self.targeting,
			since: self.since,
			level: self.level,
			limit: self.limit,
			tail: true,
			raw: self.raw,
		}
		.main()
	}
}

fn format_entry(entry: &Value, timestamp: Option<f64>) -> String {
	let clock = timestamp
		.and_then(|seconds| Local.timestamp_opt(seconds as i64, 0).single())
		.map(|time| time.format("%H:%M:%S").to_string())
		.unwrap_or_else(|| "--:--:--".to_owned());

	let level = entry.get("level").and_then(Value::as_str).unwrap_or("info");
	let painted = match level {
		"error" => "error".red(),
		"warn" => " warn".yellow(),
		_ => " info".normal(),
	};

	format!(
		"{clock} {painted} {}",
		entry.get("message").and_then(Value::as_str).unwrap_or_default()
	)
}

/// `30s`, `5m`, `2h`, `1d`, `250ms` — or a bare number of seconds
fn parse_duration(text: &str) -> Result<f64> {
	let text = text.trim();
	let digits = text
		.trim_end_matches(|character: char| character.is_ascii_alphabetic())
		.trim();
	let unit = text[digits.len()..].trim();

	let amount: f64 = digits
		.parse()
		.with_context(|| format!("--since expects a duration like `30s`, `5m` or `2h`, not `{text}`"))?;

	if amount < 0.0 {
		bail!("--since cannot be negative");
	}

	let seconds = match unit {
		"ms" => amount / 1000.0,
		"" | "s" => amount,
		"m" => amount * 60.0,
		"h" => amount * 3600.0,
		"d" => amount * 86400.0,
		other => bail!("Unknown --since unit `{other}` (use ms, s, m, h or d)"),
	};

	Ok(seconds)
}

#[cfg(test)]
mod tests {
	use super::parse_duration;

	#[test]
	fn durations_parse_with_and_without_units() {
		assert_eq!(parse_duration("90").unwrap(), 90.0);
		assert_eq!(parse_duration("30s").unwrap(), 30.0);
		assert_eq!(parse_duration("5m").unwrap(), 300.0);
		assert_eq!(parse_duration("2h").unwrap(), 7200.0);
		assert_eq!(parse_duration("250ms").unwrap(), 0.25);
		assert!(parse_duration("5y").is_err());
		assert!(parse_duration("soon").is_err());
	}
}
