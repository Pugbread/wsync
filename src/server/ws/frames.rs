use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::VecDeque;

use crate::{
	constants::{PROTOCOL_VERSION, SYNC_FRAME_MAX_BYTES, SYNC_FRAME_MAX_OPS},
	core::changes::Changes,
	project::ProjectDetails,
	util,
};

/// Remote-op error codes (Design §5.4, Ro-Sync's set kept verbatim)
pub mod codes {
	pub const UNKNOWN_OP: &str = "UNKNOWN_OP";
	pub const NOT_FOUND: &str = "NOT_FOUND";
	pub const PERMISSION_REQUIRED: &str = "PERMISSION_REQUIRED";
	pub const TIMEOUT: &str = "TIMEOUT";
	pub const INVALID_ARGUMENT: &str = "INVALID_ARGUMENT";
	pub const CONFLICT: &str = "CONFLICT";
	pub const PLUGIN_ERROR: &str = "PLUGIN_ERROR";
}

/// Typed `shutdown` frame codes
pub mod shutdown {
	pub const BAD_HELLO: &str = "BAD_HELLO";
	pub const HELLO_TIMEOUT: &str = "HELLO_TIMEOUT";
	pub const PROTOCOL_MISMATCH: &str = "PROTOCOL_MISMATCH";
	pub const PLUGIN_SLOT_TAKEN: &str = "PLUGIN_SLOT_TAKEN";
	pub const HEARTBEAT_TIMEOUT: &str = "HEARTBEAT_TIMEOUT";
	pub const SLOW_CONSUMER: &str = "SLOW_CONSUMER";
	pub const OUT_OF_SYNC: &str = "OUT_OF_SYNC";
	pub const DAEMON_STOPPING: &str = "DAEMON_STOPPING";
}

/// Server → client frames. Every frame is a flat JSON object with a `type`
/// tag and its payload fields inline (Design §5.3, normative envelope note) —
/// never a nested single-key wrapper
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ServerFrame {
	#[serde(rename = "hello")]
	Hello(ServerHello),
	#[serde(rename = "sync")]
	Sync {
		#[serde(flatten)]
		changes: Changes,
	},
	#[serde(rename = "details")]
	Details {
		#[serde(flatten)]
		details: ProjectDetails,
	},
	#[serde(rename = "execute")]
	Execute { code: String },
	#[serde(rename = "push-result")]
	PushResult {
		ok: bool,
		applied: usize,
		skipped: usize,
		conflicts: usize,
		errors: Vec<String>,
	},
	#[serde(rename = "request")]
	Request {
		request_id: u32,
		op: String,
		args: Value,
		timeout_ms: u64,
	},
	#[serde(rename = "event")]
	Event {
		#[serde(flatten)]
		event: Event,
	},
	#[serde(rename = "shutdown")]
	Shutdown {
		reason: String,
		code: String,
		retryable: bool,
	},
	#[serde(rename = "ping")]
	Ping,
	#[serde(rename = "pong")]
	Pong,
}

impl ServerFrame {
	pub fn to_text(&self) -> String {
		serde_json::to_string(self).expect("Failed to serialize protocol frame")
	}

	pub fn shutdown(reason: &str, code: &str, retryable: bool) -> Self {
		Self::Shutdown {
			reason: reason.to_owned(),
			code: code.to_owned(),
			retryable,
		}
	}
}

/// The daemon's WS hello (Design §5.3 + the Phase-2 ruling): real version,
/// project identity, and hex-encoded root refs. `scope` is additive (Design
/// §7.0): `"code"` or `"full"`; older daemons omit it, which clients read as
/// full
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerHello {
	pub name: String,
	pub version: String,
	pub game_id: Option<u64>,
	pub place_ids: Vec<u64>,
	pub root_refs: Vec<String>,
	pub scope: String,
}

