//! Divergence sets (Design §7): the chunked `POST /compare` upload, the
//! frozen immutable set behind a `choiceId`, paged details, chunked
//! selective staging, and the decision endpoint whose disk direction applies
//! immediately over the live sync channel. All request/response shapes and
//! bounds are pinned protocol contracts.
//!
//! Under the Studio-first ruling (Design §7.0) the choice flow only exists on
//! `"scope": "full"` projects. On a code-scope project a committed non-empty
//! comparison never broadcasts `choice-needed`: the daemon immediately runs
//! the fenced Studio → disk apply — `differs` gets Studio content with the
//! disk original preserved, Studio-only entries are written out, disk-only
//! files stay untouched on the live disk — and leaves the disk-side entries
//! behind as the passive `disk-review` set.

use axum::{
	body::Bytes,
	extract::{Query, State},
	http::StatusCode,
	response::{IntoResponse, Json, Response},
};
use log::{debug, info, trace};
use rbx_dom_weak::{types::Ref, types::Variant, ustr};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
	collections::{BTreeSet, HashMap},
	sync::Mutex,
};
use uuid::Uuid;

use crate::{
	constants::{
		CHOICE_DETAILS_BYTE_BUDGET, CHOICE_DETAILS_DEFAULT_LIMIT, CHOICE_DETAILS_MAX_LIMIT, COMPARE_BODY_MAX_BYTES,
		COMPARE_CHUNK_MAX_ENTRIES, CONFLICT_SOURCE_CAP, REQUEST_DEFAULT_TIMEOUT_MS, SELECTION_BODY_MAX_BYTES,
		SELECTION_CHUNK_MAX_IDS,
	},
	core::{
		changes::Changes,
		conflict::{self, ContentState},
		snapshot::UpdatedSnapshot,
		Core,
	},
	lock,
	project::Scope,
	server::{
		self, bulk,
		review::{self, PendingReview, ReviewItem, ReviewState},
		ws::frames::codes,
		ws::frames::Event,
		AppState,
	},
	util,
};

/// Divergence-entry classification (Design §7.2): `add` = disk has it,
/// Studio lacks it; `update` = differs; `remove` = Studio-only
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryChange {
	Add,
	Update,
	Remove,
}

impl EntryChange {
	fn parse(change: &str) -> Option<Self> {
		match change {
			"add" => Some(Self::Add),
			"update" => Some(Self::Update),
			"remove" => Some(Self::Remove),
			_ => None,
		}
	}

	fn state(&self) -> &'static str {
		match self {
			Self::Add => "only-on-disk",
			Self::Update => "differs",
			Self::Remove => "missing-on-disk",
		}
	}
}

#[derive(Debug, Clone)]
struct Entry {
	instance: Ref,
	change: EntryChange,
	class: String,
	instance_path: String,
}

/// One frozen divergence item with its dense sequential id
#[derive(Debug, Clone)]
struct Item {
	id: u32,
	instance: Ref,
	path: Option<String>,
	instance_path: String,
	state: &'static str,
	class: String,
}

#[derive(Debug, Clone, Copy)]
struct Stats {
	total: usize,
	studio_count: usize,
	disk_count: usize,
	only_on_disk: usize,
	differs: usize,
	missing_on_disk: usize,
}

impl Stats {
	fn json(&self) -> Value {
		json!({
			"total": self.total,
			"studioCount": self.studio_count,
			"diskCount": self.disk_count,
			"onlyOnDisk": self.only_on_disk,
			"differs": self.differs,
			"missingOnDisk": self.missing_on_disk,
		})
	}
}

#[derive(Debug)]
struct Upload {
	submission_id: String,
	next_chunk: u64,
	entries: Vec<Entry>,
}

#[derive(Debug)]
struct Selection {
	submission_id: String,
	next_chunk: u64,
	ids: BTreeSet<u32>,
	committed: bool,
}

#[derive(Debug)]
struct DivergenceSet {
	choice_id: String,
	items: Vec<Item>,
	stats: Stats,
	selection: Option<Selection>,
}

#[derive(Default)]
struct Inner {
	upload: Option<Upload>,
	set: Option<DivergenceSet>,
}

/// Server-held divergence state: at most one upload in progress and one
/// frozen pending set at a time
#[derive(Default)]
pub struct Divergence {
	inner: Mutex<Inner>,
}

impl Divergence {
	pub fn new() -> Self {
		Self::default()
	}

