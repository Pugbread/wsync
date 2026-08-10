//! `GET /resolve` · `POST /resolve` — listing and resolving parked conflicts
//! (Design §6.3, §7.4-adjacent surface; record and route shapes are pinned
//! protocol contracts).

use axum::{
	extract::State,
	http::StatusCode,
	response::{IntoResponse, Json, Response},
};
use log::{debug, trace};
use rbx_dom_weak::ustr;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{
	constants::CONFLICT_SOURCE_CAP,
	core::{
		conflict::{self, Classification, ContentState, Parked, Side},
		snapshot::{AddedSnapshot, UpdatedSnapshot},
		Core,
	},
	server::{self, AppState},
};

/// Serializes one side of a conflict record. `source` (capped, with a
/// `truncated` flag) is only present for script conflicts; property
/// conflicts carry their maps in the record-level `fsProps`/`studioProps`
fn side_json(side: &Side, is_script: bool) -> Value {
	let mut value = json!({
		"present": side.present,
		"hash": side.content.as_ref().map(|content| conflict::hash_hex(&content.hash)).unwrap_or_default(),
	});

	if is_script {
		if let Some(source) = side.content.as_ref().and_then(ContentState::source) {
			let (source, truncated) = cap_source(source);

			value["source"] = Value::String(source);

			if truncated {
				value["truncated"] = Value::Bool(true);
			}
		}
	}

	value
}

fn cap_source(source: String) -> (String, bool) {
	if source.len() <= CONFLICT_SOURCE_CAP {
		return (source, false);
	}

	let mut end = CONFLICT_SOURCE_CAP;

	while !source.is_char_boundary(end) {
		end -= 1;
	}

	(source[..end].to_owned(), true)
}

/// Builds the pinned conflict record shape
fn record_json(parked: &Parked) -> Value {
	let is_script = parked.kind() == "script";

	let mut record = json!({
		"id": parked.id,
		"path": parked.info.rel_path,
		"instancePath": parked.info.instance_path,
		"class": parked.class(),
		"kind": parked.kind(),
		"classification": parked.classification.as_str(),
		"fs": side_json(&parked.fs, is_script),
		"studio": side_json(&parked.studio, is_script),
		"detectedAt": parked.detected_at,
	});

	if !is_script {
		if let Some(content) = &parked.fs.content {
			record["fsProps"] = content.props_json();
		}

		if let Some(content) = &parked.studio.content {
			record["studioProps"] = content.props_json();
		}
	}

	record
}

/// `GET /resolve` — the parked-conflict listing
pub async fn list(State(state): State<AppState>) -> Json<Value> {
	trace!("Received request: resolve (list)");

	let conflicts: Vec<Value> = state.core.conflicts().list().iter().map(record_json).collect();

	Json(json!({ "conflicts": conflicts }))
}

#[derive(Deserialize, Debug)]
pub struct ResolveRequest {
	id: Option<String>,
	#[allow(dead_code)]
	path: Option<String>,
	keep: Option<String>,
	choice: Option<String>,
}

fn error(status: StatusCode, message: &str) -> Response {
	(status, Json(json!({ "ok": false, "error": message }))).into_response()
}

/// `POST /resolve` `{id, path?, keep: "local"|"studio", choice: <same>}` —
/// resolves one parked conflict. `keep: "local"` pushes the disk state to
/// Studio over the sync channel; `keep: "studio"` writes the Studio state to
/// disk through the syncback write path. Both directions re-stamp baselines
pub async fn resolve(State(state): State<AppState>, Json(request): Json<ResolveRequest>) -> Response {
	trace!("Received request: resolve (post)");

	let direction = match request.keep.as_deref().or(request.choice.as_deref()) {
		Some("local") => Direction::Local,
		Some("studio") => Direction::Studio,
		Some(other) => {
			return error(
				StatusCode::BAD_REQUEST,
				&format!("keep must be \"local\" or \"studio\", got {other:?}"),
			)
		}
		None => return error(StatusCode::BAD_REQUEST, "Missing keep (or choice) field"),
	};

	let engine = state.core.conflicts();

	let Some(parked) = request.id.as_deref().and_then(|id| engine.take(id)) else {
		return error(StatusCode::NOT_FOUND, "Unknown conflict id");
	};

	let id = parked.id.clone();

	debug!(
		"Resolving conflict {id} ({}) keeping {}",
		parked.classification.as_str(),
		match direction {
			Direction::Local => "local",
			Direction::Studio => "studio",
		}
	);

	let outcome = match direction {
		Direction::Local => keep_local(&state.core, &parked),
		Direction::Studio => keep_studio(&state.core, &parked).await,
	};

	match outcome {
		Ok(()) => Json(json!({ "ok": true, "resolved": id })).into_response(),
		Err((status, message)) => {
			// The conflict stays parked when its resolution could not land
			engine.restore(parked);
			error(status, &message)
		}
	}
}

