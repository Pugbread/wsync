//! Handshake and diagnostics: `ping`, `version`, `status`, `doctor`,
//! `capabilities` (ping.json, version.json, status.json, doctor.json,
//! capabilities.json).
//!
//! `ping` and `capabilities` require a connected plugin. `version` tolerates
//! a missing plugin (version.json: the plugin version is reported "when
//! reachable") but not a missing daemon. `status` and `doctor` tolerate
//! everything — they are the commands an agent runs precisely because
//! something may be broken, so they always answer and `status` always exits
//! zero.

use anyhow::{bail, Result};
use clap::Parser;
use colored::Colorize;
use serde_json::{json, Value};
use std::{path::PathBuf, time::Instant};

use crate::{
	cli::client::{human_value, print_json, Client, Envelope, Hello, Target, Targeting},
	constants::PROTOCOL_VERSION,
	ext::PathExt,
	project::Project,
	wsync_info, wsync_warn,
};

/// The CLI's own build identity. Design §10.5 asks `version` to report the
/// build commit and dirty state as well; this build records neither (nothing
/// stamps them into the binary), so they are reported as `null` rather than
/// guessed
const CLI_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Round-trip a lightweight request to the Studio plugin and report latency
#[derive(Parser)]
pub struct Ping {
	#[command(flatten)]
	targeting: Targeting,

	/// Print machine-readable JSON
	#[arg(long)]
	raw: bool,
}

impl Ping {
	pub fn main(self) -> Result<()> {
		let client = Client::connect(&self.targeting)?;

		let started = Instant::now();
		let envelope = client.request("ping", json!({}))?;
		let round_trip = started.elapsed().as_millis() as u64;

		let value = envelope.clone().into_value(self.raw)?;

		if self.raw {
			print_json(&json!({
				"ok": true,
				"pong": value.get("pong").and_then(Value::as_bool).unwrap_or(true),
				"port": client.port(),
				"durationMs": envelope.duration_ms,
				"roundTripMs": round_trip,
				"protocol": envelope.protocol,
			}));

			return Ok(());
		}

		wsync_info!(
			"Studio plugin answered in {} ms ({} ms round trip, daemon port {})",
			envelope.duration_ms.unwrap_or(round_trip).to_string().bold(),
			round_trip.to_string().bold(),
			client.port().to_string().bold()
		);

		Ok(())
	}
}

/// Print the CLI, daemon, and (when reachable) Studio plugin versions
#[derive(Parser)]
pub struct Version {
	#[command(flatten)]
	targeting: Targeting,

	/// Print machine-readable JSON
	#[arg(long)]
	raw: bool,
}

impl Version {
	pub fn main(self) -> Result<()> {
		let client = Client::connect(&self.targeting)?;
		let hello = client.hello.clone();

		let plugin = client.request("version", json!({}))?;
		let plugin_value = if plugin.ok { plugin.value.clone() } else { Value::Null };

		let plugin_protocol = plugin_value.get("protocol").and_then(Value::as_u64);
		let protocol_match = plugin_protocol.is_none_or(|protocol| protocol == u64::from(hello.protocol));

		if self.raw {
			print_json(&json!({
				"ok": true,
				"cli": { "version": CLI_VERSION, "protocol": PROTOCOL_VERSION, "commit": null, "dirty": null },
				"daemon": {
					"name": hello.name,
					"version": hello.version,
					"protocol": hello.protocol,
					"port": hello.port,
					"pid": hello.pid,
					"bootId": hello.boot_id,
					"managedBy": hello.managed_by,
					"commit": null,
					"dirty": null,
				},
				"plugin": {
					"connected": plugin.ok,
					"version": plugin_value.get("pluginVersion"),
					"protocol": plugin_protocol,
					"studioVersion": plugin_value.get("studioVersion"),
					"error": (!plugin.ok).then(|| json!({
						"code": plugin.error_code(),
						"message": plugin.error_message(),
					})),
				},
				"protocolMatch": protocol_match,
			}));

			return Ok(());
		}

		println!("CLI     {CLI_VERSION} (protocol {PROTOCOL_VERSION}, commit unrecorded)");
		println!(
			"Daemon  {} (protocol {}, port {}, PID {}, commit unrecorded)",
			hello.version, hello.protocol, hello.port, hello.pid
		);

		if plugin.ok {
			println!(
				"Plugin  {} (protocol {}, Studio {})",
				plugin_value
					.get("pluginVersion")
					.and_then(Value::as_str)
					.unwrap_or("unknown"),
				plugin_protocol.map_or("?".to_owned(), |protocol| protocol.to_string()),
				plugin_value
					.get("studioVersion")
					.and_then(Value::as_str)
					.unwrap_or("unknown"),
			);
		} else {
			println!("Plugin  not reachable ({})", plugin.error_message());
		}

		if !protocol_match {
			wsync_warn!("Plugin and daemon report different protocol numbers — this is a mixed or unreleased install");
		}

		Ok(())
	}
}

