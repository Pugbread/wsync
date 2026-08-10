//! Conflict-engine integration tests (Design §6.3): parking on concurrent
//! edits, `conflict` events, the `/resolve` surface, both resolution
//! directions, deletion provenance, and baseline re-stamping.

mod common;

use futures_util::SinkExt;
use serde_json::{json, Value};
use std::{fs, path::Path, time::Duration};
use tokio_tungstenite::tungstenite::Message;

use common::{connect_client, start_daemon, wait_for_frame, TestDaemon, WsClient};

/// Startup + debounce grace: writes before it can be swallowed by the
/// watcher's self-write window (see tests/protocol.rs)
const SETTLE: Duration = Duration::from_millis(600);

async fn snapshot(daemon: &TestDaemon) -> Value {
	reqwest::get(daemon.http("/snapshot"))
		.await
		.unwrap()
		.json()
		.await
		.unwrap()
}

/// Resolves the ref of `ReplicatedStorage/<name>` from the JSON snapshot
async fn instance_ref(daemon: &TestDaemon, name: &str) -> String {
	let snapshot = snapshot(daemon).await;

	let storage = snapshot["children"]
		.as_array()
		.unwrap()
		.iter()
		.find(|child| child["name"] == "ReplicatedStorage")
		.expect("no ReplicatedStorage in snapshot");

	storage["children"]
		.as_array()
		.unwrap()
		.iter()
		.find(|child| child["name"] == name)
		.unwrap_or_else(|| panic!("no {name} under ReplicatedStorage"))["id"]
		.as_str()
		.unwrap()
		.to_owned()
}

fn push_update(id: &str, source: &str) -> String {
	json!({
		"type": "push",
		"ops": {
			"additions": [],
			"updates": [{ "id": id, "properties": { "Source": { "String": source } } }],
			"removals": [],
		},
	})
	.to_string()
}

fn push_removal(id: &str) -> String {
	json!({
		"type": "push",
		"ops": { "additions": [], "updates": [], "removals": [id] },
	})
	.to_string()
}

async fn push_result(plugin: &mut WsClient) -> Value {
	wait_for_frame(plugin, Duration::from_secs(10), |frame| frame["type"] == "push-result")
		.await
		.expect("no push-result frame")
}

async fn conflicts(daemon: &TestDaemon) -> Vec<Value> {
	let body: Value = reqwest::get(daemon.http("/resolve"))
		.await
		.unwrap()
		.json()
		.await
		.unwrap();

	body["conflicts"].as_array().unwrap().clone()
}

async fn resolve(daemon: &TestDaemon, id: &str, keep: &str) -> (reqwest::StatusCode, Value) {
	let response = reqwest::Client::new()
		.post(daemon.http("/resolve"))
		.json(&json!({ "id": id, "keep": keep, "choice": keep }))
		.send()
		.await
		.unwrap();

	let status = response.status();
	(status, response.json().await.unwrap())
}

/// Polls a file until its content matches, tolerating the async write path
fn wait_for_file_content(path: &Path, expected: &str, timeout: Duration) -> bool {
	let deadline = std::time::Instant::now() + timeout;

	while std::time::Instant::now() < deadline {
		if fs::read_to_string(path)
			.map(|content| content == expected)
			.unwrap_or(false)
		{
			return true;
		}

		std::thread::sleep(Duration::from_millis(50));
	}

	false
}

/// Edits Hello.luau on disk, waits for the propagated sync frame, then
/// immediately pushes a conflicting Studio edit for the same instance —
/// the canonical both-edited race
async fn park_both_edited(daemon: &TestDaemon, plugin: &mut WsClient, hello: &str) -> Value {
	fs::write(daemon.src_dir().join("Hello.luau"), "return \"disk-edit\"\n").unwrap();

	wait_for_frame(plugin, Duration::from_secs(15), |frame| {
		frame["type"] == "sync" && !frame["updates"].as_array().unwrap().is_empty()
	})
	.await
	.expect("no sync frame after the disk edit");

	plugin
		.send(Message::Text(push_update(hello, "return \"studio-edit\"")))
		.await
		.unwrap();

	push_result(plugin).await
}

