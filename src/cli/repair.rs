//! `repair` — live-tree access validation and generated-metadata rebuilds
//! (repair.json).
//!
//! `repair tree` is read-only: it walks the whole access chain a live
//! command depends on — the project parses, a daemon answers, the Studio
//! plugin answers, the daemon's `/snapshot` export walks, and a sample of
//! snapshot paths round-trips through the daemon's path index — and reports
//! every check, doctor-style, instead of stopping at the first failure.
//! The command exits non-zero when any check fails.
//!
//! `repair sourcemap` rebuilds the luau-lsp sourcemap from the on-disk
//! project through the engine's own sourcemap machinery, entirely
//! in-process: no daemon is contacted. It is the recovery path when the
//! generated file is missing or stale; `wsync sourcemap` remains the
//! primary command (and can watch).

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use colored::Colorize;
use serde_json::{json, Value};
use std::{fs, path::PathBuf};

use crate::{
	cli::client::{print_json, Client, Target, Targeting},
	config::Config,
	core::Core,
	ext::PathExt,
	project::{self, Project},
	wsync_info,
};

/// How many snapshot paths the path-index check samples
const PATH_SAMPLES: usize = 25;

/// Check live tree access and rebuild generated WSync metadata
#[derive(Parser)]
pub struct Repair {
	#[command(subcommand)]
	command: RepairCommand,
}

#[derive(Subcommand)]
enum RepairCommand {
	/// Validate that the live Studio tree can be read (read-only)
	Tree(RepairTree),
	/// Rebuild the luau-lsp sourcemap from the on-disk project
	Sourcemap(RepairSourcemap),
}

impl Repair {
	pub fn main(self) -> Result<()> {
		match self.command {
			RepairCommand::Tree(command) => command.main(),
			RepairCommand::Sourcemap(command) => command.main(),
		}
	}
}

// ---------------------------------------------------------------------------
// repair tree
// ---------------------------------------------------------------------------

/// One reported check — the same pass/fail/skip vocabulary `doctor` uses
struct Check {
	id: &'static str,
	status: &'static str,
	detail: String,
}

impl Check {
	fn pass(id: &'static str, detail: impl Into<String>) -> Self {
		Self {
			id,
			status: "pass",
			detail: detail.into(),
		}
	}

	fn fail(id: &'static str, detail: impl Into<String>) -> Self {
		Self {
			id,
			status: "fail",
			detail: detail.into(),
		}
	}

	fn skip(id: &'static str, detail: impl Into<String>) -> Self {
		Self {
			id,
			status: "skip",
			detail: detail.into(),
		}
	}
}

#[derive(Parser)]
struct RepairTree {
	#[command(flatten)]
	targeting: Targeting,

	/// How deep the path-index sampling walks the snapshot (0 = services
	/// only)
	#[arg(long, value_name = "N", default_value = "3")]
	depth: u32,

	/// Print machine-readable JSON
	#[arg(long)]
	raw: bool,
}

