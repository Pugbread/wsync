use anyhow::Result;
use colored::Colorize;
use crossbeam_channel::{select, Sender};
use log::{debug, error, info, trace, warn};
use rbx_dom_weak::types::Ref;
use serde::Deserialize;
use std::{
	collections::HashMap,
	path::Path,
	sync::{Arc, Mutex},
	thread::Builder,
};

use super::{
	changes::Changes,
	conflict::{self, Captured, ConflictEngine, ContentState, FsDecision, PushDecision, StudioApply},
	queue::Queue,
	snapshot::{Snapshot, UpdatedSnapshot},
	tree::Tree,
};
use crate::{
	config::Config,
	constants::{BLACKLISTED_PATHS, RUNTIME_DIRS},
	lock, logger,
	project::{Project, ProjectDetails},
	server, stats,
	vfs::{Vfs, VfsEvent},
	wsync_error,
};

pub mod read;
pub mod write;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WriteRequest {
	pub changes: Changes,
	pub client_id: u32,
}

/// Outcome of applying one client write batch. `applied` counts operations
/// that went through the write middleware (plus no-op operations whose
/// content was already agreed), `conflicts` counts operations the conflict
/// engine parked and excluded from application, `skipped` counts operations
/// left unattempted after the first error (batches abort at the first
/// failure, matching the long-poll path), and `errors` carries that failure
#[derive(Debug, Clone)]
pub struct WriteResult {
	pub applied: usize,
	pub skipped: usize,
	pub conflicts: usize,
	pub errors: Vec<String>,
}

impl WriteResult {
	pub fn ok(&self) -> bool {
		self.errors.is_empty()
	}
}

struct QueuedWrite {
	request: WriteRequest,
	result: Option<tokio::sync::oneshot::Sender<WriteResult>>,
}

pub struct Processor {
	writer: Sender<QueuedWrite>,
}

impl Processor {
	pub fn new(
		queue: Arc<Queue>,
		tree: Arc<Mutex<Tree>>,
		vfs: Arc<Vfs>,
		project: Arc<Mutex<Project>>,
		conflicts: Arc<ConflictEngine>,
	) -> Self {
		let handler = Arc::new(Handler {
			queue,
			tree,
			vfs: vfs.clone(),
			project,
			conflicts,
		});

		let handler = handler.clone();
		let (sender, receiver) = crossbeam_channel::unbounded();

		Builder::new()
			.name("processor".into())
			.spawn(move || -> Result<()> {
				let vfs_receiver = vfs.receiver();
				let client_receiver = receiver;

				loop {
					select! {
						recv(vfs_receiver) -> event => {
							handler.on_vfs_event(event?);
						}
						recv(client_receiver) -> request => {
							vfs.pause();
							handler.on_client_event(request?);
							vfs.resume();
						}
					}
				}
			})
			.unwrap();

		Self { writer: sender }
	}

	pub fn write(&self, request: WriteRequest) {
		self.writer.send(QueuedWrite { request, result: None }).unwrap();
	}

	/// Queues a client write and reports its `WriteResult` on the given
	/// channel once the processor thread has applied it (used by the WS
	/// `push` path to answer with a `push-result` frame)
	pub fn write_with_result(&self, request: WriteRequest, result: tokio::sync::oneshot::Sender<WriteResult>) {
		self.writer
			.send(QueuedWrite {
				request,
				result: Some(result),
			})
			.unwrap();
	}
}

struct Handler {
	queue: Arc<Queue>,
	tree: Arc<Mutex<Tree>>,
	vfs: Arc<Vfs>,
	project: Arc<Mutex<Project>>,
	conflicts: Arc<ConflictEngine>,
}

