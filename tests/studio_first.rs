//! Studio-first sync (Design §7.0): a code-scope connect auto-applies
//! Studio → disk (fenced, backed up, promptless) and leaves the disk-side
//! entries behind as the passive `disk-review` set — no `choice-needed`, no
//! decision modal. Covers the fenced apply with overlay (disk-only and
//! out-of-projection files survive in place), `differs` preservation, the
//! review surface (`GET /review`, details paging, push by ids / mode all,
//! dismiss), persistence across a daemon restart, server-side dropping of
//! non-code compare entries, and the `scope` field on both hellos — while a
//! `"scope": "full"` project keeps the choice flow.

mod common;

use serde_json::{json, Value};
use std::{collections::HashMap, fs, time::Duration};

use common::{
	child_ref_in, connect_client, get_json, post_json, scratch_project_scoped, service_ref, source_claims,
	spawn_fake_plugin, start_daemon, start_daemon_in, wait_for_frame, FakePlugin, FakePluginScript, TestDaemon,
};

const SETTLE: Duration = Duration::from_millis(600);

/// Fresh refs for instances that exist only in the fake Studio DataModel
const SYS_REF: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa1";
const EMPTY_REF: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa2";
const BOOT_REF: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa3";
const UI_REF: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa4";
const MAIN_REF: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa5";
const PART_REF: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa6";
const STUDIO_ONLY_REF: &str = "ffffffffffffffffffffffffffffffff";

const DISK_HELLO: &str = "return \"disk hello\"\n";
const STUDIO_HELLO: &str = "return \"studio hello\"\n";
const KEEP_ME: &str = "return \"keep me\"\n";
const NOTES: &str = "# notes\n";
const BOOT_SOURCE: &str = "print(\"boot\")\n";
const UI_SOURCE: &str = "print(\"ui\")\n";
const MAIN_SOURCE: &str = "print(\"main\")\n";

fn script_node(reference: &str, name: &str, class: &str) -> Value {
	// Script sources are elided from the structure per the pinned contract
	json!({ "id": reference, "name": name, "class": class, "properties": {}, "children": [] })
}

/// A code-scope place (the `scope` field is ABSENT — proving the default)
/// with two mapped services under `src/<Service>`, a `differs` seed, a
/// disk-only module and an out-of-projection markdown file
fn code_place() -> TestDaemon {
	let dir = scratch_project_scoped(
		json!({
			"$className": "DataModel",
			"ReplicatedStorage": { "$path": "src/ReplicatedStorage" },
			"ServerScriptService": { "$path": "src/ServerScriptService" },
		}),
		&[
			("src/ReplicatedStorage/Hello.luau", DISK_HELLO),
			("src/ReplicatedStorage/KeepMe.luau", KEEP_ME),
			("src/ReplicatedStorage/notes.md", NOTES),
		],
		None,
	);

	// ServerScriptService starts empty on disk: everything in it is
	// Studio-only
	fs::create_dir_all(dir.path().join("src/ServerScriptService")).unwrap();

	start_daemon_in(dir)
}

/// The populated fake Studio DataModel: services, nested folders (including
/// an EMPTY one), a Script, a LocalScript and a ModuleScript — everything a
/// code-scope place is made of. `KeepMe` and `notes.md` exist only on disk
async fn studio_data_model(daemon: &TestDaemon) -> (FakePlugin, String, String) {
	let hello = child_ref_in(daemon, "ReplicatedStorage", "Hello").await.unwrap();
	let rs_root = service_ref(daemon, "ReplicatedStorage").await.unwrap();
	let sss_root = service_ref(daemon, "ServerScriptService").await.unwrap();

	let rs_sources = HashMap::from([
		(hello.clone(), STUDIO_HELLO.as_bytes().to_vec()),
		(BOOT_REF.to_owned(), BOOT_SOURCE.as_bytes().to_vec()),
		(UI_REF.to_owned(), UI_SOURCE.as_bytes().to_vec()),
	]);
	let sss_sources = HashMap::from([(MAIN_REF.to_owned(), MAIN_SOURCE.as_bytes().to_vec())]);

	let rs_subtree = json!({
		"id": rs_root,
		"name": "ReplicatedStorage",
		"class": "ReplicatedStorage",
		"properties": {},
		"children": [
			script_node(&hello, "Hello", "ModuleScript"),
			{
				"id": SYS_REF,
				"name": "Systems",
				"class": "Folder",
				"properties": {},
				"children": [
					// Attributes on an otherwise-empty folder: the apply must
					// land them in an `init.meta.json` sidecar or every
					// reconnect re-flags this folder forever
					{ "id": EMPTY_REF, "name": "Empty", "class": "Folder",
						"properties": { "Attributes": { "Attributes": { "Done": { "Bool": false }, "Progress": { "Float64": 0.0 } } } },
						"children": [] },
					script_node(BOOT_REF, "Boot", "Script"),
				],
			},
			script_node(UI_REF, "UI", "LocalScript"),
		],
		"sources": source_claims(&rs_sources),
	});

	let sss_subtree = json!({
		"id": sss_root,
		"name": "ServerScriptService",
		"class": "ServerScriptService",
		"properties": {},
		"children": [script_node(MAIN_REF, "Main", "Script")],
		"sources": source_claims(&sss_sources),
	});

	let mut sources = rs_sources;
	sources.extend(sss_sources);

	let plugin = spawn_fake_plugin(
		daemon,
		"studio-first-plugin",
		FakePluginScript {
			subtrees: HashMap::from([(rs_root.clone(), rs_subtree), (sss_root.clone(), sss_subtree)]),
			sources,
			..FakePluginScript::default()
		},
	)
	.await;

	(plugin, hello, rs_root)
}

