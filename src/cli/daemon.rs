use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use colored::Colorize;
use log::debug;
use serde::Deserialize;
use serde_json::json;
use std::{
	env,
	io::{self, Write},
	path::PathBuf,
	process,
	sync::{
		atomic::{AtomicU16, Ordering},
		Arc,
	},
	thread,
	time::{Duration, Instant},
};
use uuid::Uuid;

use crate::{
	config::Config,
	daemon::{self, RuntimePaths, RuntimeRecord},
	ext::PathExt,
	project::{self, Project},
	server::{self, Server, ServerIdentity},
	sessions, util, wsync_info, wsync_warn,
};

/// How long lifecycle probes wait for `GET /hello`
const PROBE_TIMEOUT: Duration = Duration::from_millis(1500);

/// Manage the per-project background daemon
#[derive(Parser)]
pub struct Daemon {
	#[command(subcommand)]
	command: Command,
}

#[derive(Subcommand)]
enum Command {
	Start(Start),
	Status(Status),
	Stop(Stop),
	Restart(Restart),
	Logs(Logs),
}

impl Daemon {
	pub fn main(self) -> Result<()> {
		match self.command {
			Command::Start(command) => command.main(),
			Command::Status(command) => command.main(),
			Command::Stop(command) => command.main(),
			Command::Restart(command) => command.main(),
			Command::Logs(command) => command.main(),
		}
	}
}

/// Start the project daemon: this process becomes the daemon (foreground,
/// machine-managed). Idempotent — a matching running daemon is reported with
/// `alreadyRunning` instead of duplicated
#[derive(Parser)]
struct Start {
	/// Project path
	#[arg(long)]
	project: Option<PathBuf>,

	/// Server port (a port serving a different project is a hard error)
	#[arg(short = 'P', long)]
	port: Option<u16>,

	/// Name of the lifecycle manager owning this daemon
	#[arg(long, default_value = "cli")]
	managed_by: String,

	/// Environment variable holding the owner token; supplying one makes the
	/// daemon managed (authenticated stop + heartbeat watchdog)
	#[arg(long)]
	owner_token_env: Option<String>,

	/// Override the state directory (defaults to the platform data dir)
	#[arg(long)]
	data_dir: Option<PathBuf>,

	/// Persist this game ID into the project before starting
	#[arg(long)]
	game_id: Option<u64>,

	/// Persist this group ID into the project before starting
	#[arg(long)]
	group_id: Option<u64>,

	/// Persist these place IDs into the project before starting
	#[arg(long = "place-id")]
	place_id: Vec<u64>,

	/// Print one machine-readable JSON line once the daemon is serving
	#[arg(long)]
	raw: bool,
}

#[derive(Parser)]
/// Report the daemon's runtime record and live identity
struct Status {
	/// Project path
	#[arg(long)]
	project: Option<PathBuf>,

	/// Override the state directory
	#[arg(long)]
	data_dir: Option<PathBuf>,

	/// Print machine-readable JSON
	#[arg(long)]
	raw: bool,
}

/// Stop the project daemon (authenticated: exact project + boot identity is
/// verified over `/hello` before anything is killed)
#[derive(Parser)]
struct Stop {
	/// Project path
	#[arg(long)]
	project: Option<PathBuf>,

	/// Override the state directory
	#[arg(long)]
	data_dir: Option<PathBuf>,

	/// Seconds to wait for a graceful exit before the OS-kill fallback
	#[arg(long, default_value = "10")]
	timeout: u64,

	/// Print machine-readable JSON
	#[arg(long)]
	raw: bool,
}

/// Restart the project daemon, preferring its previous port
#[derive(Parser)]
struct Restart {
	/// Project path
	#[arg(long)]
	project: Option<PathBuf>,

	/// Server port (defaults to the port the daemon ran on)
	#[arg(short = 'P', long)]
	port: Option<u16>,

	/// Name of the lifecycle manager owning this daemon
	#[arg(long, default_value = "cli")]
	managed_by: String,

	/// Environment variable holding the owner token
	#[arg(long)]
	owner_token_env: Option<String>,