impl Handler {
	#[profiling::function]
	fn on_vfs_event(&self, event: VfsEvent) {
		profiling::start_frame!();

		trace!("Received VFS event: {event:?}");

		let engine_on = Config::new().conflict_engine;

		let mut tree = lock!(self.tree);
		let path = event.path();

		let changes = {
			if BLACKLISTED_PATHS.iter().any(|blacklisted| path.ends_with(blacklisted)) {
				trace!("Processing of {path:?} aborted: blacklisted");
				return;
			}

			// Events inside WSync's runtime directories (backups, staging,
			// artifacts) never concern the projection — skip before any
			// tree walk so bulk staging churn stays free (Design §12)
			if path
				.components()
				.any(|component| RUNTIME_DIRS.iter().any(|dir| component.as_os_str() == *dir))
			{
				trace!("Processing of {path:?} aborted: runtime directory");
				return;
			}

			let ids = {
				let mut current_path = path;

				loop {
					if let Some(ids) = tree.get_ids(current_path) {
						break ids.to_owned();
					}

					match current_path.parent() {
						Some(parent) => current_path = parent,
						None => {
							trace!("No ID found for path {path:?}");
							return;
						}
					}
				}
			};

			// Lock order matches the rest of the daemon: tree before project
			let workspace_dir = lock!(self.project).workspace_dir.clone();

			let mut changes = Changes::new();
			let mut pre_states: HashMap<Ref, Captured> = HashMap::new();

			for id in ids {
				// The read path mutates the tree while diffing, so the
				// conflict engine's pre-change states are captured first —
				// the walk covers exactly the re-snapshotted subtree
				if engine_on {
					for captured in conflict::capture_subtree(&tree, id, &workspace_dir) {
						pre_states.insert(captured.info.id, captured);
					}
				}

				if let Some(processed) = read::process_changes(id, &mut tree, &self.vfs) {
					changes.extend(processed);
				}
			}

			if engine_on {
				self.gate_fs_changes(changes, &tree, &pre_states)
			} else {
				changes
			}
		};

		if !changes.is_empty() {
			stats::files_synced(changes.total() as u32);

			let result = self.queue.push(server::SyncChanges(changes), None);

			match result {
				Ok(()) => trace!("Added changes to the queue"),
				Err(err) => {
					error!("Failed to add changes to the queue: {err}");
				}
			}
		} else {
			trace!("No changes detected when processing path: {path:?}");
		}

		let mut project = lock!(self.project);

		if project.path == path {
			if let VfsEvent::Write(_) = event {
				debug!("Project file was modified. Reloading project..");

				let old_details = ProjectDetails::from_project(&project, &tree);

				match project.reload() {
					Ok(project) => {
						info!("Project reloaded");

						let details = ProjectDetails::from_project(project, &tree);

						if details == old_details {
							return;
						}

						match self.queue.push(server::SyncDetails(details), None) {
							Ok(()) => trace!("Project details synced"),
							Err(err) => warn!("Failed to sync project details: {err}"),
						}
					}
					Err(err) => error!("Failed to reload project: {err}"),
				}
			} else if let VfsEvent::Delete(_) = event {
				wsync_error!("Warning! Top level project file was deleted. This might cause unexpected behavior. Skipping processing of changes!");
			}
		}
	}

