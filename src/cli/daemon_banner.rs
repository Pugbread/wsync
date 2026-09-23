//! The daemon-down banner.
//!
//! Incident report: a project's daemon crashed, and for about thirty minutes
//! edits landed on disk with nothing saying they were not syncing — every
//! command except `status` stayed quiet. So every command that addresses a
//! project (`--project`, a positional project, or the project resolved from
//! the working directory) checks, before it runs, that a live daemon serves
//! that project, and prints a short banner to **stderr** when none does.
//! stdout is never written, so `--raw` JSON stays one parseable line.
//!
//! The check is cheap by construction: it reads the project's runtime record
//! and probes `GET /hello` only on a port something claims serves the
//! project — the record's, an explicit `--port`, or (with no record at all) a
//! registered `wsync serve` session. The journal tail is read only once the
//! daemon is known to be down, to say what happened and when: the daemon's
//! own stop notes, and the `Manager: …` notes the desktop app appends when a
//! sidecar dies without a word (a stack overflow aborts before Rust can log).
//!
//! Commands where the banner means nothing — the lifecycle commands
//! themselves, `status` (which reports the daemon already), the project-less
//! install/config/auth surface, and the legacy session-addressed commands —
//! are skipped by name. `WSYNC_NO_DAEMON_BANNER=1` turns the banner off for
//! scripts that never run a daemon (CI builds).

use chrono::{Local, NaiveDateTime};
use clap::ArgMatches;
use colored::Colorize;
use std::{
	env,
	io::{self, Write},
	path::PathBuf,
};

use super::client;
use crate::{
	daemon::{self, LastEvent, LogEvidence, RuntimeRecord},
	ext::PathExt,
	project, sessions, util,
};

/// Opt-out for scripts that deliberately run without a daemon
pub const OPT_OUT_ENV: &str = "WSYNC_NO_DAEMON_BANNER";

/// Top-level commands that never get the banner
const SKIPPED: &[&str] = &[
	// Lifecycle: these start, stop or report the daemon themselves
	"daemon", "serve", "status", "stop", // Project-less or pre-project surface
	"init", "install", "update", "plugin", "config", "auth", "doc", "commands", "version",
	// Legacy commands addressed to a `wsync serve` session, not the project
	"studio", "debug", "exec",
];

/// How much of the journal the banner reads — plenty for the last lifecycle
/// event, without loading months of plugin connect lines
const LOG_TAIL_BYTES: u64 = 16 * 1024;

/// Bound on `wsync serve` session probes when there is no runtime record
const MAX_SESSION_PROBES: usize = 8;

/// Captured stderr lines shown in the banner (the journal keeps them all)
const MAX_STDERR_LINES: usize = 2;

/// What one invocation addresses, captured from the parsed arguments before
/// the command structs take ownership of them
#[derive(Debug, Clone, Default)]
pub struct DaemonCheck {
	project: Option<PathBuf>,
	port: Option<u16>,
}

impl DaemonCheck {
	/// `None` when the command is skipped or the banner is turned off.
	/// Project arguments are read generically: every command spells its
	/// project `project` (the `Targeting` flag, or a positional) or
	/// `project_flag` (the named alternative beside a positional)
	pub fn from_matches(matches: &ArgMatches) -> Option<Self> {
		if env::var_os(OPT_OUT_ENV).is_some_and(|value| !value.is_empty() && value != "0") {
			return None;
		}

		let mut levels = vec![matches];
		let mut current = matches;
		let mut top = None;

		while let Some((name, sub)) = current.subcommand() {
			top.get_or_insert(name);
			levels.push(sub);
			current = sub;
		}

		if SKIPPED.contains(&top?) {
			return None;
		}

		// `--async` re-executes the command as a hidden child; the parent
		// already printed the banner
		if levels
			.iter()
			.any(|level| level.try_get_one::<bool>("wsync_spawn").ok().flatten() == Some(&true))
		{
			return None;
		}

		let raw_path = |level: &ArgMatches, id: &str| {
			level
				.try_get_raw(id)
				.ok()
				.flatten()
				.and_then(|mut values| values.next())
				.map(PathBuf::from)
		};

		Some(Self {
			project: levels
				.iter()
				.rev()
				.find_map(|level| raw_path(level, "project_flag").or_else(|| raw_path(level, "project"))),
			port: levels
				.iter()
				.rev()
				.find_map(|level| level.try_get_one::<u16>("port").ok().flatten().copied()),
		})
	}