	/// Override the state directory
	#[arg(long)]
	data_dir: Option<PathBuf>,

	/// Seconds to wait for the old daemon's graceful exit
	#[arg(long, default_value = "10")]
	timeout: u64,

	/// Print one machine-readable JSON line once the daemon is serving
	#[arg(long)]
	raw: bool,
}

/// Print the tail of the daemon's lifecycle log
#[derive(Parser)]
struct Logs {
	/// Project path
	#[arg(long)]
	project: Option<PathBuf>,

	/// Override the state directory
	#[arg(long)]
	data_dir: Option<PathBuf>,

	/// Number of log lines to print
	#[arg(long, default_value = "50")]
	lines: usize,
}

/// The subset of `GET /hello` the lifecycle machinery relies on
#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
struct Hello {
	#[serde(default)]
	name: String,
	#[serde(default)]
	project: String,
	canonical_project: String,
	boot_id: String,
	pid: u32,
	port: u16,
	#[serde(default)]
	managed_by: String,
}

fn probe_hello(port: u16) -> Result<Hello> {
	let response = reqwest::blocking::Client::builder()
		.timeout(PROBE_TIMEOUT)
		.build()?
		.get(format!("http://localhost:{port}/hello"))
		.send()?
		.error_for_status()?;

	Ok(response.json()?)
}

fn resolve_project_path(project: Option<PathBuf>) -> Result<PathBuf> {
	let project_path = project::resolve(project.unwrap_or_default())?;

	if !project_path.exists() {
		bail!(
			"No project files found in {}. Run {} to create one",
			project_path.get_parent().to_string().bold(),
			"wsync init".bold(),
		);
	}

	Ok(project_path)
}

struct ProjectContext {
	project_path: PathBuf,
	canonical: String,
	paths: RuntimePaths,
}

fn project_context(project: Option<PathBuf>, data_dir: Option<&PathBuf>) -> Result<ProjectContext> {
	let project_path = resolve_project_path(project)?;
	let canonical = daemon::canonical_project(&project_path)?;
	let state_dir = daemon::state_dir(data_dir.map(PathBuf::as_path))?;
	let paths = daemon::runtime_paths(&state_dir, &canonical);

	Ok(ProjectContext {
		project_path,
		canonical,
		paths,
	})
}

fn report_already_running(raw: bool, record: &RuntimeRecord) -> Result<()> {
	if raw {
		println!(
			"{}",
			json!({
				"ok": true,
				"port": record.port,
				"pid": record.pid,
				"bootId": record.boot_id,
				"project": record.project,
				"canonicalProject": record.canonical_project,
				"alreadyRunning": true,
			})
		);
		io::stdout().flush().ok();
	} else {
		wsync_info!(
			"A daemon for this project is already running on port {} (PID {})",
			record.port.to_string().bold(),
			record.pid.to_string().bold()
		);
	}

	Ok(())
}

/// Builds a token-less record for a matching daemon discovered over `/hello`
/// (e.g. one started with `wsync serve`), so status and stop keep working
fn record_from_hello(hello: &Hello, canonical: &str, paths: &RuntimePaths) -> RuntimeRecord {
	RuntimeRecord {
		version: daemon::RECORD_VERSION,
		project: hello.project.clone(),
		canonical_project: canonical.to_owned(),
		pid: hello.pid,
		port: hello.port,
		boot_id: hello.boot_id.clone(),
		control_token: None,
		managed_by: if hello.managed_by.is_empty() {
			"unknown".into()
		} else {
			hello.managed_by.clone()
		},
		log_path: paths.log.to_string(),
		started_at: daemon::unix_now(),
	}
}

