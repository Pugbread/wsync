//! The backlog: disk content that lost to Studio.
//!
//! WSync is Studio-first with no questions asked — a connect applies Studio
//! over the project, and a mid-session clash resolves the same way. Neither
//! stops to ask, because being interrupted by a decision you did not plan to
//! make is worse than the loss it is guarding against, and the guard is what
//! this module provides instead.
//!
//! Whenever disk content would be overwritten or dropped, its bytes are moved
//! here first, under `<workspace>/.wsync-backups/backlog/<id>/`, with an index
//! recording where it came from and why. The app lists what is waiting and can
//! put an entry back — restoring the file and pushing it to Studio, which is
//! the old "keep disk" answer, deferred until you actually want it.
//!
//! Entries expire a day after capture. That is deliberately short: the backlog
//! is a safety net for the edit you did not mean to lose, not a version
//! history, and one left to grow forever would quietly accumulate every stale
//! file the project ever disagreed about.

use anyhow::{Context, Result};
use log::{debug, warn};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
	fs,
	path::{Path, PathBuf},
	sync::Mutex,
	time::{SystemTime, UNIX_EPOCH},
};

use uuid::Uuid;

use crate::{constants::BACKUPS_DIR, lock};

/// Directory under `.wsync-backups` that holds the backlog
const BACKLOG_DIR: &str = "backlog";

/// The persisted index inside [`BACKLOG_DIR`]
const BACKLOG_INDEX: &str = "index.json";

/// How long an entry survives. Short on purpose: this is a safety net for a
/// loss you have not noticed yet, not a history you can browse next week.
pub const ENTRY_TTL_SECONDS: u64 = 24 * 60 * 60;

/// Why a piece of disk content lost
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Reason {
	/// The connect-time Studio-first apply overwrote or removed it
	#[serde(rename = "initial-sync")]
	InitialSync,
	/// A mid-session change clashed with Studio and Studio won
	#[serde(rename = "conflict")]
	Conflict,
}

impl Reason {
	pub fn as_str(self) -> &'static str {
		match self {
			Self::InitialSync => "initial-sync",
			Self::Conflict => "conflict",
		}
	}
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
	pub id: String,
	/// Workspace-relative path the content came from, and where a restore
	/// puts it back
	pub path: String,
	pub reason: Reason,
	/// Unix seconds; an entry is swept `ENTRY_TTL_SECONDS` after this
	pub captured_at: u64,
	pub bytes: u64,
}

impl Entry {
	pub fn expires_at(&self) -> u64 {
		self.captured_at + ENTRY_TTL_SECONDS
	}

	pub fn to_json(&self, now: u64) -> Value {
		json!({
			"id": self.id,
			"path": self.path,
			"reason": self.reason.as_str(),
			"capturedAt": self.captured_at,
			"expiresAt": self.expires_at(),
			"secondsRemaining": self.expires_at().saturating_sub(now),
			"bytes": self.bytes,
		})
	}
}

fn now_seconds() -> u64 {
	SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.map(|elapsed| elapsed.as_secs())
		.unwrap_or(0)
}

/// A workspace-relative path must stay inside the workspace: relative, with no
/// parent traversal. Anything else is refused rather than joined onto the
/// workspace — a backlog entry names a path a restore will write to.
pub fn safe_rel_path(path: &str) -> bool {
	!path.is_empty()
		&& !path.starts_with('/')
		&& !path.contains('\\')
		&& !Path::new(path)
			.components()
			.any(|component| !matches!(component, std::path::Component::Normal(_)))
}

pub struct BacklogStore {
	workspace_dir: PathBuf,
	entries: Mutex<Vec<Entry>>,
}

impl BacklogStore {
	pub fn load(workspace_dir: &Path) -> Self {
		let store = Self {
			workspace_dir: workspace_dir.to_owned(),
			entries: Mutex::new(Vec::new()),
		};

		if let Ok(contents) = fs::read_to_string(store.index_path()) {
			match serde_json::from_str::<Vec<Entry>>(&contents) {
				Ok(entries) => {
					*lock!(store.entries) = entries.into_iter().filter(|entry| safe_rel_path(&entry.path)).collect()
				}
				Err(err) => warn!("Failed to read the backlog index: {err}"),
			}
		}

		store.sweep();
		store
	}

	fn root(&self) -> PathBuf {
		self.workspace_dir.join(BACKUPS_DIR).join(BACKLOG_DIR)
	}

	fn index_path(&self) -> PathBuf {
		self.root().join(BACKLOG_INDEX)
	}

	fn entry_dir(&self, id: &str) -> PathBuf {
		self.root().join(id)
	}