/// Sanitized `event` frame payloads for `watch`/`app` clients. Only counts,
/// short instance names, repo-relative paths and daemon lifecycle notes ever
/// leave the daemon — never full snapshots or absolute paths
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "topic")]
pub enum Event {
	#[serde(rename = "sync-activity")]
	SyncActivity {
		direction: String,
		additions: usize,
		updates: usize,
		removals: usize,
		names: Vec<String>,
	},
	#[serde(rename = "plugin-status")]
	PluginStatus {
		connected: bool,
		name: Option<String>,
		transport: Option<String>,
	},
	#[serde(rename = "daemon")]
	Daemon { note: String },
	/// A conflict was parked (Design §6.3); pinned payload shape
	#[serde(rename = "conflict")]
	Conflict {
		id: String,
		path: Option<String>,
		#[serde(rename = "instancePath")]
		instance_path: String,
		classification: String,
	},
	/// A divergence set was frozen and awaits a decision (Design §7.2);
	/// aggregate stats only — details are paged on demand
	#[serde(rename = "choice-needed")]
	ChoiceNeeded {
		#[serde(rename = "choiceId")]
		choice_id: String,
		total: usize,
		#[serde(rename = "studioCount")]
		studio_count: usize,
		#[serde(rename = "diskCount")]
		disk_count: usize,
		#[serde(rename = "onlyOnDisk")]
		only_on_disk: usize,
		differs: usize,
		#[serde(rename = "missingOnDisk")]
		missing_on_disk: usize,
	},
	/// A pending divergence choice was decided (or cancelled)
	#[serde(rename = "choice-made")]
	ChoiceMade {
		#[serde(rename = "choiceId")]
		choice_id: String,
		choice: String,
	},
	/// A Studio-first auto-apply landed and left disk-side entries to review
	/// (Design §7.0); counts only — details are paged via `GET /review`
	#[serde(rename = "disk-review")]
	DiskReview {
		#[serde(rename = "reviewId")]
		review_id: String,
		total: usize,
		#[serde(rename = "diskOnly")]
		disk_only: usize,
		differs: usize,
	},
	/// Disk content lost to Studio and was moved to the backlog. Counts only:
	/// the entries themselves are read from `GET /backlog`
	#[serde(rename = "backlog")]
	Backlog { total: usize, added: usize },
}

pub const EVENT_TOPICS: [&str; 7] = [
	"sync-activity",
	"plugin-status",
	"daemon",
	"conflict",
	"choice-needed",
	"choice-made",
	"disk-review",
];

/// Longest instance-name list and name length included in a sanitized
/// `sync-activity` event
const ACTIVITY_MAX_NAMES: usize = 10;
const ACTIVITY_MAX_NAME_LEN: usize = 120;

impl Event {
	pub fn topic(&self) -> &'static str {
		match self {
			Self::SyncActivity { .. } => "sync-activity",
			Self::PluginStatus { .. } => "plugin-status",
			Self::Daemon { .. } => "daemon",
			Self::Conflict { .. } => "conflict",
			Self::ChoiceNeeded { .. } => "choice-needed",
			Self::ChoiceMade { .. } => "choice-made",
			Self::DiskReview { .. } => "disk-review",
			Self::Backlog { .. } => "backlog",
		}
	}

	pub fn sync_activity(direction: &str, changes: &Changes) -> Self {
		let mut names = Vec::new();

		let mut push_name = |name: &str| {
			if names.len() < ACTIVITY_MAX_NAMES {
				names.push(name.chars().take(ACTIVITY_MAX_NAME_LEN).collect());
			}
		};

		for addition in &changes.additions {
			push_name(&addition.name);
		}

		for update in &changes.updates {
			if let Some(name) = &update.name {
				push_name(name);
			}
		}

		Self::SyncActivity {
			direction: direction.to_owned(),
			additions: changes.additions.len(),
			updates: changes.updates.len(),
			removals: changes.removals.len(),
			names,
		}
	}
}

/// Client roles accepted at hello (Design §5.3)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientRole {
	Plugin,
	Agent,
	Watch,
	App,
}

impl ClientRole {
	fn parse(role: &str) -> Option<Self> {
		match role {
			"plugin" => Some(Self::Plugin),
			"agent" => Some(Self::Agent),
			"watch" => Some(Self::Watch),
			"app" => Some(Self::App),
			_ => None,
		}
	}

	pub fn as_str(&self) -> &'static str {
		match self {
			Self::Plugin => "plugin",
			Self::Agent => "agent",
			Self::Watch => "watch",
			Self::App => "app",
		}
	}
}

