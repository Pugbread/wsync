//! Change-history control: `undo`, `redo` (undo.json, redo.json).
//!
//! Each wraps exactly one argument-free remote op that drives Studio's own
//! undo stack, so the whole family is discovery + one round trip. The ops
//! acknowledge a *request*: ChangeHistoryService reports nothing about what
//! an undo actually reversed (undo.json), so the human output says "asked",
//! never "done".

use anyhow::Result;
use clap::Parser;
use colored::Colorize;
use serde_json::json;

use crate::{
	cli::client::{print_ok, Client, Targeting},
	wsync_info,
};

/// Ask Studio to undo one change-history entry
#[derive(Parser)]
pub struct Undo {
	#[command(flatten)]
	targeting: Targeting,

	/// Print machine-readable JSON
	#[arg(long)]
	raw: bool,
}

impl Undo {
	pub fn main(self) -> Result<()> {
		step(&self.targeting, "undo", self.raw)
	}
}

/// Ask Studio to redo the last undone change-history entry
#[derive(Parser)]
pub struct Redo {
	#[command(flatten)]
	targeting: Targeting,

	/// Print machine-readable JSON
	#[arg(long)]
	raw: bool,
}

impl Redo {
	pub fn main(self) -> Result<()> {
		step(&self.targeting, "redo", self.raw)
	}
}

/// `undo`/`redo` differ only in the op name: ChangeHistoryService reports
/// nothing about what moved, so both answer `{requested:true}` and the CLI
/// reports the request, not an outcome
fn step(targeting: &Targeting, op: &str, raw: bool) -> Result<()> {
	let client = Client::connect(targeting)?;
	let value = client.value(op, json!({}), raw)?;

	if raw {
		print_ok(&value);

		return Ok(());
	}

	wsync_info!(
		"Asked Studio to {op} one change-history entry — inspect the result with `{}`",
		"wsync tree".bold()
	);

	Ok(())
}
