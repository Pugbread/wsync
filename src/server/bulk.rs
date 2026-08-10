//! Bulk divergence application (Design §7.4).
//!
//! **Keep Studio** is a server-driven pull with a fenced apply, per synced
//! root: the daemon pulls the divergent subtree from the plugin
//! (`read_subtree` for structure with script sources elided, chunked
//! `source_read` parts verified against the claimed SHA-256), materializes
//! the projected file tree through the regular write middleware into a
//! same-volume staging directory, then — with the VFS paused — verifies the
//! fenced disk generation did not move, swaps `live → backup` and
//! `stage → live`, refreshes the daemon tree from the swapped root (echo
//! suppressed: Studio already holds this state) and stamps baselines.
//! Roots commit in sequence; a failed root terminates the transfer with a
//! replayable partial receipt and earlier committed roots are not rolled
//! back.
//!
//! **Keep Disk** replays the staged entries over the live sync channel,
//! bracketed by one explicit plugin transaction
//! (`transaction_begin`/`transaction_finish`) so a successful pull remains
//! one Studio Undo.

use axum::{
	http::StatusCode,
	response::{IntoResponse, Json, Response},
};
use base64::Engine;
use chrono::{NaiveDateTime, Utc};
use log::{debug, info, warn};
use path_clean::PathClean;
use rbx_dom_weak::{types::Variant, ustr};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::{
	collections::{BTreeMap, BTreeSet, HashMap},
	fs,
	path::{Path, PathBuf},
	time::Duration,
};

use crate::{
	constants::{
		BACKUPS_DIR, BACKUP_COMPLETE_MARKER, BACKUP_KEEP_COUNT, BACKUP_KEEP_DAYS, BULK_OP_TIMEOUT_MS,
		BULK_ROOT_MAX_BYTES, BULK_SCRIPT_MAX_BYTES, BULK_TRANSFER_MAX_BYTES, REQUEST_DEFAULT_TIMEOUT_MS,
		SOURCE_READ_CHUNK_BYTES,
	},
	core::{
		changes::Changes,
		conflict::{self, ContentState},
		meta::{Context, Meta, Source, SourceEntry, SourceKind},
		processor::{read, write},
		snapshot::Snapshot,
		tree::Tree,
	},
	middleware::dir,
	server::{
		ws::frames::{self, codes, OpError, ServerFrame},
		AppState, DIRECTION_DISK_TO_STUDIO,
	},
	util,
	vfs::Vfs,
	Properties,
};

/// Filesystem-safe RFC 3339-shaped UTC stamp used for backup directory names
/// (`:` is replaced by `-` so the names stay valid on every platform)
const BACKUP_STAMP_FORMAT: &str = "%Y-%m-%dT%H-%M-%SZ";

/// Staging subdirectory inside a transfer's backup directory. Staged roots
/// are renamed out of it during the swap; leftovers mean the transfer never
/// completed and the directory is exempt from pruning
const STAGE_DIR: &str = ".stage";

// Plugin request plumbing

/// A failed plugin call, carrying the protocol error code so callers can
/// distinguish e.g. `NOT_FOUND` from transport failures
#[derive(Debug)]
pub struct PluginCallError {
	pub code: String,
	pub message: String,
}

impl PluginCallError {
	fn transport(message: impl Into<String>) -> Self {
		Self {
			code: codes::PLUGIN_ERROR.to_owned(),
			message: message.into(),
		}
	}
}

impl std::fmt::Display for PluginCallError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "{} ({})", self.message, self.code)
	}
}