#[derive(Debug, Clone)]
pub struct ClientHello {
	pub client_id: String,
	pub role: ClientRole,
	pub name: String,
}

/// A hello refusal that maps onto a typed, non-retryable `shutdown` frame
#[derive(Debug)]
pub struct HelloError {
	pub reason: String,
	pub code: &'static str,
}

impl HelloError {
	fn new(reason: impl Into<String>, code: &'static str) -> Self {
		Self {
			reason: reason.into(),
			code,
		}
	}
}

/// Parses and validates a client hello frame. Protocol and shape errors are
/// reported as typed shutdown reasons so misbehaving clients stop retrying
pub fn parse_hello(text: &str) -> Result<ClientHello, HelloError> {
	let value: Value = serde_json::from_str(text)
		.map_err(|err| HelloError::new(format!("Hello frame is not valid JSON: {err}"), shutdown::BAD_HELLO))?;

	if value.get("type").and_then(Value::as_str) != Some("hello") {
		return Err(HelloError::new(
			"First frame must be a hello frame",
			shutdown::BAD_HELLO,
		));
	}

	let protocol = value
		.get("protocol")
		.and_then(Value::as_u64)
		.ok_or_else(|| HelloError::new("Hello frame is missing the protocol field", shutdown::PROTOCOL_MISMATCH))?;

	if protocol != PROTOCOL_VERSION as u64 {
		return Err(HelloError::new(
			format!("Unsupported protocol {protocol}; this daemon speaks wsync/{PROTOCOL_VERSION}"),
			shutdown::PROTOCOL_MISMATCH,
		));
	}

	let role = value
		.get("role")
		.and_then(Value::as_str)
		.and_then(ClientRole::parse)
		.ok_or_else(|| {
			HelloError::new(
				"Hello frame is missing a valid role (plugin, agent, watch or app)",
				shutdown::BAD_HELLO,
			)
		})?;

	let client_id = match value.get("clientId") {
		Some(Value::String(id)) if !id.is_empty() => id.clone(),
		Some(Value::Number(id)) => id.to_string(),
		_ => {
			return Err(HelloError::new(
				"Hello frame is missing a clientId",
				shutdown::BAD_HELLO,
			))
		}
	};

	let name = value
		.get("name")
		.and_then(Value::as_str)
		.unwrap_or("unnamed client")
		.to_owned();

	Ok(ClientHello { client_id, role, name })
}

