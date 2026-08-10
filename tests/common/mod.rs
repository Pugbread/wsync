//! Shared harness for protocol/lifecycle integration tests: builds a scratch
//! place project, boots a real server on an ephemeral port, and provides a
//! minimal WS client.
#![allow(dead_code)]

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::{
	collections::{HashMap, HashSet},
	fs,
	net::TcpStream,
	path::PathBuf,
	sync::{mpsc, Arc, Mutex as StdMutex},
	thread,
	time::Duration,
};
use tempfile::TempDir;
use tokio_tungstenite::{tungstenite::Message, MaybeTlsStream, WebSocketStream};

use wsync::{
	core::Core,
	project::Project,
	server::{Server, ServerIdentity},
};

pub type WsClient = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

pub struct TestDaemon {
	pub port: u16,
	pub boot_id: String,
	/// Keeps the scratch project alive for the daemon's lifetime
	pub dir: TempDir,
	/// Canonicalized project root (macOS temp dirs live behind the
	/// `/var → /private/var` symlink; the VFS watcher reports canonical
	/// paths, so the project must be loaded through them too)
	pub root: PathBuf,
	pub handle: thread::JoinHandle<()>,
}

impl TestDaemon {
	pub fn http(&self, path: &str) -> String {
		format!("http://localhost:{}{}", self.port, path)
	}

	pub fn ws_url(&self) -> String {
		format!("ws://localhost:{}/ws", self.port)
	}

	pub fn src_dir(&self) -> PathBuf {
		self.root.join("src")
	}

	/// True once the daemon's listener no longer accepts connections
	pub fn wait_for_exit(&self, timeout: Duration) -> bool {
		let deadline = std::time::Instant::now() + timeout;

		while std::time::Instant::now() < deadline {
			if TcpStream::connect(("127.0.0.1", self.port)).is_err() {
				return true;
			}

			thread::sleep(Duration::from_millis(100));
		}

		false
	}
}

/// Writes a minimal place project with one synced service and one script.
/// Scratch projects live under `target/tmp` instead of the system temp dir:
/// macOS FSEvents does not deliver change events for `/var/folders` paths,
/// which would starve the watcher-driven tests.
///
/// Pinned to `"scope": "full"`: these fixtures exercise the full middleware
/// projection and the divergence-choice flow. Studio-first (code-scope)
/// behavior has its own fixtures via [`scratch_project_scoped`]
pub fn scratch_project() -> TempDir {
	let base = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"));
	fs::create_dir_all(base).unwrap();

	let dir = tempfile::Builder::new().prefix("wsync-test-").tempdir_in(base).unwrap();

	fs::create_dir_all(dir.path().join("src")).unwrap();
	fs::write(dir.path().join("src").join("Hello.luau"), "return \"hello\"\n").unwrap();

	let project = json!({
		"name": "wsync-fixture",
		"tree": {
			"$className": "DataModel",
			"ReplicatedStorage": { "$path": "src" },
		},
		"gameId": 5550001,
		"placeIds": [7770001],
		"scope": "full",
	});

	fs::write(
		dir.path().join("default.project.json"),
		serde_json::to_string_pretty(&project).unwrap(),
	)
	.unwrap();

	dir
}

/// Boots a real server (ephemeral port) for the scratch project on a
/// background thread and waits until it is ready
pub fn start_daemon(control_token: Option<&str>) -> TestDaemon {
	let dir = scratch_project();
	let root = dir.path().canonicalize().unwrap();
	let project = Project::load(&root.join("default.project.json")).unwrap();
	let core = std::sync::Arc::new(Core::new(project, true).unwrap());

	let boot_id = uuid::Uuid::new_v4().to_string();
	let identity = ServerIdentity {
		boot_id: boot_id.clone(),
		managed_by: "test".into(),
		control_token: control_token.map(str::to_owned),
		journal: None,
	};

	let (ready_tx, ready_rx) = mpsc::channel();
	let server = Server::new(core, "localhost", 0, identity);

	let handle = thread::spawn(move || {
		let ready = Box::new(move |port: u16| {
			ready_tx.send(port).unwrap();
		});

		server.start(Some(ready)).unwrap();
	});

	let port = ready_rx
		.recv_timeout(Duration::from_secs(15))
		.expect("server did not become ready");

	TestDaemon {
		port,
		boot_id,
		dir,
		root,
		handle,
	}
}

/// Connects a WS client and performs the hello handshake, returning the
/// socket and the server hello frame
pub async fn connect_client(daemon: &TestDaemon, role: &str, name: &str) -> (WsClient, Value) {
	let (mut socket, _) = tokio_tungstenite::connect_async(daemon.ws_url()).await.unwrap();

	let hello = json!({
		"type": "hello",
		"clientId": format!("test-{name}"),
		"role": role,
		"protocol": 1,
		"name": name,
	});

	socket.send(Message::Text(hello.to_string())).await.unwrap();

	let server_hello = next_frame(&mut socket, Duration::from_secs(10)).await.unwrap();

	(socket, server_hello)
}

