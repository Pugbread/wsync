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

/// Uploads the canonical three-entry divergence set (add ModA, update
/// Hello, remove a Studio-only instance) in one final chunk and returns the
/// minted choiceId
async fn commit_three_entry_set(daemon: &TestDaemon, mod_a: &str, hello: &str, submission: &str) -> String {
	let (status, receipt) = post_json(
		daemon,
		"/compare",
		&json!({
			"submissionId": submission,
			"chunkIndex": 0,
			"finalChunk": true,
			"entries": [
				entry(mod_a, "add", "ModuleScript", "ModA", "ReplicatedStorage/ModA"),
				entry(hello, "update", "ModuleScript", "Hello", "ReplicatedStorage/Hello"),
				entry(STUDIO_ONLY_REF, "remove", "Folder", "StudioOnly", "ReplicatedStorage/StudioOnly"),
			],
		}),
	)
	.await;

	assert_eq!(status, 200);
	assert_eq!(receipt["ok"], true);
	assert_eq!(receipt["committed"], true);

	receipt["choiceId"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn compare_chunks_details_selection_and_selective_disk_apply() {
	let daemon = start_daemon(None);
	let mod_a = grow_second_module(&daemon).await;
	let hello = child_ref(&daemon, "Hello").await.unwrap();

	let (mut watch, _) = connect_client(&daemon, "watch", "divergence-watch").await;
	let (mut plugin, _) = connect_client(&daemon, "plugin", "divergence-plugin").await;

	// Chunk 0: two entries, not final
	let (status, receipt) = post_json(
		&daemon,
		"/compare",
		&json!({
			"submissionId": "sub-1",
			"chunkIndex": 0,
			"finalChunk": false,
			"entries": [
				entry(&mod_a, "add", "ModuleScript", "ModA", "ReplicatedStorage/ModA"),
				entry(&hello, "update", "ModuleScript", "Hello", "ReplicatedStorage/Hello"),
			],
		}),
	)
	.await;

	assert_eq!(status, 200);
	assert_eq!(receipt["ok"], true);
	assert_eq!(receipt["acceptedChunk"], 0);
	assert_eq!(receipt["nextChunk"], 1);
	assert_eq!(receipt["committed"], false);
	assert!(receipt.get("choiceId").is_none());

	// Resubmitting a chunk index that is not nextChunk → 409 (Design §7.3)
	let (status, body) = post_json(
		&daemon,
		"/compare",
		&json!({
			"submissionId": "sub-1",
			"chunkIndex": 0,
			"finalChunk": false,
			"entries": [],
		}),
	)
	.await;

	assert_eq!(status, 409);
	assert_eq!(body["ok"], false);

	// Final chunk: the Studio-only removal entry; the set freezes
	let (status, receipt) = post_json(
		&daemon,
		"/compare",
		&json!({
			"submissionId": "sub-1",
			"chunkIndex": 1,
			"finalChunk": true,
			"entries": [
				entry(STUDIO_ONLY_REF, "remove", "Folder", "StudioOnly", "ReplicatedStorage/StudioOnly"),
			],
		}),
	)
	.await;

	assert_eq!(status, 200);
	assert_eq!(receipt["committed"], true);
	assert_eq!(receipt["acceptedChunk"], 1);

	let choice_id = receipt["choiceId"].as_str().unwrap().to_owned();

	// choice-needed broadcast with exact stats (all numbers)
	let event = wait_for_frame(&mut watch, Duration::from_secs(10), |frame| {
		frame["type"] == "event" && frame["topic"] == "choice-needed"
	})
	.await
	.expect("no choice-needed event");

	assert_eq!(event["choiceId"], choice_id.as_str());
	assert_eq!(event["total"], 3);
	assert_eq!(event["studioCount"], 2);
	assert_eq!(event["diskCount"], 2);
	assert_eq!(event["onlyOnDisk"], 1);
	assert_eq!(event["differs"], 1);
	assert_eq!(event["missingOnDisk"], 1);

	// GET /choice mirrors the pending stats
	let (status, pending) = get_json(&daemon, "/choice").await;
	assert_eq!(status, 200);
	assert_eq!(pending["pending"], true);
	assert_eq!(pending["choiceId"], choice_id.as_str());
	assert_eq!(pending["stats"]["total"], 3);
	assert_eq!(pending["stats"]["missingOnDisk"], 1);

	// Details paging: limit 2 → ids 0..1 plus a cursor, then the tail page
	let (status, page) = get_json(&daemon, &format!("/choice/details?choiceId={choice_id}&limit=2")).await;
	assert_eq!(status, 200);
	assert_eq!(page["choiceId"], choice_id.as_str());
	assert_eq!(page["totalCount"], 3);
	assert_eq!(page["nextCursor"], 2);

	let items = page["items"].as_array().unwrap();
	assert_eq!(items.len(), 2);
	assert_eq!(items[0]["id"], 0);
	assert_eq!(items[0]["state"], "only-on-disk");
	assert_eq!(items[0]["path"], "src/ModA.luau");
	assert_eq!(items[0]["class"], "ModuleScript");
	assert_eq!(items[1]["id"], 1);
	assert_eq!(items[1]["state"], "differs");
	assert_eq!(items[1]["path"], "src/Hello.luau");

	let (status, tail) = get_json(
		&daemon,
		&format!("/choice/details?choiceId={choice_id}&cursor=2&limit=2"),
	)
	.await;
	assert_eq!(status, 200);
	assert!(tail.get("nextCursor").is_none());

	let items = tail["items"].as_array().unwrap();
	assert_eq!(items.len(), 1);
	assert_eq!(items[0]["id"], 2);
	assert_eq!(items[0]["state"], "missing-on-disk");
	// Studio-only entries have no disk backing
	assert_eq!(items[0]["path"], Value::Null);
	assert_eq!(items[0]["instancePath"], "ReplicatedStorage/StudioOnly");

	// Unknown choiceId → 404
	let (status, _) = get_json(&daemon, "/choice/details?choiceId=nope").await;
	assert_eq!(status, 404);

	// Selection chunking with receipts: stage items 0 and 2, not 1
	let (status, receipt) = post_json(
		&daemon,
		"/choice/selection",
		&json!({
			"choiceId": choice_id,
			"submissionId": "sel-1",
			"chunkIndex": 0,
			"finalChunk": false,
			"ids": [0],
		}),
	)
	.await;

	assert_eq!(status, 200);
	assert_eq!(receipt["ok"], true);
	assert_eq!(receipt["acceptedChunk"], 0);
	assert_eq!(receipt["nextChunk"], 1);
	assert_eq!(receipt["selectedCount"], 1);
	assert_eq!(receipt["committed"], false);

	// Wrong chunk index → 409
	let (status, _) = post_json(
		&daemon,
		"/choice/selection",
		&json!({
			"choiceId": choice_id,
			"submissionId": "sel-1",
			"chunkIndex": 0,
			"finalChunk": false,
			"ids": [2],
		}),
	)
	.await;
	assert_eq!(status, 409);

	// Out-of-range id → 400
	let (status, _) = post_json(
		&daemon,
		"/choice/selection",
		&json!({
			"choiceId": choice_id,
			"submissionId": "sel-1",
			"chunkIndex": 1,
			"finalChunk": false,
			"ids": [3],
		}),
	)
	.await;
	assert_eq!(status, 400);

	let (status, receipt) = post_json(
		&daemon,
		"/choice/selection",
		&json!({
			"choiceId": choice_id,
			"submissionId": "sel-1",
			"chunkIndex": 1,
			"finalChunk": true,
			"ids": [2],
		}),
	)
	.await;

	assert_eq!(status, 200);
	assert_eq!(receipt["selectedCount"], 2);
	assert_eq!(receipt["committed"], true);

	// Apply the staged subset toward Studio
	let (status, outcome) = post_json(&daemon, "/choice", &json!({ "choiceId": choice_id, "choice": "disk" })).await;
	assert_eq!(status, 200);
	assert_eq!(outcome["ok"], true);
	assert_eq!(outcome["applied"], true);

	// The plugin receives exactly the staged entries over the sync channel:
	// the ModA subtree as an addition and the Studio-only ref as a removal —
	// and no update for the unstaged Hello
	let sync = wait_for_frame(&mut plugin, Duration::from_secs(10), |frame| {
		frame["type"] == "sync"
			&& (!frame["additions"].as_array().unwrap().is_empty() || !frame["removals"].as_array().unwrap().is_empty())
	})
	.await
	.expect("no disk-apply sync frame");

	let additions = sync["additions"].as_array().unwrap();
	let removals = sync["removals"].as_array().unwrap();
	let updates = sync["updates"].as_array().unwrap();

	assert_eq!(additions.len(), 1);
	assert_eq!(additions[0]["id"], mod_a.as_str());
	assert_eq!(additions[0]["name"], "ModA");
	assert_eq!(additions[0]["properties"]["Source"]["String"], "return \"mod-a\"\n");
	assert_eq!(removals, &vec![json!(STUDIO_ONLY_REF)]);
	assert!(updates.is_empty(), "unstaged entries must be untouched");

	// choice-made broadcast, pending cleared, replays race-safe
	let event = wait_for_frame(&mut watch, Duration::from_secs(10), |frame| {
		frame["type"] == "event" && frame["topic"] == "choice-made"
	})
	.await
	.expect("no choice-made event");

	assert_eq!(event["choiceId"], choice_id.as_str());
	assert_eq!(event["choice"], "disk");

	let (_, pending) = get_json(&daemon, "/choice").await;
	assert_eq!(pending["pending"], false);

	// Resolved elsewhere: a second decision for the same choiceId → 409
	let (status, body) = post_json(&daemon, "/choice", &json!({ "choiceId": choice_id, "choice": "disk" })).await;
	assert_eq!(status, 409);
	assert_eq!(body["ok"], false);
	assert_eq!(body["error"], "resolved");
}

#[tokio::test]
async fn disk_choice_mode_all_replays_the_full_set() {
	let daemon = start_daemon(None);
	let mod_a = grow_second_module(&daemon).await;
	let hello = child_ref(&daemon, "Hello").await.unwrap();

	let (mut plugin, _) = connect_client(&daemon, "plugin", "all-plugin").await;

	let choice_id = commit_three_entry_set(&daemon, &mod_a, &hello, "sub-all").await;

	let (status, outcome) = post_json(
		&daemon,
		"/choice",
		&json!({ "choiceId": choice_id, "choice": "disk", "mode": "all" }),
	)
	.await;

	assert_eq!(status, 200);
	assert_eq!(outcome["applied"], true);

	// All three entries replay: addition, full-state update, removal
	let sync = wait_for_frame(&mut plugin, Duration::from_secs(10), |frame| {
		frame["type"] == "sync" && !frame["additions"].as_array().unwrap().is_empty()
	})
	.await
	.expect("no disk-apply sync frame");

	assert_eq!(sync["additions"].as_array().unwrap()[0]["id"], mod_a.as_str());

	let update = &sync["updates"].as_array().unwrap()[0];
	assert_eq!(update["id"], hello.as_str());
	assert_eq!(update["name"], "Hello");
	assert_eq!(update["properties"]["Source"]["String"], "return \"hello\"\n");

	assert_eq!(sync["removals"].as_array().unwrap()[0], json!(STUDIO_ONLY_REF));
}

#[tokio::test]
async fn cancel_and_studio_choices_record_without_applying() {
	let daemon = start_daemon(None);
	let mod_a = grow_second_module(&daemon).await;
	let hello = child_ref(&daemon, "Hello").await.unwrap();

	let (mut watch, _) = connect_client(&daemon, "watch", "record-watch").await;

	// Cancel clears the set and broadcasts
	let choice_id = commit_three_entry_set(&daemon, &mod_a, &hello, "sub-cancel").await;

	let (status, outcome) = post_json(
		&daemon,
		"/choice",
		&json!({ "choiceId": choice_id, "choice": "cancel" }),
	)
	.await;
	assert_eq!(status, 200);
	assert_eq!(outcome["ok"], true);
	assert_eq!(outcome["applied"], false);

	let event = wait_for_frame(&mut watch, Duration::from_secs(10), |frame| {
		frame["type"] == "event" && frame["topic"] == "choice-made" && frame["choiceId"] == choice_id.as_str()
	})
	.await
	.unwrap();
	assert_eq!(event["choice"], "cancel");

	let (_, pending) = get_json(&daemon, "/choice").await;
	assert_eq!(pending["pending"], false);

	// Keep Studio records the choice but never fakes the bulk push
	let choice_id = commit_three_entry_set(&daemon, &mod_a, &hello, "sub-studio").await;

	let (status, outcome) = post_json(
		&daemon,
		"/choice",
		&json!({ "choiceId": choice_id, "choice": "studio" }),
	)
	.await;
	assert_eq!(status, 200);
	assert_eq!(outcome["ok"], true);
	assert_eq!(outcome["applied"], false);
	assert_eq!(outcome["pendingApplication"], true);

	let event = wait_for_frame(&mut watch, Duration::from_secs(10), |frame| {
		frame["type"] == "event" && frame["topic"] == "choice-made" && frame["choiceId"] == choice_id.as_str()
	})
	.await
	.unwrap();
	assert_eq!(event["choice"], "studio");

	// The disk stays untouched: recording is not application
	assert_eq!(
		fs::read_to_string(daemon.src_dir().join("Hello.luau")).unwrap(),
		"return \"hello\"\n"
	);
}

#[tokio::test]
async fn plugin_disconnect_and_restart_invalidate_pending_sets() {
	let daemon = start_daemon(None);
	let mod_a = grow_second_module(&daemon).await;
	let hello = child_ref(&daemon, "Hello").await.unwrap();

	// A pending set dies with the plugin connection that produced it
	{
		let (plugin, _) = connect_client(&daemon, "plugin", "stale-plugin").await;
		let choice_id = commit_three_entry_set(&daemon, &mod_a, &hello, "sub-stale").await;

		drop(plugin);

		// Wait until the daemon noticed the disconnect
		let deadline = tokio::time::Instant::now() + Duration::from_secs(10);

		loop {
			let (_, pending) = get_json(&daemon, "/choice").await;

			if pending["pending"] == false {
				break;
			}

			assert!(tokio::time::Instant::now() < deadline, "set never invalidated");
			tokio::time::sleep(Duration::from_millis(100)).await;
		}

		let (status, _) = get_json(&daemon, &format!("/choice/details?choiceId={choice_id}")).await;
		assert_eq!(status, 404);

		let (status, body) = post_json(&daemon, "/choice", &json!({ "choiceId": choice_id, "choice": "disk" })).await;
		assert_eq!(status, 409);
		assert_eq!(body["error"], "resolved");
	}

	// A restart upload invalidates the previous pending set too
	let stale_choice = commit_three_entry_set(&daemon, &mod_a, &hello, "sub-a").await;

	let (status, receipt) = post_json(
		&daemon,
		"/compare",
		&json!({
			"submissionId": "sub-b",
			"chunkIndex": 0,
			"finalChunk": true,
			"restart": true,
			"entries": [],
		}),
	)
	.await;

	// An empty comparison commits clean: in sync, no choice minted
	assert_eq!(status, 200);
	assert_eq!(receipt["committed"], true);
	assert!(receipt.get("choiceId").is_none());

	let (status, _) = get_json(&daemon, &format!("/choice/details?choiceId={stale_choice}")).await;
	assert_eq!(status, 404);

	let (_, pending) = get_json(&daemon, "/choice").await;
	assert_eq!(pending["pending"], false);
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
