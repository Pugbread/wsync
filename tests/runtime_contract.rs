//! Regression tests for offline command execution, project targeting and
//! uncertain completion. All Studio work uses a fake WebSocket plugin.
mod common;

use common::{connect_client, scratch_project, start_daemon_with_state_dir, wait_for_frame};
use futures_util::SinkExt;
use serde_json::{json, Value};
use std::{
	io::{BufRead, BufReader, Read, Write},
	net::TcpListener,
	process::{Command, Output},
	sync::{
		atomic::{AtomicUsize, Ordering},
		Arc,
	},
	thread,
	time::Duration,
};
use tempfile::TempDir;
use tokio_tungstenite::tungstenite::Message;

fn command(home: &TempDir, args: &[&str]) -> Output {
	Command::new(env!("CARGO_BIN_EXE_wsync"))
		.args(args)
		.current_dir(home.path())
		.env("HOME", home.path())
		.env("USERPROFILE", home.path())
		.env("WSYNC_STATE_DIR", home.path().join("state"))
		.env("NO_COLOR", "1")
		.env("HTTP_PROXY", "http://127.0.0.1:1")
		.env("HTTPS_PROXY", "http://127.0.0.1:1")
		.env("ALL_PROXY", "http://127.0.0.1:1")
		.env_remove("NO_PROXY")
		.env_remove("no_proxy")
		.env_remove("RUST_LOG")
		.env_remove("RUST_VERBOSE")
		.output()
		.unwrap()
}

#[test]
fn help_schema_and_registry_are_offline_and_do_not_install_or_track() {
	let home = TempDir::new().unwrap();
	for args in [
		vec!["--help"],
		vec!["--version"],
		vec!["commands", "set"],
		vec!["commands"],
		vec!["commands", "types", "Vector3"],
	] {
		let output = command(&home, &args);
		assert!(
			output.status.success(),
			"{args:?}: {}",
			String::from_utf8_lossy(&output.stderr)
		);
		assert!(
			!home.path().join(".wsync").exists(),
			"{args:?} created installation or stats state"
		);
	}
	let output = command(&home, &["set", "--not-an-option"]);
	assert!(!output.status.success());
	assert!(
		!home.path().join(".wsync").exists(),
		"usage errors must also avoid installation"
	);
}

/// A daemon that reports the supplied identity and then loses every operation
/// reply. Request counts prove mismatched targets never reach execution.
fn lost_reply_daemon(project: &std::path::Path) -> (u16, Arc<AtomicUsize>) {
	let listener = TcpListener::bind("127.0.0.1:0").unwrap();
	let port = listener.local_addr().unwrap().port();
	let canonical = project.canonicalize().unwrap().to_string_lossy().into_owned();
	let requests = Arc::new(AtomicUsize::new(0));
	let count = requests.clone();
	thread::spawn(move || {
		for stream in listener.incoming() {
			let Ok(mut stream) = stream else { break };
			stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
			let mut reader = BufReader::new(stream.try_clone().unwrap());
			let mut line = String::new();
			if reader.read_line(&mut line).is_err() {
				continue;
			}
			let path = line.split_whitespace().nth(1).unwrap_or("").to_owned();
			let mut length = 0;
			loop {
				line.clear();
				if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
					break;
				}
				if let Some((name, value)) = line.split_once(':') {
					if name.eq_ignore_ascii_case("content-length") {
						length = value.trim().parse().unwrap();
					}
				}
			}
			let mut body = vec![0; length];
			reader.read_exact(&mut body).unwrap();
			if path == "/request" {
				count.fetch_add(1, Ordering::SeqCst);
				// The daemon could have applied a mutation here. A closed socket
				// does not let the caller infer rollback or retry safely.
				drop(reader);
				drop(stream);
				continue;
			}
			let body = json!({"name":"stub", "project":canonical, "canonicalProject":canonical,
				"bootId":"stub-boot", "protocol":1, "port":port})
			.to_string();
			write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
		}
	});
	(port, requests)
}

