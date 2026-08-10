use axum::{
	extract::{
		ws::{Message as WsMessage, WebSocket},
		State, WebSocketUpgrade,
	},
	response::Response,
};
use futures_util::{stream::SplitSink, SinkExt, StreamExt};
use log::{debug, trace, warn};
use std::{sync::Arc, thread, time::Instant};
use tokio::sync::{mpsc, oneshot, watch};

use crate::{
	constants::{WS_HELLO_DEADLINE, WS_PING_INTERVAL, WS_PLUGIN_QUEUE, WS_PONG_TIMEOUT, WS_WATCH_QUEUE},
	core::{processor::WriteRequest, queue::Queue, Core},
	server::{self, AppState, Message},
	util,
};

pub mod frames;
pub mod state;

use frames::{shutdown, ClientHello, ClientRole, Event, Incoming, ServerFrame, ServerHello};
use state::{ClaimError, CloseReason, WsState};

/// `GET /ws` — protocol v1's realtime channel (Design §5.3)
pub async fn handler(State(state): State<AppState>, upgrade: WebSocketUpgrade) -> Response {
	trace!("Received request: ws upgrade");

	upgrade.on_upgrade(move |socket| connection(state, socket))
}

async fn connection(state: AppState, mut socket: WebSocket) {
	// Every connect has a deadline: the client must deliver its hello frame
	// before the daemon commits any resources to it
	let hello = match tokio::time::timeout(WS_HELLO_DEADLINE, read_text(&mut socket)).await {
		Err(_) => {
			refuse(
				&mut socket,
				"No hello frame within the deadline",
				shutdown::HELLO_TIMEOUT,
			)
			.await;
			return;
		}
		Ok(None) => return,
		Ok(Some(text)) => text,
	};

	let hello = match frames::parse_hello(&hello) {
		Ok(hello) => hello,
		Err(err) => {
			refuse(&mut socket, &err.reason, err.code).await;
			return;
		}
	};

	debug!(
		"WS client hello: role {}, id {}, name {}",
		hello.role.as_str(),
		hello.client_id,
		hello.name
	);

	match hello.role {
		ClientRole::Plugin => plugin_connection(state, socket, hello).await,
		ClientRole::Agent | ClientRole::Watch | ClientRole::App => watcher_connection(state, socket, hello).await,
	}
}

/// Reads the next text frame, transparently answering protocol pings
async fn read_text(socket: &mut WebSocket) -> Option<String> {
	while let Some(message) = socket.recv().await {
		match message {
			Ok(WsMessage::Text(text)) => return Some(text),
			Ok(WsMessage::Ping(payload)) => {
				socket.send(WsMessage::Pong(payload)).await.ok()?;
			}
			Ok(WsMessage::Close(_)) | Err(_) => return None,
			Ok(_) => continue,
		}
	}

	None
}

/// Sends a typed non-retryable shutdown frame and closes the socket
async fn refuse(socket: &mut WebSocket, reason: &str, code: &str) {
	debug!("Refusing WS client: {reason} ({code})");

	socket
		.send(WsMessage::Text(ServerFrame::shutdown(reason, code, false).to_text()))
		.await
		.ok();
	socket.send(WsMessage::Close(None)).await.ok();
}

/// Builds the daemon's WS hello per the Phase-2 ruling: real version,
/// project identity and hex root refs
fn server_hello(core: &Core) -> ServerFrame {
	let (name, game_id, place_ids, is_place, scope) = {
		let project = core.project();

		(
			project.name.clone(),
			project.game_id,
			project.place_ids.clone(),
			project.is_place(),
			project.scope(),
		)
	};

	let root_refs = {
		let tree = core.tree();

		if is_place {
			tree.place_root_refs().iter().copied().map(util::format_ref).collect()
		} else {
			vec![util::format_ref(tree.root_ref())]
		}
	};

	ServerFrame::Hello(ServerHello {
		name,
		version: env!("CARGO_PKG_VERSION").to_owned(),
		game_id,
		place_ids,
		root_refs,
		scope: scope.as_str().to_owned(),
	})
}

fn journal(state: &AppState, note: &str) {
	if let Some(journal) = state.identity.journal.as_ref() {
		journal.note(note);
	}
}