	#[profiling::function]
	fn on_client_event(&self, queued: QueuedWrite) {
		profiling::start_frame!();

		let QueuedWrite { request, result: reply } = queued;
		let changes = request.changes;
		let client_id = request.client_id;
		let total = changes.total();

		let engine_on = Config::new().conflict_engine;
		// Daemon-internal writes (conflict resolution) carry a reserved
		// client id: the resolution route already decided, so the engine's
		// decisions are bypassed while baselines still re-stamp
		let bypass = client_id == conflict::RESOLUTION_CLIENT_ID;

		trace!("Received client event: {total:?} changes");

		// With the conflict engine on, per-op decisions replace the coarse
		// count-threshold prompt (Design §6.3 supersedes §6.4 for pushes);
		// engine-off mode keeps the inherited prompt exactly as it was
		if !engine_on && !bypass && total > Config::new().changes_threshold {
			let accept = logger::prompt(
				&format!(
					"You are about to apply {}, {} and {}. Do you want to continue?",
					format!("{} additions", changes.additions.len()).bold().green(),
					format!("{} updates", changes.updates.len()).bold().blue(),
					format!("{} removals", changes.removals.len()).bold().red(),
				),
				true,
			);

			if !accept {
				trace!("Aborted applying client event! {total} changes were not applied");

				match self.queue.disconnect("Client and server got out of sync!", client_id) {
					Ok(()) => trace!("Client {client_id} disconnected"),
					Err(err) => warn!("Failed to disconnect client: {err}"),
				}

				if let Some(reply) = reply {
					reply
						.send(WriteResult {
							applied: 0,
							skipped: total,
							conflicts: 0,
							errors: vec!["Changes threshold declined; client and server are out of sync".into()],
						})
						.ok();
				}

				return;
			}
		}

		let mut tree = lock!(self.tree);
		// Lock order matches the rest of the daemon: tree before project
		let workspace_dir = lock!(self.project).workspace_dir.clone();

		let mut applied = 0;
		let mut conflicts = 0;

		let result = || -> Result<()> {
			for snapshot in changes.additions {
				let id = snapshot.id;

				if engine_on && !bypass {
					self.conflicts.on_push_addition(id);
				}

				write::apply_addition(snapshot, &mut tree, &self.vfs)?;

				if engine_on {
					self.stamp_subtree(&tree, id);
				}

				applied += 1;
			}

			for snapshot in changes.updates {
				if engine_on && !bypass {
					match self.decide_push_update(&tree, &snapshot, &workspace_dir) {
						PushDecision::Apply => {}
						PushDecision::NoChange => {
							applied += 1;
							continue;
						}
						PushDecision::Conflict => {
							conflicts += 1;
							continue;
						}
					}
				}

				let id = snapshot.id;

				write::apply_update(snapshot, &mut tree, &self.vfs)?;

				if engine_on {
					self.stamp_instance(&tree, id);
				}

				applied += 1;
			}

			for id in changes.removals {
				if engine_on && !bypass {
					match self.decide_push_removal(&tree, id, &workspace_dir) {
						PushDecision::Apply => {}
						PushDecision::NoChange => {
							applied += 1;
							continue;
						}
						PushDecision::Conflict => {
							conflicts += 1;
							continue;
						}
					}
				}

				// Capture the subtree before the removal cascades so every
				// affected baseline is forgotten
				let refs = if engine_on {
					conflict::subtree_refs(&tree, id)
				} else {
					Vec::new()
				};

				write::apply_removal(id, &mut tree, &self.vfs)?;

				for id in refs {
					self.conflicts.forget(id);
				}

				applied += 1;
			}

			Ok(())
		}();

		if conflicts > 0 {
			debug!("Parked {conflicts} conflicting operations from client {client_id}");
		}

		let errors = match result {
			Ok(()) => {
				trace!("Changes applied successfully");
				Vec::new()
			}
			Err(err) => {
				error!("Failed to apply changes: {err}");
				vec![format!("{err:#}")]
			}
		};

		if let Some(reply) = reply {
			// The operation that errored is reported through `errors`, not
			// counted as skipped; skipped covers the unattempted remainder
			let errored = usize::from(!errors.is_empty());

			reply
				.send(WriteResult {
					applied,
					skipped: total - applied - conflicts - errored,
					conflicts,
					errors,
				})
				.ok();
		}

		self.queue.push(server::SyncbackChanges(), Some(0)).ok();
	}

	// Conflict-engine hooks