/// Persists `--game-id`/`--group-id`/`--place-id` into the project file
/// before the tree is built, writing only when a value actually changes
fn persist_metadata(
	project_path: &PathBuf,
	game_id: Option<u64>,
	group_id: Option<u64>,
	place_ids: &[u64],
) -> Result<()> {
	if game_id.is_none() && group_id.is_none() && place_ids.is_empty() {
		return Ok(());
	}

	let mut project = Project::load(project_path)?;
	let mut changed = false;

	if let Some(game_id) = game_id {
		if project.game_id != Some(game_id) {
			project.game_id = Some(game_id);
			changed = true;
		}
	}

	if let Some(group_id) = group_id {
		if project.group_id != Some(group_id) {
			project.group_id = Some(group_id);
			changed = true;
		}
	}

	if !place_ids.is_empty() && project.place_ids != place_ids {
		place_ids.clone_into(&mut project.place_ids);
		changed = true;
	}

	if changed {
		project.save(project_path)?;
		debug!("Persisted daemon start metadata into {project_path:?}");
	}

	Ok(())
}

impl Start {
	fn main(self) -> Result<()> {
		let context = project_context(self.project.clone(), self.data_dir.as_ref())?;
		let ProjectContext {
			project_path,
			canonical,
			paths,
		} = &context;

		Config::load_workspace(project_path.get_parent());

		// The start lock serializes the whole critical section: probes, port
		// selection, boot, and the runtime-record write. It is released from
		// the ready callback once the record exists and the listener is bound
		let start_lock = daemon::StartLock::acquire(&paths.start_lock)?;

		persist_metadata(project_path, self.game_id, self.group_id, &self.place_id)?;

		// Idempotent start: a live record whose boot identity matches over
		// `/hello` short-circuits into alreadyRunning
		if let Some(record) = daemon::read_record(&paths.record)? {
			match probe_hello(record.port) {
				Ok(hello) if hello.canonical_project == *canonical => {
					let record = if hello.boot_id == record.boot_id {
						record
					} else {
						// Same project, new boot (restarted outside the
						// record machinery): adopt the live identity
						let refreshed = record_from_hello(&hello, canonical, paths);
						daemon::write_record(&paths.record, &refreshed)?;
						refreshed
					};

					return report_already_running(self.raw, &record);
				}
				Ok(_) => {
					// The port now serves a different project; the record is
					// stale
					daemon::remove_record_if_boot(&paths.record, &record.boot_id)?;
				}
				Err(_) => {
					if util::process_exists(record.pid) {
						// The recorded process exists but does not answer.
						// Retry briefly (it may be mid-boot), then refuse
						// rather than double-serving or blind-killing
						let mut adopted = false;

						for _ in 0..3 {
							thread::sleep(Duration::from_millis(500));

							if let Ok(hello) = probe_hello(record.port) {
								if hello.canonical_project == *canonical {
									adopted = true;
								}

								break;
							}
						}

						if adopted {
							return report_already_running(self.raw, &record);
						}

						bail!(
							"A recorded daemon (PID {}) exists but does not answer on port {}; its runtime record was preserved and no duplicate was started",
							record.pid,
							record.port
						);
					}

					// Dead process: recover the stale record
					daemon::remove_record_if_boot(&paths.record, &record.boot_id)?;
				}
			}
		}

		let project = Project::load(project_path)?;

		if !project.is_place() {
			bail!("Cannot serve non-place project!");
		}

		let config = Config::new();
		let host = project.host.clone().unwrap_or_else(|| config.host.clone());
		let mut port = self.port.unwrap_or(project.port.unwrap_or(config.port));
		let scan_ports = config.scan_ports;
		let port_scan_max = config.port_scan_max;
		drop(config);

		// `/hello` is the authoritative occupancy probe: a daemon may hold
		// only one loopback stack (e.g. 127.0.0.1), which a plain bind probe
		// on `localhost` would miss by succeeding on the other stack.
		// One daemon per project means neighbouring ports are expected to be
		// other projects' daemons (Design §3.2) — scan past them unless the
		// caller pinned a port explicitly
		let max_port = port_scan_max.max(port);
		let start_port = port;

		loop {
			match probe_hello(port) {
				Ok(hello) if hello.canonical_project == *canonical => {
					// A daemon for this project runs on the port without a
					// record (plain `wsync serve`): adopt it
					let record = record_from_hello(&hello, canonical, paths);
					daemon::write_record(&paths.record, &record)?;

					return report_already_running(self.raw, &record);
				}
				Ok(hello) => {
					if self.port.is_some() {
						bail!(
							"Port {} is already serving a different project: {}",
							port,
							hello.project.bold()
						);
					}

					if !scan_ports {
						bail!(
							"Port {} is already serving a different project: {}! Enable the {} setting to use the first available port automatically",
							port.to_string().bold(),
							hello.project.bold(),
							"scan_ports".bold()
						);
					}
				}
				Err(_) => {
					if server::is_port_free(&host, port) {
						break;
					}

					// Occupied by something that is not a WSync daemon
					if self.port.is_some() {
						bail!("Port {port} is already in use by another process");
					}

					if !scan_ports {
						bail!(
							"Port {} is already in use! Enable {} setting to use the first available port automatically",
							port.to_string().bold(),
							"scan_ports".bold()
						);
					}
				}
			}

			if port >= max_port {
				bail!("No free port found in range {start_port}-{max_port}! Increase the port_scan_max setting");
			}

			port += 1;
		}

		let owner_token =
			match &self.owner_token_env {
				None => None,
				Some(variable) => Some(env::var(variable).ok().filter(|token| !token.is_empty()).with_context(
					|| format!("--owner-token-env: environment variable {variable} is unset or empty"),
				)?),
			};

		if self.managed_by.trim().is_empty() {
			bail!("--managed-by cannot be empty");
		}

		let core = Arc::new(crate::core::Core::new(project, true)?);
		let boot_id = Uuid::new_v4().to_string();
		let journal = Arc::new(daemon::DaemonLog::open(&paths.log)?);

		journal.note(&format!(
			"Starting daemon for {canonical} (managed by {}, boot {boot_id})",
			self.managed_by
		));

		let identity = ServerIdentity {
			boot_id: boot_id.clone(),
			managed_by: self.managed_by.clone(),
			control_token: owner_token.clone(),
			journal: Some(journal.clone()),
		};

		let server = Server::new(core, &host, port, identity);
		let bound_port = Arc::new(AtomicU16::new(port));

		let ready: Box<dyn FnOnce(u16) + Send> = {
			let record_path = paths.record.clone();
			let log_path = paths.log.to_string();
			let project_string = project_path.to_string();
			let canonical = canonical.clone();
			let managed_by = self.managed_by.clone();
			let boot_id = boot_id.clone();
			let host = host.clone();
			let journal = journal.clone();
			let bound_port = bound_port.clone();
			let raw = self.raw;

			Box::new(move |actual_port| {
				bound_port.store(actual_port, Ordering::Relaxed);

				let pid = process::id();
				let record = RuntimeRecord {
					version: daemon::RECORD_VERSION,
					project: project_string.clone(),
					canonical_project: canonical.clone(),
					pid,
					port: actual_port,
					boot_id: boot_id.clone(),
					control_token: owner_token,
					managed_by,
					log_path,
					started_at: daemon::unix_now(),
				};

				if let Err(err) = daemon::write_record(&record_path, &record) {
					wsync_warn!("Failed to write daemon runtime record: {}", err);
				}

				sessions::add(None, Some(host.clone()), Some(actual_port), pid, true).ok();

				// Signal cleanup: the record and session entry must not
				// outlive the process
				{
					let journal = journal.clone();
					let record_path = record_path.clone();
					let boot_id = boot_id.clone();
					let session = sessions::Session {
						pid,
						host: Some(host.clone()),
						port: Some(actual_port),
					};

					ctrlc::set_handler(move || {
						journal.note("Stopping: signal received");
						daemon::remove_record_if_boot(&record_path, &boot_id).ok();
						sessions::remove(&session).ok();
						process::exit(0);
					})
					.ok();
				}

				journal.note(&format!("Listening on {host}:{actual_port} (PID {pid})"));

				if raw {
					println!(
						"{}",
						json!({
							"ok": true,
							"port": actual_port,
							"pid": pid,
							"bootId": boot_id,
							"project": project_string,
							"canonicalProject": canonical,
						})
					);
					io::stdout().flush().ok();
				} else {
					wsync_info!(
						"Serving on: {}, project: {}",
						server::format_address(&host, actual_port).bold(),
						project_string.bold()
					);
				}

				// Only now may a concurrent `daemon start` probe: the record
				// exists and the listener is bound
				drop(start_lock);
			})
		};

		let result = server.start(Some(ready));

		// Graceful shutdown path (authenticated /stop, manager close or
		// watchdog): clean the runtime record and session entry
		journal.note("Daemon stopped");
		daemon::remove_record_if_boot(&paths.record, &boot_id).ok();

		let final_port = bound_port.load(Ordering::Relaxed);
		if let Ok(Some(session)) = sessions::get(None, None, Some(final_port)) {
			sessions::remove(&session).ok();
		}

		result
	}
}

