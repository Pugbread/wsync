//! Divergence sets (Design §7): the chunked `POST /compare` upload, the
//! frozen immutable set behind a `choiceId`, paged details, chunked
//! selective staging, and the decision endpoint whose disk direction applies
//! immediately over the live sync channel. All request/response shapes and
//! bounds are pinned protocol contracts.
//!
//! A committed comparison never asks (Design §7.0). Every difference is
//! decided three ways against the sync base (`sync_base`, `three_way`): the
//! side that moved since the last sync wins; when both moved, or there is no
//! base to tell, Studio wins and the disk side goes to the backlog. What
//! Studio wins lands through the fenced root apply (`bulk`), what disk wins is
//! pushed to Studio as one bracketed replay, and the plugin re-pairs its
//! instances afterwards so nothing written during the connect is orphaned.
//! The legacy choice endpoints below remain for full-scope tooling.

use axum::{
	body::Bytes,
	extract::{Query, State},
	http::StatusCode,
	response::{IntoResponse, Json, Response},
};
use log::{debug, info, trace, warn};
use rbx_dom_weak::{types::Ref, types::Variant, ustr};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
	collections::{BTreeMap, BTreeSet, HashMap, HashSet},
	fs,
	path::{Path, PathBuf},
	sync::Mutex,
};
use uuid::Uuid;

use crate::{
	constants::{
		BACKUPS_DIR, CHOICE_DETAILS_BYTE_BUDGET, CHOICE_DETAILS_DEFAULT_LIMIT, CHOICE_DETAILS_MAX_LIMIT,
		COMPARE_BODY_MAX_BYTES, COMPARE_CHUNK_MAX_ENTRIES, CONFLICT_SOURCE_CAP, REQUEST_DEFAULT_TIMEOUT_MS,
		SELECTION_BODY_MAX_BYTES, SELECTION_CHUNK_MAX_IDS,
	},
	core::{
		changes::Changes,
		conflict::{self, ContentState},
		meta::SourceEntry,
		snapshot::UpdatedSnapshot,
		tree::Tree,
		Core,
	},
	lock,
	project::Scope,
	server::{
		self, backlog, bulk, crash_guard,
		sync_base::{self, BaseView, Key, Place},
		three_way::{self, Winner},
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
	studio: Studio,
}

/// What the plugin reported about the Studio side of an entry, for the
/// three-way classification (absent from plugins older than the sync base)
#[derive(Debug, Clone, Default)]
struct Studio {
	key: Option<Key>,
	/// Descendants of a `remove` root, keyed relative to it; `None` when not
	/// sent or truncated
	descendants: Option<BTreeMap<String, Option<Key>>>,
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
	studio: Studio,
}

impl Item {
	fn three_way_state(&self) -> three_way::State {
		match self.state {
			"only-on-disk" => three_way::State::OnlyOnDisk,
			"differs" => three_way::State::Differs,
			_ => three_way::State::MissingOnDisk,
		}
	}

	/// The served root an item lives under (first instance-path segment)
	fn root(&self) -> Option<&str> {
		self.instance_path.split('/').next().filter(|root| !root.is_empty())
	}
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
	place: Option<Place>,
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
	/// The connecting place, so a sync base is only ever read by the place it
	/// was recorded against (absent from older plugins)
	#[serde(default)]
	place: Option<Place>,
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
	/// SHA-256 of Studio's LF-normalized editor source, for scripts on
	/// `update` and `remove` entries
	#[serde(default)]
	source_sha256: Option<String>,
	/// Studio's descendants of a `remove` root (bounded by the plugin)
	#[serde(default)]
	descendants: Option<Vec<CompareDescendant>>,
	#[serde(default)]
	descendants_truncated: bool,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct CompareDescendant {
	/// Relative to the entry's instance path
	path: String,
	class: String,
	#[serde(default)]
	source_sha256: Option<String>,
}

impl CompareEntry {
	/// The Studio side as far as the plugin reported it. Only `update` and
	/// `remove` entries describe an instance that exists in Studio
	fn studio(&self, change: EntryChange, scope: Scope) -> Studio {
		if change == EntryChange::Add {
			return Studio::default();
		}

		let key = Key::from_studio(&self.class, self.source_sha256.as_deref());

		let descendants = match (&self.descendants, change) {
			(Some(descendants), EntryChange::Remove) if !self.descendants_truncated => Some(
				descendants
					.iter()
					.filter(|descendant| scope.allows_class(&descendant.class))
					.map(|descendant| {
						(
							descendant.path.clone(),
							Key::from_studio(&descendant.class, descendant.source_sha256.as_deref()),
						)
					})
					.collect(),
			),
			_ => None,
		};

		Studio { key, descendants }
	}
}

/// What a `/compare` chunk resolved to while the divergence lock was held.
/// The Studio-first apply awaits plugin ops, so it runs lock-free afterwards
/// (the guard is `!Send` and must never live across an await)
enum Ingested {
	/// Fully answered synchronously (receipts, errors, full-scope commits)
	Done(Response),
	/// A comparison froze `items`; the connect apply runs next
	StudioFirst {
		items: Vec<Item>,
		accepted: u64,
		next: u64,
		place: Option<Place>,
	},
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
			studio: entry.studio(change, scope),
		});
	}

	match ingest_chunk(&state, &request, entries, scope) {
		Ingested::Done(response) => response,
		Ingested::StudioFirst {
			items,
			accepted,
			next,
			place,
		} => connect_commit(&state, items, accepted, next, place).await,
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
				place: request.place,
			});

			// From here until the commit the tree is not the agreed state
			state.sync_base.begin_compare(request.place);
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
	upload.place = upload.place.or(request.place);

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
		// Both sides agree. This is the moment agreement is proven for the
		// whole projection, so every baseline stamps and the sync base is
		// recorded. Nothing is cleared alongside it: the backlog is not
		// pending work, it is what was kept, and it expires on its own
		// schedule rather than on a clean connect.
		drop(inner);

		seed_all_baselines(&state.core);
		commit_sync_base(state, &HashSet::new(), BTreeSet::new(), false);

		info!("Divergence comparison committed clean: sides are in sync");

		return Ingested::Done(
			Json(json!({
				"ok": true,
				"acceptedChunk": accepted,
				"nextChunk": next,
				"committed": true,
				"clean": true,
			}))
			.into_response(),
		);
	}

	let place = upload.place;
	let (items, _stats) = freeze(&state.core, upload.entries);

	// No choice, no prompt, whatever the scope. The caller runs the apply
	// outside the divergence lock (it awaits plugin ops); what loses goes to
	// the backlog rather than to a question.
	let _ = scope;

	Ingested::StudioFirst {
		items,
		accepted,
		next,
		place,
	}
}

