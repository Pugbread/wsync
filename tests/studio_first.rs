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

	let review_id = receipt["reviewId"].as_str().expect("reviewId in receipt").to_owned();

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

	// Disk-only and out-of-projection files survived IN PLACE (the staged
	// swap overlays them forward instead of displacing them into backup)
	assert_eq!(fs::read_to_string(rs.join("KeepMe.luau")).unwrap(), KEEP_ME);
	assert_eq!(fs::read_to_string(rs.join("notes.md")).unwrap(), NOTES);

	// The differs disk original was preserved into the review store
	let preserved = daemon
		.root
		.join(".wsync-backups")
		.join("review")
		.join(&review_id)
		.join("src/ReplicatedStorage/Hello.luau");
	assert_eq!(fs::read_to_string(&preserved).unwrap(), DISK_HELLO);

	// The pull ran per staged root over the op surface
	let events = plugin.events();
	assert_eq!(
		events.iter().filter(|event| *event == "read_subtree").count(),
		2,
		"one structure pull per staged root: {events:?}"
	);

	// disk-review broadcast with exact counts — and never a choice-needed
	let event = wait_for_frame(&mut watch, Duration::from_secs(10), |frame| {
		frame["type"] == "event" && (frame["topic"] == "disk-review" || frame["topic"] == "choice-needed")
	})
	.await
	.expect("no disk-review event");

	assert_eq!(event["topic"], "disk-review", "code scope must never ask");
	assert_eq!(event["reviewId"], review_id.as_str());
	assert_eq!(event["total"], 2);
	assert_eq!(event["diskOnly"], 1);
	assert_eq!(event["differs"], 1);

	// The review surface: status, details shape, and the silent choice side
	let (status, pending) = get_json(&daemon, "/review").await;
	assert_eq!(status, 200);
	assert_eq!(pending["pending"], true);
	assert_eq!(pending["reviewId"], review_id.as_str());
	assert_eq!(pending["stats"]["total"], 2);
	assert_eq!(pending["stats"]["diskOnly"], 1);
	assert_eq!(pending["stats"]["differs"], 1);

	let (status, details) = get_json(&daemon, &format!("/review/details?reviewId={review_id}")).await;
	assert_eq!(status, 200);
	assert_eq!(details["reviewId"], review_id.as_str());
	assert_eq!(details["totalCount"], 2);

	let items = details["items"].as_array().unwrap();
	assert_eq!(items.len(), 2);
	assert_eq!(items[0]["id"], 0);
	assert_eq!(items[0]["path"], "src/ReplicatedStorage/KeepMe.luau");
	assert_eq!(items[0]["instancePath"], "ReplicatedStorage/KeepMe");
	assert_eq!(items[0]["state"], "disk-only");
	assert_eq!(items[0]["class"], "ModuleScript");
	assert_eq!(items[1]["id"], 1);
	assert_eq!(items[1]["path"], "src/ReplicatedStorage/Hello.luau");
	assert_eq!(items[1]["state"], "differs");

	// Details page with limit 1 → cursor to the next id
	let (_, page) = get_json(&daemon, &format!("/review/details?reviewId={review_id}&limit=1")).await;
	assert_eq!(page["items"].as_array().unwrap().len(), 1);
	assert_eq!(page["nextCursor"], 1);

	// Code-scope projects answer the old choice surface with nothing pending
	let (_, choice) = get_json(&daemon, "/choice").await;
	assert_eq!(choice["pending"], false);

	// Stale review ids → 404
	let (status, _) = get_json(&daemon, "/review/details?reviewId=nope").await;
	assert_eq!(status, 404);

	// Push the disk-only entry alone (id 0): the module is created in Studio
	// from the live file
	let (status, outcome) = post_json(&daemon, "/review/push", &json!({ "reviewId": review_id, "ids": [0] })).await;
	assert_eq!(status, 200);
	assert_eq!(outcome["ok"], true, "push outcome: {outcome}");
	assert_eq!(outcome["pushed"], 1);
	assert_eq!(outcome["remaining"], 1);

	let sync = plugin
		.wait_for(Duration::from_secs(10), |frame| {
			frame["type"] == "sync"
				&& frame["additions"]
					.as_array()
					.is_some_and(|additions| additions.iter().any(|addition| addition["name"] == "KeepMe"))
		})
		.await
		.expect("no sync frame creating the disk-only module");

	let addition = sync["additions"]
		.as_array()
		.unwrap()
		.iter()
		.find(|addition| addition["name"] == "KeepMe")
		.unwrap();
	assert_eq!(addition["properties"]["Source"]["String"], KEEP_ME);

	// Item ids are STABLE for the lifetime of the review: the survivor keeps
	// its original id 1 — never renumbered to fill the gap — so ids picked
	// before a partial push stay valid across chunked pushes
	let (_, details) = get_json(&daemon, &format!("/review/details?reviewId={review_id}")).await;
	assert_eq!(details["totalCount"], 1);

	let survivors = details["items"].as_array().unwrap();
	assert_eq!(survivors.len(), 1);
	assert_eq!(survivors[0]["id"], 1, "surviving entries keep their original ids");
	assert_eq!(survivors[0]["state"], "differs");

	let (_, pending) = get_json(&daemon, "/review").await;
	assert_eq!(pending["stats"]["total"], 1);
	assert_eq!(pending["stats"]["diskOnly"], 0);

	// A renumbering server would now reject (or misroute) the originally
	// picked id 1; pushing it lands on the differs entry: the preserved disk
	// copy goes to Studio AND is restored to the live disk
	let (status, outcome) = post_json(&daemon, "/review/push", &json!({ "reviewId": review_id, "ids": [1] })).await;
	assert_eq!(status, 200);
	assert_eq!(outcome["pushed"], 1);
	assert_eq!(outcome["remaining"], 0);

	let sync = plugin
		.wait_for(Duration::from_secs(10), |frame| {
			frame["type"] == "sync"
				&& frame["updates"].as_array().is_some_and(|updates| {
					updates
						.iter()
						.any(|update| update["properties"]["Source"]["String"] == DISK_HELLO)
				})
		})
		.await
		.expect("no sync frame carrying the preserved disk content");

	let update = sync["updates"]
		.as_array()
		.unwrap()
		.iter()
		.find(|update| update["properties"]["Source"]["String"] == DISK_HELLO)
		.unwrap();
	assert_eq!(update["id"], hello.as_str());

	assert_eq!(
		fs::read_to_string(rs.join("Hello.luau")).unwrap(),
		DISK_HELLO,
		"the preserved disk copy must be restored to the live disk"
	);
	assert!(!preserved.exists(), "the consumed preserved copy is deleted");

	// The review is spent: no pending set, preserved store gone
	let (_, pending) = get_json(&daemon, "/review").await;
	assert_eq!(pending["pending"], false);
	assert!(!daemon.root.join(".wsync-backups").join("review").exists());

	// A repeated push against the spent review → 404
	let (status, _) = post_json(
		&daemon,
		"/review/push",
		&json!({ "reviewId": review_id, "mode": "all" }),
	)
	.await;
	assert_eq!(status, 404);

	plugin.abort();
}

