//! Protocol v1 integration tests: hello/version gating, the single-plugin
//! slot, sync/push frames, the request router, heartbeats, the JSON snapshot
//! surface and authenticated stop (Design §5, §15 phase 2).

mod common;

use futures_util::SinkExt;
use serde_json::{json, Value};
use std::{fs, time::Duration};
use tokio_tungstenite::tungstenite::Message;

use common::{connect_client, next_frame, start_daemon, wait_for_frame};

fn is_hex_ref(value: &Value) -> bool {
	value
		.as_str()
		.is_some_and(|hex| hex.len() == 32 && hex.bytes().all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f')))
}

#[tokio::test]
async fn http_hello_reports_real_version_and_protocol() {
	let daemon = start_daemon(None);

	let hello: Value = reqwest::get(daemon.http("/hello")).await.unwrap().json().await.unwrap();

	assert_eq!(hello["version"], env!("CARGO_PKG_VERSION"));
	assert_eq!(hello["protocol"], 1);
	assert_eq!(hello["name"], "wsync-fixture");
	assert_eq!(hello["gameId"], 5550001);
	assert_eq!(hello["bootId"], Value::String(daemon.boot_id.clone()));
	assert_eq!(hello["managedBy"], "test");
	assert!(hello["canonicalProject"]
		.as_str()
		.unwrap()
		.ends_with("default.project.json"));
}

#[tokio::test]
async fn ws_hello_gates_protocol_and_reports_identity() {
	let daemon = start_daemon(None);

	// Wrong protocol → typed non-retryable shutdown
	let (mut socket, _) = tokio_tungstenite::connect_async(daemon.ws_url()).await.unwrap();

	socket
		.send(Message::Text(
			json!({ "type": "hello", "clientId": "x", "role": "plugin", "protocol": 7 }).to_string(),
		))
		.await
		.unwrap();

	let refusal = next_frame(&mut socket, Duration::from_secs(5)).await.unwrap();
	assert_eq!(refusal["type"], "shutdown");
	assert_eq!(refusal["code"], "PROTOCOL_MISMATCH");
	assert_eq!(refusal["retryable"], false);

	// Correct hello → server hello with the ruling's exact shape
	let (_socket, hello) = connect_client(&daemon, "plugin", "gate-plugin").await;

	assert_eq!(hello["type"], "hello");
	assert_eq!(hello["name"], "wsync-fixture");
	assert_eq!(hello["version"], env!("CARGO_PKG_VERSION"));
	assert_eq!(hello["gameId"], 5550001);
	assert_eq!(hello["placeIds"], json!([7770001]));

	let root_refs = hello["rootRefs"].as_array().unwrap();
	assert!(!root_refs.is_empty());
	assert!(root_refs.iter().all(is_hex_ref));
}

#[tokio::test]
async fn second_plugin_is_rejected_with_typed_shutdown() {
	let daemon = start_daemon(None);

	let (_first, _) = connect_client(&daemon, "plugin", "first-plugin").await;

	let (mut second, _) = tokio_tungstenite::connect_async(daemon.ws_url()).await.unwrap();

	second
		.send(Message::Text(
			json!({
				"type": "hello",
				"clientId": "second",
				"role": "plugin",
				"protocol": 1,
				"name": "second-plugin",
			})
			.to_string(),
		))
		.await
		.unwrap();

	let refusal = next_frame(&mut second, Duration::from_secs(5)).await.unwrap();

	assert_eq!(refusal["type"], "shutdown");
	assert_eq!(refusal["code"], "PLUGIN_SLOT_TAKEN");
	assert_eq!(refusal["retryable"], false);
	assert!(refusal["reason"].as_str().unwrap().contains("first-plugin"));
}

#[tokio::test]
async fn file_edit_delivers_flat_sync_frame() {
	let daemon = start_daemon(None);
	let (mut plugin, _) = connect_client(&daemon, "plugin", "sync-plugin").await;

	// The debouncer suppresses events inside the 300 ms self-write grace
	// window that also covers startup; write after it, like a real editor
	tokio::time::sleep(Duration::from_millis(600)).await;

	fs::write(daemon.src_dir().join("Hello.luau"), "return \"edited\"\n").unwrap();

	let sync = wait_for_frame(&mut plugin, Duration::from_secs(15), |frame| frame["type"] == "sync")
		.await
		.expect("no sync frame after file edit");

	// Flat envelope: payload fields inline, never a nested wrapper
	assert!(sync["additions"].is_array());
	assert!(sync["updates"].is_array());
	assert!(sync["removals"].is_array());

	let updates = sync["updates"].as_array().unwrap();
	let additions = sync["additions"].as_array().unwrap();
	assert!(!updates.is_empty() || !additions.is_empty());

	for update in updates {
		assert!(is_hex_ref(&update["id"]));
	}

	for addition in additions {
		assert!(is_hex_ref(&addition["id"]));
		assert!(is_hex_ref(&addition["parent"]));
	}
}

async fn fetch_child_ref(daemon: &common::TestDaemon, name: &str) -> String {
	let snapshot: Value = reqwest::get(daemon.http("/snapshot"))
		.await
		.unwrap()
		.json()
		.await
		.unwrap();

	let children = snapshot["children"].as_array().unwrap();

	children
		.iter()
		.find(|child| child["name"] == name)
		.unwrap_or_else(|| panic!("no {name} child in snapshot"))["id"]
		.as_str()
		.unwrap()
		.to_owned()
}

#[tokio::test]
async fn push_writes_to_disk_and_reports_push_result() {
	let daemon = start_daemon(None);
	let (mut plugin, _) = connect_client(&daemon, "plugin", "push-plugin").await;

	let parent = fetch_child_ref(&daemon, "ReplicatedStorage").await;
	let new_ref = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

	let push = json!({
		"type": "push",
		"ops": {
			"additions": [{
				"id": new_ref,
				"parent": parent,
				"name": "NewMod",
				"class": "ModuleScript",
				"meta": { "keepUnknowns": false },
				"properties": { "Source": { "String": "return 1" } },
				"children": [],
			}],
			"updates": [],
			"removals": [],
		},
	});

	plugin.send(Message::Text(push.to_string())).await.unwrap();

	let result = wait_for_frame(&mut plugin, Duration::from_secs(10), |frame| {
		frame["type"] == "push-result"
	})
	.await
	.expect("no push-result frame");

	assert_eq!(result["ok"], true);
	assert_eq!(result["applied"], 1);
	assert_eq!(result["skipped"], 0);
	assert_eq!(result["conflicts"], 0);
	assert_eq!(result["errors"], json!([]));

	let written = daemon.src_dir().join("NewMod.luau");
	assert!(written.exists(), "push did not write {written:?}");
	assert_eq!(fs::read_to_string(written).unwrap(), "return 1");
}

#[tokio::test]
async fn push_with_malformed_ref_is_rejected_wholesale() {
	let daemon = start_daemon(None);
	let (mut plugin, _) = connect_client(&daemon, "plugin", "reject-plugin").await;

	let parent = fetch_child_ref(&daemon, "ReplicatedStorage").await;

	let push = json!({
		"type": "push",
		"ops": {
			"additions": [{
				"id": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
				"parent": parent,
				"name": "ShouldNotExist",
				"class": "ModuleScript",
				"meta": { "keepUnknowns": false },
				"properties": { "Source": { "String": "return 2" } },
				"children": [],
			}],
			"updates": [],
			"removals": ["THIS-IS-NOT-A-REF"],
		},
	});

	plugin.send(Message::Text(push.to_string())).await.unwrap();

	let result = wait_for_frame(&mut plugin, Duration::from_secs(10), |frame| {
		frame["type"] == "push-result"
	})
	.await
	.expect("no push-result frame");

	assert_eq!(result["ok"], false);
	assert_eq!(result["applied"], 0);
	assert!(result["errors"][0].as_str().unwrap().contains("removals[0]"));

	// Nothing from the frame may have been applied
	assert!(!daemon.src_dir().join("ShouldNotExist.luau").exists());
}

#[tokio::test]
async fn request_round_trips_through_a_ws_plugin() {
	let daemon = start_daemon(None);
	let (mut plugin, _) = connect_client(&daemon, "plugin", "op-plugin").await;

	let client = reqwest::Client::new();
	let post = client
		.post(daemon.http("/request"))
		.json(&json!({ "op": "ping", "args": { "echo": 42 } }))
		.send();

	let http = tokio::spawn(async move { post.await.unwrap().json::<Value>().await.unwrap() });

	// The plugin sees a request frame and answers it
	let request = wait_for_frame(&mut plugin, Duration::from_secs(10), |frame| frame["type"] == "request")
		.await
		.expect("plugin never received the request frame");

	assert_eq!(request["op"], "ping");
	assert_eq!(request["args"]["echo"], 42);
	assert_eq!(request["timeout_ms"], 5000);

	let response = json!({
		"type": "response",
		"request_id": request["request_id"],
		"ok": true,
		"value": { "pong": true },
	});

	plugin.send(Message::Text(response.to_string())).await.unwrap();

	let reply = http.await.unwrap();

	assert_eq!(reply["ok"], true);
	assert_eq!(reply["value"]["pong"], true);
	assert_eq!(reply["meta"]["op"], "ping");
	assert_eq!(reply["meta"]["protocol"], 1);
	assert!(reply["meta"]["durationMs"].is_u64());
}

#[tokio::test]
async fn request_times_out_when_the_plugin_stays_silent() {
	let daemon = start_daemon(None);
	let (_plugin, _) = connect_client(&daemon, "plugin", "silent-plugin").await;

	let reply: Value = reqwest::Client::new()
		.post(daemon.http("/request"))
		.json(&json!({ "op": "slow", "timeoutMs": 300 }))
		.send()
		.await
		.unwrap()
		.json()
		.await
		.unwrap();

	assert_eq!(reply["ok"], false);
	assert_eq!(reply["error"]["code"], "TIMEOUT");
	assert!(reply["meta"]["durationMs"].as_u64().unwrap() >= 300);
}

#[tokio::test]
async fn request_without_plugin_reports_plugin_error() {
	let daemon = start_daemon(None);

	let reply: Value = reqwest::Client::new()
		.post(daemon.http("/request"))
		.json(&json!({ "op": "ping" }))
		.send()
		.await
		.unwrap()
		.json()
		.await
		.unwrap();

	assert_eq!(reply["ok"], false);
	assert_eq!(reply["error"]["code"], "PLUGIN_ERROR");
	assert_eq!(reply["error"]["message"], "no Studio plugin connected");
}

#[tokio::test]
async fn silent_client_is_disconnected_by_the_heartbeat() {
	let daemon = start_daemon(None);
	let (mut socket, _) = connect_client(&daemon, "watch", "mute-watch").await;

	// Read frames but never answer pings: the server must close us with a
	// typed heartbeat shutdown after ~8 seconds
	let mut shutdown = None;
	let deadline = tokio::time::Instant::now() + Duration::from_secs(20);

	while tokio::time::Instant::now() < deadline {
		let Some(frame) = next_frame(&mut socket, Duration::from_secs(20)).await else {
			break;
		};

		if frame["type"] == "shutdown" {
			shutdown = Some(frame);
			break;
		}
	}

	let shutdown = shutdown.expect("no shutdown frame before the connection closed");
	assert_eq!(shutdown["code"], "HEARTBEAT_TIMEOUT");
}

#[tokio::test]
async fn watch_clients_receive_sanitized_activity_events() {
	let daemon = start_daemon(None);
	let (mut watch, _) = connect_client(&daemon, "watch", "event-watch").await;
	let (mut plugin, _) = connect_client(&daemon, "plugin", "event-plugin").await;

	let parent = fetch_child_ref(&daemon, "ReplicatedStorage").await;

	let push = json!({
		"type": "push",
		"ops": {
			"additions": [{
				"id": "cccccccccccccccccccccccccccccccc",
				"parent": parent,
				"name": "EventMod",
				"class": "ModuleScript",
				"meta": { "keepUnknowns": false },
				"properties": { "Source": { "String": "return 3" } },
				"children": [],
			}],
			"updates": [],
			"removals": [],
		},
	});

	plugin.send(Message::Text(push.to_string())).await.unwrap();

	let event = wait_for_frame(&mut watch, Duration::from_secs(10), |frame| {
		frame["type"] == "event" && frame["topic"] == "sync-activity" && frame["direction"] == "studio-to-disk"
	})
	.await
	.expect("watch client never received the activity event");

	assert_eq!(event["additions"], 1);
	assert_eq!(event["names"], json!(["EventMod"]));
	// Sanitized: names only, never snapshots or absolute paths
	assert!(event.get("ops").is_none());
	assert!(event.get("properties").is_none());
}

#[tokio::test]
async fn json_snapshot_serves_subtrees_and_strips_argon_empty() {
	let daemon = start_daemon(None);
	let (mut plugin, _) = connect_client(&daemon, "plugin", "snap-plugin").await;

	let parent = fetch_child_ref(&daemon, "ReplicatedStorage").await;

	// The plugin marks empty property maps with ArgonEmpty (wire workaround);
	// no JSON surface may ever leak it back out
	let push = json!({
		"type": "push",
		"ops": {
			"additions": [{
				"id": "dddddddddddddddddddddddddddddddd",
				"parent": parent,
				"name": "EmptyFolder",
				"class": "Folder",
				"meta": { "keepUnknowns": false },
				"properties": { "ArgonEmpty": { "Bool": true } },
				"children": [],
			}],
			"updates": [],
			"removals": [],
		},
	});

	plugin.send(Message::Text(push.to_string())).await.unwrap();

	let result = wait_for_frame(&mut plugin, Duration::from_secs(10), |frame| {
		frame["type"] == "push-result"
	})
	.await
	.unwrap();
	assert_eq!(result["ok"], true);

	// Subtree snapshot by hex ref
	let subtree: Value = reqwest::get(format!("{}?ref={}", daemon.http("/snapshot"), parent))
		.await
		.unwrap()
		.json()
		.await
		.unwrap();

	assert_eq!(subtree["name"], "ReplicatedStorage");
	assert_eq!(subtree["id"], Value::String(parent.clone()));
	assert!(is_hex_ref(&subtree["parent"]));
	assert!(subtree["meta"]["keepUnknowns"].is_boolean());

	let children = subtree["children"].as_array().unwrap();
	let folder = children
		.iter()
		.find(|child| child["name"] == "EmptyFolder")
		.expect("pushed folder missing from snapshot");

	assert_eq!(folder["class"], "Folder");
	assert!(folder["properties"].get("ArgonEmpty").is_none());
	assert!(!serde_json::to_string(&subtree).unwrap().contains("ArgonEmpty"));

	// Root sentinel equals the whole tree
	let root: Value = reqwest::get(format!(
		"{}?ref=00000000000000000000000000000000",
		daemon.http("/snapshot")
	))
	.await
	.unwrap()
	.json()
	.await
	.unwrap();
	assert_eq!(root["class"], "DataModel");

	// Malformed refs are rejected with a clear error
	let bad = reqwest::get(format!("{}?ref=nope", daemon.http("/snapshot")))
		.await
		.unwrap();
	assert_eq!(bad.status(), 400);
	let body: Value = bad.json().await.unwrap();
	assert!(body["error"].as_str().unwrap().contains("Malformed ref"));

	// Unknown (but well-formed) refs are a 404
	let missing = reqwest::get(format!(
		"{}?ref=eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
		daemon.http("/snapshot")
	))
	.await
	.unwrap();
	assert_eq!(missing.status(), 404);
}

#[tokio::test]
async fn managed_stop_requires_exact_boot_id_and_token() {
	let daemon = start_daemon(Some("sekrit-token"));
	let client = reqwest::Client::new();

	// Missing credentials → 401
	let missing = client.post(daemon.http("/stop")).send().await.unwrap();
	assert_eq!(missing.status(), 401);

	// Wrong token → 403
	let wrong_token = client
		.post(daemon.http("/stop"))
		.json(&json!({ "bootId": daemon.boot_id, "token": "wrong" }))
		.send()
		.await
		.unwrap();
	assert_eq!(wrong_token.status(), 403);

	// Wrong boot id → 403
	let wrong_boot = client
		.post(daemon.http("/stop"))
		.json(&json!({ "bootId": "not-this-boot", "token": "sekrit-token" }))
		.send()
		.await
		.unwrap();
	assert_eq!(wrong_boot.status(), 403);

	// The daemon must still be alive after every refused attempt
	let hello = reqwest::get(daemon.http("/hello")).await.unwrap();
	assert!(hello.status().is_success());

	// Exact identity → graceful exit
	let accepted = client
		.post(daemon.http("/stop"))
		.json(&json!({ "bootId": daemon.boot_id, "token": "sekrit-token" }))
		.send()
		.await
		.unwrap();
	assert!(accepted.status().is_success());

	assert!(daemon.wait_for_exit(Duration::from_secs(10)), "daemon did not exit");
}

#[tokio::test]
async fn unmanaged_stop_accepts_an_empty_body() {
	let daemon = start_daemon(None);

	let accepted = reqwest::Client::new().post(daemon.http("/stop")).send().await.unwrap();
	assert!(accepted.status().is_success());

	assert!(daemon.wait_for_exit(Duration::from_secs(10)), "daemon did not exit");
}

#[tokio::test]
async fn manager_heartbeat_and_close_are_authenticated() {
	let daemon = start_daemon(Some("owner-token"));
	let client = reqwest::Client::new();

	// Wrong token → 403; missing token → 401
	let wrong = client
		.post(daemon.http("/manager-heartbeat"))
		.json(&json!({ "token": "nope" }))
		.send()
		.await
		.unwrap();
	assert_eq!(wrong.status(), 403);

	let missing = client.post(daemon.http("/manager-heartbeat")).send().await.unwrap();
	assert_eq!(missing.status(), 401);

	let ok = client
		.post(daemon.http("/manager-heartbeat"))
		.json(&json!({ "token": "owner-token" }))
		.send()
		.await
		.unwrap();
	assert_eq!(ok.status(), 204);

	let close = client
		.post(daemon.http("/manager-close"))
		.json(&json!({ "token": "owner-token" }))
		.send()
		.await
		.unwrap();
	assert_eq!(close.status(), 204);

	assert!(daemon.wait_for_exit(Duration::from_secs(10)), "daemon did not exit");
}

#[tokio::test]
async fn unmanaged_daemons_refuse_manager_routes() {
	let daemon = start_daemon(None);

	let response = reqwest::Client::new()
		.post(daemon.http("/manager-heartbeat"))
		.json(&json!({ "token": "anything" }))
		.send()
		.await
		.unwrap();

	assert_eq!(response.status(), 403);
}

#[tokio::test]
async fn stopping_daemon_notifies_ws_clients_with_typed_shutdown() {
	let daemon = start_daemon(None);
	let (mut watch, _) = connect_client(&daemon, "watch", "shutdown-watch").await;

	reqwest::Client::new().post(daemon.http("/stop")).send().await.unwrap();

	let shutdown = wait_for_frame(&mut watch, Duration::from_secs(10), |frame| frame["type"] == "shutdown")
		.await
		.expect("no shutdown frame on daemon stop");

	assert_eq!(shutdown["code"], "DAEMON_STOPPING");
	assert_eq!(shutdown["retryable"], false);

	assert!(daemon.wait_for_exit(Duration::from_secs(10)));
}