/// Commits a comparison: decides every difference three ways against the
/// sync base (Studio-first when there is none, Design §7.0), writes what
/// Studio wins to disk through the fenced root apply, pushes what disk wins
/// to Studio, and moves every disk original that lost into the backlog.
///
/// Nothing asks. Where the base shows that only one side moved since the last
/// sync, that side wins and nothing is backlogged. Where both moved, or the
/// base cannot tell, Studio wins and the disk side is kept in the backlog —
/// the old Studio-first rule, now reserved for real conflicts. A file Studio
/// deleted leaves the disk for the backlog; a file created on disk while the
/// two were apart goes to Studio instead.
async fn connect_commit(
	state: &AppState,
	items: Vec<Item>,
	accepted: u64,
	next: u64,
	place: Option<Place>,
) -> Response {
	let transfer_id = Uuid::new_v4().to_string();
	let workspace_dir = state.core.project().workspace_dir.clone();
	let base = state.sync_base.view(place);
	let winners = decide(&state.core, &items, base.as_ref());

	// The last daemon died applying a connect: decide, report, apply nothing
	if let Some(safe_mode) = state.crash_guard.safe_mode() {
		return refuse_in_safe_mode(state, &items, &winners, base.is_some(), safe_mode, accepted, next);
	}

	let guard_generation = state.crash_guard.begin();

	// Roots that need Studio's content on disk: something Studio wins that
	// exists in Studio. Everything else is handled without a pull
	let staged_roots: BTreeSet<String> = items
		.iter()
		.zip(&winners)
		.filter(|(item, winner)| **winner != Winner::Disk && item.state != "only-on-disk")
		.filter_map(|(item, _)| item.root().map(str::to_owned))
		.collect();

	let staging = workspace_dir
		.join(BACKUPS_DIR)
		.join("backlog-staging")
		.join(&transfer_id);

	let mut options = bulk::StudioFirstOptions {
		workspace_dir: workspace_dir.clone(),
		staging: staging.clone(),
		..Default::default()
	};

	// What each disk-only item is on disk: its own directory when it has
	// one, otherwise its files and sidecars — so nothing of it (an empty
	// folder, an orphaned `.meta.json`) is left behind when it leaves
	let mut disk_only_paths: HashMap<u32, Vec<PathBuf>> = HashMap::new();

	{
		let tree = state.core.tree();

		for (item, winner) in items.iter().zip(&winners) {
			let staged = item.root().is_some_and(|root| staged_roots.contains(root));

			match (item.state, winner) {
				("differs", Winner::StudioKeepDisk) => {
					if let Some(live) = safe_live_path(&workspace_dir, item.path.as_deref()) {
						if live.is_file() {
							options.preserve.push(live);
						}
					}
				}
				("differs", Winner::Disk) if staged => {
					options
						.keep_live
						.push((item.instance, bulk::file_entries(&tree, item.instance)));
				}
				("only-on-disk", _) => {
					let paths = own_paths(&tree, item.instance, &workspace_dir);

					if staged {
						options.disk_only.extend(paths.iter().cloned());
					}

					disk_only_paths.insert(item.id, paths);
				}
				("missing-on-disk", Winner::Disk) if staged => options.drop_staged.push(item.instance),
				_ => {}
			}
		}
	}

	let count = |wanted: Winner| winners.iter().filter(|winner| **winner == wanted).count();

	info!(
		"Connect {transfer_id}: {} difference(s) — {} to Studio, {} to disk, {} kept in the backlog ({}), {} staged root(s)",
		items.len(),
		count(Winner::Disk),
		count(Winner::Studio),
		count(Winner::StudioKeepDisk),
		if base.is_some() {
			"three-way against the sync base"
		} else {
			"no sync base: Studio-first"
		},
		staged_roots.len()
	);

	let backups = match bulk::apply_roots(state, &transfer_id, &staged_roots, Some(&options)).await {
		Ok(backups) => backups,
		Err((status, mut payload)) => {
			fs::remove_dir_all(&staging).ok();

			// A failed apply is not a crash
			state.crash_guard.settle(guard_generation);

			payload["acceptedChunk"] = json!(accepted);
			payload["nextChunk"] = json!(next);
			payload["committed"] = json!(true);

			return (status, Json(payload)).into_response();
		}
	};

	let report = std::mem::take(&mut *lock!(options.report));

	// The VFS is paused across the moves for the same reason the apply pauses
	// it: content leaving disk here is the resolution, not an edit to
	// broadcast, and a disk-only file has no instance in Studio to remove
	let vfs = state.core.vfs();

	vfs.pause();

	let mut backlogged = 0;
	let mut to_studio: Vec<Item> = Vec::new();
	let mut left_disk: Vec<Ref> = Vec::new();

	for (item, winner) in items.iter().zip(&winners) {
		match (item.state, winner) {
			("differs", Winner::StudioKeepDisk) => {
				if let Some(live) = safe_live_path(&workspace_dir, item.path.as_deref()) {
					if report.preserved.contains(&live) {
						backlogged += keep(state, &workspace_dir, &live, &options.preserved_path(&live));
					}
				}
			}
			("differs", Winner::Disk) => {
				if report.demoted.contains(&item.instance) {
					// Studio laid it out differently, so Studio's version took
					// the path; the disk version is kept instead of pushed
					for (_, files) in options.keep_live.iter().filter(|(id, _)| *id == item.instance) {
						for live in files.iter().filter(|live| report.preserved.contains(*live)) {
							backlogged += keep(state, &workspace_dir, live, &options.preserved_path(live));
						}
					}
				} else {
					to_studio.push(item.clone());
				}
			}
			("only-on-disk", _) => {
				let paths = disk_only_paths.remove(&item.id).unwrap_or_default();
				let mut pushable = *winner == Winner::Disk;
				let mut leaves = false;

				for live in &paths {
					if report.preserved.contains(live) {
						let original = options.preserved_path(live);

						if report.claimed.contains(live) {
							// Studio has content at this very path after all
							// (an instance the connect could not pair): its
							// version stands, the disk original is kept if it
							// differs, and nothing here is pushed
							pushable = false;

							if !bulk::same_contents(&original, live) {
								backlogged += keep(state, &workspace_dir, live, &original);
							}
						} else if !pushable {
							backlogged += keep(state, &workspace_dir, live, &original);
							remove_any(live);
							leaves = true;
						}
					} else if !pushable {
						// Nothing was staged under this root: move it straight
						backlogged += keep(state, &workspace_dir, live, live);
						leaves = true;
					}
				}

				if pushable {
					to_studio.push(item.clone());
				} else if leaves {
					left_disk.push(item.instance);
				}
			}
			("missing-on-disk", Winner::Disk) => to_studio.push(item.clone()),
			_ => {}
		}
	}

	fs::remove_dir_all(&staging).ok();

	// What left the disk leaves the tree too. Studio never had it, so there
	// is nothing to tell the plugin — but a tree still holding it would keep
	// answering for its path: a new file written there later would be sent
	// as an update to an instance Studio never received, and deleting it
	// would send Studio a removal for an id it never saw
	forget_instances(state, &left_disk);

	vfs.resume();

	let pushed = to_studio.len();

	if !to_studio.is_empty() {
		let changes = build_disk_apply(&state.core, &to_studio);

		let replayed = if state.ws.plugin_frames().is_ok() {
			bulk::replay_disk_bracketed(state, &transfer_id, changes)
		} else {
			state
				.core
				.queue()
				.push(server::SyncChanges(changes), None)
				.map_err(|err| format!("Failed to queue sync frames: {err}"))
		};

		if let Err(err) = replayed {
			warn!("Connect {transfer_id}: failed to push {pushed} disk change(s) to Studio: {err}");
		}
	}

	// The tree now holds the agreed state, except where disk won: those are
	// in flight to Studio, so their base holds until the writer sees the
	// session survive them
	seed_all_baselines(&state.core);

	let hold: HashSet<Ref> = to_studio
		.iter()
		.filter(|item| item.state != "missing-on-disk")
		.map(|item| item.instance)
		.collect();

	let held_paths: BTreeSet<String> = to_studio
		.iter()
		.filter(|item| item.state == "missing-on-disk")
		.map(|item| item.instance_path.clone())
		.collect();

	commit_sync_base(state, &hold, held_paths, !to_studio.is_empty());
	state.crash_guard.settle(guard_generation);

	if backlogged > 0 {
		info!("Backlogged {backlogged} disk entr(ies) that lost to Studio");

		state.ws.emit(Event::Backlog {
			total: state.backlog.list().len(),
			added: backlogged,
		});
	}

	Json(json!({
		"ok": true,
		"acceptedChunk": accepted,
		"nextChunk": next,
		"committed": true,
		"applied": true,
		"direction": "studio",
		"transferId": transfer_id,
		"backups": backups,
		"backlogged": backlogged,
		"toStudio": pushed,
		"toDisk": count(Winner::Studio) + count(Winner::StudioKeepDisk),
		"threeWay": base.is_some(),
		// A root swap re-reads what Studio wrote, and anything new to disk
		// gets a daemon id the plugin has never seen: it must re-pair its
		// instances before live sync starts, or those instances stay
		// unreachable in both directions
		"rehydrate": !staged_roots.is_empty(),
	}))
	.into_response()
}

