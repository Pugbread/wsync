//! Divergence-set integration tests (Design §7): the chunked `/compare`
//! upload with receipts, `choice-needed` stats, details paging, selection
//! chunking, the immediate disk-direction apply over the sync channel,
//! cancel/studio recording, resolved-elsewhere races and stale-set rules.

mod common;

use serde_json::{json, Value};
use std::{fs, time::Duration};

use common::{connect_client, start_daemon, wait_for_frame, TestDaemon};

const SETTLE: Duration = Duration::from_millis(600);

/// A well-formed ref no daemon-side instance ever has (the "Studio-only"
/// side of the comparison)
const STUDIO_ONLY_REF: &str = "ffffffffffffffffffffffffffffffff";

async fn snapshot(daemon: &TestDaemon) -> Value {
	reqwest::get(daemon.http("/snapshot"))
		.await
		.unwrap()
		.json()
		.await
		.unwrap()
}

async fn child_ref(daemon: &TestDaemon, name: &str) -> Option<String> {
	let snapshot = snapshot(daemon).await;

	let storage = snapshot["children"]
		.as_array()?
		.iter()
		.find(|child| child["name"] == "ReplicatedStorage")?;

	storage["children"]
		.as_array()?
		.iter()
		.find(|child| child["name"] == name)
		.and_then(|child| child["id"].as_str())
		.map(str::to_owned)
}

/// Writes a second module and waits until the watcher ingested it
async fn grow_second_module(daemon: &TestDaemon) -> String {
	tokio::time::sleep(SETTLE).await;
	fs::write(daemon.src_dir().join("ModA.luau"), "return \"mod-a\"\n").unwrap();

	let deadline = tokio::time::Instant::now() + Duration::from_secs(15);

	loop {
		if let Some(id) = child_ref(daemon, "ModA").await {
			return id;
		}

		assert!(
			tokio::time::Instant::now() < deadline,
			"ModA never appeared in the tree"
		);
		tokio::time::sleep(Duration::from_millis(100)).await;
	}
}

async fn post_json(daemon: &TestDaemon, path: &str, body: &Value) -> (reqwest::StatusCode, Value) {
	let response = reqwest::Client::new()
		.post(daemon.http(path))
		.json(body)
		.send()
		.await
		.unwrap();

	let status = response.status();
	(status, response.json().await.unwrap())
}

async fn get_json(daemon: &TestDaemon, path: &str) -> (reqwest::StatusCode, Value) {
	let response = reqwest::get(daemon.http(path)).await.unwrap();
	let status = response.status();

	(status, response.json().await.unwrap())
}

fn entry(reference: &str, change: &str, class: &str, name: &str, instance_path: &str) -> Value {
	json!({
		"ref": reference,
		"change": change,
		"class": class,
		"name": name,
		"instancePath": instance_path,
	})
}






#[tokio::test]
async fn compare_and_selection_validate_bounds() {
	let daemon = start_daemon(None);
	tokio::time::sleep(Duration::from_millis(100)).await;

	// Malformed ref → 400
	let (status, _) = post_json(
		&daemon,
		"/compare",
		&json!({
			"submissionId": "sub-v",
			"chunkIndex": 0,
			"finalChunk": false,
			"entries": [entry("not-a-ref", "add", "Folder", "X", "X")],
		}),
	)
	.await;
	assert_eq!(status, 400);

	// Unknown change kind → 400
	let (status, _) = post_json(
		&daemon,
		"/compare",
		&json!({
			"submissionId": "sub-v",
			"chunkIndex": 0,
			"finalChunk": false,
			"entries": [entry(STUDIO_ONLY_REF, "mutate", "Folder", "X", "X")],
		}),
	)
	.await;
	assert_eq!(status, 400);

	// Chunk count over the 512-entry bound → 400
	let oversized: Vec<Value> = (0..513)
		.map(|index| entry(STUDIO_ONLY_REF, "remove", "Folder", &format!("X{index}"), "X"))
		.collect();

	let (status, _) = post_json(
		&daemon,
		"/compare",
		&json!({
			"submissionId": "sub-v",
			"chunkIndex": 0,
			"finalChunk": false,
			"entries": oversized,
		}),
	)
	.await;
	assert_eq!(status, 400);

	// First chunk must be index 0 → 409 otherwise
	let (status, _) = post_json(
		&daemon,
		"/compare",
		&json!({
			"submissionId": "sub-v",
			"chunkIndex": 3,
			"finalChunk": false,
			"entries": [],
		}),
	)
	.await;
	assert_eq!(status, 409);

	// Selection against no pending set → 404
	let (status, _) = post_json(
		&daemon,
		"/choice/selection",
		&json!({
			"choiceId": "nope",
			"submissionId": "sel-v",
			"chunkIndex": 0,
			"finalChunk": true,
			"ids": [0],
		}),
	)
	.await;
	assert_eq!(status, 404);

	// Decision against no pending set → 409 resolved
	let (status, body) = post_json(&daemon, "/choice", &json!({ "choiceId": "nope", "choice": "cancel" })).await;
	assert_eq!(status, 409);
	assert_eq!(body["error"], "resolved");
}
