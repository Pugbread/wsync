//! Sync-state commands: `wait-synced` (wait-synced.json) and `verify`
//! (verify.json).
//!
//! `wait-synced` is the barrier an agent puts after a batch of file edits: it
//! returns once Studio has applied every disk change the daemon had seen when
//! it was called (the daemon drains watcher → processor → queue → socket, then
//! asks the plugin a question it only answers after applying what came
//! before — see `server::synced`). `verify` compares disk with Studio's editor
//! text and lists what disagrees. Both talk to daemon routes rather than to a
//! single remote op, and both can legitimately outlast the 5 s op timeout, so
//! they carry their own HTTP deadline.

use anyhow::{bail, Context, Result};
use clap::Parser;
use colored::Colorize;
use serde_json::{json, Value};
use std::time::Duration;

use crate::{
	cli::client::{field, print_json, Client, Targeting},
	constants::WAIT_SYNCED_MAX_TIMEOUT_MS,
	wsync_info, wsync_warn,
};

/// Head-room the HTTP hop gets over the daemon-side deadline, so the daemon's
/// own typed timeout wins the race against a client-side abort
const HTTP_SLACK: Duration = Duration::from_secs(5);

/// A `verify` scan pages through every script in both trees; the barrier
/// deadline alone does not bound it
const VERIFY_SCAN_BUDGET: Duration = Duration::from_secs(600);

/// Entries printed per category in human output (`--raw` carries them all,
/// up to the daemon's own list bound)
const HUMAN_LIST: usize = 20;

fn timeout_ms(seconds: u64) -> Result<u64> {
	if seconds == 0 {
		bail!("--timeout must be at least 1 second");
	}

	Ok(seconds.saturating_mul(1000).min(WAIT_SYNCED_MAX_TIMEOUT_MS))
}

/// `POST` to a daemon route with a caller-chosen deadline. A redirect is a
/// daemon build that predates the route
fn post(client: &Client, path: &str, body: &Value, timeout: Duration) -> Result<Value> {
	let response = reqwest::blocking::Client::builder()
		// Loopback only: never consult platform proxies (see client.rs)
		.no_proxy()
		.timeout(timeout)
		.redirect(reqwest::redirect::Policy::none())
		.build()?
		.post(format!("http://localhost:{}{path}", client.port()))
		.json(body)
		.send()
		.with_context(|| format!("Failed to reach {path} on the daemon at port {}", client.port()))?;

	let status = response.status();

	if status.is_redirection() || status == reqwest::StatusCode::NOT_FOUND {
		bail!(
			"The daemon on port {} does not serve {path} — it predates this command. \
			 Restart it with this wsync build (`wsync daemon restart --project <path>`)",
			client.port()
		);
	}

	let text = response.text().unwrap_or_default();

	serde_json::from_str(&text)
		.with_context(|| format!("Daemon answered {path} with HTTP {status} and a non-JSON body"))
}

fn error_line(reply: &Value) -> String {
	format!(
		"{} [{}]",
		reply
			.pointer("/error/message")
			.and_then(Value::as_str)
			.unwrap_or("the daemon reported failure without detail"),
		reply
			.pointer("/error/code")
			.and_then(Value::as_str)
			.unwrap_or("PLUGIN_ERROR")
	)
}

fn count(value: &Value, pointer: &str) -> u64 {
	value.pointer(pointer).and_then(Value::as_u64).unwrap_or(0)
}

// ---------------------------------------------------------------------------
// wait-synced
// ---------------------------------------------------------------------------

/// Block until Studio has applied every disk change the daemon has seen
#[derive(Parser)]
pub struct WaitSynced {
	#[command(flatten)]
	targeting: Targeting,

	/// Give up after this many seconds
	#[arg(long, value_name = "SECS", default_value_t = 30)]
	timeout: u64,

	/// Print machine-readable JSON
	#[arg(long)]
	raw: bool,
}

