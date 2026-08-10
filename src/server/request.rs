use axum::{extract::State, response::Json};
use log::trace;
use serde::Deserialize;
use serde_json::{json, Value};
use std::time::{Duration, Instant};

use crate::{
	constants::{PROTOCOL_VERSION, REQUEST_DEFAULT_TIMEOUT_MS, REQUEST_MAX_TIMEOUT_MS},
	server::{
		ops,
		ws::frames::{codes, OpError, ServerFrame},
		AppState,
	},
};

/// The audit `source` recorded for HTTP `/request` callers (the CLI surface;
/// WS-originated requests would carry their client's role instead)
const HTTP_REQUEST_SOURCE: &str = "cli";

/// `POST /request` body: a one-shot remote op from the CLI or an agent.
/// Daemon-side ops (`path`, `meta`, `where`) are answered locally; everything
/// else is routed to the connected Studio plugin over WS `request`/`response`
#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Request {
	op: String,
	#[serde(default)]
	args: Option<Value>,
	timeout_ms: Option<u64>,
}

fn reply(ok: bool, payload: (&'static str, Value), op: &str, started: Instant) -> Json<Value> {
	let (key, value) = payload;

	Json(json!({
		"ok": ok,
		key: value,
		"meta": {
			"op": op,
			"durationMs": started.elapsed().as_millis() as u64,
			"protocol": PROTOCOL_VERSION,
		},
	}))
}

fn error_reply(code: &str, message: &str, op: &str, started: Instant) -> Json<Value> {
	reply(
		false,
		("error", json!({ "code": code, "message": message })),
		op,
		started,
	)
}

pub async fn main(State(state): State<AppState>, Json(request): Json<Request>) -> Json<Value> {
	trace!("Received request: request (op {})", request.op);

	let started = Instant::now();
	let op = request.op.clone();

	if op.is_empty() {
		return error_reply(codes::INVALID_ARGUMENT, "op must be a non-empty string", &op, started);
	}

	let args = request.args.unwrap_or_else(|| json!({}));

	// Daemon-side ops answer from the live tree and never touch the plugin,
	// so they work with no Studio connected (registry contracts path/meta/where)
	if let Some(result) = ops::try_answer(&state.core, &op, &args) {
		return match result {
			Ok(value) => reply(true, ("value", value), &op, started),
			Err((code, message)) => error_reply(code, &message, &op, started),
		};
	}

	let timeout_ms = match request.timeout_ms {
		None => REQUEST_DEFAULT_TIMEOUT_MS,
		Some(0) => {
			return error_reply(codes::INVALID_ARGUMENT, "timeoutMs must be at least 1", &op, started);
		}
		// Values above the cap are clamped rather than rejected
		Some(ms) => ms.min(REQUEST_MAX_TIMEOUT_MS),
	};

	let plugin = match state.ws.plugin_frames() {
		Ok(frames) => frames,
		Err(reason) => return error_reply(codes::PLUGIN_ERROR, reason, &op, started),
	};

	let request_id = state.ws.next_request_id();
	let receiver = state.ws.register_request(request_id);

	let frame = ServerFrame::Request {
		request_id,
		op: op.clone(),
		args,
		timeout_ms,
	};

	if plugin.try_send(frame.to_text()).is_err() {
		state.ws.abort_request(request_id);

		return error_reply(
			codes::PLUGIN_ERROR,
			"Plugin outbound queue is overloaded or the plugin disconnected",
			&op,
			started,
		);
	}

	match tokio::time::timeout(Duration::from_millis(timeout_ms), receiver).await {
		// Plugin answered
		Ok(Ok(response)) => {
			// Every response that reports a Studio mutation lands in the
			// audit log (Design §6.4/§11), success or failure alike
			if let Some(meta) = response.meta.as_ref() {
				if meta.get("mutation").and_then(Value::as_bool) == Some(true) {
					state.audit.record(&op, response.ok, HTTP_REQUEST_SOURCE, meta);
				}
			}

			if response.ok {
				reply(true, ("value", response.value.unwrap_or(Value::Null)), &op, started)
			} else {
				let error = response.error.unwrap_or_else(|| OpError {
					code: codes::PLUGIN_ERROR.to_owned(),
					message: "Plugin reported failure without error detail".to_owned(),
				});

				reply(
					false,
					("error", json!({ "code": error.code, "message": error.message })),
					&op,
					started,
				)
			}
		}
		// Plugin disconnected while the request was pending
		Ok(Err(_)) => error_reply(
			codes::PLUGIN_ERROR,
			"Studio plugin disconnected before answering",
			&op,
			started,
		),
		// Deadline elapsed
		Err(_) => {
			state.ws.abort_request(request_id);

			error_reply(
				codes::TIMEOUT,
				&format!("No plugin response within {timeout_ms} ms"),
				&op,
				started,
			)
		}
	}
}
