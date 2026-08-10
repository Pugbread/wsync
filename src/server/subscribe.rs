use axum::{
	extract::State,
	http::StatusCode,
	response::{IntoResponse, Response},
};
use log::trace;
use serde::Deserialize;

use crate::server::{
	ws::{frames::Event, state::ClaimError},
	AppState, MsgPack,
};

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Request {
	client_id: u32,
	name: String,
}

/// msgpack `POST /subscribe` — the long-poll plugin connect. A long-poll
/// plugin occupies the same single-plugin slot as a WS plugin: one Studio
/// connection owns the live bridge at a time, whichever transport it uses
pub async fn main(State(state): State<AppState>, request: MsgPack<Request>) -> Response {
	trace!("Received request: subscribe");

	match state
		.ws
		.claim_longpoll_plugin(&state.core.queue(), request.client_id, &request.name)
	{
		Ok(()) => {
			state.ws.emit(Event::PluginStatus {
				connected: true,
				name: Some(request.name.clone()),
				transport: Some("long-poll".into()),
			});

			// The generated agent docs regenerate on every plugin connect
			// (Design §10.6) — debounced, off this handshake, warn-only
			crate::cli::refresh::auto_refresh_on_connect(state.core.clone());

			(StatusCode::OK, "Subscribed successfully").into_response()
		}
		Err(ClaimError::AlreadySubscribed) => (StatusCode::BAD_REQUEST, "Already subscribed").into_response(),
		Err(ClaimError::SlotTaken(existing)) => (
			StatusCode::CONFLICT,
			format!("Another Studio plugin is already connected ({existing})"),
		)
			.into_response(),
	}
}
