use axum::{
	extract::{Path, State},
	response::Json,
};
use log::trace;
use serde::Deserialize;
use serde_json::{json, Value};
use std::time::{Duration, Instant};
use uuid::Uuid;

use crate::{
	constants::{PROTOCOL_VERSION, REQUEST_DEFAULT_TIMEOUT_MS, REQUEST_MAX_TIMEOUT_MS},
	daemon,
	server::{
		operation, ops,
		ws::frames::{codes, OpError, ServerFrame},
		AppState,
	},
};

/// Remote requests may name an operation and pin the daemon identity observed
/// during discovery. Older clients can omit the additive fields.
#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Request {
	op: String,
	#[serde(default)]
	args: Option<Value>,
	timeout_ms: Option<u64>,
	operation_id: Option<String>,
	expected_project: Option<String>,
	expected_boot_id: Option<String>,
}

struct Reply {
	op: String,
	operation_id: String,
	request_id: u32,
	started: Instant,
}

impl Reply {
	fn value(&self, value: Value) -> Json<Value> {
		self.make(true, ("value", value), "completed", None)
	}

	fn error(&self, code: &str, message: &str, completion: &'static str) -> Json<Value> {
		self.make(
			false,
			("error", json!({ "code": code, "message": message })),
			completion,
			None,
		)
	}

	fn make(
		&self,
		ok: bool,
		payload: (&str, Value),
		completion: &'static str,
		plugin_meta: Option<Value>,
	) -> Json<Value> {
		let (key, value) = payload;
		let mut body = json!({
			"ok": ok,
			key: value,
			"meta": {
				"op": self.op,
				"durationMs": self.started.elapsed().as_millis() as u64,
				"protocol": PROTOCOL_VERSION,
				"operationId": self.operation_id,
				"requestId": self.request_id,
				"completion": completion,
				"completionScope": "requestedEffects",
				"retrySafe": completion == "notStarted",
				"statusPath": format!("/operations/{}", self.operation_id),
			},
		});
		if let Some(meta) = plugin_meta {
			body["meta"]["plugin"] = meta;
		}
		if ok && operation::acknowledgement_only(Some(&self.op)) {
			body["meta"]["acknowledged"] = json!(true);
			if let Some(job_id) = body.pointer("/value/jobId").cloned() {
				body["meta"]["jobId"] = job_id;
				body["meta"]["nextCheck"] = json!("Read playtest_status or playtest_run_poll for this job; operation receipts do not track background job completion.");
			} else {
				body["meta"]["nextCheck"] = json!("Studio accepted the request; inspect Studio state/output to confirm its effects before repeating it.");
			}
		}
		if !ok && completion != "notStarted" {
			body["meta"]["recovery"] = json!("Inspect the operation receipt and affected Studio state before retrying; the failed command may have applied changes.");
		}
		Json(body)
	}
}

/// Read-only, bounded receipt lookup. Missing means unknown: receipts expire,
/// can be evicted, and do not survive a daemon restart.
pub async fn status(State(state): State<AppState>, Path(operation_id): Path<String>) -> Json<Value> {
	let receipt = state.ws.operation_receipt(&operation_id);
	Json(json!({
		"ok": true,
		"found": receipt.is_some(),
		"receipt": receipt,
		"bootId": state.identity.boot_id,
		"retention": { "maxReceipts": 256, "seconds": 600, "durable": false },
		"missingMeans": "unknown; the operation may have run before receipt expiry, eviction, or a daemon restart",
	}))
}

