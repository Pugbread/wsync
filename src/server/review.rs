//! The Studio-first disk review (Design §7.0).
//!
//! When a code-scope comparison commits, the daemon applies Studio → disk
//! immediately (fenced, backed up) and leaves the disk-side entries behind as
//! a **passive, optional, non-blocking review**: the disk-only items that
//! stayed untouched on the live disk, and the preserved disk originals of
//! `differs` items. Nothing waits on it — live sync runs while it pends.
//!
//! Surface (all JSON, shapes pinned):
//! * `GET /review` → `{pending, reviewId?, stats?}`
//! * `GET /review/details?reviewId=&cursor=&limit=` — paged like
//!   `/choice/details`
//! * `POST /review/push` `{reviewId, mode: "all"} | {reviewId, ids: […]}` —
//!   pushes entries back to Studio over the live sync channel (disk-only →
//!   created from the live files; `differs` → the preserved disk copy is
//!   pushed AND restored to the live disk), stamps baselines, removes the
//!   pushed entries; repeatable until the set is empty
//! * `POST /review/dismiss` `{reviewId}` — clears the set and deletes the
//!   preserved copies
//!
//! The review index and the preserved copies persist on disk (under
//! `.wsync-backups/review/`) so a daemon restart keeps a pending review
//! answerable. A new comparison replaces any pending review.

use axum::{
	body::Bytes,
	extract::{Query, State},
	http::StatusCode,
	response::{IntoResponse, Json, Response},
};
use log::{debug, trace, warn};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
	collections::BTreeSet,
	fs,
	path::{Component, Path, PathBuf},
	sync::Mutex,
};

use crate::{
	constants::{
		BACKUPS_DIR, CHOICE_DETAILS_BYTE_BUDGET, CHOICE_DETAILS_DEFAULT_LIMIT, CHOICE_DETAILS_MAX_LIMIT,
		SELECTION_BODY_MAX_BYTES, SELECTION_CHUNK_MAX_IDS,
	},
	core::{changes::Changes, conflict, processor::read, Core},
	lock,
	server::{self, ws::frames::Event, AppState},
};

/// Directory under `.wsync-backups/` holding the review index and the
/// preserved copies (`review/<reviewId>/<relpath>`)
pub const REVIEW_DIR: &str = "review";

/// The persisted review index inside [`REVIEW_DIR`]
const REVIEW_INDEX: &str = "index.json";