/// Safe mode's answer to a connect: what it would have done, written to the
/// report, and a refusal the plugin surfaces instead of going live
fn refuse_in_safe_mode(
	state: &AppState,
	items: &[Item],
	winners: &[Winner],
	three_way: bool,
	safe_mode: crash_guard::SafeMode,
	accepted: u64,
	next: u64,
) -> Response {
	let decisions: Vec<Value> = items
		.iter()
		.zip(winners)
		.map(|(item, winner)| {
			json!({
				"instancePath": item.instance_path,
				"path": item.path,
				"state": item.state,
				"class": item.class,
				"wouldApply": match winner {
					Winner::Disk => "disk-to-studio",
					Winner::Studio => "studio-to-disk",
					Winner::StudioKeepDisk => "studio-to-disk, disk copy to the backlog",
				},
			})
		})
		.collect();

	let report = json!({
		"safeMode": safe_mode,
		"threeWay": three_way,
		"differences": decisions,
	});

	let written = state.crash_guard.write_report(&report);

	warn!(
		"Safe mode: refused a connect with {} difference(s); nothing was applied{}",
		items.len(),
		written
			.as_ref()
			.map(|path| format!(" (report: {})", path.display()))
			.unwrap_or_default()
	);

	(
		StatusCode::LOCKED,
		Json(json!({
			"ok": false,
			"error": format!(
				"WSync is in safe mode: the previous daemon crashed while applying a connect, so this connect \
				 applied nothing. {} difference(s) are listed in {}. Fix what crashed it (`wsync doctor` checks \
				 the usual disk hazards), then run `wsync daemon resume` and reconnect.",
				items.len(),
				written
					.map(|path| path.display().to_string())
					.unwrap_or_else(|| "the daemon log".into())
			),
			"safeMode": true,
			"acceptedChunk": accepted,
			"nextChunk": next,
			"committed": false,
		})),
	)
		.into_response()
}