impl Status {
	fn main(self) -> Result<()> {
		let context = project_context(self.project, self.data_dir.as_ref())?;
		let record = daemon::read_record(&context.paths.record)?;

		let live = record.as_ref().and_then(|record| probe_hello(record.port).ok());

		let running = matches!(
			(&record, &live),
			(Some(record), Some(hello))
				if hello.boot_id == record.boot_id && hello.canonical_project == context.canonical
		);
		let stale = record.is_some() && !running;

		if self.raw {
			let record_json = record.as_ref().map(|record| {
				// The control token is a capability, never printed
				json!({
					"project": record.project,
					"canonicalProject": record.canonical_project,
					"pid": record.pid,
					"port": record.port,
					"bootId": record.boot_id,
					"managed": record.control_token.is_some(),
					"managedBy": record.managed_by,
					"logPath": record.log_path,
					"startedAt": record.started_at,
				})
			});
			let live_json = live.as_ref().map(|hello| {
				json!({
					"name": hello.name,
					"project": hello.project,
					"canonicalProject": hello.canonical_project,
					"bootId": hello.boot_id,
					"pid": hello.pid,
					"port": hello.port,
					"managedBy": hello.managed_by,
				})
			});

			println!(
				"{}",
				json!({
					"ok": true,
					"running": running,
					"stale": stale,
					"canonicalProject": context.canonical,
					"record": record_json,
					"live": live_json,
				})
			);

			return Ok(());
		}

		match (&record, running) {
			(Some(record), true) => wsync_info!(
				"Daemon is running on port {} (PID {}, managed by {})",
				record.port.to_string().bold(),
				record.pid.to_string().bold(),
				record.managed_by.bold()
			),
			(Some(record), false) => wsync_warn!(
				"Daemon record exists (port {}, PID {}) but no matching daemon answers — the record is stale",
				record.port.to_string().bold(),
				record.pid.to_string().bold()
			),
			(None, _) => wsync_info!("No daemon is running for this project"),
		}

		Ok(())
	}
}

