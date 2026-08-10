use log::{debug, warn};
use std::{
	collections::HashMap,
	sync::{
		atomic::{AtomicU32, AtomicU64, Ordering},
		Arc, Mutex,
	},
	time::Instant,
};
use tokio::sync::{mpsc, oneshot, watch};
use uuid::Uuid;

use super::frames::{shutdown, Event, ResponseFrame, ServerFrame};
use crate::{constants::LONGPOLL_EVICT_AFTER, core::queue::Queue, lock};

/// Reason attached to a server-initiated close of one WS connection; the
/// connection task turns it into a typed `shutdown` frame before closing
#[derive(Debug, Clone)]
pub struct CloseReason {
	pub reason: String,
	pub code: String,
	pub retryable: bool,
}

pub type CloseSignal = watch::Sender<Option<CloseReason>>;

/// The transport currently occupying the single-plugin slot. A WS plugin and
/// a msgpack long-poll plugin are the same slot: one Studio connection owns
/// the live bridge at a time regardless of transport
pub enum PluginTransport {
	Ws {
		frames: mpsc::Sender<String>,
		close: CloseSignal,
	},
	LongPoll {
		last_seen: Instant,
	},
}

pub struct PluginConn {
	pub queue_id: u32,
	pub name: String,
	pub transport: PluginTransport,
}

impl PluginConn {
	pub fn transport_name(&self) -> &'static str {
		match self.transport {
			PluginTransport::Ws { .. } => "ws",
			PluginTransport::LongPoll { .. } => "long-poll",
		}
	}
}

pub struct Watcher {
	frames: mpsc::Sender<String>,
	close: CloseSignal,
	role: &'static str,
	/// `None` = all topics (the default until an `event-sub` frame arrives)
	topics: Arc<Mutex<Option<Vec<String>>>>,
}

/// Outcome of a plugin-slot claim attempt
pub enum ClaimError {
	/// The queue already has a listener with this client id
	AlreadySubscribed,
	/// A live plugin owns the slot; carries its name for the typed refusal
	SlotTaken(String),
}

/// Shared WebSocket-layer state: the single-plugin slot (bridging both
/// transports), the watch/app event fan-out, and the request/response router
pub struct WsState {
	plugin: Mutex<Option<PluginConn>>,
	watchers: Mutex<HashMap<u64, Watcher>>,
	next_watcher_id: AtomicU64,
	next_request_id: AtomicU32,
	pending: Mutex<HashMap<u32, oneshot::Sender<ResponseFrame>>>,
}

impl WsState {
	pub fn new() -> Self {
		Self {
			plugin: Mutex::new(None),
			watchers: Mutex::new(HashMap::new()),
			next_watcher_id: AtomicU64::new(1),
			next_request_id: AtomicU32::new(1),
			pending: Mutex::new(HashMap::new()),
		}
	}

	/// Allocates a queue listener id that is not in use and never collides
	/// with the internal listener range (which counts up from 0)
	pub fn allocate_queue_id(&self, queue: &Queue) -> u32 {
		loop {
			let id = (Uuid::new_v4().as_u128() >> 96) as u32;

			if id > 0xFFFF && !queue.is_subscribed(id) {
				return id;
			}
		}
	}

	/// Evicts a long-poll plugin whose last `/read` poll is older than the
	/// liveness window. Must be called with the slot lock held
	fn evict_stale_longpoll(slot: &mut Option<PluginConn>, queue: &Queue) {
		if let Some(conn) = slot {
			if let PluginTransport::LongPoll { last_seen } = &conn.transport {
				if last_seen.elapsed() > LONGPOLL_EVICT_AFTER {
					debug!(
						"Evicting stale long-poll plugin {} (client {})",
						conn.name, conn.queue_id
					);

					queue.unsubscribe(conn.queue_id).ok();
					*slot = None;
				}
			}
		}
	}

	/// Claims the plugin slot for a WS plugin. The caller must have
	/// subscribed `queue_id` on the queue already; on refusal the caller
	/// unsubscribes it again
	pub fn claim_ws_plugin(
		&self,
		queue: &Queue,
		queue_id: u32,
		name: &str,
		frames: mpsc::Sender<String>,
		close: CloseSignal,
	) -> Result<(), ClaimError> {
		let mut slot = lock!(self.plugin);

		Self::evict_stale_longpoll(&mut slot, queue);

		if let Some(existing) = &*slot {
			return Err(ClaimError::SlotTaken(existing.name.clone()));
		}

		*slot = Some(PluginConn {
			queue_id,
			name: name.to_owned(),
			transport: PluginTransport::Ws { frames, close },
		});

		Ok(())
	}

	/// Claims the plugin slot for a msgpack long-poll plugin (the compat
	/// transport), subscribing it on the queue on success
	pub fn claim_longpoll_plugin(&self, queue: &Queue, client_id: u32, name: &str) -> Result<(), ClaimError> {
		let mut slot = lock!(self.plugin);

		Self::evict_stale_longpoll(&mut slot, queue);

		if let Some(existing) = &*slot {
			return Err(ClaimError::SlotTaken(existing.name.clone()));
		}

		if queue.subscribe(client_id, name).is_err() {
			return Err(ClaimError::AlreadySubscribed);
		}

		*slot = Some(PluginConn {
			queue_id: client_id,
			name: name.to_owned(),
			transport: PluginTransport::LongPoll {
				last_seen: Instant::now(),
			},
		});

		Ok(())
	}