#[tokio::test]
async fn concurrent_edit_parks_a_conflict_and_lists_it() {
	let daemon = start_daemon(None);
	let (mut watch, _) = connect_client(&daemon, "watch", "conflict-watch").await;
	let (mut plugin, _) = connect_client(&daemon, "plugin", "conflict-plugin").await;

	tokio::time::sleep(SETTLE).await;
	let hello = instance_ref(&daemon, "Hello").await;

	let result = park_both_edited(&daemon, &mut plugin, &hello).await;

	// The batch reports the real parked count, not an error
	assert_eq!(result["ok"], true);
	assert_eq!(result["applied"], 0);
	assert_eq!(result["skipped"], 0);
	assert_eq!(result["conflicts"], 1);

	// The parked push must not have touched the disk
	assert_eq!(
		fs::read_to_string(daemon.src_dir().join("Hello.luau")).unwrap(),
		"return \"disk-edit\"\n"
	);

	// The conflict event reaches watch clients with the pinned flat shape
	let event = wait_for_frame(&mut watch, Duration::from_secs(10), |frame| {
		frame["type"] == "event" && frame["topic"] == "conflict"
	})
	.await
	.expect("no conflict event");

	assert!(!event["id"].as_str().unwrap().is_empty());
	assert_eq!(event["path"], "src/Hello.luau");
	assert_eq!(event["instancePath"], "ReplicatedStorage/Hello");
	assert_eq!(event["classification"], "both-edited");

	// GET /resolve lists the parked conflict with both sides' content
	let listed = conflicts(&daemon).await;
	assert_eq!(listed.len(), 1);

	let record = &listed[0];
	assert_eq!(record["id"], event["id"]);
	assert_eq!(record["path"], "src/Hello.luau");
	assert_eq!(record["instancePath"], "ReplicatedStorage/Hello");
	assert_eq!(record["class"], "ModuleScript");
	assert_eq!(record["kind"], "script");
	assert_eq!(record["classification"], "both-edited");
	assert_eq!(record["fs"]["present"], true);
	assert_eq!(record["fs"]["source"], "return \"disk-edit\"\n");
	assert_eq!(record["studio"]["present"], true);
	assert_eq!(record["studio"]["source"], "return \"studio-edit\"");
	assert_eq!(record["fs"]["hash"].as_str().unwrap().len(), 64);
	assert_eq!(record["studio"]["hash"].as_str().unwrap().len(), 64);
	assert!(record["detectedAt"].as_str().unwrap().contains('T'));

	// Unknown id and missing direction are rejected cleanly
	let (status, body) = resolve(&daemon, "c999", "local").await;
	assert_eq!(status, 404);
	assert_eq!(body["ok"], false);
}

#[tokio::test]
async fn resolve_keep_local_pushes_disk_state_and_restamps() {
	let daemon = start_daemon(None);
	let (mut plugin, _) = connect_client(&daemon, "plugin", "keep-local-plugin").await;

	tokio::time::sleep(SETTLE).await;
	let hello = instance_ref(&daemon, "Hello").await;

	park_both_edited(&daemon, &mut plugin, &hello).await;

	let id = conflicts(&daemon).await[0]["id"].as_str().unwrap().to_owned();

	let (status, body) = resolve(&daemon, &id, "local").await;
	assert_eq!(status, 200);
	assert_eq!(body["ok"], true);
	assert_eq!(body["resolved"], id);

	// Keep local emits the disk state over the sync channel
	let sync = wait_for_frame(&mut plugin, Duration::from_secs(10), |frame| {
		frame["type"] == "sync"
			&& frame["updates"]
				.as_array()
				.unwrap()
				.iter()
				.any(|update| update["id"] == hello)
	})
	.await
	.expect("no resolution sync frame");

	let update = sync["updates"]
		.as_array()
		.unwrap()
		.iter()
		.find(|update| update["id"] == hello)
		.unwrap();

	assert_eq!(update["properties"]["Source"]["String"], "return \"disk-edit\"\n");

	// The conflict is gone and the disk was never touched
	assert!(conflicts(&daemon).await.is_empty());
	assert_eq!(
		fs::read_to_string(daemon.src_dir().join("Hello.luau")).unwrap(),
		"return \"disk-edit\"\n"
	);

	// Baselines re-stamped: a follow-up Studio edit now applies cleanly
	plugin
		.send(Message::Text(push_update(&hello, "return \"studio-later\"")))
		.await
		.unwrap();

	let result = push_result(&mut plugin).await;
	assert_eq!(result["ok"], true);
	assert_eq!(result["applied"], 1);
	assert_eq!(result["conflicts"], 0);

	assert!(wait_for_file_content(
		&daemon.src_dir().join("Hello.luau"),
		"return \"studio-later\"",
		Duration::from_secs(5),
	));
}

#[tokio::test]
async fn resolve_keep_studio_writes_disk_and_restamps() {
	let daemon = start_daemon(None);
	let (mut plugin, _) = connect_client(&daemon, "plugin", "keep-studio-plugin").await;

	tokio::time::sleep(SETTLE).await;
	let hello = instance_ref(&daemon, "Hello").await;

	park_both_edited(&daemon, &mut plugin, &hello).await;

	let id = conflicts(&daemon).await[0]["id"].as_str().unwrap().to_owned();

	let (status, body) = resolve(&daemon, &id, "studio").await;
	assert_eq!(status, 200);
	assert_eq!(body["ok"], true);

	// Keep studio lands the parked Studio state on disk via the write path
	assert!(wait_for_file_content(
		&daemon.src_dir().join("Hello.luau"),
		"return \"studio-edit\"",
		Duration::from_secs(5),
	));

	assert!(conflicts(&daemon).await.is_empty());

	// Baselines re-stamped: the next disk edit propagates normally
	tokio::time::sleep(SETTLE).await;
	fs::write(daemon.src_dir().join("Hello.luau"), "return \"after\"\n").unwrap();

	let sync = wait_for_frame(&mut plugin, Duration::from_secs(15), |frame| {
		frame["type"] == "sync" && !frame["updates"].as_array().unwrap().is_empty()
	})
	.await
	.expect("no sync frame after resolution");

	let update = &sync["updates"].as_array().unwrap()[0];
	assert_eq!(update["properties"]["Source"]["String"], "return \"after\"\n");
}