async fn plugin_connection(state: AppState, mut socket: WebSocket, hello: ClientHello) {
	let core = state.core.clone();
	let queue = core.queue();
	let ws = state.ws.clone();

	// Subscribe on the core bus first (cheap and reversible), then claim the
	// single-plugin slot shared with the long-poll transport
	let queue_id = ws.allocate_queue_id(&queue);

	if queue.subscribe(queue_id, &hello.name).is_err() {
		refuse(
			&mut socket,
			"Failed to subscribe plugin on the sync bus",
			shutdown::BAD_HELLO,
		)
		.await;
		return;
	}

	let (frames_tx, mut frames_rx) = mpsc::channel::<String>(WS_PLUGIN_QUEUE);
	let (close_tx, mut close_rx) = watch::channel::<Option<CloseReason>>(None);

	if let Err(err) = ws.claim_ws_plugin(&queue, queue_id, &hello.name, frames_tx.clone(), close_tx.clone()) {
		queue.unsubscribe(queue_id).ok();

		let existing = match err {
			ClaimError::SlotTaken(name) => name,
			ClaimError::AlreadySubscribed => "unknown".into(),
		};

		refuse(
			&mut socket,
			&format!(
				"Another Studio plugin is already connected ({existing}); one plugin owns the live bridge at a time"
			),
			shutdown::PLUGIN_SLOT_TAKEN,
		)
		.await;
		return;
	}

	if socket
		.send(WsMessage::Text(server_hello(&core).to_text()))
		.await
		.is_err()
	{
		cleanup_plugin(&state, queue_id, &hello);
		return;
	}

	journal(&state, &format!("Plugin connected: {} (ws)", hello.name));
	ws.emit(Event::PluginStatus {
		connected: true,
		name: Some(hello.name.clone()),
		transport: Some("ws".into()),
	});

	// The generated agent docs regenerate on every plugin connect (Design
	// §10.6) — debounced, on the blocking pool, and warn-only on failure
	crate::cli::refresh::auto_refresh_on_connect(core.clone());

	// Push results are reported strictly in application order through a
	// dedicated FIFO pump so overlapping pushes cannot reorder their acks
	let (result_tx, mut result_rx) =
		mpsc::unbounded_channel::<oneshot::Receiver<crate::core::processor::WriteResult>>();

	let pump_frames = frames_tx.clone();
	let pump = tokio::spawn(async move {
		while let Some(receiver) = result_rx.recv().await {
			let Ok(result) = receiver.await else { continue };

			let frame = ServerFrame::PushResult {
				ok: result.ok(),
				applied: result.applied,
				skipped: result.skipped,
				// The real parked count for this batch (Design §6.3)
				conflicts: result.conflicts,
				errors: result.errors,
			};

			if pump_frames.send(frame.to_text()).await.is_err() {
				break;
			}
		}
	});

	// The bridge thread drains the core queue (shared with the long-poll
	// transport) and translates messages into protocol frames
	{
		let queue = queue.clone();
		let ws = ws.clone();
		let frames = frames_tx.clone();
		let close = close_tx.clone();

		thread::Builder::new()
			.name("ws-plugin-bridge".into())
			.spawn(move || bridge(queue, queue_id, frames, close, ws))
			.ok();
	}

	let (mut sink, mut stream) = socket.split();
	let mut shutdown_rx = state.lifecycle.subscribe();
	let mut ping = tokio::time::interval(WS_PING_INTERVAL);
	ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

	let mut last_inbound = Instant::now();
	let mut close_frame = None;

	loop {
		tokio::select! {
			message = stream.next() => {
				match message {
					None | Some(Err(_)) | Some(Ok(WsMessage::Close(_))) => break,
					Some(Ok(WsMessage::Text(text))) => {
						last_inbound = Instant::now();
						handle_plugin_frame(&state, &text, queue_id, &frames_tx, &result_tx);
					}
					Some(Ok(WsMessage::Ping(payload))) => {
						last_inbound = Instant::now();

						if sink.send(WsMessage::Pong(payload)).await.is_err() {
							break;
						}
					}
					Some(Ok(_)) => last_inbound = Instant::now(),
				}
			}
			frame = frames_rx.recv() => {
				let Some(frame) = frame else { break };

				if sink.send(WsMessage::Text(frame)).await.is_err() {
					break;
				}
			}
			_ = ping.tick() => {
				if !heartbeat(&mut sink, &mut last_inbound, &mut close_frame).await {
					break;
				}
			}
			changed = close_rx.changed() => {
				if closed_by_server(changed, &close_rx, &mut close_frame) {
					break;
				}
			}
			changed = shutdown_rx.changed() => {
				if daemon_stopping(changed, &shutdown_rx, &mut close_frame) {
					break;
				}
			}
		}
	}

	if let Some(frame) = close_frame {
		sink.send(WsMessage::Text(frame)).await.ok();
	}

	sink.send(WsMessage::Close(None)).await.ok();

	pump.abort();
	cleanup_plugin(&state, queue_id, &hello);
}

