//! The baseline conflict engine (Design §6.3), generalized from Ro-Sync's
//! per-path engine to instances.
//!
//! For every synced instance the daemon keeps a **baseline**: the content
//! hash of the last state both sides agreed on, stamped whenever a change is
//! successfully applied in either direction. Decisions:
//!
//! * **FS change** → `Propagate | NoChange | Conflict`. Propagating an edit
//!   does not advance the baseline immediately: the plugin never
//!   acknowledges applied `sync` frames, so the previous baseline is held
//!   for a bounded race window (`CONFLICT_RACE_WINDOW`). A Studio push
//!   arriving inside that window that matches neither the sent nor the
//!   prior content is a both-edited conflict; after the window the baseline
//!   commits to the sent content.
//! * **Studio push** → `Apply | NoChange | Conflict`, the mirror logic: a
//!   push may only apply while the disk side still matches the baseline.
//!
//! Destructive provenance: applied FS removals leave a short-lived
//! tombstone so a racing Studio edit of the deleted instance parks as
//! `fs-deleted-studio-edited`; a Studio removal racing an in-flight FS edit
//! parks as `studio-deleted-fs-edited` and the file stays on disk.
//!
//! Parked conflicts are bounded (`CONFLICT_PARK_LIMIT`), keep both sides'
//! latest content for diff rendering, are broadcast as `conflict` events and
//! surface via `GET /resolve`. While an instance is parked nothing
//! propagates for it; live sync for other instances continues unaffected.
//!
//! Everything is gated behind `Config.conflict_engine` by the callers: with
//! the flag off the daemon behaves exactly like the engine-less fork
//! (last-writer-wins plus the `changes_threshold` prompt).

use chrono::{SecondsFormat, Utc};
use log::warn;
use rbx_dom_weak::types::Ref;
use std::{
	collections::HashMap,
	sync::Mutex,
	time::{Duration, Instant},
};

use crate::{
	constants::{CONFLICT_PARK_LIMIT, CONFLICT_RACE_WINDOW},
	lock, Properties,
};

pub mod content;

pub use content::{
	capture_instance, capture_subtree, hash_hex, instance_path, subtree_refs, Captured, ContentHash, ContentState,
	InstanceInfo,
};

/// Client id reserved for daemon-internal writes (conflict resolution).
/// Writes carrying it bypass the engine's decisions — the resolution routes
/// have already decided — while still re-stamping baselines
pub const RESOLUTION_CLIENT_ID: u32 = u32::MAX;

/// Outcome for an FS-side change (Design §6.3)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsDecision {
	/// Real FS-side change, safe to send toward Studio
	Propagate,
	/// Nothing to send (echo of the agreed or already-known state)
	NoChange,
	/// Parked: the op must be withheld from Studio
	Conflict,
}

/// Outcome for one Studio push operation (Design §6.3)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushDecision {
	/// Safe to apply through the write middleware
	Apply,
	/// Nothing to write (content already agreed)
	NoChange,
	/// Parked: the op must be excluded from application
	Conflict,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Classification {
	BothEdited,
	FsDeletedStudioEdited,
	StudioDeletedFsEdited,
}

impl Classification {
	pub fn as_str(&self) -> &'static str {
		match self {
			Self::BothEdited => "both-edited",
			Self::FsDeletedStudioEdited => "fs-deleted-studio-edited",
			Self::StudioDeletedFsEdited => "studio-deleted-fs-edited",
		}
	}
}

/// One side of a parked conflict: present + the latest known content
#[derive(Debug, Clone)]
pub struct Side {
	pub present: bool,
	pub content: Option<ContentState>,
}

impl Side {
	fn present(content: ContentState) -> Self {
		Self {
			present: true,
			content: Some(content),
		}
	}

	fn absent() -> Self {
		Self {
			present: false,
			content: None,
		}
	}
}

/// The raw Studio-side state kept so a `keep: "studio"` resolution can be
/// written back through the middleware write path
#[derive(Debug, Clone)]
pub struct StudioApply {
	pub name: String,
	pub class: String,
	pub properties: Properties,
}

#[derive(Debug, Clone)]
pub struct Parked {
	/// Stable conflict id (`c<n>`), the `POST /resolve` handle
	pub id: String,
	pub instance: Ref,
	pub info: InstanceInfo,
	pub classification: Classification,
	pub fs: Side,
	pub studio: Side,
	/// Present when the Studio side still exists (needed for `keep: "studio"`)
	pub studio_apply: Option<StudioApply>,
	pub detected_at: String,
}