	/// Prints the banner when no live daemon serves the project. Never fails:
	/// anything it cannot determine reads as "no project" and stays silent
	pub fn warn_if_down(&self) {
		let Some(banner) = self.down().map(|down| down.render()) else {
			return;
		};

		let mut stderr = io::stderr().lock();

		stderr.write_all(banner.as_bytes()).ok();
		stderr.flush().ok();
	}

	fn down(&self) -> Option<Down> {
		let project_path = project::resolve(self.project.clone().unwrap_or_default()).ok()?;

		if !project_path.is_file() {
			return None;
		}

		let canonical = daemon::canonical_project(&project_path).ok()?;
		let state_dir = daemon::state_dir(None).ok()?;
		let paths = daemon::runtime_paths(&state_dir, &canonical);
		// A corrupted record says nothing about a port; the explicit and
		// session probes below still get their chance
		let record = daemon::read_record(&paths.record).ok().flatten();

		let serves = |port: u16| client::probe_port(port).is_ok_and(|hello| hello.canonical_project == canonical);
		let mut probed = Vec::new();
		let mut record_port_taken = false;

		if let Some(record) = &record {
			probed.push(record.port);

			match client::probe_port(record.port) {
				// A different boot of the same project (restarted outside the
				// record machinery) still serves it
				Ok(hello) if hello.canonical_project == canonical => return None,
				Ok(_) => record_port_taken = true,
				Err(_) => {}
			}
		}

		if let Some(port) = self.port.filter(|port| !probed.contains(port)) {
			probed.push(port);

			if serves(port) {
				return None;
			}
		}

		if record.is_none() {
			let ports = sessions::peek_ports();

			if ports
				.into_iter()
				.filter(|port| !probed.contains(port))
				.take(MAX_SESSION_PROBES)
				.any(serves)
			{
				return None;
			}
		}

		// Only a record whose port is silent can point at a hung process; a
		// port answering for another project means ours is gone
		let pid_alive = match &record {
			Some(record) if !record_port_taken => util::process_exists(record.pid),
			_ => false,
		};

		let log_path = record
			.as_ref()
			.map(|record| PathBuf::from(&record.log_path))
			.filter(|path| !path.as_os_str().is_empty() && path.exists())
			.unwrap_or(paths.log);
		let has_log = log_path.exists();
		let evidence = daemon::last_event(&daemon::read_log_tail_lines(&log_path, LOG_TAIL_BYTES).unwrap_or_default());

		Some(Down::classify(
			project_path,
			record.as_ref(),
			pid_alive,
			has_log,
			evidence,
		))
	}
}

/// How the daemon ended, as far as the record and the journal can tell
#[derive(Debug, Clone, PartialEq, Eq)]
enum State {
	/// No journal: no daemon has run for this project with this state dir
	NeverStarted,
	Stopped {
		reason: Option<String>,
	},
	/// `status` is the manager's rendering (`signal 6 (SIGABRT)`); `None`
	/// when the process vanished without anyone noting how
	Crashed {
		status: Option<String>,
	},
	HeartbeatLost {
		detail: String,
	},
	/// The recorded process exists but its port is silent
	Hung {
		pid: u32,
		port: u16,
	},
}

#[derive(Debug, Clone)]
struct Down {
	project_path: PathBuf,
	state: State,
	at: Option<String>,
	stderr: Vec<String>,
	has_log: bool,
	/// The desktop app managed this daemon, so re-serving it there is the
	/// other way back
	desktop: bool,
}

impl Down {
	fn classify(
		project_path: PathBuf,
		record: Option<&RuntimeRecord>,
		pid_alive: bool,
		has_log: bool,
		evidence: LogEvidence,
	) -> Self {
		let manager_noted = matches!(
			evidence.event,
			LastEvent::Exited { .. } | LastEvent::HeartbeatLost { .. }
		);

		let state = match (record, evidence.event) {
			(Some(record), _) if pid_alive => State::Hung {
				pid: record.pid,
				port: record.port,
			},
			(_, LastEvent::Exited { status }) => State::Crashed { status: Some(status) },
			(_, LastEvent::HeartbeatLost { detail }) => State::HeartbeatLost { detail },
			(_, LastEvent::Stopped { reason }) => State::Stopped { reason },
			// A record left behind, or a journal that just ends: the process
			// died without cleaning up or saying why
			(Some(_), _) | (None, LastEvent::Silent) => State::Crashed { status: None },
			(None, LastEvent::Empty) => State::NeverStarted,
		};

		Self {
			project_path,
			state,
			at: evidence.at,
			stderr: evidence.stderr,
			has_log,
			desktop: manager_noted || record.is_some_and(|record| record.managed_by == "desktop"),
		}
	}