/// Summarize daemon reachability, plugin handshake, project config,
/// sourcemap, and write-log health
#[derive(Parser)]
pub struct Status {
	#[command(flatten)]
	targeting: Targeting,

	/// Print machine-readable JSON
	#[arg(long)]
	raw: bool,
}

impl Status {
	pub fn main(self) -> Result<()> {
		let target = Target::resolve(&self.targeting)?;
		let hello = target.probe().ok();

		let project = if target.project_exists {
			Project::load(&target.project_path).ok()
		} else {
			None
		};

		let sourcemap = target.sourcemap();

		// The daemon owns the audit log: it is the process that appends to it,
		// so its `/hello` location wins over the CLI's state-dir guess. A
		// daemon build that reports none leaves the surface `pending` rather
		// than letting `status` assert a path nothing confirmed
		let reported = hello.as_ref().and_then(|hello| hello.writes_log.clone());
		let writes_log = reported.as_ref().map_or_else(|| target.writes_log(), PathBuf::from);

		// "Running" is about the daemon that actually answers serving *this*
		// project — a daemon started outside the record machinery (plain
		// `wsync serve`) is running. A record that names a different boot is
		// separately reported as stale rather than folded into `running`
		let running = hello.as_ref().is_some_and(|hello| {
			target
				.canonical
				.as_ref()
				.is_none_or(|canonical| &hello.canonical_project == canonical)
		});
		let stale = target.record.is_some() && hello.as_ref().is_none_or(|hello| !target.record_is_live(hello));

		let plugin =
			Client::open_probed(&target, hello.clone()).and_then(|client| client.request("version", json!({})).ok());

		if self.raw {
			print_json(&json!({
				"ok": true,
				"project": {
					"path": target.project_path.to_string(),
					"canonicalProject": target.canonical,
					"exists": target.project_exists,
					"parses": project.is_some(),
					"name": project.as_ref().map(|project| project.name.clone()),
					"gameId": project.as_ref().and_then(|project| project.game_id),
					"placeIds": project.as_ref().map(|project| project.place_ids.clone()),
				},
				"daemon": {
					"port": target.port,
					"portSource": target.port_source.as_str(),
					"recorded": target.record.is_some(),
					"reachable": hello.is_some(),
					"running": running,
					"stale": stale,
					"pid": hello.as_ref().map(|hello| hello.pid),
					"bootId": hello.as_ref().map(|hello| hello.boot_id.clone()),
					"version": hello.as_ref().map(|hello| hello.version.clone()),
					"protocol": hello.as_ref().map(|hello| hello.protocol),
					"managedBy": hello.as_ref().map(|hello| hello.managed_by.clone()),
					// The daemon's own view of the place identity, which is
					// what the plugin matches against when it auto-discovers
					"gameId": hello.as_ref().and_then(|hello| hello.game_id),
					"placeIds": hello.as_ref().map(|hello| hello.place_ids.clone()),
				},
				"plugin": plugin_json(plugin.as_ref()),
				"sourcemap": { "present": sourcemap.exists(), "path": sourcemap.to_string() },
				// `present` is the file on disk; `pending` now means "no daemon
				// has reported an audit-log location" — the only state in which
				// `path` is the CLI's own default instead of the writer's answer
				"writeLog": {
					"present": writes_log.exists(),
					"path": writes_log.to_string(),
					"pending": reported.is_none(),
					"source": if reported.is_some() { "daemon" } else { "state directory" },
				},
			}));

			return Ok(());
		}

		println!("Project   {}", target.project_path.to_string().bold());

		match &project {
			Some(project) => println!(
				"          name {}, gameId {}",
				project.name,
				project
					.game_id
					.map_or_else(|| "unset".to_owned(), |game_id| game_id.to_string())
			),
			None if target.project_exists => wsync_warn!("The project file exists but does not parse"),
			None => wsync_warn!("No project file at this path — run `wsync init` first"),
		}

		match (&hello, running) {
			(Some(hello), true) => println!(
				"Daemon    running on port {} (PID {}, v{}, protocol {}){}",
				target.port.to_string().bold(),
				hello.pid,
				hello.version,
				hello.protocol,
				match (target.record.is_some(), stale) {
					(false, _) => " — no runtime record, so discovery cannot find it without --port",
					(true, true) => " — the runtime record names a different boot and is stale",
					(true, false) => "",
				}
			),
			(Some(hello), false) => println!(
				"Daemon    port {} serves a different project: {}",
				target.port.to_string().bold(),
				hello.project
			),
			(None, _) => println!(
				"Daemon    not reachable on port {} (from the {})",
				target.port.to_string().bold(),
				target.port_source.as_str()
			),
		}

		match &plugin {
			Some(envelope) if envelope.ok => println!(
				"Plugin    connected (v{}, Studio {})",
				envelope
					.value
					.get("pluginVersion")
					.and_then(Value::as_str)
					.unwrap_or("unknown"),
				envelope
					.value
					.get("studioVersion")
					.and_then(Value::as_str)
					.unwrap_or("unknown"),
			),
			Some(envelope) => println!("Plugin    not connected ({})", envelope.error_message()),
			None => println!("Plugin    unknown (no daemon to ask)"),
		}

		println!(
			"Sourcemap {}",
			if sourcemap.exists() {
				sourcemap.to_string()
			} else {
				"not generated".to_owned()
			}
		);
		println!(
			"Write log {}{}",
			writes_log.to_string(),
			match (reported.is_some(), writes_log.exists()) {
				(_, true) => String::new(),
				(true, false) => format!(" {}", "(no writes recorded yet)".dimmed()),
				// Nothing confirmed this location: it is the CLI's state-dir
				// default, which a daemon elsewhere may not share
				(false, false) => format!(" {}", "(default location — no daemon reported one)".dimmed()),
			}
		);

		Ok(())
	}
}