	/// Drops the in-progress upload and any pending set (stale-set rule:
	/// `restart` uploads and plugin disconnects both land here)
	pub fn invalidate(&self, reason: &str) {
		let mut inner = lock!(self.inner);

		if inner.upload.take().is_some() || inner.set.take().is_some() {
			info!("Invalidated pending divergence state: {reason}");
		}
	}
}

fn error(status: StatusCode, message: &str) -> Response {
	(status, Json(json!({ "ok": false, "error": message }))).into_response()
}

// POST /compare

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct CompareRequest {
	submission_id: String,
	chunk_index: u64,
	final_chunk: bool,
	#[serde(default)]
	restart: bool,
	entries: Vec<CompareEntry>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct CompareEntry {
	#[serde(rename = "ref")]
	instance: String,
	change: String,
	class: String,
	#[allow(dead_code)]
	name: String,
	instance_path: String,
}

/// What a `/compare` chunk resolved to while the divergence lock was held.
/// The Studio-first apply awaits plugin ops, so it runs lock-free afterwards
/// (the guard is `!Send` and must never live across an await)
enum Ingested {
	/// Fully answered synchronously (receipts, errors, full-scope commits)
	Done(Response),
	/// A code-scope commit froze `items`; the auto-apply runs next
	StudioFirst { items: Vec<Item>, accepted: u64, next: u64 },
}

/// `POST /compare` — the plugin's chunked divergence upload (Design §7.2).
/// Strict receipts: only `chunkIndex == nextChunk` is accepted, anything
/// else is a 409. Committing the final chunk freezes an immutable set: on a
/// `"full"`-scope project it mints a `choiceId` and broadcasts
/// `choice-needed`; on a code-scope project the Studio-first apply runs
/// immediately instead (Design §7.0) — no choice, no prompt
pub async fn compare(State(state): State<AppState>, body: Bytes) -> Response {
	trace!("Received request: compare");

	if body.len() > COMPARE_BODY_MAX_BYTES {
		return error(StatusCode::PAYLOAD_TOO_LARGE, "Compare chunk exceeds 512 KiB");
	}

	let request: CompareRequest = match serde_json::from_slice(&body) {
		Ok(request) => request,
		Err(err) => return error(StatusCode::BAD_REQUEST, &format!("Malformed compare request: {err}")),
	};

	if request.entries.len() > COMPARE_CHUNK_MAX_ENTRIES {
		return error(
			StatusCode::BAD_REQUEST,
			&format!("Compare chunk exceeds {COMPARE_CHUNK_MAX_ENTRIES} entries"),
		);
	}

	let scope = state.core.project().scope();

	let mut entries = Vec::with_capacity(request.entries.len());

	for (index, entry) in request.entries.iter().enumerate() {
		let instance = match util::parse_ref(&entry.instance) {
			Ok(instance) => instance,
			Err(err) => return error(StatusCode::BAD_REQUEST, &format!("entries[{index}].ref: {err}")),
		};

		let Some(change) = EntryChange::parse(&entry.change) else {
			return error(
				StatusCode::BAD_REQUEST,
				&format!("entries[{index}].change must be add, update or remove"),
			);
		};

		// Code scope (Design §7.0): entries outside the class allowlist are
		// dropped server-side — those instances are Studio-authoritative and
		// never part of the projection. A drop, never an error
		if !scope.allows_class(&entry.class) {
			debug!(
				"Dropping compare entry {} ({}): class is outside the code-scope projection",
				entry.instance_path, entry.class
			);
			continue;
		}

		entries.push(Entry {
			instance,
			change,
			class: entry.class.clone(),
			instance_path: entry.instance_path.clone(),
		});
	}

	match ingest_chunk(&state, &request, entries, scope) {
		Ingested::Done(response) => response,
		Ingested::StudioFirst { items, accepted, next } => studio_first_commit(&state, items, accepted, next).await,
	}
}

/// Runs the chunk-receipt state machine under the divergence lock and, on the
/// final chunk, freezes the set: full scope commits synchronously (choice
/// flow), code scope hands the frozen items back for the Studio-first apply
fn ingest_chunk(state: &AppState, request: &CompareRequest, entries: Vec<Entry>, scope: Scope) -> Ingested {
	let divergence = state.divergence.clone();
	let mut inner = lock!(divergence.inner);

	if request.restart {
		if inner.upload.take().is_some() {
			debug!("Compare restart: dropped in-progress upload");
		}

		if inner.set.take().is_some() {
			info!("Compare restart: invalidated the pending divergence set");
		}
	}

	match &mut inner.upload {
		None => {
			if request.chunk_index != 0 {
				return Ingested::Done(error(
					StatusCode::CONFLICT,
					"No upload in progress; the first chunk must have chunkIndex 0",
				));
			}

			inner.upload = Some(Upload {
				submission_id: request.submission_id.clone(),
				next_chunk: 0,
				entries: Vec::new(),
			});
		}
		Some(upload) => {
			if upload.submission_id != request.submission_id {
				return Ingested::Done(error(
					StatusCode::CONFLICT,
					"Another compare submission is in progress; send restart to replace it",
				));
			}

			if request.chunk_index != upload.next_chunk {
				return Ingested::Done(error(
					StatusCode::CONFLICT,
					&format!(
						"Receipt mismatch: expected chunkIndex {}, got {}",
						upload.next_chunk, request.chunk_index
					),
				));
			}
		}
	}

	let upload = inner.upload.as_mut().unwrap();

	upload.entries.extend(entries);
	upload.next_chunk += 1;

	let accepted = request.chunk_index;
	let next = upload.next_chunk;

	if !request.final_chunk {
		return Ingested::Done(
			Json(json!({
				"ok": true,
				"acceptedChunk": accepted,
				"nextChunk": next,
				"committed": false,
			}))
			.into_response(),
		);
	}

	// Final chunk: freeze the immutable set
	let upload = inner.upload.take().unwrap();

	if upload.entries.is_empty() {
		// Both sides agree — no choice needed. This is the moment agreement
		// is proven for the whole projection, so every baseline stamps
		drop(inner);

		// A new comparison replaces any pending review (Design §7.0); a
		// clean commit leaves nothing to review
		if scope.is_code() {
			state.review.clear();
		}

		seed_all_baselines(&state.core);

		info!("Divergence comparison committed clean: sides are in sync");

		return Ingested::Done(
			Json(json!({
				"ok": true,
				"acceptedChunk": accepted,
				"nextChunk": next,
				"committed": true,
			}))
			.into_response(),
		);
	}

	let (items, stats) = freeze(&state.core, upload.entries);

	if scope.is_code() {
		// Studio-first (Design §7.0): no choice, no prompt — the caller runs
		// the apply outside the divergence lock (it awaits plugin ops)
		return Ingested::StudioFirst { items, accepted, next };
	}

	let choice_id = Uuid::new_v4().to_string();

	info!(
		"Divergence set {choice_id} frozen: {} entries ({} only-on-disk, {} differ, {} missing-on-disk)",
		stats.total, stats.only_on_disk, stats.differs, stats.missing_on_disk
	);

	inner.set = Some(DivergenceSet {
		choice_id: choice_id.clone(),
		items,
		stats,
		selection: None,
	});

	drop(inner);

	state.ws.emit(Event::ChoiceNeeded {
		choice_id: choice_id.clone(),
		total: stats.total,
		studio_count: stats.studio_count,
		disk_count: stats.disk_count,
		only_on_disk: stats.only_on_disk,
		differs: stats.differs,
		missing_on_disk: stats.missing_on_disk,
	});

	Ingested::Done(
		Json(json!({
			"ok": true,
			"acceptedChunk": accepted,
			"nextChunk": next,
			"committed": true,
			"choiceId": choice_id,
		}))
		.into_response(),
	)
}

/// Commits a code-scope comparison Studio-first (Design §7.0): runs the
/// fenced Studio → disk apply immediately — no `choice-needed`, no prompt,
/// no `changes_threshold` — and leaves the disk-side entries behind as the
/// passive `disk-review` set. Deltas: `differs` → Studio content lands on
/// disk with the disk original preserved into the review store; disk-only →
/// left untouched on the live disk (the staged swap overlays them forward);
/// Studio-only → written to disk as part of the apply
async fn studio_first_commit(state: &AppState, items: Vec<Item>, accepted: u64, next: u64) -> Response {
	let review_id = Uuid::new_v4().to_string();

	// A new connect/compare replaces any pending review
	state.review.clear();

	// Disk-side entries become the review candidates; `differs` disk
	// originals are preserved under the fence during the apply
	let workspace_dir = state.core.project().workspace_dir.clone();
	let mut review_items = Vec::new();
	let mut preserve = HashMap::new();

	for item in &items {
		let state_kind = match item.state {
			"only-on-disk" => ReviewState::DiskOnly,
			"differs" => ReviewState::Differs,
			_ => continue,
		};

		let Some(path) = item.path.clone().filter(|path| review::safe_rel_path(path)) else {
			debug!(
				"Skipping review candidate {}: no safe disk path behind it",
				item.instance_path
			);
			continue;
		};

		if state_kind == ReviewState::Differs {
			preserve.insert(
				workspace_dir.join(&path),
				state.review.preserved_path(&review_id, &path),
			);
		}

		review_items.push(ReviewItem {
			id: review_items.len() as u32,
			path,
			instance_path: Some(item.instance_path.clone()),
			state: state_kind,
			class: Some(item.class.clone()),
		});
	}

	// Roots carrying Studio-side content (differs / missing-on-disk) are
	// pulled and swapped whole; roots with only disk-only entries have
	// nothing to pull — their files stay untouched on the live disk
	let staged_roots: BTreeSet<String> = items
		.iter()
		.filter(|item| item.state != "only-on-disk")
		.filter_map(|item| item.instance_path.split('/').next())
		.filter(|root| !root.is_empty())
		.map(str::to_owned)
		.collect();

	info!(
		"Studio-first apply for review {review_id}: {} staged root(s), {} review candidate(s)",
		staged_roots.len(),
		review_items.len()
	);

	let options = bulk::StudioFirstOptions { preserve };

	if let Err((status, mut payload)) = bulk::apply_roots(state, &review_id, &staged_roots, Some(&options)).await {
		// A failed apply leaves no pending review; the partial preservation
		// is discarded (the transfer backups carry the recovery state)
		state.review.discard_preserved(&review_id);

		payload["acceptedChunk"] = json!(accepted);
		payload["nextChunk"] = json!(next);
		payload["committed"] = json!(true);

		return (status, Json(payload)).into_response();
	}

	// Baselines stamp as today: the tree now holds exactly the applied state
	// (agreed content + Studio content + carried-forward disk-only files)
	seed_all_baselines(&state.core);

	if review_items.is_empty() {
		// Nothing stayed disk-side: done silently, no event (Design §7.0)
		return Json(json!({
			"ok": true,
			"acceptedChunk": accepted,
			"nextChunk": next,
			"committed": true,
			"applied": true,
		}))
		.into_response();
	}

	let total = review_items.len();
	let disk_only = review_items
		.iter()
		.filter(|item| item.state == ReviewState::DiskOnly)
		.count();
	let differs = total - disk_only;

	info!("Disk review {review_id} pending: {total} entr(ies) ({disk_only} disk-only, {differs} differ)");

	state.review.replace(PendingReview {
		review_id: review_id.clone(),
		items: review_items,
	});

	state.ws.emit(Event::DiskReview {
		review_id: review_id.clone(),
		total,
		disk_only,
		differs,
	});

	Json(json!({
		"ok": true,
		"acceptedChunk": accepted,
		"nextChunk": next,
		"committed": true,
		"applied": true,
		"reviewId": review_id,
	}))
	.into_response()
}

/// Classifies entries, projects disk paths through the tree/`Meta`, assigns
/// dense sequential ids and computes the aggregate stats
fn freeze(core: &Core, entries: Vec<Entry>) -> (Vec<Item>, Stats) {
	let workspace_dir = core.project().workspace_dir.clone();
	let tree = core.tree();

	let mut items = Vec::with_capacity(entries.len());
	let mut only_on_disk = 0;
	let mut differs = 0;
	let mut missing_on_disk = 0;

	for (index, entry) in entries.into_iter().enumerate() {
		match entry.change {
			EntryChange::Add => only_on_disk += 1,
			EntryChange::Update => differs += 1,
			EntryChange::Remove => missing_on_disk += 1,
		}

		items.push(Item {
			id: index as u32,
			instance: entry.instance,
			// Studio-only entries have no disk backing: path stays null
			path: conflict::content::relative_path(&tree, entry.instance, &workspace_dir),
			instance_path: entry.instance_path,
			state: entry.change.state(),
			class: entry.class,
		});
	}

	let stats = Stats {
		total: items.len(),
		// Per-side involvement (Design §7.3 step 1): the Studio card counts
		// entries that exist in Studio, the disk card entries on disk
		studio_count: differs + missing_on_disk,
		disk_count: differs + only_on_disk,
		only_on_disk,
		differs,
		missing_on_disk,
	};

	(items, stats)
}

/// Stamps a baseline for every instance in the live tree — called when a
/// comparison commits clean, the one moment whole-projection agreement is
/// proven (Design §6.3/§7.4)
fn seed_all_baselines(core: &Core) {
	let engine = core.conflicts();
	let tree = core.tree();

	for id in conflict::subtree_refs(&tree, tree.root_ref()) {
		if let Some(instance) = tree.get_instance(id) {
			let content = ContentState::new(&instance.name, &instance.class, &instance.properties);

			engine.stamp(id, content.hash);
		}
	}
}

// GET /choice

/// `GET /choice` — pending divergence decision status
pub async fn choice_get(State(state): State<AppState>) -> Json<Value> {
	trace!("Received request: choice (get)");

	let inner = lock!(state.divergence.inner);

	match &inner.set {
		Some(set) => Json(json!({
			"pending": true,
			"choiceId": set.choice_id,
			"stats": set.stats.json(),
		})),
		None => Json(json!({ "pending": false })),
	}
}

// GET /choice/details

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct DetailsParams {
	choice_id: Option<String>,
	cursor: Option<u32>,
	limit: Option<usize>,
}

/// `GET /choice/details?choiceId=&cursor=&limit=` — pages the immutable
/// divergence detail list (≤1024 records / ≤512 KiB per page)
pub async fn choice_details(State(state): State<AppState>, Query(params): Query<DetailsParams>) -> Response {
	trace!("Received request: choice details");

	let inner = lock!(state.divergence.inner);

	let Some(set) = inner.set.as_ref() else {
		return error(StatusCode::NOT_FOUND, "No pending divergence set");
	};

	if params.choice_id.as_deref() != Some(set.choice_id.as_str()) {
		return error(StatusCode::NOT_FOUND, "Unknown or stale choiceId");
	}

	let cursor = params.cursor.unwrap_or(0) as usize;
	let limit = params
		.limit
		.unwrap_or(CHOICE_DETAILS_DEFAULT_LIMIT)
		.clamp(1, CHOICE_DETAILS_MAX_LIMIT);

	let mut items = Vec::new();
	let mut bytes = 0usize;
	let mut next_cursor = None;

	for item in set.items.iter().skip(cursor) {
		if items.len() >= limit {
			next_cursor = Some(item.id);
			break;
		}

		let value = json!({
			"id": item.id,
			"path": item.path,
			"instancePath": item.instance_path,
			"state": item.state,
			"class": item.class,
		});

		// Stay under the page byte budget even when individual records are
		// large; every page carries at least one item so paging terminates
		let size = value.to_string().len() + 1;

		if !items.is_empty() && bytes + size > CHOICE_DETAILS_BYTE_BUDGET {
			next_cursor = Some(item.id);
			break;
		}

		bytes += size;
		items.push(value);
	}

	let mut response = json!({
		"choiceId": set.choice_id,
		"items": items,
		"totalCount": set.items.len(),
	});

	if let Some(next_cursor) = next_cursor {
		response["nextCursor"] = json!(next_cursor);
	}

	Json(response).into_response()
}

// POST /choice/selection

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct SelectionRequest {
	choice_id: String,
	submission_id: String,
	chunk_index: u64,
	final_chunk: bool,
	#[serde(default)]
	restart: bool,
	ids: Vec<u32>,
}

/// `POST /choice/selection` — chunked selective-pull id submission with
/// verified receipts (Design §7.3). A committed selection equal to the full
/// set is exactly `mode: "all"`
pub async fn choice_selection(State(state): State<AppState>, body: Bytes) -> Response {
	trace!("Received request: choice selection");

	if body.len() > SELECTION_BODY_MAX_BYTES {
		return error(StatusCode::PAYLOAD_TOO_LARGE, "Selection chunk exceeds 64 KiB");
	}

	let request: SelectionRequest = match serde_json::from_slice(&body) {
		Ok(request) => request,
		Err(err) => return error(StatusCode::BAD_REQUEST, &format!("Malformed selection request: {err}")),
	};

	if request.ids.len() > SELECTION_CHUNK_MAX_IDS {
		return error(
			StatusCode::BAD_REQUEST,
			&format!("Selection chunk exceeds {SELECTION_CHUNK_MAX_IDS} ids"),
		);
	}

	let mut inner = lock!(state.divergence.inner);

	let Some(set) = inner.set.as_mut() else {
		return error(StatusCode::NOT_FOUND, "No pending divergence set");
	};

	if set.choice_id != request.choice_id {
		return error(StatusCode::NOT_FOUND, "Unknown or stale choiceId");
	}

	let total = set.items.len() as u32;

	if let Some(out_of_range) = request.ids.iter().find(|id| **id >= total) {
		return error(
			StatusCode::BAD_REQUEST,
			&format!("Selection id {out_of_range} is outside the divergence set (total {total})"),
		);
	}

	if request.restart {
		set.selection = None;
	}

	match &mut set.selection {
		None => {
			if request.chunk_index != 0 {
				return error(
					StatusCode::CONFLICT,
					"No selection in progress; the first chunk must have chunkIndex 0",
				);
			}

			set.selection = Some(Selection {
				submission_id: request.submission_id.clone(),
				next_chunk: 0,
				ids: BTreeSet::new(),
				committed: false,
			});
		}
		Some(selection) => {
			if selection.committed {
				return error(
					StatusCode::CONFLICT,
					"The selection is already committed; send restart to replace it",
				);
			}

			if selection.submission_id != request.submission_id {
				return error(
					StatusCode::CONFLICT,
					"Another selection submission is in progress; send restart to replace it",
				);
			}

			if request.chunk_index != selection.next_chunk {
				return error(
					StatusCode::CONFLICT,
					&format!(
						"Receipt mismatch: expected chunkIndex {}, got {}",
						selection.next_chunk, request.chunk_index
					),
				);
			}
		}
	}

	let selection = set.selection.as_mut().unwrap();

	selection.ids.extend(request.ids.iter().copied());
	selection.next_chunk += 1;
	selection.committed = request.final_chunk;

	Json(json!({
		"ok": true,
		"acceptedChunk": request.chunk_index,
		"nextChunk": selection.next_chunk,
		"selectedCount": selection.ids.len(),
		"committed": selection.committed,
	}))
	.into_response()
}

// POST /choice

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ChoiceRequest {
	choice_id: Option<String>,
	choice: Option<String>,
	mode: Option<String>,
}

/// The staged subset of a pending set for a decision: `mode: "all"` takes the
/// whole set, otherwise a committed selection restricts it (a selection equal
/// to the full set is exactly `mode: "all"`); no selection means everything.
/// The error is the conflict message for an uncommitted in-flight selection
fn staged_items(set: &DivergenceSet, mode: Option<&str>) -> Result<Vec<Item>, &'static str> {
	if mode == Some("all") {
		return Ok(set.items.clone());
	}

	match &set.selection {
		Some(selection) if selection.committed => Ok(set
			.items
			.iter()
			.filter(|item| selection.ids.contains(&item.id))
			.cloned()
			.collect()),
		Some(_) => Err("A selection upload is in progress but not committed"),
		None => Ok(set.items.clone()),
	}
}

