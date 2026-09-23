//! Daemon lifecycle state: per-project runtime records, OS-exclusive start
//! locks, per-daemon lifecycle journals and the managed-watchdog timer math
//! (Design §3.2, §11, §12 — the Ro-Sync model, reimplemented clean-room).

use anyhow::{anyhow, bail, Context, Result};
use chrono::Local;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
	env, fs,
	io::Write,
	path::{Path, PathBuf},
	sync::Mutex,
	time::{Duration, SystemTime, UNIX_EPOCH},
};

use crate::{crash_handler, ext::PathExt, lock};

/// Bump when the record schema changes incompatibly
pub const RECORD_VERSION: u32 = 1;

/// Environment override for the state directory (Design §12)
pub const STATE_DIR_ENV: &str = "WSYNC_STATE_DIR";

/// A managed daemon self-terminates when its manager has not heartbeated for
/// this long...
pub const MANAGER_HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// ...but a freshly expired heartbeat is treated as *suspect* first and only
/// commits to shutdown after this grace window, so a machine waking from
/// sleep gives its manager a chance to run (`Instant` advances during sleep
/// on supported platforms)
pub const HEARTBEAT_SUSPECT_GRACE: Duration = Duration::from_secs(30);

/// Authoritative per-project runtime record, written by the daemon process
/// itself once its listener is bound. Contains the control token for managed
/// daemons, so records are always private (0600 file in a 0700 directory)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeRecord {
	pub version: u32,
	pub project: String,
	pub canonical_project: String,
	pub pid: u32,
	pub port: u16,
	pub boot_id: String,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub control_token: Option<String>,
	pub managed_by: String,
	pub log_path: String,
	pub started_at: u64,
}

#[derive(Debug, Clone)]
pub struct RuntimePaths {
	pub record: PathBuf,
	pub start_lock: PathBuf,
	pub log: PathBuf,
}

/// Resolves the WSync state directory: explicit `--data-dir` flag first, the
/// `WSYNC_STATE_DIR` environment variable second, then the platform data dir
/// (`~/Library/Application Support/WSync`, `%LOCALAPPDATA%\WSync`,
/// `~/.local/share/WSync`)
pub fn state_dir(explicit: Option<&Path>) -> Result<PathBuf> {
	if let Some(explicit) = explicit {
		return explicit.resolve();
	}

	if let Some(value) = env::var_os(STATE_DIR_ENV) {
		if !value.is_empty() {
			return Path::new(&value).resolve();
		}
	}

	let base = directories::BaseDirs::new().context("Failed to resolve the platform data directory")?;

	Ok(base.data_local_dir().join("WSync"))
}

/// Canonicalizes a project file path into the daemon's identity string. Both
/// the runtime record and `GET /hello` derive `canonicalProject` this way,
/// so identity comparisons are exact string matches
pub fn canonical_project(project_path: &Path) -> Result<String> {
	let canonical = fs::canonicalize(project_path)
		.with_context(|| format!("Failed to canonicalize project path {}", project_path.display()))?;

	Ok(canonical.to_string())
}

/// Stable state-file key for one canonical project
pub fn project_key(canonical_project: &str) -> String {
	let mut digest = Sha256::new();
	digest.update(canonical_project.as_bytes());

	format!("{:x}", digest.finalize())
}

pub fn runtime_paths(state_dir: &Path, canonical_project: &str) -> RuntimePaths {
	let key = project_key(canonical_project);
	let daemons = state_dir.join("daemons");

	RuntimePaths {
		record: daemons.join(format!("{key}.json")),
		start_lock: daemons.join(format!("{key}.start.lock")),
		log: daemons.join(format!("{key}.log")),
	}
}

pub fn unix_now() -> u64 {
	SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.unwrap_or_default()
		.as_secs()
}

fn create_private_dir(path: &Path) -> Result<()> {
	fs::create_dir_all(path)?;

	#[cfg(unix)]
	{
		use std::os::unix::fs::PermissionsExt;
		fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
	}

	Ok(())
}