fn plugin_json(plugin: Option<&Envelope>) -> Value {
	match plugin {
		None => json!({ "connected": false, "reason": "no daemon to ask" }),
		Some(envelope) if envelope.ok => json!({
			"connected": true,
			"version": envelope.value.get("pluginVersion"),
			"protocol": envelope.value.get("protocol"),
			"studioVersion": envelope.value.get("studioVersion"),
		}),
		Some(envelope) => json!({
			"connected": false,
			"code": envelope.error_code(),
			"reason": envelope.error_message(),
		}),
	}
}

/// One `doctor` finding
struct Check {
	id: &'static str,
	label: &'static str,
	status: &'static str,
	detail: String,
	advice: Option<String>,
}

impl Check {
	fn pass(id: &'static str, label: &'static str, detail: impl Into<String>) -> Self {
		Self {
			id,
			label,
			status: "pass",
			detail: detail.into(),
			advice: None,
		}
	}

	fn fail(id: &'static str, label: &'static str, detail: impl Into<String>, advice: impl Into<String>) -> Self {
		Self {
			id,
			label,
			status: "fail",
			detail: detail.into(),
			advice: Some(advice.into()),
		}
	}

	fn warn(id: &'static str, label: &'static str, detail: impl Into<String>, advice: impl Into<String>) -> Self {
		Self {
			id,
			label,
			status: "warn",
			detail: detail.into(),
			advice: Some(advice.into()),
		}
	}

	fn skip(id: &'static str, label: &'static str, detail: impl Into<String>) -> Self {
		Self {
			id,
			label,
			status: "skip",
			detail: detail.into(),
			advice: None,
		}
	}

	fn to_json(&self) -> Value {
		json!({
			"id": self.id,
			"label": self.label,
			"status": self.status,
			"detail": self.detail,
			"advice": self.advice,
		})
	}
}

/// Run a broader health check over project files, daemon, and plugin
#[derive(Parser)]
pub struct Doctor {
	#[command(flatten)]
	targeting: Targeting,

	/// Print machine-readable JSON
	#[arg(long)]
	raw: bool,
}