fn cleanup_plugin(state: &AppState, queue_id: u32, hello: &ClientHello) {
	let ws = state.ws.clone();
	let queue = state.core.queue();

	ws.release_plugin(queue_id);

	// Wake the bridge (it exits on the Disconnect message), then drop the
	// queue subscription and fail outstanding request/response correlations
	queue.disconnect("Plugin disconnected", queue_id).ok();
	queue.unsubscribe(queue_id).ok();
	ws.fail_pending_requests();

	// A pending divergence set is only valid against the Studio state that
	// produced it; the plugin going away makes it stale (Design §7.2)
	state.divergence.invalidate("plugin disconnected");

	journal(state, &format!("Plugin disconnected: {}", hello.name));
	ws.emit(Event::PluginStatus {
		connected: false,
		name: Some(hello.name.clone()),
		transport: Some("ws".into()),
	});
}

fn handle_plugin_frame(
	state: &AppState,
	text: &str,
	queue_id: u32,
	frames_tx: &mpsc::Sender<String>,
	result_tx: &mpsc::UnboundedSender<oneshot::Receiver<crate::core::processor::WriteResult>>,
) {
	let incoming = match frames::parse_incoming(text) {
		Ok(incoming) => incoming,
		Err(err) => {
			warn!("Ignoring malformed plugin frame: {err:#}");
			return;
		}
	};

	match incoming {
		Incoming::Push(changes) => {
			state
				.ws
				.emit(Event::sync_activity(server::DIRECTION_STUDIO_TO_DISK, &changes));

			let (sender, receiver) = oneshot::channel();

			state.core.processor().write_with_result(
				WriteRequest {
					changes,
					client_id: queue_id,
				},
				sender,
			);

			result_tx.send(receiver).ok();
		}
		Incoming::PushRejected(reason) => {
			warn!("Rejected plugin push: {reason}");

			let frame = ServerFrame::PushResult {
				ok: false,
				applied: 0,
				skipped: 0,
				conflicts: 0,
				errors: vec![reason],
			};

			frames_tx.try_send(frame.to_text()).ok();
		}
		Incoming::Response(response) => state.ws.resolve_request(response),
		Incoming::Ping => {
			frames_tx.try_send(ServerFrame::Pong.to_text()).ok();
		}
		Incoming::Pong => {}
		Incoming::EventSub(_) => debug!("Ignoring event-sub frame from the plugin"),
		Incoming::Unknown(kind) => debug!("Ignoring unknown plugin frame type {kind:?}"),
	}
}

async fn watcher_connection(state: AppState, socket: WebSocket, hello: ClientHello) {
	let ws = state.ws.clone();
	let role = hello.role.as_str();

	let (frames_tx, mut frames_rx) = mpsc::channel::<String>(WS_WATCH_QUEUE);
	let (close_tx, mut close_rx) = watch::channel::<Option<CloseReason>>(None);

	let watcher_id = ws.register_watcher(role, frames_tx.clone(), close_tx.clone());

	let (mut sink, mut stream) = socket.split();

	if sink
		.send(WsMessage::Text(server_hello(&state.core).to_text()))
		.await
		.is_err()
	{
		ws.remove_watcher(watcher_id);
		return;
	}

	let mut shutdown_rx = state.lifecycle.subscribe();
	let mut ping = tokio::time::interval(WS_PING_INTERVAL);
	ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

	let mut last_inbound = Instant::now();
	let mut close_frame = None;

	loop {
		tokio::select! {
			message = stream.next() => {
				match message {
					None | Some(Err(_)) | Some(Ok(WsMessage::Close(_))) => break,
					Some(Ok(WsMessage::Text(text))) => {
						last_inbound = Instant::now();
						handle_watcher_frame(&state, watcher_id, &text, &frames_tx);
					}
					Some(Ok(WsMessage::Ping(payload))) => {
						last_inbound = Instant::now();

						if sink.send(WsMessage::Pong(payload)).await.is_err() {
							break;
						}
					}
					Some(Ok(_)) => last_inbound = Instant::now(),
				}
			}
			frame = frames_rx.recv() => {
				let Some(frame) = frame else { break };

				if sink.send(WsMessage::Text(frame)).await.is_err() {
					break;
				}
			}
			_ = ping.tick() => {
				if !heartbeat(&mut sink, &mut last_inbound, &mut close_frame).await {
					break;
				}
			}
			changed = close_rx.changed() => {
				if closed_by_server(changed, &close_rx, &mut close_frame) {
					break;
				}
			}
			changed = shutdown_rx.changed() => {
				if daemon_stopping(changed, &shutdown_rx, &mut close_frame) {
					break;
				}
			}
		}
	}

	if let Some(frame) = close_frame {
		sink.send(WsMessage::Text(frame)).await.ok();
	}

	sink.send(WsMessage::Close(None)).await.ok();

	ws.remove_watcher(watcher_id);
}