/// Atomically writes private bytes: staged as a 0600 sibling temp file, then
/// renamed over the destination so readers never observe a partial record
fn write_private_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
	let parent = path.parent().context("Invalid state path")?;

	create_private_dir(parent)?;

	let name = path
		.file_name()
		.and_then(|name| name.to_str())
		.context("Invalid state file name")?;
	let temporary = parent.join(format!(".{name}.{}.tmp", std::process::id()));

	let result = (|| -> Result<()> {
		let mut options = fs::OpenOptions::new();
		options.write(true).create_new(true);

		#[cfg(unix)]
		{
			use std::os::unix::fs::OpenOptionsExt;
			options.mode(0o600);
		}

		let mut file = options.open(&temporary)?;

		file.write_all(bytes)?;
		file.write_all(b"\n")?;
		file.sync_all()?;
		drop(file);

		fs::rename(&temporary, path)?;

		Ok(())
	})();

	if result.is_err() {
		fs::remove_file(&temporary).ok();
	}

	result
}

pub fn write_record(path: &Path, record: &RuntimeRecord) -> Result<()> {
	let bytes = serde_json::to_vec_pretty(record)?;

	write_private_atomic(path, &bytes)
}

pub fn read_record(path: &Path) -> Result<Option<RuntimeRecord>> {
	let text = match fs::read_to_string(path) {
		Ok(text) => text,
		Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
		Err(err) => return Err(err.into()),
	};

	let record: RuntimeRecord = serde_json::from_str(&text)
		.map_err(|err| anyhow!("Corrupted daemon runtime record {}: {err}", path.display()))?;

	if record.version != RECORD_VERSION {
		bail!(
			"Unsupported daemon runtime record version {} (expected {})",
			record.version,
			RECORD_VERSION
		);
	}

	Ok(Some(record))
}

/// Removes the record only when it still belongs to `boot_id`, so a crashed
/// daemon's successor is never deleted by the predecessor's cleanup
pub fn remove_record_if_boot(path: &Path, boot_id: &str) -> Result<bool> {
	let Some(record) = read_record(path)? else {
		return Ok(false);
	};

	if record.boot_id != boot_id {
		return Ok(false);
	}

	match fs::remove_file(path) {
		Ok(()) => Ok(true),
		Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
		Err(err) => Err(err.into()),
	}
}

/// OS-exclusive start lock serializing the daemon-start critical section for
/// one project. The lock file is persistent by design: unlinking it on drop
/// would let another process lock a fresh inode while a waiter still holds
/// the old one. Exclusivity comes from the held OS lock (released by the OS
/// even after a crash), so a stale file left behind is harmless — acquiring
/// truncates and rewrites the diagnostics
pub struct StartLock {
	file: fs::File,
}

impl StartLock {
	pub fn acquire(path: &Path) -> Result<Self> {
		let parent = path.parent().context("Invalid start-lock path")?;

		create_private_dir(parent)?;

		let mut options = fs::OpenOptions::new();
		options.read(true).write(true).create(true).truncate(false);

		#[cfg(unix)]
		{
			use std::os::unix::fs::OpenOptionsExt;
			options.mode(0o600);
		}

		let file = options.open(path)?;

		file.try_lock().map_err(|err| match err {
			fs::TryLockError::WouldBlock => {
				anyhow!(
					"Another daemon start for this project is already in progress (lock {})",
					path.display()
				)
			}
			fs::TryLockError::Error(err) => anyhow!("Failed to lock {}: {err}", path.display()),
		})?;

		// Contents are diagnostic only; rewriting them also proves a stale
		// file from a crashed process has been recovered
		file.set_len(0)?;

		let mut file = file;
		writeln!(file, "pid={}", std::process::id())?;
		writeln!(file, "acquiredAt={}", unix_now())?;
		file.sync_all()?;

		Ok(Self { file })
	}
}

impl Drop for StartLock {
	fn drop(&mut self) {
		// Explicit for clarity; dropping the handle releases the OS lock
		let _ = self.file.sync_all();
	}
}

/// Per-daemon lifecycle journal backing `wsync daemon logs`: timestamped
/// one-line notes for boot, plugin connects, stop reasons and watchdog
/// decisions — and crashes: opening the journal routes the panic hook's
/// report (message, location, the path being processed) into it too. (Full
/// logger output stays on stderr for the process manager; the journal is
/// the machine-findable record.)
#[derive(Debug)]
pub struct DaemonLog {
	file: Mutex<fs::File>,
}