impl Parked {
	/// The record's class: the side that still exists wins
	pub fn class(&self) -> String {
		self.studio
			.content
			.as_ref()
			.or(self.fs.content.as_ref())
			.map(|content| content.class.clone())
			.unwrap_or_default()
	}

	pub fn kind(&self) -> &'static str {
		if crate::util::is_script(&self.class()) {
			"script"
		} else {
			"properties"
		}
	}
}

/// An FS edit propagated toward Studio whose agreement is not confirmed yet
#[derive(Debug, Clone, Copy)]
struct Pending {
	prior: ContentHash,
	sent: ContentHash,
	at: Instant,
}

/// Provenance of an applied FS removal, held for the race window so a
/// concurrent Studio edit of the deleted instance conflicts correctly
#[derive(Debug, Clone)]
struct Tombstone {
	/// Content hashes that count as "Studio only echoed the deleted state"
	hashes: Vec<ContentHash>,
	info: InstanceInfo,
	/// The state the instance had when it was deleted, the base a Studio
	/// update overlay is applied onto
	base: StudioApply,
	at: Instant,
}

struct EngineState {
	baselines: HashMap<Ref, ContentHash>,
	pending: HashMap<Ref, Pending>,
	tombstones: HashMap<Ref, Tombstone>,
	parked: Vec<Parked>,
	next_id: u64,
	race_window: Duration,
	park_limit: usize,
	overflow_warned: bool,
}

type Notifier = Box<dyn Fn(&Parked) + Send + Sync>;

pub struct ConflictEngine {
	state: Mutex<EngineState>,
	notifier: Mutex<Option<Notifier>>,
}

impl ConflictEngine {
	pub fn new() -> Self {
		Self {
			state: Mutex::new(EngineState {
				baselines: HashMap::new(),
				pending: HashMap::new(),
				tombstones: HashMap::new(),
				parked: Vec::new(),
				next_id: 1,
				race_window: CONFLICT_RACE_WINDOW,
				park_limit: CONFLICT_PARK_LIMIT,
				overflow_warned: false,
			}),
			notifier: Mutex::new(None),
		}
	}

	/// Installs the `conflict` event sink (the server layer wires this to the
	/// WS event fan-out at boot)
	pub fn set_notifier(&self, notifier: Notifier) {
		*lock!(self.notifier) = Some(notifier);
	}

	/// Test hook: shrink or stretch the race window
	pub fn set_race_window(&self, window: Duration) {
		lock!(self.state).race_window = window;
	}

	/// Test hook: override the parked-conflict bound
	pub fn set_park_limit(&self, limit: usize) {
		lock!(self.state).park_limit = limit;
	}

	fn notify(&self, parked: &Parked) {
		if let Some(notifier) = &*lock!(self.notifier) {
			notifier(parked);
		}
	}

	// Bookkeeping

	/// Records agreement: both sides now hold `hash` for `id`. Clears any
	/// pending window or tombstone (Design §6.3 "stamped on every
	/// successfully applied change in either direction")
	pub fn stamp(&self, id: Ref, hash: ContentHash) {
		let mut state = lock!(self.state);

		state.baselines.insert(id, hash);
		state.pending.remove(&id);
		state.tombstones.remove(&id);
	}

	/// Forgets every trace of `id` (applied removals)
	pub fn forget(&self, id: Ref) {
		let mut state = lock!(self.state);

		state.baselines.remove(&id);
		state.pending.remove(&id);
		state.tombstones.remove(&id);
	}

	pub fn baseline(&self, id: Ref) -> Option<ContentHash> {
		lock!(self.state).baselines.get(&id).copied()
	}

	/// Commits expired pending windows (baseline advances to the sent
	/// content) and drops expired tombstones
	fn prune(state: &mut EngineState, now: Instant) {
		let window = state.race_window;

		let expired: Vec<(Ref, ContentHash)> = state
			.pending
			.iter()
			.filter(|(_, pending)| now.duration_since(pending.at) > window)
			.map(|(id, pending)| (*id, pending.sent))
			.collect();

		for (id, sent) in expired {
			state.pending.remove(&id);
			state.baselines.insert(id, sent);
		}

		state
			.tombstones
			.retain(|_, tombstone| now.duration_since(tombstone.at) <= window);
	}

	fn parked_index(state: &EngineState, id: Ref) -> Option<usize> {
		state.parked.iter().position(|parked| parked.instance == id)
	}

	pub fn is_parked(&self, id: Ref) -> bool {
		let state = lock!(self.state);

		Self::parked_index(&state, id).is_some()
	}