/// Sends one request to the connected WS plugin and awaits its response
pub async fn plugin_request(
	state: &AppState,
	op: &str,
	args: Value,
	timeout_ms: u64,
) -> Result<Value, PluginCallError> {
	let frames = state.ws.plugin_frames().map_err(PluginCallError::transport)?;

	let request_id = state.ws.next_request_id();
	let receiver = state.ws.register_request(request_id);

	let frame = ServerFrame::Request {
		request_id,
		op: op.to_owned(),
		args,
		timeout_ms,
	};

	if frames.try_send(frame.to_text()).is_err() {
		state.ws.abort_request(request_id);

		return Err(PluginCallError::transport(
			"Plugin outbound queue is overloaded or the plugin disconnected",
		));
	}

	match tokio::time::timeout(Duration::from_millis(timeout_ms), receiver).await {
		Ok(Ok(response)) => {
			if response.ok {
				Ok(response.value.unwrap_or(Value::Null))
			} else {
				let error = response.error.unwrap_or_else(|| OpError {
					code: codes::PLUGIN_ERROR.to_owned(),
					message: "Plugin reported failure without error detail".to_owned(),
				});

				Err(PluginCallError {
					code: error.code,
					message: error.message,
				})
			}
		}
		Ok(Err(_)) => Err(PluginCallError::transport(
			"Studio plugin disconnected before answering",
		)),
		Err(_) => {
			state.ws.abort_request(request_id);

			Err(PluginCallError {
				code: codes::TIMEOUT.to_owned(),
				message: format!("No plugin response to {op} within {timeout_ms} ms"),
			})
		}
	}
}

// Keep Disk: bracketed sync-channel replay

/// Sends a fire-and-forget request frame (used for the transaction bracket:
/// ordering on the single FIFO plugin channel is the guarantee; the plugin's
/// response is routed and dropped)
fn send_bracket_op(state: &AppState, frames: &tokio::sync::mpsc::Sender<String>, op: &str, args: Value) -> bool {
	let request_id = state.ws.next_request_id();

	// Register (and drop) the response slot so the plugin's answer resolves
	// silently instead of logging an unknown-request warning
	drop(state.ws.register_request(request_id));

	let frame = ServerFrame::Request {
		request_id,
		op: op.to_owned(),
		args,
		timeout_ms: REQUEST_DEFAULT_TIMEOUT_MS,
	};

	let sent = frames.try_send(frame.to_text()).is_ok();

	if !sent {
		state.ws.abort_request(request_id);
	}

	sent
}

/// Replays a Keep-Disk change set directly over the plugin's frame channel,
/// bracketed by `transaction_begin` / `transaction_finish` so the whole pull
/// lands as one Studio recording (Design §7.4-B). Any frame that fails to
/// enqueue aborts the apply with `transaction_finish {commit: false}`
pub fn replay_disk_bracketed(state: &AppState, choice_id: &str, changes: Changes) -> Result<(), String> {
	let frames = state.ws.plugin_frames().map_err(str::to_owned)?;

	if !send_bracket_op(
		state,
		&frames,
		"transaction_begin",
		json!({ "name": format!("WSync: Pull {choice_id}") }),
	) {
		return Err("Failed to enqueue transaction_begin: the plugin queue is overloaded or gone".into());
	}

	let activity = frames::Event::sync_activity(DIRECTION_DISK_TO_STUDIO, &changes);

	for frame in frames::sync_frames(changes) {
		if frames.try_send(frame).is_err() {
			send_bracket_op(state, &frames, "transaction_finish", json!({ "commit": false }));

			return Err(
				"Failed to enqueue a sync frame: the plugin queue is overloaded or gone; the transaction was aborted"
					.into(),
			);
		}
	}

	if !send_bracket_op(state, &frames, "transaction_finish", json!({ "commit": true })) {
		return Err("Failed to enqueue transaction_finish: the plugin queue is overloaded or gone".into());
	}

	state.ws.emit(activity);

	Ok(())
}

// Keep Studio: pulled snapshot decoding

/// A claimed script source from `read_subtree`'s `snapshot.sources` map
#[derive(Debug, Clone)]
struct SourceClaim {
	bytes: u64,
	sha256: String,
}

/// One node of the pulled Studio subtree (AddedSnapshot shape with script
/// sources elided)
#[derive(Debug)]
struct PulledNode {
	id: String,
	name: String,
	class: String,
	properties: Map<String, Value>,
	children: Vec<PulledNode>,
}

