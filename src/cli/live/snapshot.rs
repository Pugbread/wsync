//! `snapshot` — export the live Studio tree to deterministic JSON
//! (snapshot.json).
//!
//! Backed by the daemon's `GET /snapshot` route (which also serves subtree
//! exports by hex ref), so no plugin round-trip is involved — the daemon's
//! own tree projection is the export. This is the most expensive read in the
//! CLI and a backup/debugging tool by contract; `tree`, `ls`, and `query`
//! come first for inspection.
//!
//! The export lands in a file, never on stdout: the default name is
//! `wsync-snapshot-<unix-seconds>.json` under the project (or the current
//! directory when no project resolves), and `--output` may name either a
//! file or a directory to drop the default name into.

use anyhow::{bail, Context, Result};
use clap::Parser;
use colored::Colorize;
use serde_json::json;
use std::{
	fs,
	path::PathBuf,
	time::{SystemTime, UNIX_EPOCH},
};

use crate::{
	cli::client::{print_json, Client, Targeting},
	cli::live::transfer::human_size,
	ext::PathExt,
	wsync_info,
};

/// Export the live Studio tree, properties, attributes, and tags to JSON
#[derive(Parser)]
pub struct Snapshot {
	#[command(flatten)]
	targeting: Targeting,

	/// Hex ref of a subtree root; omit for the whole DataModel projection
	#[arg(long = "ref", value_name = "HEX")]
	instance: Option<String>,

	/// Output file, or a directory for the default file name
	#[arg(short, long, value_name = "FILE-OR-DIR")]
	output: Option<PathBuf>,

	/// Print machine-readable JSON (the export summary, never the export)
	#[arg(long)]
	raw: bool,
}

impl Snapshot {
	pub fn main(self) -> Result<()> {
		let client = Client::connect(&self.targeting)?;

		// The output location is settled before the expensive read, so a full
		// tree walk never renders into an unwritable destination
		let output = self.resolve_output(&client)?;

		if let Some(parent) = output.parent() {
			fs::create_dir_all(parent)
				.with_context(|| format!("Failed to create the output directory {}", parent.to_string()))?;
		}

		let route = match &self.instance {
			Some(reference) => format!("/snapshot?ref={reference}"),
			None => "/snapshot".to_owned(),
		};

		let endpoint = client.get_endpoint(&route)?;

		if endpoint.unavailable() {
			bail!(
				"This daemon build does not serve /snapshot — update the daemon \
				 (`wsync daemon restart --project <path>`)"
			);
		}

		if endpoint.status == 404 {
			bail!(
				"The daemon has no instance with ref {} — refs come from `wsync query --format refs`",
				self.instance.as_deref().unwrap_or("<none>")
			);
		}

		let snapshot = endpoint.json("/snapshot")?;
		let rendered = serde_json::to_string_pretty(snapshot)?;

		fs::write(&output, &rendered).with_context(|| format!("Failed to write {}", output.to_string()))?;

		if self.raw {
			print_json(&json!({
				"ok": true,
				"path": output.to_string(),
				"bytes": rendered.len() as u64,
				"ref": self.instance,
			}));

			return Ok(());
		}

		wsync_info!(
			"Exported the live tree → {} ({})",
			output.to_string().bold(),
			human_size(rendered.len() as u64)
		);

		Ok(())
	}

	/// `--output` names a file, or a directory to receive the default name;
	/// the default lands next to the project (snapshot.json)
	fn resolve_output(&self, client: &Client) -> Result<PathBuf> {
		let unix = SystemTime::now()
			.duration_since(UNIX_EPOCH)
			.map(|elapsed| elapsed.as_secs())
			.unwrap_or_default();
		let default_name = format!("wsync-snapshot-{unix}.json");

		let output = match &self.output {
			Some(output) if output.is_dir() => output.join(&default_name),
			Some(output) => output.clone(),
			None => {
				let target = &client.target;

				if target.project_exists {
					target.project_path.get_parent().join(&default_name)
				} else {
					PathBuf::from(&default_name)
				}
			}
		};

		output.resolve()
	}
}
