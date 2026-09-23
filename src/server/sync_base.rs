//! The sync base: what both sides last agreed on, per instance path, kept on
//! disk so a connect can tell *which* side moved while they were apart.
//!
//! Without it a connect only sees that disk and Studio differ. It cannot tell
//! "disk moved ahead while the daemon was down" from "both sides changed", so
//! Studio-first had to treat every difference as a clash and win it — which is
//! how one daemon crash turned into a mass overwrite of good disk work. With a
//! base, every difference is a three-way comparison:
//!
//! * only disk changed since the base → disk wins, pushed to Studio;
//! * only Studio changed → Studio wins, written to disk;
//! * both changed, or the base cannot tell → Studio wins and the disk side goes
//!   to the backlog, exactly as before.
//!
//! The key per instance is deliberately narrow — the class plus the SHA-256 of
//! the normalized script source — because that is what both sides can compute
//! identically: the plugin hashes the editor text it would sync, the daemon the
//! file it read. A difference outside the key (a sidecar property, an
//! attribute) is never evidence that one side moved, so it falls back to
//! Studio-first.
//!
//! Recording happens at every committed connect (the tree is then the agreed
//! state) and, while a plugin stays connected and synced, from a lagged
//! snapshot of the tree: taken when the tree changed, committed one tick later
//! only if the connection held the whole time. An edit that was queued but
//! never delivered therefore cannot be recorded as agreed. Parked conflicts
//! keep their previous base.
//!
//! A base belongs to one place. It records the place the plugin reported, and a
//! connect from a different place ignores it rather than reading every
//! difference as a deletion.

use log::{debug, info, warn};
use rbx_dom_weak::types::{Ref, Variant};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
	collections::{BTreeMap, BTreeSet, HashSet},
	fs,
	path::{Path, PathBuf},
	sync::Mutex,
	time::{Duration, SystemTime, UNIX_EPOCH},
};

use crate::{constants::BACKUPS_DIR, core::tree::Tree, lock, server::AppState, util, Properties};

/// The persisted base, inside `.wsync-backups` so it is never projected
const BASE_FILE: &str = "sync-base.json";

const BASE_VERSION: u32 = 1;

/// How often the writer looks at the tree. A snapshot is committed one tick
/// after it was taken, so this is also the delivery grace an edit gets before
/// it may be recorded as agreed
pub const WRITER_TICK: Duration = Duration::from_secs(2);

/// What a side "is" for the three-way comparison: its class, plus the SHA-256
/// of the LF-normalized source when it is a script
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Key {
	pub class: String,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub source: Option<String>,
}

impl Key {
	/// The key of a daemon-side instance (its properties as read from disk)
	pub fn of(class: &str, properties: &Properties) -> Self {
		let source = util::is_script(class).then(|| {
			let source = match properties.get(&rbx_dom_weak::ustr("Source")) {
				Some(Variant::String(source)) => source.as_str(),
				_ => "",
			};

			source_hash(source)
		});

		Self {
			class: class.to_owned(),
			source,
		}
	}

	/// The key of a Studio-side instance as the plugin reported it. `None`
	/// when the plugin sent no source hash for a script — an older plugin —
	/// which the comparison must read as "cannot tell"
	pub fn from_studio(class: &str, source_sha256: Option<&str>) -> Option<Self> {
		if !util::is_script(class) {
			return Some(Self {
				class: class.to_owned(),
				source: None,
			});
		}

		let hash = source_sha256?.trim().to_ascii_lowercase();

		if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
			return None;
		}

		Some(Self {
			class: class.to_owned(),
			source: Some(hash),
		})
	}
}

/// SHA-256 hex of a script source with CRLF folded to LF — the same
/// normalization the conflict engine and the plugin apply
pub fn source_hash(source: &str) -> String {
	let digest = if source.contains('\r') {
		Sha256::digest(source.replace("\r\n", "\n").as_bytes())
	} else {
		Sha256::digest(source.as_bytes())
	};

	format!("{digest:x}")
}

/// The Studio place a base was recorded against
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Place {
	pub place_id: u64,
	pub game_id: u64,
}

/// Per-path keys taken from one tree state. Paths that occur more than once
/// (same-named siblings) cannot be told apart by path and are listed as
/// ambiguous instead of keyed; `held` are paths whose instances were parked
/// and so must keep whatever base they had
#[derive(Debug, Default, Clone)]
pub struct Snapshot {
	pub entries: BTreeMap<String, Key>,
	pub ambiguous: BTreeSet<String>,
	pub held: BTreeSet<String>,
}