#[tokio::test]
async fn push_mode_all_consumes_the_whole_review() {
	let daemon = code_place();
	tokio::time::sleep(SETTLE).await;

	let keep_me = child_ref_in(&daemon, "ReplicatedStorage", "KeepMe").await.unwrap();
	let (plugin, hello, _) = studio_data_model(&daemon).await;

	let (_, receipt) = post_json(
		&daemon,
		"/compare",
		&json!({
			"submissionId": "sub-all",
			"chunkIndex": 0,
			"finalChunk": true,
			"entries": [
				{ "ref": keep_me, "change": "add", "class": "ModuleScript", "name": "KeepMe", "instancePath": "ReplicatedStorage/KeepMe" },
				{ "ref": hello, "change": "update", "class": "ModuleScript", "name": "Hello", "instancePath": "ReplicatedStorage/Hello" },
			],
		}),
	)
	.await;

	let review_id = receipt["reviewId"].as_str().unwrap().to_owned();

	let (status, outcome) = post_json(
		&daemon,
		"/review/push",
		&json!({ "reviewId": review_id, "mode": "all" }),
	)
	.await;
	assert_eq!(status, 200);
	assert_eq!(outcome["ok"], true, "push outcome: {outcome}");
	assert_eq!(outcome["pushed"], 2);
	assert_eq!(outcome["remaining"], 0);

	// Both directions travel the sync channel: create the disk-only module,
	// restore-and-push the preserved differs content
	plugin
		.wait_for(Duration::from_secs(10), |frame| {
			frame["type"] == "sync"
				&& frame["additions"]
					.as_array()
					.is_some_and(|additions| additions.iter().any(|addition| addition["name"] == "KeepMe"))
		})
		.await
		.expect("no sync frame creating the disk-only module");

	plugin
		.wait_for(Duration::from_secs(10), |frame| {
			frame["type"] == "sync"
				&& frame["updates"].as_array().is_some_and(|updates| {
					updates
						.iter()
						.any(|update| update["properties"]["Source"]["String"] == DISK_HELLO)
				})
		})
		.await
		.expect("no sync frame restoring the differs entry");

	assert_eq!(
		fs::read_to_string(daemon.root.join("src/ReplicatedStorage/Hello.luau")).unwrap(),
		DISK_HELLO
	);

	let (_, pending) = get_json(&daemon, "/review").await;
	assert_eq!(pending["pending"], false);

	plugin.abort();
}