/// Decides every item against the base
fn decide(core: &Core, items: &[Item], base: Option<&BaseView>) -> Vec<Winner> {
	let tree = core.tree();
	let tree_ambiguous = sync_base::ambiguous_paths(&tree);

	let mut path_counts: HashMap<&str, usize> = HashMap::new();

	for item in items {
		*path_counts.entry(item.instance_path.as_str()).or_default() += 1;
	}

	items
		.iter()
		.map(|item| {
			let disk = if item.state == "missing-on-disk" {
				None
			} else {
				tree.get_instance(item.instance)
					.map(|instance| Key::of(&instance.class, &instance.properties))
			};

			let input = three_way::Input {
				state: item.three_way_state(),
				instance_path: &item.instance_path,
				disk: disk.as_ref(),
				studio: item.studio.key.as_ref(),
				studio_descendants: item.studio.descendants.as_ref(),
				ambiguous: path_counts.get(item.instance_path.as_str()).copied().unwrap_or(0) > 1
					|| tree_ambiguous.contains(&item.instance_path),
			};

			three_way::classify(base, &input)
		})
		.collect()
}

/// Records the tree as the agreed state. `hold`/`held_paths` keep their
/// previous base (changes still in flight); `settle` makes the writer take a
/// fresh snapshot on its next tick so those catch up once delivered
fn commit_sync_base(state: &AppState, hold: &HashSet<Ref>, held_paths: BTreeSet<String>, settle: bool) {
	let (mut snapshot, generation) = {
		let tree = state.core.tree();

		(sync_base::snapshot_tree(&tree, hold), tree.generation())
	};

	snapshot.held.extend(held_paths);

	state.sync_base.commit(snapshot, generation, settle);
}