fn parse_pulled_node(value: &Value, at: &str) -> Result<PulledNode, String> {
	let id = value
		.get("id")
		.and_then(Value::as_str)
		.ok_or_else(|| format!("{at}: missing id"))?;

	util::parse_ref(id).map_err(|err| format!("{at}: {err}"))?;

	let name = value
		.get("name")
		.and_then(Value::as_str)
		.ok_or_else(|| format!("{at}: missing name"))?;

	let class = value
		.get("class")
		.and_then(Value::as_str)
		.ok_or_else(|| format!("{at}: missing class"))?;

	let properties = match value.get("properties") {
		None | Some(Value::Null) => Map::new(),
		Some(Value::Object(map)) => map.clone(),
		Some(_) => return Err(format!("{at}: properties must be an object")),
	};

	let mut children = Vec::new();

	if let Some(list) = value.get("children") {
		let list = list
			.as_array()
			.ok_or_else(|| format!("{at}: children must be an array"))?;

		for (index, child) in list.iter().enumerate() {
			children.push(parse_pulled_node(child, &format!("{at}.children[{index}]"))?);
		}
	}

	Ok(PulledNode {
		id: id.to_owned(),
		name: name.to_owned(),
		class: class.to_owned(),
		properties,
		children,
	})
}

/// Parses a `read_subtree` response: the pulled root node plus the sibling
/// `sources` claim map covering every script in the subtree
fn parse_read_subtree(value: &Value) -> Result<(PulledNode, HashMap<String, SourceClaim>), String> {
	let snapshot = value
		.get("snapshot")
		.ok_or("read_subtree response is missing the snapshot field")?;

	let node = parse_pulled_node(snapshot, "snapshot")?;

	let mut sources = HashMap::new();

	if let Some(map) = snapshot.get("sources") {
		let map = map.as_object().ok_or("snapshot.sources must be an object")?;

		for (reference, claim) in map {
			util::parse_ref(reference).map_err(|err| format!("snapshot.sources key: {err}"))?;

			let bytes = claim
				.get("bytes")
				.and_then(Value::as_u64)
				.ok_or_else(|| format!("snapshot.sources[{reference}]: missing bytes"))?;

			let sha256 = claim
				.get("sha256")
				.and_then(Value::as_str)
				.ok_or_else(|| format!("snapshot.sources[{reference}]: missing sha256"))?;

			sources.insert(
				reference.clone(),
				SourceClaim {
					bytes,
					sha256: sha256.to_lowercase(),
				},
			);
		}
	}

	Ok((node, sources))
}

/// Pulls one script source through chunked `source_read` calls and verifies
/// the assembled bytes against the claimed length and SHA-256 (mismatch =
/// abort, per the pinned contract)
async fn pull_source(state: &AppState, reference: &str, claim: &SourceClaim) -> Result<Vec<u8>, String> {
	let mut assembled: Vec<u8> = Vec::with_capacity(claim.bytes as usize);

	loop {
		let value = plugin_request(
			state,
			"source_read",
			json!({
				"ref": reference,
				"offset": assembled.len() as u64,
				"maxLen": SOURCE_READ_CHUNK_BYTES,
			}),
			BULK_OP_TIMEOUT_MS,
		)
		.await
		.map_err(|err| format!("source_read for {reference} failed: {err}"))?;

		let data = value
			.get("data")
			.and_then(Value::as_str)
			.ok_or_else(|| format!("source_read for {reference}: missing data"))?;

		let eof = value.get("eof").and_then(Value::as_bool).unwrap_or(false);

		let total = value
			.get("totalBytes")
			.and_then(Value::as_u64)
			.ok_or_else(|| format!("source_read for {reference}: missing totalBytes"))?;

		if total != claim.bytes {
			return Err(format!(
				"source_read for {reference}: totalBytes {total} contradicts the read_subtree claim of {} bytes",
				claim.bytes
			));
		}

		let part = base64::engine::general_purpose::STANDARD
			.decode(data)
			.map_err(|err| format!("source_read for {reference}: invalid base64: {err}"))?;

		if part.len() as u64 > SOURCE_READ_CHUNK_BYTES {
			return Err(format!(
				"source_read for {reference}: part of {} bytes exceeds the {SOURCE_READ_CHUNK_BYTES}-byte chunk bound",
				part.len()
			));
		}

		if part.is_empty() && !eof {
			return Err(format!("source_read for {reference} made no progress"));
		}

		assembled.extend_from_slice(&part);

		if assembled.len() as u64 > claim.bytes {
			return Err(format!(
				"source_read for {reference} returned more bytes than the claimed {}",
				claim.bytes
			));
		}

		if eof {
			break;
		}
	}

	if assembled.len() as u64 != claim.bytes {
		return Err(format!(
			"source_read for {reference} ended at {} bytes, expected {}",
			assembled.len(),
			claim.bytes
		));
	}

	let digest = format!("{:x}", Sha256::digest(&assembled));

	if digest != claim.sha256 {
		return Err(format!(
			"SHA-256 mismatch for {reference}: assembled {digest}, claimed {}",
			claim.sha256
		));
	}

	Ok(assembled)
}

