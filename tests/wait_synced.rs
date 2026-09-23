//! `wsync wait-synced` and the pending-outbound counts in `status`
//! (wait-synced.json): the daemon-side barrier is exercised over its HTTP
//! route with a recording fake plugin (ordering), and the CLI end to end
//! (exit codes, `--raw`, `status`).

mod common;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::{
	collections::HashMap,
	fs,
	sync::Arc,
	time::{Duration, Instant},
};
use tokio_tungstenite::tungstenite::Message;

use common::{
	cli_json, cli_stderr, connect_client, get_json, post_json, scratch_project, spawn_cli_plugin, spawn_fake_plugin,
	start_daemon, CliAnswer, CliSandbox, FakePluginScript, TestDaemon,
};

/// The answer a current plugin gives once everything before it applied
fn barrier_answer() -> Value {
	json!({
		"applied": true,
		"settled": true,
		"sourceWritesAwaited": 0,
		"pendingSourceWrites": 0,
		"waitedMs": 0,
	})
}

fn barrier_script() -> FakePluginScript {
	FakePluginScript {
		canned: HashMap::from([(
			"sync_barrier".to_owned(),
			json!({ "ok": true, "value": barrier_answer() }),
		)]),
		..Default::default()
	}
}

async fn pending(daemon: &TestDaemon) -> Value {
	let (status, reply) = get_json(daemon, "/sync/pending").await;

	assert_eq!(status, 200);
	assert_eq!(reply["ok"], true, "{reply}");

	reply["pending"].clone()
}

#[tokio::test]
async fn the_barrier_is_answered_only_after_the_sync_frames_of_earlier_disk_edits() {
	let daemon = start_daemon(None);
	let plugin = spawn_fake_plugin(&daemon, "barrier-plugin", barrier_script()).await;

	tokio::time::sleep(Duration::from_millis(300)).await;

	// Edits the watcher's debouncer is still holding when the barrier starts:
	// a plain request sent now would overtake them
	fs::write(
		daemon.src_dir().join("Hello.luau"),
		"return \"edited before the barrier\"\n",
	)
	.unwrap();
	fs::write(
		daemon.src_dir().join("Added.luau"),
		"return \"added before the barrier\"\n",
	)
	.unwrap();

	let (status, reply) = post_json(&daemon, "/sync/wait", &json!({ "timeoutMs": 15000 })).await;

	assert_eq!(status, 200);
	assert_eq!(reply["ok"], true, "the barrier failed: {reply}");
	assert_eq!(reply["synced"], true);
	assert!(
		reply["flushedChanges"].as_u64().unwrap() >= 2,
		"both edits should be confirmed: {reply}"
	);
	assert!(
		reply["stages"]["watcherMs"].as_u64().unwrap() >= 100,
		"the watcher stage must outlast the debounce window: {reply}"
	);

	// Ordering on the wire: every change made before the barrier arrived in a
	// sync frame ahead of the barrier request
	let frames = plugin.frames.lock().unwrap().clone();
	let barrier = frames
		.iter()
		.position(|frame| frame["type"] == "request" && frame["op"] == "sync_barrier")
		.expect("the plugin never received sync_barrier");
	let before: String = frames[..barrier]
		.iter()
		.filter(|frame| frame["type"] == "sync")
		.map(Value::to_string)
		.collect();

	assert!(
		before.contains("edited before the barrier"),
		"the update must precede the barrier: {frames:?}"
	);
	assert!(
		before.contains("added before the barrier"),
		"the addition must precede the barrier: {frames:?}"
	);

	// The barrier confirmed everything it flushed
	let pending = pending(&daemon).await;

	assert_eq!(pending["plugin"]["tracked"], true);
	assert_eq!(pending["unconfirmedChanges"], 0, "{pending}");
	assert_eq!(pending["queuedChanges"], 0, "{pending}");

	// A second barrier with nothing new flushes nothing
	let (_, reply) = post_json(&daemon, "/sync/wait", &json!({ "timeoutMs": 15000 })).await;

	assert_eq!(reply["ok"], true, "{reply}");
	assert_eq!(reply["flushedChanges"], 0, "{reply}");
}

#[tokio::test]
async fn no_plugin_fails_fast_instead_of_waiting_out_the_timeout() {
	let daemon = start_daemon(None);
	let started = Instant::now();

	let (status, reply) = post_json(&daemon, "/sync/wait", &json!({ "timeoutMs": 20000 })).await;

	assert_eq!(status, 200);
	assert_eq!(reply["ok"], false);
	assert_eq!(reply["synced"], false);
	assert_eq!(reply["stage"], "plugin");
	assert_eq!(reply["error"]["code"], "PLUGIN_ERROR");
	assert!(
		reply["error"]["message"].as_str().unwrap().contains("no Studio plugin"),
		"{reply}"
	);
	assert!(
		started.elapsed() < Duration::from_secs(5),
		"a missing plugin must not wait"
	);

	let pending = pending(&daemon).await;

	assert_eq!(pending["plugin"]["connected"], false);
	assert_eq!(pending["total"], 0);
}