/// One remaining review entry. `path` is the workspace-relative disk path
/// (also the preserved-copy relpath for `differs`). Ids are assigned once,
/// when the review freezes, and are **stable for its lifetime** — surviving
/// entries keep their original ids after partial pushes (details pages then
/// carry strictly-increasing, no-longer-dense ids), so ids picked before a
/// chunked push never land on the wrong entry
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewItem {
	pub id: u32,
	pub path: String,
	pub instance_path: Option<String>,
	pub state: ReviewState,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub class: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReviewState {
	#[serde(rename = "disk-only")]
	DiskOnly,
	#[serde(rename = "differs")]
	Differs,
}

impl ReviewState {
	pub fn as_str(&self) -> &'static str {
		match self {
			Self::DiskOnly => "disk-only",
			Self::Differs => "differs",
		}
	}
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingReview {
	pub review_id: String,
	pub items: Vec<ReviewItem>,
}

impl PendingReview {
	fn stats(&self) -> (usize, usize, usize) {
		let disk_only = self
			.items
			.iter()
			.filter(|item| item.state == ReviewState::DiskOnly)
			.count();
		let differs = self.items.len() - disk_only;

		(self.items.len(), disk_only, differs)
	}
}

/// A workspace-relative review path must stay inside the workspace: relative,
/// with no parent traversal. Anything else is skipped (build) or refused
/// (load) — never joined onto the workspace
pub fn safe_rel_path(path: &str) -> bool {
	let path = Path::new(path);

	!path.as_os_str().is_empty()
		&& path.is_relative()
		&& path
			.components()
			.all(|component| matches!(component, Component::Normal(_)))
}

/// Server-held review state: at most one pending review, mirrored to disk so
/// a daemon restart keeps it answerable
pub struct ReviewStore {
	workspace_dir: PathBuf,
	inner: Mutex<Option<PendingReview>>,
}

impl ReviewStore {
	/// Loads any persisted pending review from the workspace
	pub fn load(workspace_dir: &Path) -> Self {
		let store = Self {
			workspace_dir: workspace_dir.to_owned(),
			inner: Mutex::new(None),
		};

		let index = store.review_root().join(REVIEW_INDEX);

		if let Ok(contents) = fs::read_to_string(&index) {
			match serde_json::from_str::<PendingReview>(&contents) {
				Ok(review) if review.items.iter().all(|item| safe_rel_path(&item.path)) => {
					debug!(
						"Restored pending disk review {} ({} item(s))",
						review.review_id,
						review.items.len()
					);
					*lock!(store.inner) = Some(review);
				}
				Ok(_) => warn!("Ignoring persisted disk review: it carries unsafe paths"),
				Err(err) => warn!("Ignoring unparseable persisted disk review: {err}"),
			}
		}

		store
	}

	fn review_root(&self) -> PathBuf {
		self.workspace_dir.join(BACKUPS_DIR).join(REVIEW_DIR)
	}

	/// Where the preserved disk original of a `differs` entry lives
	pub fn preserved_path(&self, review_id: &str, rel_path: &str) -> PathBuf {
		self.review_root().join(review_id).join(rel_path)
	}

	/// Drops any pending review and deletes its preserved copies. A new
	/// connect/compare replaces the pending review (Design §7.0)
	pub fn clear(&self) {
		let dropped = lock!(self.inner).take();

		if let Some(review) = dropped {
			debug!("Cleared pending disk review {}", review.review_id);
		}

		fs::remove_dir_all(self.review_root()).ok();
	}

	/// Installs a new pending review (replacing any old one) and persists it
	pub fn replace(&self, review: PendingReview) {
		self.persist(&review);
		*lock!(self.inner) = Some(review);
	}

	fn persist(&self, review: &PendingReview) {
		let root = self.review_root();

		if let Err(err) = fs::create_dir_all(&root) {
			warn!("Failed to create the review directory: {err}");
			return;
		}

		match serde_json::to_string(review) {
			Ok(contents) => {
				if let Err(err) = fs::write(root.join(REVIEW_INDEX), contents) {
					warn!("Failed to persist the disk review index: {err}");
				}
			}
			Err(err) => warn!("Failed to serialize the disk review index: {err}"),
		}
	}

	/// Deletes the preserved copies of one review id without touching the
	/// pending state (failed auto-applies discard their partial preservation)
	pub fn discard_preserved(&self, review_id: &str) {
		fs::remove_dir_all(self.review_root().join(review_id)).ok();
	}

	/// Removes the disk-only entries of `review` from the workspace, moving
	/// each into `<backups>/dismissed-<reviewId>/` so the choice is
	/// recoverable. Returns how many were moved.
	///
	/// This is what makes "keep Studio's versions everywhere" true for
	/// disk-only files: they exist nowhere in Studio, so leaving them behind
	/// would keep disk permanently ahead of the place with nothing left to
	/// reconcile it.
	pub fn discard_disk_only(&self, review: &PendingReview) -> usize {
		let graveyard = self
			.workspace_dir
			.join(BACKUPS_DIR)
			.join(format!("dismissed-{}", review.review_id));
		let mut moved = 0;

		for item in &review.items {
			if item.state != ReviewState::DiskOnly || !safe_rel_path(&item.path) {
				continue;
			}

			let live = self.workspace_dir.join(&item.path);

			if !live.exists() {
				continue;
			}

			let target = graveyard.join(&item.path);

			if let Some(parent) = target.parent() {
				if let Err(err) = fs::create_dir_all(parent) {
					warn!("Failed to prepare the dismissed-file backup for {}: {err}", item.path);
					continue;
				}
			}

			match fs::rename(&live, &target) {
				Ok(()) => moved += 1,
				Err(err) => warn!("Failed to discard the disk-only file {}: {err}", item.path),
			}
		}

		if moved > 0 {
			debug!("Discarded {moved} disk-only file(s) for review {}", review.review_id);
		}

		moved
	}

	pub fn pending(&self) -> Option<PendingReview> {
		lock!(self.inner).clone()
	}

	/// Removes the given item ids from the pending review, re-persisting or
	/// clearing it, and returns how many items remain
	fn complete_push(&self, review_id: &str, pushed: &BTreeSet<u32>) -> usize {
		let mut inner = lock!(self.inner);

		let Some(review) = inner.as_mut() else {
			return 0;
		};

		if review.review_id != review_id {
			return review.items.len();
		}

		review.items.retain(|item| !pushed.contains(&item.id));

		let remaining = review.items.len();

		if remaining == 0 {
			let finished = inner.take();
			drop(inner);

			if let Some(finished) = finished {
				debug!("Disk review {} fully pushed", finished.review_id);
			}

			// Every preserved copy was consumed (or belongs to nothing now)
			fs::remove_dir_all(self.review_root()).ok();
		} else {
			let snapshot = review.clone();
			drop(inner);

			self.persist(&snapshot);
		}

		remaining
	}
}

fn error(status: StatusCode, message: &str) -> Response {
	(status, Json(json!({ "ok": false, "error": message }))).into_response()
}

// GET /review

/// `GET /review` — the pending disk-review status (Design §7.0)
pub async fn status(State(state): State<AppState>) -> Json<Value> {
	trace!("Received request: review (get)");

	match state.review.pending() {
		Some(review) => {
			let (total, disk_only, differs) = review.stats();

			Json(json!({
				"pending": true,
				"reviewId": review.review_id,
				"stats": { "total": total, "diskOnly": disk_only, "differs": differs },
			}))
		}
		None => Json(json!({ "pending": false })),
	}
}

// GET /review/details

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct DetailsParams {
	review_id: Option<String>,
	cursor: Option<u32>,
	limit: Option<usize>,
}

/// `GET /review/details?reviewId=&cursor=&limit=` — pages the pending review
/// items with the same discipline as `/choice/details` (≤1024 records /
/// ≤512 KiB per page). The cursor is an item id: entries pushed away between
/// pages are simply skipped
pub async fn details(State(state): State<AppState>, Query(params): Query<DetailsParams>) -> Response {
	trace!("Received request: review details");

	let Some(review) = state.review.pending() else {
		return error(StatusCode::NOT_FOUND, "No pending disk review");
	};

	if params.review_id.as_deref() != Some(review.review_id.as_str()) {
		return error(StatusCode::NOT_FOUND, "Unknown or stale reviewId");
	}

	let cursor = params.cursor.unwrap_or(0);
	let limit = params
		.limit
		.unwrap_or(CHOICE_DETAILS_DEFAULT_LIMIT)
		.clamp(1, CHOICE_DETAILS_MAX_LIMIT);

	let mut items = Vec::new();
	let mut bytes = 0usize;
	let mut next_cursor = None;

	for item in review.items.iter().filter(|item| item.id >= cursor) {
		if items.len() >= limit {
			next_cursor = Some(item.id);
			break;
		}

		let mut value = json!({
			"id": item.id,
			"path": item.path,
			"instancePath": item.instance_path,
			"state": item.state.as_str(),
		});

		// `class` is optional in the pinned item shape: omitted, never null
		if let Some(class) = &item.class {
			value["class"] = json!(class);
		}

		// Stay under the page byte budget; every page carries at least one
		// item so paging terminates
		let size = value.to_string().len() + 1;

		if !items.is_empty() && bytes + size > CHOICE_DETAILS_BYTE_BUDGET {
			next_cursor = Some(item.id);
			break;
		}

		bytes += size;
		items.push(value);
	}

	let mut response = json!({
		"reviewId": review.review_id,
		"items": items,
		"totalCount": review.items.len(),
	});

	if let Some(next_cursor) = next_cursor {
		response["nextCursor"] = json!(next_cursor);
	}

	Json(response).into_response()
}

// POST /review/push

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct PushRequest {
	review_id: Option<String>,
	mode: Option<String>,
	ids: Option<Vec<u32>>,
}

/// `POST /review/push` `{reviewId, mode: "all"} | {reviewId, ids: […]}` —
/// pushes the selected entries back to Studio over the live sync channel:
/// disk-only entries are created from the live files; `differs` entries have
/// their preserved disk copy restored to the live disk and pushed. Baselines
/// stamp, pushed entries leave the set, and the call is repeatable until the
/// set is empty
pub async fn push(State(state): State<AppState>, body: Bytes) -> Response {
	trace!("Received request: review push");

	if body.len() > SELECTION_BODY_MAX_BYTES {
		return error(StatusCode::PAYLOAD_TOO_LARGE, "Push request exceeds 64 KiB");
	}

	let request: PushRequest = match serde_json::from_slice(&body) {
		Ok(request) => request,
		Err(err) => return error(StatusCode::BAD_REQUEST, &format!("Malformed push request: {err}")),
	};

	let Some(review) = state.review.pending() else {
		return error(StatusCode::NOT_FOUND, "No pending disk review");
	};

	if request.review_id.as_deref() != Some(review.review_id.as_str()) {
		return error(StatusCode::NOT_FOUND, "Unknown or stale reviewId");
	}

	let selected: Vec<ReviewItem> = match (request.mode.as_deref(), request.ids) {
		(Some("all"), None) => review.items.clone(),
		(None, Some(ids)) => {
			if ids.len() > SELECTION_CHUNK_MAX_IDS {
				return error(
					StatusCode::BAD_REQUEST,
					&format!("Push request exceeds {SELECTION_CHUNK_MAX_IDS} ids"),
				);
			}

			let wanted: BTreeSet<u32> = ids.into_iter().collect();

			if let Some(unknown) = wanted
				.iter()
				.find(|id| !review.items.iter().any(|item| item.id == **id))
			{
				return error(
					StatusCode::BAD_REQUEST,
					&format!("Push id {unknown} is not part of the pending review"),
				);
			}

			review
				.items
				.iter()
				.filter(|item| wanted.contains(&item.id))
				.cloned()
				.collect()
		}
		_ => {
			return error(
				StatusCode::BAD_REQUEST,
				"Pass exactly one of mode: \"all\" or ids: [\u{2026}]",
			)
		}
	};

	// The push travels the live sync channel; without any plugin transport
	// subscribed it would vanish into the unsynced-changes counter — refuse
	// honestly instead (the call is repeatable once Studio reconnects)
	if state.core.queue().get_first_non_internal_listener_name().is_none() {
		return error(
			StatusCode::SERVICE_UNAVAILABLE,
			"No Studio plugin is connected; the push needs a live sync channel",
		);
	}

	match push_entries(&state.core, &state.review, &review.review_id, &selected) {
		Ok(work) => {
			if !work.changes.is_empty() {
				if let Err(err) = state.core.queue().push(server::SyncChanges(work.changes), None) {
					// The preserved copies were NOT deleted yet, so the push
					// stays repeatable after a queue failure
					return error(
						StatusCode::INTERNAL_SERVER_ERROR,
						&format!("Failed to queue sync frames: {err}"),
					);
				}
			}

			// The frames are on their way: only now are the consumed
			// preserved copies deleted and the entries removed from the set
			for preserved in &work.consumed_preserved {
				fs::remove_file(preserved).ok();
			}

			let pushed = work.pushed.len();
			let remaining = state.review.complete_push(&review.review_id, &work.pushed);

			// The plugin's indicator line follows these events (its poll only
			// runs at reconnect) — without this, a review resolved here reads
			// as pending in Studio until the next reconnect
			emit_review_total(&state, &review.review_id);

			Json(json!({ "ok": true, "pushed": pushed, "remaining": remaining })).into_response()
		}
		Err(message) => error(StatusCode::INTERNAL_SERVER_ERROR, &message),
	}
}

/// The outcome of assembling one push: the replay change set, the item ids
/// leaving the review, and the preserved copies to delete once the frames
/// are queued (never before — a failed queue push must stay repeatable)
struct PushWork {
	changes: Changes,
	pushed: BTreeSet<u32>,
	consumed_preserved: Vec<PathBuf>,
}

/// Builds the disk → Studio replay for the selected entries and restores
/// `differs` entries to the live disk. Entries whose disk backing vanished
/// leave the review too — there is nothing left to review
fn push_entries(
	core: &Core,
	store: &ReviewStore,
	review_id: &str,
	selected: &[ReviewItem],
) -> Result<PushWork, String> {
	let workspace_dir = core.project().workspace_dir.clone();
	let vfs = core.vfs();

	let mut changes = Changes::new();
	let mut pushed: BTreeSet<u32> = BTreeSet::new();
	let mut consumed_preserved: Vec<PathBuf> = Vec::new();
	let mut additions: Vec<(u32, rbx_dom_weak::types::Ref)> = Vec::new();
	let mut stamped: Vec<rbx_dom_weak::types::Ref> = Vec::new();

	// Restores and re-reads happen under one VFS pause so the watcher never
	// echoes the daemon's own writes back through the sync pipeline
	vfs.pause();

	let restored = (|| -> Result<(), String> {
		let mut tree = core.tree();

		for item in selected {
			if !safe_rel_path(&item.path) {
				debug!("Skipping review entry {}: unsafe path", item.id);
				pushed.insert(item.id);
				continue;
			}

			let live_path = workspace_dir.join(&item.path);

			match item.state {
				ReviewState::DiskOnly => {
					// Collected under the tree lock, snapshotted after it is
					// released (`Core::snapshot` takes the lock itself)
					match tree.get_ids(&live_path).and_then(|ids| ids.first().copied()) {
						Some(id) => additions.push((item.id, id)),
						None => {
							debug!(
								"Review entry {} ({}) is gone from the tree; dropping it",
								item.id, item.path
							);
							pushed.insert(item.id);
						}
					}
				}
				ReviewState::Differs => {
					let preserved = store.preserved_path(review_id, &item.path);

					let bytes = match fs::read(&preserved) {
						Ok(bytes) => bytes,
						Err(err) => {
							debug!(
								"Review entry {} ({}) has no preserved copy ({err}); dropping it",
								item.id, item.path
							);
							pushed.insert(item.id);
							continue;
						}
					};

					// Restore the preserved disk original to the live disk…
					fs::write(&live_path, &bytes)
						.map_err(|err| format!("failed to restore {} to the live disk: {err}", item.path))?;

					// …and re-read it through the middleware so the tree and
					// the outgoing sync frames carry exactly that content
					let ids = tree.get_ids(&live_path).map(|ids| ids.to_owned()).unwrap_or_default();

					if ids.is_empty() {
						debug!(
							"Review entry {} ({}) resolves to no instance; restored on disk only",
							item.id, item.path
						);
						pushed.insert(item.id);
						consumed_preserved.push(preserved);
						continue;
					}

					for id in ids {
						if let Some(processed) = read::process_changes(id, &mut tree, &vfs) {
							changes.extend(processed);
						}

						stamped.push(id);
					}

					pushed.insert(item.id);
					consumed_preserved.push(preserved);
				}
			}
		}

		Ok(())
	})();

	vfs.resume();
	restored?;

	// Disk-only subtrees snapshot whole (additions carry their children)
	for (item_id, id) in additions {
		match core.snapshot(id) {
			Some(snapshot) => {
				changes.additions.push(snapshot);
				stamped.push(id);
				pushed.insert(item_id);
			}
			None => {
				debug!("Review entry {item_id} vanished before snapshotting; dropping it");
				pushed.insert(item_id);
			}
		}
	}

	// Both sides converge on the pushed content: stamp every touched subtree
	{
		let tree = core.tree();
		let engine = core.conflicts();

		for root in stamped {
			for id in conflict::subtree_refs(&tree, root) {
				if let Some(instance) = tree.get_instance(id) {
					let content = conflict::ContentState::new(&instance.name, &instance.class, &instance.properties);

					engine.stamp(id, content.hash);
				}
			}
		}
	}

	Ok(PushWork {
		changes,
		pushed,
		consumed_preserved,
	})
}

// POST /review/dismiss

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct DismissRequest {
	review_id: Option<String>,
}

/// `POST /review/dismiss` `{reviewId}` — clears the pending review and
/// deletes the preserved copies. Studio's version stays where the auto-apply
/// put it
pub async fn dismiss(State(state): State<AppState>, body: Bytes) -> Response {
	trace!("Received request: review dismiss");

	let request: DismissRequest = match serde_json::from_slice(&body) {
		Ok(request) => request,
		Err(err) => return error(StatusCode::BAD_REQUEST, &format!("Malformed dismiss request: {err}")),
	};

	let Some(review) = state.review.pending() else {
		return error(StatusCode::NOT_FOUND, "No pending disk review");
	};

	if request.review_id.as_deref() != Some(review.review_id.as_str()) {
		return error(StatusCode::NOT_FOUND, "Unknown or stale reviewId");
	}

	// Skip means what the prompt offers: keep Studio's versions everywhere.
	// The Studio-first apply deliberately carries disk-only files forward so
	// this choice can still go either way, so dropping the review without
	// removing them stranded them on disk — present, absent from Studio, and
	// with no review left to reconcile them. `differs` entries already hold
	// Studio's content on disk (the apply wrote it) and need nothing here.
	//
	// They move under the backups directory rather than being unlinked, so a
	// mis-click is recoverable.
	// The VFS is paused across the move for the same reason the fenced apply
	// pauses it: these files leaving disk is not an edit to broadcast. They are
	// disk-only by definition — Studio never had them — so letting the watcher
	// turn each removal into a sync message only asks Studio to delete
	// instances it does not have, which it answers with a `NoInstanceRemove`
	// warning apiece.
	let vfs = state.core.vfs();

	vfs.pause();

	let discarded = state.review.discard_disk_only(&review);

	vfs.resume();

	state.review.clear();

	// Same reason as the push emit: Studio's indicator must hear the clear
	emit_review_total(&state, &review.review_id);

	Json(json!({ "ok": true, "discarded": discarded })).into_response()
}

/// Broadcast the review's current totals after a mutation, so the plugin's
/// Connected-page line tracks resolution in the app live instead of waiting
/// for its reconnect-time poll. A cleared review is `total: 0`, which is the
/// frame that clears the indicator.
fn emit_review_total(state: &AppState, review_id: &str) {
	let (total, disk_only, differs) = match state.review.pending() {
		Some(review) => {
			let disk_only = review
				.items
				.iter()
				.filter(|item| item.state == ReviewState::DiskOnly)
				.count();
			(review.items.len(), disk_only, review.items.len() - disk_only)
		}
		None => (0, 0, 0),
	};

	state.ws.emit(Event::DiskReview {
		review_id: review_id.to_owned(),
		total,
		disk_only,
		differs,
	});
}