impl WaitSynced {
	pub fn main(self) -> Result<()> {
		let timeout_ms = timeout_ms(self.timeout)?;
		let client = Client::connect(&self.targeting)?;

		let reply = post(
			&client,
			"/sync/wait",
			&json!({ "timeoutMs": timeout_ms }),
			Duration::from_millis(timeout_ms) + HTTP_SLACK,
		)?;

		if self.raw {
			print_json(&reply);
		}

		let captured = reply
			.pointer("/backlog/capturedDuringWait")
			.and_then(Value::as_array)
			.cloned()
			.unwrap_or_default();

		if reply.get("ok").and_then(Value::as_bool) != Some(true) {
			bail!(
				"Not synced: {} (stage: {}, {} pending outbound change(s))",
				error_line(&reply),
				field(&reply, "stage"),
				count(&reply, "/pending/total")
			);
		}

		if !self.raw {
			wsync_info!(
				"Synced: {} change(s) confirmed applied in Studio in {} ms",
				count(&reply, "/flushedChanges").to_string().bold(),
				count(&reply, "/durationMs")
			);

			let drained = count(&reply, "/drainedEvents");

			if drained > 0 {
				println!("          {drained} file event(s) were still in the watcher pipeline and were flushed first");
			}
		}

		// Changes the barrier will never deliver: a clash resolved toward
		// Studio moves the disk side to the backlog instead
		if !captured.is_empty() && !self.raw {
			wsync_warn!(
				"{} disk edit(s) clashed with Studio while waiting and were moved to the backlog instead of syncing:",
				captured.len()
			);

			for entry in &captured {
				println!("          {} ({})", field(entry, "path"), field(entry, "reason"));
			}

			println!("          Restore one with `wsync backlog restore <id>` if the disk version should win");
		}

		let suppressed = count(&reply, "/suppressedEvents");

		if suppressed > 0 && !self.raw {
			wsync_warn!(
				"{suppressed} file event(s) touched a path WSync was writing from Studio at that moment and \
				 were treated as its echo; if one was your own edit to that same file, re-save it"
			);
		}

		let parked = count(&reply, "/pending/parkedConflicts");

		if parked > 0 && !self.raw {
			wsync_warn!("{parked} conflict(s) are parked and withheld from Studio until they resolve");
		}

		Ok(())
	}
}

// ---------------------------------------------------------------------------
// verify
// ---------------------------------------------------------------------------

/// Compare disk with Studio's editor text and list what disagrees
#[derive(Parser)]
pub struct Verify {
	#[command(flatten)]
	targeting: Targeting,

	/// Skip the `wait-synced` barrier that runs first (changes still in
	/// flight then show up as mismatches)
	#[arg(long)]
	no_wait: bool,

	/// Deadline for that barrier, in seconds
	#[arg(long, value_name = "SECS", default_value_t = 30)]
	timeout: u64,

	/// Print machine-readable JSON
	#[arg(long)]
	raw: bool,
}

/// One human line per category entry
fn describe(category: &str, entry: &Value) -> String {
	let path = field(entry, "path");
	let fs_path = entry.get("fsPath").and_then(Value::as_str);
	let located = match fs_path {
		Some(fs_path) if !fs_path.is_empty() => format!("{path} ({fs_path})"),
		_ => path.to_owned(),
	};

	match category {
		"mismatch" => format!(
			"{located}: disk {} bytes, Studio {} bytes{}",
			count(entry, "/diskBytes"),
			count(entry, "/studioBytes"),
			if entry.get("tracked").and_then(Value::as_bool) == Some(false) {
				" (matched by path; not tracked by the sync session)"
			} else {
				""
			}
		),
		"staleDaemon" => format!(
			"{located}: disk {} bytes, daemon {} bytes",
			count(entry, "/diskBytes"),
			count(entry, "/daemonBytes")
		),
		"draft" => format!(
			"{path}: editor {} bytes, committed Source {} bytes",
			count(entry, "/editorBytes"),
			count(entry, "/sourceBytes")
		),
		"missingOnDisk" | "missingInStudio" => match entry.get("reason").and_then(Value::as_str) {
			Some(reason) => format!("{located} — {reason}"),
			None => located,
		},
		"backlogStuck" => format!("{path} — {} (entry {})", field(entry, "why"), field(entry, "id")),
		_ => located,
	}
}

fn print_category(report: &Value, key: &str, title: &str, hint: &str) {
	let entries = report.get(key).and_then(Value::as_array).cloned().unwrap_or_default();
	let total = count(report, &format!("/counts/{key}"));

	if total == 0 {
		return;
	}

	println!("{} ({total})", title.bold());

	for entry in entries.iter().take(HUMAN_LIST) {
		println!("  {}", describe(key, entry));
	}

	let shown = HUMAN_LIST.min(entries.len());

	if total as usize > shown {
		println!("  … and {} more (use --raw)", total as usize - shown);
	}

	if !hint.is_empty() {
		println!("  {}", hint.dimmed());
	}
}

