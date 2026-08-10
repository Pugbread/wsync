use axum::{
	body::Bytes,
	extract::State,
	http::StatusCode,
	response::{IntoResponse, Response},
};
use log::{debug, info, trace};
use serde::Deserialize;
use std::{
	sync::Mutex,
	time::{Duration, Instant},
};
use tokio::sync::watch;

use crate::{
	daemon::{self, HEARTBEAT_SUSPECT_GRACE, MANAGER_HEARTBEAT_TIMEOUT},
	lock,
	server::AppState,
};

/// How often the watchdog re-checks the manager heartbeat
const WATCHDOG_INTERVAL: Duration = Duration::from_secs(2);

/// Per-boot lifecycle state: the graceful-shutdown trigger and, for managed
/// daemons, the manager heartbeat the watchdog guards
pub struct Lifecycle {
	shutdown: watch::Sender<Option<String>>,
	manager_last_seen: Mutex<Instant>,
	managed: bool,
}

impl Lifecycle {
	pub fn new(managed: bool) -> Self {
		let (shutdown, _) = watch::channel(None);

		Self {
			shutdown,
			// The clock starts at boot so a manager that never heartbeats
			// still trips the watchdog after the full timeout
			manager_last_seen: Mutex::new(Instant::now()),
			managed,
		}
	}

	pub fn managed(&self) -> bool {
		self.managed
	}

	pub fn subscribe(&self) -> watch::Receiver<Option<String>> {
		self.shutdown.subscribe()
	}

	/// Triggers a graceful daemon shutdown (idempotent; the first reason wins)
	pub fn trigger_shutdown(&self, reason: &str) {
		self.shutdown.send_if_modified(|current| {
			if current.is_none() {
				info!("Daemon shutdown requested: {reason}");
				*current = Some(reason.to_owned());
				true
			} else {
				false
			}
		});
	}

	pub fn heartbeat(&self) {
		*lock!(self.manager_last_seen) = Instant::now();
	}

	fn manager_last_seen(&self) -> Instant {
		*lock!(self.manager_last_seen)
	}
}

/// Spawns the manager watchdog (managed daemons only): a lost heartbeat past
/// the 5-minute timeout enters a 30-second "suspect" window first, so a
/// machine waking from sleep gives its manager a chance to heartbeat before
/// the daemon self-terminates (Ro-Sync's laptop-sleep-tolerant semantics)
pub fn spawn_watchdog(state: AppState) {
	if !state.lifecycle.managed() {
		return;
	}

	tokio::spawn(async move {
		let mut interval = tokio::time::interval(WATCHDOG_INTERVAL);
		let mut suspect_since: Option<Instant> = None;

		loop {
			interval.tick().await;

			let last_seen_elapsed = state.lifecycle.manager_last_seen().elapsed();

			if !daemon::heartbeat_expired(last_seen_elapsed, MANAGER_HEARTBEAT_TIMEOUT) {
				suspect_since = None;
				continue;
			}

			let first_suspect = *suspect_since.get_or_insert_with(Instant::now);

			if daemon::watchdog_should_terminate(
				last_seen_elapsed,
				Some(first_suspect.elapsed()),
				MANAGER_HEARTBEAT_TIMEOUT,
				HEARTBEAT_SUSPECT_GRACE,
			) {
				state
					.lifecycle
					.trigger_shutdown("Manager heartbeat lost (watchdog timeout)");
				break;
			}
		}
	});
}

#[derive(Deserialize, Debug, Default)]
#[serde(rename_all = "camelCase")]
struct ControlBody {
	token: Option<String>,
}

/// Returns the refusal response when the request does not carry this managed
/// daemon's owner token, `None` when it is authorized
fn refusal(state: &AppState, body: &Bytes) -> Option<Response> {
	let Some(expected) = &state.identity.control_token else {
		return Some((StatusCode::FORBIDDEN, "This daemon is not managed").into_response());
	};

	let body: ControlBody = if body.is_empty() {
		ControlBody::default()
	} else {
		match serde_json::from_slice(body) {
			Ok(body) => body,
			Err(_) => return Some((StatusCode::BAD_REQUEST, "Body must be JSON with a token field").into_response()),
		}
	};

	match body.token {
		None => Some((StatusCode::UNAUTHORIZED, "Missing owner token").into_response()),
		Some(token) if &token == expected => None,
		Some(_) => Some((StatusCode::FORBIDDEN, "Invalid owner token").into_response()),
	}
}

/// `POST /manager-heartbeat` — the lifecycle manager (desktop app) proves it
/// is still alive; managed daemons self-terminate without it (Design §3.3)
pub async fn heartbeat(State(state): State<AppState>, body: Bytes) -> Response {
	trace!("Received request: manager-heartbeat");

	if let Some(response) = refusal(&state, &body) {
		return response;
	}

	state.lifecycle.heartbeat();

	StatusCode::NO_CONTENT.into_response()
}

/// `POST /manager-close` — authenticated graceful shutdown for the lifecycle
/// manager
pub async fn close(State(state): State<AppState>, body: Bytes) -> Response {
	trace!("Received request: manager-close");

	if let Some(response) = refusal(&state, &body) {
		return response;
	}

	debug!("Manager requested daemon close");
	state.lifecycle.trigger_shutdown("Manager requested close");

	StatusCode::NO_CONTENT.into_response()
}