/// What a `POST /choice` request resolved to while the divergence lock was
/// held; the (possibly slow, awaiting) application runs after the lock is
/// released
enum Decision {
	Refused(Response),
	Cancel { choice_id: String },
	Studio { choice_id: String, staged: Vec<Item> },
	Disk { choice_id: String, staged: Vec<Item> },
}

/// `POST /choice` — submit the divergence decision. `disk` applies
/// immediately by replaying the staged entries over the live sync channel
/// inside one plugin transaction; `studio` runs the server-driven pull with
/// fenced apply per synced root (recorded as pending application when no
/// plugin is connected); `cancel` clears the set
pub async fn choice_post(State(state): State<AppState>, Json(request): Json<ChoiceRequest>) -> Response {
	trace!("Received request: choice (post)");

	let choice = request.choice.as_deref().unwrap_or_default();

	if !matches!(choice, "studio" | "disk" | "cancel") {
		return error(
			StatusCode::BAD_REQUEST,
			"choice must be \"studio\", \"disk\" or \"cancel\"",
		);
	}

	// Everything that needs the pending set happens under the lock in this
	// synchronous block; the decision is applied lock-free afterwards
	let decision = {
		let mut inner = lock!(state.divergence.inner);

		let matches_pending = inner
			.set
			.as_ref()
			.is_some_and(|set| Some(set.choice_id.as_str()) == request.choice_id.as_deref());

		if !matches_pending {
			// Already resolved (or never existed): another client won the race
			Decision::Refused(error(StatusCode::CONFLICT, "resolved"))
		} else {
			let choice_id = inner.set.as_ref().unwrap().choice_id.clone();

			match choice {
				"cancel" => {
					inner.set = None;

					Decision::Cancel { choice_id }
				}
				kept => match staged_items(inner.set.as_ref().unwrap(), request.mode.as_deref()) {
					Err(message) => Decision::Refused(error(StatusCode::CONFLICT, message)),
					Ok(staged) => {
						inner.set = None;

						if kept == "studio" {
							Decision::Studio { choice_id, staged }
						} else {
							Decision::Disk { choice_id, staged }
						}
					}
				},
			}
		}
	};

	match decision {
		Decision::Refused(response) => response,
		Decision::Cancel { choice_id } => {
			state.ws.emit(Event::ChoiceMade {
				choice_id,
				choice: "cancel".into(),
			});

			Json(json!({ "ok": true, "applied": false })).into_response()
		}
		Decision::Studio { choice_id, staged } => {
			// The apply operates per synced root: every root that carries at
			// least one staged entry is pulled and swapped whole
			let staged_roots: BTreeSet<String> = staged
				.iter()
				.filter_map(|item| item.instance_path.split('/').next())
				.filter(|root| !root.is_empty())
				.map(str::to_owned)
				.collect();

			state.ws.emit(Event::ChoiceMade {
				choice_id: choice_id.clone(),
				choice: "studio".into(),
			});

			// Without a live plugin there is nothing to pull from: record the
			// decision honestly as pending application — never faked
			if state.ws.plugin_frames().is_err() {
				return Json(json!({
					"ok": true,
					"applied": false,
					"pendingApplication": true,
					"reason": "no plugin connected",
				}))
				.into_response();
			}

			debug!(
				"Applying divergence choice {choice_id} toward disk: {} staged roots",
				staged_roots.len()
			);

			bulk::apply_keep_studio(&state, &choice_id, staged_roots).await
		}
		Decision::Disk { choice_id, staged } => {
			let changes = build_disk_apply(&state.core, &staged);

			debug!(
				"Applying divergence choice {choice_id} toward Studio: {} additions, {} updates, {} removals",
				changes.additions.len(),
				changes.updates.len(),
				changes.removals.len()
			);

			// With a live WS plugin the replay goes directly over its frame
			// channel, bracketed by one explicit transaction so the pull is
			// one Studio Undo (Design §7.4-B). Without one, the changes are
			// queued for whichever transport connects (compat behavior)
			let replayed = if state.ws.plugin_frames().is_ok() {
				bulk::replay_disk_bracketed(&state, &choice_id, changes)
			} else {
				state
					.core
					.queue()
					.push(server::SyncChanges(changes), None)
					.map_err(|err| format!("Failed to queue sync frames: {err}"))
			};

			if let Err(err) = replayed {
				return error(StatusCode::INTERNAL_SERVER_ERROR, &err);
			}

			state.ws.emit(Event::ChoiceMade {
				choice_id,
				choice: "disk".into(),
			});

			Json(json!({ "ok": true, "applied": true })).into_response()
		}
	}
}