impl RepairTree {
	fn main(self) -> Result<()> {
		let target = Target::resolve(&self.targeting)?;
		let mut checks: Vec<Check> = Vec::new();

		// 1. The project file parses
		if !target.project_exists {
			checks.push(Check::fail(
				"project",
				format!("no project file at {}", target.project_path.to_string()),
			));
		} else {
			match Project::load(&target.project_path) {
				Ok(project) => checks.push(Check::pass(
					"project",
					format!("{} parses ({})", target.project_path.get_name(), project.name),
				)),
				Err(err) => checks.push(Check::fail(
					"project",
					format!("the project file does not parse: {err}"),
				)),
			}
		}

		// 2. A daemon answers on the resolved port
		let hello = target.probe().ok();
		let client = match &hello {
			Some(hello) => {
				checks.push(Check::pass(
					"daemon",
					format!(
						"port {} answers (v{}, PID {}, from the {})",
						target.port,
						hello.version,
						hello.pid,
						target.port_source.as_str()
					),
				));

				Client::open_probed(&target, Some(hello.clone()))
			}
			None => {
				checks.push(Check::fail(
					"daemon",
					format!(
						"no daemon answers on port {} (from the {}) — start one with `wsync daemon start --project <path>`",
						target.port,
						target.port_source.as_str()
					),
				));

				None
			}
		};

		// 3. The Studio plugin answers a ping through that daemon
		match &client {
			Some(client) => match client.request("ping", json!({})) {
				Ok(envelope) if envelope.ok => checks.push(Check::pass(
					"plugin",
					format!("Studio plugin answers ({} ms)", envelope.duration_ms.unwrap_or(0)),
				)),
				Ok(envelope) => checks.push(Check::fail(
					"plugin",
					format!("{} [{}]", envelope.error_message(), envelope.error_code()),
				)),
				Err(err) => checks.push(Check::fail("plugin", err.to_string())),
			},
			None => checks.push(Check::skip("plugin", "no daemon to ask")),
		}

		// 4. The daemon's snapshot export walks end to end
		let snapshot = client
			.as_ref()
			.and_then(|client| self.check_snapshot(client, &mut checks));

		// 5. A sample of snapshot paths resolves through the path index
		match (&client, &snapshot) {
			(Some(client), Some(snapshot)) => self.check_path_index(client, snapshot, &mut checks),
			_ => checks.push(Check::skip("path-index", "no snapshot to sample")),
		}

		let failed: Vec<&Check> = checks.iter().filter(|check| check.status == "fail").collect();

		if self.raw {
			print_json(&json!({
				"ok": failed.is_empty(),
				"project": target.project_path.to_string(),
				"port": target.port,
				"checks": checks
					.iter()
					.map(|check| json!({ "id": check.id, "status": check.status, "detail": check.detail }))
					.collect::<Vec<Value>>(),
			}));
		} else {
			for check in &checks {
				let status = match check.status {
					"pass" => "pass".green(),
					"fail" => "fail".red(),
					_ => "skip".dimmed(),
				};

				println!("{status}  {:<10} {}", check.id, check.detail);
			}
		}

		if !failed.is_empty() {
			let names: Vec<&str> = failed.iter().map(|check| check.id).collect();

			bail!(
				"{} of {} check(s) failed: {}",
				failed.len(),
				checks.len(),
				names.join(", ")
			);
		}

		if !self.raw {
			wsync_info!("Live tree access is healthy ({} checks)", checks.len());
		}

		Ok(())
	}

	/// Fetches `/snapshot` and proves the JSON tree walks; the parsed export
	/// is handed on for path sampling
	fn check_snapshot(&self, client: &Client, checks: &mut Vec<Check>) -> Option<Value> {
		let endpoint = match client.get_endpoint("/snapshot") {
			Ok(endpoint) => endpoint,
			Err(err) => {
				checks.push(Check::fail("snapshot", err.to_string()));

				return None;
			}
		};

		if endpoint.unavailable() {
			checks.push(Check::fail(
				"snapshot",
				"this daemon build does not serve /snapshot — update the daemon",
			));

			return None;
		}

		let snapshot = match endpoint.json("/snapshot") {
			Ok(snapshot) => snapshot.clone(),
			Err(err) => {
				checks.push(Check::fail("snapshot", err.to_string()));

				return None;
			}
		};

		let (nodes, depth) = walk_stats(&snapshot, 0);

		checks.push(Check::pass(
			"snapshot",
			format!("walked {nodes} node(s), max depth {depth}"),
		));

		Some(snapshot)
	}

	/// Samples snapshot paths and round-trips each through the daemon's
	/// `path` op — a resolved answer whose `studioPath` names the same
	/// instance is a sane index entry
	fn check_path_index(&self, client: &Client, snapshot: &Value, checks: &mut Vec<Check>) {
		let mut samples: Vec<String> = Vec::new();

		collect_paths(snapshot, "", 0, self.depth, &mut samples);

		if samples.is_empty() {
			checks.push(Check::skip("path-index", "the snapshot has no sampleable paths"));

			return;
		}

		let mut broken: Vec<String> = Vec::new();

		for sample in &samples {
			let resolved = client
				.request("path", json!({ "target": sample, "from": "studio" }))
				.ok()
				.filter(|envelope| envelope.ok)
				.map(|envelope| envelope.value);

			let round_trip = resolved
				.as_ref()
				.and_then(|value| value.get("studioPath"))
				.and_then(Value::as_str);

			if round_trip != Some(sample.as_str()) {
				broken.push(sample.clone());
			}
		}

		if broken.is_empty() {
			checks.push(Check::pass(
				"path-index",
				format!("{} sampled path(s) resolve and round-trip", samples.len()),
			));
		} else {
			checks.push(Check::fail(
				"path-index",
				format!(
					"{} of {} sampled path(s) do not resolve: {}",
					broken.len(),
					samples.len(),
					broken.iter().take(3).map(String::as_str).collect::<Vec<_>>().join(", ")
				),
			));
		}
	}
}