/// Client → server frames after the hello handshake
#[derive(Debug)]
pub enum Incoming {
	Push(Changes),
	/// A `push` frame that failed strict validation or decoding; the whole
	/// frame is rejected (never partially applied) and the reason goes back
	/// in a `push-result` frame
	PushRejected(String),
	Response(ResponseFrame),
	EventSub(Vec<String>),
	Ping,
	Pong,
	/// Valid JSON with an unknown or role-inappropriate `type`; ignored for
	/// forward compatibility
	Unknown(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseFrame {
	pub request_id: u32,
	pub ok: bool,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub value: Option<Value>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub error: Option<OpError>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub meta: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpError {
	pub code: String,
	pub message: String,
}

/// Parses one post-hello client frame. `push` payloads are strictly
/// validated (every ref must be exactly 32 lowercase hex chars) before any
/// decoding, so a frame with one malformed ref is rejected wholesale and
/// never partially applied
pub fn parse_incoming(text: &str) -> Result<Incoming> {
	let mut value: Value = serde_json::from_str(text).map_err(|err| anyhow!("Frame is not valid JSON: {err}"))?;

	let kind = value
		.get("type")
		.and_then(Value::as_str)
		.ok_or_else(|| anyhow!("Frame is missing the type tag"))?
		.to_owned();

	match kind.as_str() {
		"push" => {
			let decoded = || -> Result<Changes> {
				let ops = value
					.get_mut("ops")
					.ok_or_else(|| anyhow!("Push frame is missing the ops field"))?
					.take();

				validate_push_refs(&ops)?;

				serde_json::from_value(ops).map_err(|err| anyhow!("Failed to decode push ops: {err}"))
			}();

			Ok(match decoded {
				Ok(changes) => Incoming::Push(changes),
				Err(err) => Incoming::PushRejected(format!("{err:#}")),
			})
		}
		"response" => {
			let response: ResponseFrame =
				serde_json::from_value(value).map_err(|err| anyhow!("Failed to decode response frame: {err}"))?;

			Ok(Incoming::Response(response))
		}
		"event-sub" => {
			let topics = value
				.get("topics")
				.and_then(Value::as_array)
				.map(|topics| {
					topics
						.iter()
						.filter_map(Value::as_str)
						.map(str::to_owned)
						.collect::<Vec<_>>()
				})
				.ok_or_else(|| anyhow!("event-sub frame is missing a topics array"))?;

			Ok(Incoming::EventSub(topics))
		}
		"ping" => Ok(Incoming::Ping),
		"pong" => Ok(Incoming::Pong),
		_ => Ok(Incoming::Unknown(kind)),
	}
}

/// Validates every ref-position string in a push `ops` object before
/// decoding: additions (id, parent and nested children ids), updates (id)
/// and removals
fn validate_push_refs(ops: &Value) -> Result<()> {
	fn expect_ref(value: Option<&Value>, at: &str) -> Result<()> {
		let hex = value
			.and_then(Value::as_str)
			.ok_or_else(|| anyhow!("Missing or non-string ref at {at}"))?;

		util::parse_ref(hex).map_err(|err| anyhow!("Invalid ref at {at}: {err}"))?;

		Ok(())
	}

	fn walk_children(children: Option<&Value>, at: &str) -> Result<()> {
		let Some(children) = children.and_then(Value::as_array) else {
			return Ok(());
		};

		for (index, child) in children.iter().enumerate() {
			let at = format!("{at}.children[{index}]");

			expect_ref(child.get("id"), &format!("{at}.id"))?;
			walk_children(child.get("children"), &at)?;
		}

		Ok(())
	}

	if !ops.is_object() {
		bail!("Push ops must be an object with additions, updates and removals");
	}

	if let Some(additions) = ops.get("additions").and_then(Value::as_array) {
		for (index, addition) in additions.iter().enumerate() {
			let at = format!("additions[{index}]");

			expect_ref(addition.get("id"), &format!("{at}.id"))?;
			expect_ref(addition.get("parent"), &format!("{at}.parent"))?;
			walk_children(addition.get("children"), &at)?;
		}
	}

	if let Some(updates) = ops.get("updates").and_then(Value::as_array) {
		for (index, update) in updates.iter().enumerate() {
			expect_ref(update.get("id"), &format!("updates[{index}].id"))?;
		}
	}

	if let Some(removals) = ops.get("removals").and_then(Value::as_array) {
		for (index, removal) in removals.iter().enumerate() {
			expect_ref(Some(removal), &format!("removals[{index}]"))?;
		}
	}

	Ok(())
}

#[derive(Serialize)]
struct SyncFrameBorrowed<'a> {
	#[serde(rename = "type")]
	kind: &'static str,
	#[serde(flatten)]
	changes: &'a Changes,
}

fn sync_frame_text(changes: &Changes) -> String {
	serde_json::to_string(&SyncFrameBorrowed { kind: "sync", changes }).expect("Failed to serialize sync frame")
}

/// Serializes a change set into one or more `sync` frames, batching by op
/// count (≤`SYNC_FRAME_MAX_OPS`) and bisecting frames that exceed the soft
/// byte limit while they still contain more than one operation. Relative op
/// order (additions → updates → removals) is preserved across frames
pub fn sync_frames(changes: Changes) -> Vec<String> {
	let mut pending: VecDeque<Changes> = chunk_by_ops(changes, SYNC_FRAME_MAX_OPS).into();
	let mut frames = Vec::new();

	while let Some(chunk) = pending.pop_front() {
		let text = sync_frame_text(&chunk);

		if text.len() <= SYNC_FRAME_MAX_BYTES || chunk.total() <= 1 {
			frames.push(text);
		} else {
			let (first, second) = split_half(chunk);

			pending.push_front(second);
			pending.push_front(first);
		}
	}

	frames
}

fn chunk_by_ops(changes: Changes, max_ops: usize) -> Vec<Changes> {
	if changes.total() <= max_ops {
		return vec![changes];
	}

	let mut chunks = Vec::new();
	let mut current = Changes::new();
	let mut count = 0;

	macro_rules! push_ops {
		($source:expr, $target:ident) => {
			for op in $source {
				if count == max_ops {
					chunks.push(std::mem::replace(&mut current, Changes::new()));
					count = 0;
				}

				current.$target.push(op);
				count += 1;
			}
		};
	}

	push_ops!(changes.additions, additions);
	push_ops!(changes.updates, updates);
	push_ops!(changes.removals, removals);

	if !current.is_empty() {
		chunks.push(current);
	}

	chunks
}

fn split_half(changes: Changes) -> (Changes, Changes) {
	let mid = (changes.total() / 2).max(1);

	let mut first = Changes::new();
	let mut second = Changes::new();
	let mut index = 0;

	macro_rules! split_ops {
		($source:expr, $target:ident) => {
			for op in $source {
				if index < mid {
					first.$target.push(op);
				} else {
					second.$target.push(op);
				}

				index += 1;
			}
		};
	}

	split_ops!(changes.additions, additions);
	split_ops!(changes.updates, updates);
	split_ops!(changes.removals, removals);

	(first, second)
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::core::snapshot::UpdatedSnapshot;
	use rbx_dom_weak::types::Ref;

	fn changes_with_updates(count: usize) -> Changes {
		let mut changes = Changes::new();

		for _ in 0..count {
			changes.update(UpdatedSnapshot::new(Ref::new()));
		}

		changes
	}

	#[test]
	fn frame_envelope_is_flat() {
		let text = sync_frame_text(&changes_with_updates(1));
		let value: Value = serde_json::from_str(&text).unwrap();

		assert_eq!(value["type"], "sync");
		assert!(value["additions"].is_array());
		assert!(value["updates"].is_array());
		assert!(value["removals"].is_array());
	}

	#[test]
	fn sync_frames_split_by_op_count() {
		let frames = sync_frames(changes_with_updates(SYNC_FRAME_MAX_OPS * 2 + 5));

		assert_eq!(frames.len(), 3);

		let last: Value = serde_json::from_str(frames.last().unwrap()).unwrap();
		assert_eq!(last["updates"].as_array().unwrap().len(), 5);
	}

	#[test]
	fn push_refs_are_validated_strictly() {
		let ok = serde_json::json!({
			"type": "push",
			"ops": {
				"additions": [],
				"updates": [{ "id": "0123456789abcdef0123456789abcdef" }],
				"removals": ["00000000000000000000000000000001"],
			},
		});

		assert!(matches!(
			parse_incoming(&ok.to_string()).unwrap(),
			Incoming::Push(changes) if changes.total() == 2
		));

		let bad = serde_json::json!({
			"type": "push",
			"ops": { "additions": [], "updates": [], "removals": ["not-a-ref"] },
		});

		match parse_incoming(&bad.to_string()).unwrap() {
			Incoming::PushRejected(reason) => assert!(reason.contains("removals[0]"), "unexpected reason: {reason}"),
			other => panic!("expected PushRejected, got {other:?}"),
		}

		let uppercase = serde_json::json!({
			"type": "push",
			"ops": { "additions": [], "updates": [{ "id": "0123456789ABCDEF0123456789ABCDEF" }], "removals": [] },
		});

		assert!(matches!(
			parse_incoming(&uppercase.to_string()).unwrap(),
			Incoming::PushRejected(_)
		));
	}

	#[test]
	fn hello_gates_protocol() {
		let missing = parse_hello(r#"{"type":"hello","clientId":"a","role":"watch"}"#).unwrap_err();
		assert_eq!(missing.code, shutdown::PROTOCOL_MISMATCH);

		let wrong = parse_hello(r#"{"type":"hello","clientId":"a","role":"watch","protocol":7}"#).unwrap_err();
		assert_eq!(wrong.code, shutdown::PROTOCOL_MISMATCH);

		let ok = parse_hello(r#"{"type":"hello","clientId":42,"role":"plugin","protocol":1,"name":"n"}"#).unwrap();
		assert_eq!(ok.client_id, "42");
		assert_eq!(ok.role, ClientRole::Plugin);
	}
}