pub async fn main(State(state): State<AppState>, Json(request): Json<Request>) -> Json<Value> {
	trace!("Received request: request (op {})", request.op);
	let op = request.op;
	let operation_id = request.operation_id.unwrap_or_else(|| Uuid::new_v4().to_string());
	let reply = Reply {
		op: op.clone(),
		operation_id: operation_id.clone(),
		request_id: state.ws.next_request_id(),
		started: Instant::now(),
	};

	if op.is_empty() || op.len() > 128 {
		return reply.error(codes::INVALID_ARGUMENT, "op must contain 1–128 bytes", "notStarted");
	}
	if Uuid::parse_str(&operation_id).is_err() {
		return reply.error(codes::INVALID_ARGUMENT, "operationId must be a UUID", "notStarted");
	}

	if request
		.expected_boot_id
		.as_ref()
		.is_some_and(|boot| boot != &state.identity.boot_id)
	{
		return reply.error(
			codes::CONFLICT,
			"Daemon restarted after discovery; reconnect and verify the project before sending this operation",
			"notStarted",
		);
	}
	if let Some(expected) = request.expected_project {
		let actual = daemon::canonical_project(&state.core.project().path);
		if actual.as_ref().ok() != Some(&expected) {
			return reply.error(
				codes::CONFLICT,
				"Daemon project does not match the project verified during discovery",
				"notStarted",
			);
		}
	}
	if !state.ws.begin_operation(reply.request_id, &operation_id, &op) {
		// Do not call this notStarted: the earlier use of this ID may already
		// have mutated Studio. Clients must consult that receipt.
		return reply.error(
			codes::CONFLICT,
			"operationId already has a receipt; inspect it before creating a new operation",
			"unknown",
		);
	}
	let fail_before_send = |code: &str, message: &str| {
		state.ws.finish_operation(reply.request_id, "notStarted", Some(code));
		reply.error(code, message, "notStarted")
	};
	let timeout_ms = match request.timeout_ms {
		None => REQUEST_DEFAULT_TIMEOUT_MS,
		Some(0) => return fail_before_send(codes::INVALID_ARGUMENT, "timeoutMs must be at least 1"),
		Some(ms) => ms.min(REQUEST_MAX_TIMEOUT_MS),
	};
	let args = request.args.unwrap_or_else(|| json!({}));

	if let Some(result) = ops::try_answer(&state.core, &op, &args) {
		return match result {
			Ok(value) => {
				state.ws.finish_operation(reply.request_id, "completed", None);
				reply.value(value)
			}
			Err((code, message)) => fail_before_send(code, &message),
		};
	}
	let plugin = match state.ws.plugin_frames() {
		Ok(frames) => frames,
		Err(reason) => return fail_before_send(codes::PLUGIN_ERROR, reason),
	};
	let receiver = state.ws.register_request(reply.request_id);
	let frame = ServerFrame::Request {
		request_id: reply.request_id,
		op: op.clone(),
		args,
		timeout_ms,
	};
	if plugin.try_send(frame.to_text()).is_err() {
		state.ws.abort_request(reply.request_id);
		return fail_before_send(
			codes::PLUGIN_ERROR,
			"Plugin outbound queue is overloaded or the plugin disconnected before dispatch",
		);
	}

	match tokio::time::timeout(Duration::from_millis(timeout_ms), receiver).await {
		Ok(Ok(response)) => {
			let completion = operation::completion(&response, Some(&op));
			if response.ok {
				reply.make(
					true,
					("value", response.value.unwrap_or(Value::Null)),
					completion,
					response.meta,
				)
			} else {
				let error = response.error.unwrap_or_else(|| OpError {
					code: codes::PLUGIN_ERROR.to_owned(),
					message: "Plugin reported failure without error detail".to_owned(),
				});
				reply.make(
					false,
					("error", json!({ "code": error.code, "message": error.message })),
					completion,
					response.meta,
				)
			}
		}
		Ok(Err(_)) => {
			state
				.ws
				.finish_operation(reply.request_id, "unknown", Some(codes::PLUGIN_ERROR));
			reply.error(
				codes::PLUGIN_ERROR,
				"Studio plugin disconnected before answering; execution may have started or completed",
				"unknown",
			)
		}
		Err(_) => {
			state.ws.abort_request(reply.request_id);
			// Do not overwrite a racing or later plugin receipt. No response
			// by the deadline means unknown completion, never rollback.
			reply.error(
				codes::TIMEOUT,
				&format!("No plugin response within {timeout_ms} ms; execution may have started or completed"),
				"unknown",
			)
		}
	}
}
