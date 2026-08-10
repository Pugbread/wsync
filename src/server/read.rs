use axum::{
	extract::State,
	http::StatusCode,
	response::{IntoResponse, Response},
};
use log::trace;
use tokio::task;

use crate::server::{self, ws::frames::Event, AppState, AuthRequest, Message, MsgPack};

pub async fn main(State(state): State<AppState>, request: MsgPack<AuthRequest>) -> Response {
	trace!("Received request: read");

	let id = request.client_id;
	let queue = state.core.queue();

	if !queue.is_subscribed(id) {
		return (StatusCode::UNAUTHORIZED, "Not subscribed").into_response();
	}

	// Polling proves the long-poll plugin is alive, which keeps its claim on
	// the single-plugin slot fresh
	state.ws.touch_longpoll(id);

	// The queue receive blocks for up to 60 seconds (long polling), so it has
	// to run on the blocking thread pool to keep the async runtime responsive
	let message = task::spawn_blocking(move || queue.get_timeout(id)).await;

	match message {
		Ok(Ok(message)) => {
			if let Some(Message::SyncChanges(server::SyncChanges(changes))) = &message {
				state
					.ws
					.emit(Event::sync_activity(server::DIRECTION_DISK_TO_STUDIO, changes));
			}

			MsgPack(message).into_response()
		}
		Ok(Err(err)) => (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()).into_response(),
		Err(err) => (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()).into_response(),
	}
}
