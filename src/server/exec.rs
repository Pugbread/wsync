use axum::{
	extract::State,
	http::StatusCode,
	response::{IntoResponse, Response},
};
use log::{error, trace};
use serde::Deserialize;
use tokio::task;

use crate::{
	daemon,
	server::{self, AppState, MsgPack},
	studio,
};

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Request {
	code: String,
	focus: bool,
	expected_project: Option<String>,
	expected_boot_id: Option<String>,
}

pub async fn main(State(state): State<AppState>, request: MsgPack<Request>) -> Response {
	trace!("Received request: exec");
	if request
		.expected_boot_id
		.as_ref()
		.is_some_and(|boot| boot != &state.identity.boot_id)
	{
		return (
			StatusCode::CONFLICT,
			"Daemon restarted after discovery; no code was queued",
		)
			.into_response();
	}
	if let Some(expected) = &request.expected_project {
		if daemon::canonical_project(&state.core.project().path).as_ref().ok() != Some(expected) {
			return (
				StatusCode::CONFLICT,
				"Daemon project does not match discovery; no code was queued",
			)
				.into_response();
		}
	}

	let queue = state.core.queue();
	if queue.get_first_non_internal_listener_name().is_none() {
		return (
			StatusCode::SERVICE_UNAVAILABLE,
			"No Studio plugin is connected; no code was queued",
		)
			.into_response();
	}

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
		Ok(()) => (StatusCode::OK, "Code queued; execution completion is unknown").into_response(),
		Err(err) => (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()).into_response(),
	}
}