	pub fn parked_count(&self) -> usize {
		lock!(self.state).parked.len()
	}

	pub fn list(&self) -> Vec<Parked> {
		lock!(self.state).parked.clone()
	}

	/// Removes and returns the parked conflict with the given record id
	pub fn take(&self, conflict_id: &str) -> Option<Parked> {
		let mut state = lock!(self.state);
		let index = state.parked.iter().position(|parked| parked.id == conflict_id)?;

		Some(state.parked.remove(index))
	}

	/// Re-parks a conflict whose resolution failed to apply
	pub fn restore(&self, parked: Parked) {
		lock!(self.state).parked.push(parked);
	}

	/// Parks a new conflict, returning whether it was stored. Beyond the
	/// bound the losing operation is still excluded by the caller — nothing
	/// is ever silently clobbered — but the record is dropped with a warning
	fn park(
		state: &mut EngineState,
		classification: Classification,
		instance: Ref,
		info: InstanceInfo,
		fs: Side,
		studio: Side,
		studio_apply: Option<StudioApply>,
	) -> Option<Parked> {
		if state.parked.len() >= state.park_limit {
			if !state.overflow_warned {
				warn!(
					"Parked conflict limit ({}) reached; further conflicts are excluded from sync but not listed. Resolve pending conflicts",
					state.park_limit
				);
				state.overflow_warned = true;
			}

			return None;
		}

		let id = format!("c{}", state.next_id);
		state.next_id += 1;

		let parked = Parked {
			id,
			instance,
			info,
			classification,
			fs,
			studio,
			studio_apply,
			detected_at: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
		};

		state.parked.push(parked.clone());
		state.pending.remove(&instance);
		state.tombstones.remove(&instance);

		Some(parked)
	}

	// FS-side decisions

	/// A new instance appeared on disk: always propagates, and the baseline
	/// stamps immediately (the plugin cannot race an id it has never seen)
	pub fn on_fs_addition(&self, id: Ref, content: &ContentState) -> FsDecision {
		let mut state = lock!(self.state);

		Self::prune(&mut state, Instant::now());
		state.baselines.insert(id, content.hash);
		state.tombstones.remove(&id);

		FsDecision::Propagate
	}

	/// An existing instance changed on disk. `pre` is the tree state before
	/// the change, `post` the state after
	pub fn on_fs_update(&self, pre: &Captured, post: &ContentState) -> FsDecision {
		let now = Instant::now();
		let mut state = lock!(self.state);
		let id = pre.info.id;

		Self::prune(&mut state, now);

		// Parked: fold the fresh FS content into the record — unless disk
		// converged onto Studio's version, which resolves the conflict
		if let Some(index) = Self::parked_index(&state, id) {
			let studio_hash = state.parked[index].studio.content.as_ref().map(|content| content.hash);

			if studio_hash == Some(post.hash) {
				state.parked.remove(index);
				state.baselines.insert(id, post.hash);
				return FsDecision::NoChange;
			}

			let parked = &mut state.parked[index];
			parked.fs = Side::present(post.clone());

			// A recreated file turns an fs-deleted conflict back into a
			// plain both-edited one
			if parked.classification == Classification::FsDeletedStudioEdited {
				parked.classification = Classification::BothEdited;
			}

			return FsDecision::Conflict;
		}

		// First touch seeds the baseline from the pre-change tree state: the
		// tree is the state Studio hydrated (and §7 reconciled) against
		let baseline = *state.baselines.entry(id).or_insert(pre.content.hash);

		if let Some(pending) = state.pending.get(&id).copied() {
			if post.hash == pending.prior {
				// Disk reverted to the old agreement while the sent frame is
				// unconfirmed; propagate the revert and settle on the prior
				state.pending.remove(&id);
				state.baselines.insert(id, pending.prior);
				return FsDecision::Propagate;
			}

			let entry = state.pending.get_mut(&id).unwrap();
			entry.sent = post.hash;
			entry.at = now;

			return FsDecision::Propagate;
		}

		if post.hash == baseline {
			// Studio already holds this content
			return FsDecision::NoChange;
		}

		state.pending.insert(
			id,
			Pending {
				prior: baseline,
				sent: post.hash,
				at: now,
			},
		);

		FsDecision::Propagate
	}