/// The compare upload a code-scope plugin would send after hydrate+diff:
/// disk-only → `add`, differs → `update`, Studio-only subtree roots →
/// `remove` — plus one non-code-class entry the daemon must drop server-side
fn compare_entries(keep_me: &str, hello: &str) -> Value {
	json!([
		{ "ref": keep_me, "change": "add", "class": "ModuleScript", "name": "KeepMe", "instancePath": "ReplicatedStorage/KeepMe" },
		{ "ref": hello, "change": "update", "class": "ModuleScript", "name": "Hello", "instancePath": "ReplicatedStorage/Hello" },
		{ "ref": SYS_REF, "change": "remove", "class": "Folder", "name": "Systems", "instancePath": "ReplicatedStorage/Systems" },
		{ "ref": UI_REF, "change": "remove", "class": "LocalScript", "name": "UI", "instancePath": "ReplicatedStorage/UI" },
		{ "ref": MAIN_REF, "change": "remove", "class": "Script", "name": "Main", "instancePath": "ServerScriptService/Main" },
		// Non-code class: dropped with a debug log, never an error — if it
		// were kept, the apply would fail trying to pull a "Workspace" root
		{ "ref": PART_REF, "change": "remove", "class": "Part", "name": "Junk", "instancePath": "Workspace/Junk" },
	])
}

#[tokio::test]
async fn studio_first_connect_applies_and_leaves_a_disk_review() {
	let daemon = code_place();
	tokio::time::sleep(SETTLE).await;

	// The absent scope field defaults to code and both hellos report it
	let (_, hello_body) = get_json(&daemon, "/hello").await;
	assert_eq!(hello_body["scope"], "code");

	let (mut watch, watch_hello) = connect_client(&daemon, "watch", "studio-first-watch").await;
	assert_eq!(watch_hello["scope"], "code");

	// Code scope keeps out-of-projection files out of the tree entirely
	assert!(child_ref_in(&daemon, "ReplicatedStorage", "notes").await.is_none());

	let keep_me = child_ref_in(&daemon, "ReplicatedStorage", "KeepMe").await.unwrap();
	let (plugin, hello, _) = studio_data_model(&daemon).await;

	// One final compare chunk commits the whole set
	let (status, receipt) = post_json(
		&daemon,
		"/compare",
		&json!({
			"submissionId": "sub-studio-first",
			"chunkIndex": 0,
			"finalChunk": true,
			"entries": compare_entries(&keep_me, &hello),
		}),
	)
	.await;

	assert_eq!(status, 200);
	assert_eq!(receipt["ok"], true, "receipt: {receipt}");
	assert_eq!(receipt["committed"], true);
	assert_eq!(receipt["applied"], true);

	assert_eq!(receipt["direction"], "studio");

	// Actual .luau files and directories landed under src/<Service>/…
	let rs = daemon.root.join("src/ReplicatedStorage");
	let sss = daemon.root.join("src/ServerScriptService");

	assert_eq!(fs::read_to_string(rs.join("Hello.luau")).unwrap(), STUDIO_HELLO);
	assert!(rs.join("Systems").is_dir());
	assert_eq!(
		fs::read_to_string(rs.join("Systems").join("Boot.server.luau")).unwrap(),
		BOOT_SOURCE
	);
	// A LocalScript lands as `.local.luau` — `.client.luau` now means a
	// `Script` with `RunContext = Client` (the suffix scheme, one meaning each)
	assert_eq!(fs::read_to_string(rs.join("UI.local.luau")).unwrap(), UI_SOURCE);
	assert_eq!(fs::read_to_string(sss.join("Main.server.luau")).unwrap(), MAIN_SOURCE);

	// The EMPTY folder became a directory (folders sync even empty), and its
	// attributes landed in the data sidecar — the code-scope round trip that
	// keeps the connect-time review from re-flagging it forever
	let empty = rs.join("Systems").join("Empty");
	assert!(empty.is_dir(), "empty Studio folder must land as a directory");
	let sidecar = fs::read_dir(&empty)
		.unwrap()
		.map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
		.collect::<Vec<_>>();
	assert_eq!(sidecar, vec!["init.meta.json".to_string()], "attributes need a sidecar");
	let data: serde_json::Value =
		serde_json::from_str(&fs::read_to_string(empty.join("init.meta.json")).unwrap()).unwrap();
	assert_eq!(data["properties"]["Attributes"]["Done"], false, "sidecar: {data}");

	// A disk-only file inside the projection is a divergence Studio does not
	// have, so it leaves for the backlog rather than lingering on disk
	assert!(
		!rs.join("KeepMe.luau").exists(),
		"a disk-only file must not survive a Studio-first apply"
	);

	// A file outside the projection is not WSync's to move
	assert_eq!(fs::read_to_string(rs.join("notes.md")).unwrap(), NOTES);

	// Both losers are recoverable: the replaced original and the disk-only file
	let (_, backlog) = get_json(&daemon, "/backlog").await;
	let entries = backlog["entries"].as_array().unwrap();

	assert_eq!(entries.len(), 2, "backlog: {backlog}");

	let paths: Vec<&str> = entries.iter().map(|entry| entry["path"].as_str().unwrap()).collect();

	assert!(paths.contains(&"src/ReplicatedStorage/Hello.luau"), "paths: {paths:?}");
	assert!(paths.contains(&"src/ReplicatedStorage/KeepMe.luau"), "paths: {paths:?}");

	for entry in entries {
		assert_eq!(entry["reason"], "initial-sync");
		assert!(entry["secondsRemaining"].as_u64().unwrap() > 0);
	}

	// The replaced disk original is the content that was there before
	let stored = daemon
		.root
		.join(".wsync-backups")
		.join("backlog")
		.join(entries.iter().find(|entry| entry["path"] == "src/ReplicatedStorage/Hello.luau").unwrap()["id"].as_str().unwrap())
		.join("src/ReplicatedStorage/Hello.luau");

	assert_eq!(fs::read_to_string(&stored).unwrap(), DISK_HELLO);

	// The pull ran per staged root over the op surface
	let events = plugin.events();
	assert_eq!(
		events.iter().filter(|event| *event == "read_subtree").count(),
		2,
		"one structure pull per staged root: {events:?}"
	);

	// A backlog broadcast with exact counts — and never a question
	let event = wait_for_frame(&mut watch, Duration::from_secs(10), |frame| {
		frame["type"] == "event" && (frame["topic"] == "backlog" || frame["topic"] == "choice-needed")
	})
	.await
	.expect("no backlog event");

	assert_eq!(event["topic"], "backlog", "sync must never ask");
	assert_eq!(event["total"], 2);
	assert_eq!(event["added"], 2);
}