	/// The `--project` argument to hand back: the project directory for the
	/// default project file, the file itself otherwise
	fn project_arg(&self) -> String {
		let path = if self.project_path.get_name() == "default.project.json" {
			self.project_path.get_parent().to_path_buf()
		} else {
			self.project_path.clone()
		};

		shell_quote(&path.to_string())
	}

	fn render(&self) -> String {
		let project = self.project_arg();
		let start = format!("wsync daemon start --project {project}");
		let when = self.at.as_deref().map(when);

		let state = match &self.state {
			State::NeverStarted => None,
			State::Stopped { reason } => Some(format!(
				"stopped{}{}",
				reason
					.as_deref()
					.map(|reason| format!(" ({reason})"))
					.unwrap_or_default(),
				when.map(|when| format!(" at {when}")).unwrap_or_default()
			)),
			State::Crashed { status: Some(status) } => Some(format!(
				"crashed — {status}{}",
				when.map(|when| format!(", at {when}")).unwrap_or_default()
			)),
			State::Crashed { status: None } => Some(format!(
				"crashed — exited without a clean shutdown{}",
				when.map(|when| format!("; last log entry {when}")).unwrap_or_default()
			)),
			State::HeartbeatLost { detail } => Some(format!(
				"heartbeat lost — {detail}{}",
				when.map(|when| format!(", at {when}")).unwrap_or_default()
			)),
			State::Hung { pid, port } => Some(format!(
				"not answering — recorded PID {pid} still exists but port {port} is silent{}",
				when.map(|when| format!("; last log entry {when}")).unwrap_or_default()
			)),
		};

		let headline = match self.state {
			State::NeverStarted => "No WSync daemon is running for this project",
			_ => "The WSync daemon for this project is DOWN",
		};

		let mut lines = vec![format!(
			"{} {} — edits are NOT syncing to Studio",
			"!!".red().bold(),
			headline.red().bold()
		)];

		lines.push(format!("   project:  {}", self.project_path.to_string()));

		if let Some(state) = state {
			lines.push(format!("   state:    {state}"));
		}

		let stderr: Vec<&String> = self.stderr.iter().filter(|line| !line.trim().is_empty()).collect();

		for line in stderr.iter().rev().take(MAX_STDERR_LINES).rev() {
			lines.push(format!("   stderr:   {}", client::truncate(line, 160)));
		}

		let start = match self.state {
			State::Hung { pid, .. } => format!(
				"if PID {pid} is a hung wsync (`ps -p {pid}`), kill it, then `{}`",
				start.bold()
			),
			_ if self.desktop => format!("`{}` (or serve it again in the WSync app)", start.bold()),
			_ => format!("`{}`", start.bold()),
		};

		lines.push(format!("   start it: {start}"));

		if self.has_log {
			lines.push(format!("   history:  `wsync daemon logs --project {project}`"));
		}

		let mut banner = lines.join("\n");
		banner.push('\n');
		banner
	}
}

/// `2026-09-22 20:28:05 (35 min ago)` — the journal's local stamp plus its
/// age, which is the part that says how much editing went unsynced
fn when(stamp: &str) -> String {
	let Ok(then) = NaiveDateTime::parse_from_str(stamp, daemon::LOG_STAMP_FORMAT) else {
		return stamp.to_owned();
	};

	let seconds = (Local::now().naive_local() - then).num_seconds();

	match age(seconds) {
		Some(age) => format!("{stamp} ({age})"),
		None => stamp.to_owned(),
	}
}

fn age(seconds: i64) -> Option<String> {
	let minutes = seconds / 60;
	let hours = minutes / 60;

	Some(match seconds {
		..0 => return None,
		0..60 => "just now".to_owned(),
		60..3600 => format!("{minutes} min ago"),
		3600..172_800 => format!("{hours} h {} min ago", minutes % 60),
		_ => format!("{} days ago", hours / 24),
	})
}