	/// An instance disappeared from disk. `pre` is the state it had
	pub fn on_fs_removal(&self, pre: &Captured) -> FsDecision {
		let now = Instant::now();
		let mut state = lock!(self.state);
		let id = pre.info.id;

		Self::prune(&mut state, now);

		// Parked: the FS side of the conflict is now a deletion
		if let Some(index) = Self::parked_index(&state, id) {
			if !state.parked[index].studio.present {
				// Studio deleted it too — both sides agree it is gone
				state.parked.remove(index);
				state.baselines.remove(&id);
				state.pending.remove(&id);
				return FsDecision::NoChange;
			}

			let reclassified = {
				let parked = &mut state.parked[index];
				parked.fs = Side::absent();

				if parked.classification != Classification::FsDeletedStudioEdited {
					parked.classification = Classification::FsDeletedStudioEdited;
					true
				} else {
					false
				}
			};

			if reclassified {
				let parked = state.parked[index].clone();
				drop(state);
				self.notify(&parked);
			}

			return FsDecision::Conflict;
		}

		// Clean delete: propagate, but keep provenance for the race window —
		// hashes that only echo the deleted content must not resurrect it,
		// while a real concurrent Studio edit parks
		let mut hashes = vec![pre.content.hash];

		if let Some(baseline) = state.baselines.get(&id) {
			if !hashes.contains(baseline) {
				hashes.push(*baseline);
			}
		}

		if let Some(pending) = state.pending.get(&id) {
			if !hashes.contains(&pending.prior) {
				hashes.push(pending.prior);
			}
		}

		state.baselines.remove(&id);
		state.pending.remove(&id);
		state.tombstones.insert(
			id,
			Tombstone {
				hashes,
				info: pre.info.clone(),
				base: StudioApply {
					name: pre.content.name.clone(),
					class: pre.content.class.clone(),
					properties: pre.properties.clone(),
				},
				at: now,
			},
		);

		FsDecision::Propagate
	}

	// Studio-push decisions

	/// Studio created a new instance: always applies (the baseline is
	/// stamped by the caller from the tree once the write middleware ran)
	pub fn on_push_addition(&self, id: Ref) -> PushDecision {
		let mut state = lock!(self.state);

		Self::prune(&mut state, Instant::now());
		state.tombstones.remove(&id);

		PushDecision::Apply
	}

	/// Studio pushed an update for an instance the tree still has. `pre` is
	/// the current tree state, `pushed` the state Studio reports after the
	/// update, `apply` the raw Studio state kept if the op parks
	pub fn on_push_update(&self, pre: &Captured, pushed: &ContentState, apply: StudioApply) -> PushDecision {
		let now = Instant::now();
		let mut state = lock!(self.state);
		let id = pre.info.id;

		Self::prune(&mut state, now);

		// Parked: fold, or resolve by convergence when Studio caught up to
		// the disk content
		if let Some(index) = Self::parked_index(&state, id) {
			let fs_hash = state.parked[index].fs.content.as_ref().map(|content| content.hash);

			if fs_hash == Some(pushed.hash) {
				state.parked.remove(index);
				state.baselines.insert(id, pushed.hash);
				return PushDecision::NoChange;
			}

			let reclassified = {
				let parked = &mut state.parked[index];
				parked.studio = Side::present(pushed.clone());
				parked.studio_apply = Some(apply);

				if parked.classification == Classification::StudioDeletedFsEdited {
					parked.classification = Classification::BothEdited;
					true
				} else {
					false
				}
			};

			if reclassified {
				let parked = state.parked[index].clone();
				drop(state);
				self.notify(&parked);
			}

			return PushDecision::Conflict;
		}

		let baseline = *state.baselines.entry(id).or_insert(pre.content.hash);

		if let Some(pending) = state.pending.get(&id).copied() {
			if pushed.hash == pending.sent {
				// Studio reports exactly the content we sent — agreement
				state.pending.remove(&id);
				state.baselines.insert(id, pending.sent);
				return PushDecision::NoChange;
			}

			if pushed.hash == pending.prior {
				// Stale echo from before our frame applied; the sync frame
				// in flight will supersede it
				return PushDecision::NoChange;
			}

			// Both sides drifted from the last agreement inside the race
			// window: park, propagate nothing (Design §6.3)
			let parked = Self::park(
				&mut state,
				Classification::BothEdited,
				id,
				pre.info.clone(),
				Side::present(pre.content.clone()),
				Side::present(pushed.clone()),
				Some(apply),
			);

			if let Some(parked) = parked {
				drop(state);
				self.notify(&parked);
			}

			return PushDecision::Conflict;
		}

		if pushed.hash == pre.content.hash {
			// Bytes identical to disk — refresh the baseline
			state.baselines.insert(id, pushed.hash);
			return PushDecision::NoChange;
		}

		if baseline == pre.content.hash {
			// Disk still matches the last agreed state — safe to apply
			return PushDecision::Apply;
		}

		// Disk drifted from the baseline outside any known propagation —
		// fail closed rather than let the push silently win
		let parked = Self::park(
			&mut state,
			Classification::BothEdited,
			id,
			pre.info.clone(),
			Side::present(pre.content.clone()),
			Side::present(pushed.clone()),
			Some(apply),
		);

		if let Some(parked) = parked {
			drop(state);
			self.notify(&parked);
		}

		PushDecision::Conflict
	}