/// A workspace path behind a frozen item, if it is safe to touch
fn safe_live_path(workspace_dir: &Path, path: Option<&str>) -> Option<PathBuf> {
	path.filter(|path| backlog::safe_rel_path(path))
		.map(|path| workspace_dir.join(path))
}

/// Everything an instance occupies on disk by itself: its directory when it
/// is directory-backed (children included), else its file and sidecars
fn own_paths(tree: &Tree, id: Ref, workspace_dir: &Path) -> Vec<PathBuf> {
	let Some(meta) = tree.get_meta(id) else {
		return Vec::new();
	};

	let inside = |path: &Path| path.starts_with(workspace_dir) && path != workspace_dir;

	if let Some(folder) = meta.source.relevant().iter().find_map(|entry| match entry {
		SourceEntry::Folder(path) if inside(path) => Some(path.clone()),
		_ => None,
	}) {
		return vec![folder];
	}

	meta.source
		.relevant()
		.iter()
		.filter_map(|entry| match entry {
			SourceEntry::File(path) | SourceEntry::Data(path) if inside(path) => Some(path.clone()),
			_ => None,
		})
		.collect()
}

/// Moves `source` into the backlog under `live`'s workspace-relative path
fn keep(state: &AppState, workspace_dir: &Path, live: &Path, source: &Path) -> usize {
	let Some(relative) = live
		.strip_prefix(workspace_dir)
		.ok()
		.and_then(Path::to_str)
		.map(|path| path.replace('\\', "/"))
	else {
		return 0;
	};

	usize::from(
		state
			.backlog
			.capture(&relative, source, backlog::Reason::InitialSync)
			.is_some(),
	)
}

fn remove_any(path: &Path) {
	if path.is_dir() {
		fs::remove_dir_all(path).ok();
	} else {
		fs::remove_file(path).ok();
	}
}

/// Drops instances (and their subtrees) from the tree and the conflict
/// engine without telling anyone — for content that just left the disk and
/// that Studio never had
fn forget_instances(state: &AppState, ids: &[Ref]) {
	if ids.is_empty() {
		return;
	}

	let engine = state.core.conflicts();
	let mut tree = state.core.tree();

	for id in ids {
		if !tree.exists(*id) {
			continue;
		}

		for id in conflict::subtree_refs(&tree, *id) {
			engine.forget(id);
		}

		tree.remove_instance(*id);
	}
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
			studio: entry.studio,
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
