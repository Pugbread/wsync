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
	_line: u32,
}

pub async fn main(State(state): State<AppState>, request: MsgPack<Request>) -> Response {
	trace!("Received request: open");

	let instance = request.instance;
	let core = state.core.clone();

	// Opening the file launches an external editor, so it runs on the
	// blocking thread pool
	let result = task::spawn_blocking(move || core.open(instance)).await;

	match result {
		Ok(Ok(_)) => (StatusCode::OK, "Opened file successfully").into_response(),
		Ok(Err(err)) => (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()).into_response(),
		Err(err) => (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()).into_response(),
	}
}
