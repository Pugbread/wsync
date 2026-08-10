//! Integration coverage for the live CLI surface (Design §10.1–10.2): the
//! real `wsync` binary is executed against a real daemon with a fake Studio
//! plugin on the WebSocket, so discovery, the `POST /request` hop, `--raw`
//! shapes, and exit codes are exercised end to end rather than mocked.
//!
//! Three fixtures carry the file:
//!
//! * `Sandbox` — an isolated `$HOME` and state dir, so a test run never
//!   touches the developer's `~/.wsync` and never reaches the network;
//! * `spawn_plugin` — a WS client in the plugin slot that answers `request`
//!   frames from a per-test responder (including "never answer", for the
//!   timeout path);
//! * `spawn_stub` — a ~40-line HTTP responder for the `/review` (and legacy
//!   `/choice`) surfaces, so the CLI shapes are pinned without staging a full
//!   Studio-first apply.

mod common;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::{
	fs,
	io::{BufRead, BufReader, Read, Write},
	net::{TcpListener, TcpStream},
	path::Path,
	process::{Command, Output},
	sync::Arc,
	thread,
	time::{Duration, Instant},
};
use tempfile::TempDir;
use tokio_tungstenite::tungstenite::Message;

use common::{scratch_project, start_daemon, TestDaemon};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// Isolated environment for one CLI invocation. `$HOME` gets a config that
/// turns off the update check and the plugin installer, so the child process
/// is fast, offline, and cannot disturb the developer's real installation
struct Sandbox {
	home: TempDir,
	state: TempDir,
}

impl Sandbox {
	fn new() -> Self {
		let base = Path::new(env!("CARGO_TARGET_TMPDIR"));
		fs::create_dir_all(base).unwrap();

		let home = tempfile::Builder::new().prefix("wsync-home-").tempdir_in(base).unwrap();
		let state = tempfile::Builder::new()
			.prefix("wsync-state-")
			.tempdir_in(base)
			.unwrap();

		let config_dir = home.path().join(".wsync");
		fs::create_dir_all(&config_dir).unwrap();
		fs::write(
			config_dir.join("config.toml"),
			"check_updates = false\ninstall_plugin = false\n",
		)
		.unwrap();

		Self { home, state }
	}