/// Converts a pulled node into a core snapshot, attaching verified script
/// sources. The `ArgonEmpty` wire marker is stripped exactly as on every
/// other decode path
fn to_snapshot(node: &PulledNode, sources: &HashMap<String, Vec<u8>>) -> Result<Snapshot, String> {
	let id = util::parse_ref(&node.id).map_err(|err| err.to_string())?;

	let mut properties = node.properties.clone();
	properties.remove("ArgonEmpty");
	properties.remove("Source");

	let mut properties: Properties = serde_json::from_value(Value::Object(properties))
		.map_err(|err| format!("{}: failed to decode properties: {err}", node.name))?;

	if util::is_script(&node.class) {
		let bytes = sources
			.get(&node.id)
			.ok_or_else(|| format!("script {} ({}) has no source entry", node.name, node.id))?;

		let source = String::from_utf8_lossy(bytes).into_owned();

		properties.insert(ustr("Source"), Variant::String(source));
	}

	let mut children = Vec::new();

	for child in &node.children {
		children.push(to_snapshot(child, sources)?);
	}

	Ok(Snapshot::new()
		.with_id(id)
		.with_name(&node.name)
		.with_class(&node.class)
		.with_properties(properties)
		.with_children(children))
}

// Keep Studio: staging, fencing, swapping

/// Studio-first apply options (Design §7.0): the promptless code-scope
/// auto-apply overlays unclaimed live files into the staged root (disk-only
/// and out-of-projection files survive the swap in place) and preserves the
/// disk originals of `differs` entries into the review store before the swap
#[derive(Debug, Default)]
pub struct StudioFirstOptions {
	/// Absolute live file → absolute preservation target (inside the review
	/// directory). Copied under the fence, right before the swap
	pub preserve: HashMap<PathBuf, PathBuf>,
}

/// Carries unclaimed live entries forward into the staged root (Design §7.0):
/// any file or directory the staged projection does not claim — disk-only
/// projection files and out-of-projection files alike — is copied in, so the
/// swap cannot displace it into the backup. Claimed paths (same relative
/// path exists in the stage) keep their staged Studio content
fn overlay_unclaimed(live: &Path, stage: &Path) -> Result<(), String> {
	fn walk(live: &Path, stage: &Path) -> std::io::Result<()> {
		for entry in fs::read_dir(live)? {
			let entry = entry?;
			let staged = stage.join(entry.file_name());

			if entry.file_type()?.is_dir() {
				// The stage claims a file here (folder demoted to file):
				// the live subtree is claimed-away, not carried forward
				if staged.is_file() {
					continue;
				}

				if !staged.exists() {
					fs::create_dir_all(&staged)?;
				}

				walk(&entry.path(), &staged)?;
			} else if !staged.exists() {
				fs::copy(entry.path(), &staged)?;
			}
		}

		Ok(())
	}

	walk(live, stage).map_err(|err| format!("failed to overlay unclaimed disk files into the stage: {err}"))
}

/// SHA-256 of every file under `dir`, keyed by relative path — the fence
/// generation. Any difference between two captures means the disk moved
fn fingerprint_dir(dir: &Path) -> Result<BTreeMap<PathBuf, [u8; 32]>, String> {
	let mut fingerprint = BTreeMap::new();

	fn walk(base: &Path, current: &Path, fingerprint: &mut BTreeMap<PathBuf, [u8; 32]>) -> std::io::Result<()> {
		for entry in fs::read_dir(current)? {
			let entry = entry?;
			let path = entry.path();

			if entry.file_type()?.is_dir() {
				walk(base, &path, fingerprint)?;
			} else {
				let bytes = fs::read(&path)?;
				let relative = path.strip_prefix(base).unwrap_or(&path).to_owned();

				fingerprint.insert(relative, Sha256::digest(&bytes).into());
			}
		}

		Ok(())
	}

	walk(dir, dir, &mut fingerprint).map_err(|err| format!("failed to read {}: {err}", dir.display()))?;

	Ok(fingerprint)
}

