//! Conflict-engine integration tests (Design §6.3): parking on concurrent
//! edits, `conflict` events, the `/resolve` surface, both resolution
//! directions, deletion provenance, and baseline re-stamping.

mod common;

use futures_util::SinkExt;
use serde_json::{json, Value};
use std::{fs, path::Path, time::Duration};
use tokio_tungstenite::tungstenite::Message;

use common::{connect_client, get_json, post_json, start_daemon, wait_for_frame, TestDaemon, WsClient};

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
async fn a_concurrent_edit_resolves_toward_studio_and_backlogs_the_disk_side() {
	let daemon = start_daemon(None);
	let (mut plugin, _) = connect_client(&daemon, "plugin", "conflict-plugin").await;

	tokio::time::sleep(SETTLE).await;
	let hello = instance_ref(&daemon, "Hello").await;

	let result = park_both_edited(&daemon, &mut plugin, &hello).await;

	// The batch still reports the clash honestly; what changed is that nothing
	// waits on an answer afterwards
	assert_eq!(result["ok"], true);
	assert_eq!(result["conflicts"], 1);

	// Studio wins, without being asked: its content reaches disk on its own
	assert!(
		wait_for_file_content(
			&daemon.src_dir().join("Hello.luau"),
			"return \"studio-edit\"",
			Duration::from_secs(15),
		),
		"Studio's version should land on disk without a decision"
	);

	// …and the disk edit that lost is recoverable rather than gone
	let (_, backlog) = get_json(&daemon, "/backlog").await;
	let entries = backlog["entries"].as_array().unwrap();

	assert_eq!(entries.len(), 1, "backlog: {backlog}");
	assert_eq!(entries[0]["path"], "src/Hello.luau");
	assert_eq!(entries[0]["reason"], "conflict");
	assert!(entries[0]["secondsRemaining"].as_u64().unwrap() > 0);

	// Nothing is left parked waiting for a decision that never comes
	assert!(conflicts(&daemon).await.is_empty());
}

#[tokio::test]
async fn a_backlog_entry_restores_to_disk() {
	let daemon = start_daemon(None);
	let (mut plugin, _) = connect_client(&daemon, "plugin", "conflict-plugin").await;

	tokio::time::sleep(SETTLE).await;
	let hello = instance_ref(&daemon, "Hello").await;

	park_both_edited(&daemon, &mut plugin, &hello).await;

	assert!(
		wait_for_file_content(
			&daemon.src_dir().join("Hello.luau"),
			"return \"studio-edit\"",
			Duration::from_secs(15),
		),
		"Studio's version should land on disk first"
	);

	let (_, backlog) = get_json(&daemon, "/backlog").await;
	let id = backlog["entries"][0]["id"].as_str().unwrap().to_owned();

	let (status, body) = post_json(&daemon, "/backlog/restore", &json!({ "id": id })).await;

	assert_eq!(status, 200, "restore: {body}");
	assert_eq!(body["ok"], true);

	// The disk edit is back where it came from, and the entry is consumed
	assert!(
		wait_for_file_content(
			&daemon.src_dir().join("Hello.luau"),
			"return \"disk-edit\"\n",
			Duration::from_secs(15),
		),
		"the restored content should be back on disk"
	);

	let (_, backlog) = get_json(&daemon, "/backlog").await;
	assert!(backlog["entries"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn an_unknown_backlog_entry_is_refused() {
	let daemon = start_daemon(None);
	let (mut _plugin, _) = connect_client(&daemon, "plugin", "conflict-plugin").await;

	tokio::time::sleep(SETTLE).await;

	let (status, body) = post_json(&daemon, "/backlog/restore", &json!({ "id": "nope" })).await;

	assert_eq!(status, 404);
	assert_eq!(body["ok"], false);
}





