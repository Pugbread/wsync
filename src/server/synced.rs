//! `wait-synced` — a disk → Studio barrier — and the pending-outbound counts
//! `status` reports.
//!
//! A disk change travels watcher → debouncer → processor → core queue → WS
//! bridge → the plugin's socket queue → Studio. The plugin never acknowledges
//! `sync` frames, so "has Studio applied everything?" is answered by draining
//! each stage in order and then asking the plugin a question whose answer can
//! only come back after everything before it was applied:
//!
//! 1. **Watcher.** The debouncer holds a raw event for its debounce window
//!    and emits it in a flush pass, forwarding straight into the processor
//!    channel. The barrier waits for a *completed* pass that started at least
//!    the debounce window (plus a small OS-delivery grace) after it began, so
//!    every write finished before the barrier is past the debouncer.
//! 2. **Processor.** A fence drains every event already handed over and
//!    answers once each resulting change set is on the core queue.
//! 3. **Queue.** The plugin listener's delivery accounting (see
//!    `core::queue::Flow`) proves the bridge translated every one of those
//!    messages into frames on the socket queue.
//! 4. **Studio.** A `sync_barrier` request goes onto the same FIFO socket
//!    queue. The plugin queues it behind the sync frames it already holds,
//!    its sync loop applies those first, and the handler additionally waits
//!    out the asynchronous script-source writes they started — so the answer
//!    means applied, not merely received.
//!
//! A disk edit that clashed with Studio is not pending: Studio wins at once
//! and the disk bytes move to the backlog. The barrier reports parked
//! conflicts and backlog entries captured while it ran, because those are the
//! changes it will never deliver.

use axum::{body::Bytes, extract::State, response::Json};
use log::trace;
use serde::Deserialize;
use serde_json::{json, Value};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::{
	constants::{WAIT_SYNCED_DEFAULT_TIMEOUT_MS, WAIT_SYNCED_MAX_TIMEOUT_MS, WATCHER_DELIVERY_SLACK},
	core::queue::FlowMark,
	server::{bulk, ws::frames::codes, AppState},
	vfs::debouncer::DEBOUNCE_TIMEOUT,
};

/// How often a waiting stage re-checks its condition
const POLL: Duration = Duration::from_millis(5);

/// Head-room kept between the plugin's own source-write wait and the op
/// deadline, so the plugin's typed answer wins the race against the daemon's
/// timeout
const STUDIO_SLACK_MS: u64 = 500;

/// Which leg of the path a barrier failed in
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
	Plugin,
	Watcher,
	Processor,
	Queue,
	Studio,
}

impl Stage {
	pub fn as_str(self) -> &'static str {
		match self {
			Self::Plugin => "plugin",
			Self::Watcher => "watcher",
			Self::Processor => "processor",
			Self::Queue => "queue",
			Self::Studio => "studio",
		}
	}
}

#[derive(Debug)]
pub struct BarrierFailure {
	pub stage: Stage,
	pub code: &'static str,
	pub message: String,
}

impl BarrierFailure {
	fn new(stage: Stage, code: &'static str, message: impl Into<String>) -> Self {
		Self {
			stage,
			code,
			message: message.into(),
		}
	}

	fn timeout(stage: Stage, what: &str) -> Self {
		Self::new(stage, codes::TIMEOUT, format!("Timed out {what}"))
	}

	fn disconnected() -> Self {
		Self::new(
			Stage::Plugin,
			codes::PLUGIN_ERROR,
			"The Studio plugin disconnected or reconnected during the barrier; run it again once it is connected",
		)
	}
}

#[derive(Debug)]
pub struct BarrierOutcome {
	/// Changes sent since the previous confirmation that this barrier proved
	/// applied in Studio
	pub flushed_changes: u64,
	/// Watcher events the processor fence drained
	pub drained_events: usize,
	/// Script-source writes the plugin waited out before answering
	pub source_writes_awaited: u64,
	/// Watcher events dropped as echoes of a Studio -> disk write while the
	/// barrier ran — normally exactly that, but a real edit that landed in
	/// the same window was dropped with them and will never be delivered
	pub suppressed_events: u64,
	pub stage_ms: [u64; 4],
}

fn remaining(deadline: Instant) -> Duration {
	deadline.saturating_duration_since(Instant::now())
}

fn elapsed_ms(since: Instant) -> u64 {
	since.elapsed().as_millis() as u64
}

/// Maps a failed `sync_barrier` call onto the barrier's own vocabulary
fn studio_failure(err: bulk::PluginCallError) -> BarrierFailure {
	match err.code.as_str() {
		codes::TIMEOUT => BarrierFailure::timeout(
			Stage::Studio,
			"waiting for Studio to apply the delivered changes \
			 (the plugin may be busy or waiting on a connect decision)",
		),
		codes::UNKNOWN_OP => BarrierFailure::new(
			Stage::Studio,
			codes::UNKNOWN_OP,
			"The connected Studio plugin predates `sync_barrier`; \
			 update the plugin (`wsync plugin install`) and reconnect",
		),
		_ => BarrierFailure::new(Stage::Studio, codes::PLUGIN_ERROR, err.message),
	}
}