enum Direction {
	Local,
	Studio,
}

type Failure = (StatusCode, String);

/// Keep the disk state: emit sync frames from the live tree so Studio
/// converges onto disk, then re-stamp the baseline
fn keep_local(core: &Core, parked: &Parked) -> Result<(), Failure> {
	let engine = core.conflicts();
	let mut changes = server::SyncChanges(crate::core::changes::Changes::new());

	match parked.classification {
		Classification::FsDeletedStudioEdited => {
			// Disk wins and disk deleted it: remove the instance in Studio
			changes.0.remove(parked.instance);
			engine.forget(parked.instance);
		}
		Classification::StudioDeletedFsEdited => {
			// Studio deleted it, disk wins: recreate the subtree in Studio
			let snapshot = core.snapshot(parked.instance).ok_or((
				StatusCode::CONFLICT,
				"The conflicted instance no longer exists on disk".to_owned(),
			))?;

			changes.0.additions.push(snapshot);
			stamp_from_tree(core, parked.instance);
		}
		Classification::BothEdited => {
			let tree = core.tree();

			let instance = tree.get_instance(parked.instance).ok_or((
				StatusCode::CONFLICT,
				"The conflicted instance no longer exists on disk".to_owned(),
			))?;

			let mut update = UpdatedSnapshot::new(parked.instance);
			update.name = Some(instance.name.clone());
			update.properties = Some(instance.properties.clone());

			let content = ContentState::new(&instance.name, &instance.class, &instance.properties);

			changes.0.update(update);
			drop(tree);

			engine.stamp(parked.instance, content.hash);
		}
	}

	core.queue().push(changes, None).map_err(|err| {
		(
			StatusCode::INTERNAL_SERVER_ERROR,
			format!("Failed to queue sync frames: {err}"),
		)
	})?;

	Ok(())
}

/// Keep the Studio state: route the parked Studio content through the
/// middleware write path (the processor re-stamps baselines on the way)
async fn keep_studio(core: &Core, parked: &Parked) -> Result<(), Failure> {
	let mut changes = crate::core::changes::Changes::new();

	match parked.classification {
		Classification::StudioDeletedFsEdited => {
			// Studio wins and Studio deleted it: delete on disk
			changes.remove(parked.instance);
		}
		Classification::FsDeletedStudioEdited => {
			// Disk deleted it, Studio wins: recreate the file from the
			// parked Studio state
			let apply = parked.studio_apply.as_ref().ok_or((
				StatusCode::CONFLICT,
				"The conflict carries no Studio state to restore".to_owned(),
			))?;

			if !core.tree().exists(parked.info.parent) {
				return Err((
					StatusCode::CONFLICT,
					"The parent of the deleted instance no longer exists".to_owned(),
				));
			}

			changes.additions.push(AddedSnapshot {
				id: parked.instance,
				meta: crate::core::meta::Meta::new(),
				parent: parked.info.parent,
				name: apply.name.clone(),
				class: ustr(&apply.class),
				properties: apply.properties.clone(),
				children: Vec::new(),
			});
		}
		Classification::BothEdited => {
			let apply = parked.studio_apply.as_ref().ok_or((
				StatusCode::CONFLICT,
				"The conflict carries no Studio state to apply".to_owned(),
			))?;

			let current_name = {
				let tree = core.tree();

				tree.get_instance(parked.instance)
					.map(|instance| instance.name.clone())
					.ok_or((
						StatusCode::CONFLICT,
						"The conflicted instance no longer exists on disk".to_owned(),
					))?
			};

			let mut update = UpdatedSnapshot::new(parked.instance);
			update.properties = Some(apply.properties.clone());

			if apply.name != current_name {
				update.name = Some(apply.name.clone());
			}

			changes.update(update);
		}
	}

	let (sender, receiver) = tokio::sync::oneshot::channel();

	core.processor().write_with_result(
		crate::core::processor::WriteRequest {
			changes,
			client_id: conflict::RESOLUTION_CLIENT_ID,
		},
		sender,
	);

	let result = receiver.await.map_err(|_| {
		(
			StatusCode::INTERNAL_SERVER_ERROR,
			"The write processor dropped the resolution".to_owned(),
		)
	})?;

	if !result.ok() {
		return Err((
			StatusCode::INTERNAL_SERVER_ERROR,
			format!("Failed to apply the Studio state: {}", result.errors.join("; ")),
		));
	}

	Ok(())
}

/// Stamps baselines for an entire live-tree subtree (used after re-creating
/// it in Studio)
fn stamp_from_tree(core: &Core, id: rbx_dom_weak::types::Ref) {
	let engine = core.conflicts();
	let tree = core.tree();

	for id in conflict::subtree_refs(&tree, id) {
		if let Some(instance) = tree.get_instance(id) {
			let content = ContentState::new(&instance.name, &instance.class, &instance.properties);

			engine.stamp(id, content.hash);
		}
	}
}