/// Keys every instance under the tree root. `hold` names instances whose
/// state is not agreed right now (parked conflicts)
pub fn snapshot_tree(tree: &Tree, hold: &HashSet<Ref>) -> Snapshot {
	let mut snapshot = Snapshot::default();
	let mut seen: HashSet<String> = HashSet::new();

	fn walk(tree: &Tree, id: Ref, prefix: &str, hold: &HashSet<Ref>, seen: &mut HashSet<String>, out: &mut Snapshot) {
		let Some(instance) = tree.get_instance(id) else {
			return;
		};

		for child in instance.children() {
			let Some(child_instance) = tree.get_instance(*child) else {
				continue;
			};

			let path = if prefix.is_empty() {
				child_instance.name.clone()
			} else {
				format!("{prefix}/{}", child_instance.name)
			};

			if !seen.insert(path.clone()) {
				out.entries.remove(&path);
				out.ambiguous.insert(path.clone());
			} else if hold.contains(child) {
				out.held.insert(path.clone());
			} else {
				out.entries
					.insert(path.clone(), Key::of(&child_instance.class, &child_instance.properties));
			}

			walk(tree, *child, &path, hold, seen, out);
		}
	}

	walk(tree, tree.root_ref(), "", hold, &mut seen, &mut snapshot);

	// A held path that later turned out ambiguous is ambiguous
	for path in &snapshot.ambiguous {
		snapshot.held.remove(path);
	}

	snapshot
}

/// Instance paths that occur more than once in the tree — cheap, no hashing
pub fn ambiguous_paths(tree: &Tree) -> BTreeSet<String> {
	let mut seen: HashSet<String> = HashSet::new();
	let mut ambiguous = BTreeSet::new();
	let mut stack: Vec<(Ref, String)> = vec![(tree.root_ref(), String::new())];

	while let Some((id, prefix)) = stack.pop() {
		let Some(instance) = tree.get_instance(id) else {
			continue;
		};

		for child in instance.children() {
			let Some(child_instance) = tree.get_instance(*child) else {
				continue;
			};

			let path = if prefix.is_empty() {
				child_instance.name.clone()
			} else {
				format!("{prefix}/{}", child_instance.name)
			};

			if !seen.insert(path.clone()) {
				ambiguous.insert(path.clone());
			}

			stack.push((*child, path));
		}
	}

	ambiguous
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Stored {
	version: u32,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	place: Option<Place>,
	saved_at: u64,
	entries: BTreeMap<String, Key>,
	#[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
	ambiguous: BTreeSet<String>,
}

/// What a lookup in the base found
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lookup<'a> {
	/// The path was agreed at the last sync with this key
	Known(&'a Key),
	/// The path did not exist at the last sync
	Absent,
	/// The base cannot say (same-named siblings)
	Unknown,
}

/// A read-only copy of the base, handed to the connect classification
#[derive(Debug, Clone)]
pub struct BaseView {
	entries: BTreeMap<String, Key>,
	ambiguous: BTreeSet<String>,
}

impl BaseView {
	#[cfg(test)]
	pub fn from_entries(entries: BTreeMap<String, Key>, ambiguous: BTreeSet<String>) -> Self {
		Self { entries, ambiguous }
	}

	pub fn lookup(&self, path: &str) -> Lookup<'_> {
		if self.ambiguous.contains(path) {
			return Lookup::Unknown;
		}

		match self.entries.get(path) {
			Some(key) => Lookup::Known(key),
			None => Lookup::Absent,
		}
	}

	/// Every base entry strictly below `path`, keyed relative to it. `None`
	/// when any descendant path is ambiguous — the subtree cannot be compared
	pub fn descendants(&self, path: &str) -> Option<BTreeMap<String, &Key>> {
		let prefix = format!("{path}/");

		if self
			.ambiguous
			.range(prefix.clone()..)
			.take_while(|candidate| candidate.starts_with(&prefix))
			.next()
			.is_some()
		{
			return None;
		}

		Some(
			self.entries
				.range(prefix.clone()..)
				.take_while(|(candidate, _)| candidate.starts_with(&prefix))
				.map(|(candidate, key)| (candidate[prefix.len()..].to_owned(), key))
				.collect(),
		)
	}
}

/// A tree snapshot waiting out one writer tick before it may be committed
struct Pending {
	epoch: u64,
	snapshot: Snapshot,
}

#[derive(Default)]
struct Inner {
	stored: Option<Stored>,
	/// True from a committed connect until the plugin goes away: only then is
	/// the tree the agreed state
	synced: bool,
	/// Bumped on every sync-state transition; a pending snapshot is only
	/// committed under the epoch it was taken in
	epoch: u64,
	/// Place reported by the plugin for the current session
	place: Option<Place>,
	pending: Option<Pending>,
	/// Tree generation of the last committed (or pending) snapshot
	last_generation: Option<u64>,
	/// Tree generation when a synced plugin dropped: a reconnect that finds
	/// the tree unchanged may resume without a fresh compare
	resume_generation: Option<u64>,
}