/// Materializes the pulled subtree into `stage_root` through the regular
/// write middleware (file/folder promotion, sidecars, name sanitization), so
/// the staged projection is byte-for-byte what syncback would produce
fn materialize_stage(
	stage_root: &Path,
	root_name: &str,
	root_class: &str,
	pulled: &PulledNode,
	sources: &HashMap<String, Vec<u8>>,
	context: &Context,
) -> Result<(), String> {
	let stage_vfs = Vfs::new(false);

	dir::write_dir(stage_root, &stage_vfs).map_err(|err| format!("failed to create the staging root: {err}"))?;

	let root_meta = Meta::new()
		.with_context(context)
		.with_source(Source::directory(stage_root));

	let root_snapshot = Snapshot::new()
		.with_name(root_name)
		.with_class(root_class)
		.with_meta(root_meta);

	let mut scratch = Tree::new(root_snapshot);
	let scratch_root = scratch.root_ref();

	for child in &pulled.children {
		let snapshot = to_snapshot(child, sources)?;

		write::apply_addition(snapshot.as_new(scratch_root), &mut scratch, &stage_vfs)
			.map_err(|err| format!("failed to stage {}: {err:#}", child.name))?;
	}

	Ok(())
}

/// Everything needed to apply one root, resolved from the live tree up front
/// so no lock is held across plugin awaits
struct RootTarget {
	id: rbx_dom_weak::types::Ref,
	class: String,
	live_dir: PathBuf,
	context: Context,
}

fn resolve_root(state: &AppState, root_name: &str) -> Result<RootTarget, String> {
	let tree = state.core.tree();

	let id = tree
		.root()
		.children()
		.iter()
		.copied()
		.find(|child| tree.get_instance(*child).is_some_and(|child| child.name == root_name))
		.ok_or_else(|| format!("{root_name} is not a served root"))?;

	let meta = tree
		.get_meta(id)
		.ok_or_else(|| format!("{root_name} has no sync metadata"))?;

	let live_dir = match meta.source.get() {
		SourceKind::Project(_, project_path, node, _) => node
			.path
			.as_ref()
			.map(|custom| project_path.with_file_name(custom.path()).clean()),
		SourceKind::Path(path) => Some(path.clone()),
		SourceKind::None => None,
	}
	.or_else(|| {
		meta.source.relevant().iter().find_map(|entry| match entry {
			SourceEntry::Folder(path) => Some(path.clone()),
			_ => None,
		})
	})
	.ok_or_else(|| {
		format!("{root_name} has no directory backing ($path); only directory-backed roots support the fenced apply")
	})?;

	let class = tree
		.get_instance(id)
		.map(|instance| instance.class.to_string())
		.unwrap_or_else(|| root_name.to_owned());

	Ok(RootTarget {
		id,
		class,
		live_dir,
		context: meta.context.clone(),
	})
}