	fn persist(&self, entries: &[Entry]) {
		let root = self.root();

		if let Err(err) = fs::create_dir_all(&root) {
			warn!("Failed to create the backlog directory: {err}");
			return;
		}

		match serde_json::to_string_pretty(entries) {
			Ok(contents) => {
				if let Err(err) = fs::write(self.index_path(), contents) {
					warn!("Failed to persist the backlog index: {err}");
				}
			}
			Err(err) => warn!("Failed to serialize the backlog index: {err}"),
		}
	}

	/// Drops expired entries and their stored bytes. Called on load, and before
	/// every read, so an expiry never depends on the app being open.
	pub fn sweep(&self) {
		let now = now_seconds();
		let mut entries = lock!(self.entries);
		let before = entries.len();

		entries.retain(|entry| {
			if entry.expires_at() > now {
				return true;
			}

			fs::remove_dir_all(self.entry_dir(&entry.id)).ok();
			false
		});

		if entries.len() != before {
			debug!("Backlog swept {} expired entr(ies)", before - entries.len());

			let snapshot = entries.clone();
			drop(entries);
			self.persist(&snapshot);
		}
	}

	/// Moves `source` (a live workspace file) into the backlog, recording that
	/// it came from `rel_path`. The file leaves the workspace: this is called
	/// as the content is being replaced, and a copy left behind would be the
	/// divergence the move exists to end.
	pub fn capture(&self, rel_path: &str, source: &Path, reason: Reason) -> Option<Entry> {
		if !safe_rel_path(rel_path) || !source.exists() {
			return None;
		}

		let id = Uuid::new_v4().to_string();
		let target = self.entry_dir(&id).join(rel_path);

		if let Some(parent) = target.parent() {
			if let Err(err) = fs::create_dir_all(parent) {
				warn!("Failed to prepare a backlog entry for {rel_path}: {err}");
				return None;
			}
		}

		let bytes = fs::metadata(source).map(|meta| meta.len()).unwrap_or(0);

		if let Err(err) = fs::rename(source, &target) {
			// Across devices, or a directory: fall back to copy + remove
			if copy_tree(source, &target).is_err() {
				warn!("Failed to move {rel_path} into the backlog: {err}");
				return None;
			}

			remove_any(source);
		}

		let entry = Entry {
			id,
			path: rel_path.to_owned(),
			reason,
			captured_at: now_seconds(),
			bytes,
		};

		let mut entries = lock!(self.entries);

		entries.push(entry.clone());

		let snapshot = entries.clone();

		drop(entries);
		self.persist(&snapshot);

		debug!("Backlogged {rel_path} ({})", reason.as_str());

		Some(entry)
	}

	pub fn list(&self) -> Vec<Entry> {
		self.sweep();

		lock!(self.entries).clone()
	}

	pub fn get(&self, id: &str) -> Option<Entry> {
		self.sweep();

		lock!(self.entries).iter().find(|entry| entry.id == id).cloned()
	}

	/// The stored file for an entry — what a restore writes back
	pub fn stored_path(&self, entry: &Entry) -> PathBuf {
		self.entry_dir(&entry.id).join(&entry.path)
	}

	/// Restores an entry's bytes to its original workspace path and forgets it.
	/// The caller is responsible for pushing the result to Studio.
	pub fn restore(&self, id: &str) -> Result<PathBuf> {
		let entry = self.get(id).context("no such backlog entry")?;
		let stored = self.stored_path(&entry);
		let live = self.workspace_dir.join(&entry.path);

		if let Some(parent) = live.parent() {
			fs::create_dir_all(parent).with_context(|| format!("could not create {}", parent.display()))?;
		}

		copy_tree(&stored, &live).with_context(|| format!("could not restore {}", entry.path))?;
		self.forget(id);

		Ok(live)
	}

	/// Drops an entry and its stored bytes
	pub fn forget(&self, id: &str) -> bool {
		let mut entries = lock!(self.entries);
		let before = entries.len();

		entries.retain(|entry| entry.id != id);

		if entries.len() == before {
			return false;
		}

		fs::remove_dir_all(self.entry_dir(id)).ok();

		let snapshot = entries.clone();

		drop(entries);
		self.persist(&snapshot);

		true
	}

	/// Drops every entry
	pub fn clear(&self) -> usize {
		let mut entries = lock!(self.entries);
		let dropped = entries.len();

		entries.clear();
		drop(entries);

		fs::remove_dir_all(self.root()).ok();

		dropped
	}