/// Node count and maximum depth of a snapshot export
fn walk_stats(node: &Value, depth: u32) -> (u64, u32) {
	let mut nodes = 1;
	let mut max_depth = depth;

	if let Some(children) = node.get("children").and_then(Value::as_array) {
		for child in children {
			let (child_nodes, child_depth) = walk_stats(child, depth + 1);

			nodes += child_nodes;
			max_depth = max_depth.max(child_depth);
		}
	}

	(nodes, max_depth)
}

/// Collects up to [`PATH_SAMPLES`] `/`-joined studio paths from the export,
/// shallowest first. Names containing `/` cannot be expressed as a path-op
/// target and are skipped rather than sampled wrong
fn collect_paths(node: &Value, prefix: &str, depth: u32, max_depth: u32, samples: &mut Vec<String>) {
	if samples.len() >= PATH_SAMPLES {
		return;
	}

	let Some(children) = node.get("children").and_then(Value::as_array) else {
		return;
	};

	for child in children {
		if samples.len() >= PATH_SAMPLES {
			return;
		}

		let Some(name) = child.get("name").and_then(Value::as_str) else {
			continue;
		};

		if name.contains('/') {
			continue;
		}

		let path = if prefix.is_empty() {
			name.to_owned()
		} else {
			format!("{prefix}/{name}")
		};

		samples.push(path.clone());

		if depth < max_depth {
			collect_paths(child, &path, depth + 1, max_depth, samples);
		}
	}
}

// ---------------------------------------------------------------------------
// repair sourcemap
// ---------------------------------------------------------------------------

#[derive(Parser)]
struct RepairSourcemap {
	/// Project path (defaults to the current directory)
	#[arg(long, value_name = "PATH")]
	project: Option<PathBuf>,

	/// Output path (defaults to sourcemap.json next to the project file)
	#[arg(short, long, value_name = "PATH")]
	output: Option<PathBuf>,

	/// Print machine-readable JSON
	#[arg(long)]
	raw: bool,
}

impl RepairSourcemap {
	fn main(self) -> Result<()> {
		let project_path = project::resolve(self.project.clone().unwrap_or_default())?;

		if !project_path.exists() {
			bail!(
				"No project files found in {}",
				project_path.get_parent().to_string().bold()
			);
		}

		Config::load_workspace(project_path.get_parent());

		let output = match &self.output {
			Some(output) => output.resolve()?,
			None => project_path.get_parent().join("sourcemap.json"),
		};

		if let Some(parent) = output.parent() {
			fs::create_dir_all(parent)
				.with_context(|| format!("Failed to create the output directory {}", parent.to_string()))?;
		}

		// The engine's own sourcemap machinery, in-process: load the
		// project, project it into a tree, and serialize the walk
		let project = Project::load(&project_path)?;
		let core = Core::new(project, false)?;

		core.sourcemap(Some(output.clone()), false)?;

		let bytes = fs::metadata(&output).map(|meta| meta.len()).unwrap_or(0);

		if self.raw {
			print_json(&json!({
				"ok": true,
				"project": project_path.to_string(),
				"path": output.to_string(),
				"bytes": bytes,
			}));

			return Ok(());
		}

		wsync_info!(
			"Rebuilt the sourcemap of {} at {} ({} bytes)",
			project_path.to_string().bold(),
			output.to_string().bold(),
			bytes
		);

		Ok(())
	}
}