fn handle_watcher_frame(state: &AppState, watcher_id: u64, text: &str, frames_tx: &mpsc::Sender<String>) {
	match frames::parse_incoming(text) {
		Ok(Incoming::EventSub(topics)) => state.ws.set_watcher_topics(watcher_id, topics),
		Ok(Incoming::Ping) => {
			frames_tx.try_send(ServerFrame::Pong.to_text()).ok();
		}
		Ok(Incoming::Pong) => {}
		Ok(other) => debug!("Ignoring watcher frame: {other:?}"),
		Err(err) => warn!("Ignoring malformed watcher frame: {err:#}"),
	}
}

/// Shared heartbeat arm: closes the connection with a typed shutdown when the
/// client has been silent past the pong timeout, otherwise sends a ping.
/// Returns `false` when the connection should end
async fn heartbeat(
	sink: &mut SplitSink<WebSocket, WsMessage>,
	last_inbound: &mut Instant,
	close_frame: &mut Option<String>,
) -> bool {
	if last_inbound.elapsed() > WS_PONG_TIMEOUT {
		*close_frame = Some(
			ServerFrame::shutdown(
				"No heartbeat pong within the timeout",
				shutdown::HEARTBEAT_TIMEOUT,
				true,
			)
			.to_text(),
		);

		return false;
	}

	sink.send(WsMessage::Text(ServerFrame::Ping.to_text())).await.is_ok()
}

/// Shared close-signal arm (slot eviction, overload, targeted close)
fn closed_by_server(
	changed: Result<(), watch::error::RecvError>,
	close_rx: &watch::Receiver<Option<CloseReason>>,
	close_frame: &mut Option<String>,
) -> bool {
	if changed.is_err() {
		return true;
	}

	let reason = close_rx.borrow().clone();

	if let Some(reason) = reason {
		*close_frame = Some(ServerFrame::shutdown(&reason.reason, &reason.code, reason.retryable).to_text());
		true
	} else {
		false
	}
}

/// Shared daemon-shutdown arm
fn daemon_stopping(
	changed: Result<(), watch::error::RecvError>,
	shutdown_rx: &watch::Receiver<Option<String>>,
	close_frame: &mut Option<String>,
) -> bool {
	if changed.is_err() {
		return true;
	}

	let reason = shutdown_rx.borrow().clone();

	if let Some(reason) = reason {
		*close_frame = Some(ServerFrame::shutdown(&reason, shutdown::DAEMON_STOPPING, false).to_text());
		true
	} else {
		false
	}
}

/// Drains the core queue for the WS plugin, translating bus messages into
/// protocol frames. Runs on a dedicated thread because the queue is a
/// blocking crossbeam channel shared with the long-poll transport
fn bridge(queue: Arc<Queue>, queue_id: u32, frames: mpsc::Sender<String>, close: state::CloseSignal, ws: Arc<WsState>) {
	let send = |text: String| -> bool {
		match frames.try_send(text) {
			Ok(()) => true,
			Err(mpsc::error::TrySendError::Full(_)) => {
				debug!("Plugin outbound queue overloaded, disconnecting");

				close
					.send(Some(CloseReason {
						reason: "Plugin is not reading sync frames fast enough".into(),
						code: shutdown::SLOW_CONSUMER.into(),
						retryable: true,
					}))
					.ok();

				false
			}
			Err(mpsc::error::TrySendError::Closed(_)) => false,
		}
	};

	loop {
		let message = match queue.get_timeout(queue_id) {
			// Unsubscribed — the connection is gone
			Err(_) => break,
			// Idle poll window elapsed
			Ok(None) => continue,
			Ok(Some(message)) => message,
		};

		match message {
			Message::SyncChanges(server::SyncChanges(changes)) => {
				ws.emit(Event::sync_activity(server::DIRECTION_DISK_TO_STUDIO, &changes));

				let mut alive = true;

				for frame in frames::sync_frames(changes) {
					if !send(frame) {
						alive = false;
						break;
					}
				}

				if !alive {
					break;
				}
			}
			Message::SyncDetails(server::SyncDetails(details)) => {
				if !send(ServerFrame::Details { details }.to_text()) {
					break;
				}
			}
			Message::ExecuteCode(execute) => {
				if !send(ServerFrame::Execute { code: execute.code }.to_text()) {
					break;
				}
			}
			Message::Disconnect(disconnect) => {
				// The processor parked this client (e.g. declined changes
				// threshold); forward as a typed shutdown and end the bridge
				close
					.send(Some(CloseReason {
						reason: disconnect.message,
						code: shutdown::OUT_OF_SYNC.into(),
						retryable: false,
					}))
					.ok();

				break;
			}
			// Internal marker used by the sourcemap regenerator
			Message::SyncbackChanges(_) => continue,
		}
	}

	trace!("Plugin bridge for client {queue_id} ended");
}