pub struct SyncBase {
	file: PathBuf,
	inner: Mutex<Inner>,
}

fn now_seconds() -> u64 {
	SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.map(|elapsed| elapsed.as_secs())
		.unwrap_or(0)
}

impl SyncBase {
	pub fn load(workspace_dir: &Path) -> Self {
		let file = workspace_dir.join(BACKUPS_DIR).join(BASE_FILE);

		let stored = match fs::read_to_string(&file) {
			Ok(contents) => match serde_json::from_str::<Stored>(&contents) {
				Ok(stored) if stored.version == BASE_VERSION => Some(stored),
				Ok(stored) => {
					warn!(
						"Ignoring the sync base at {}: version {} is not {BASE_VERSION}",
						file.display(),
						stored.version
					);
					None
				}
				Err(err) => {
					warn!("Ignoring an unreadable sync base at {}: {err}", file.display());
					None
				}
			},
			Err(_) => None,
		};

		if let Some(stored) = &stored {
			debug!("Loaded the sync base: {} path(s)", stored.entries.len());
		}

		Self {
			file,
			inner: Mutex::new(Inner {
				stored,
				..Inner::default()
			}),
		}
	}

	/// The base for a connect from `place`, or `None` when there is none or it
	/// belongs to another place. A base recorded without a place (an older
	/// plugin) is accepted from any place; one recorded with a place is only
	/// accepted when the connecting plugin names the same one
	pub fn view(&self, place: Option<Place>) -> Option<BaseView> {
		let inner = lock!(self.inner);
		let stored = inner.stored.as_ref()?;

		match (stored.place, place) {
			(Some(recorded), Some(connecting)) if recorded != connecting => {
				info!(
					"Sync base belongs to place {} (game {}), not {} (game {}): connecting without it",
					recorded.place_id, recorded.game_id, connecting.place_id, connecting.game_id
				);
				return None;
			}
			(Some(_), None) => {
				info!("The plugin did not report its place, so the sync base cannot be matched to it");
				return None;
			}
			_ => {}
		}

		Some(BaseView {
			entries: stored.entries.clone(),
			ambiguous: stored.ambiguous.clone(),
		})
	}

	pub fn is_synced(&self) -> bool {
		lock!(self.inner).synced
	}

	/// A compare started: the tree stops being the agreed state until it
	/// commits
	pub fn begin_compare(&self, place: Option<Place>) {
		let mut inner = lock!(self.inner);

		inner.synced = false;
		inner.epoch += 1;
		inner.pending = None;
		inner.resume_generation = None;

		if place.is_some() {
			inner.place = place;
		}
	}

	/// A connect committed: `snapshot` is the agreed state (its held paths
	/// keep their previous base) and the synced session starts. `settle`
	/// makes the next tick take a fresh snapshot even if the tree did not
	/// move, so held paths catch up once the session proves it survived them
	pub fn commit(&self, snapshot: Snapshot, generation: u64, settle: bool) {
		let mut inner = lock!(self.inner);

		let stored = merge(inner.stored.as_ref(), snapshot, inner.place);

		self.persist(&stored);

		inner.stored = Some(stored);
		inner.synced = true;
		inner.epoch += 1;
		inner.pending = None;
		inner.last_generation = if settle { None } else { Some(generation) };
		inner.resume_generation = None;
	}

	/// The plugin went away. Nothing that happens to the tree from here on is
	/// delivered, so recording stops
	pub fn on_disconnect(&self, generation: u64) {
		let mut inner = lock!(self.inner);

		if inner.synced {
			debug!("Sync base: plugin disconnected, recording paused");

			inner.synced = false;
			inner.epoch += 1;
			inner.pending = None;
			inner.resume_generation = Some(generation);
		}
	}

	/// A plugin reconnected without a compare (a socket blip). Recording
	/// resumes only if nothing changed on disk meanwhile — anything that did
	/// was dropped undelivered, and only the next full connect settles it
	pub fn on_reconnect(&self, generation: u64) {
		let mut inner = lock!(self.inner);

		if let Some(dropped_at) = inner.resume_generation.take() {
			if dropped_at == generation {
				debug!("Sync base: plugin reconnected with nothing missed, recording resumed");

				inner.synced = true;
				inner.epoch += 1;
			} else {
				info!("Sync base: the tree changed while the plugin was away; recording waits for the next connect");
			}
		}
	}

