use axum::{
	extract::State,
	http::StatusCode,
	response::{IntoResponse, Response},
};
use log::trace;

use crate::server::{ws::frames::Event, AppState, AuthRequest, MsgPack};

pub async fn main(State(state): State<AppState>, request: MsgPack<AuthRequest>) -> Response {
	trace!("Received request: unsubscribe");

	let unsubscribed = state.core.queue().unsubscribe(request.client_id);

	if unsubscribed.is_ok() {
		if let Some(released) = state.ws.release_plugin(request.client_id) {
			state.ws.emit(Event::PluginStatus {
				connected: false,
				name: Some(released.name),
				transport: Some("long-poll".into()),
			});

			// Stale-set rule (Design §7.2): the plugin going away
			// invalidates any pending divergence set
			state.divergence.invalidate("plugin unsubscribed");
		}

		(StatusCode::OK, "Unsubscribed successfully").into_response()
	} else {
		(StatusCode::BAD_REQUEST, "Not subscribed").into_response()
	}
}