fn print_duplicates(report: &Value) {
	let total = count(report, "/counts/duplicateSiblings");

	if total == 0 {
		return;
	}

	println!("{} ({total})", "Duplicate siblings".bold());

	for (side, key) in [
		("Studio", "/duplicateSiblings/studio"),
		("disk", "/duplicateSiblings/daemon"),
	] {
		let groups = report
			.pointer(key)
			.and_then(Value::as_array)
			.cloned()
			.unwrap_or_default();

		for group in groups.iter().take(HUMAN_LIST) {
			let parent = field(group, "parent");

			println!(
				"  {side}: {} × {:?} under {}{}",
				count(group, "/count"),
				field(group, "name"),
				if parent.is_empty() { "the root" } else { parent },
				match group.get("fsPaths").and_then(Value::as_array) {
					Some(paths) if !paths.is_empty() => format!(
						" ({})",
						paths.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", ")
					),
					_ => String::new(),
				}
			);
		}
	}

	println!(
		"  {}",
		"Same-named siblings cannot be told apart by path; rename or merge them".dimmed()
	);
}

impl Verify {
	pub fn main(self) -> Result<()> {
		let timeout_ms = timeout_ms(self.timeout)?;
		let client = Client::connect(&self.targeting)?;

		let report = post(
			&client,
			"/verify",
			&json!({ "noSettle": self.no_wait, "timeoutMs": timeout_ms }),
			Duration::from_millis(timeout_ms) + VERIFY_SCAN_BUDGET,
		)?;

		if self.raw {
			print_json(&report);
		}

		if report.get("ok").and_then(Value::as_bool) != Some(true) {
			bail!("Verify could not run: {}", error_line(&report));
		}

		let clean = report.get("clean").and_then(Value::as_bool) == Some(true);

		if !self.raw {
			if report.pointer("/settle/ok").and_then(Value::as_bool) == Some(false) {
				wsync_warn!(
					"Sync did not settle before the check ({}); changes still in flight may show up below",
					error_line(&report["settle"])
				);
			}

			print_category(
				&report,
				"mismatch",
				"Content mismatch",
				"Studio's editor text differs from disk; `wsync wait-synced` first if edits were in flight",
			);
			print_category(
				&report,
				"missingInStudio",
				"Missing in Studio",
				"On disk and in the daemon's projection, but not in Studio",
			);
			print_category(&report, "missingOnDisk", "Missing on disk", "");
			print_category(
				&report,
				"staleDaemon",
				"Daemon behind disk",
				"The file changed but the daemon never picked it up; touch or re-save the file",
			);
			print_duplicates(&report);
			print_category(
				&report,
				"backlogStuck",
				"Backlog-stuck paths",
				"Re-save the file or reconnect the plugin; `backlog restore` would put back the older, backlogged bytes",
			);

			let hazards = &report["hazards"];
			let errors = count(hazards, "/count");
			let warnings = count(hazards, "/warnings");

			if errors + warnings > 0 {
				println!("{} ({errors} error(s), {warnings} warning(s))", "Disk hazards".bold());

				for finding in hazards
					.get("findings")
					.and_then(Value::as_array)
					.into_iter()
					.flatten()
					.take(HUMAN_LIST)
				{
					println!("  [{}] {}", field(finding, "kind"), field(finding, "message"));

					if !field(finding, "fix").is_empty() {
						println!("    {}", field(finding, "fix").dimmed());
					}
				}
			}

			print_category(
				&report,
				"draft",
				"Unsaved editor drafts (informational)",
				"The open editor differs from the committed Source; sync compares the editor text",
			);

			let checked = count(&report, "/checked/scripts");
			let studio = count(&report, "/checked/studioScripts");

			if clean {
				wsync_info!("Verified {checked} script(s) against Studio ({studio} in Studio): disk and Studio agree");
			}
		}

		if !clean {
			let counts = &report["counts"];
			let problems: u64 = [
				"missingInStudio",
				"missingOnDisk",
				"mismatch",
				"staleDaemon",
				"duplicateSiblings",
				"backlogStuck",
				"hazards",
			]
			.iter()
			.map(|key| counts.get(*key).and_then(Value::as_u64).unwrap_or(0))
			.sum();

			bail!("Verify found {problems} problem(s) between disk and Studio");
		}

		Ok(())
	}
}