/// Applies one root: pull → stage → fence → (preserve + overlay when
/// studio-first) → swap → refresh tree → stamp baselines. Returns the backup
/// name (`<transfer>/<root>`) on success
async fn apply_root(
	state: &AppState,
	transfer_dir: &Path,
	transfer_name: &str,
	root_name: &str,
	transfer_bytes: &mut u64,
	studio_first: Option<&StudioFirstOptions>,
) -> Result<String, String> {
	let target = resolve_root(state, root_name)?;

	if !target.live_dir.is_dir() {
		return Err(format!(
			"the live directory {} for {root_name} does not exist",
			target.live_dir.display()
		));
	}

	// The fence generation: captured before anything is pulled, verified
	// again under the pause right before the swap
	let fence = fingerprint_dir(&target.live_dir)?;

	// Pull structure
	let value = plugin_request(
		state,
		"read_subtree",
		json!({ "ref": util::format_ref(target.id) }),
		BULK_OP_TIMEOUT_MS,
	)
	.await
	.map_err(|err| format!("read_subtree for {root_name} failed: {err}"))?;

	let (pulled, claims) = parse_read_subtree(&value)?;

	// Byte budgets (Design §7.4-A)
	let mut root_bytes = 0u64;

	for (reference, claim) in &claims {
		if claim.bytes > BULK_SCRIPT_MAX_BYTES {
			return Err(format!(
				"script {reference} claims {} bytes, over the {BULK_SCRIPT_MAX_BYTES}-byte per-script bound",
				claim.bytes
			));
		}

		root_bytes += claim.bytes;
	}

	if root_bytes > BULK_ROOT_MAX_BYTES {
		return Err(format!(
			"{root_name} carries {root_bytes} source bytes, over the {BULK_ROOT_MAX_BYTES}-byte per-root bound"
		));
	}

	*transfer_bytes += root_bytes;

	if *transfer_bytes > BULK_TRANSFER_MAX_BYTES {
		return Err(format!("the transfer exceeds the {BULK_TRANSFER_MAX_BYTES}-byte bound"));
	}

	// Pull sources (chunked, hash-verified)
	let mut sources = HashMap::new();

	for (reference, claim) in &claims {
		sources.insert(reference.clone(), pull_source(state, reference, claim).await?);
	}

	// Stage the complete same-volume copy
	let stage_root = transfer_dir.join(STAGE_DIR).join(root_name);

	fs::create_dir_all(transfer_dir.join(STAGE_DIR))
		.map_err(|err| format!("failed to create the staging directory: {err}"))?;

	materialize_stage(
		&stage_root,
		root_name,
		&target.class,
		&pulled,
		&sources,
		&target.context,
	)?;

	// Fence, swap and refresh — synchronous, VFS paused throughout
	let backup_path = transfer_dir.join(root_name);
	let vfs = state.core.vfs();

	vfs.pause();

	let swapped = (|| -> Result<(), String> {
		let current = fingerprint_dir(&target.live_dir)?;

		if current != fence {
			return Err(format!(
				"the contents of {} changed while the apply was running; refusing to overwrite this root",
				target.live_dir.display()
			));
		}

		if let Some(options) = studio_first {
			// Preserve the disk originals of `differs` entries under this
			// root into the review store — the fence just proved these bytes
			// are the generation the comparison ran against (Design §7.0)
			for (live_path, preserve_target) in &options.preserve {
				if !live_path.starts_with(&target.live_dir) || !live_path.is_file() {
					continue;
				}

				if let Some(parent) = preserve_target.parent() {
					fs::create_dir_all(parent)
						.map_err(|err| format!("failed to create the review preservation directory: {err}"))?;
				}

				fs::copy(live_path, preserve_target).map_err(|err| {
					format!(
						"failed to preserve {} into the review store: {err}",
						live_path.display()
					)
				})?;
			}

			// Disk-only and out-of-projection files stay on the live disk:
			// the stage is overlaid so the swap carries them forward instead
			// of displacing them into the backup
			overlay_unclaimed(&target.live_dir, &stage_root)?;
		}

		// The watch must move off the directory before it is renamed away
		// (inotify-style watchers follow inodes, not paths)
		vfs.unwatch(&target.live_dir).ok();

		fs::rename(&target.live_dir, &backup_path)
			.map_err(|err| format!("failed to move the live root into the backup: {err}"))?;

		if let Err(err) = fs::rename(&stage_root, &target.live_dir) {
			// A failed swap restores the backup (Design §7.4-A)
			let restored = fs::rename(&backup_path, &target.live_dir).is_ok();
			vfs.watch(&target.live_dir, true).ok();

			return Err(format!(
				"failed to swap the staged root into place: {err}; the backup was {}",
				if restored { "restored" } else { "NOT restored" }
			));
		}

		vfs.watch(&target.live_dir, true).ok();

		Ok(())
	})();

	if let Err(message) = swapped {
		vfs.resume();
		return Err(message);
	}

	// Refresh the daemon tree from the swapped root. The resulting changes
	// are dropped deliberately — Studio already holds exactly this state
	// (it is where the bytes came from), so replaying them would be an echo
	{
		let mut tree = state.core.tree();

		read::process_changes(target.id, &mut tree, &vfs);

		// Baselines stamp for every touched instance (Design §7.4): both
		// sides provably agree on the swapped content
		let engine = state.core.conflicts();

		for id in conflict::subtree_refs(&tree, target.id) {
			if let Some(instance) = tree.get_instance(id) {
				let content = ContentState::new(&instance.name, &instance.class, &instance.properties);

				engine.stamp(id, content.hash);
			}
		}
	}

	vfs.resume();

	info!("Keep-Studio apply committed root {root_name} (backup {transfer_name}/{root_name})");

	Ok(format!("{transfer_name}/{root_name}"))
}

