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

use crate::{ext::PathExt, lock};

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
/// decisions. (Full logger output stays on stderr for the process manager;
/// the journal is the machine-findable record.)
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

		Ok(Self {
			file: Mutex::new(options.open(path)?),
		})
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