	fn run(&self, args: &[&str]) -> Output {
		Command::new(env!("CARGO_BIN_EXE_wsync"))
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
}

fn stdout(output: &Output) -> String {
	String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
	String::from_utf8_lossy(&output.stderr).into_owned()
}

/// The single JSON line a `--raw` command prints on stdout
fn raw_json(output: &Output) -> Value {
	let text = stdout(output);
	let line = text
		.lines()
		.find(|line| !line.trim().is_empty())
		.unwrap_or_else(|| panic!("no stdout to parse as JSON; stderr was:\n{}", stderr(output)));

	serde_json::from_str(line).unwrap_or_else(|err| panic!("stdout is not JSON ({err}): {line}"))
}

/// Every NDJSON line a `--raw` listing prints on stdout
fn raw_lines(output: &Output) -> Vec<Value> {
	stdout(output)
		.lines()
		.filter(|line| !line.trim().is_empty())
		.map(|line| serde_json::from_str(line).unwrap_or_else(|err| panic!("stdout is not NDJSON ({err}): {line}")))
		.collect()
}

/// What the fake plugin does with one `request` frame
enum Answer {
	Value(Value),
	Failure(&'static str, &'static str),
	/// Never answers — the daemon's own remote-op deadline has to fire
	Silent,
}

type Responder = Arc<dyn Fn(&str, &Value) -> Answer + Send + Sync>;

/// Connects a WS client into the daemon's plugin slot and answers ops from
/// `responder` until the connection drops. Returns once the handshake is
/// complete, so the CLI can be launched immediately afterwards
fn spawn_plugin(daemon: &TestDaemon, name: &'static str, responder: Responder) {
	let url = daemon.ws_url();
	let (ready_tx, ready_rx) = std::sync::mpsc::channel();

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

						let response = match responder(&op, &frame["args"]) {
							Answer::Value(value) => json!({
								"type": "response",
								"request_id": frame["request_id"],
								"ok": true,
								"value": value,
							}),
							Answer::Failure(code, message) => json!({
								"type": "response",
								"request_id": frame["request_id"],
								"ok": false,
								"error": { "code": code, "message": message },
							}),
							Answer::Silent => continue,
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
}

/// Answers ops from a fixed `op -> value` table and fails anything else, so a
/// test never passes on an op it did not mean to exercise
fn table_responder(entries: Vec<(&'static str, Value)>) -> Responder {
	Arc::new(move |op, _args| match entries.iter().find(|(name, _)| *name == op) {
		Some((_, value)) => Answer::Value(value.clone()),
		None => Answer::Failure("UNKNOWN_OP", "the fake plugin does not implement this op"),
	})
}

/// `GET /hello` body for a stub daemon claiming to serve `project_file`, so
/// the CLI's identity check passes
fn stub_hello(project_file: &Path) -> String {
	json!({
		"name": "wsync-fixture",
		"version": "0.1.0",
		"protocol": 1,
		"project": project_file.to_string_lossy(),
		"canonicalProject": project_file.to_string_lossy(),
		"bootId": "stub-boot",
		"pid": 1,
		"port": 0,
		"managedBy": "test",
	})
	.to_string()
}

/// A minimal HTTP/1.1 responder for endpoints the real daemon build does not
/// serve yet. Routes are keyed `"<METHOD> <path>"`, ignoring the query string
fn spawn_stub(routes: Vec<(&'static str, u16, String)>) -> u16 {
	let listener = TcpListener::bind("127.0.0.1:0").unwrap();
	let port = listener.local_addr().unwrap().port();

	thread::spawn(move || {
		for stream in listener.incoming() {
			let Ok(stream) = stream else {
				continue;
			};

			serve_once(stream, &routes);
		}
	});

	port
}

fn serve_once(mut stream: TcpStream, routes: &[(&'static str, u16, String)]) {
	let mut reader = BufReader::new(stream.try_clone().unwrap());
	let mut request_line = String::new();

	if reader.read_line(&mut request_line).is_err() {
		return;
	}

	let mut content_length = 0usize;

	loop {
		let mut header = String::new();

		if reader.read_line(&mut header).is_err() || header.trim().is_empty() {
			break;
		}

		if let Some(value) = header.to_ascii_lowercase().strip_prefix("content-length:") {
			content_length = value.trim().parse().unwrap_or(0);
		}
	}

	if content_length > 0 {
		// Drain the body so the client sees a clean response, not a reset
		let mut body = vec![0; content_length];
		reader.read_exact(&mut body).ok();
	}

	let mut parts = request_line.split_whitespace();
	let method = parts.next().unwrap_or("GET").to_owned();
	let path = parts.next().unwrap_or("/").split('?').next().unwrap_or("/").to_owned();
	let route_key = format!("{method} {path}");

	// Unrouted paths mirror the real daemon, which redirects unknown routes
	// to `/` instead of answering 404
	let (status, body) = routes
		.iter()
		.find(|(route, _, _)| *route == route_key)
		.map(|(_, status, body)| (*status, body.clone()))
		.unwrap_or((307, String::new()));

	let reason = match status {
		200 => "OK",
		307 => "Temporary Redirect",
		409 => "Conflict",
		_ => "Not Found",
	};

	let response = format!(
		"HTTP/1.1 {status} {reason}\r\nLocation: /\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
		body.len()
	);

	stream.write_all(response.as_bytes()).ok();
	stream.flush().ok();
}

/// A port nothing listens on, for the checks that must fail before any
/// network access
fn dead_port() -> u16 {
	let listener = TcpListener::bind("127.0.0.1:0").unwrap();
	let port = listener.local_addr().unwrap().port();

	drop(listener);

	port
}

// ---------------------------------------------------------------------------
// Handshake and diagnostics
// ---------------------------------------------------------------------------

#[test]
fn ping_round_trips_through_the_plugin() {
	let daemon = start_daemon(None);
	spawn_plugin(
		&daemon,
		"ping-plugin",
		table_responder(vec![("ping", json!({ "pong": true }))]),
	);

	let sandbox = Sandbox::new();
	let project = daemon.root.to_string_lossy().into_owned();
	let port = daemon.port.to_string();

	let human = sandbox.run(&["ping", "--project", &project, "--port", &port]);

	assert!(human.status.success(), "ping failed: {}", stderr(&human));
	assert!(
		stderr(&human).contains("Studio plugin answered"),
		"unexpected output: {}",
		stderr(&human)
	);

	let raw = sandbox.run(&["ping", "--project", &project, "--port", &port, "--raw"]);

	assert!(raw.status.success(), "ping --raw failed: {}", stderr(&raw));

	let value = raw_json(&raw);

	assert_eq!(value["ok"], true);
	assert_eq!(value["pong"], true);
	assert_eq!(value["port"], daemon.port);
	assert_eq!(value["protocol"], 1);
	assert!(value["roundTripMs"].is_u64());
}

#[test]
fn status_raw_reports_project_daemon_and_plugin() {
	let daemon = start_daemon(None);
	spawn_plugin(
		&daemon,
		"status-plugin",
		table_responder(vec![(
			"version",
			json!({ "pluginVersion": "9.9.9", "protocol": 1, "studioVersion": "0.600" }),
		)]),
	);

	let sandbox = Sandbox::new();
	let output = sandbox.run(&[
		"status",
		"--project",
		&daemon.root.to_string_lossy(),
		"--port",
		&daemon.port.to_string(),
		"--raw",
	]);

	assert!(output.status.success(), "status failed: {}", stderr(&output));

	let value = raw_json(&output);

	assert_eq!(value["ok"], true);
	assert_eq!(value["project"]["exists"], true);
	assert_eq!(value["project"]["parses"], true);
	assert_eq!(value["project"]["name"], "wsync-fixture");
	assert_eq!(value["daemon"]["reachable"], true);
	assert_eq!(value["daemon"]["port"], daemon.port);
	assert_eq!(value["daemon"]["portSource"], "--port");
	assert_eq!(value["plugin"]["connected"], true);
	assert_eq!(value["plugin"]["version"], "9.9.9");
	// The daemon reports its audit-log location through `/hello.writesLog`,
	// so `status` no longer has to guess: the surface is daemon-sourced
	assert_eq!(value["writeLog"]["pending"], false);
	assert_eq!(value["writeLog"]["source"], "daemon");
	assert!(
		value["writeLog"]["path"]
			.as_str()
			.is_some_and(|path| path.ends_with("writes.log")),
		"writeLog.path should be the daemon-reported writes.log"
	);
}

#[test]
fn status_still_answers_when_no_daemon_is_running() {
	let dir = scratch_project();
	let root = dir.path().canonicalize().unwrap();
	let sandbox = Sandbox::new();

	let output = sandbox.run(&[
		"status",
		"--project",
		&root.to_string_lossy(),
		"--port",
		&dead_port().to_string(),
		"--raw",
	]);

	// `status` is the command an agent runs *because* something may be
	// broken: it always completes
	assert!(output.status.success(), "status failed: {}", stderr(&output));

	let value = raw_json(&output);

	assert_eq!(value["ok"], true);
	assert_eq!(value["daemon"]["reachable"], false);
	assert_eq!(value["plugin"]["connected"], false);
	assert_eq!(value["project"]["parses"], true);
}

#[test]
fn version_reports_the_daemon_even_without_a_plugin() {
	let daemon = start_daemon(None);
	let sandbox = Sandbox::new();

	let output = sandbox.run(&[
		"version",
		"--project",
		&daemon.root.to_string_lossy(),
		"--port",
		&daemon.port.to_string(),
		"--raw",
	]);

	assert!(output.status.success(), "version failed: {}", stderr(&output));

	let value = raw_json(&output);

	assert_eq!(value["ok"], true);
	assert_eq!(value["daemon"]["protocol"], 1);
	assert_eq!(value["plugin"]["connected"], false);
	assert_eq!(value["plugin"]["error"]["code"], "PLUGIN_ERROR");
}

// ---------------------------------------------------------------------------
// Inspection
// ---------------------------------------------------------------------------

#[test]
fn get_ls_and_tree_render_plugin_values() {
	let daemon = start_daemon(None);

	spawn_plugin(
		&daemon,
		"inspect-plugin",
		table_responder(vec![
			(
				"get",
				json!({
					"path": "Workspace/Baseplate",
					"class": "Part",
					"name": "Baseplate",
					"properties": { "Anchored": true },
					"childrenCount": 0,
				}),
			),
			(
				"ls",
				json!({
					"path": "ReplicatedStorage",
					"class": "ReplicatedStorage",
					"children": [{ "name": "Shared", "class": "Folder" }],
					"count": 1,
					"total": 1,
					"truncated": false,
				}),
			),
			(
				"tree",
				json!({
					"root": {
						"name": "Workspace",
						"class": "Workspace",
						"children": [{ "name": "Baseplate", "class": "Part" }],
					},
					"depth": 3,
					"visitedNodes": 2,
					"truncated": false,
				}),
			),
		]),
	);

	let sandbox = Sandbox::new();
	let project = daemon.root.to_string_lossy().into_owned();
	let port = daemon.port.to_string();

	let get = sandbox.run(&[
		"get",
		"--project",
		&project,
		"--port",
		&port,
		"--path",
		"Workspace/Baseplate",
	]);

	assert!(get.status.success(), "get failed: {}", stderr(&get));
	assert!(stdout(&get).contains("Part"), "unexpected get output: {}", stdout(&get));
	assert!(stdout(&get).contains("Anchored"));

	let ls = sandbox.run(&[
		"ls",
		"--project",
		&project,
		"--port",
		&port,
		"--path",
		"ReplicatedStorage",
	]);

	assert!(ls.status.success(), "ls failed: {}", stderr(&ls));
	assert!(stdout(&ls).contains("Shared"), "unexpected ls output: {}", stdout(&ls));

	let tree = sandbox.run(&[
		"tree",
		"--project",
		&project,
		"--port",
		&port,
		"--path",
		"Workspace",
		"--raw",
	]);

	assert!(tree.status.success(), "tree failed: {}", stderr(&tree));

	// `--raw` on a single-op command is that op's value, verbatim
	let value = raw_json(&tree);

	assert_eq!(value["root"]["name"], "Workspace");
	assert_eq!(value["root"]["children"][0]["class"], "Part");
	assert_eq!(value["visitedNodes"], 2);
}

#[test]
fn get_with_a_prop_prints_only_the_value() {
	let daemon = start_daemon(None);
	spawn_plugin(
		&daemon,
		"prop-plugin",
		table_responder(vec![("get", json!("Baseplate"))]),
	);

	let sandbox = Sandbox::new();
	let output = sandbox.run(&[
		"get",
		"--project",
		&daemon.root.to_string_lossy(),
		"--port",
		&daemon.port.to_string(),
		"--path",
		"Workspace/Baseplate",
		"--prop",
		"Name",
	]);

	assert!(output.status.success(), "get --prop failed: {}", stderr(&output));
	assert_eq!(stdout(&output).trim(), "Baseplate");
}

#[test]
fn query_limit_is_validated_before_any_request() {
	let sandbox = Sandbox::new();
	let dir = scratch_project();
	let root = dir.path().canonicalize().unwrap();
	// Nothing listens here: a limit that reaches the network would fail with
	// a connection error instead of the range message
	let port = dead_port().to_string();

	for limit in ["0", "10001"] {
		let output = sandbox.run(&[
			"query",
			"--project",
			&root.to_string_lossy(),
			"--port",
			&port,
			"Workspace/**",
			"--limit",
			limit,
		]);

		assert!(!output.status.success(), "--limit {limit} was accepted");

		let message = stderr(&output);

		assert!(
			message.contains("1..=10000") || message.contains("not in 1..=10000"),
			"--limit {limit} did not report the documented range: {message}"
		);
		assert!(
			!message.contains("No WSync daemon answers"),
			"--limit {limit} reached the network before validation: {message}"
		);
	}

	// The documented bounds themselves are accepted (and then fail on the
	// network, proving validation is not simply rejecting everything)
	let accepted = sandbox.run(&[
		"query",
		"--project",
		&root.to_string_lossy(),
		"--port",
		&port,
		"Workspace/**",
		"--limit",
		"10000",
	]);

	assert!(
		stderr(&accepted).contains("No WSync daemon answers"),
		"--limit 10000 should have been accepted: {}",
		stderr(&accepted)
	);
}

#[test]
fn find_reads_the_matches_array() {
	let daemon = start_daemon(None);

	spawn_plugin(
		&daemon,
		"find-plugin",
		table_responder(vec![(
			"find",
			json!({
				"matches": [{ "path": "ReplicatedStorage/Remotes/Hit", "class": "RemoteEvent", "name": "Hit" }],
				"count": 1,
				"truncated": false,
				"truncationReason": null,
			}),
		)]),
	);

	let sandbox = Sandbox::new();
	let output = sandbox.run(&[
		"find",
		"--project",
		&daemon.root.to_string_lossy(),
		"--port",
		&daemon.port.to_string(),
		"--class",
		"RemoteEvent",
	]);

	assert!(output.status.success(), "find failed: {}", stderr(&output));
	assert!(
		stdout(&output).contains("ReplicatedStorage/Remotes/Hit"),
		"find did not read .matches: {}",
		stdout(&output)
	);
	assert!(stdout(&output).contains("1 match(es)"));
}

#[test]
fn find_refuses_a_bare_array_response() {
	let daemon = start_daemon(None);

	// A plugin that answers with a bare array instead of the documented
	// `{matches,…}` envelope must fail loudly, not print nothing
	spawn_plugin(
		&daemon,
		"bare-find-plugin",
		table_responder(vec![("find", json!([{ "path": "Workspace/Part", "class": "Part" }]))]),
	);

	let sandbox = Sandbox::new();
	let output = sandbox.run(&[
		"find",
		"--project",
		&daemon.root.to_string_lossy(),
		"--port",
		&daemon.port.to_string(),
		"--class",
		"Part",
	]);

	assert!(!output.status.success(), "a bare array was accepted");
	assert!(
		stderr(&output).contains("matches"),
		"unexpected failure message: {}",
		stderr(&output)
	);
}

#[test]
fn select_set_validates_paths_before_the_request() {
	let sandbox = Sandbox::new();
	let dir = scratch_project();
	let root = dir.path().canonicalize().unwrap();

	let output = sandbox.run(&[
		"select",
		"set",
		"--project",
		&root.to_string_lossy(),
		"--port",
		&dead_port().to_string(),
		"--paths",
		"Workspace/Box",
	]);

	assert!(!output.status.success(), "a non-JSON --paths was accepted");
	assert!(
		stderr(&output).contains("--paths must be a JSON array"),
		"unexpected failure message: {}",
		stderr(&output)
	);
}

// ---------------------------------------------------------------------------
// Failure paths
// ---------------------------------------------------------------------------

#[test]
fn a_missing_plugin_fails_the_command() {
	let daemon = start_daemon(None);
	let sandbox = Sandbox::new();

	let output = sandbox.run(&[
		"get",
		"--project",
		&daemon.root.to_string_lossy(),
		"--port",
		&daemon.port.to_string(),
		"--path",
		"Workspace",
	]);

	assert!(!output.status.success(), "get succeeded without a plugin");

	let message = stderr(&output);

	assert!(message.contains("PLUGIN_ERROR"), "unexpected message: {message}");
	assert!(
		message.contains("no Studio plugin connected"),
		"unexpected message: {message}"
	);
}

#[test]
fn a_missing_plugin_still_prints_a_raw_envelope() {
	let daemon = start_daemon(None);
	let sandbox = Sandbox::new();

	let output = sandbox.run(&[
		"ls",
		"--project",
		&daemon.root.to_string_lossy(),
		"--port",
		&daemon.port.to_string(),
		"--raw",
	]);

	assert!(!output.status.success());

	// A machine caller gets one parseable line even on failure
	let value = raw_json(&output);

	assert_eq!(value["ok"], false);
	assert_eq!(value["error"]["code"], "PLUGIN_ERROR");
	assert_eq!(value["meta"]["op"], "ls");
}

#[test]
fn a_silent_plugin_trips_the_five_second_remote_timeout() {
	let daemon = start_daemon(None);

	spawn_plugin(
		&daemon,
		"silent-plugin",
		Arc::new(|op, _args| match op {
			// Everything else still answers, so the timeout is provably the
			// op's own deadline and not a dead connection
			"tree" => Answer::Silent,
			_ => Answer::Value(json!({ "pong": true })),
		}),
	);

	let sandbox = Sandbox::new();
	let project = daemon.root.to_string_lossy().into_owned();
	let port = daemon.port.to_string();

	let ping = sandbox.run(&["ping", "--project", &project, "--port", &port]);

	assert!(ping.status.success(), "the plugin is not answering at all");

	let started = Instant::now();
	let output = sandbox.run(&["tree", "--project", &project, "--port", &port]);
	let elapsed = started.elapsed();

	assert!(!output.status.success(), "a silent plugin did not fail the command");
	assert!(
		stderr(&output).contains("TIMEOUT"),
		"unexpected failure message: {}",
		stderr(&output)
	);
	assert!(
		elapsed >= Duration::from_secs(5),
		"the remote deadline fired early ({elapsed:?}) — the 5 s default is not being sent"
	);
}

// ---------------------------------------------------------------------------
// Conflict / decision surface
// ---------------------------------------------------------------------------

#[test]
fn decision_reports_when_no_review_is_pending() {
	let dir = scratch_project();
	let root = dir.path().canonicalize().unwrap();
	let project_file = root.join("default.project.json");

	let port = spawn_stub(vec![
		("GET /hello", 200, stub_hello(&project_file)),
		("GET /review", 200, json!({ "pending": false }).to_string()),
		("GET /choice", 200, json!({ "pending": false }).to_string()),
	]);

	let sandbox = Sandbox::new();
	let project = root.to_string_lossy().into_owned();
	let port = port.to_string();

	let human = sandbox.run(&["decision", "--project", &project, "--port", &port]);

	assert!(human.status.success(), "decision failed: {}", stderr(&human));
	assert!(
		stderr(&human).contains("No pending disk review"),
		"unexpected output: {}",
		stderr(&human)
	);

	let raw = sandbox.run(&["decision", "--project", &project, "--port", &port, "--raw"]);

	assert!(raw.status.success(), "decision --raw failed: {}", stderr(&raw));
	assert_eq!(raw_json(&raw), json!({ "ok": true, "pending": false }));
}

#[test]
fn decision_points_full_scope_projects_at_the_choice_surface() {
	let dir = scratch_project();
	let root = dir.path().canonicalize().unwrap();
	let project_file = root.join("default.project.json");

	// A full-scope daemon has no review but may hold a pending choice; the
	// human output must say where that choice is answered
	let port = spawn_stub(vec![
		("GET /hello", 200, stub_hello(&project_file)),
		("GET /review", 200, json!({ "pending": false }).to_string()),
		(
			"GET /choice",
			200,
			json!({ "pending": true, "choiceId": "choice-9", "stats": { "total": 1 } }).to_string(),
		),
	]);

	let sandbox = Sandbox::new();
	let output = sandbox.run(&[
		"decision",
		"--project",
		&root.to_string_lossy(),
		"--port",
		&port.to_string(),
	]);

	assert!(output.status.success(), "decision failed: {}", stderr(&output));
	assert!(
		stderr(&output).contains("full-scope divergence choice") && stderr(&output).contains("choice-9"),
		"unexpected output: {}",
		stderr(&output)
	);
}

#[test]
fn decision_reports_a_pending_review_and_pushes_it_whole() {
	let dir = scratch_project();
	let root = dir.path().canonicalize().unwrap();
	let project_file = root.join("default.project.json");

	let port = spawn_stub(vec![
		("GET /hello", 200, stub_hello(&project_file)),
		(
			"GET /review",
			200,
			json!({
				"pending": true,
				"reviewId": "review-1",
				"stats": { "total": 3, "diskOnly": 2, "differs": 1 },
			})
			.to_string(),
		),
		(
			"POST /review/push",
			200,
			json!({ "ok": true, "pushed": 3, "remaining": 0 }).to_string(),
		),
	]);

	let sandbox = Sandbox::new();
	let project = root.to_string_lossy().into_owned();
	let port = port.to_string();

	let listing = sandbox.run(&["decision", "--project", &project, "--port", &port, "--raw"]);

	assert!(listing.status.success(), "decision failed: {}", stderr(&listing));

	let value = raw_json(&listing);

	assert_eq!(value["pending"], true);
	assert_eq!(value["reviewId"], "review-1");
	assert_eq!(value["stats"]["diskOnly"], 2);

	// --disk pushes the whole review back to Studio
	let submit = sandbox.run(&["decision", "--project", &project, "--port", &port, "--disk", "--raw"]);

	assert!(submit.status.success(), "push failed: {}", stderr(&submit));
	assert_eq!(
		raw_json(&submit),
		json!({ "ok": true, "reviewId": "review-1", "pushed": 3, "remaining": 0 })
	);

	// --studio is informational: Studio already won at connect (exit 0)
	let studio = sandbox.run(&["decision", "--project", &project, "--port", &port, "--studio"]);

	assert!(studio.status.success(), "--studio must exit 0: {}", stderr(&studio));
	assert!(
		stderr(&studio).contains("Studio already won"),
		"unexpected output: {}",
		stderr(&studio)
	);
}

#[test]
fn decision_fails_when_the_review_is_stale() {
	let dir = scratch_project();
	let root = dir.path().canonicalize().unwrap();
	let project_file = root.join("default.project.json");

	let port = spawn_stub(vec![
		("GET /hello", 200, stub_hello(&project_file)),
		("GET /review", 200, json!({ "pending": false }).to_string()),
		(
			"POST /review/push",
			404,
			json!({ "ok": false, "error": "Unknown or stale reviewId" }).to_string(),
		),
	]);

	let sandbox = Sandbox::new();

	// `--choice-id` stays accepted as the alias of `--review-id`
	let output = sandbox.run(&[
		"decision",
		"--project",
		&root.to_string_lossy(),
		"--port",
		&port.to_string(),
		"--choice-id",
		"review-1",
		"--disk",
	]);

	// The submitted push did not take effect, so the exit code says so
	assert!(!output.status.success(), "a 404 was reported as success");
	assert!(
		stderr(&output).contains("stale or already handled"),
		"unexpected message: {}",
		stderr(&output)
	);
}

#[test]
fn decision_dismisses_the_review() {
	let dir = scratch_project();
	let root = dir.path().canonicalize().unwrap();
	let project_file = root.join("default.project.json");

	let port = spawn_stub(vec![
		("GET /hello", 200, stub_hello(&project_file)),
		(
			"GET /review",
			200,
			json!({
				"pending": true,
				"reviewId": "review-2",
				"stats": { "total": 1, "diskOnly": 0, "differs": 1 },
			})
			.to_string(),
		),
		("POST /review/dismiss", 200, json!({ "ok": true }).to_string()),
	]);

	let sandbox = Sandbox::new();
	let output = sandbox.run(&[
		"decision",
		"--project",
		&root.to_string_lossy(),
		"--port",
		&port.to_string(),
		"--cancel",
		"--raw",
	]);

	assert!(output.status.success(), "dismiss failed: {}", stderr(&output));
	assert_eq!(
		raw_json(&output),
		json!({ "ok": true, "reviewId": "review-2", "dismissed": true })
	);
}

#[test]
fn conflicts_lists_an_empty_parked_set() {
	let daemon = start_daemon(None);
	let sandbox = Sandbox::new();
	let project = daemon.root.to_string_lossy().into_owned();
	let port = daemon.port.to_string();

	let human = sandbox.run(&["conflicts", "--project", &project, "--port", &port]);

	assert!(human.status.success(), "conflicts failed: {}", stderr(&human));
	assert!(
		stderr(&human).contains("No parked conflicts"),
		"unexpected output: {}",
		stderr(&human)
	);

	let raw = sandbox.run(&["conflicts", "--project", &project, "--port", &port, "--raw"]);

	assert!(raw.status.success(), "conflicts --raw failed: {}", stderr(&raw));
	assert_eq!(raw_json(&raw), json!({ "ok": true, "count": 0, "conflicts": [] }));
}

#[test]
fn a_daemon_without_the_conflict_surface_is_reported_honestly() {
	let dir = scratch_project();
	let root = dir.path().canonicalize().unwrap();
	let project_file = root.join("default.project.json");

	// A build that predates the conflict engine redirects unknown routes to
	// `/`; the command must say so instead of rendering the daemon's home
	// page as conflict data
	let port = spawn_stub(vec![("GET /hello", 200, stub_hello(&project_file))]);

	let sandbox = Sandbox::new();
	let output = sandbox.run(&[
		"conflicts",
		"--project",
		&root.to_string_lossy(),
		"--port",
		&port.to_string(),
	]);

	assert!(!output.status.success(), "a missing endpoint was reported as success");
	assert!(
		stderr(&output).contains("does not serve /resolve"),
		"unexpected message: {}",
		stderr(&output)
	);
}

#[test]
fn diff_explains_that_the_comparison_runs_on_plugin_connect() {
	let daemon = start_daemon(None);
	let sandbox = Sandbox::new();

	let output = sandbox.run(&[
		"diff",
		"--project",
		&daemon.root.to_string_lossy(),
		"--port",
		&daemon.port.to_string(),
	]);

	// No plugin has connected, so nothing has been applied and no review is
	// pending — that is a reportable state, not a failure
	assert!(output.status.success(), "diff failed: {}", stderr(&output));
	assert!(
		stderr(&output).contains("No pending disk review"),
		"unexpected output: {}",
		stderr(&output)
	);
}

#[test]
fn diff_lists_the_pending_review_with_markers() {
	let dir = scratch_project();
	let root = dir.path().canonicalize().unwrap();
	let project_file = root.join("default.project.json");

	let port = spawn_stub(vec![
		("GET /hello", 200, stub_hello(&project_file)),
		(
			"GET /review",
			200,
			json!({
				"pending": true,
				"reviewId": "review-3",
				"stats": { "total": 2, "diskOnly": 1, "differs": 1 },
			})
			.to_string(),
		),
		(
			"GET /review/details",
			200,
			json!({
				"reviewId": "review-3",
				"totalCount": 2,
				"items": [
					{ "id": 0, "path": "src/KeepMe.luau", "instancePath": "ReplicatedStorage/KeepMe", "state": "disk-only", "class": "ModuleScript" },
					{ "id": 1, "path": "src/Hello.luau", "instancePath": null, "state": "differs", "class": "ModuleScript" },
				],
			})
			.to_string(),
		),
	]);

	let sandbox = Sandbox::new();
	let project = root.to_string_lossy().into_owned();
	let port = port.to_string();

	let human = sandbox.run(&["diff", "--project", &project, "--port", &port]);

	assert!(human.status.success(), "diff failed: {}", stderr(&human));

	let listing = stdout(&human);

	assert!(listing.contains("review-3"), "unexpected listing: {listing}");
	assert!(
		listing.contains("+ ModuleScript") && listing.contains("~ ModuleScript"),
		"markers missing: {listing}"
	);

	// NDJSON entries carry the reviewId they belong to
	let raw = sandbox.run(&["diff", "--project", &project, "--port", &port, "--raw"]);

	assert!(raw.status.success(), "diff --raw failed: {}", stderr(&raw));

	let lines: Vec<Value> = raw_lines(&raw);

	assert_eq!(lines.len(), 2);
	assert_eq!(lines[0]["reviewId"], "review-3");
	assert_eq!(lines[0]["state"], "disk-only");
	assert_eq!(lines[1]["state"], "differs");
}

#[test]
fn resolve_requires_exactly_one_side() {
	let sandbox = Sandbox::new();
	let dir = scratch_project();
	let root = dir.path().canonicalize().unwrap();

	let output = sandbox.run(&[
		"resolve",
		"--project",
		&root.to_string_lossy(),
		"--port",
		&dead_port().to_string(),
		"--path",
		"src/Hello.luau",
	]);

	assert!(!output.status.success(), "resolve ran without a side");
	assert!(
		stderr(&output).contains("exactly one of --disk"),
		"unexpected message: {}",
		stderr(&output)
	);
}

// ---------------------------------------------------------------------------
// source --disk (no daemon involved)
// ---------------------------------------------------------------------------

#[test]
fn source_disk_reads_through_the_middleware_projection() {
	let dir = scratch_project();
	let root = dir.path().canonicalize().unwrap();
	let sandbox = Sandbox::new();

	let output = sandbox.run(&[
		"source",
		"--project",
		&root.to_string_lossy(),
		"--path",
		"ReplicatedStorage/Hello",
		"--disk",
	]);

	assert!(output.status.success(), "source --disk failed: {}", stderr(&output));
	assert!(
		stdout(&output).contains("return \"hello\""),
		"unexpected source: {}",
		stdout(&output)
	);

	let raw = sandbox.run(&[
		"source",
		"--project",
		&root.to_string_lossy(),
		"--path",
		"ReplicatedStorage/Hello",
		"--disk",
		"--raw",
	]);

	assert!(raw.status.success(), "source --disk --raw failed: {}", stderr(&raw));

	let value = raw_json(&raw);

	assert_eq!(value["ok"], true);
	assert_eq!(value["from"], "disk");
	assert!(value["file"].as_str().unwrap().ends_with("Hello.luau"));
}

#[test]
fn source_disk_explains_an_instance_outside_the_projection() {
	let dir = scratch_project();
	let root = dir.path().canonicalize().unwrap();
	let sandbox = Sandbox::new();

	let output = sandbox.run(&[
		"source",
		"--project",
		&root.to_string_lossy(),
		"--path",
		"Workspace/Baseplate",
		"--disk",
	]);

	assert!(!output.status.success(), "an unprojected instance was resolved");
	assert!(
		stderr(&output).contains("not projected to disk"),
		"unexpected message: {}",
		stderr(&output)
	);
}
