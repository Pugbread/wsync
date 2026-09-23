//! `wsync verify` — compares disk against what Studio actually holds, so the
//! drift an agent would otherwise hunt down with a hand-written `eval` script
//! is one command.
//!
//! The daemon side walks its own tree: every script's disk text (read through
//! the same VFS path and line-ending normalization the sync engine uses, see
//! `core::conflict::content`), its duplicate siblings, and the backlog entries
//! whose path is back on disk. The plugin side answers two bounded ops:
//!
//! * `verify_sources` — per ref: is the instance in Studio, and does its
//!   editor text (`ScriptEditorService:GetEditorSource`, falling back to
//!   `.Source`) hash to the disk text? It also reports an editor draft that
//!   differs from the committed `.Source`. Chunked by entry count and by the
//!   disk bytes the plugin may have to hash.
//! * `verify_tree` — a paged walk of the synced roots listing Studio's scripts
//!   and its same-named siblings.
//!
//! The disk hazard scanner (`core::hazards`) runs alongside for the layout
//! problems only the files show — double-init folders, flat+folder pairs.
//!
//! The result is grouped by category; `clean` is true only when no problem
//! category has an entry (editor drafts, untracked matches and hazard
//! warnings are informational).

use axum::{body::Bytes, extract::State, response::Json};
use log::trace;
use rbx_dom_weak::{types::Variant, ustr};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::{
	borrow::Cow,
	collections::{BTreeMap, HashMap, HashSet},
	path::{Path, PathBuf},
	time::{Duration, Instant},
};

use crate::{
	constants::{
		VERIFY_CHUNK_MAX_BYTES, VERIFY_CHUNK_MAX_ENTRIES, VERIFY_LIST_LIMIT, VERIFY_OP_TIMEOUT_MS, VERIFY_TREE_PAGE,
		WAIT_SYNCED_DEFAULT_TIMEOUT_MS, WAIT_SYNCED_MAX_TIMEOUT_MS,
	},
	core::{
		conflict::content::{instance_path, relative_path},
		hazards, Core,
	},
	project::Project,
	server::{bulk, synced, ws::frames::codes, AppState},
	util,
};

#[derive(Deserialize, Debug, Default)]
#[serde(rename_all = "camelCase")]
struct VerifyRequest {
	/// Skip the `wait-synced` barrier that normally runs first
	#[serde(default)]
	no_settle: bool,
	/// Deadline for that barrier
	timeout_ms: Option<u64>,
}

/// The engine's line-ending rule (`core::conflict::content`): CRLF folds to LF
fn normalize(text: &str) -> Cow<'_, str> {
	if text.contains('\r') {
		Cow::Owned(text.replace("\r\n", "\n"))
	} else {
		Cow::Borrowed(text)
	}
}

fn sha256_hex(text: &str) -> String {
	format!("{:x}", Sha256::digest(text.as_bytes()))
}

fn is_luau_file(path: &Path) -> bool {
	matches!(path.extension().and_then(|ext| ext.to_str()), Some("luau" | "lua"))
}

/// One daemon-tree script with its disk side resolved
struct Script {
	ref_hex: String,
	path: String,
	fs_path: Option<String>,
	class: String,
	/// Where the compared text came from: the backing Luau file, or the
	/// daemon tree (a script a middleware generates — JSON/TOML modules,
	/// project-defined scripts)
	origin: &'static str,
	/// `(bytes, sha256)` of the normalized disk text; `None` when the backing
	/// file is gone or unreadable
	disk: Option<(u64, String)>,
	/// Why the backing file could not be read, when it still exists
	disk_error: Option<String>,
	/// Bytes of the daemon tree's own source when it disagrees with the file
	/// on disk — a disk change the daemon never picked up
	stale_tree_bytes: Option<u64>,
}

/// A backlog entry whose path is back on disk
struct BacklogCheck {
	entry: Value,
	refs: Vec<String>,
}

/// Everything verify needs from the daemon, gathered in one pass
struct DaemonSide {
	scope: &'static str,
	is_place: bool,
	roots: Vec<Value>,
	scripts: Vec<Script>,
	/// Every instance path in the tree (place projects, where Studio paths
	/// and daemon paths share the DataModel root)
	instance_paths: HashSet<String>,
	known_refs: HashSet<String>,
	/// Refs of instances that keep unknown (Studio-only) children
	keep_unknowns: HashSet<String>,
	duplicates: Vec<Value>,
	backlog: Vec<BacklogCheck>,
}

