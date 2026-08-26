use anyhow::{bail, Result};
use axum::{
	extract::DefaultBodyLimit,
	response::Redirect,
	routing::{get, post},
	Router,
};
use derive_from_one::FromOne;
use log::{debug, warn};
use serde::{Deserialize, Serialize};
use std::{
	net::{TcpListener, ToSocketAddrs},
	path::PathBuf,
	sync::Arc,
	time::Duration,
};
use uuid::Uuid;

use crate::{
	constants::MAX_PAYLOAD_SIZE,
	core::{changes::Changes, Core},
	daemon::{self, DaemonLog},
	project::ProjectDetails,
};

pub mod audit;
pub mod backlog;
pub mod bulk;
mod details;
pub mod divergence;
mod exec;
mod hello;
mod home;
pub mod lifecycle;
mod msgpack;
mod open;
pub mod ops;
mod read;
pub mod request;
pub mod resolve;
mod snapshot;
pub mod snapshot_json;
mod stop;
mod subscribe;
mod unsubscribe;
mod write;
pub mod ws;

pub use msgpack::MsgPack;

/// Sanitized activity-event directions (Design §5.3 `sync-activity`)
pub const DIRECTION_DISK_TO_STUDIO: &str = "disk-to-studio";
pub const DIRECTION_STUDIO_TO_DISK: &str = "studio-to-disk";

/// How long the daemon waits for graceful connection drain after a shutdown
/// trigger before forcing the server loop to end anyway
const SHUTDOWN_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Serialize, FromOne)]
pub enum Message {
	SyncChanges(SyncChanges),
	SyncbackChanges(SyncbackChanges),
	SyncDetails(SyncDetails),
	ExecuteCode(ExecuteCode),
	Disconnect(Disconnect),
}

impl Message {
	pub fn is_change(&self) -> bool {
		matches!(self, Message::SyncChanges(_) | Message::SyncbackChanges(_))
	}
}

#[derive(Debug, Clone, Serialize)]
pub struct SyncChanges(pub Changes);

#[derive(Debug, Clone, Serialize)]
pub struct SyncbackChanges();

#[derive(Debug, Clone, Serialize)]
pub struct SyncDetails(pub ProjectDetails);

