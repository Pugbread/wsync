use axum::{
	extract::State,
	http::StatusCode,
	response::{IntoResponse, Response},
};
use log::trace;

use crate::{
	core::processor::WriteRequest,
	server::{self, ws::frames::Event, AppState, MsgPack},
};

pub async fn main(State(state): State<AppState>, request: MsgPack<WriteRequest>) -> Response {
	trace!("Received request: write");

	let request = request.0;

	if !state.core.queue().is_subscribed(request.client_id) {
		return (StatusCode::UNAUTHORIZED, "Not subscribed").into_response();
	}

	state.ws.touch_longpoll(request.client_id);
	state
		.ws
		.emit(Event::sync_activity(server::DIRECTION_STUDIO_TO_DISK, &request.changes));

	state.core.processor().write(request);

	(StatusCode::OK, "Written changes successfully").into_response()
}