#[test]
fn explicit_port_cannot_bypass_a_mismatching_project() {
	let home = TempDir::new().unwrap();
	let actual = scratch_project();
	let requested = scratch_project();
	let (port, requests) = lost_reply_daemon(&actual.path().join("default.project.json"));
	let output = command(
		&home,
		&[
			"get",
			"--path",
			"Workspace.Part",
			"--project",
			requested.path().to_str().unwrap(),
			"--port",
			&port.to_string(),
			"--raw",
		],
	);
	assert!(!output.status.success());
	assert!(String::from_utf8_lossy(&output.stderr).contains("different project"));
	assert_eq!(requests.load(Ordering::SeqCst), 0);
}

#[test]
fn lost_reply_is_a_parseable_unknown_receipt_and_loopback_ignores_proxies() {
	let home = TempDir::new().unwrap();
	let project = scratch_project();
	let (port, requests) = lost_reply_daemon(&project.path().join("default.project.json"));
	let output = command(
		&home,
		&[
			"get",
			"--path",
			"Workspace.Part",
			"--project",
			project.path().to_str().unwrap(),
			"--port",
			&port.to_string(),
			"--raw",
		],
	);
	assert!(!output.status.success());
	let value: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
		panic!(
			"stdout: {} stderr: {}",
			String::from_utf8_lossy(&output.stdout),
			String::from_utf8_lossy(&output.stderr)
		)
	});
	assert_eq!(value["ok"], false);
	assert_eq!(value["error"]["code"], "TRANSPORT_ERROR");
	assert_eq!(value["meta"]["completion"], "unknown");
	assert_eq!(value["meta"]["retrySafe"], false);
	assert!(uuid::Uuid::parse_str(value["meta"]["operationId"].as_str().unwrap()).is_ok());
	assert!(value["meta"]["statusUrl"].as_str().unwrap().contains("/operations/"));
	assert_eq!(requests.load(Ordering::SeqCst), 1);
}

fn http() -> reqwest::Client {
	reqwest::Client::builder().no_proxy().build().unwrap()
}

#[tokio::test]
async fn missing_plugin_proves_not_started_and_can_be_looked_up() {
	let daemon = start_daemon_with_state_dir();
	let client = http();
	let id = uuid::Uuid::new_v4().to_string();
	let reply: Value = client
		.post(daemon.http("/request"))
		.json(&json!({"op":"set", "operationId":id}))
		.send()
		.await
		.unwrap()
		.json()
		.await
		.unwrap();
	assert_eq!(reply["meta"]["completion"], "notStarted");
	assert_eq!(reply["meta"]["retrySafe"], true);
	let status: Value = client
		.get(daemon.http(&format!("/operations/{id}")))
		.send()
		.await
		.unwrap()
		.json()
		.await
		.unwrap();
	assert_eq!(status["found"], true);
	assert_eq!(status["receipt"]["completion"], "notStarted");
}

#[tokio::test]
async fn timeout_is_unknown_and_a_late_response_updates_the_receipt() {
	let daemon = start_daemon_with_state_dir();
	let (mut plugin, _) = connect_client(&daemon, "plugin", "late-plugin").await;
	let client = http();
	let id = uuid::Uuid::new_v4().to_string();
	let post = client
		.post(daemon.http("/request"))
		.json(&json!({"op":"set", "operationId":id, "timeoutMs":100}))
		.send();
	let pending = tokio::spawn(async move { post.await.unwrap().json::<Value>().await.unwrap() });
	let request = wait_for_frame(&mut plugin, Duration::from_secs(5), |frame| frame["type"] == "request")
		.await
		.unwrap();
	let reply = pending.await.unwrap();
	assert_eq!(reply["meta"]["completion"], "unknown");
	assert_eq!(reply["meta"]["retrySafe"], false);
	plugin
		.send(Message::Text(
			json!({"type":"response", "request_id":request["request_id"], "ok":true, "value":{"applied":1}})
				.to_string(),
		))
		.await
		.unwrap();
	let mut status = Value::Null;
	for _ in 0..30 {
		status = client
			.get(daemon.http(&format!("/operations/{id}")))
			.send()
			.await
			.unwrap()
			.json()
			.await
			.unwrap();
		if status["receipt"]["completion"] == "completed" {
			break;
		}
		tokio::time::sleep(Duration::from_millis(10)).await;
	}
	assert_eq!(status["receipt"]["completion"], "completed");
	assert_eq!(status["receipt"]["retrySafe"], false);
}

