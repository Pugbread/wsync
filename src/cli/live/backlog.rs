//! `backlog` — the disk content that lost to Studio.
//!
//! WSync is Studio-first and never stops to ask: a connect applies Studio over
//! the project and a mid-session clash resolves the same way. The disk bytes
//! that lost are not discarded, they are moved here, and this command is the
//! terminal half of the app's backlog view — list what is waiting, put an entry
//! back, or drop it.
//!
//! Entries expire a day after capture. The backlog is a safety net for an edit
//! you have not noticed losing, not a version history.

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

	#[command(flatten)]
	targeting: Targeting,

	/// Print machine-readable JSON
	#[arg(long)]
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
}

impl Backlog {
	pub fn main(self) -> Result<()> {
		let client = Client::connect(&self.targeting)?;

		let Backlog { command, raw, .. } = self;

		match command {
			None | Some(BacklogCommand::List) => list(&client, raw),
			Some(BacklogCommand::Restore { id }) => restore(&client, &id, raw),
			Some(BacklogCommand::Drop { id, all }) => drop_entry(&client, id, all, raw),
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

	for entry in entries {
		let remaining = entry.get("secondsRemaining").and_then(Value::as_u64).unwrap_or(0);

		print_line(&format!(
			"{}  {}  {}  expires in {}",
			entry.get("id").and_then(Value::as_str).unwrap_or("?").dimmed(),
			entry.get("path").and_then(Value::as_str).unwrap_or("?").bold(),
			entry.get("reason").and_then(Value::as_str).unwrap_or("?"),
			human_remaining(remaining),
		));
	}

	wsync_info!(
		"{} entr(ies) waiting — `wsync backlog restore <id>` puts one back",
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

fn human_remaining(seconds: u64) -> String {
	if seconds >= 3600 {
		return format!("{}h", seconds / 3600);
	}

	if seconds >= 60 {
		return format!("{}m", seconds / 60);
	}

	format!("{seconds}s")
}