	/// One writer step. Commits the snapshot taken last tick if the session
	/// held, then takes a new one if the tree moved. `take` builds a snapshot
	/// and reports the generation it saw; it is only called when needed
	pub fn tick(&self, generation: u64, plugin_present: bool, take: impl FnOnce() -> (Snapshot, u64)) {
		let epoch = {
			let mut inner = lock!(self.inner);

			if inner.synced && !plugin_present {
				// Backstop for a transport that left without a release hook
				inner.synced = false;
				inner.epoch += 1;
				inner.pending = None;
				inner.resume_generation = Some(generation);
			}

			if !inner.synced {
				inner.pending = None;
				return;
			}

			if let Some(pending) = inner.pending.take() {
				if pending.epoch == inner.epoch {
					let merged = merge(inner.stored.as_ref(), pending.snapshot, inner.place);

					self.persist(&merged);

					inner.stored = Some(merged);
				}
			}

			if inner.last_generation == Some(generation) {
				return;
			}

			inner.epoch
		};

		let (snapshot, taken_at) = take();

		let mut inner = lock!(self.inner);

		if inner.synced && inner.epoch == epoch {
			inner.last_generation = Some(taken_at);
			inner.pending = Some(Pending { epoch, snapshot });
		}
	}

	fn persist(&self, stored: &Stored) {
		let write = || -> std::io::Result<()> {
			if let Some(parent) = self.file.parent() {
				fs::create_dir_all(parent)?;
			}

			let temp = self.file.with_extension("json.tmp");

			fs::write(&temp, serde_json::to_vec(stored)?)?;
			fs::rename(&temp, &self.file)
		};

		if let Err(err) = write() {
			warn!("Failed to write the sync base to {}: {err}", self.file.display());
		}
	}
}

/// Runs the writer for the daemon's lifetime: every [`WRITER_TICK`], commit
/// last tick's snapshot if the session held and take a new one if the tree
/// moved. Parked conflicts are held at their previous base
pub fn spawn_writer(state: AppState) {
	tokio::spawn(async move {
		let mut interval = tokio::time::interval(WRITER_TICK);

		interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

		loop {
			interval.tick().await;

			let generation = state.core.tree().generation();
			let plugin_present = state.ws.plugin_status().is_some();

			state.sync_base.tick(generation, plugin_present, || {
				// Parked ids first: the engine's lock is never taken while the
				// tree is held here
				let hold: HashSet<Ref> = state
					.core
					.conflicts()
					.list()
					.iter()
					.map(|parked| parked.instance)
					.collect();

				let tree = state.core.tree();

				(snapshot_tree(&tree, &hold), tree.generation())
			});
		}
	});
}