/// Outcome of an authenticated stop attempt
struct StopOutcome {
	stopped: bool,
	port: Option<u16>,
	detail: String,
}

fn stop_daemon(context: &ProjectContext, timeout: Duration) -> Result<StopOutcome> {
	let Some(record) = daemon::read_record(&context.paths.record)? else {
		return Ok(StopOutcome {
			stopped: false,
			port: None,
			detail: "No runtime record — nothing to stop".into(),
		});
	};

	if record.canonical_project != context.canonical {
		bail!(
			"Runtime record belongs to {}, not {}",
			record.canonical_project,
			context.canonical
		);
	}

	// A stale PID record is never sufficient authority to kill: the live
	// boot identity must be verified over /hello first
	let hello = match probe_hello(record.port) {
		Ok(hello) => hello,
		Err(_) => {
			if util::process_exists(record.pid) {
				bail!(
					"Daemon (PID {}) does not answer on port {}; refusing a blind kill — its runtime record was preserved",
					record.pid,
					record.port
				);
			}

			daemon::remove_record_if_boot(&context.paths.record, &record.boot_id)?;

			return Ok(StopOutcome {
				stopped: false,
				port: Some(record.port),
				detail: "Daemon was already dead; stale record removed".into(),
			});
		}
	};

	if hello.boot_id != record.boot_id || hello.canonical_project != record.canonical_project {
		// The port serves another boot or project now; never kill it
		daemon::remove_record_if_boot(&context.paths.record, &record.boot_id)?;

		return Ok(StopOutcome {
			stopped: false,
			port: Some(record.port),
			detail: "Record was stale (another daemon owns the port now); nothing stopped".into(),
		});
	}

	// Exact boot verified: graceful, authenticated /stop first
	let deadline = Instant::now() + timeout;
	let client = reqwest::blocking::Client::builder().timeout(PROBE_TIMEOUT).build()?;
	let body = match &record.control_token {
		Some(token) => json!({ "bootId": record.boot_id, "token": token }),
		None => json!({}),
	};

	match client
		.post(format!("http://localhost:{}/stop", record.port))
		.json(&body)
		.send()
	{
		Ok(response) if response.status().is_success() => {}
		Ok(response) => {
			let status = response.status();
			let text = response.text().unwrap_or_default();

			bail!("Daemon rejected the stop request ({status}): {text}");
		}
		// The daemon may have died between the probe and the request; the
		// wait loop below settles it
		Err(err) => debug!("Graceful stop request failed: {err}"),
	}

	let mut graceful = false;

	while Instant::now() < deadline {
		match probe_hello(record.port) {
			Err(_) => {
				graceful = true;
				break;
			}
			Ok(hello) if hello.boot_id != record.boot_id => {
				graceful = true;
				break;
			}
			Ok(_) => thread::sleep(Duration::from_millis(150)),
		}
	}

	if !graceful {
		// OS-kill fallback, authorized by the exact /hello identity match
		// established above
		debug!("Graceful stop timed out; killing PID {}", record.pid);
		util::kill_process(record.pid);
		thread::sleep(Duration::from_millis(300));
	}

	daemon::remove_record_if_boot(&context.paths.record, &record.boot_id)?;

	if let Ok(Some(session)) = sessions::get(None, None, Some(record.port)) {
		sessions::remove(&session).ok();
	}

	Ok(StopOutcome {
		stopped: true,
		port: Some(record.port),
		detail: if graceful {
			"Stopped gracefully".into()
		} else {
			"Stopped via OS kill fallback after graceful timeout".into()
		},
	})
}