/// Runs the fenced per-root apply for every staged root in sequence. Returns
/// the backup names on success, or the pinned failure payload with its HTTP
/// status (the terminal, replayable partial receipt — earlier committed roots
/// stand, never rolled back, and their backups are preserved indefinitely
/// because the transfer never marks complete)
pub async fn apply_roots(
	state: &AppState,
	transfer_id: &str,
	staged_roots: &BTreeSet<String>,
	studio_first: Option<&StudioFirstOptions>,
) -> Result<Vec<String>, (StatusCode, Value)> {
	if staged_roots.is_empty() {
		return Ok(Vec::new());
	}

	let workspace_dir = state.core.project().workspace_dir.clone();
	let backups_root = workspace_dir.join(BACKUPS_DIR);

	prune_backups(&backups_root);

	let stamp = Utc::now().format(BACKUP_STAMP_FORMAT);
	let transfer_name = format!("{stamp}-{transfer_id}");
	let transfer_dir = backups_root.join(&transfer_name);

	if let Err(err) = fs::create_dir_all(&transfer_dir) {
		return Err((
			StatusCode::INTERNAL_SERVER_ERROR,
			json!({
				"ok": false,
				"error": format!("Failed to create the backup directory: {err}"),
			}),
		));
	}

	let mut committed: Vec<Value> = Vec::new();
	let mut backups: Vec<String> = Vec::new();
	let mut transfer_bytes = 0u64;

	for root_name in staged_roots {
		let applied = apply_root(
			state,
			&transfer_dir,
			&transfer_name,
			root_name,
			&mut transfer_bytes,
			studio_first,
		)
		.await;

		match applied {
			Ok(backup) => {
				committed.push(json!({
					"root": root_name,
					"backup": backup,
					"recoveryAction": "restoreBackup",
				}));
				backups.push(backup);
			}
			Err(error) => {
				warn!("Keep-Studio apply failed at root {root_name}: {error}");

				return Err((
					StatusCode::OK,
					json!({
						"ok": false,
						"action": "partial",
						"recoveryRequired": true,
						"failedRoot": root_name,
						"error": error,
						"committedRoots": committed,
					}),
				));
			}
		}
	}

	// The completion marker is what makes this transfer's backups prunable
	let marker = json!({
		"choiceId": transfer_id,
		"completedAt": Utc::now().to_rfc3339(),
		"roots": staged_roots.iter().collect::<Vec<_>>(),
	});

	if let Err(err) = fs::write(transfer_dir.join(BACKUP_COMPLETE_MARKER), marker.to_string()) {
		warn!("Failed to write the backup completion marker: {err}");
	}

	// Leftover empty staging directory
	fs::remove_dir_all(transfer_dir.join(STAGE_DIR)).ok();

	prune_backups(&backups_root);

	Ok(backups)
}

/// Applies the Keep-Studio decision for every staged root in sequence,
/// producing the pinned success or partial receipt (Design §7.4-A)
pub async fn apply_keep_studio(state: &AppState, choice_id: &str, staged_roots: BTreeSet<String>) -> Response {
	match apply_roots(state, choice_id, &staged_roots, None).await {
		Ok(backups) => Json(json!({
			"ok": true,
			"applied": true,
			"direction": "studio",
			"backups": backups,
		}))
		.into_response(),
		Err((status, payload)) => (status, Json(payload)).into_response(),
	}
}

// Backup pruning

/// Age of a transfer directory, derived from the stamp prefix of its name
/// (`YYYY-MM-DDTHH-MM-SSZ-...`). Unparseable names count as fresh so they
/// are only ever pruned by the count rule
fn transfer_age_days(name: &str) -> Option<i64> {
	let stamp = name.get(..20)?;
	let parsed = NaiveDateTime::parse_from_str(stamp, BACKUP_STAMP_FORMAT).ok()?;

	Some((Utc::now().naive_utc() - parsed).num_days())
}

