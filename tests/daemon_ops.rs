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