impl Stop {
	fn main(self) -> Result<()> {
		let context = project_context(self.project, self.data_dir.as_ref())?;
		let outcome = stop_daemon(&context, Duration::from_secs(self.timeout))?;

		if self.raw {
			println!(
				"{}",
				json!({
					"ok": true,
					"stopped": outcome.stopped,
					"port": outcome.port,
					"detail": outcome.detail,
				})
			);
		} else if outcome.stopped {
			wsync_info!("{}", outcome.detail);
		} else {
			wsync_warn!("{}", outcome.detail);
		}

		Ok(())
	}
}

impl Restart {
	fn main(self) -> Result<()> {
		let context = project_context(self.project.clone(), self.data_dir.as_ref())?;
		let previous_port = daemon::read_record(&context.paths.record)?.map(|record| record.port);

		let outcome = stop_daemon(&context, Duration::from_secs(self.timeout))?;

		if !self.raw {
			wsync_info!("{}", outcome.detail);
		}

		Start {
			project: self.project,
			// Same-port preference: explicit flag first, then the port the
			// daemon previously ran on
			port: self.port.or(outcome.port).or(previous_port),
			managed_by: self.managed_by,
			owner_token_env: self.owner_token_env,
			data_dir: self.data_dir,
			game_id: None,
			group_id: None,
			place_id: Vec::new(),
			raw: self.raw,
		}
		.main()
	}
}

impl Logs {
	fn main(self) -> Result<()> {
		let context = project_context(self.project, self.data_dir.as_ref())?;

		let log_path = match daemon::read_record(&context.paths.record)? {
			Some(record) => PathBuf::from(record.log_path),
			None => context.paths.log.clone(),
		};

		if !log_path.exists() {
			wsync_warn!("No daemon log exists for this project yet");
			return Ok(());
		}

		let tail = daemon::read_log_tail(&log_path, self.lines)?;

		if tail.is_empty() {
			wsync_warn!("The daemon log is empty");
		} else {
			println!("{tail}");
		}

		Ok(())
	}
}
