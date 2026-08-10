use axum::{
	extract::{Query, State},
	http::StatusCode,
	response::{IntoResponse, Json, Response},
};
use log::trace;
use rbx_dom_weak::{types::Ref, ustr};
use serde::Deserialize;
use serde_json::json;
use tokio::task;

use crate::{
	core::snapshot::{AddedSnapshot, Snapshot},
	server::AppState,
	util,
};

#[derive(Deserialize, Debug)]
pub struct Params {
	/// Hex ref of the subtree root; absent or all-zeros means the whole tree
	#[serde(rename = "ref")]
	instance: Option<String>,
}

/// `GET /snapshot?ref=<hex>` — protocol v1's JSON snapshot export (Design
/// §5.2 and Phase-2 checklist item b). The msgpack `POST /snapshot` remains
/// untouched for the compat transport
pub async fn main(State(state): State<AppState>, Query(params): Query<Params>) -> Response {
	trace!("Received request: snapshot (json)");

	let instance = match &params.instance {
		None => Ref::none(),
		Some(hex) => match util::parse_ref(hex) {
			Ok(id) => id,
			Err(err) => {
				return (StatusCode::BAD_REQUEST, Json(json!({ "error": err.to_string() }))).into_response();
			}
		},
	};

	let core = state.core.clone();

	// Snapshotting locks the tree and may walk a large subtree, so it runs
	// on the blocking thread pool
	let snapshot = task::spawn_blocking(move || core.snapshot(instance)).await;

	match snapshot {
		Ok(Some(mut snapshot)) => {
			strip_argon_empty(&mut snapshot);
			Json(snapshot).into_response()
		}
		Ok(None) => (
			StatusCode::NOT_FOUND,
			Json(json!({ "error": format!("No instance with ref {}", util::format_ref(instance)) })),
		)
			.into_response(),
		Err(err) => (
			StatusCode::INTERNAL_SERVER_ERROR,
			Json(json!({ "error": err.to_string() })),
		)
			.into_response(),
	}
}

/// Strips the `ArgonEmpty` marker property from every node of the snapshot,
/// exactly as the msgpack write path strips it on ingest (the marker is the
/// plugin's workaround for empty Luau maps serializing as arrays and must
/// never leak onto any JSON surface)
fn strip_argon_empty(snapshot: &mut AddedSnapshot) {
	fn walk(snapshot: &mut Snapshot) {
		snapshot.properties.remove(&ustr("ArgonEmpty"));

		for child in &mut snapshot.children {
			walk(child);
		}
	}

	snapshot.properties.remove(&ustr("ArgonEmpty"));

	for child in &mut snapshot.children {
		walk(child);
	}
}