#[tokio::test]
async fn new_comparisons_replace_reviews_and_dismiss_deletes_preserved_copies() {
	let daemon = code_place();
	tokio::time::sleep(SETTLE).await;

	let keep_me = child_ref_in(&daemon, "ReplicatedStorage", "KeepMe").await.unwrap();
	let (plugin, hello, _) = studio_data_model(&daemon).await;

	// First comparison leaves a review pending
	let (_, receipt) = post_json(
		&daemon,
		"/compare",
		&json!({
			"submissionId": "sub-first",
			"chunkIndex": 0,
			"finalChunk": true,
			"entries": compare_entries(&keep_me, &hello),
		}),
	)
	.await;

	let first_review = receipt["reviewId"].as_str().unwrap().to_owned();

	let preserved = daemon
		.root
		.join(".wsync-backups")
		.join("review")
		.join(&first_review)
		.join("src/ReplicatedStorage/Hello.luau");
	assert!(preserved.exists());

	// An empty (clean) comparison replaces it: committed silently, nothing
	// pending, the old preserved copies deleted
	let (status, receipt) = post_json(
		&daemon,
		"/compare",
		&json!({
			"submissionId": "sub-clean",
			"chunkIndex": 0,
			"finalChunk": true,
			"restart": true,
			"entries": [],
		}),
	)
	.await;

	assert_eq!(status, 200);
	assert_eq!(receipt["committed"], true);
	assert!(receipt.get("reviewId").is_none(), "a clean commit mints no review");

	let (_, pending) = get_json(&daemon, "/review").await;
	assert_eq!(pending["pending"], false);
	assert!(!preserved.exists(), "replaced reviews lose their preserved copies");

	// A second divergent comparison mints a fresh review…
	let (_, receipt) = post_json(
		&daemon,
		"/compare",
		&json!({
			"submissionId": "sub-second",
			"chunkIndex": 0,
			"finalChunk": true,
			"restart": true,
			"entries": [
				{ "ref": hello, "change": "update", "class": "ModuleScript", "name": "Hello", "instancePath": "ReplicatedStorage/Hello" },
			],
		}),
	)
	.await;

	let review_id = receipt["reviewId"].as_str().unwrap().to_owned();
	assert_ne!(review_id, first_review);

	let preserved = daemon
		.root
		.join(".wsync-backups")
		.join("review")
		.join(&review_id)
		.join("src/ReplicatedStorage/Hello.luau");
	assert!(preserved.exists());

	// …whose stale-id handling and dismissal are pinned
	let (status, _) = post_json(&daemon, "/review/push", &json!({ "reviewId": "nope", "mode": "all" })).await;
	assert_eq!(status, 404);

	let (status, _) = post_json(&daemon, "/review/dismiss", &json!({ "reviewId": "nope" })).await;
	assert_eq!(status, 404);

	// Push ids outside the set → 400
	let (status, _) = post_json(&daemon, "/review/push", &json!({ "reviewId": review_id, "ids": [7] })).await;
	assert_eq!(status, 400);

	let (status, outcome) = post_json(&daemon, "/review/dismiss", &json!({ "reviewId": review_id })).await;
	assert_eq!(status, 200);
	// Dismiss also reports how many disk-only files it discarded, so "keep
	// Studio's versions everywhere" is not a silent deletion. This review holds
	// a `differs` entry and no disk-only ones, so nothing is removed here
	assert_eq!(outcome, json!({ "ok": true, "discarded": 0 }));

	let (_, pending) = get_json(&daemon, "/review").await;
	assert_eq!(pending["pending"], false);
	assert!(
		!daemon.root.join(".wsync-backups").join("review").exists(),
		"dismiss deletes the preserved copies"
	);

	// Studio's version stands on disk after the dismissal
	assert_eq!(
		fs::read_to_string(daemon.root.join("src/ReplicatedStorage/Hello.luau")).unwrap(),
		STUDIO_HELLO
	);

	plugin.abort();
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

	let review_id = receipt["reviewId"].as_str().unwrap().to_owned();

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

	// Reboot over the same workspace: the persisted review index and the
	// preserved copies keep the review answerable
	let TestDaemon { dir, .. } = daemon;
	let daemon = start_daemon_in(dir);
	tokio::time::sleep(SETTLE).await;

	let (status, pending) = get_json(&daemon, "/review").await;
	assert_eq!(status, 200);
	assert_eq!(pending["pending"], true, "the review must survive a restart");
	assert_eq!(pending["reviewId"], review_id.as_str());
	assert_eq!(pending["stats"]["total"], 2);

	let (status, details) = get_json(&daemon, &format!("/review/details?reviewId={review_id}")).await;
	assert_eq!(status, 200);
	assert_eq!(details["items"].as_array().unwrap().len(), 2);

	// Pushing needs a live sync channel: without a plugin it refuses
	// honestly (and stays repeatable) instead of dropping frames
	let (status, body) = post_json(
		&daemon,
		"/review/push",
		&json!({ "reviewId": review_id, "mode": "all" }),
	)
	.await;
	assert_eq!(status, 503);
	assert_eq!(body["ok"], false);

	// Dismissal needs no plugin
	let (status, _) = post_json(&daemon, "/review/dismiss", &json!({ "reviewId": review_id })).await;
	assert_eq!(status, 200);

	let (_, pending) = get_json(&daemon, "/review").await;
	assert_eq!(pending["pending"], false);
}