impl Doctor {
	pub fn main(self) -> Result<()> {
		let target = Target::resolve(&self.targeting)?;
		let mut checks = Vec::new();

		// 1. Project file
		if !target.project_exists {
			checks.push(Check::fail(
				"project-file",
				"Project file",
				format!("No project file at {}", target.project_path.to_string()),
				"Run `wsync init --project <path>` to create one",
			));
		} else {
			match Project::load(&target.project_path) {
				Ok(project) => checks.push(Check::pass(
					"project-file",
					"Project file",
					format!("{} parses ({})", target.project_path.to_string(), project.name),
				)),
				Err(err) => checks.push(Check::fail(
					"project-file",
					"Project file",
					format!("{} does not parse: {err}", target.project_path.to_string()),
					"Fix the JSON syntax or the `tree` shape reported above",
				)),
			}
		}

		// 2. Runtime record
		match &target.record {
			Some(record) => checks.push(Check::pass(
				"runtime-record",
				"Runtime record",
				format!(
					"port {}, PID {}, managed by {}",
					record.port, record.pid, record.managed_by
				),
			)),
			None => checks.push(Check::warn(
				"runtime-record",
				"Runtime record",
				format!(
					"No runtime record for this project; using port {} from the {}",
					target.port,
					target.port_source.as_str()
				),
				"Start the daemon with `wsync daemon start --project <path>` so discovery stops guessing",
			)),
		}

		// 3. Daemon reachability
		let hello = target.probe().ok();

		match &hello {
			Some(hello) => checks.push(Check::pass(
				"daemon",
				"Daemon reachable",
				format!("v{} on port {} (PID {})", hello.version, target.port, hello.pid),
			)),
			None => checks.push(Check::fail(
				"daemon",
				"Daemon reachable",
				format!("Nothing answers GET /hello on port {}", target.port),
				"Start it with `wsync daemon start --project <path>`, or pass --port for a daemon on another port",
			)),
		}

		// 4. Identity: the answering daemon serves this project, on the boot
		// the record names
		match (&hello, &target.canonical) {
			(Some(hello), Some(canonical)) if &hello.canonical_project != canonical => checks.push(Check::fail(
				"identity",
				"Daemon identity",
				format!("Port {} serves {} instead", target.port, hello.project),
				"Stop the other daemon or target this project's port with --port",
			)),
			(Some(hello), _) if target.record.is_some() && !target.record_is_live(hello) => checks.push(Check::warn(
				"identity",
				"Daemon identity",
				"The runtime record names a different boot than the daemon answering now",
				"Run `wsync daemon restart --project <path>` to refresh the record",
			)),
			(Some(_), _) => checks.push(Check::pass(
				"identity",
				"Daemon identity",
				"The answering daemon serves this project",
			)),
			(None, _) => checks.push(Check::skip("identity", "Daemon identity", "No daemon answered")),
		}

		// 5. Protocol
		match &hello {
			Some(hello) if hello.protocol == PROTOCOL_VERSION => checks.push(Check::pass(
				"protocol",
				"Protocol match",
				format!("CLI and daemon both speak protocol {PROTOCOL_VERSION}"),
			)),
			Some(hello) => checks.push(Check::fail(
				"protocol",
				"Protocol match",
				format!(
					"CLI speaks protocol {PROTOCOL_VERSION}, daemon speaks {}",
					hello.protocol
				),
				"Binary, plugin, and daemon ship from one commit — reinstall the matching build",
			)),
			None => checks.push(Check::skip("protocol", "Protocol match", "No daemon answered")),
		}

		// 6/7. Plugin connection and its protocol
		let plugin = match Client::open_probed(&target, hello.clone()) {
			Some(client) => client.request("version", json!({})).ok(),
			None => None,
		};

		match &plugin {
			Some(envelope) if envelope.ok => {
				checks.push(Check::pass(
					"plugin",
					"Studio plugin",
					format!(
						"connected, v{}",
						envelope
							.value
							.get("pluginVersion")
							.and_then(Value::as_str)
							.unwrap_or("unknown")
					),
				));

				let plugin_protocol = envelope.value.get("protocol").and_then(Value::as_u64);

				match plugin_protocol {
					Some(protocol) if protocol == u64::from(PROTOCOL_VERSION) => checks.push(Check::pass(
						"plugin-protocol",
						"Plugin protocol",
						format!("plugin speaks protocol {protocol}"),
					)),
					Some(protocol) => checks.push(Check::fail(
						"plugin-protocol",
						"Plugin protocol",
						format!("plugin speaks protocol {protocol}, CLI speaks {PROTOCOL_VERSION}"),
						"Reinstall the plugin with `wsync plugin install` and restart Studio",
					)),
					None => checks.push(Check::warn(
						"plugin-protocol",
						"Plugin protocol",
						"The plugin did not report a protocol number",
						"Reinstall the plugin with `wsync plugin install`",
					)),
				}
			}
			Some(envelope) => {
				checks.push(Check::fail(
					"plugin",
					"Studio plugin",
					envelope.error_message(),
					if envelope.plugin_missing() {
						"Open the place in Studio with the WSync plugin installed (`wsync plugin install`)"
					} else {
						"The plugin answered but failed the `version` op — check the Studio output for plugin errors"
					},
				));
				checks.push(Check::skip("plugin-protocol", "Plugin protocol", "No plugin connected"));
			}
			None => {
				checks.push(Check::skip("plugin", "Studio plugin", "No daemon to ask"));
				checks.push(Check::skip("plugin-protocol", "Plugin protocol", "No daemon to ask"));
			}
		}

		let count = |status: &str| checks.iter().filter(|check| check.status == status).count();
		let failures = count("fail");

		if self.raw {
			print_json(&json!({
				"ok": failures == 0,
				"checks": checks.iter().map(Check::to_json).collect::<Vec<_>>(),
				"summary": {
					"pass": count("pass"),
					"warn": count("warn"),
					"fail": failures,
					"skip": count("skip"),
				},
			}));
		} else {
			for check in &checks {
				let mark = match check.status {
					"pass" => "  ok  ".green(),
					"warn" => " warn ".yellow(),
					"fail" => " fail ".red(),
					_ => " skip ".dimmed(),
				};

				println!("[{mark}] {:<18} {}", check.label, check.detail);

				if let Some(advice) = &check.advice {
					println!("         {}", advice.dimmed());
				}
			}
		}

		if failures > 0 {
			// A failed check means the live surface will not work; the exit
			// code has to say so even though `doctor` itself completed
			bail!("{failures} health check(s) failed");
		}

		Ok(())
	}
}

