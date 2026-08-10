use axum::{
	extract::State,
	http::StatusCode,
	response::{IntoResponse, Response},
};
use log::trace;
use rbx_dom_weak::types::Ref;
use serde::Deserialize;
use tokio::task;

use crate::server::{AppState, MsgPack};

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Request {
	instance: Ref,
}

pub async fn main(State(state): State<AppState>, request: MsgPack<Request>) -> Response {
	trace!("Received request: snapshot");

	let instance = request.instance;
	let core = state.core.clone();

	// Snapshotting locks the tree and may walk a large subtree, so it runs on
	// the blocking thread pool
	let snapshot = task::spawn_blocking(move || core.snapshot(instance)).await;

	match snapshot {
		Ok(snapshot) => MsgPack(snapshot).into_response(),
		Err(err) => (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()).into_response(),
	}
}