	/// Routes an FS-derived change set through the conflict engine: parked
	/// and no-change operations are withheld from Studio, everything else
	/// propagates (Design §6.3). The tree has already been mutated by the
	/// read path; `pre_states` holds the captured pre-change states
	fn gate_fs_changes(&self, changes: Changes, tree: &Tree, pre_states: &HashMap<Ref, Captured>) -> Changes {
		let mut gated = Changes::new();

		for addition in changes.additions {
			// New instances always propagate; their whole subtree stamps as
			// agreed (the plugin cannot race refs it has never seen)
			fn stamp_added(engine: &ConflictEngine, snapshot: &Snapshot) {
				engine.on_fs_addition(
					snapshot.id,
					&ContentState::new(&snapshot.name, &snapshot.class, &snapshot.properties),
				);

				for child in &snapshot.children {
					stamp_added(engine, child);
				}
			}

			self.conflicts.on_fs_addition(
				addition.id,
				&ContentState::new(&addition.name, &addition.class, &addition.properties),
			);

			for child in &addition.children {
				stamp_added(&self.conflicts, child);
			}

			gated.additions.push(addition);
		}

		for update in changes.updates {
			// Meta-only updates carry no content; pass them through
			if update.name.is_none() && update.class.is_none() && update.properties.is_none() {
				gated.updates.push(update);
				continue;
			}

			let (Some(pre), Some(instance)) = (pre_states.get(&update.id), tree.get_instance(update.id)) else {
				gated.updates.push(update);
				continue;
			};

			let post = ContentState::new(&instance.name, &instance.class, &instance.properties);

			match self.conflicts.on_fs_update(pre, &post) {
				FsDecision::Propagate => gated.updates.push(update),
				FsDecision::NoChange => trace!("Withholding no-change FS update for {:?}", update.id),
				FsDecision::Conflict => debug!("Parked FS update for {:?} (conflict)", update.id),
			}
		}

		for removal in changes.removals {
			let Some(pre) = pre_states.get(&removal) else {
				gated.removals.push(removal);
				continue;
			};

			match self.conflicts.on_fs_removal(pre) {
				FsDecision::Propagate => gated.removals.push(removal),
				FsDecision::NoChange => trace!("Withholding no-change FS removal for {removal:?}"),
				FsDecision::Conflict => debug!("Parked FS removal for {removal:?} (conflict)"),
			}
		}

		gated
	}

	/// Decides one pushed update against the current tree state
	fn decide_push_update(&self, tree: &Tree, snapshot: &UpdatedSnapshot, workspace_dir: &Path) -> PushDecision {
		// Nothing content-bearing to decide on
		if snapshot.name.is_none() && snapshot.properties.is_none() {
			return PushDecision::Apply;
		}

		match conflict::capture_instance(tree, snapshot.id, workspace_dir) {
			Some(pre) => {
				let name = snapshot.name.as_deref().unwrap_or(&pre.content.name);
				let properties = snapshot.properties.as_ref().unwrap_or(&pre.properties);
				let pushed = ContentState::new(name, &pre.content.class, properties);

				let apply = StudioApply {
					name: name.to_owned(),
					class: pre.content.class.clone(),
					properties: properties.clone(),
				};

				self.conflicts.on_push_update(&pre, &pushed, apply)
			}
			None => self.conflicts.on_push_update_missing(
				snapshot.id,
				snapshot.name.as_deref(),
				snapshot.properties.as_ref(),
			),
		}
	}

	/// Decides one pushed removal against the current tree state
	fn decide_push_removal(&self, tree: &Tree, id: Ref, workspace_dir: &Path) -> PushDecision {
		match conflict::capture_instance(tree, id, workspace_dir) {
			Some(pre) => self.conflicts.on_push_removal(&pre),
			None => self.conflicts.on_push_removal_missing(id),
		}
	}

	/// Re-stamps the baseline of one instance from its post-apply tree state
	fn stamp_instance(&self, tree: &Tree, id: Ref) {
		if let Some(instance) = tree.get_instance(id) {
			let content = ContentState::new(&instance.name, &instance.class, &instance.properties);

			self.conflicts.stamp(id, content.hash);
		}
	}

	/// Re-stamps baselines for an applied subtree (Studio additions carry
	/// their children nested)
	fn stamp_subtree(&self, tree: &Tree, id: Ref) {
		for id in conflict::subtree_refs(tree, id) {
			self.stamp_instance(tree, id);
		}
	}
}