/// Negotiate daemon, plugin, and host feature support before choosing a
/// command path
#[derive(Parser)]
pub struct Capabilities {
	#[command(flatten)]
	targeting: Targeting,

	/// Print the complete correlated response envelope
	#[arg(long)]
	raw: bool,
}

impl Capabilities {
	pub fn main(self) -> Result<()> {
		let client = Client::connect(&self.targeting)?;
		let envelope = client.request("capabilities", json!({}))?;

		// capabilities.json: `--raw` prints the complete correlated envelope,
		// not just the value
		if self.raw {
			print_json(&envelope.body);

			return if envelope.ok {
				Ok(())
			} else {
				bail!("{} [{}]", envelope.error_message(), envelope.error_code())
			};
		}

		let value = envelope.into_value(false)?;

		println!(
			"Daemon    v{} (protocol {})",
			client.hello.version, client.hello.protocol
		);
		println!(
			"Plugin    v{} (protocol {})",
			value.get("plugin").and_then(Value::as_str).unwrap_or("unknown"),
			value.get("protocol").and_then(Value::as_u64).unwrap_or(0),
		);

		if let Some(features) = value.get("features").and_then(Value::as_object) {
			let mut supported: Vec<&str> = features
				.iter()
				.filter(|(_, value)| value.as_bool() == Some(true))
				.map(|(name, _)| name.as_str())
				.collect();
			let missing = features.len() - supported.len();

			supported.sort_unstable();

			println!("Features  {} of {} ops implemented", supported.len(), features.len());
			println!("          {}", supported.join(", "));

			if missing > 0 {
				println!("          {missing} cataloged op(s) not implemented by this plugin build");
			}
		}

		if let Some(limits) = value.get("limits").and_then(Value::as_object) {
			println!("Limits");

			let mut names: Vec<&String> = limits.keys().collect();
			names.sort();

			for name in names {
				println!("          {name:<8} {}", human_value(&limits[name]));
			}
		}

		Ok(())
	}
}

impl Client {
	/// Wraps an already-probed daemon, swallowing the identity check so the
	/// tolerant diagnostics (`status`, `doctor`, `context`) can still ask a
	/// mismatched daemon for its plugin state instead of aborting
	pub(crate) fn open_probed(target: &Target, hello: Option<Hello>) -> Option<Client> {
		let hello = hello?;
		let target = Target {
			project_path: target.project_path.clone(),
			project_exists: target.project_exists,
			// Identity is reported as its own `doctor` check; re-checking it
			// here would turn a reportable mismatch into a hard failure
			canonical: None,
			state_dir: target.state_dir.clone(),
			record: None,
			port: target.port,
			port_source: target.port_source,
		};

		Client::open(target, hello).ok()
	}
}
