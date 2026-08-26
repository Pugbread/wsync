//! `refresh` — regenerate the agent docs without starting the daemon
//! (refresh.json; Design §10.6), plus the plugin-connect auto-refresh hook.
//!
//! Both paths run the identical pipeline: load the project from disk, resolve
//! the workspace config, render through [`crate::docsgen`], and merge with
//! [`Preserve::UserNotes`] so hand-written notes outside the marker blocks
//! survive byte for byte. The `Shipped` set is derived from the running
//! binary's own clap command tree — never a hand-typed list — so the docs can
//! only ever describe commands this exact build actually parses.
//!
//! The connect hook ([`auto_refresh_on_connect`]) is called from the two
//! plugin-connect sites (WS hello accepted, msgpack subscribe). It runs on
//! the blocking pool, never blocks the handshake, reports failure as a
//! warning only, and debounces per workspace: a reconnect storm rewrites
//! nothing, because a refresh younger than [`AUTO_REFRESH_DEBOUNCE`] is
//! skipped outright.

use anyhow::{Context, Result};
use clap::{CommandFactory, Parser};
use lazy_static::lazy_static;
use log::{debug, warn};
use serde_json::{json, Value};
use std::{
	collections::HashMap,
	path::{Path, PathBuf},
	sync::{Arc, Mutex},
	time::{Duration, Instant},
};

use crate::{
	cli::client::print_json,
	cli::registry_bundle,
	config::Config,
	constants::PROTOCOL_VERSION,
	core::Core,
	daemon,
	docsgen::{self, DocsInput, EnvFacts, FileOutcome, Preserve, ProjectFacts, RegistryFacts, Shipped, WriteStatus},
	ext::PathExt,
	lock,
	project::{self, Project},
	wsync_info,
};

/// Reconnect-storm guard: a workspace refreshed more recently than this is
/// skipped by the connect hook (`wsync refresh` itself never debounces)
pub const AUTO_REFRESH_DEBOUNCE: Duration = Duration::from_secs(30);

/// Regenerate the WSync agent docs for a project without starting the daemon
#[derive(Parser)]
pub struct Refresh {
	/// Project path (defaults to the current directory)
	#[arg(long, value_name = "PATH")]
	project: Option<PathBuf>,

	/// Print the per-file outcomes as one JSON line
	#[arg(long)]
	raw: bool,
}

impl Refresh {
	pub fn main(self) -> Result<()> {
		let project_path = project::resolve(self.project.clone().unwrap_or_default())?;

		if !project_path.exists() {
			anyhow::bail!(
				"No project file at {} — run `wsync init` first, or pass --project",
				project_path.to_string()
			);
		}

		let outcomes = refresh_workspace(&project_path)?;

		if self.raw {
			print_json(&json!({
				"ok": true,
				"files": outcomes.iter().map(outcome_record).collect::<Vec<_>>(),
			}));

			return Ok(());
		}

		for outcome in &outcomes {
			let detail = match &outcome.status {
				WriteStatus::Skipped(reason) => format!(" — {reason}"),
				_ => String::new(),
			};

			println!(
				"{:<10} {}{detail}",
				outcome.status.as_str(),
				outcome.path.to_string_lossy()
			);
		}

		let changed = outcomes
			.iter()
			.filter(|outcome| matches!(outcome.status, WriteStatus::Created | WriteStatus::Updated))
			.count();

		wsync_info!("Agent docs refreshed: {changed} of {} file(s) changed", outcomes.len());

		Ok(())
	}
}

fn outcome_record(outcome: &FileOutcome) -> Value {
	let mut record = json!({
		"file": outcome.file.relative_path(),
		"path": outcome.path.to_string_lossy(),
		"status": outcome.status.as_str(),
	});

	if let WriteStatus::Skipped(reason) = &outcome.status {
		record["reason"] = json!(reason);
	}

	record
}

