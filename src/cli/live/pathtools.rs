//! Path tools: `path`, `meta`, `where` (path.json, meta.json, where.json).
//!
//! All three wrap daemon-answered ops (`src/server/ops.rs`): the daemon
//! resolves them against its live projected tree without asking the plugin,
//! so they answer whether or not Studio is connected. Each command wraps
//! exactly one remote op, so `--raw` prints that op's value verbatim as one
//! JSON line, and a NOT_FOUND — including the daemon's "it is Studio-only"
//! refusal for instances outside the projection — surfaces as the op error
//! before the non-zero exit.

use anyhow::Result;
use clap::{Parser, ValueEnum};
use colored::Colorize;
use serde_json::{json, Value};

use crate::{
	cli::client::{clip, field, print_json, Client, Targeting},
	wsync_warn,
};

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
enum From {
	Auto,
	Studio,
	Fs,
}

impl From {
	fn as_str(self) -> &'static str {
		match self {
			From::Auto => "auto",
			From::Studio => "studio",
			From::Fs => "fs",
		}
	}
}

/// Translate between a Studio instance path and the files backing it on disk
#[derive(Parser)]
pub struct Path {
	#[command(flatten)]
	targeting: Targeting,

	/// Studio path or workspace-relative filesystem path
	#[arg(value_name = "TARGET")]
	target: String,

	/// How to read TARGET; `auto` tries a Studio path first, then a
	/// filesystem path
	#[arg(long, value_enum, value_name = "SIDE", default_value = "auto")]
	from: From,

	/// Print machine-readable JSON
	#[arg(long)]
	raw: bool,
}

impl Path {
	pub fn main(self) -> Result<()> {
		let client = Client::connect(&self.targeting)?;
		let args = json!({ "target": self.target, "from": self.from.as_str() });
		let value = client.value("path", args, self.raw)?;

		if self.raw {
			print_json(&value);

			return Ok(());
		}

		println!("Studio  {}", field(&value, "studioPath").bold());

		let empty = Vec::new();
		let fs_paths = value.get("fsPaths").and_then(Value::as_array).unwrap_or(&empty);

		if fs_paths.is_empty() {
			// A project-node lives in the project file's `tree`, not in a file
			// of its own; saying so beats printing a blank
			println!("Disk    (no file of its own — defined by the project file)");
		} else {
			for (index, path) in fs_paths.iter().enumerate() {
				let label = if index == 0 { "Disk  " } else { "      " };

				println!("{label}  {}", path.as_str().unwrap_or_default());
			}
		}

		println!("Kind    {}", field(&value, "kind"));

		Ok(())
	}
}

/// Show the Studio path, class, and backing files for a syncable target
#[derive(Parser)]
pub struct Meta {
	#[command(flatten)]
	targeting: Targeting,

	/// Studio path or workspace-relative filesystem path
	#[arg(value_name = "TARGET")]
	target: String,

	/// How to read TARGET; `auto` tries a Studio path first, then a
	/// filesystem path
	#[arg(long, value_enum, value_name = "SIDE", default_value = "auto")]
	from: From,

	/// Print machine-readable JSON
	#[arg(long)]
	raw: bool,
}

impl Meta {
	pub fn main(self) -> Result<()> {
		let client = Client::connect(&self.targeting)?;
		let args = json!({ "target": self.target, "from": self.from.as_str() });
		let value = client.value("meta", args, self.raw)?;

		if self.raw {
			print_json(&value);

			return Ok(());
		}

		println!("Instance    {}", field(&value, "instancePath").bold());
		println!("Class       {}", field(&value, "class"));

		// `middleware` is present only when a middleware maps the class; its
		// absence is a fact worth stating, not a blank
		match value.get("middleware").and_then(Value::as_str) {
			Some(middleware) => println!("Middleware  {middleware}"),
			None => println!("Middleware  (none — synced structurally)"),
		}

		let empty = Vec::new();
		let sources = value.get("sourcePaths").and_then(Value::as_array).unwrap_or(&empty);

		if sources.is_empty() {
			println!("Files       (none — defined by the project file)");
		} else {
			for (index, path) in sources.iter().enumerate() {
				let label = if index == 0 { "Files     " } else { "          " };

				println!("{label}  {}", path.as_str().unwrap_or_default());
			}
		}

		if value.get("keepUnknowns").and_then(Value::as_bool) == Some(true) {
			println!("Unknowns    kept (children outside the projection are preserved)");
		}

		Ok(())
	}
}

/// Find projected instances by name substring, resolved to disk when possible
#[derive(Parser)]
pub struct Where {
	#[command(flatten)]
	targeting: Targeting,

	/// Name substring to search for (case-insensitive)
	#[arg(value_name = "TARGET")]
	target: String,

	/// Only search below this Studio path
	#[arg(long, value_name = "STUDIO-PATH")]
	under: Option<String>,

	/// Print machine-readable JSON
	#[arg(long)]
	raw: bool,
}

impl Where {
	pub fn main(self) -> Result<()> {
		let client = Client::connect(&self.targeting)?;

		let mut args = json!({ "target": self.target });

		if let Some(under) = &self.under {
			args["under"] = json!(under);
		}

		let value = client.value("where", args, self.raw)?;

		if self.raw {
			print_json(&value);

			return Ok(());
		}

		let empty = Vec::new();
		let matches = value.get("matches").and_then(Value::as_array).unwrap_or(&empty);

		for entry in matches {
			// Matches the projection does not back on disk are still listed,
			// marked Studio-only (where.json)
			let disk = entry
				.get("fsPath")
				.and_then(Value::as_str)
				.map_or_else(|| "(Studio-only)".dimmed().to_string(), str::to_owned);

			println!("{:<44} {disk}", clip(field(entry, "instancePath"), 44));
		}

		println!("\n{} match(es) for `{}`", matches.len(), self.target);

		if value.get("truncated").and_then(Value::as_bool) == Some(true) {
			wsync_warn!("The match list was truncated — narrow the search with --under or a longer substring");
		}

		Ok(())
	}
}