/// Single-quotes a path for a copy-pasteable command when it needs it
fn shell_quote(text: &str) -> String {
	if !text.is_empty()
		&& text
			.chars()
			.all(|char| char.is_ascii_alphanumeric() || "/._-~+:@,".contains(char))
	{
		return text.to_owned();
	}

	format!("'{}'", text.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
	use super::*;

	fn evidence(event: LastEvent, at: Option<&str>, stderr: &[&str]) -> LogEvidence {
		LogEvidence {
			event,
			at: at.map(str::to_owned),
			stderr: stderr.iter().map(|line| (*line).to_owned()).collect(),
		}
	}

	fn record(managed_by: &str) -> RuntimeRecord {
		RuntimeRecord {
			version: daemon::RECORD_VERSION,
			project: "/p/default.project.json".into(),
			canonical_project: "/p/default.project.json".into(),
			pid: 4242,
			port: 7978,
			boot_id: "boot".into(),
			control_token: None,
			managed_by: managed_by.into(),
			log_path: String::new(),
			started_at: 0,
		}
	}

	fn project() -> PathBuf {
		PathBuf::from("/p/default.project.json")
	}

	#[test]
	fn a_manager_exit_note_reads_as_a_crash_with_its_stderr() {
		let down = Down::classify(
			project(),
			Some(&record("desktop")),
			false,
			true,
			evidence(
				LastEvent::Exited {
					status: "signal 6 (SIGABRT)".into(),
				},
				Some("2026-09-22 20:28:05"),
				&[
					"thread 'main' has overflowed its stack",
					"fatal runtime error: stack overflow",
				],
			),
		);

		assert_eq!(
			down.state,
			State::Crashed {
				status: Some("signal 6 (SIGABRT)".into())
			}
		);

		let banner = down.render();

		assert!(banner.contains("DOWN"), "{banner}");
		assert!(banner.contains("NOT syncing"), "{banner}");
		assert!(
			banner.contains("crashed — signal 6 (SIGABRT), at 2026-09-22 20:28:05"),
			"{banner}"
		);
		assert!(banner.contains("overflowed its stack"), "{banner}");
		assert!(banner.contains("wsync daemon start --project /p"), "{banner}");
		assert!(banner.contains("WSync app"), "{banner}");
		assert!(banner.contains("wsync daemon logs --project /p"), "{banner}");
	}

	#[test]
	fn record_and_journal_combine_into_the_reported_state() {
		// A record left behind by a process that is gone, with no note
		let crashed = Down::classify(
			project(),
			Some(&record("cli")),
			false,
			true,
			evidence(LastEvent::Silent, Some("2026-09-22 20:00:00"), &[]),
		);
		assert_eq!(crashed.state, State::Crashed { status: None });
		assert!(!crashed.desktop);

		// The recorded process still exists but its port is silent
		let hung = Down::classify(
			project(),
			Some(&record("cli")),
			true,
			true,
			evidence(LastEvent::Silent, None, &[]),
		);
		assert_eq!(hung.state, State::Hung { pid: 4242, port: 7978 });

		// No record: the journal decides
		let stopped = Down::classify(
			project(),
			None,
			false,
			true,
			evidence(
				LastEvent::Stopped {
					reason: Some("manager closed".into()),
				},
				Some("2026-09-22 22:00:01"),
				&[],
			),
		);
		assert_eq!(
			stopped.state,
			State::Stopped {
				reason: Some("manager closed".into())
			}
		);

		let heartbeat = Down::classify(
			project(),
			None,
			false,
			true,
			evidence(
				LastEvent::HeartbeatLost {
					detail: "connection refused".into(),
				},
				None,
				&[],
			),
		);
		assert_eq!(
			heartbeat.state,
			State::HeartbeatLost {
				detail: "connection refused".into()
			}
		);
		assert!(heartbeat.desktop, "only a manager writes heartbeat notes");

		let never = Down::classify(project(), None, false, false, evidence(LastEvent::Empty, None, &[]));
		assert_eq!(never.state, State::NeverStarted);
	}

	#[test]
	fn a_never_started_project_gets_the_short_banner() {
		let banner = Down::classify(project(), None, false, false, evidence(LastEvent::Empty, None, &[])).render();

		assert!(
			banner.contains("No WSync daemon is running for this project"),
			"{banner}"
		);
		assert!(!banner.contains("state:"), "{banner}");
		assert!(!banner.contains("history:"), "{banner}");
		assert!(banner.contains("wsync daemon start --project /p"), "{banner}");
	}

	#[test]
	fn ages_read_as_elapsed_time() {
		assert_eq!(age(-5), None);
		assert_eq!(age(12).as_deref(), Some("just now"));
		assert_eq!(age(35 * 60).as_deref(), Some("35 min ago"));
		assert_eq!(age(2 * 3600 + 5 * 60).as_deref(), Some("2 h 5 min ago"));
		assert_eq!(age(3 * 86_400).as_deref(), Some("3 days ago"));
	}

	#[test]
	fn project_paths_are_quoted_only_when_needed() {
		assert_eq!(shell_quote("/Users/me/game"), "/Users/me/game");
		assert_eq!(shell_quote("/Users/me/My Game"), "'/Users/me/My Game'");
		assert_eq!(shell_quote("/tmp/it's"), r"'/tmp/it'\''s'");
	}
}