#[tokio::test]
async fn a_plugin_that_never_answers_times_out_in_the_studio_stage() {
	let daemon = start_daemon(None);
	let (mut socket, _) = connect_client(&daemon, "plugin", "silent-plugin").await;

	// Keeps the connection alive (pongs) but never answers a request
	let silent = tokio::spawn(async move {
		while let Some(Ok(message)) = socket.next().await {
			if let Message::Text(text) = message {
				if text.contains("\"ping\"") {
					socket.send(Message::Text(r#"{"type":"pong"}"#.into())).await.ok();
				}
			}
		}
	});

	let started = Instant::now();
	let (_, reply) = post_json(&daemon, "/sync/wait", &json!({ "timeoutMs": 1500 })).await;

	assert_eq!(reply["ok"], false, "{reply}");
	assert_eq!(reply["stage"], "studio", "{reply}");
	assert_eq!(reply["error"]["code"], "TIMEOUT", "{reply}");
	assert!(
		started.elapsed() < Duration::from_secs(5),
		"the deadline must bound the whole barrier"
	);

	silent.abort();
}

#[tokio::test]
async fn an_outdated_plugin_is_told_to_update() {
	let daemon = start_daemon(None);
	// The default fake plugin answers UNKNOWN_OP for ops it does not serve
	let _plugin = spawn_fake_plugin(&daemon, "old-plugin", FakePluginScript::default()).await;

	let (_, reply) = post_json(&daemon, "/sync/wait", &json!({ "timeoutMs": 5000 })).await;

	assert_eq!(reply["ok"], false);
	assert_eq!(reply["error"]["code"], "UNKNOWN_OP");
	assert!(reply["error"]["message"]
		.as_str()
		.unwrap()
		.contains("update the plugin"));
}

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

fn cli_plugin(daemon: &TestDaemon) {
	spawn_cli_plugin(
		daemon,
		"wait-plugin",
		Arc::new(|op, _args| match op {
			"sync_barrier" => CliAnswer::Value(barrier_answer()),
			"version" => CliAnswer::Value(json!({ "pluginVersion": "9.9.9", "protocol": 1 })),
			_ => CliAnswer::Failure("UNKNOWN_OP", "not served by this fake"),
		}),
	);
}

#[test]
fn wait_synced_and_status_report_the_same_pending_changes() {
	let daemon = start_daemon(None);
	cli_plugin(&daemon);

	let sandbox = CliSandbox::new();
	let project = daemon.root.to_string_lossy().into_owned();
	let port = daemon.port.to_string();
	let status = || sandbox.run(&["status", "--project", &project, "--port", &port, "--raw"]);

	std::thread::sleep(Duration::from_millis(300));
	fs::write(daemon.src_dir().join("Hello.luau"), "return \"pending\"\n").unwrap();

	// Once the change is on the plugin connection it is sent-but-unconfirmed
	let deadline = Instant::now() + Duration::from_secs(10);

	let outbound = loop {
		let value = cli_json(&status());
		let outbound = value["outbound"].clone();

		if outbound["unconfirmedChanges"].as_u64().unwrap_or(0) >= 1 {
			break outbound;
		}

		assert!(Instant::now() < deadline, "the edit never reached the plugin: {value}");
		std::thread::sleep(Duration::from_millis(100));
	};

	assert_eq!(outbound["plugin"]["tracked"], true);
	assert!(outbound["total"].as_u64().unwrap() >= 1);

	let human = sandbox.run(&["status", "--project", &project, "--port", &port]);
	assert!(
		common::cli_stdout(&human).contains("pending change(s)"),
		"status should print the outbound line: {}",
		common::cli_stdout(&human)
	);

	let raw = sandbox.run(&["wait-synced", "--project", &project, "--port", &port, "--raw"]);

	assert!(raw.status.success(), "wait-synced failed: {}", cli_stderr(&raw));

	let reply = cli_json(&raw);

	assert_eq!(reply["ok"], true);
	assert!(reply["flushedChanges"].as_u64().unwrap() >= 1, "{reply}");

	let value = cli_json(&status());

	assert_eq!(
		value["outbound"]["total"], 0,
		"confirmed changes are no longer pending: {value}"
	);

	let human = sandbox.run(&["wait-synced", "--project", &project, "--port", &port]);

	assert!(human.status.success(), "wait-synced failed: {}", cli_stderr(&human));
	assert!(cli_stderr(&human).contains("Synced"), "{}", cli_stderr(&human));
}

#[test]
fn wait_synced_fails_clearly_without_a_plugin_or_a_daemon() {
	let daemon = start_daemon(None);
	let sandbox = CliSandbox::new();
	let project = daemon.root.to_string_lossy().into_owned();

	let output = sandbox.run(&[
		"wait-synced",
		"--project",
		&project,
		"--port",
		&daemon.port.to_string(),
		"--timeout",
		"2",
	]);

	assert!(!output.status.success(), "no plugin must not count as synced");
	assert!(
		cli_stderr(&output).contains("no Studio plugin connected"),
		"{}",
		cli_stderr(&output)
	);

	// --raw still prints one parseable line before the non-zero exit
	let raw = sandbox.run(&[
		"wait-synced",
		"--project",
		&project,
		"--port",
		&daemon.port.to_string(),
		"--raw",
	]);

	assert!(!raw.status.success());
	assert_eq!(cli_json(&raw)["ok"], false);

	// No daemon at all
	let dir = scratch_project();
	let root = dir.path().canonicalize().unwrap();
	let dead = std::net::TcpListener::bind("127.0.0.1:0")
		.unwrap()
		.local_addr()
		.unwrap()
		.port();

	let output = sandbox.run(&[
		"wait-synced",
		"--project",
		&root.to_string_lossy(),
		"--port",
		&dead.to_string(),
	]);

	assert!(!output.status.success());
	assert!(
		cli_stderr(&output).contains("No WSync daemon answers"),
		"{}",
		cli_stderr(&output)
	);

	let zero = sandbox.run(&["wait-synced", "--project", &project, "--timeout", "0"]);

	assert!(!zero.status.success());
	assert!(cli_stderr(&zero).contains("--timeout must be at least 1"));
}
