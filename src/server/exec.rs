use axum::{
	extract::State,
	http::StatusCode,
	response::{IntoResponse, Response},
};
use log::{error, trace};
use serde::Deserialize;
use tokio::task;

use crate::{
	server::{self, AppState, MsgPack},
	studio,
};

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Request {
	code: String,
	focus: bool,
}

pub async fn main(State(state): State<AppState>, request: MsgPack<Request>) -> Response {
	trace!("Received request: exec");

	let queue = state.core.queue();

	let pushed = queue.push(
		server::ExecuteCode {
			code: request.code.clone(),
		},
		None,
	);

	if request.focus {
		if let Some(name) = queue.get_first_non_internal_listener_name() {
			// Focusing Roblox Studio shells out to the OS, so it runs on the
			// blocking thread pool
			let result = task::spawn_blocking(move || studio::focus(Some(name))).await;

			match result {
				Ok(Ok(())) => (),
				Ok(Err(err)) => error!("Failed to focus Roblox Studio: {err}"),
				Err(err) => error!("Failed to focus Roblox Studio: {err}"),
			}
		}
	}

	match pushed {
		Ok(()) => (StatusCode::OK, "Code executed successfully").into_response(),
		Err(err) => (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()).into_response(),
	}
}