/// Runs the four-stage barrier described in the module docs against the
/// connected WS plugin, bounded by `deadline`
pub async fn run_barrier(state: &AppState, deadline: Instant) -> Result<BarrierOutcome, BarrierFailure> {
	let started = Instant::now();

	if let Err(reason) = state.ws.plugin_frames() {
		return Err(BarrierFailure::new(Stage::Plugin, codes::PLUGIN_ERROR, reason));
	}

	let Some(queue_id) = state.ws.plugin_queue_id() else {
		return Err(BarrierFailure::disconnected());
	};

	let core = state.core.clone();
	let queue = core.queue();
	let suppressed_before = core.vfs().suppressed_events();

	// 1. Watcher: a completed flush pass that began after every raw event
	// from before `started` had aged past the debounce window
	let flushed_through = started + WATCHER_DELIVERY_SLACK + DEBOUNCE_TIMEOUT;

	loop {
		// Events held while a sync write was in progress count as not yet
		// delivered: they reach the processor once that write window closes
		let held = core.vfs().held_events();

		match core.vfs().last_flush() {
			Some(pass) if pass >= flushed_through && held == 0 => break,
			// No debouncing watcher (virtual VFS): the window alone bounds it
			None if Instant::now() >= flushed_through && held == 0 => break,
			_ => {}
		}

		if Instant::now() >= deadline {
			return Err(BarrierFailure::timeout(
				Stage::Watcher,
				"waiting for the file watcher to flush (is the machine under heavy load?)",
			));
		}

		tokio::time::sleep(POLL).await;
	}

	let watcher_ms = elapsed_ms(started);

	// 2. Processor: everything the watcher handed over becomes queue messages
	let processor_started = Instant::now();
	let (fence_tx, fence_rx) = tokio::sync::oneshot::channel();

	core.processor().fence(fence_tx);

	let drained_events = match tokio::time::timeout(remaining(deadline), fence_rx).await {
		Ok(Ok(drained)) => drained,
		Ok(Err(_)) => {
			return Err(BarrierFailure::new(
				Stage::Processor,
				codes::PLUGIN_ERROR,
				"The sync processor is not running",
			))
		}
		Err(_) => {
			return Err(BarrierFailure::timeout(
				Stage::Processor,
				"waiting for the sync processor to drain (a large change set may still be processing)",
			))
		}
	};

	let processor_ms = elapsed_ms(processor_started);

	// 3. Queue: the bridge has handed every one of those messages to the
	// socket queue
	let queue_started = Instant::now();
	let target = queue.flow(queue_id).ok_or_else(BarrierFailure::disconnected)?;

	loop {
		let mark = queue.flow(queue_id).ok_or_else(BarrierFailure::disconnected)?;

		if mark.settled >= target.pushed {
			break;
		}

		if Instant::now() >= deadline {
			return Err(BarrierFailure::timeout(
				Stage::Queue,
				&format!(
					"handing {} queued change(s) to the plugin connection",
					target.pushed_changes.saturating_sub(mark.settled_changes)
				),
			));
		}

		tokio::time::sleep(POLL).await;
	}

	let queue_ms = elapsed_ms(queue_started);

	// 4. Studio: the request rides the same FIFO socket queue behind those
	// frames, and the plugin answers only after applying them
	let studio_started = Instant::now();
	let budget_ms = remaining(deadline).as_millis() as u64;

	if budget_ms == 0 {
		return Err(BarrierFailure::timeout(Stage::Studio, "before Studio could be asked"));
	}

	let answer = bulk::plugin_request(
		state,
		"sync_barrier",
		json!({ "waitMs": budget_ms.saturating_sub(STUDIO_SLACK_MS) }),
		budget_ms,
	)
	.await
	.map_err(studio_failure)?;

	// A plugin that reconnected meanwhile answered for a different stream
	if state.ws.plugin_queue_id() != Some(queue_id) {
		return Err(BarrierFailure::disconnected());
	}

	let pending_writes = answer.get("pendingSourceWrites").and_then(Value::as_u64).unwrap_or(0);

	if answer.get("settled").and_then(Value::as_bool) == Some(false) {
		return Err(BarrierFailure::new(
			Stage::Studio,
			codes::TIMEOUT,
			format!("Studio applied the changes but {pending_writes} script source write(s) are still in flight"),
		));
	}

	// Everything up to the target is proven applied
	queue.confirm(
		queue_id,
		&FlowMark {
			settled_changes: target.pushed_changes,
			..target
		},
	);

	Ok(BarrierOutcome {
		flushed_changes: target.pushed_changes.saturating_sub(target.confirmed_changes),
		drained_events,
		source_writes_awaited: answer.get("sourceWritesAwaited").and_then(Value::as_u64).unwrap_or(0),
		suppressed_events: core.vfs().suppressed_events().saturating_sub(suppressed_before),
		stage_ms: [watcher_ms, processor_ms, queue_ms, elapsed_ms(studio_started)],
	})
}