	/// Studio pushed an update for an instance the tree no longer has. A
	/// recent FS deletion (tombstone) turns a genuine edit into an
	/// `fs-deleted-studio-edited` conflict; a pure echo of the deleted
	/// content — or an id the daemon never knew — is dropped. The overlay
	/// (`name`/`properties`, whichever the push carried) is applied onto the
	/// tombstoned base state to reconstruct what Studio holds
	pub fn on_push_update_missing(&self, id: Ref, name: Option<&str>, properties: Option<&Properties>) -> PushDecision {
		let now = Instant::now();
		let mut state = lock!(self.state);

		Self::prune(&mut state, now);

		if let Some(index) = Self::parked_index(&state, id) {
			// Already parked (fs side deleted); fold the fresh Studio content
			let parked = &mut state.parked[index];

			if let Some(apply) = &mut parked.studio_apply {
				if let Some(name) = name {
					name.clone_into(&mut apply.name);
				}

				if let Some(properties) = properties {
					apply.properties = properties.clone();
				}

				let apply = apply.clone();
				parked.studio = Side::present(ContentState::new(&apply.name, &apply.class, &apply.properties));
			}

			return PushDecision::Conflict;
		}

		let Some(tombstone) = state.tombstones.get(&id).cloned() else {
			return PushDecision::NoChange;
		};

		let mut apply = tombstone.base.clone();

		if let Some(name) = name {
			name.clone_into(&mut apply.name);
		}

		if let Some(properties) = properties {
			apply.properties = properties.clone();
		}

		let pushed = ContentState::new(&apply.name, &apply.class, &apply.properties);

		if tombstone.hashes.contains(&pushed.hash) {
			// Studio merely still holds the deleted content; the removal
			// frame in flight will clear it
			return PushDecision::NoChange;
		}

		state.tombstones.remove(&id);

		let parked = Self::park(
			&mut state,
			Classification::FsDeletedStudioEdited,
			id,
			tombstone.info.clone(),
			Side::absent(),
			Side::present(pushed),
			Some(apply),
		);

		if let Some(parked) = parked {
			drop(state);
			self.notify(&parked);
		}

		PushDecision::Conflict
	}

	/// Studio removed an instance the tree still has
	pub fn on_push_removal(&self, pre: &Captured) -> PushDecision {
		let now = Instant::now();
		let mut state = lock!(self.state);
		let id = pre.info.id;

		Self::prune(&mut state, now);

		if let Some(index) = Self::parked_index(&state, id) {
			if !state.parked[index].fs.present {
				// FS deleted it earlier — both sides agree it is gone
				state.parked.remove(index);
				state.baselines.remove(&id);
				return PushDecision::NoChange;
			}

			let reclassified = {
				let parked = &mut state.parked[index];
				parked.studio = Side::absent();
				parked.studio_apply = None;

				if parked.classification != Classification::StudioDeletedFsEdited {
					parked.classification = Classification::StudioDeletedFsEdited;
					true
				} else {
					false
				}
			};

			if reclassified {
				let parked = state.parked[index].clone();
				drop(state);
				self.notify(&parked);
			}

			return PushDecision::Conflict;
		}

		let baseline = *state.baselines.entry(id).or_insert(pre.content.hash);

		if state.pending.contains_key(&id) || baseline != pre.content.hash {
			// Disk moved past the agreement — deleting it now would destroy
			// an edit Studio has not seen
			let parked = Self::park(
				&mut state,
				Classification::StudioDeletedFsEdited,
				id,
				pre.info.clone(),
				Side::present(pre.content.clone()),
				Side::absent(),
				None,
			);

			if let Some(parked) = parked {
				drop(state);
				self.notify(&parked);
			}

			return PushDecision::Conflict;
		}

		PushDecision::Apply
	}