#[tokio::test]
async fn a_daemon_restart_keeps_the_pending_review_answerable() {
	let daemon = code_place();
	tokio::time::sleep(SETTLE).await;

	let keep_me = child_ref_in(&daemon, "ReplicatedStorage", "KeepMe").await.unwrap();
	let (plugin, hello, _) = studio_data_model(&daemon).await;

	let (_, receipt) = post_json(
		&daemon,
		"/compare",
		&json!({
			"submissionId": "sub-restart",
			"chunkIndex": 0,
			"finalChunk": true,
			"entries": compare_entries(&keep_me, &hello),
		}),
	)
	.await;

	assert_eq!(receipt["backlogged"], 2, "receipt: {receipt}");

	plugin.abort();

	// Stop the daemon gracefully (unmanaged daemons accept an empty body)
	let (status, _) = reqwest::Client::new()
		.post(daemon.http("/stop"))
		.body("")
		.send()
		.await
		.map(|response| (response.status(), ()))
		.unwrap();
	assert_eq!(status, 200);
	assert!(daemon.wait_for_exit(Duration::from_secs(15)), "daemon never exited");

	// Reboot over the same workspace: the persisted index and the stored bytes
	// keep the backlog answerable, so a restart is never what loses the edit
	let TestDaemon { dir, .. } = daemon;
	let daemon = start_daemon_in(dir);
	tokio::time::sleep(SETTLE).await;

	let (status, backlog) = get_json(&daemon, "/backlog").await;

	assert_eq!(status, 200);

	let entries = backlog["entries"].as_array().unwrap();

	assert_eq!(entries.len(), 2, "the backlog must survive a restart: {backlog}");

	// Restoring still needs a live sync channel: without a plugin it refuses
	// rather than leaving the file on disk for the next connect to send back
	let id = entries[0]["id"].as_str().unwrap().to_owned();
	let (status, body) = post_json(&daemon, "/backlog/restore", &json!({ "id": id })).await;

	assert_eq!(status, 503, "restore: {body}");
	assert_eq!(body["ok"], false);

	// Dropping needs nothing but the daemon
	let (status, body) = post_json(&daemon, "/backlog/drop", &json!({ "id": id })).await;

	assert_eq!(status, 200, "drop: {body}");
	assert_eq!(body["dropped"], 1);

	let (_, backlog) = get_json(&daemon, "/backlog").await;
	assert_eq!(backlog["entries"].as_array().unwrap().len(), 1);
}