/// Reads frames until the next JSON text frame (skipping protocol frames),
/// with a deadline. Returns None when the connection closes first
pub async fn next_frame(socket: &mut WsClient, timeout: Duration) -> Option<Value> {
	let deadline = tokio::time::Instant::now() + timeout;

	loop {
		let remaining = deadline.checked_duration_since(tokio::time::Instant::now())?;

		let message = tokio::time::timeout(remaining, socket.next()).await.ok()??;

		match message.ok()? {
			Message::Text(text) => return serde_json::from_str(&text).ok(),
			Message::Close(_) => return None,
			_ => continue,
		}
	}
}

/// Reads frames (answering pings) until one matches the predicate
pub async fn wait_for_frame<F>(socket: &mut WsClient, timeout: Duration, mut predicate: F) -> Option<Value>
where
	F: FnMut(&Value) -> bool,
{
	let deadline = tokio::time::Instant::now() + timeout;

	loop {
		let remaining = deadline.checked_duration_since(tokio::time::Instant::now())?;
		let message = tokio::time::timeout(remaining, socket.next()).await.ok()??;

		let frame: Value = match message.ok()? {
			Message::Text(text) => serde_json::from_str(&text).ok()?,
			Message::Close(_) => return None,
			_ => continue,
		};

		if frame.get("type").and_then(Value::as_str) == Some("ping") {
			socket.send(Message::Text(r#"{"type":"pong"}"#.into())).await.ok()?;
			continue;
		}

		if predicate(&frame) {
			return Some(frame);
		}
	}
}

// Additive Phase-4 helpers: custom scratch projects, a pinned state dir for
// the audit surfaces, plain HTTP helpers and a scriptable fake WS plugin
// implementing the bulk-apply op contracts.

/// Writes a scratch place project with an arbitrary `tree` and initial files
/// (workspace-relative path → contents). Parent directories are created.
/// Pinned to `"scope": "full"` like [`scratch_project`]
pub fn scratch_project_custom(tree: Value, files: &[(&str, &str)]) -> TempDir {
	scratch_project_scoped(tree, files, Some("full"))
}

/// [`scratch_project_custom`] with an explicit `scope` field: `Some("full")`
/// / `Some("code")` write it, `None` omits it (exercising the code-scope
/// default of Design §7.0)
pub fn scratch_project_scoped(tree: Value, files: &[(&str, &str)], scope: Option<&str>) -> TempDir {
	let base = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"));
	fs::create_dir_all(base).unwrap();

	let dir = tempfile::Builder::new().prefix("wsync-test-").tempdir_in(base).unwrap();

	for (path, contents) in files {
		let path = dir.path().join(path);

		fs::create_dir_all(path.parent().unwrap()).unwrap();
		fs::write(path, contents).unwrap();
	}

	let mut project = json!({
		"name": "wsync-fixture",
		"tree": tree,
		"gameId": 5550001,
		"placeIds": [7770001],
	});

	if let Some(scope) = scope {
		project["scope"] = json!(scope);
	}

	fs::write(
		dir.path().join("default.project.json"),
		serde_json::to_string_pretty(&project).unwrap(),
	)
	.unwrap();

	dir
}

/// Boots a daemon for an existing scratch project directory. The state dir
/// (home of `writes.log`) is pinned inside the scratch dir at `state/`, so
/// audit assertions stay hermetic
pub fn start_daemon_in(dir: TempDir) -> TestDaemon {
	let root = dir.path().canonicalize().unwrap();
	let project = Project::load(&root.join("default.project.json")).unwrap();
	let core = std::sync::Arc::new(Core::new(project, true).unwrap());

	let boot_id = uuid::Uuid::new_v4().to_string();
	let identity = ServerIdentity {
		boot_id: boot_id.clone(),
		managed_by: "test".into(),
		control_token: None,
		journal: None,
	};

	let (ready_tx, ready_rx) = mpsc::channel();
	let server = Server::new(core, "localhost", 0, identity).with_state_dir(root.join("state"));

	let handle = thread::spawn(move || {
		let ready = Box::new(move |port: u16| {
			ready_tx.send(port).unwrap();
		});

		server.start(Some(ready)).unwrap();
	});

	let port = ready_rx
		.recv_timeout(Duration::from_secs(15))
		.expect("server did not become ready");

	TestDaemon {
		port,
		boot_id,
		dir,
		root,
		handle,
	}
}

/// Boots the standard one-service scratch project with a pinned state dir
pub fn start_daemon_with_state_dir() -> TestDaemon {
	start_daemon_in(scratch_project())
}

/// The daemon's pinned state dir when booted through `start_daemon_in`
pub fn state_dir(daemon: &TestDaemon) -> PathBuf {
	daemon.root.join("state")
}

pub async fn post_json(daemon: &TestDaemon, path: &str, body: &Value) -> (reqwest::StatusCode, Value) {
	let response = reqwest::Client::new()
		.post(daemon.http(path))
		.json(body)
		.send()
		.await
		.unwrap();

	let status = response.status();
	(status, response.json().await.unwrap())
}

pub async fn get_json(daemon: &TestDaemon, path: &str) -> (reqwest::StatusCode, Value) {
	let response = reqwest::get(daemon.http(path)).await.unwrap();
	let status = response.status();

	(status, response.json().await.unwrap())
}

/// Full-tree JSON snapshot
pub async fn snapshot_json(daemon: &TestDaemon) -> Value {
	get_json(daemon, "/snapshot").await.1
}

/// The ref of a top-level service child (e.g. `ReplicatedStorage`) from the
/// live snapshot
pub async fn service_ref(daemon: &TestDaemon, service: &str) -> Option<String> {
	let snapshot = snapshot_json(daemon).await;

	snapshot["children"]
		.as_array()?
		.iter()
		.find(|child| child["name"] == service)
		.and_then(|child| child["id"].as_str())
		.map(str::to_owned)
}

/// The ref of an instance one level below a service
pub async fn child_ref_in(daemon: &TestDaemon, service: &str, name: &str) -> Option<String> {
	let snapshot = snapshot_json(daemon).await;

	let parent = snapshot["children"]
		.as_array()?
		.iter()
		.find(|child| child["name"] == service)?;

	parent["children"]
		.as_array()?
		.iter()
		.find(|child| child["name"] == name)
		.and_then(|child| child["id"].as_str())
		.map(str::to_owned)
}

/// Builds the `snapshot.sources` claim map for a set of script sources
/// (ref hex → bytes), with real lengths and SHA-256 digests
pub fn source_claims(sources: &HashMap<String, Vec<u8>>) -> Value {
	use sha2::{Digest, Sha256};

	let mut claims = serde_json::Map::new();

	for (reference, bytes) in sources {
		claims.insert(
			reference.clone(),
			json!({
				"bytes": bytes.len(),
				"sha256": format!("{:x}", Sha256::digest(bytes)),
			}),
		);
	}

	Value::Object(claims)
}

/// Script for the fake WS plugin: canned answers for the bulk-apply op
/// surface (`read_subtree`, `source_read`, `transaction_*`, `get`) plus
/// optional side effects
#[derive(Default)]
pub struct FakePluginScript {
	/// `read_subtree` responses: root ref hex → the `snapshot` value
	/// (AddedSnapshot shape with `sources` sibling map inside)
	pub subtrees: HashMap<String, Value>,
	/// `source_read` byte stores: script ref hex → full contents
	pub sources: HashMap<String, Vec<u8>>,
	/// Refs whose served bytes are corrupted (first byte flipped) while the
	/// claims still advertise the original digest
	pub corrupt: HashSet<String>,
	/// Files written the moment a `read_subtree` arrives (fence tests:
	/// guaranteed to land after the fingerprint, before the swap)
	pub touch_on_read_subtree: Vec<(PathBuf, String)>,
	/// `get {path, prop: "Source"}` answers: instance path → source
	pub get_sources: HashMap<String, String>,
	/// Fully canned responses by op name (fields merged into the response
	/// frame: `ok`, `value`, `error`, `meta`). Overrides the built-in ops
	pub canned: HashMap<String, Value>,
}

/// A connected fake plugin driven by a `FakePluginScript`. `events` is the
/// ordered log of protocol activity (op names, `sync`, `push-result`);
/// `frames` keeps every received frame for detailed assertions
pub struct FakePlugin {
	pub events: Arc<StdMutex<Vec<String>>>,
	pub frames: Arc<StdMutex<Vec<Value>>>,
	outbound: tokio::sync::mpsc::UnboundedSender<String>,
	handle: tokio::task::JoinHandle<()>,
}

impl FakePlugin {
	/// Snapshot of the ordered event log
	pub fn events(&self) -> Vec<String> {
		self.events.lock().unwrap().clone()
	}

	/// Sends a raw client frame (e.g. a `push`) through the socket
	pub fn send(&self, frame: &Value) {
		self.outbound.send(frame.to_string()).unwrap();
	}

	/// Waits until a received frame matches the predicate
	pub async fn wait_for<F>(&self, timeout: Duration, mut predicate: F) -> Option<Value>
	where
		F: FnMut(&Value) -> bool,
	{
		let deadline = tokio::time::Instant::now() + timeout;

		loop {
			{
				let frames = self.frames.lock().unwrap();

				if let Some(frame) = frames.iter().find(|frame| predicate(frame)) {
					return Some(frame.clone());
				}
			}

			if tokio::time::Instant::now() >= deadline {
				return None;
			}

			tokio::time::sleep(Duration::from_millis(25)).await;
		}
	}

	pub fn abort(&self) {
		self.handle.abort();
	}
}

/// Connects a fake WS plugin and serves the scripted op surface on a
/// background task
pub async fn spawn_fake_plugin(daemon: &TestDaemon, name: &str, script: FakePluginScript) -> FakePlugin {
	use base64::Engine;

	let (mut socket, _) = connect_client(daemon, "plugin", name).await;

	let events = Arc::new(StdMutex::new(Vec::new()));
	let frames = Arc::new(StdMutex::new(Vec::new()));
	let (outbound_tx, mut outbound_rx) = tokio::sync::mpsc::unbounded_channel::<String>();

	let task_events = events.clone();
	let task_frames = frames.clone();

	let handle = tokio::spawn(async move {
		loop {
			let message = tokio::select! {
				frame = outbound_rx.recv() => {
					match frame {
						Some(text) => {
							socket.send(Message::Text(text)).await.ok();
							continue;
						}
						// The handle was dropped; nothing can assert anymore
						None => break,
					}
				}
				message = socket.next() => message,
			};

			let Some(Ok(message)) = message else { break };

			let text = match message {
				Message::Text(text) => text,
				Message::Close(_) => break,
				_ => continue,
			};

			let Ok(frame) = serde_json::from_str::<Value>(&text) else {
				continue;
			};

			let kind = frame.get("type").and_then(Value::as_str).unwrap_or_default().to_owned();

			if kind == "ping" {
				socket.send(Message::Text(r#"{"type":"pong"}"#.into())).await.ok();
				continue;
			}

			task_frames.lock().unwrap().push(frame.clone());

			if kind != "request" {
				task_events.lock().unwrap().push(kind.clone());
				continue;
			}

			let request_id = frame["request_id"].as_u64().unwrap_or_default();
			let op = frame["op"].as_str().unwrap_or_default().to_owned();
			let args = frame.get("args").cloned().unwrap_or_else(|| json!({}));

			let logged = match op.as_str() {
				"transaction_finish" => format!(
					"transaction_finish:{}",
					args.get("commit").and_then(Value::as_bool).unwrap_or_default()
				),
				other => other.to_owned(),
			};

			task_events.lock().unwrap().push(logged);

			if let Some(canned) = script.canned.get(&op) {
				let mut response = json!({ "type": "response", "request_id": request_id });

				if let (Some(response), Some(canned)) = (response.as_object_mut(), canned.as_object()) {
					for (key, value) in canned {
						response.insert(key.clone(), value.clone());
					}
				}

				socket.send(Message::Text(response.to_string())).await.ok();
				continue;
			}

			let response = match op.as_str() {
				"read_subtree" => {
					for (path, contents) in &script.touch_on_read_subtree {
						fs::write(path, contents).expect("fence-touch write failed");
					}

					let reference = args.get("ref").and_then(Value::as_str).unwrap_or_default();

					match script.subtrees.get(reference) {
						Some(snapshot) => ok_response(request_id, json!({ "snapshot": snapshot })),
						None => err_response(request_id, "NOT_FOUND", "no such subtree"),
					}
				}
				"source_read" => {
					let reference = args.get("ref").and_then(Value::as_str).unwrap_or_default();
					let offset = args.get("offset").and_then(Value::as_u64).unwrap_or_default() as usize;
					let max_len = args.get("maxLen").and_then(Value::as_u64).unwrap_or(65536) as usize;

					match script.sources.get(reference) {
						Some(bytes) => {
							let mut bytes = bytes.clone();

							if script.corrupt.contains(reference) {
								if let Some(first) = bytes.first_mut() {
									*first ^= 0xFF;
								}
							}

							let end = (offset + max_len).min(bytes.len());
							let part = &bytes[offset.min(bytes.len())..end];

							ok_response(
								request_id,
								json!({
									"data": base64::engine::general_purpose::STANDARD.encode(part),
									"eof": end >= bytes.len(),
									"totalBytes": bytes.len(),
								}),
							)
						}
						None => err_response(request_id, "NOT_FOUND", "no such source"),
					}
				}
				"transaction_begin" | "transaction_finish" => ok_response(request_id, json!({})),
				"get" => {
					let path = args.get("path").and_then(Value::as_str).unwrap_or_default();

					match script.get_sources.get(path) {
						Some(source) => ok_response(request_id, json!(source)),
						None => err_response(request_id, "NOT_FOUND", "no such instance"),
					}
				}
				_ => err_response(request_id, "UNKNOWN_OP", "fake plugin does not serve this op"),
			};

			socket.send(Message::Text(response.to_string())).await.ok();
		}
	});

	FakePlugin {
		events,
		frames,
		outbound: outbound_tx,
		handle,
	}
}

// Additive Phase-5 helpers: an isolated sandbox for driving the real `wsync`
// binary, and a journalled synchronous fake plugin for the CLI-surface tests
// (capture, clipboard, registry). These mirror the fixtures cli_live.rs and
// cli_write.rs carry privately, shared here so the newer test files stop
// duplicating them.

/// Isolated environment for one CLI invocation: `$HOME` carries a config that
/// turns off the update check and the plugin installer, and the state dir is
/// pinned via `WSYNC_STATE_DIR` — so the child process is fast, offline, and
/// cannot disturb a real installation. The state dir is also where the
/// cross-project clipboard lives, so two runs through one sandbox share it
pub struct CliSandbox {
	pub home: TempDir,
	pub state: TempDir,
	/// Scratch space for input files and `-o` targets
	pub work: TempDir,
}

impl CliSandbox {
	pub fn new() -> Self {
		let base = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"));
		fs::create_dir_all(base).unwrap();

		let temp = |prefix: &str| {
			tempfile::Builder::new()
				.prefix(prefix)
				.tempdir_in(base)
				.expect("failed to create a scratch directory")
		};

		let home = temp("wsync-cli-home-");
		let config_dir = home.path().join(".wsync");

		fs::create_dir_all(&config_dir).unwrap();
		fs::write(
			config_dir.join("config.toml"),
			"check_updates = false\ninstall_plugin = false\n",
		)
		.unwrap();

		Self {
			home,
			state: temp("wsync-cli-state-"),
			work: temp("wsync-cli-work-"),
		}
	}

	pub fn run(&self, args: &[&str]) -> std::process::Output {
		std::process::Command::new(env!("CARGO_BIN_EXE_wsync"))
			.args(args)
			.env("HOME", self.home.path())
			.env("USERPROFILE", self.home.path())
			.env("WSYNC_STATE_DIR", self.state.path())
			.env("NO_COLOR", "1")
			.env_remove("RUST_LOG")
			.env_remove("RUST_VERBOSE")
			.output()
			.expect("failed to run the wsync binary")
	}

	/// [`Self::run`] with extra environment variables — for the surfaces
	/// whose behavior is env-driven (cloud base URLs, toolchain resolution,
	/// a pinned PATH)
	pub fn run_with_envs(&self, args: &[&str], envs: &[(&str, &str)]) -> std::process::Output {
		let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_wsync"));

		command
			.args(args)
			.env("HOME", self.home.path())
			.env("USERPROFILE", self.home.path())
			.env("WSYNC_STATE_DIR", self.state.path())
			.env("NO_COLOR", "1")
			.env_remove("RUST_LOG")
			.env_remove("RUST_VERBOSE");

		for (name, value) in envs {
			command.env(name, value);
		}

		command.output().expect("failed to run the wsync binary")
	}

	/// [`Self::run`] with bytes piped into the child's stdin — for the
	/// credential surface, whose secrets are never accepted as arguments
	pub fn run_with_stdin(&self, args: &[&str], stdin: &[u8]) -> std::process::Output {
		use std::io::Write as _;

		let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_wsync"))
			.args(args)
			.env("HOME", self.home.path())
			.env("USERPROFILE", self.home.path())
			.env("WSYNC_STATE_DIR", self.state.path())
			.env("NO_COLOR", "1")
			.env_remove("RUST_LOG")
			.env_remove("RUST_VERBOSE")
			.stdin(std::process::Stdio::piped())
			.stdout(std::process::Stdio::piped())
			.stderr(std::process::Stdio::piped())
			.spawn()
			.expect("failed to spawn the wsync binary");

		child
			.stdin
			.take()
			.expect("stdin was piped")
			.write_all(stdin)
			.expect("failed to write the child's stdin");

		child.wait_with_output().expect("failed to run the wsync binary")
	}

	/// Spawns the binary without waiting, stdout piped — for streaming
	/// commands (`tail`) that a test reads from and then kills
	pub fn spawn(&self, args: &[&str]) -> std::process::Child {
		std::process::Command::new(env!("CARGO_BIN_EXE_wsync"))
			.args(args)
			.env("HOME", self.home.path())
			.env("USERPROFILE", self.home.path())
			.env("WSYNC_STATE_DIR", self.state.path())
			.env("NO_COLOR", "1")
			.env_remove("RUST_LOG")
			.env_remove("RUST_VERBOSE")
			.stdout(std::process::Stdio::piped())
			.stderr(std::process::Stdio::piped())
			.spawn()
			.expect("failed to spawn the wsync binary")
	}
}

pub fn cli_stdout(output: &std::process::Output) -> String {
	String::from_utf8_lossy(&output.stdout).into_owned()
}

pub fn cli_stderr(output: &std::process::Output) -> String {
	String::from_utf8_lossy(&output.stderr).into_owned()
}

/// The first JSON value on stdout (a `--raw` line, or a pretty-printed dump)
pub fn cli_json(output: &std::process::Output) -> Value {
	let text = cli_stdout(output);
	let trimmed = text.trim();

	assert!(
		!trimmed.is_empty(),
		"no stdout to parse as JSON; stderr was:\n{}",
		cli_stderr(output)
	);

	serde_json::from_str(trimmed).unwrap_or_else(|err| panic!("stdout is not JSON ({err}): {trimmed}"))
}

/// What the journalled fake plugin does with one `request` frame
pub enum CliAnswer {
	Value(Value),
	Failure(&'static str, &'static str),
}

pub type CliResponder = Arc<dyn Fn(&str, &Value) -> CliAnswer + Send + Sync>;

/// Every `(op, args)` the fake plugin received, in order — journalled before
/// it is answered, so "refused before the network" is a testable claim
pub type CliJournal = Arc<StdMutex<Vec<(String, Value)>>>;

/// Connects a WS client into the daemon's plugin slot, journals every op it
/// is asked for, and answers from `responder` until the connection drops.
/// Returns once the handshake is complete, so the CLI can be launched
/// immediately afterwards
pub fn spawn_cli_plugin(daemon: &TestDaemon, name: &'static str, responder: CliResponder) -> CliJournal {
	let journal: CliJournal = Arc::new(StdMutex::new(Vec::new()));
	let recorder = Arc::clone(&journal);
	let url = daemon.ws_url();
	let (ready_tx, ready_rx) = mpsc::channel();

	thread::spawn(move || {
		let runtime = tokio::runtime::Builder::new_current_thread()
			.enable_all()
			.build()
			.unwrap();

		runtime.block_on(async move {
			let (mut socket, _) = tokio_tungstenite::connect_async(url).await.unwrap();

			let hello = json!({
				"type": "hello",
				"clientId": format!("test-{name}"),
				"role": "plugin",
				"protocol": 1,
				"name": name,
			});

			socket.send(Message::Text(hello.to_string())).await.unwrap();

			// The daemon's hello answer means the plugin slot is ours
			let _ = socket.next().await;
			ready_tx.send(()).unwrap();

			loop {
				let message = match tokio::time::timeout(Duration::from_millis(250), socket.next()).await {
					Ok(Some(Ok(message))) => message,
					// Idle tick: nothing to read yet
					Err(_) => continue,
					_ => break,
				};

				let Message::Text(text) = message else {
					continue;
				};

				let Ok(frame) = serde_json::from_str::<Value>(&text) else {
					continue;
				};

				match frame["type"].as_str() {
					Some("ping") => {
						socket.send(Message::Text(r#"{"type":"pong"}"#.into())).await.ok();
					}
					Some("request") => {
						let op = frame["op"].as_str().unwrap_or_default().to_owned();
						let args = frame["args"].clone();

						recorder.lock().unwrap().push((op.clone(), args.clone()));

						let response = match responder(&op, &args) {
							CliAnswer::Value(value) => json!({
								"type": "response",
								"request_id": frame["request_id"],
								"ok": true,
								"value": value,
							}),
							CliAnswer::Failure(code, message) => json!({
								"type": "response",
								"request_id": frame["request_id"],
								"ok": false,
								"error": { "code": code, "message": message },
							}),
						};

						socket.send(Message::Text(response.to_string())).await.ok();
					}
					_ => {}
				}
			}
		});
	});

	ready_rx
		.recv_timeout(Duration::from_secs(15))
		.expect("the fake plugin never completed its handshake");

	journal
}

/// Ops the fake plugin received, by name
pub fn journal_ops(journal: &CliJournal) -> Vec<String> {
	journal.lock().unwrap().iter().map(|(op, _)| op.clone()).collect()
}

/// The arguments of the first `op` the fake plugin received
pub fn journal_args(journal: &CliJournal, op: &str) -> Value {
	journal
		.lock()
		.unwrap()
		.iter()
		.find(|(name, _)| name == op)
		.map(|(_, args)| args.clone())
		.unwrap_or_else(|| {
			panic!(
				"the fake plugin never received op `{op}`; it saw {:?}",
				journal_ops(journal)
			)
		})
}

/// Serves one `{offset, maxLen}` chunk read out of a byte store, in the
/// `{data, eof, totalBytes}` shape the transfer op families share
pub fn chunk_answer(bytes: &[u8], args: &Value) -> Value {
	use base64::Engine;

	let offset = args["offset"].as_u64().unwrap_or(0) as usize;
	let max_len = args["maxLen"].as_u64().unwrap_or(65536) as usize;
	let start = offset.min(bytes.len());
	let end = (start + max_len).min(bytes.len());

	json!({
		"data": base64::engine::general_purpose::STANDARD.encode(&bytes[start..end]),
		"eof": end >= bytes.len(),
		"totalBytes": bytes.len(),
	})
}

// Additive Phase-6 helpers: a minimal local HTTP stub for the Open Cloud
// command family (`upload`, `monetization` — pointed here through
// `WSYNC_CLOUD_BASE_URL`), plus multipart body inspection.

/// One recorded HTTP request the cloud stub served
#[derive(Clone)]
pub struct StubRequest {
	pub method: String,
	pub path: String,
	/// Header names lowercased
	pub headers: HashMap<String, String>,
	pub body: Vec<u8>,
}

impl StubRequest {
	pub fn header(&self, name: &str) -> Option<&str> {
		self.headers.get(&name.to_ascii_lowercase()).map(String::as_str)
	}

	/// The multipart parts of this request's body, per its content-type
	/// boundary
	pub fn multipart(&self) -> Vec<MultipartPart> {
		let content_type = self.header("content-type").unwrap_or_default();

		parse_multipart(&self.body, content_type)
	}

	/// The UTF-8 content of the named multipart field
	pub fn part_text(&self, name: &str) -> Option<String> {
		self.multipart()
			.into_iter()
			.find(|part| part.name.as_deref() == Some(name))
			.map(|part| String::from_utf8_lossy(&part.data).into_owned())
	}

	/// The named multipart field parsed as JSON
	pub fn part_json(&self, name: &str) -> Option<Value> {
		serde_json::from_str(&self.part_text(name)?).ok()
	}
}

/// One decoded `multipart/form-data` part
#[derive(Clone)]
pub struct MultipartPart {
	pub name: Option<String>,
	pub filename: Option<String>,
	pub content_type: Option<String>,
	pub data: Vec<u8>,
}

pub type StubResponder = Arc<dyn Fn(&StubRequest) -> (u16, Value) + Send + Sync>;

/// A local HTTP server answering from `responder` and journalling every
/// request. Lives on a detached thread for the test process lifetime
pub struct CloudStub {
	pub base_url: String,
	requests: Arc<StdMutex<Vec<StubRequest>>>,
}

impl CloudStub {
	pub fn requests(&self) -> Vec<StubRequest> {
		self.requests.lock().unwrap().clone()
	}

	/// Requests matching a path substring, in arrival order
	pub fn requests_to(&self, path_part: &str) -> Vec<StubRequest> {
		self.requests()
			.into_iter()
			.filter(|request| request.path.contains(path_part))
			.collect()
	}
}

pub fn start_cloud_stub(responder: StubResponder) -> CloudStub {
	use std::io::{Read as _, Write as _};

	let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("failed to bind the cloud stub");
	let addr = listener.local_addr().unwrap();
	let requests: Arc<StdMutex<Vec<StubRequest>>> = Arc::new(StdMutex::new(Vec::new()));
	let journal = Arc::clone(&requests);

	thread::spawn(move || {
		for stream in listener.incoming() {
			let Ok(mut stream) = stream else { continue };

			// Read the head
			let mut buffer: Vec<u8> = Vec::new();
			let mut chunk = [0u8; 4096];

			let head_end = loop {
				match stream.read(&mut chunk) {
					Ok(0) => break None,
					Ok(read) => {
						buffer.extend_from_slice(&chunk[..read]);

						if let Some(position) = find_subslice(&buffer, b"\r\n\r\n") {
							break Some(position + 4);
						}
					}
					Err(_) => break None,
				}
			};

			let Some(head_end) = head_end else { continue };

			let head = String::from_utf8_lossy(&buffer[..head_end]).into_owned();
			let mut lines = head.split("\r\n");
			let request_line = lines.next().unwrap_or_default();
			let mut parts = request_line.split_whitespace();
			let method = parts.next().unwrap_or_default().to_owned();
			let path = parts.next().unwrap_or_default().to_owned();

			let mut headers: HashMap<String, String> = HashMap::new();

			for line in lines {
				if let Some((name, value)) = line.split_once(':') {
					headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_owned());
				}
			}

			// Read the body: content-length, or minimal chunked decoding
			let mut body: Vec<u8> = buffer[head_end..].to_vec();

			if let Some(length) = headers
				.get("content-length")
				.and_then(|value| value.parse::<usize>().ok())
			{
				while body.len() < length {
					match stream.read(&mut chunk) {
						Ok(0) => break,
						Ok(read) => body.extend_from_slice(&chunk[..read]),
						Err(_) => break,
					}
				}

				body.truncate(length);
			} else if headers
				.get("transfer-encoding")
				.is_some_and(|value| value.eq_ignore_ascii_case("chunked"))
			{
				// Accumulate until the terminating zero-length chunk
				while find_subslice(&body, b"\r\n0\r\n").is_none() && !body.starts_with(b"0\r\n") {
					match stream.read(&mut chunk) {
						Ok(0) => break,
						Ok(read) => body.extend_from_slice(&chunk[..read]),
						Err(_) => break,
					}
				}

				body = decode_chunked(&body);
			}

			let request = StubRequest {
				method,
				path,
				headers,
				body,
			};

			journal.lock().unwrap().push(request.clone());

			let (status, value) = responder(&request);
			let payload = value.to_string();

			let response = format!(
				"HTTP/1.1 {status} STUB\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{payload}",
				payload.len()
			);

			stream.write_all(response.as_bytes()).ok();
			stream.flush().ok();
		}
	});

	CloudStub {
		base_url: format!("http://{addr}"),
		requests,
	}
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
	haystack.windows(needle.len()).position(|window| window == needle)
}

fn decode_chunked(body: &[u8]) -> Vec<u8> {
	let mut decoded = Vec::new();
	let mut offset = 0;

	while offset < body.len() {
		let Some(line_end) = find_subslice(&body[offset..], b"\r\n").map(|position| offset + position) else {
			break;
		};

		let size_text = String::from_utf8_lossy(&body[offset..line_end]);
		let Ok(size) = usize::from_str_radix(size_text.trim(), 16) else {
			break;
		};

		if size == 0 {
			break;
		}

		let data_start = line_end + 2;
		let data_end = (data_start + size).min(body.len());

		decoded.extend_from_slice(&body[data_start..data_end]);
		offset = data_end + 2;
	}

	decoded
}

/// Splits a `multipart/form-data` body on its boundary and decodes each
/// part's headers
pub fn parse_multipart(body: &[u8], content_type: &str) -> Vec<MultipartPart> {
	let Some(boundary) = content_type
		.split(';')
		.map(str::trim)
		.find_map(|part| part.strip_prefix("boundary="))
	else {
		return Vec::new();
	};

	let delimiter = format!("--{}", boundary.trim_matches('"'));
	let mut parts = Vec::new();
	let mut offset = 0;

	// Advance from delimiter to delimiter
	while let Some(position) = find_subslice(&body[offset..], delimiter.as_bytes()) {
		let start = offset + position + delimiter.len();

		// Closing delimiter
		if body[start..].starts_with(b"--") {
			break;
		}

		// Skip the CRLF after the delimiter
		let start = if body[start..].starts_with(b"\r\n") {
			start + 2
		} else {
			start
		};

		let Some(head_length) = find_subslice(&body[start..], b"\r\n\r\n") else {
			break;
		};

		let head = String::from_utf8_lossy(&body[start..start + head_length]).into_owned();
		let data_start = start + head_length + 4;

		let Some(next) = find_subslice(&body[data_start..], delimiter.as_bytes()) else {
			break;
		};

		// The CRLF before the next delimiter belongs to the framing
		let data_end = (data_start + next).saturating_sub(2);

		let mut part = MultipartPart {
			name: None,
			filename: None,
			content_type: None,
			data: body[data_start..data_end].to_vec(),
		};

		for line in head.split("\r\n") {
			let lower = line.to_ascii_lowercase();

			if lower.starts_with("content-disposition:") {
				part.name = extract_quoted(line, "name=");
				part.filename = extract_quoted(line, "filename=");
			} else if let Some(value) = lower.strip_prefix("content-type:") {
				part.content_type = Some(value.trim().to_owned());
			}
		}

		parts.push(part);
		offset = data_start + next;
	}

	parts
}

fn extract_quoted(line: &str, key: &str) -> Option<String> {
	let start = line.find(key)? + key.len();
	let rest = &line[start..];
	let rest = rest.strip_prefix('"')?;
	let end = rest.find('"')?;

	Some(rest[..end].to_owned())
}

fn ok_response(request_id: u64, value: Value) -> Value {
	json!({ "type": "response", "request_id": request_id, "ok": true, "value": value })
}

fn err_response(request_id: u64, code: &str, message: &str) -> Value {
	json!({
		"type": "response",
		"request_id": request_id,
		"ok": false,
		"error": { "code": code, "message": message },
	})
}