	/// Studio removed an instance the tree does not have either. A parked
	/// `fs-deleted-studio-edited` conflict dissolves — both sides now agree
	/// the instance is gone
	pub fn on_push_removal_missing(&self, id: Ref) -> PushDecision {
		let mut state = lock!(self.state);

		Self::prune(&mut state, Instant::now());

		if let Some(index) = Self::parked_index(&state, id) {
			state.parked.remove(index);
		}

		state.tombstones.remove(&id);
		state.baselines.remove(&id);

		PushDecision::NoChange
	}
}

impl Default for ConflictEngine {
	fn default() -> Self {
		Self::new()
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use rbx_dom_weak::{ustr, HashMapExt, UstrMap};

	fn props(source: &str) -> Properties {
		let mut properties = UstrMap::new();
		properties.insert(ustr("Source"), rbx_dom_weak::types::Variant::String(source.to_owned()));
		properties
	}

	fn content(source: &str) -> ContentState {
		ContentState::new("Hello", "ModuleScript", &props(source))
	}

	fn captured(id: Ref, source: &str) -> Captured {
		Captured {
			info: InstanceInfo {
				id,
				parent: Ref::new(),
				instance_path: "ReplicatedStorage/Hello".into(),
				rel_path: Some("src/Hello.luau".into()),
			},
			content: content(source),
			properties: props(source),
		}
	}

	fn apply(source: &str) -> StudioApply {
		StudioApply {
			name: "Hello".into(),
			class: "ModuleScript".into(),
			properties: props(source),
		}
	}

	#[test]
	fn content_hash_normalizes_source_and_strips_argon_empty() {
		let crlf = content("a\r\nb");
		let lf = content("a\nb");
		assert_eq!(crlf.hash, lf.hash);
		assert_eq!(crlf.source().as_deref(), Some("a\nb"));

		let mut with_marker = props("x");
		with_marker.insert(ustr("ArgonEmpty"), rbx_dom_weak::types::Variant::Bool(true));
		let marked = ContentState::new("Hello", "ModuleScript", &with_marker);
		assert_eq!(marked.hash, content("x").hash);

		// Different name or class changes the hash
		assert_ne!(
			ContentState::new("Other", "ModuleScript", &props("x")).hash,
			content("x").hash
		);
		assert_ne!(
			ContentState::new("Hello", "Script", &props("x")).hash,
			content("x").hash
		);
	}

	#[test]
	fn fs_update_propagates_and_echo_is_no_change() {
		let engine = ConflictEngine::new();
		let id = Ref::new();
		let pre = captured(id, "agreed");

		engine.stamp(id, pre.content.hash);

		// Same content as the baseline → NoChange
		assert_eq!(engine.on_fs_update(&pre, &content("agreed")), FsDecision::NoChange);

		// Real change → Propagate
		assert_eq!(engine.on_fs_update(&pre, &content("edited")), FsDecision::Propagate);
	}

	#[test]
	fn racing_push_inside_window_parks_both_edited() {
		let engine = ConflictEngine::new();
		let id = Ref::new();
		let pre = captured(id, "agreed");

		engine.stamp(id, pre.content.hash);
		assert_eq!(engine.on_fs_update(&pre, &content("fs-edit")), FsDecision::Propagate);

		// The tree now holds the propagated state
		let tree_now = captured(id, "fs-edit");

		// Studio races with its own edit based on the old agreement → park
		let decision = engine.on_push_update(&tree_now, &content("studio-edit"), apply("studio-edit"));
		assert_eq!(decision, PushDecision::Conflict);

		let parked = engine.list();
		assert_eq!(parked.len(), 1);
		assert_eq!(parked[0].classification, Classification::BothEdited);
		assert_eq!(parked[0].kind(), "script");
		assert_eq!(
			parked[0].fs.content.as_ref().unwrap().source().as_deref(),
			Some("fs-edit")
		);
		assert_eq!(
			parked[0].studio.content.as_ref().unwrap().source().as_deref(),
			Some("studio-edit")
		);
	}

	#[test]
	fn push_matching_sent_content_commits_the_baseline() {
		let engine = ConflictEngine::new();
		let id = Ref::new();
		let pre = captured(id, "agreed");

		engine.stamp(id, pre.content.hash);
		engine.on_fs_update(&pre, &content("fs-edit"));

		let tree_now = captured(id, "fs-edit");

		// Studio reports exactly what we sent → agreement, no conflict
		assert_eq!(
			engine.on_push_update(&tree_now, &content("fs-edit"), apply("fs-edit")),
			PushDecision::NoChange
		);
		assert_eq!(engine.baseline(id), Some(content("fs-edit").hash));

		// A later Studio edit now applies cleanly
		assert_eq!(
			engine.on_push_update(&tree_now, &content("studio-later"), apply("studio-later")),
			PushDecision::Apply
		);
	}

	#[test]
	fn stale_echo_of_prior_content_is_dropped() {
		let engine = ConflictEngine::new();
		let id = Ref::new();
		let pre = captured(id, "agreed");

		engine.stamp(id, pre.content.hash);
		engine.on_fs_update(&pre, &content("fs-edit"));

		let tree_now = captured(id, "fs-edit");

		assert_eq!(
			engine.on_push_update(&tree_now, &content("agreed"), apply("agreed")),
			PushDecision::NoChange
		);
		assert!(engine.list().is_empty());
	}

	#[test]
	fn pending_window_expiry_commits_the_baseline() {
		let engine = ConflictEngine::new();
		engine.set_race_window(Duration::ZERO);

		let id = Ref::new();
		let pre = captured(id, "agreed");

		engine.stamp(id, pre.content.hash);
		engine.on_fs_update(&pre, &content("fs-edit"));

		std::thread::sleep(Duration::from_millis(5));

		// The window expired: the sent content is the new agreement, so a
		// later Studio edit applies instead of parking
		let tree_now = captured(id, "fs-edit");
		assert_eq!(
			engine.on_push_update(&tree_now, &content("studio-later"), apply("studio-later")),
			PushDecision::Apply
		);
		assert_eq!(engine.baseline(id), Some(content("fs-edit").hash));
	}

	#[test]
	fn push_parks_when_disk_drifted_without_a_pending_window() {
		let engine = ConflictEngine::new();
		let id = Ref::new();

		// Baseline says "agreed" but the tree already holds "fs-edit"
		engine.stamp(id, content("agreed").hash);

		let tree_now = captured(id, "fs-edit");
		assert_eq!(
			engine.on_push_update(&tree_now, &content("studio-edit"), apply("studio-edit")),
			PushDecision::Conflict
		);
	}

	#[test]
	fn push_applies_when_disk_matches_baseline() {
		let engine = ConflictEngine::new();
		let id = Ref::new();
		let pre = captured(id, "agreed");

		engine.stamp(id, pre.content.hash);

		assert_eq!(
			engine.on_push_update(&pre, &content("studio-edit"), apply("studio-edit")),
			PushDecision::Apply
		);
	}

	#[test]
	fn push_without_baseline_seeds_from_tree_and_applies() {
		let engine = ConflictEngine::new();
		let id = Ref::new();
		let pre = captured(id, "current");

		// No baseline (fresh daemon): tree state is treated as the agreement
		assert_eq!(
			engine.on_push_update(&pre, &content("studio-edit"), apply("studio-edit")),
			PushDecision::Apply
		);
	}

	#[test]
	fn identical_push_refreshes_baseline() {
		let engine = ConflictEngine::new();
		let id = Ref::new();
		let pre = captured(id, "same");

		assert_eq!(
			engine.on_push_update(&pre, &content("same"), apply("same")),
			PushDecision::NoChange
		);
		assert_eq!(engine.baseline(id), Some(content("same").hash));
	}

	#[test]
	fn fs_delete_then_studio_edit_parks_fs_deleted_studio_edited() {
		let engine = ConflictEngine::new();
		let id = Ref::new();
		let pre = captured(id, "agreed");

		engine.stamp(id, pre.content.hash);
		assert_eq!(engine.on_fs_removal(&pre), FsDecision::Propagate);

		// Echo of the deleted content is dropped
		assert_eq!(
			engine.on_push_update_missing(id, None, Some(&props("agreed"))),
			PushDecision::NoChange
		);

		// A genuine Studio edit of the deleted instance parks
		assert_eq!(
			engine.on_push_update_missing(id, None, Some(&props("studio-edit"))),
			PushDecision::Conflict
		);

		let parked = engine.list();
		assert_eq!(parked.len(), 1);
		assert_eq!(parked[0].classification, Classification::FsDeletedStudioEdited);
		assert!(!parked[0].fs.present);
		assert!(parked[0].studio.present);
	}

	#[test]
	fn studio_delete_racing_fs_edit_parks_studio_deleted_fs_edited() {
		let engine = ConflictEngine::new();
		let id = Ref::new();
		let pre = captured(id, "agreed");

		engine.stamp(id, pre.content.hash);
		engine.on_fs_update(&pre, &content("fs-edit"));

		let tree_now = captured(id, "fs-edit");
		assert_eq!(engine.on_push_removal(&tree_now), PushDecision::Conflict);

		let parked = engine.list();
		assert_eq!(parked.len(), 1);
		assert_eq!(parked[0].classification, Classification::StudioDeletedFsEdited);
		assert!(parked[0].fs.present);
		assert!(!parked[0].studio.present);
	}

	#[test]
	fn clean_studio_delete_applies() {
		let engine = ConflictEngine::new();
		let id = Ref::new();
		let pre = captured(id, "agreed");

		engine.stamp(id, pre.content.hash);
		assert_eq!(engine.on_push_removal(&pre), PushDecision::Apply);
	}

	#[test]
	fn fs_update_folds_into_parked_conflict() {
		let engine = ConflictEngine::new();
		let id = Ref::new();

		engine.stamp(id, content("agreed").hash);
		engine.on_push_update(&captured(id, "fs-edit"), &content("studio-edit"), apply("studio-edit"));
		assert!(engine.is_parked(id));

		// Further FS edits update the parked FS side and stay withheld
		assert_eq!(
			engine.on_fs_update(&captured(id, "fs-edit"), &content("fs-edit-2")),
			FsDecision::Conflict
		);
		assert_eq!(
			engine.list()[0].fs.content.as_ref().unwrap().source().as_deref(),
			Some("fs-edit-2")
		);

		// Disk converging onto Studio's version resolves the conflict
		assert_eq!(
			engine.on_fs_update(&captured(id, "fs-edit-2"), &content("studio-edit")),
			FsDecision::NoChange
		);
		assert!(!engine.is_parked(id));
		assert_eq!(engine.baseline(id), Some(content("studio-edit").hash));
	}

	#[test]
	fn fs_delete_of_parked_conflict_reclassifies() {
		let engine = ConflictEngine::new();
		let id = Ref::new();

		engine.stamp(id, content("agreed").hash);
		engine.on_push_update(&captured(id, "fs-edit"), &content("studio-edit"), apply("studio-edit"));

		assert_eq!(engine.on_fs_removal(&captured(id, "fs-edit")), FsDecision::Conflict);
		assert_eq!(engine.list()[0].classification, Classification::FsDeletedStudioEdited);

		// Studio deleting it too dissolves the conflict
		assert_eq!(engine.on_push_removal(&captured(id, "fs-edit")), PushDecision::NoChange);
		assert!(engine.list().is_empty());
	}

	#[test]
	fn park_limit_drops_records_but_keeps_excluding() {
		let engine = ConflictEngine::new();
		engine.set_park_limit(1);

		let first = Ref::new();
		let second = Ref::new();

		engine.stamp(first, content("agreed").hash);
		engine.stamp(second, content("agreed").hash);

		assert_eq!(
			engine.on_push_update(&captured(first, "fs-1"), &content("studio-1"), apply("studio-1")),
			PushDecision::Conflict
		);
		// Beyond the limit: still Conflict (excluded), but not listed
		assert_eq!(
			engine.on_push_update(&captured(second, "fs-2"), &content("studio-2"), apply("studio-2")),
			PushDecision::Conflict
		);

		assert_eq!(engine.list().len(), 1);
	}

	#[test]
	fn take_and_restore_round_trip() {
		let engine = ConflictEngine::new();
		let id = Ref::new();

		engine.stamp(id, content("agreed").hash);
		engine.on_push_update(&captured(id, "fs-edit"), &content("studio-edit"), apply("studio-edit"));

		let record = engine.take(&engine.list()[0].id).unwrap();
		assert!(engine.list().is_empty());
		assert!(engine.take("c999").is_none());

		engine.restore(record);
		assert_eq!(engine.list().len(), 1);
	}

	#[test]
	fn notifier_fires_on_new_parks() {
		use std::sync::atomic::{AtomicUsize, Ordering};
		use std::sync::Arc;

		let engine = ConflictEngine::new();
		let count = Arc::new(AtomicUsize::new(0));

		let seen = count.clone();
		engine.set_notifier(Box::new(move |parked| {
			assert_eq!(parked.classification, Classification::BothEdited);
			seen.fetch_add(1, Ordering::SeqCst);
		}));

		let id = Ref::new();
		engine.stamp(id, content("agreed").hash);
		engine.on_push_update(&captured(id, "fs-edit"), &content("studio-edit"), apply("studio-edit"));

		assert_eq!(count.load(Ordering::SeqCst), 1);
	}
}