/// A lagged snapshot folded into the stored base: held paths keep their old
/// key (or stay absent), everything else is the snapshot's
fn merge(previous: Option<&Stored>, snapshot: Snapshot, place: Option<Place>) -> Stored {
	let mut entries = snapshot.entries;

	if let Some(previous) = previous {
		for path in &snapshot.held {
			if let Some(key) = previous.entries.get(path) {
				entries.insert(path.clone(), key.clone());
			}
		}
	}

	Stored {
		version: BASE_VERSION,
		place: place.or_else(|| previous.and_then(|previous| previous.place)),
		saved_at: now_seconds(),
		entries,
		ambiguous: snapshot.ambiguous,
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn key(class: &str, source: Option<&str>) -> Key {
		Key {
			class: class.to_owned(),
			source: source.map(source_hash),
		}
	}

	fn snapshot(entries: &[(&str, Key)]) -> Snapshot {
		Snapshot {
			entries: entries
				.iter()
				.map(|(path, key)| (path.to_string(), key.clone()))
				.collect(),
			..Snapshot::default()
		}
	}

	fn place(place_id: u64) -> Place {
		Place { place_id, game_id: 7 }
	}

	#[test]
	fn source_hash_folds_crlf() {
		assert_eq!(source_hash("a\r\nb"), source_hash("a\nb"));
		assert_ne!(source_hash("a\nb"), source_hash("a\nc"));
	}

	#[test]
	fn studio_keys_need_a_valid_hash_for_scripts_only() {
		assert!(Key::from_studio("ModuleScript", None).is_none());
		assert!(Key::from_studio("ModuleScript", Some("nope")).is_none());
		assert_eq!(
			Key::from_studio("Folder", None),
			Some(Key {
				class: "Folder".into(),
				source: None
			})
		);

		let hash = source_hash("return 1");

		assert_eq!(
			Key::from_studio("ModuleScript", Some(&hash.to_uppercase())),
			Some(key("ModuleScript", Some("return 1")))
		);
	}

	#[test]
	fn lookups_and_descendants() {
		let view = BaseView::from_entries(
			[
				("SSS".to_owned(), key("Folder", None)),
				("SSS/A".to_owned(), key("ModuleScript", Some("a"))),
				("SSS/A/B".to_owned(), key("ModuleScript", Some("b"))),
				("SSS/AB".to_owned(), key("ModuleScript", Some("ab"))),
			]
			.into_iter()
			.collect(),
			["SSS/Dup".to_owned()].into_iter().collect(),
		);

		assert_eq!(view.lookup("SSS/A"), Lookup::Known(&key("ModuleScript", Some("a"))));
		assert_eq!(view.lookup("SSS/Missing"), Lookup::Absent);
		assert_eq!(view.lookup("SSS/Dup"), Lookup::Unknown);

		let below = view.descendants("SSS/A").unwrap();

		assert_eq!(below.len(), 1, "SSS/AB is a sibling, not a descendant");
		assert!(below.contains_key("B"));
		assert!(
			view.descendants("SSS").is_none(),
			"an ambiguous descendant poisons the subtree"
		);
	}

	#[test]
	fn commit_persists_and_reloads_for_the_same_place_only() {
		let dir = tempfile::tempdir().unwrap();
		let base = SyncBase::load(dir.path());

		assert!(base.view(Some(place(1))).is_none());

		base.begin_compare(Some(place(1)));
		base.commit(snapshot(&[("SSS/A", key("ModuleScript", Some("a")))]), 1, false);

		assert!(base.is_synced());

		let reloaded = SyncBase::load(dir.path());

		assert!(reloaded.view(Some(place(1))).is_some());
		assert!(
			reloaded.view(Some(place(2))).is_none(),
			"another place never reads this base"
		);
		assert!(reloaded.view(None).is_none(), "an unidentified place cannot be matched");
	}

	#[test]
	fn a_snapshot_commits_one_tick_later_only_if_the_session_held() {
		let dir = tempfile::tempdir().unwrap();
		let base = SyncBase::load(dir.path());

		base.begin_compare(Some(place(1)));
		base.commit(snapshot(&[("SSS/A", key("ModuleScript", Some("a")))]), 1, false);

		// Tree moved: the snapshot is taken but not yet committed
		base.tick(2, true, || (snapshot(&[("SSS/A", key("ModuleScript", Some("a2")))]), 2));

		let view = base.view(Some(place(1))).unwrap();
		assert_eq!(view.lookup("SSS/A"), Lookup::Known(&key("ModuleScript", Some("a"))));

		// Next tick with the session intact commits it
		base.tick(2, true, || unreachable!("the tree did not move"));

		let view = base.view(Some(place(1))).unwrap();
		assert_eq!(view.lookup("SSS/A"), Lookup::Known(&key("ModuleScript", Some("a2"))));

		// A snapshot taken before a disconnect is never committed
		base.tick(3, true, || (snapshot(&[("SSS/A", key("ModuleScript", Some("a3")))]), 3));
		base.on_disconnect(3);
		base.tick(3, false, || unreachable!("not synced"));

		let view = base.view(Some(place(1))).unwrap();
		assert_eq!(view.lookup("SSS/A"), Lookup::Known(&key("ModuleScript", Some("a2"))));
		assert!(!base.is_synced());
	}

	#[test]
	fn reconnect_resumes_only_when_nothing_was_missed() {
		let dir = tempfile::tempdir().unwrap();
		let base = SyncBase::load(dir.path());

		base.begin_compare(None);
		base.commit(Snapshot::default(), 5, false);

		base.on_disconnect(5);
		base.on_reconnect(5);
		assert!(base.is_synced());

		base.on_disconnect(5);
		base.on_reconnect(6);
		assert!(!base.is_synced(), "a disk change while away was dropped undelivered");
	}

	#[test]
	fn held_paths_keep_their_previous_key() {
		let previous = Stored {
			version: BASE_VERSION,
			place: None,
			saved_at: 0,
			entries: [("SSS/A".to_owned(), key("ModuleScript", Some("old")))]
				.into_iter()
				.collect(),
			ambiguous: BTreeSet::new(),
		};

		let mut next = snapshot(&[("SSS/B", key("ModuleScript", Some("b")))]);
		next.held.insert("SSS/A".to_owned());

		let merged = merge(Some(&previous), next, None);

		assert_eq!(merged.entries.get("SSS/A"), Some(&key("ModuleScript", Some("old"))));
		assert!(merged.entries.contains_key("SSS/B"));
	}
}