#[derive(Debug, Clone, Serialize)]
pub struct ExecuteCode {
	pub code: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Disconnect {
	pub message: String,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct AuthRequest {
	client_id: u32,
}

/// Per-boot server identity exposed by `/hello` and enforced by the
/// authenticated lifecycle routes
#[derive(Debug)]
pub struct Identity {
	pub boot_id: String,
	pub port: u16,
	pub managed_by: String,
	/// Present when the daemon was started managed (with an owner token);
	/// gates `/stop`, `/manager-heartbeat` and `/manager-close`
	pub control_token: Option<String>,
	/// Per-daemon lifecycle journal backing `wsync daemon logs`
	pub journal: Option<Arc<DaemonLog>>,
}

/// Boot parameters supplied by the CLI entrypoint (`wsync serve` or
/// `wsync daemon start`)
#[derive(Clone)]
pub struct ServerIdentity {
	pub boot_id: String,
	pub managed_by: String,
	pub control_token: Option<String>,
	pub journal: Option<Arc<DaemonLog>>,
}

impl ServerIdentity {
	/// Identity for a plain `wsync serve` foreground daemon
	pub fn unmanaged() -> Self {
		Self {
			boot_id: Uuid::new_v4().to_string(),
			managed_by: String::from("cli"),
			control_token: None,
			journal: None,
		}
	}
}

#[derive(Clone)]
pub struct AppState {
	core: Arc<Core>,
	identity: Arc<Identity>,
	ws: Arc<ws::state::WsState>,
	lifecycle: Arc<lifecycle::Lifecycle>,
	divergence: Arc<divergence::Divergence>,
	backlog: Arc<backlog::BacklogStore>,
	audit: Arc<audit::WritesLog>,
}

pub struct Server {
	core: Arc<Core>,
	host: String,
	port: u16,
	identity: ServerIdentity,
	/// Overrides the state directory (`writes.log` home); defaults to the
	/// platform state dir resolution (`WSYNC_STATE_DIR` honored)
	state_dir: Option<PathBuf>,
}

impl Server {
	pub fn new(core: Arc<Core>, host: &str, port: u16, identity: ServerIdentity) -> Self {
		Self {
			core,
			host: host.to_owned(),
			port,
			identity,
			state_dir: None,
		}
	}

	/// Pins the state directory instead of resolving the platform default
	/// (used by hermetic tests and embedders)
	pub fn with_state_dir(mut self, state_dir: PathBuf) -> Self {
		self.state_dir = Some(state_dir);
		self
	}

	/// Resolves where `writes.log` lives: explicit override, then the
	/// platform state dir; a workspace-local fallback keeps the audit sink
	/// working even when no platform data dir resolves
	fn resolve_state_dir(&self) -> PathBuf {
		if let Some(state_dir) = &self.state_dir {
			return state_dir.clone();
		}

		daemon::state_dir(None).unwrap_or_else(|err| {
			let fallback = self.core.project().workspace_dir.join(".wsync-state");

			warn!(
				"Failed to resolve the platform state directory ({err}); writes.log falls back to {}",
				fallback.display()
			);

			fallback
		})
	}

	/// Runs the daemon until a graceful shutdown is triggered (`/stop`,
	/// `/manager-close` or the managed watchdog). `ready` is invoked exactly
	/// once with the actually bound port after the listener exists — the
	/// `daemon start` machine handshake and runtime-record write hang off it
	#[tokio::main]
	pub async fn start(&self, ready: Option<Box<dyn FnOnce(u16) + Send>>) -> Result<()> {
		// WSync servers bind loopback interfaces only; browser-facing routes
		// additionally require the owner capability once the app lands
		let addresses: Vec<_> = (self.host.as_str(), self.port).to_socket_addrs()?.collect();

		if addresses.is_empty() {
			bail!("Host {} does not resolve to any address", self.host);
		}

		if let Some(address) = addresses.iter().find(|address| !address.ip().is_loopback()) {
			bail!(
				"WSync only binds loopback addresses, but host {} resolves to {}",
				self.host,
				address.ip()
			);
		}

		// Prefer the IPv4 loopback: the Studio plugin and the desktop app both
		// probe 127.0.0.1, while `localhost` resolves to ::1 first on macOS —
		// binding whatever comes first would leave every 127.0.0.1 probe,
		// heartbeat, and port scan blind to this daemon (Design §3.2)
		let bind_address = addresses
			.iter()
			.find(|address| address.is_ipv4())
			.copied()
			.unwrap_or(addresses[0]);

		let listener = tokio::net::TcpListener::bind(bind_address).await?;
		let port = listener.local_addr().map(|address| address.port()).unwrap_or(self.port);

		let workspace_dir = self.core.project().workspace_dir.clone();

		let state = AppState {
			core: self.core.clone(),
			identity: Arc::new(Identity {
				boot_id: self.identity.boot_id.clone(),
				port,
				managed_by: self.identity.managed_by.clone(),
				control_token: self.identity.control_token.clone(),
				journal: self.identity.journal.clone(),
			}),
			ws: Arc::new(ws::state::WsState::new()),
			lifecycle: Arc::new(lifecycle::Lifecycle::new(self.identity.control_token.is_some())),
			divergence: Arc::new(divergence::Divergence::new()),
			// A pending disk review (Design §7.0) persists across restarts
			backlog: Arc::new(backlog::BacklogStore::load(&workspace_dir)),
			audit: Arc::new(audit::WritesLog::new(&self.resolve_state_dir())),
		};

		// A clash is never a question. The engine still parks — that is where
		// both sides are captured — but the park is resolved toward Studio
		// immediately, and the disk side goes to the backlog so the edit that
		// lost is recoverable for a day instead of being dropped or waiting on
		// a decision nobody planned to make.
		{
			let resolver = state.clone();
			// The engine notifies from the processor thread, which is not a
			// Tokio context, so the handle is captured here (where one exists)
			// rather than reached for at notify time
			let runtime = tokio::runtime::Handle::current();

			self.core.conflicts().set_notifier(Box::new(move |parked| {
				let state = resolver.clone();
				let parked = parked.clone();

				runtime.spawn(async move { resolve::auto_keep_studio(&state, parked).await });
			}));
		}

		lifecycle::spawn_watchdog(state.clone());

		// The msgpack (Argon-protocol) routes below are protocol v1's
		// fallback transport and are served unconditionally; `compat_argon`
		// only changes the version string `GET /details` reports
		let app = Router::new()
			.route("/details", get(details::main).fallback(Self::default_redirect))
			.route("/subscribe", post(subscribe::main).fallback(Self::default_redirect))
			.route("/unsubscribe", post(unsubscribe::main).fallback(Self::default_redirect))
			.route(
				"/snapshot",
				get(snapshot_json::main)
					.post(snapshot::main)
					.fallback(Self::default_redirect),
			)
			.route("/read", post(read::main).fallback(Self::default_redirect))
			.route("/write", post(write::main).fallback(Self::default_redirect))
			.route("/exec", post(exec::main).fallback(Self::default_redirect))
			.route("/open", post(open::main).fallback(Self::default_redirect))
			.route("/stop", post(stop::main).fallback(Self::default_redirect))
			.route("/hello", get(hello::main).fallback(Self::default_redirect))
			.route("/ws", get(ws::handler).fallback(Self::default_redirect))
			.route("/request", post(request::main).fallback(Self::default_redirect))
			.route(
				"/resolve",
				get(resolve::list)
					.post(resolve::resolve)
					.fallback(Self::default_redirect),
			)
			.route("/compare", post(divergence::compare).fallback(Self::default_redirect))
			.route(
				"/choice",
				get(divergence::choice_get)
					.post(divergence::choice_post)
					.fallback(Self::default_redirect),
			)
			.route(
				"/choice/details",
				get(divergence::choice_details).fallback(Self::default_redirect),
			)
			.route(
				"/choice/source",
				get(divergence::choice_source).fallback(Self::default_redirect),
			)
			.route(
				"/choice/selection",
				post(divergence::choice_selection).fallback(Self::default_redirect),
			)
			.route("/backlog", get(backlog::status).fallback(Self::default_redirect))
			.route(
				"/backlog/restore",
				post(backlog::restore).fallback(Self::default_redirect),
			)
			.route(
				"/backlog/drop",
				post(backlog::drop_entry).fallback(Self::default_redirect),
			)
			.route(
				"/manager-heartbeat",
				post(lifecycle::heartbeat).fallback(Self::default_redirect),
			)
			.route(
				"/manager-close",
				post(lifecycle::close).fallback(Self::default_redirect),
			)
			.route("/", get(home::main).fallback(Self::default_redirect))
			.fallback(Self::default_redirect)
			.layer(DefaultBodyLimit::max(MAX_PAYLOAD_SIZE))
			.with_state(state.clone());

		if let Some(ready) = ready {
			ready(port);
		}

		// Graceful shutdown: notify every connected client with a typed
		// shutdown frame, wake pending long-polls, then stop accepting and
		// drain in-flight requests
		let graceful = {
			let state = state.clone();
			let mut shutdown = state.lifecycle.subscribe();

			async move {
				while shutdown.changed().await.is_ok() {
					if shutdown.borrow().is_some() {
						break;
					}
				}

				let reason = shutdown.borrow().clone().unwrap_or_else(|| "Daemon stopping".into());

				if let Some(journal) = &state.identity.journal {
					journal.note(&format!("Stopping: {reason}"));
				}

				state.ws.emit(ws::frames::Event::Daemon {
					note: format!("Daemon stopping: {reason}"),
				});
				state.ws.broadcast_close(&reason, ws::frames::shutdown::DAEMON_STOPPING);

				// Wake blocked long-poll `/read` requests so the drain below
				// does not have to wait out their 60-second window
				state
					.core
					.queue()
					.push(
						Disconnect {
							message: format!("Daemon stopping: {reason}"),
						},
						None,
					)
					.ok();

				// Give the shutdown frames a moment to flush
				tokio::time::sleep(Duration::from_millis(200)).await;
			}
		};

		// Failsafe: if draining wedges (a client that never closes), force
		// the server loop to end so record cleanup still runs in the caller
		let force_exit = {
			let mut shutdown = state.lifecycle.subscribe();

			async move {
				while shutdown.changed().await.is_ok() {
					if shutdown.borrow().is_some() {
						break;
					}
				}

				tokio::time::sleep(SHUTDOWN_DRAIN_TIMEOUT).await;
			}
		};

		tokio::select! {
			result = axum::serve(listener, app).with_graceful_shutdown(graceful) => result?,
			_ = force_exit => debug!("Forced server loop exit after shutdown drain timeout"),
		}

		Ok(())
	}

	async fn default_redirect() -> Redirect {
		Redirect::temporary("/")
	}
}

pub fn is_port_free(host: &str, port: u16) -> bool {
	// Probe the same address the server would bind (IPv4 loopback preferred,
	// matching `Server::start`) — a plain `(host, port)` bind probes only the
	// first resolved stack, which on macOS is ::1 and misses IPv4 occupants
	match (host, port).to_socket_addrs() {
		Ok(addresses) => {
			let addresses: Vec<_> = addresses.collect();
			match addresses
				.iter()
				.find(|address| address.is_ipv4())
				.or_else(|| addresses.first())
			{
				Some(address) => TcpListener::bind(*address).is_ok(),
				None => false,
			}
		}
		Err(_) => false,
	}
}

/// Returns the first free port between `port` and `max_port` (inclusive),
/// scanning upwards from `port`
pub fn get_free_port(host: &str, port: u16, max_port: u16) -> Option<u16> {
	let mut port = port;

	while !is_port_free(host, port) {
		if port >= max_port {
			return None;
		}

		port += 1;
	}

	Some(port)
}

pub fn format_address(host: &str, port: u16) -> String {
	format!("http://{host}:{port}")
}