impl DaemonLog {
	pub fn open(path: &Path) -> Result<Self> {
		let parent = path.parent().context("Invalid daemon log path")?;

		create_private_dir(parent)?;

		let mut options = fs::OpenOptions::new();
		options.create(true).append(true);

		#[cfg(unix)]
		{
			use std::os::unix::fs::OpenOptionsExt;
			options.mode(0o600);
		}

		let file = options.open(path)?;

		// An unattended daemon has nobody reading stderr: its crash report
		// belongs where `wsync daemon logs` looks
		crash_handler::set_panic_log(path);

		Ok(Self { file: Mutex::new(file) })
	}

	pub fn note(&self, message: &str) {
		let mut file = lock!(self.file);
		let stamp = Local::now().format("%Y-%m-%d %H:%M:%S");

		writeln!(file, "[{stamp}] {message}").ok();
	}
}

/// Reads the last `lines` lines of a daemon log
pub fn read_log_tail(path: &Path, lines: usize) -> Result<String> {
	let text = fs::read_to_string(path)?;
	let all: Vec<&str> = text.lines().collect();
	let start = all.len().saturating_sub(lines);

	Ok(all[start..].join("\n"))
}

/// The journal's timestamp layout, as [`DaemonLog::note`] writes it
pub const LOG_STAMP_FORMAT: &str = "%Y-%m-%d %H:%M:%S";

// Notes a lifecycle *manager* appends to a daemon's journal after the daemon
// itself can no longer write anything (the desktop app, `desktop/src-tauri/
// src/daemon.rs`). A daemon killed by a stack overflow or a signal leaves no
// line of its own, so these are the only evidence of how it died. The
// prefixes are a contract: the daemon-down banner keys on them.

/// `Manager: daemon exited unexpectedly (<status>)[; last stderr lines follow]`
pub const MANAGER_EXIT_NOTE: &str = "Manager: daemon exited unexpectedly";
/// `Manager: heartbeat lost (<detail>)`
pub const MANAGER_HEARTBEAT_NOTE: &str = "Manager: heartbeat lost";
/// `Manager: stderr | <line>` — one per captured stderr line, right after
/// the exit or heartbeat note
pub const MANAGER_STDERR_NOTE: &str = "Manager: stderr |";

/// Reads at most the last `max_bytes` of a daemon log as lines. Journals are
/// append-only and grow for as long as a project is served, so callers that
/// only need the latest events never load the whole file
pub fn read_log_tail_lines(path: &Path, max_bytes: u64) -> Result<Vec<String>> {
	use std::io::{Read, Seek, SeekFrom};

	let mut file = fs::File::open(path)?;
	let length = file.metadata()?.len();
	// One byte before the window tells whether it starts on a line boundary
	let start = length.saturating_sub(max_bytes).saturating_sub(1);

	file.seek(SeekFrom::Start(start))?;

	let mut bytes = Vec::new();
	file.read_to_end(&mut bytes)?;

	if start > 0 {
		// Drop the partial line the window starts inside (or just the
		// boundary newline when it starts on a fresh line)
		let cut = bytes
			.iter()
			.position(|byte| *byte == b'\n')
			.map_or(bytes.len(), |index| index + 1);

		bytes.drain(..cut);
	}

	Ok(String::from_utf8_lossy(&bytes).lines().map(str::to_owned).collect())
}

/// The last lifecycle event a daemon journal records
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LastEvent {
	/// A manager saw the process exit without being asked to
	/// (`status` is the manager's rendering: `signal 6 (SIGABRT)`, `exit code 101`)
	Exited { status: String },
	/// A manager stopped getting heartbeat answers
	HeartbeatLost { detail: String },
	/// The daemon journalled its own graceful stop
	Stopped { reason: Option<String> },
	/// The journal just ends mid-life: no stop, no manager note. The process
	/// died without writing anything (a crash, SIGKILL, power loss)
	Silent,
	/// No journal lines at all
	Empty,
}