#[tokio::test]
async fn fs_delete_racing_studio_edit_parks_and_can_restore() {
	let daemon = start_daemon(None);
	let (mut plugin, _) = connect_client(&daemon, "plugin", "fs-delete-plugin").await;

	tokio::time::sleep(SETTLE).await;
	let hello = instance_ref(&daemon, "Hello").await;

	// Disk deletes the script; the removal propagates cleanly
	fs::remove_file(daemon.src_dir().join("Hello.luau")).unwrap();

	wait_for_frame(&mut plugin, Duration::from_secs(15), |frame| {
		frame["type"] == "sync"
			&& frame["removals"]
				.as_array()
				.unwrap()
				.iter()
				.any(|id| id == &json!(hello))
	})
	.await
	.expect("no removal sync frame");

	// A Studio edit of the just-deleted instance races the removal
	plugin
		.send(Message::Text(push_update(&hello, "return \"studio-edit\"")))
		.await
		.unwrap();

	let result = push_result(&mut plugin).await;
	assert_eq!(result["ok"], true);
	assert_eq!(result["conflicts"], 1);

	let listed = conflicts(&daemon).await;
	assert_eq!(listed.len(), 1);
	assert_eq!(listed[0]["classification"], "fs-deleted-studio-edited");
	assert_eq!(listed[0]["fs"]["present"], false);
	assert_eq!(listed[0]["studio"]["present"], true);
	assert_eq!(listed[0]["studio"]["source"], "return \"studio-edit\"");

	// Keep studio restores the file from the parked Studio state
	let id = listed[0]["id"].as_str().unwrap().to_owned();
	let (status, body) = resolve(&daemon, &id, "studio").await;
	assert_eq!(status, 200);
	assert_eq!(body["ok"], true);

	assert!(wait_for_file_content(
		&daemon.src_dir().join("Hello.luau"),
		"return \"studio-edit\"",
		Duration::from_secs(5),
	));
	assert!(conflicts(&daemon).await.is_empty());
}

#[tokio::test]
async fn studio_delete_racing_fs_edit_parks_and_keep_local_recreates() {
	let daemon = start_daemon(None);
	let (mut plugin, _) = connect_client(&daemon, "plugin", "studio-delete-plugin").await;

	tokio::time::sleep(SETTLE).await;
	let hello = instance_ref(&daemon, "Hello").await;

	// Disk edit propagates, then Studio deletes the same instance while the
	// edit is still in flight
	fs::write(daemon.src_dir().join("Hello.luau"), "return \"disk-edit\"\n").unwrap();

	wait_for_frame(&mut plugin, Duration::from_secs(15), |frame| {
		frame["type"] == "sync" && !frame["updates"].as_array().unwrap().is_empty()
	})
	.await
	.expect("no sync frame after the disk edit");

	plugin.send(Message::Text(push_removal(&hello))).await.unwrap();

	let result = push_result(&mut plugin).await;
	assert_eq!(result["ok"], true);
	assert_eq!(result["conflicts"], 1);

	// The deletion was excluded: the edited file survives
	assert_eq!(
		fs::read_to_string(daemon.src_dir().join("Hello.luau")).unwrap(),
		"return \"disk-edit\"\n"
	);

	let listed = conflicts(&daemon).await;
	assert_eq!(listed.len(), 1);
	assert_eq!(listed[0]["classification"], "studio-deleted-fs-edited");
	assert_eq!(listed[0]["fs"]["present"], true);
	assert_eq!(listed[0]["studio"]["present"], false);

	// Keep local recreates the instance in Studio from the live tree
	let id = listed[0]["id"].as_str().unwrap().to_owned();
	let (status, body) = resolve(&daemon, &id, "local").await;
	assert_eq!(status, 200);
	assert_eq!(body["ok"], true);

	let sync = wait_for_frame(&mut plugin, Duration::from_secs(10), |frame| {
		frame["type"] == "sync" && !frame["additions"].as_array().unwrap().is_empty()
	})
	.await
	.expect("no recreation sync frame");

	let addition = &sync["additions"].as_array().unwrap()[0];
	assert_eq!(addition["id"], hello);
	assert_eq!(addition["name"], "Hello");
	assert_eq!(addition["properties"]["Source"]["String"], "return \"disk-edit\"\n");

	assert!(conflicts(&daemon).await.is_empty());
}