	/// Releases the plugin slot if it is still owned by `queue_id`, returning
	/// the released connection
	pub fn release_plugin(&self, queue_id: u32) -> Option<PluginConn> {
		let mut slot = lock!(self.plugin);

		if slot.as_ref().is_some_and(|conn| conn.queue_id == queue_id) {
			slot.take()
		} else {
			None
		}
	}

	/// Marks the long-poll plugin as alive (called from `/read` and `/write`)
	pub fn touch_longpoll(&self, client_id: u32) {
		let mut slot = lock!(self.plugin);

		if let Some(conn) = &mut *slot {
			if conn.queue_id == client_id {
				if let PluginTransport::LongPoll { last_seen } = &mut conn.transport {
					*last_seen = Instant::now();
				}
			}
		}
	}

	/// Describes the current plugin connection as `(name, transport)`
	pub fn plugin_status(&self) -> Option<(String, &'static str)> {
		let slot = lock!(self.plugin);

		slot.as_ref().map(|conn| (conn.name.clone(), conn.transport_name()))
	}

	/// Returns the WS plugin's outbound frame queue, or a typed reason why
	/// requests cannot be routed right now
	pub fn plugin_frames(&self) -> Result<mpsc::Sender<String>, &'static str> {
		let slot = lock!(self.plugin);

		match &*slot {
			None => Err("no Studio plugin connected"),
			Some(PluginConn {
				transport: PluginTransport::LongPoll { .. },
				..
			}) => Err("the connected Studio plugin uses the long-poll compat transport, which does not support requests"),
			Some(PluginConn {
				transport: PluginTransport::Ws { frames, .. },
				..
			}) => Ok(frames.clone()),
		}
	}

	// Request/response router

	pub fn next_request_id(&self) -> u32 {
		loop {
			let id = self.next_request_id.fetch_add(1, Ordering::Relaxed);

			if id != 0 {
				return id;
			}
		}
	}

	pub fn register_request(&self, request_id: u32) -> oneshot::Receiver<ResponseFrame> {
		let (sender, receiver) = oneshot::channel();

		lock!(self.pending).insert(request_id, sender);

		receiver
	}

	pub fn abort_request(&self, request_id: u32) {
		lock!(self.pending).remove(&request_id);
	}

	/// Routes a plugin `response` frame to the pending `POST /request` caller
	pub fn resolve_request(&self, response: ResponseFrame) {
		let sender = lock!(self.pending).remove(&response.request_id);

		match sender {
			Some(sender) => {
				sender.send(response).ok();
			}
			None => warn!(
				"Received response for unknown or timed out request {}",
				response.request_id
			),
		}
	}

	/// Fails every pending request (called when the plugin disconnects); the
	/// dropped senders surface as `PLUGIN_ERROR` replies
	pub fn fail_pending_requests(&self) {
		lock!(self.pending).clear();
	}

	// Watch/app event fan-out

	pub fn register_watcher(&self, role: &'static str, frames: mpsc::Sender<String>, close: CloseSignal) -> u64 {
		let id = self.next_watcher_id.fetch_add(1, Ordering::Relaxed);

		lock!(self.watchers).insert(
			id,
			Watcher {
				frames,
				close,
				role,
				topics: Arc::new(Mutex::new(None)),
			},
		);

		id
	}

	pub fn remove_watcher(&self, id: u64) {
		lock!(self.watchers).remove(&id);
	}

	/// Replaces the topic filter of one watcher (`event-sub` frame)
	pub fn set_watcher_topics(&self, id: u64, topics: Vec<String>) {
		let watchers = lock!(self.watchers);

		if let Some(watcher) = watchers.get(&id) {
			*lock!(watcher.topics) = Some(topics);
		}
	}

	/// Broadcasts a sanitized event to every subscribed watch/app client. A
	/// client whose bounded queue is full is closed with a typed overload
	/// shutdown instead of ever blocking the caller
	pub fn emit(&self, event: Event) {
		let topic = event.topic();
		let text = ServerFrame::Event { event: event.clone() }.to_text();

		let watchers = lock!(self.watchers);

		for (id, watcher) in watchers.iter() {
			let subscribed = {
				let topics = lock!(watcher.topics);

				match &*topics {
					None => true,
					Some(topics) => topics.iter().any(|t| t == topic),
				}
			};

			if !subscribed {
				continue;
			}

			if let Err(mpsc::error::TrySendError::Full(_)) = watcher.frames.try_send(text.clone()) {
				debug!("Disconnecting overloaded {} client #{id}", watcher.role);

				watcher
					.close
					.send(Some(CloseReason {
						reason: "Client is not reading events fast enough".into(),
						code: shutdown::SLOW_CONSUMER.into(),
						retryable: true,
					}))
					.ok();
			}
		}
	}

	/// Asks every connected WS client to close with a typed shutdown frame
	/// (daemon stop path). Long-poll clients are notified through the queue's
	/// `Disconnect` message instead
	pub fn broadcast_close(&self, reason: &str, code: &str) {
		let close = CloseReason {
			reason: reason.to_owned(),
			code: code.to_owned(),
			retryable: false,
		};

		{
			let slot = lock!(self.plugin);

			if let Some(PluginConn {
				transport: PluginTransport::Ws { close: signal, .. },
				..
			}) = &*slot
			{
				signal.send(Some(close.clone())).ok();
			}
		}

		let watchers = lock!(self.watchers);

		for watcher in watchers.values() {
			watcher.close.send(Some(close.clone())).ok();
		}
	}
}