	pub fn to_json(&self) -> Value {
		let now = now_seconds();
		let entries = self.list();

		json!({
			"total": entries.len(),
			"ttlSeconds": ENTRY_TTL_SECONDS,
			"entries": entries.iter().map(|entry| entry.to_json(now)).collect::<Vec<Value>>(),
		})
	}
}

fn remove_any(path: &Path) {
	if path.is_dir() {
		fs::remove_dir_all(path).ok();
	} else {
		fs::remove_file(path).ok();
	}
}

/// Copies a file or a whole directory tree
fn copy_tree(source: &Path, target: &Path) -> Result<()> {
	if source.is_dir() {
		fs::create_dir_all(target)?;

		for entry in fs::read_dir(source)? {
			let entry = entry?;

			copy_tree(&entry.path(), &target.join(entry.file_name()))?;
		}

		return Ok(());
	}

	if let Some(parent) = target.parent() {
		fs::create_dir_all(parent)?;
	}

	fs::copy(source, target)?;

	Ok(())
}

// ---------------------------------------------------------------------------
// HTTP surface
// ---------------------------------------------------------------------------

use axum::{
	body::Bytes,
	extract::State,
	http::StatusCode,
	response::{IntoResponse, Response},
	Json,
};

use crate::server::{ws::frames::Event, AppState};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EntryRequest {
	pub id: Option<String>,
	/// `drop` only: forget everything instead of one entry
	pub all: Option<bool>,
}

fn error(status: StatusCode, message: &str) -> Response {
	(status, Json(json!({ "ok": false, "error": message }))).into_response()
}

/// `GET /backlog` — what is waiting, and how long each entry has left
pub async fn status(State(state): State<AppState>) -> Json<Value> {
	Json(state.backlog.to_json())
}

/// `POST /backlog/restore {id}` — put an entry back.
///
/// The file returns to its project path and the ordinary watcher carries it to
/// Studio from there: after a Studio-first apply the two sides agree, so a
/// restore is a plain one-sided disk edit and needs no special path to travel.
/// The VFS is deliberately **not** paused for that reason — the watcher seeing
/// this write is the mechanism, not an echo to suppress.
pub async fn restore(State(state): State<AppState>, body: Bytes) -> Response {
	let request: EntryRequest = match serde_json::from_slice(&body) {
		Ok(request) => request,
		Err(err) => return error(StatusCode::BAD_REQUEST, &format!("Malformed restore request: {err}")),
	};

	let Some(id) = request.id else {
		return error(StatusCode::BAD_REQUEST, "Pass the id of the entry to restore");
	};

	let Some(entry) = state.backlog.get(&id) else {
		return error(StatusCode::NOT_FOUND, "No such backlog entry (it may have expired)");
	};

	// Restoring is only half-done if it never reaches Studio, and without a
	// live sync channel the write would sit on disk alone until the next
	// connect — which is Studio-first, and would send it straight back here
	if state.core.queue().get_first_non_internal_listener_name().is_none() {
		return error(
			StatusCode::SERVICE_UNAVAILABLE,
			"No Studio plugin is connected; a restore needs a live sync channel",
		);
	}

	match state.backlog.restore(&id) {
		Ok(path) => {
			debug!("Restored {} from the backlog", entry.path);

			state.ws.emit(Event::Backlog {
				total: state.backlog.list().len(),
				added: 0,
			});

			Json(json!({
				"ok": true,
				"path": entry.path,
				"restoredTo": path.to_string_lossy(),
			}))
			.into_response()
		}
		Err(err) => error(StatusCode::INTERNAL_SERVER_ERROR, &format!("{err}")),
	}
}

/// `POST /backlog/drop {id}` / `{all: true}` — forget without restoring
pub async fn drop_entry(State(state): State<AppState>, body: Bytes) -> Response {
	let request: EntryRequest = match serde_json::from_slice(&body) {
		Ok(request) => request,
		Err(err) => return error(StatusCode::BAD_REQUEST, &format!("Malformed drop request: {err}")),
	};

	if request.all == Some(true) {
		let dropped = state.backlog.clear();

		state.ws.emit(Event::Backlog { total: 0, added: 0 });

		return Json(json!({ "ok": true, "dropped": dropped })).into_response();
	}

	let Some(id) = request.id else {
		return error(StatusCode::BAD_REQUEST, "Pass an entry id, or all: true");
	};

	if !state.backlog.forget(&id) {
		return error(StatusCode::NOT_FOUND, "No such backlog entry");
	}

	state.ws.emit(Event::Backlog {
		total: state.backlog.list().len(),
		added: 0,
	});

	Json(json!({ "ok": true, "dropped": 1 })).into_response()
}