// GET /choice/source

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct SourceParams {
	choice_id: Option<String>,
	id: Option<u32>,
}

/// Caps one side's source at the pinned 256 KiB, flagging the cut
fn capped_source(source: &str) -> (String, bool) {
	if source.len() <= CONFLICT_SOURCE_CAP {
		return (source.to_owned(), false);
	}

	let mut end = CONFLICT_SOURCE_CAP;

	while !source.is_char_boundary(end) {
		end -= 1;
	}

	(source[..end].to_owned(), true)
}

fn side_json(source: Option<&str>) -> Value {
	match source {
		Some(source) => {
			let (source, truncated) = capped_source(source);
			let mut side = json!({ "present": true, "source": source });

			if truncated {
				side["truncated"] = json!(true);
			}

			side
		}
		None => json!({ "present": false }),
	}
}

/// `GET /choice/source?choiceId=&id=` — both sides of one pending divergence
/// row for staging diffs (Design §5.2): the disk side from the projected
/// file, the Studio side fetched live through a bounded plugin `get` op.
/// Script-backed rows only; each side is capped at 256 KiB with a
/// `truncated` flag
pub async fn choice_source(State(state): State<AppState>, Query(params): Query<SourceParams>) -> Response {
	trace!("Received request: choice source");

	let item = {
		let inner = lock!(state.divergence.inner);

		let Some(set) = inner.set.as_ref() else {
			return error(StatusCode::NOT_FOUND, "No pending divergence set");
		};

		if params.choice_id.as_deref() != Some(set.choice_id.as_str()) {
			return error(StatusCode::NOT_FOUND, "Unknown or stale choiceId");
		}

		let Some(id) = params.id else {
			return error(StatusCode::BAD_REQUEST, "id is required");
		};

		let Some(item) = set.items.iter().find(|item| item.id == id) else {
			return error(StatusCode::NOT_FOUND, "No such row in the pending set");
		};

		if !util::is_script(&item.class) {
			return error(
				StatusCode::BAD_REQUEST,
				"Only script-backed rows carry sources; use /choice/details for this row",
			);
		}

		item.clone()
	};

	// Disk side: the projected content the live tree holds for this row
	let disk = if item.state == "missing-on-disk" {
		side_json(None)
	} else {
		let tree = state.core.tree();

		let source = tree.get_instance(item.instance).and_then(|instance| {
			instance.properties.get(&ustr("Source")).and_then(|value| match value {
				Variant::String(source) => Some(source.clone()),
				_ => None,
			})
		});

		side_json(source.as_deref())
	};

	// Studio side: fetched live through the plugin; without one there is no
	// honest answer, so the row is unavailable rather than half-rendered
	let studio = if item.state == "only-on-disk" {
		side_json(None)
	} else {
		if let Err(reason) = state.ws.plugin_frames() {
			return (
				StatusCode::SERVICE_UNAVAILABLE,
				Json(json!({ "ok": false, "error": reason })),
			)
				.into_response();
		}

		let fetched = bulk::plugin_request(
			&state,
			"get",
			json!({ "path": item.instance_path, "prop": "Source" }),
			REQUEST_DEFAULT_TIMEOUT_MS,
		)
		.await;

		match fetched {
			Ok(value) => {
				let source = value
					.as_str()
					.map(str::to_owned)
					.or_else(|| value.get("value").and_then(Value::as_str).map(str::to_owned));

				side_json(source.as_deref())
			}
			// The row went stale in Studio: an honest absent side
			Err(err) if err.code == codes::NOT_FOUND => side_json(None),
			Err(err) => {
				return (
					StatusCode::SERVICE_UNAVAILABLE,
					Json(json!({ "ok": false, "error": err.to_string() })),
				)
					.into_response();
			}
		}
	};

	Json(json!({
		"id": item.id,
		"path": item.path,
		"instancePath": item.instance_path,
		"state": item.state,
		"disk": disk,
		"studio": studio,
	}))
	.into_response()
}