/// What a daemon journal says about the daemon's end
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogEvidence {
	pub event: LastEvent,
	/// Timestamp of the line that decided `event` (for [`LastEvent::Silent`],
	/// the last line of any kind — the last sign of life)
	pub at: Option<String>,
	/// `Manager: stderr | …` lines attached to the exit or heartbeat note
	pub stderr: Vec<String>,
}

/// Splits `[stamp] message` into its parts; lines without a stamp keep the
/// whole text as the message
fn split_log_line(line: &str) -> (Option<&str>, &str) {
	line.strip_prefix('[')
		.and_then(|rest| rest.split_once("] "))
		.map_or((None, line), |(stamp, message)| (Some(stamp), message))
}

/// `Manager: … (<detail>)[; last stderr lines follow]` → `<detail>`
fn manager_detail(rest: &str) -> String {
	let rest = rest.trim();
	let rest = rest.strip_suffix("; last stderr lines follow").unwrap_or(rest).trim();

	rest.strip_prefix('(')
		.and_then(|inner| inner.strip_suffix(')'))
		.unwrap_or(rest)
		.trim()
		.to_owned()
}

/// Reads a journal tail backwards to its last lifecycle event. Pure, so the
/// banner's wording is unit-tested against the exact lines daemons and
/// managers write
pub fn last_event(lines: &[String]) -> LogEvidence {
	let lines: Vec<&str> = lines
		.iter()
		.map(|line| line.trim_end())
		.filter(|line| !line.is_empty())
		.collect();
	let mut stderr = Vec::new();
	let mut index = lines.len();

	// Trailing stderr lines belong to the manager note right above them
	while index > 0 {
		let (_, message) = split_log_line(lines[index - 1]);

		// An empty captured line is journalled as `Manager: stderr | ` and
		// arrives here with its trailing space trimmed
		match message.strip_prefix(MANAGER_STDERR_NOTE) {
			Some(line) => stderr.push(line.strip_prefix(' ').unwrap_or(line).to_owned()),
			None => break,
		}

		index -= 1;
	}

	stderr.reverse();

	let Some(last) = index.checked_sub(1).map(|index| lines[index]) else {
		return LogEvidence {
			event: LastEvent::Empty,
			at: None,
			stderr,
		};
	};

	let (at, message) = split_log_line(last);
	let at = at.map(str::to_owned);

	let event = if let Some(rest) = message.strip_prefix(MANAGER_EXIT_NOTE) {
		LastEvent::Exited {
			status: manager_detail(rest),
		}
	} else if let Some(rest) = message.strip_prefix(MANAGER_HEARTBEAT_NOTE) {
		LastEvent::HeartbeatLost {
			detail: manager_detail(rest),
		}
	} else if let Some(reason) = message.strip_prefix("Stopping: ") {
		LastEvent::Stopped {
			reason: Some(reason.trim().to_owned()),
		}
	} else if message == "Daemon stopped" {
		// The graceful path journals `Stopping: <reason>` just before this
		let reason = lines[..index - 1]
			.iter()
			.rev()
			.take(4)
			.map(|line| split_log_line(line).1)
			.find_map(|message| message.strip_prefix("Stopping: "))
			.map(|reason| reason.trim().to_owned());

		LastEvent::Stopped { reason }
	} else {
		LastEvent::Silent
	};

	LogEvidence { event, at, stderr }
}

// Watchdog timer math (pure, unit-tested; the async loop lives in
// `server::lifecycle`)

/// The manager heartbeat is expired once it is older than the timeout
pub fn heartbeat_expired(last_seen_elapsed: Duration, timeout: Duration) -> bool {
	last_seen_elapsed > timeout
}

