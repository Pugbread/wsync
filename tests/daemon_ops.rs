//! Daemon-side op tests (registry contracts `path`, `meta`, `where`): the
//! request router answers these from the live tree without any plugin, and
//! `GET /choice/source` serves both sides of a pending divergence row.

mod common;

use serde_json::{json, Value};
use std::{collections::HashMap, fs, time::Duration};

use common::{child_ref_in, get_json, post_json, spawn_fake_plugin, start_daemon, FakePluginScript, TestDaemon};

const SETTLE: Duration = Duration::from_millis(600);

/// A well-formed ref no daemon-side instance ever has
const STUDIO_ONLY_REF: &str = "ffffffffffffffffffffffffffffffff";
const STUDIO_ONLY_REF_2: &str = "fffffffffffffffffffffffffffffffe";

async fn request(daemon: &TestDaemon, op: &str, args: Value) -> (reqwest::StatusCode, Value) {
	post_json(daemon, "/request", &json!({ "op": op, "args": args })).await
}

#[tokio::test]
async fn path_meta_and_where_answer_without_a_plugin() {
	let daemon = start_daemon(None);
	tokio::time::sleep(Duration::from_millis(100)).await;

	// path: studio-path target
	let (status, reply) = request(&daemon, "path", json!({ "target": "ReplicatedStorage/Hello" })).await;
	assert_eq!(status, 200);
	assert_eq!(reply["ok"], true, "daemon ops must answer with no plugin: {reply}");
	assert_eq!(reply["value"]["studioPath"], "ReplicatedStorage/Hello");
	assert_eq!(reply["value"]["fsPaths"], json!(["src/Hello.luau"]));
	assert_eq!(reply["value"]["kind"], "file");
	assert_eq!(reply["meta"]["op"], "path");

	// path: explicit fs resolution
	let (_, reply) = request(&daemon, "path", json!({ "target": "src/Hello.luau", "from": "fs" })).await;
	assert_eq!(reply["ok"], true);
	assert_eq!(reply["value"]["studioPath"], "ReplicatedStorage/Hello");
	assert_eq!(reply["value"]["kind"], "file");

	// path: auto falls through studio → fs
	let (_, reply) = request(&daemon, "path", json!({ "target": "src/Hello.luau" })).await;
	assert_eq!(reply["ok"], true);
	assert_eq!(reply["value"]["studioPath"], "ReplicatedStorage/Hello");

	// path: a directory-backed root
	let (_, reply) = request(&daemon, "path", json!({ "target": "ReplicatedStorage" })).await;
	assert_eq!(reply["ok"], true);
	assert_eq!(reply["value"]["kind"], "dir");
	assert!(
		reply["value"]["fsPaths"]
			.as_array()
			.unwrap()
			.iter()
			.any(|path| path == "src"),
		"root fsPaths should include the $path dir: {reply}"
	);

	// path: not projected → NOT_FOUND with a Studio-only note
	let (status, reply) = request(&daemon, "path", json!({ "target": "ReplicatedStorage/Nope" })).await;
	assert_eq!(status, 200);
	assert_eq!(reply["ok"], false);
	assert_eq!(reply["error"]["code"], "NOT_FOUND");
	assert!(
		reply["error"]["message"].as_str().unwrap().contains("Studio-only"),
		"message should note the target may be Studio-only: {reply}"
	);

	// path: bad from → INVALID_ARGUMENT
	let (_, reply) = request(&daemon, "path", json!({ "target": "x", "from": "disk" })).await;
	assert_eq!(reply["error"]["code"], "INVALID_ARGUMENT");

	// path: missing target → INVALID_ARGUMENT
	let (_, reply) = request(&daemon, "path", json!({})).await;
	assert_eq!(reply["error"]["code"], "INVALID_ARGUMENT");

	// meta
	let (_, reply) = request(&daemon, "meta", json!({ "target": "ReplicatedStorage/Hello" })).await;
	assert_eq!(reply["ok"], true);
	assert_eq!(reply["value"]["instancePath"], "ReplicatedStorage/Hello");
	assert_eq!(reply["value"]["class"], "ModuleScript");
	assert_eq!(reply["value"]["sourcePaths"], json!(["src/Hello.luau"]));
	assert_eq!(reply["value"]["middleware"], "ModuleScript");

	// meta: services keep unknown children by default
	let (_, reply) = request(&daemon, "meta", json!({ "target": "ReplicatedStorage" })).await;
	assert_eq!(reply["ok"], true);
	assert_eq!(reply["value"]["class"], "ReplicatedStorage");
	assert_eq!(reply["value"]["keepUnknowns"], true);

	// where: substring match with fs resolution
	let (_, reply) = request(&daemon, "where", json!({ "target": "hell" })).await;
	assert_eq!(reply["ok"], true);
	assert_eq!(reply["value"]["truncated"], false);

	let matches = reply["value"]["matches"].as_array().unwrap();
	assert_eq!(matches.len(), 1);
	assert_eq!(matches[0]["instancePath"], "ReplicatedStorage/Hello");
	assert_eq!(matches[0]["fsPath"], "src/Hello.luau");

	// where: scoped under a subtree
	let (_, reply) = request(
		&daemon,
		"where",
		json!({ "target": "Hello", "under": "ReplicatedStorage" }),
	)
	.await;
	assert_eq!(reply["ok"], true);
	assert_eq!(reply["value"]["matches"].as_array().unwrap().len(), 1);

	// where: unknown scope → NOT_FOUND
	let (_, reply) = request(&daemon, "where", json!({ "target": "x", "under": "Nope" })).await;
	assert_eq!(reply["error"]["code"], "NOT_FOUND");

	// where: no matches is an empty result, not an error
	let (_, reply) = request(&daemon, "where", json!({ "target": "zzz-not-there" })).await;
	assert_eq!(reply["ok"], true);
	assert_eq!(reply["value"]["matches"], json!([]));

	// Non-daemon ops still require the plugin
	let (_, reply) = request(&daemon, "get", json!({ "path": "ReplicatedStorage/Hello" })).await;
	assert_eq!(reply["ok"], false);
	assert_eq!(reply["error"]["code"], "PLUGIN_ERROR");
}