#[tokio::test]
async fn full_scope_projects_keep_the_choice_flow() {
	// The stock scratch project pins "scope": "full"
	let daemon = start_daemon(None);
	tokio::time::sleep(SETTLE).await;

	let (_, hello_body) = get_json(&daemon, "/hello").await;
	assert_eq!(hello_body["scope"], "full");

	let (mut watch, watch_hello) = connect_client(&daemon, "watch", "full-scope-watch").await;
	assert_eq!(watch_hello["scope"], "full");

	let (_plugin, _) = connect_client(&daemon, "plugin", "full-scope-plugin").await;

	let (status, receipt) = post_json(
		&daemon,
		"/compare",
		&json!({
			"submissionId": "sub-full",
			"chunkIndex": 0,
			"finalChunk": true,
			"entries": [
				{ "ref": STUDIO_ONLY_REF, "change": "remove", "class": "Folder", "name": "StudioOnly", "instancePath": "ReplicatedStorage/StudioOnly" },
			],
		}),
	)
	.await;

	// Full scope still freezes a choice: choiceId minted, choice-needed
	// broadcast, nothing auto-applied, no review
	assert_eq!(status, 200);
	assert!(receipt["choiceId"].is_string(), "receipt: {receipt}");
	assert!(receipt.get("reviewId").is_none());

	let event = wait_for_frame(&mut watch, Duration::from_secs(10), |frame| {
		frame["type"] == "event" && (frame["topic"] == "choice-needed" || frame["topic"] == "disk-review")
	})
	.await
	.expect("no choice-needed event");

	assert_eq!(event["topic"], "choice-needed", "full scope must keep asking");

	let (_, choice) = get_json(&daemon, "/choice").await;
	assert_eq!(choice["pending"], true);

	let (_, review) = get_json(&daemon, "/review").await;
	assert_eq!(review["pending"], false);

	// The disk was not touched: full scope waits for the decision
	assert_eq!(
		fs::read_to_string(daemon.src_dir().join("Hello.luau")).unwrap(),
		"return \"hello\"\n"
	);
}