/// Script identity collected under the tree lock; files are read after it is
/// released so the processor is never stalled on disk I/O
struct Pending {
	ref_hex: String,
	path: String,
	fs_path: Option<String>,
	class: String,
	file: Option<PathBuf>,
	tree_source: Option<String>,
}

fn collect(core: &Core, backlog: &Value) -> DaemonSide {
	// Lock order matches the rest of the daemon: never tree and project at
	// once here
	let (workspace_dir, scope, is_place) = {
		let project = core.project();

		(
			project.workspace_dir.clone(),
			project.scope().as_str(),
			project.is_place(),
		)
	};

	let mut pending = Vec::new();
	let mut instance_paths = HashSet::new();
	let mut known_refs = HashSet::new();
	let mut keep_unknowns = HashSet::new();
	let mut duplicates = Vec::new();
	let mut backlog_checks = Vec::new();

	let roots = {
		let tree = core.tree();

		let root_refs: Vec<_> = if is_place {
			tree.place_root_refs().to_vec()
		} else {
			vec![tree.root_ref()]
		};

		let roots = root_refs
			.iter()
			.map(|id| {
				json!({
					"ref": util::format_ref(*id),
					"path": if is_place { Some(instance_path(&tree, *id)) } else { None },
				})
			})
			.collect();

		let mut walk = vec![tree.root_ref()];

		while let Some(id) = walk.pop() {
			let Some(instance) = tree.get_instance(id) else {
				continue;
			};

			let ref_hex = util::format_ref(id);
			let path = instance_path(&tree, id);

			if tree.get_meta(id).is_some_and(|meta| meta.keep_unknowns) {
				keep_unknowns.insert(ref_hex.clone());
			}

			if util::is_script(&instance.class) {
				let file = tree
					.get_meta(id)
					.and_then(|meta| meta.source.get_file())
					.map(|entry| entry.path().to_path_buf())
					.filter(|file| is_luau_file(file));

				let tree_source = match instance.properties.get(&ustr("Source")) {
					Some(Variant::String(source)) => Some(normalize(source).into_owned()),
					_ => None,
				};

				pending.push(Pending {
					ref_hex: ref_hex.clone(),
					path: path.clone(),
					fs_path: relative_path(&tree, id, &workspace_dir),
					class: instance.class.to_string(),
					file,
					tree_source,
				});
			}

			// Same-named siblings: two instances the projection cannot tell
			// apart by path (a `Foo.luau` next to a `Foo/` folder, two files
			// mapping to one name)
			let mut by_name: BTreeMap<&str, Vec<_>> = BTreeMap::new();

			for child in instance.children() {
				if let Some(child_instance) = tree.get_instance(*child) {
					by_name.entry(child_instance.name.as_str()).or_default().push(*child);
				}
			}

			for (name, members) in by_name {
				if members.len() < 2 {
					continue;
				}

				duplicates.push(json!({
					"parent": path,
					"name": name,
					"count": members.len(),
					"classes": members
						.iter()
						.filter_map(|member| tree.get_instance(*member).map(|member| member.class.to_string()))
						.collect::<Vec<_>>(),
					"fsPaths": members
						.iter()
						.filter_map(|member| relative_path(&tree, *member, &workspace_dir))
						.collect::<Vec<_>>(),
				}));
			}

			if is_place {
				instance_paths.insert(path);
			}

			known_refs.insert(ref_hex);
			walk.extend(instance.children().iter().copied());
		}

		// Backlog entries whose path exists on disk again: which tree
		// instances (if any) that file projects to
		let empty = Vec::new();

		for entry in backlog.get("entries").and_then(Value::as_array).unwrap_or(&empty) {
			let Some(rel) = entry.get("path").and_then(Value::as_str) else {
				continue;
			};

			let live = workspace_dir.join(rel);

			if !live.exists() {
				continue;
			}

			let refs = tree
				.get_ids(&live)
				.map(|ids| ids.iter().map(|id| util::format_ref(*id)).collect())
				.unwrap_or_default();

			backlog_checks.push(BacklogCheck {
				entry: json!({
					"id": entry.get("id"),
					"path": rel,
					"reason": entry.get("reason"),
					"capturedAt": entry.get("capturedAt"),
				}),
				refs,
			});
		}

		roots
	};

	let vfs = core.vfs();

	let scripts = pending
		.into_iter()
		.map(|script| {
			let mut disk_error = None;

			let (origin, disk, stale_tree_bytes) = match &script.file {
				Some(file) => match vfs.read_to_string(file) {
					Ok(text) => {
						let text = normalize(&text);
						let stale = script
							.tree_source
							.as_ref()
							.filter(|tree| tree.as_str() != text.as_ref())
							.map(|tree| tree.len() as u64);

						("file", Some((text.len() as u64, sha256_hex(&text))), stale)
					}
					Err(err) => {
						if file.exists() {
							disk_error = Some(err.to_string());
						}

						("file", None, None)
					}
				},
				None => {
					let text = script.tree_source.clone().unwrap_or_default();

					("generated", Some((text.len() as u64, sha256_hex(&text))), None)
				}
			};

			Script {
				ref_hex: script.ref_hex,
				path: script.path,
				fs_path: script.fs_path,
				class: script.class,
				origin,
				disk,
				disk_error,
				stale_tree_bytes,
			}
		})
		.collect();

	DaemonSide {
		scope,
		is_place,
		roots,
		scripts,
		instance_paths,
		known_refs,
		keep_unknowns,
		duplicates,
		backlog: backlog_checks,
	}
}