/// Builds the disk → Studio replay for the staged entries: additions and
/// full-state updates from the live tree, removals for the Studio-only
/// entries. Unstaged entries are untouched. The queue's sync-frame writer
/// chunks the result per the existing frame limits
fn build_disk_apply(core: &Core, staged: &[Item]) -> Changes {
	let engine = core.conflicts();
	let mut changes = Changes::new();

	{
		let tree = core.tree();

		for item in staged {
			match item.state {
				"differs" => {
					let Some(instance) = tree.get_instance(item.instance) else {
						debug!("Skipping stale divergence entry {}: gone from the tree", item.id);
						continue;
					};

					let mut update = UpdatedSnapshot::new(item.instance);
					update.name = Some(instance.name.clone());
					update.class = Some(instance.class);
					update.properties = Some(instance.properties.clone());

					let content = ContentState::new(&instance.name, &instance.class, &instance.properties);

					changes.update(update);
					engine.stamp(item.instance, content.hash);
				}
				"missing-on-disk" => {
					// Studio-only instance: staged means "remove from Studio"
					changes.remove(item.instance);
				}
				_ => {}
			}
		}
	}

	// Additions snapshot whole subtrees; `Core::snapshot` takes the tree
	// lock itself, so they are collected outside the guard above
	for item in staged {
		if item.state != "only-on-disk" {
			continue;
		}

		match core.snapshot(item.instance) {
			Some(snapshot) => {
				changes.additions.push(snapshot);
				stamp_subtree(core, item.instance);
			}
			None => debug!("Skipping stale divergence entry {}: gone from the tree", item.id),
		}
	}

	changes
}

fn stamp_subtree(core: &Core, id: Ref) {
	let engine = core.conflicts();
	let tree = core.tree();

	for id in conflict::subtree_refs(&tree, id) {
		if let Some(instance) = tree.get_instance(id) {
			let content = ContentState::new(&instance.name, &instance.class, &instance.properties);

			engine.stamp(id, content.hash);
		}
	}
}
