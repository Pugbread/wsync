//! Crash-loop protection for the connect.
//!
//! The failure this exists for: a daemon crashes while (or right after)
//! applying a connect, the next start connects again, applies the same thing,
//! and crashes again — each round rewriting disk. A crash cannot be caught
//! reliably (a stack overflow aborts without unwinding), so the guard works
//! from the other end: the connect writes a marker when it starts applying and
//! clears it once the daemon has survived the apply by [`SETTLE`]. A daemon
//! that starts and finds the marker knows the last one died in that window,
//! and comes up in **safe mode**: a connect still compares and decides, but
//! applies nothing to either side, writes what it would have done to
//! `.wsync-backups/safe-mode-report.json`, and refuses the connect with that
//! explanation. `wsync daemon resume` (POST `/safe-mode/resume`) ends it.
//!
//! The marker only spans the risky window on purpose. A daemon stopped with
//! Ctrl+C or killed at any other time is not a crash loop, and must not make
//! the next start refuse to sync.

use log::{error, info, warn};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
	fs,
	path::{Path, PathBuf},
	sync::Mutex,
	time::{Duration, SystemTime, UNIX_EPOCH},
};

use crate::{constants::BACKUPS_DIR, lock};

/// How long a daemon must survive a committed connect before the marker is
/// cleared
pub const SETTLE: Duration = Duration::from_secs(30);

const MARKER_FILE: &str = "connect-in-progress.json";
const REPORT_FILE: &str = "safe-mode-report.json";