/// Prunes completed-transfer backups: older than `BACKUP_KEEP_DAYS` or
/// beyond the newest `BACKUP_KEEP_COUNT`. Directories without the completion
/// marker are partial or in-flight transfers and are never touched
pub fn prune_backups(backups_root: &Path) {
	let Ok(entries) = fs::read_dir(backups_root) else {
		return;
	};

	let mut completed: Vec<(String, PathBuf)> = entries
		.filter_map(|entry| {
			let entry = entry.ok()?;
			let path = entry.path();

			if !path.is_dir() || !path.join(BACKUP_COMPLETE_MARKER).is_file() {
				return None;
			}

			Some((entry.file_name().to_string_lossy().into_owned(), path))
		})
		.collect();

	// The fixed-width UTC stamp prefix makes name order chronological;
	// newest first
	completed.sort_by(|(a, _), (b, _)| b.cmp(a));

	for (index, (name, path)) in completed.iter().enumerate() {
		let beyond_count = index >= BACKUP_KEEP_COUNT;
		let too_old = transfer_age_days(name).is_some_and(|days| days > BACKUP_KEEP_DAYS);

		if beyond_count || too_old {
			debug!("Pruning completed backup {name}");
			fs::remove_dir_all(path).ok();
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn transfer_ages_parse_from_the_stamp_prefix() {
		let old = "2000-01-02T03-04-05Z-abcdef";
		assert!(transfer_age_days(old).unwrap() > BACKUP_KEEP_DAYS);

		let fresh = format!("{}-abc", Utc::now().format(BACKUP_STAMP_FORMAT));
		assert_eq!(transfer_age_days(&fresh), Some(0));

		assert_eq!(transfer_age_days("garbage"), None);
		assert_eq!(transfer_age_days(""), None);
	}

	#[test]
	fn prune_keeps_partials_and_the_newest_completed() {
		let scratch = tempfile::tempdir().unwrap();
		let root = scratch.path();

		let make = |name: &str, complete: bool| {
			let dir = root.join(name);
			fs::create_dir_all(&dir).unwrap();
			fs::write(dir.join("payload"), b"x").unwrap();

			if complete {
				fs::write(dir.join(BACKUP_COMPLETE_MARKER), b"{}").unwrap();
			}
		};

		// An ancient completed transfer, an ancient partial one, and more
		// recent completed transfers than the keep count
		make("2000-01-01T00-00-00Z-old-complete", true);
		make("2000-01-01T00-00-00Z-old-partial", false);

		let stamp = Utc::now().format(BACKUP_STAMP_FORMAT);

		for index in 0..(BACKUP_KEEP_COUNT + 3) {
			make(&format!("{stamp}-recent-{index:03}"), true);
		}

		prune_backups(root);

		assert!(!root.join("2000-01-01T00-00-00Z-old-complete").exists(), "age rule");
		assert!(root.join("2000-01-01T00-00-00Z-old-partial").exists(), "partials stay");

		let survivors = fs::read_dir(root)
			.unwrap()
			.filter_map(|entry| entry.ok())
			.filter(|entry| entry.path().join(BACKUP_COMPLETE_MARKER).is_file())
			.count();

		assert_eq!(survivors, BACKUP_KEEP_COUNT, "count rule keeps the newest 32");

		// The newest entries survived, the oldest recent ones went
		assert!(root
			.join(format!("{stamp}-recent-{:03}", BACKUP_KEEP_COUNT + 2))
			.exists());
		assert!(!root.join(format!("{stamp}-recent-000")).exists());
	}

	#[test]
	fn fingerprints_catch_edits_additions_and_removals() {
		let scratch = tempfile::tempdir().unwrap();
		let dir = scratch.path();

		fs::create_dir_all(dir.join("nested")).unwrap();
		fs::write(dir.join("a.luau"), b"return 1").unwrap();
		fs::write(dir.join("nested").join("b.luau"), b"return 2").unwrap();

		let base = fingerprint_dir(dir).unwrap();
		assert_eq!(base.len(), 2);
		assert_eq!(fingerprint_dir(dir).unwrap(), base);

		fs::write(dir.join("a.luau"), b"return 9").unwrap();
		assert_ne!(fingerprint_dir(dir).unwrap(), base, "edit detected");

		fs::write(dir.join("a.luau"), b"return 1").unwrap();
		fs::write(dir.join("c.luau"), b"new").unwrap();
		assert_ne!(fingerprint_dir(dir).unwrap(), base, "addition detected");

		fs::remove_file(dir.join("c.luau")).unwrap();
		fs::remove_file(dir.join("nested").join("b.luau")).unwrap();
		assert_ne!(fingerprint_dir(dir).unwrap(), base, "removal detected");
	}
}