/// The disk-hazard scan (`core::hazards`): double-init folders, flat+folder
/// pairs, reshape leftovers and the other layout problems read from the files
/// alone. Errors count toward `clean`; warnings are reported but do not fail.
/// Scans a freshly loaded copy of the project so the live project lock is
/// never held across the directory walk
fn disk_hazards(core: &Core) -> Value {
	let (project_path, workspace_dir) = {
		let project = core.project();

		(project.path.clone(), project.workspace_dir.clone())
	};

	let project = match Project::load(&project_path) {
		Ok(project) => project,
		Err(err) => {
			return json!({
				"available": false,
				"count": 0,
				"warnings": 0,
				"findings": [],
				"note": format!("the project file could not be loaded for the hazard scan: {err:#}"),
			})
		}
	};

	let report = hazards::scan(&project);
	let relative = |path: &PathBuf| {
		path.strip_prefix(&workspace_dir)
			.unwrap_or(path)
			.to_string_lossy()
			.into_owned()
	};

	let findings: Vec<Value> = report
		.hazards
		.iter()
		.take(VERIFY_LIST_LIMIT)
		.map(|hazard| {
			json!({
				"kind": hazard.kind.as_str(),
				"severity": hazard.severity,
				"paths": hazard.paths.iter().map(relative).collect::<Vec<_>>(),
				"winner": hazard.winner.as_ref().map(relative),
				"message": hazard.message,
				"fix": hazard.fix,
			})
		})
		.collect();

	json!({
		"available": true,
		"count": report.errors(),
		"warnings": report.warnings(),
		"directories": report.directories,
		"files": report.files,
		"findings": findings,
	})
}

/// One plugin `verify_sources` entry
struct Check {
	value: Value,
	bytes: u64,
}

/// Splits checks into chunks bounded by entry count and by the disk bytes the
/// plugin may have to hash (at least one entry per chunk)
fn chunk(checks: Vec<Check>) -> Vec<Vec<Value>> {
	let mut chunks = Vec::new();
	let mut current = Vec::new();
	let mut bytes = 0;

	for check in checks {
		if !current.is_empty()
			&& (current.len() >= VERIFY_CHUNK_MAX_ENTRIES || bytes + check.bytes > VERIFY_CHUNK_MAX_BYTES)
		{
			chunks.push(std::mem::take(&mut current));
			bytes = 0;
		}

		bytes += check.bytes;
		current.push(check.value);
	}

	if !current.is_empty() {
		chunks.push(current);
	}

	chunks
}

fn failure(code: &str, message: impl Into<String>) -> Json<Value> {
	Json(json!({ "ok": false, "error": { "code": code, "message": message.into() } }))
}

/// A failed plugin op, keeping its protocol code
fn op_failure(op: &str, err: bulk::PluginCallError) -> Json<Value> {
	let hint = if err.code == codes::UNKNOWN_OP {
		" — update the Studio plugin (`wsync plugin install`) and reconnect"
	} else {
		""
	};

	failure(&err.code, format!("{op} failed: {}{hint}", err.message))
}