#[tokio::test]
async fn plugin_disconnect_after_dispatch_is_unknown() {
	let daemon = start_daemon_with_state_dir();
	let (mut plugin, _) = connect_client(&daemon, "plugin", "disconnect-plugin").await;
	let post = http().post(daemon.http("/request")).json(&json!({"op":"set"})).send();
	let pending = tokio::spawn(async move { post.await.unwrap().json::<Value>().await.unwrap() });
	wait_for_frame(&mut plugin, Duration::from_secs(5), |frame| frame["type"] == "request")
		.await
		.unwrap();
	plugin.close(None).await.unwrap();
	let reply = pending.await.unwrap();
	assert_eq!(reply["meta"]["completion"], "unknown");
	assert_eq!(reply["meta"]["retrySafe"], false);
}

#[tokio::test]
async fn partial_validation_failure_preserves_plugin_audit_and_is_not_retry_safe() {
	let daemon = start_daemon_with_state_dir();
	let (mut plugin, _) = connect_client(&daemon, "plugin", "partial-plugin").await;
	let client = http();
	let id = uuid::Uuid::new_v4().to_string();
	let post = client
		.post(daemon.http("/request"))
		.json(&json!({"op":"set", "operationId":id}))
		.send();
	let pending = tokio::spawn(async move { post.await.unwrap().json::<Value>().await.unwrap() });
	let request = wait_for_frame(&mut plugin, Duration::from_secs(5), |frame| frame["type"] == "request")
		.await
		.unwrap();
	plugin
		.send(Message::Text(
			json!({"type":"response", "request_id":request["request_id"], "ok":false,
		"error":{"code":"INVALID_ARGUMENT", "message":"second property is invalid"},
		"meta":{"mutation":true, "audit":{"applied":1}, "completion":"notStarted"}})
			.to_string(),
		))
		.await
		.unwrap();
	let reply = pending.await.unwrap();
	assert_eq!(reply["meta"]["completion"], "partial");
	assert_eq!(reply["meta"]["retrySafe"], false);
	assert_eq!(reply["meta"]["plugin"]["audit"]["applied"], 1);
	let duplicate: Value = client
		.post(daemon.http("/request"))
		.json(&json!({"op":"set", "operationId":id}))
		.send()
		.await
		.unwrap()
		.json()
		.await
		.unwrap();
	assert_eq!(duplicate["error"]["code"], "CONFLICT");
	assert_eq!(duplicate["meta"]["completion"], "unknown");
}

#[tokio::test]
async fn changed_boot_or_project_is_rejected_before_plugin_dispatch() {
	let daemon = start_daemon_with_state_dir();
	let client = http();
	for guard in [
		json!({"expectedBootId":"another-boot"}),
		json!({"expectedProject":"/another/project.json"}),
	] {
		let mut body = guard;
		body["op"] = json!("set");
		let reply: Value = client
			.post(daemon.http("/request"))
			.json(&body)
			.send()
			.await
			.unwrap()
			.json()
			.await
			.unwrap();
		assert_eq!(reply["error"]["code"], "CONFLICT");
		assert_eq!(reply["meta"]["completion"], "notStarted");
	}
}

#[tokio::test]
async fn absent_receipt_explicitly_means_unknown() {
	let daemon = start_daemon_with_state_dir();
	let status: Value = http()
		.get(daemon.http(&format!("/operations/{}", uuid::Uuid::new_v4())))
		.send()
		.await
		.unwrap()
		.json()
		.await
		.unwrap();
	assert_eq!(status["found"], false);
	assert_eq!(status["receipt"], Value::Null);
	assert!(status["missingMeans"].as_str().unwrap().starts_with("unknown"));
	assert_eq!(status["retention"]["durable"], false);
}