/// The complete refresh pipeline for one project file, shared by the CLI
/// command and the connect hook so the two can never render different docs
pub fn refresh_workspace(project_path: &Path) -> Result<Vec<FileOutcome>> {
	let workspace = project_path.get_parent();

	Config::load_workspace(workspace);

	let project = Project::load(project_path)
		.with_context(|| format!("Failed to load the project file {}", project_path.to_string()))?;

	let config = Config::new().clone();
	let project_facts = ProjectFacts::from_project(&project, &config);

	let registry = RegistryFacts::from_generated_json(registry_bundle::CLIENT_COMMANDS)
		.context("The embedded command registry does not parse — this binary was built from a broken docs bundle")?
		.with_shipped(Shipped::only(shipped_commands()));

	let state_dir = daemon::state_dir(None)?;
	let env = EnvFacts::new(env!("CARGO_PKG_VERSION"), PROTOCOL_VERSION, &state_dir);

	let docs = docsgen::render(&DocsInput::new(&project_facts, &registry, &env));

	docsgen::write_all(workspace, &docs, Preserve::UserNotes)
}

/// The real clap command list of this binary — the only honest source for
/// `Shipped`: a documented command missing from this set renders as "not
/// built yet", and nothing hand-maintained can drift
pub fn shipped_commands() -> Vec<String> {
	super::Cli::command()
		.get_subcommands()
		.map(|command| command.get_name().to_owned())
		.filter(|name| name != "help")
		.collect()
}

lazy_static! {
	/// Last successful auto-refresh per workspace, for the debounce
	static ref LAST_AUTO_REFRESH: Mutex<HashMap<PathBuf, Instant>> = Mutex::new(HashMap::new());
}

/// The plugin-connect hook. Spawns onto the blocking pool immediately (the
/// caller is a connection handshake and must not wait on file I/O), skips
/// workspaces refreshed within [`AUTO_REFRESH_DEBOUNCE`], and reports any
/// failure as a warning — a broken docs render never costs a connection
pub fn auto_refresh_on_connect(core: Arc<Core>) {
	tokio::task::spawn_blocking(move || {
		let project_path = core.project().path.clone();

		if !debounce_allows(&project_path) {
			debug!(
				"Skipping the agent-doc auto-refresh for {} (refreshed under {}s ago)",
				project_path.to_string(),
				AUTO_REFRESH_DEBOUNCE.as_secs()
			);

			return;
		}

		match refresh_workspace(&project_path) {
			Ok(outcomes) => {
				let changed = outcomes
					.iter()
					.filter(|outcome| matches!(outcome.status, WriteStatus::Created | WriteStatus::Updated))
					.count();

				debug!("Agent docs auto-refreshed on plugin connect ({changed} file(s) changed)");
			}
			Err(err) => warn!("Agent-doc auto-refresh failed: {err:#}"),
		}
	});
}

/// True when this workspace may refresh now; records the attempt so a
/// reconnect storm collapses into one refresh per window
fn debounce_allows(project_path: &Path) -> bool {
	let mut last = lock!(LAST_AUTO_REFRESH);
	let now = Instant::now();

	if let Some(previous) = last.get(project_path) {
		if now.duration_since(*previous) < AUTO_REFRESH_DEBOUNCE {
			return false;
		}
	}

	last.insert(project_path.to_path_buf(), now);

	true
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn the_clap_tree_is_the_shipped_source() {
		let commands = shipped_commands();

		// A representative spread across the surface — including this change's
		// own commands, so `refresh` can never render itself as unbuilt
		for name in [
			"serve", "get", "set", "capture", "playtest", "run", "plan", "refresh", "auth", "snapshot", "backlog",
			"services", "open",
		] {
			assert!(commands.iter().any(|command| command == name), "missing `{name}`");
		}

		assert!(!commands.iter().any(|command| command == "help"));
	}

	#[test]
	fn the_debounce_holds_within_one_window() {
		let path = PathBuf::from("/tmp/wsync-debounce-test-fixture");

		assert!(debounce_allows(&path));
		assert!(!debounce_allows(&path));

		// A different workspace is unaffected
		assert!(debounce_allows(&PathBuf::from("/tmp/wsync-debounce-test-other")));
	}
}