/// Caps one category list (sorted by path, so reports are stable),
/// remembering how many entries were cut
fn bounded(mut list: Vec<Value>, key: &str, truncated: &mut Map<String, Value>) -> Value {
	list.sort_by(|a, b| {
		let path = |value: &Value| value.get("path").or_else(|| value.get("parent")).map(Value::to_string);

		path(a).cmp(&path(b))
	});

	if list.len() > VERIFY_LIST_LIMIT {
		truncated.insert(key.to_owned(), json!(list.len() - VERIFY_LIST_LIMIT));

		Value::Array(list.into_iter().take(VERIFY_LIST_LIMIT).collect())
	} else {
		Value::Array(list)
	}
}

fn str_field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
	value.get(key).and_then(Value::as_str)
}

/// `POST /verify {noSettle?, timeoutMs?}` — always HTTP 200; `ok` says whether
/// the check ran, `clean` whether it found anything
pub async fn main(State(state): State<AppState>, body: Bytes) -> Json<Value> {
	trace!("Received request: verify");

	let request: VerifyRequest = serde_json::from_slice(&body).unwrap_or_default();
	let started = Instant::now();

	if let Err(reason) = state.ws.plugin_frames() {
		return failure(codes::PLUGIN_ERROR, reason);
	}

	// Settle first, so changes still in flight are not reported as drift
	let settle = if request.no_settle {
		json!({ "ran": false })
	} else {
		let timeout_ms = request
			.timeout_ms
			.unwrap_or(WAIT_SYNCED_DEFAULT_TIMEOUT_MS)
			.clamp(1, WAIT_SYNCED_MAX_TIMEOUT_MS);

		match synced::run_barrier(&state, Instant::now() + Duration::from_millis(timeout_ms)).await {
			Ok(outcome) => json!({ "ran": true, "ok": true, "flushedChanges": outcome.flushed_changes }),
			Err(failure) => json!({
				"ran": true,
				"ok": false,
				"stage": failure.stage.as_str(),
				"error": { "code": failure.code, "message": failure.message },
			}),
		}
	};

	let backlog = state.backlog.to_json();
	let core = state.core.clone();

	let side = match tokio::task::spawn_blocking(move || {
		let side = collect(&core, &backlog);
		let hazards = disk_hazards(&core);

		(side, hazards)
	})
	.await
	{
		Ok(side) => side,
		Err(err) => return failure("INTERNAL_ERROR", format!("The daemon-side scan failed: {err}")),
	};

	let (side, hazards) = side;

	// Studio: per-ref presence and source hashes
	let mut checks = Vec::new();

	for script in &side.scripts {
		let Some((bytes, sha256)) = &script.disk else {
			continue;
		};

		let mut value = json!({ "ref": script.ref_hex, "bytes": bytes, "sha256": sha256 });

		if side.is_place {
			value["path"] = json!(script.path);
		}

		checks.push(Check { value, bytes: *bytes });
	}

	let script_refs: HashSet<&str> = side.scripts.iter().map(|script| script.ref_hex.as_str()).collect();
	let mut presence_refs = HashSet::new();

	for check in &side.backlog {
		for reference in &check.refs {
			if !script_refs.contains(reference.as_str()) && presence_refs.insert(reference.clone()) {
				checks.push(Check {
					value: json!({ "ref": reference }),
					bytes: 0,
				});
			}
		}
	}

	let mut studio: HashMap<String, Value> = HashMap::new();

	for entries in chunk(checks) {
		let answer = match bulk::plugin_request(
			&state,
			"verify_sources",
			json!({ "entries": entries }),
			VERIFY_OP_TIMEOUT_MS,
		)
		.await
		{
			Ok(answer) => answer,
			Err(err) => return op_failure("verify_sources", err),
		};

		for result in answer.get("results").and_then(Value::as_array).into_iter().flatten() {
			if let Some(reference) = str_field(result, "ref") {
				studio.insert(reference.to_owned(), result.clone());
			}
		}
	}

	// Studio: the scripts and same-named siblings under the synced roots
	let mut studio_scripts = Vec::new();
	let mut studio_duplicates = Vec::new();
	let mut walk = json!({});
	let mut offset = 0;

	loop {
		let answer = match bulk::plugin_request(
			&state,
			"verify_tree",
			json!({ "roots": side.roots, "offset": offset, "limit": VERIFY_TREE_PAGE }),
			VERIFY_OP_TIMEOUT_MS,
		)
		.await
		{
			Ok(answer) => answer,
			Err(err) => return op_failure("verify_tree", err),
		};

		let page = answer
			.get("scripts")
			.and_then(Value::as_array)
			.cloned()
			.unwrap_or_default();
		let total = answer.get("total").and_then(Value::as_u64).unwrap_or(0) as usize;

		if offset == 0 {
			studio_duplicates = answer
				.get("duplicates")
				.and_then(Value::as_array)
				.cloned()
				.unwrap_or_default();
			walk = json!({
				"nodes": answer.get("nodes"),
				"truncated": answer.get("truncated").and_then(Value::as_bool).unwrap_or(false),
				"missingRoots": answer.get("missingRoots").cloned().unwrap_or_else(|| json!([])),
			});
		}

		let count = page.len();

		offset += count;
		studio_scripts.extend(page);

		// A short page is the last one; the bound stops a plugin that keeps
		// answering full pages past its own total
		if count < VERIFY_TREE_PAGE || offset >= total || offset > total + VERIFY_TREE_PAGE {
			break;
		}
	}

	// Classification
	let mut missing_in_studio = Vec::new();
	let mut missing_on_disk = Vec::new();
	let mut mismatch = Vec::new();
	let mut stale_daemon = Vec::new();
	let mut draft = Vec::new();
	let mut untracked = Vec::new();

	for script in &side.scripts {
		if let Some(daemon_bytes) = script.stale_tree_bytes {
			stale_daemon.push(json!({
				"path": script.path,
				"fsPath": script.fs_path,
				"diskBytes": script.disk.as_ref().map(|(bytes, _)| *bytes),
				"daemonBytes": daemon_bytes,
			}));
		}

		let Some((disk_bytes, disk_sha)) = &script.disk else {
			let reason = match &script.disk_error {
				Some(err) => format!("the file exists but could not be read: {err}"),
				None => "the daemon still projects this script but its file is gone from disk".to_owned(),
			};

			missing_on_disk.push(json!({
				"path": script.path,
				"fsPath": script.fs_path,
				"class": script.class,
				"reason": reason,
			}));
			continue;
		};

		let Some(result) = studio.get(&script.ref_hex) else {
			continue;
		};

		if result.get("found").and_then(Value::as_bool) != Some(true) {
			missing_in_studio.push(json!({
				"path": script.path,
				"fsPath": script.fs_path,
				"class": script.class,
			}));
			continue;
		}

		let tracked = result.get("tracked").and_then(Value::as_bool).unwrap_or(false);

		if !tracked {
			untracked.push(json!({ "path": script.path, "fsPath": script.fs_path }));
		}

		if result.get("match").and_then(Value::as_bool) == Some(false) {
			mismatch.push(json!({
				"path": script.path,
				"fsPath": script.fs_path,
				"origin": script.origin,
				"diskBytes": disk_bytes,
				"studioBytes": result.get("bytes"),
				"diskSha256": disk_sha,
				"studioSha256": result.get("sha256"),
				"studioClass": result.get("class"),
				"tracked": tracked,
			}));
		}

		if let Some(draft_info) = result.get("draft").filter(|draft| draft.is_object()) {
			draft.push(json!({
				"path": script.path,
				"editorBytes": result.get("bytes"),
				"sourceBytes": draft_info.get("bytes"),
			}));
		}
	}

	let mut kept_studio_only = 0;

	for script in &studio_scripts {
		let known = str_field(script, "ref").is_some_and(|reference| side.known_refs.contains(reference));
		let path = str_field(script, "path").unwrap_or_default();

		if known || (side.is_place && side.instance_paths.contains(path)) {
			continue;
		}

		// Full scope keeps Studio-only children under containers that keep
		// unknowns; that is the projection working, not drift
		if side.scope == "full"
			&& str_field(script, "parentRef").is_some_and(|parent| side.keep_unknowns.contains(parent))
		{
			kept_studio_only += 1;
			continue;
		}

		missing_on_disk.push(json!({
			"path": path,
			"class": script.get("class"),
			"reason": "Studio has this script but the daemon's projection of disk does not",
		}));
	}

	let mut backlog_stuck = Vec::new();

	for check in &side.backlog {
		let why = if check.refs.is_empty() {
			Some("the file is back on disk but the daemon never picked it up")
		} else if !check.refs.iter().any(|reference| {
			studio
				.get(reference)
				.and_then(|result| result.get("found"))
				.and_then(Value::as_bool)
				== Some(true)
		}) {
			Some("the file is back on disk but never reached Studio")
		} else {
			None
		};

		if let Some(why) = why {
			let mut entry = check.entry.clone();

			entry["why"] = json!(why);
			backlog_stuck.push(entry);
		}
	}

	let hazard_count = hazards.get("count").and_then(Value::as_u64).unwrap_or(0) as usize;
	let duplicate_count = studio_duplicates.len() + side.duplicates.len();

	let counts = json!({
		"missingInStudio": missing_in_studio.len(),
		"missingOnDisk": missing_on_disk.len(),
		"mismatch": mismatch.len(),
		"staleDaemon": stale_daemon.len(),
		"duplicateSiblings": duplicate_count,
		"backlogStuck": backlog_stuck.len(),
		"hazards": hazard_count,
		"draft": draft.len(),
		"untracked": untracked.len(),
		"keptStudioOnly": kept_studio_only,
	});

	let clean = missing_in_studio.is_empty()
		&& missing_on_disk.is_empty()
		&& mismatch.is_empty()
		&& stale_daemon.is_empty()
		&& duplicate_count == 0
		&& backlog_stuck.is_empty()
		&& hazard_count == 0;

	let mut truncated = Map::new();

	let mut report = json!({
		"ok": true,
		"clean": clean,
		"scope": side.scope,
		"durationMs": started.elapsed().as_millis() as u64,
		"settle": settle,
		"checked": {
			"scripts": side.scripts.len(),
			"studioScripts": studio_scripts.len(),
			"backlogOnDisk": side.backlog.len(),
		},
		"counts": counts,
		"missingInStudio": bounded(missing_in_studio, "missingInStudio", &mut truncated),
		"missingOnDisk": bounded(missing_on_disk, "missingOnDisk", &mut truncated),
		"mismatch": bounded(mismatch, "mismatch", &mut truncated),
		"staleDaemon": bounded(stale_daemon, "staleDaemon", &mut truncated),
		"duplicateSiblings": {
			"studio": bounded(studio_duplicates, "duplicateSiblings.studio", &mut truncated),
			"daemon": bounded(side.duplicates, "duplicateSiblings.daemon", &mut truncated),
		},
		"backlogStuck": bounded(backlog_stuck, "backlogStuck", &mut truncated),
		"hazards": hazards,
		"draft": bounded(draft, "draft", &mut truncated),
		"untracked": bounded(untracked, "untracked", &mut truncated),
		"studioWalk": walk,
	});

	report["truncated"] = Value::Object(truncated);

	Json(report)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn normalize_folds_crlf_like_the_engine() {
		assert_eq!(normalize("a\r\nb\n"), "a\nb\n");
		assert_eq!(normalize("plain\n"), "plain\n");
		// A lone CR is left alone, exactly as `content::normalize_source`
		assert_eq!(normalize("a\rb"), "a\rb");
	}

	#[test]
	fn chunks_are_bounded_by_entries_and_bytes() {
		let checks = |sizes: &[u64]| -> Vec<Check> {
			sizes
				.iter()
				.map(|bytes| Check {
					value: json!({ "bytes": bytes }),
					bytes: *bytes,
				})
				.collect()
		};

		// Byte bound: two halves fit, the third starts a new chunk
		let half = VERIFY_CHUNK_MAX_BYTES / 2;
		let chunks = chunk(checks(&[half, half, half]));
		assert_eq!(chunks.iter().map(Vec::len).collect::<Vec<_>>(), vec![2, 1]);

		// An oversized entry still travels, alone
		let chunks = chunk(checks(&[VERIFY_CHUNK_MAX_BYTES * 3, 1]));
		assert_eq!(chunks.iter().map(Vec::len).collect::<Vec<_>>(), vec![1, 1]);

		// Entry bound
		let chunks = chunk(checks(&vec![1; VERIFY_CHUNK_MAX_ENTRIES + 1]));
		assert_eq!(chunks.len(), 2);
		assert_eq!(chunks[0].len(), VERIFY_CHUNK_MAX_ENTRIES);
	}
}
