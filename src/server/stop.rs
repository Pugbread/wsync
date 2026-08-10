use axum::{
	body::Bytes,
	extract::State,
	http::StatusCode,
	response::{IntoResponse, Response},
};
use log::{info, trace};
use serde::Deserialize;

use crate::server::AppState;

#[derive(Deserialize, Debug, Default)]
#[serde(rename_all = "camelCase")]
struct StopBody {
	boot_id: Option<String>,
	token: Option<String>,
}

/// `POST /stop` — authenticated shutdown (Design §5.2). Daemons started
/// managed (with an owner token) require a JSON body whose `bootId` and
/// `token` both match this boot exactly; unmanaged daemons keep accepting an
/// empty body for argon-plugin and `wsync stop` compatibility. Shutdown is
/// graceful: connected clients get typed `shutdown` frames and the record
/// cleanup runs in the daemon's foreground process
pub async fn main(State(state): State<AppState>, body: Bytes) -> Response {
	trace!("Received request: stop");

	if let Some(expected_token) = &state.identity.control_token {
		let body: StopBody = if body.is_empty() {
			StopBody::default()
		} else {
			match serde_json::from_slice(&body) {
				Ok(body) => body,
				Err(_) => {
					return (
						StatusCode::BAD_REQUEST,
						"Body must be JSON with bootId and token fields",
					)
						.into_response();
				}
			}
		};

		let (Some(boot_id), Some(token)) = (body.boot_id, body.token) else {
			return (
				StatusCode::UNAUTHORIZED,
				"This daemon is managed; stopping it requires bootId and token",
			)
				.into_response();
		};

		if boot_id != state.identity.boot_id || &token != expected_token {
			return (StatusCode::FORBIDDEN, "Invalid bootId or token").into_response();
		}
	}

	info!("Stopping WSync!");
	state.lifecycle.trigger_shutdown("Stop requested");

	(StatusCode::OK, "WSync stopped successfully").into_response()
}