/// A daemon only self-terminates when the heartbeat is expired *and* the
/// suspect window that started at first observation has also elapsed
pub fn watchdog_should_terminate(
	last_seen_elapsed: Duration,
	suspect_elapsed: Option<Duration>,
	timeout: Duration,
	grace: Duration,
) -> bool {
	heartbeat_expired(last_seen_elapsed, timeout) && suspect_elapsed.is_some_and(|elapsed| elapsed > grace)
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::path::Path;

	fn record(boot_id: &str) -> RuntimeRecord {
		RuntimeRecord {
			version: RECORD_VERSION,
			project: "/tmp/project/default.project.json".into(),
			canonical_project: "/tmp/project/default.project.json".into(),
			pid: 42,
			port: 7978,
			boot_id: boot_id.into(),
			control_token: Some("secret".into()),
			managed_by: "test".into(),
			log_path: "/tmp/daemon.log".into(),
			started_at: 123,
		}
	}

	#[test]
	fn project_key_is_stable_and_input_specific() {
		let first = project_key("/tmp/one");
		let second = project_key("/tmp/two");

		assert_eq!(first, project_key("/tmp/one"));
		assert_ne!(first, second);
		assert_eq!(first.len(), 64);
	}

	#[test]
	fn record_round_trips_and_boot_guard_holds() {
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("daemons").join("record.json");
		let record = record("boot-a");

		write_record(&path, &record).unwrap();
		assert_eq!(read_record(&path).unwrap(), Some(record.clone()));

		// A different boot must never delete the record
		assert!(!remove_record_if_boot(&path, "boot-b").unwrap());
		assert!(path.exists());

		assert!(remove_record_if_boot(&path, "boot-a").unwrap());
		assert!(!path.exists());

		// Removing an absent record is not an error
		assert!(!remove_record_if_boot(&path, "boot-a").unwrap());
	}

	#[test]
	fn corrupted_records_error_instead_of_passing_as_absent() {
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("record.json");

		std::fs::write(&path, "not json").unwrap();

		assert!(read_record(&path).is_err());
	}

	#[cfg(unix)]
	#[test]
	fn records_are_private_on_unix() {
		use std::os::unix::fs::PermissionsExt;

		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("daemons").join("record.json");

		write_record(&path, &record("boot")).unwrap();

		assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
		assert_eq!(
			std::fs::metadata(path.parent().unwrap()).unwrap().permissions().mode() & 0o777,
			0o700
		);
	}

	#[test]
	fn start_lock_is_exclusive_and_recovers_after_release() {
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("daemons").join("x.start.lock");

		let first = StartLock::acquire(&path).unwrap();
		assert!(StartLock::acquire(&path).is_err());

		drop(first);
		StartLock::acquire(&path).unwrap();
		assert!(path.exists());
	}

	#[test]
	fn start_lock_recovers_from_a_stale_unlocked_file() {
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("x.start.lock");
		let stale = format!("pid=999999\nacquiredAt=1\nstale={}\n", "x".repeat(256));

		std::fs::write(&path, &stale).unwrap();

		let lock = StartLock::acquire(&path).unwrap();
		drop(lock);

		let contents = std::fs::read_to_string(&path).unwrap();
		assert!(contents.contains(&format!("pid={}", std::process::id())));
		assert!(!contents.contains("stale="));
	}

	#[test]
	fn state_dir_prefers_explicit_over_environment() {
		let explicit = state_dir(Some(Path::new("/tmp/explicit-state"))).unwrap();
		assert_eq!(explicit, PathBuf::from("/tmp/explicit-state"));
	}

	fn lines(text: &str) -> Vec<String> {
		text.lines().map(str::to_owned).collect()
	}

	#[test]
	fn log_evidence_reads_a_manager_exit_note_and_its_stderr() {
		let evidence = last_event(&lines(
			"[2026-09-22 20:01:00] Listening on localhost:7978 (PID 42)\n\
			 [2026-09-22 20:05:10] Plugin connected: Place (ws)\n\
			 [2026-09-22 20:28:05] Manager: daemon exited unexpectedly (signal 6 (SIGABRT)); last stderr lines follow\n\
			 [2026-09-22 20:28:05] Manager: stderr | \n\
			 [2026-09-22 20:28:05] Manager: stderr | thread 'main' has overflowed its stack\n\
			 [2026-09-22 20:28:05] Manager: stderr | fatal runtime error: stack overflow\n",
		));

		assert_eq!(
			evidence.event,
			LastEvent::Exited {
				status: "signal 6 (SIGABRT)".into()
			}
		);
		assert_eq!(evidence.at.as_deref(), Some("2026-09-22 20:28:05"));
		assert_eq!(
			evidence.stderr,
			vec![
				String::new(),
				"thread 'main' has overflowed its stack".to_owned(),
				"fatal runtime error: stack overflow".to_owned()
			]
		);
	}

	#[test]
	fn log_evidence_reads_heartbeat_loss_and_graceful_stops() {
		let heartbeat = last_event(&lines(
			"[2026-09-22 20:01:00] Plugin connected: Place (ws)\n\
			 [2026-09-22 21:00:00] Manager: heartbeat lost (/manager-heartbeat answered 403)\n",
		));

		assert_eq!(
			heartbeat.event,
			LastEvent::HeartbeatLost {
				detail: "/manager-heartbeat answered 403".into()
			}
		);
		assert!(heartbeat.stderr.is_empty());

		let stopped = last_event(&lines(
			"[2026-09-22 20:01:00] Plugin connected: Place (ws)\n\
			 [2026-09-22 22:00:00] Stopping: manager closed\n\
			 [2026-09-22 22:00:01] Daemon stopped\n",
		));

		assert_eq!(
			stopped.event,
			LastEvent::Stopped {
				reason: Some("manager closed".into())
			}
		);
		assert_eq!(stopped.at.as_deref(), Some("2026-09-22 22:00:01"));

		let signalled = last_event(&lines("[2026-09-22 22:00:00] Stopping: signal received\n"));

		assert_eq!(
			signalled.event,
			LastEvent::Stopped {
				reason: Some("signal received".into())
			}
		);
	}

	#[test]
	fn log_evidence_calls_a_journal_that_just_ends_silent() {
		let silent = last_event(&lines(
			"[2026-09-22 20:01:00] Listening on localhost:7978 (PID 42)\n\
			 [2026-09-22 20:05:10] Plugin disconnected: Place\n",
		));

		assert_eq!(silent.event, LastEvent::Silent);
		assert_eq!(silent.at.as_deref(), Some("2026-09-22 20:05:10"));

		assert_eq!(last_event(&[]).event, LastEvent::Empty);
		assert_eq!(last_event(&lines("\n\n")).event, LastEvent::Empty);
	}

	#[test]
	fn log_tail_lines_drops_the_partial_first_line() {
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("daemon.log");

		std::fs::write(&path, "first line\nsecond line\nthird line\n").unwrap();

		assert_eq!(
			read_log_tail_lines(&path, 1024).unwrap(),
			vec!["first line", "second line", "third line"]
		);
		// 16 bytes from the end starts inside "second line"
		assert_eq!(read_log_tail_lines(&path, 16).unwrap(), vec!["third line"]);
		// 23 bytes from the end starts exactly on "second line"
		assert_eq!(
			read_log_tail_lines(&path, 23).unwrap(),
			vec!["second line", "third line"]
		);
	}

	#[test]
	fn watchdog_timer_math() {
		let timeout = MANAGER_HEARTBEAT_TIMEOUT;
		let grace = HEARTBEAT_SUSPECT_GRACE;
		let secs = Duration::from_secs;

		// Healthy heartbeat: nothing expires
		assert!(!heartbeat_expired(secs(299), timeout));
		assert!(!watchdog_should_terminate(secs(299), None, timeout, grace));

		// Freshly expired: suspect window opens but must not yet terminate
		assert!(heartbeat_expired(secs(301), timeout));
		assert!(!watchdog_should_terminate(secs(301), Some(secs(0)), timeout, grace));
		assert!(!watchdog_should_terminate(secs(320), Some(secs(29)), timeout, grace));

		// Suspect grace elapsed with the heartbeat still stale: terminate
		assert!(watchdog_should_terminate(secs(340), Some(secs(31)), timeout, grace));

		// A heartbeat that recovered mid-grace clears the suspicion — the
		// watchdog loop resets suspect_since, modeled here by the expiry
		// check failing again
		assert!(!watchdog_should_terminate(secs(1), Some(secs(31)), timeout, grace));

		// Laptop sleep: a huge elapsed gap alone is still gated on the grace
		// window re-probe
		assert!(!watchdog_should_terminate(secs(7200), Some(secs(2)), timeout, grace));
		assert!(watchdog_should_terminate(secs(7200), Some(secs(40)), timeout, grace));
	}
}