fn now_seconds() -> u64 {
	SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.map(|elapsed| elapsed.as_secs())
		.unwrap_or(0)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Marker {
	boot_id: String,
	pid: u32,
	started_at: u64,
}

/// Why this boot is in safe mode
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SafeMode {
	/// When the previous daemon started the connect it did not survive
	pub crashed_connect_started_at: u64,
	pub previous_pid: u32,
	pub previous_boot_id: String,
	/// Where the last refused connect's report was written, once one was
	pub report: Option<String>,
}

pub struct CrashGuard {
	backups_dir: PathBuf,
	boot_id: String,
	safe_mode: Mutex<Option<SafeMode>>,
	/// Connect generation, so only the latest connect's settle clears the
	/// marker
	generation: Mutex<u64>,
}

impl CrashGuard {
	/// Reads what the previous daemon left behind. A leftover marker means it
	/// died applying a connect: this boot starts in safe mode
	pub fn load(workspace_dir: &Path, boot_id: &str) -> Self {
		let backups_dir = workspace_dir.join(BACKUPS_DIR);
		let marker_path = backups_dir.join(MARKER_FILE);

		let safe_mode = fs::read_to_string(&marker_path)
			.ok()
			.and_then(|contents| serde_json::from_str::<Marker>(&contents).ok())
			.map(|marker| {
				error!(
					"The previous WSync daemon (pid {}) died while applying a connect. Safe mode: \
					 connects will compare and report but apply nothing until `wsync daemon resume`",
					marker.pid
				);

				SafeMode {
					crashed_connect_started_at: marker.started_at,
					previous_pid: marker.pid,
					previous_boot_id: marker.boot_id,
					report: None,
				}
			});

		Self {
			backups_dir,
			boot_id: boot_id.to_owned(),
			safe_mode: Mutex::new(safe_mode),
			generation: Mutex::new(0),
		}
	}

	pub fn safe_mode(&self) -> Option<SafeMode> {
		lock!(self.safe_mode).clone()
	}

	pub fn to_json(&self) -> Value {
		match self.safe_mode() {
			Some(safe_mode) => json!(safe_mode),
			None => Value::Null,
		}
	}

	/// Leaves safe mode and forgets the crash. Returns whether it was on
	pub fn resume(&self) -> bool {
		let was = lock!(self.safe_mode).take().is_some();

		fs::remove_file(self.backups_dir.join(MARKER_FILE)).ok();

		if was {
			info!("Safe mode ended: the next connect applies normally");
		}

		was
	}

	/// A connect is about to write disk or Studio. Returns its generation for
	/// [`CrashGuard::settle`]
	pub fn begin(&self) -> u64 {
		let generation = {
			let mut generation = lock!(self.generation);
			*generation += 1;
			*generation
		};

		let marker = Marker {
			boot_id: self.boot_id.clone(),
			pid: std::process::id(),
			started_at: now_seconds(),
		};

		let written = fs::create_dir_all(&self.backups_dir).and_then(|()| {
			fs::write(
				self.backups_dir.join(MARKER_FILE),
				serde_json::to_vec(&marker).unwrap_or_default(),
			)
		});

		if let Err(err) = written {
			warn!("Failed to write the connect marker (crash-loop protection is off for this connect): {err}");
		}

		generation
	}

	/// The daemon survived the connect: clear the marker once it has also
	/// survived [`SETTLE`], unless a newer connect took over meanwhile
	pub fn settle(self: &std::sync::Arc<Self>, generation: u64) {
		let guard = self.clone();

		tokio::spawn(async move {
			tokio::time::sleep(SETTLE).await;
			guard.clear_if_current(generation);
		});
	}

	fn clear_if_current(&self, generation: u64) {
		if *lock!(self.generation) == generation {
			fs::remove_file(self.backups_dir.join(MARKER_FILE)).ok();
		}
	}

	/// A graceful shutdown is never a crash
	pub fn on_shutdown(&self) {
		fs::remove_file(self.backups_dir.join(MARKER_FILE)).ok();
	}

	/// Writes what a refused safe-mode connect would have done, returning the
	/// report's path
	pub fn write_report(&self, report: &Value) -> Option<PathBuf> {
		let path = self.backups_dir.join(REPORT_FILE);

		let written = fs::create_dir_all(&self.backups_dir)
			.and_then(|()| fs::write(&path, serde_json::to_vec_pretty(report).unwrap_or_default()));

		match written {
			Ok(()) => {
				if let Some(safe_mode) = lock!(self.safe_mode).as_mut() {
					safe_mode.report = Some(path.to_string_lossy().into_owned());
				}

				Some(path)
			}
			Err(err) => {
				warn!("Failed to write the safe-mode report: {err}");
				None
			}
		}
	}
}

/// `POST /safe-mode/resume` — leave safe mode; the next connect applies
pub async fn resume(axum::extract::State(state): axum::extract::State<crate::server::AppState>) -> axum::Json<Value> {
	let was = state.crash_guard.resume();

	axum::Json(json!({ "ok": true, "wasInSafeMode": was }))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn a_marker_left_behind_starts_safe_mode_and_resume_ends_it() {
		let dir = tempfile::tempdir().unwrap();

		let first = CrashGuard::load(dir.path(), "boot-1");
		assert!(first.safe_mode().is_none());

		// The connect starts applying … and the process dies here
		first.begin();

		let second = CrashGuard::load(dir.path(), "boot-2");
		let safe_mode = second.safe_mode().expect("a crash mid-connect must start safe mode");

		assert_eq!(safe_mode.previous_boot_id, "boot-1");
		assert!(second.resume());
		assert!(second.safe_mode().is_none());

		let third = CrashGuard::load(dir.path(), "boot-3");
		assert!(third.safe_mode().is_none(), "resume forgets the crash");
	}

	#[test]
	fn a_settled_or_graceful_connect_leaves_nothing_behind() {
		let dir = tempfile::tempdir().unwrap();

		let guard = CrashGuard::load(dir.path(), "boot-1");
		let generation = guard.begin();
		guard.clear_if_current(generation);

		assert!(CrashGuard::load(dir.path(), "boot-2").safe_mode().is_none());

		let guard = CrashGuard::load(dir.path(), "boot-3");
		let stale = guard.begin();
		guard.begin();

		// An older connect's settle must not clear a newer connect's marker
		guard.clear_if_current(stale);
		assert!(CrashGuard::load(dir.path(), "boot-4").safe_mode().is_some());

		guard.on_shutdown();
		assert!(CrashGuard::load(dir.path(), "boot-5").safe_mode().is_none());
	}
}
