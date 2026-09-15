//! Bounded, per-boot operation receipts. These record completion evidence;
//! they are not a durable replay log or a promise of exactly-once execution.

use serde::Serialize;
use std::{
	collections::VecDeque,
	time::{Duration, Instant},
};

use super::ws::frames::ResponseFrame;

const RETAIN_FOR: Duration = Duration::from_secs(10 * 60);
const MAX_RECEIPTS: usize = 256;

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Receipt {
	pub operation_id: String,
	pub request_id: u32,
	pub op: String,
	pub completion: &'static str,
	pub completion_scope: &'static str,
	pub retry_safe: bool,
	pub finished: bool,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub error_code: Option<String>,
	#[serde(skip)]
	created: Instant,
}

#[derive(Default)]
pub struct Receipts(VecDeque<Receipt>);

impl Receipts {
	fn prune(&mut self) {
		self.0.retain(|receipt| receipt.created.elapsed() < RETAIN_FOR);
	}

	pub fn begin(&mut self, request_id: u32, operation_id: &str, op: &str) -> bool {
		self.prune();
		if self.0.iter().any(|receipt| receipt.operation_id == operation_id) {
			return false;
		}
		if self.0.len() >= MAX_RECEIPTS {
			self.0.pop_front();
		}
		self.0.push_back(Receipt {
			operation_id: operation_id.to_owned(),
			request_id,
			op: op.to_owned(),
			completion: "unknown",
			completion_scope: "requestedEffects",
			retry_safe: false,
			finished: false,
			error_code: None,
			created: Instant::now(),
		});
		true
	}

	pub fn finish(&mut self, request_id: u32, completion: &'static str, error_code: Option<&str>) {
		if let Some(receipt) = self.0.iter_mut().find(|receipt| receipt.request_id == request_id) {
			// A duplicate or stale plugin response must not replace conclusive
			// completion evidence already recorded for this request.
			if receipt.finished && receipt.completion != "unknown" {
				return;
			}
			receipt.completion = completion;
			receipt.retry_safe = completion == "notStarted";
			receipt.finished = true;
			receipt.error_code = error_code.map(str::to_owned);
		}
	}

	pub fn get(&mut self, operation_id: &str) -> Option<Receipt> {
		self.prune();
		self.0
			.iter()
			.find(|receipt| receipt.operation_id == operation_id)
			.cloned()
	}

	pub fn op(&self, request_id: u32) -> Option<String> {
		self.0
			.iter()
			.find(|receipt| receipt.request_id == request_id && !receipt.finished)
			.map(|receipt| receipt.op.clone())
	}
}

/// These handlers acknowledge a request or start a background job. Their
/// return does not confirm that saving/history effects or the job completed.
pub fn acknowledgement_only(op: Option<&str>) -> bool {
	matches!(
		op,
		Some("save" | "undo" | "redo" | "playtest_start" | "playtest_run_start")
	)
}

/// A failure alone does not prove that a handler made no changes. An audit
/// explicitly reporting mutation proves partial execution; otherwise only
/// an explicit plugin completion receipt can make a retry safe.
pub fn completion(response: &ResponseFrame, op: Option<&str>) -> &'static str {
	if response.ok && acknowledgement_only(op) {
		return "unknown";
	}
	// These two handlers intentionally return task failures in their value
	// while the dispatcher itself succeeds. Do not treat arbitrary returned
	// user tables (e.g. eval returning `{ok=false}`) as protocol failures.
	let task_failed = match (op, response.value.as_ref()) {
		(Some("playtest_exec"), Some(value)) => value.get("ok").and_then(serde_json::Value::as_bool) == Some(false),
		(Some("set_batch"), Some(value)) => {
			value
				.get("failed")
				.and_then(serde_json::Value::as_u64)
				.is_some_and(|failed| failed > 0)
				|| value
					.get("results")
					.and_then(serde_json::Value::as_array)
					.is_some_and(|results| {
						results
							.iter()
							.any(|entry| entry.get("ok").and_then(serde_json::Value::as_bool) == Some(false))
					})
		}
		_ => false,
	};
	if response.ok && !task_failed {
		return "completed";
	}
	if op == Some("set_batch")
		&& response
			.value
			.as_ref()
			.and_then(|value| value.get("applied"))
			.and_then(serde_json::Value::as_u64)
			.is_some_and(|applied| applied > 0)
	{
		return "partial";
	}
	if response
		.meta
		.as_ref()
		.and_then(|meta| meta.get("mutation"))
		.and_then(serde_json::Value::as_bool)
		== Some(true)
	{
		return "partial";
	}
	match response
		.meta
		.as_ref()
		.and_then(|meta| meta.get("completion"))
		.and_then(serde_json::Value::as_str)
	{
		Some("notStarted") => "notStarted",
		Some("completed") => "completed",
		Some("partial") => "partial",
		_ => "unknown",
	}
}