/// Uploads a four-row divergence set: add (script), update (script), remove
/// (folder — not script-backed), remove (script)
async fn commit_source_set(daemon: &TestDaemon, mod_a: &str, hello: &str) -> String {
	let (status, receipt) = post_json(
		daemon,
		"/compare",
		&json!({
			"submissionId": "sub-source",
			"chunkIndex": 0,
			"finalChunk": true,
			"entries": [
				{ "ref": mod_a, "change": "add", "class": "ModuleScript", "name": "ModA", "instancePath": "ReplicatedStorage/ModA" },
				{ "ref": hello, "change": "update", "class": "ModuleScript", "name": "Hello", "instancePath": "ReplicatedStorage/Hello" },
				{ "ref": STUDIO_ONLY_REF, "change": "remove", "class": "Folder", "name": "StudioOnly", "instancePath": "ReplicatedStorage/StudioOnly" },
				{ "ref": STUDIO_ONLY_REF_2, "change": "remove", "class": "ModuleScript", "name": "GoneScript", "instancePath": "ReplicatedStorage/GoneScript" },
			],
		}),
	)
	.await;

	assert_eq!(status, 200);
	assert_eq!(receipt["committed"], true);

	receipt["choiceId"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn choice_source_serves_both_sides_with_truncation_and_errors() {
	let daemon = start_daemon(None);

	// Grow a second module whose disk source is oversized (> 256 KiB) so the
	// disk side truncates
	tokio::time::sleep(SETTLE).await;

	let big_disk = format!("-- disk filler\n{}", "d".repeat(300 * 1024));
	fs::write(daemon.src_dir().join("ModA.luau"), &big_disk).unwrap();

	let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
	let mod_a = loop {
		if let Some(id) = child_ref_in(&daemon, "ReplicatedStorage", "ModA").await {
			break id;
		}

		assert!(tokio::time::Instant::now() < deadline, "ModA never appeared");
		tokio::time::sleep(Duration::from_millis(100)).await;
	};

	let hello = child_ref_in(&daemon, "ReplicatedStorage", "Hello").await.unwrap();

	// The fake plugin answers the bounded `get` op for the Studio side; the
	// Studio-side Hello source is oversized to prove that side truncates too
	let big_studio = format!("-- studio filler\n{}", "s".repeat(300 * 1024));

	let plugin = spawn_fake_plugin(
		&daemon,
		"source-plugin",
		FakePluginScript {
			get_sources: HashMap::from([
				("ReplicatedStorage/Hello".to_owned(), big_studio.clone()),
				(
					"ReplicatedStorage/GoneScript".to_owned(),
					"return \"gone\"\n".to_owned(),
				),
			]),
			..FakePluginScript::default()
		},
	)
	.await;

	let choice_id = commit_source_set(&daemon, &mod_a, &hello).await;

	// only-on-disk: disk present (truncated), studio absent — no plugin call
	let (status, row) = get_json(&daemon, &format!("/choice/source?choiceId={choice_id}&id=0")).await;
	assert_eq!(status, 200);
	assert_eq!(row["id"], 0);
	assert_eq!(row["path"], "src/ModA.luau");
	assert_eq!(row["instancePath"], "ReplicatedStorage/ModA");
	assert_eq!(row["state"], "only-on-disk");
	assert_eq!(row["disk"]["present"], true);
	assert_eq!(row["disk"]["truncated"], true);
	assert_eq!(row["disk"]["source"].as_str().unwrap().len(), 256 * 1024);
	assert!(row["disk"]["source"].as_str().unwrap().starts_with("-- disk filler"));
	assert_eq!(row["studio"]["present"], false);
	assert!(row["studio"].get("source").is_none());

	// differs: both sides, studio side truncated
	let (status, row) = get_json(&daemon, &format!("/choice/source?choiceId={choice_id}&id=1")).await;
	assert_eq!(status, 200);
	assert_eq!(row["state"], "differs");
	assert_eq!(row["disk"]["present"], true);
	assert_eq!(row["disk"]["source"], "return \"hello\"\n");
	assert!(row["disk"].get("truncated").is_none());
	assert_eq!(row["studio"]["present"], true);
	assert_eq!(row["studio"]["truncated"], true);
	assert_eq!(row["studio"]["source"].as_str().unwrap().len(), 256 * 1024);
	assert!(row["studio"]["source"]
		.as_str()
		.unwrap()
		.starts_with("-- studio filler"));

	// missing-on-disk (script): disk absent, studio fetched live
	let (status, row) = get_json(&daemon, &format!("/choice/source?choiceId={choice_id}&id=3")).await;
	assert_eq!(status, 200);
	assert_eq!(row["state"], "missing-on-disk");
	assert_eq!(row["disk"]["present"], false);
	assert_eq!(row["studio"]["present"], true);
	assert_eq!(row["studio"]["source"], "return \"gone\"\n");

	// Non-script row → 400
	let (status, body) = get_json(&daemon, &format!("/choice/source?choiceId={choice_id}&id=2")).await;
	assert_eq!(status, 400);
	assert_eq!(body["ok"], false);

	// Unknown row id → 404; stale choiceId → 404
	let (status, _) = get_json(&daemon, &format!("/choice/source?choiceId={choice_id}&id=99")).await;
	assert_eq!(status, 404);

	let (status, _) = get_json(&daemon, "/choice/source?choiceId=nope&id=0").await;
	assert_eq!(status, 404);

	plugin.abort();
}

#[tokio::test]
async fn choice_source_without_a_plugin_is_unavailable() {
	let daemon = start_daemon(None);
	tokio::time::sleep(Duration::from_millis(200)).await;

	let hello = child_ref_in(&daemon, "ReplicatedStorage", "Hello").await.unwrap();

	// The comparison itself arrives over HTTP, so a set can exist with no
	// plugin connection at all
	let (status, receipt) = post_json(
		&daemon,
		"/compare",
		&json!({
			"submissionId": "sub-503",
			"chunkIndex": 0,
			"finalChunk": true,
			"entries": [
				{ "ref": hello, "change": "update", "class": "ModuleScript", "name": "Hello", "instancePath": "ReplicatedStorage/Hello" },
			],
		}),
	)
	.await;

	assert_eq!(status, 200);
	let choice_id = receipt["choiceId"].as_str().unwrap();

	// The studio side needs the live plugin → 503
	let (status, body) = get_json(&daemon, &format!("/choice/source?choiceId={choice_id}&id=0")).await;
	assert_eq!(status, 503);
	assert_eq!(body["ok"], false);
	assert!(body["error"].as_str().unwrap().contains("plugin"));

	// No pending set at all → 404
	let (_, _) = post_json(
		&daemon,
		"/choice",
		&json!({ "choiceId": choice_id, "choice": "cancel" }),
	)
	.await;

	let (status, _) = get_json(&daemon, &format!("/choice/source?choiceId={choice_id}&id=0")).await;
	assert_eq!(status, 404);
}