#[tokio::test]
async fn handler_level_failures_cannot_claim_completed_execution() {
	let daemon = start_daemon_with_state_dir();
	let (mut plugin, _) = connect_client(&daemon, "plugin", "inner-failure-plugin").await;
	let client = http();
	for (op, value, meta, expected) in [
		("undo", json!({"requested":true}), json!({"mutation":true}), "unknown"),
		("redo", json!({"requested":true}), json!({"mutation":true}), "unknown"),
		(
			"playtest_start",
			json!({"jobId":"job-1","status":"starting"}),
			json!({"mutation":true}),
			"unknown",
		),
		(
			"playtest_run_start",
			json!({"jobId":"job-2","run":{"done":false}}),
			json!({"mutation":true}),
			"unknown",
		),
		("eval", json!({"requested":true}), json!({}), "completed"),
		(
			"set_batch",
			json!({"applied":1,"failed":1,"results":[{"ok":true},{"ok":false}]}),
			json!({}),
			"partial",
		),
		(
			"set_batch",
			json!({"applied":0,"failed":1,"results":[{"ok":false}]}),
			json!({}),
			"unknown",
		),
		(
			"playtest_exec",
			json!({"ok":false,"error":"source failed"}),
			json!({"mutation":true,"audit":{"target":"server"}}),
			"partial",
		),
		(
			"eval",
			json!({"ok":false,"error":"user table, not a protocol error"}),
			json!({}),
			"completed",
		),
	] {
		let id = uuid::Uuid::new_v4().to_string();
		let post = client
			.post(daemon.http("/request"))
			.json(&json!({"op":op,"operationId":id}))
			.send();
		let pending = tokio::spawn(async move { post.await.unwrap().json::<Value>().await.unwrap() });
		let request = wait_for_frame(&mut plugin, Duration::from_secs(5), |frame| frame["type"] == "request")
			.await
			.unwrap();
		plugin
			.send(Message::Text(
				json!({"type":"response", "request_id":request["request_id"], "ok":true,"value":value,"meta":meta})
					.to_string(),
			))
			.await
			.unwrap();
		let reply = pending.await.unwrap();
		assert_eq!(reply["meta"]["completion"], expected, "{op}");
		assert_eq!(reply["meta"]["retrySafe"], false, "{op}");
		assert_eq!(reply["meta"]["completionScope"], "requestedEffects", "{op}");
		if matches!(op, "undo" | "redo" | "playtest_start" | "playtest_run_start") {
			assert_eq!(reply["meta"]["acknowledged"], true, "{op}");
			assert!(reply["meta"]["nextCheck"].is_string(), "{op}");
		}
		let status: Value = client
			.get(daemon.http(&format!("/operations/{id}")))
			.send()
			.await
			.unwrap()
			.json()
			.await
			.unwrap();
		assert_eq!(status["receipt"]["completion"], expected, "{op}");
	}
}

#[tokio::test]
async fn legacy_exec_acknowledges_queueing_and_checks_identity() {
	let daemon = start_daemon_with_state_dir();
	let client = http();
	let request = |extra: Value| {
		let mut body = json!({"code":"print('mock only')", "focus":false});
		body.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
		client
			.post(daemon.http("/exec"))
			.header("content-type", "application/msgpack")
			.body(rmp_serde::to_vec_named(&body).unwrap())
	};
	let no_plugin = request(json!({})).send().await.unwrap();
	assert_eq!(no_plugin.status(), 503);
	assert!(no_plugin.text().await.unwrap().contains("no code was queued"));
	let (mut plugin, _) = connect_client(&daemon, "plugin", "legacy-exec-plugin").await;
	let stale = request(json!({"expectedBootId":"another-boot"})).send().await.unwrap();
	assert_eq!(stale.status(), 409);
	let success = request(json!({"expectedBootId":daemon.boot_id})).send().await.unwrap();
	assert_eq!(success.status(), 200);
	let body = success.text().await.unwrap();
	assert!(body.contains("queued") && body.contains("unknown"));
	let execute = wait_for_frame(&mut plugin, Duration::from_secs(5), |frame| frame["type"] == "execute")
		.await
		.unwrap();
	assert_eq!(execute["code"], "print('mock only')");
}