/// The pending-outbound picture shared by `GET /sync/pending` and the
/// `wait-synced` reply
pub fn pending_json(state: &AppState) -> Value {
	let queue = state.core.queue();
	let plugin = state.ws.plugin_status();
	let mark = state.ws.plugin_queue_id().and_then(|id| queue.flow(id));

	let (queued, unconfirmed, queued_messages) = match &mark {
		Some(mark) => (
			mark.queued_changes(),
			mark.unconfirmed_changes(),
			mark.pushed.saturating_sub(mark.settled),
		),
		None => (0, 0, 0),
	};

	json!({
		"plugin": {
			"connected": plugin.is_some(),
			"name": plugin.as_ref().map(|(name, _)| name.clone()),
			"transport": plugin.as_ref().map(|(_, transport)| *transport),
			// Only the WS transport carries the accounting below
			"tracked": mark.is_some(),
		},
		// Changes for Studio not yet on the plugin connection, plus changes
		// sent since the last `wait-synced` confirmed Studio applied them
		"total": queued + unconfirmed,
		"queuedChanges": queued,
		"queuedMessages": queued_messages,
		"unconfirmedChanges": unconfirmed,
		// Watcher events the processor has not turned into changes yet
		"unprocessedEvents": state.core.vfs().pending_events(),
		"parkedConflicts": state.core.conflicts().parked_count(),
	})
}

/// `GET /sync/pending` — cheap counts, no waiting
pub async fn pending(State(state): State<AppState>) -> Json<Value> {
	trace!("Received request: sync/pending");

	Json(json!({ "ok": true, "pending": pending_json(&state) }))
}

#[derive(Deserialize, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct WaitRequest {
	timeout_ms: Option<u64>,
}

fn unix_now() -> u64 {
	SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.map(|elapsed| elapsed.as_secs())
		.unwrap_or(0)
}

/// Backlog entries captured at or after `since` (unix seconds): disk edits
/// that lost to Studio while the barrier ran and will never be delivered
fn backlog_since(state: &AppState, since: u64) -> Value {
	let backlog = state.backlog.to_json();
	let empty = Vec::new();
	let entries = backlog.get("entries").and_then(Value::as_array).unwrap_or(&empty);

	let recent: Vec<Value> = entries
		.iter()
		.filter(|entry| entry.get("capturedAt").and_then(Value::as_u64).unwrap_or(0) >= since)
		.map(|entry| {
			json!({
				"id": entry.get("id"),
				"path": entry.get("path"),
				"reason": entry.get("reason"),
				"capturedAt": entry.get("capturedAt"),
			})
		})
		.collect();

	json!({
		"total": entries.len(),
		"capturedDuringWait": recent,
	})
}

/// `POST /sync/wait {timeoutMs?}` — blocks until Studio has applied every disk
/// change the daemon had seen when the request arrived. Always HTTP 200;
/// `ok` carries the verdict
pub async fn wait(State(state): State<AppState>, body: Bytes) -> Json<Value> {
	trace!("Received request: sync/wait");

	// An empty or unparseable body means the defaults; the deadline is the
	// only knob and a bad one must not turn into an unbounded wait
	let request: WaitRequest = serde_json::from_slice(&body).unwrap_or_default();
	let timeout_ms = request
		.timeout_ms
		.unwrap_or(WAIT_SYNCED_DEFAULT_TIMEOUT_MS)
		.clamp(1, WAIT_SYNCED_MAX_TIMEOUT_MS);

	let started = Instant::now();
	let started_unix = unix_now();
	let outcome = run_barrier(&state, started + Duration::from_millis(timeout_ms)).await;
	let duration_ms = elapsed_ms(started);

	match outcome {
		Ok(outcome) => {
			let [watcher, processor, queue, studio] = outcome.stage_ms;

			Json(json!({
				"ok": true,
				"synced": true,
				"flushedChanges": outcome.flushed_changes,
				"drainedEvents": outcome.drained_events,
				"sourceWritesAwaited": outcome.source_writes_awaited,
				"suppressedEvents": outcome.suppressed_events,
				"durationMs": duration_ms,
				"timeoutMs": timeout_ms,
				"stages": {
					"watcherMs": watcher,
					"processorMs": processor,
					"queueMs": queue,
					"studioMs": studio,
				},
				"backlog": backlog_since(&state, started_unix),
				"pending": pending_json(&state),
			}))
		}
		Err(failure) => Json(json!({
			"ok": false,
			"synced": false,
			"stage": failure.stage.as_str(),
			"error": { "code": failure.code, "message": failure.message },
			"durationMs": duration_ms,
			"timeoutMs": timeout_ms,
			"backlog": backlog_since(&state, started_unix),
			"pending": pending_json(&state),
		})),
	}
}
