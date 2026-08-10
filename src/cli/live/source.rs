//! `source` — the live script source, or the file backing an instance on
//! disk (source.json).
//!
//! Live source reads `Source` through the plugin's `ScriptEditorService`
//! path, which is a *loose* divergence diagnostic: an open editor draft makes
//! `Source` unreliable, so a mismatch against disk is not by itself proof of
//! divergence.
//!
//! `--disk` never needs the daemon. It resolves the instance through the
//! project's own middleware projection in-process, so `.model.json`,
//! `.rbxm`, and `.data.json`-backed instances answer as well as plain script
//! files.

use anyhow::{bail, Context, Result};
use clap::Parser;
use colored::Colorize;
use serde_json::{json, Value};
use std::{fs, path::PathBuf};

use crate::{
	cli::client::{print_json, Client, Target, Targeting},
	config::Config,
	core::{meta::SourceEntry, Core},
	ext::PathExt,
	project::Project,
};

/// Print an instance's script source from live Studio, or the content of the
/// file backing it on disk
#[derive(Parser)]
#[command(
	long_about = "Print an instance's script source from live Studio, or the content of the file backing it on disk.\n\n\
	              Live source is read through the plugin's ScriptEditorService path and is a loose divergence \
	              diagnostic only: while an editor draft is open for that script the live Source is unreliable, so a \
	              mismatch against disk is not by itself proof of divergence. To verify an edit, use the narrowest \
	              `wsync lint --path` plus a local file read.\n\n\
	              --disk resolves the target through the sync middleware and needs no daemon."
)]
pub struct Source {
	#[command(flatten)]
	targeting: Targeting,

	/// Studio path, `/`-separated
	#[arg(long, value_name = "STUDIO-PATH")]
	path: String,

	/// Read the file backing the instance on disk instead of live Studio
	#[arg(long)]
	disk: bool,

	/// Print machine-readable JSON
	#[arg(long)]
	raw: bool,
}

impl Source {
	pub fn main(self) -> Result<()> {
		if self.disk {
			return self.read_disk();
		}

		let client = Client::connect(&self.targeting)?;
		let value = client.value("get", json!({ "path": self.path, "prop": "Source" }), self.raw)?;

		let source = match &value {
			Value::String(source) => source.clone(),
			Value::Null => bail!("{} has no readable Source property", self.path),
			// A codec-tagged value means the property is not a plain string
			other => bail!("Source of {} is not a string value: {other}", self.path),
		};

		if self.raw {
			print_json(&json!({
				"ok": true,
				"path": self.path,
				"from": "studio",
				"file": null,
				"source": source,
			}));

			return Ok(());
		}

		print!("{source}");

		if !source.ends_with('\n') {
			println!();
		}

		Ok(())
	}

	/// Resolves the instance through the project's middleware projection and
	/// prints the file backing it
	fn read_disk(self) -> Result<()> {
		let target = Target::resolve(&self.targeting)?;

		if !target.project_exists {
			bail!(
				"No project file at {} — `--disk` reads through the project's own sync rules",
				target.project_path.to_string().bold()
			);
		}

		Config::load_workspace(target.project_path.get_parent());

		let project = Project::load(&target.project_path)?;
		let core = Core::new(project, false)?;
		let file = resolve_file(&core, &self.path)?;

		let source = fs::read_to_string(&file)
			.with_context(|| format!("Failed to read {} (is it a binary model file?)", file.to_string()))?;

		if self.raw {
			print_json(&json!({
				"ok": true,
				"path": self.path,
				"from": "disk",
				"file": file.to_string(),
				"source": source,
			}));

			return Ok(());
		}

		print!("{source}");

		if !source.ends_with('\n') {
			println!();
		}

		Ok(())
	}
}

/// Walks the project's projected tree by Studio path segments and returns
/// the file the instance is backed by
fn resolve_file(core: &Core, studio_path: &str) -> Result<PathBuf> {
	let tree = core.tree();
	let mut current = tree.inner().root_ref();
	let mut walked = String::new();

	for segment in studio_path.split('/').filter(|segment| !segment.is_empty()) {
		let instance = tree
			.get_instance(current)
			.with_context(|| format!("Projected tree is missing the instance at {walked}"))?;

		let next = instance
			.children()
			.iter()
			.copied()
			.find(|child| tree.get_instance(*child).is_some_and(|child| child.name == segment));

		match next {
			Some(next) => current = next,
			None => bail!(
				"`{segment}` is not projected to disk under `{}`. The live tree is wider than the disk projection — \
				 drop --disk to read the Studio-side source",
				if walked.is_empty() { "the DataModel" } else { &walked }
			),
		}

		if !walked.is_empty() {
			walked.push('/');
		}

		walked.push_str(segment);
	}

	if walked.is_empty() {
		bail!("--path must name an instance; the DataModel root has no file behind it");
	}

	let meta = tree
		.get_meta(current)
		.with_context(|| format!("No sync metadata recorded for {walked}"))?;

	// File first (the script or model itself), then the sidecar, then the
	// project file for `$path`-less project nodes
	let entries = meta.source.relevant();
	let file = entries
		.iter()
		.find(|entry| matches!(entry, SourceEntry::File(_)))
		.or_else(|| entries.iter().find(|entry| matches!(entry, SourceEntry::Data(_))))
		.or_else(|| entries.iter().find(|entry| matches!(entry, SourceEntry::Project(_))));

	match file {
		Some(entry) => Ok(entry.path().to_owned()),
		None => {
			let paths: Vec<String> = entries.iter().map(|entry| entry.path().to_string()).collect();

			if paths.is_empty() {
				bail!("{walked} is not backed by any file on disk");
			}

			bail!(
				"{walked} is backed by a directory, not a file ({}) — name a child instance instead",
				paths.join(", ")
			)
		}
	}
}
