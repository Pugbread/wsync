//! The three-way connect (Design §7.0 with the sync base): a connect decides
//! every difference by which side moved since the last agreed state, so a
//! daemon that was down while disk moved ahead pushes that work to Studio
//! instead of overwriting it. Covers the incident this exists for (edits,
//! creations and deletions made on disk while the daemon was down), a real
//! conflict (both moved → Studio wins, disk kept in the backlog), a
//! Studio-only change (Studio wins, nothing backlogged), a connect from a
//! different place (the base is ignored), and the daemon tree forgetting
//! what the backlog took, so a file rewritten there reaches Studio again.

mod common;

use serde_json::{json, Value};
use std::{collections::HashMap, fs, time::Duration};

use common::{
	child_ref_in, post_json, scratch_project_scoped, service_ref, source_claims, spawn_fake_plugin, start_daemon_in,
	FakePlugin, FakePluginScript, TestDaemon,
};
use wsync::server::sync_base::source_hash;

const SETTLE: Duration = Duration::from_millis(600);

const PLACE: u64 = 7770001;
const GAME: u64 = 5550001;

const V1_A: &str = "return \"a v1\"\n";
const V1_B: &str = "return \"b v1\"\n";
const DISK_A: &str = "return \"a edited on disk\"\n";
const STUDIO_A: &str = "return \"a edited in Studio\"\n";
const NEW_C: &str = "return \"c created on disk\"\n";

/// Minted ref for an instance that exists only in the fake Studio
const STUDIO_B_REF: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb1";

fn place(place_id: u64) -> Value {
	json!({ "placeId": place_id, "gameId": GAME })
}

/// A code-scope place with ServerScriptService mapped to src/SSS holding
/// `A` and `B`
fn project() -> TestDaemon {
	let dir = scratch_project_scoped(
		json!({
			"$className": "DataModel",
			"ServerScriptService": { "$path": "src/SSS" },
		}),
		&[("src/SSS/A.luau", V1_A), ("src/SSS/B.luau", V1_B)],
		None,
	);

	start_daemon_in(dir)
}

fn sss(daemon: &TestDaemon) -> std::path::PathBuf {
	daemon.root.join("src/SSS")
}

async fn compare(daemon: &TestDaemon, submission: &str, place_id: u64, entries: Value) -> Value {
	let (status, receipt) = post_json(
		daemon,
		"/compare",
		&json!({
			"submissionId": submission,
			"chunkIndex": 0,
			"finalChunk": true,
			"restart": true,
			"place": place(place_id),
			"entries": entries,
		}),
	)
	.await;

	assert_eq!(status, 200, "receipt: {receipt}");
	assert_eq!(receipt["ok"], true, "receipt: {receipt}");
	assert_eq!(receipt["committed"], true, "receipt: {receipt}");

	receipt
}

/// The first connect: both sides agree, the clean commit records the base
async fn agree(daemon: &TestDaemon) {
	let receipt = compare(daemon, "sub-agree", PLACE, json!([])).await;

	assert_eq!(receipt["clean"], true, "receipt: {receipt}");
	assert!(
		daemon.root.join(".wsync-backups/sync-base.json").is_file(),
		"a clean connect records the sync base"
	);
}

async fn restart(daemon: TestDaemon) -> TestDaemon {
	let (status, _) = reqwest::Client::new()
		.post(daemon.http("/stop"))
		.body("")
		.send()
		.await
		.map(|response| (response.status(), ()))
		.unwrap();

	assert_eq!(status, 200);
	assert!(daemon.wait_for_exit(Duration::from_secs(15)), "daemon never exited");

	let TestDaemon { dir, .. } = daemon;
	let daemon = start_daemon_in(dir);

	tokio::time::sleep(SETTLE).await;

	daemon
}

/// A fake Studio whose ServerScriptService holds `scripts` (ref, name,
/// source) — served for a Studio → disk pull
async fn studio(daemon: &TestDaemon, scripts: &[(&str, &str, &str)]) -> FakePlugin {
	let root = service_ref(daemon, "ServerScriptService").await.unwrap();

	let sources: HashMap<String, Vec<u8>> = scripts
		.iter()
		.map(|(reference, _, source)| (reference.to_string(), source.as_bytes().to_vec()))
		.collect();

	let subtree = json!({
		"id": root,
		"name": "ServerScriptService",
		"class": "ServerScriptService",
		"properties": {},
		"children": scripts.iter().map(|(reference, name, _)| json!({
			"id": reference, "name": name, "class": "ModuleScript", "properties": {}, "children": [],
		})).collect::<Vec<_>>(),
		"sources": source_claims(&sources),
	});

	spawn_fake_plugin(
		daemon,
		"three-way-plugin",
		FakePluginScript {
			subtrees: HashMap::from([(root, subtree)]),
			sources,
			..FakePluginScript::default()
		},
	)
	.await
}

fn update(reference: &str, name: &str, studio_source: &str) -> Value {
	json!({
		"ref": reference, "change": "update", "class": "ModuleScript", "name": name,
		"instancePath": format!("ServerScriptService/{name}"),
		"sourceSha256": source_hash(studio_source),
	})
}

fn add(reference: &str, name: &str) -> Value {
	json!({
		"ref": reference, "change": "add", "class": "ModuleScript", "name": name,
		"instancePath": format!("ServerScriptService/{name}"),
	})
}

fn remove(reference: &str, name: &str, studio_source: &str) -> Value {
	json!({
		"ref": reference, "change": "remove", "class": "ModuleScript", "name": name,
		"instancePath": format!("ServerScriptService/{name}"),
		"sourceSha256": source_hash(studio_source),
		"descendants": [],
	})
}

/// Every sync frame the plugin received, merged
fn synced(plugin: &FakePlugin) -> (Vec<Value>, Vec<Value>, Vec<String>) {
	let mut additions = Vec::new();
	let mut updates = Vec::new();
	let mut removals = Vec::new();

	for frame in plugin.frames.lock().unwrap().iter() {
		if frame["type"] != "sync" {
			continue;
		}

		// The change set is flattened into the frame itself
		let changes = frame;

		additions.extend(changes["additions"].as_array().cloned().unwrap_or_default());
		updates.extend(changes["updates"].as_array().cloned().unwrap_or_default());
		removals.extend(
			changes["removals"]
				.as_array()
				.cloned()
				.unwrap_or_default()
				.into_iter()
				.filter_map(|removal| removal.as_str().map(str::to_owned)),
		);
	}

	(additions, updates, removals)
}

fn backlog_paths(daemon: &TestDaemon) -> Vec<String> {
	let index = daemon.root.join(".wsync-backups/backlog/index.json");

	let Ok(contents) = fs::read_to_string(index) else {
		return Vec::new();
	};

	let entries: Vec<Value> = serde_json::from_str(&contents).unwrap();

	entries
		.iter()
		.filter_map(|entry| entry["path"].as_str().map(str::to_owned))
		.collect()
}

#[tokio::test]
async fn disk_work_done_while_the_daemon_was_down_goes_to_studio() {
	let daemon = project();
	tokio::time::sleep(SETTLE).await;

	agree(&daemon).await;

	// The daemon dies; work carries on on disk: an edit, a new file, a delete
	fs::write(sss(&daemon).join("A.luau"), DISK_A).unwrap();
	fs::write(sss(&daemon).join("C.luau"), NEW_C).unwrap();
	fs::remove_file(sss(&daemon).join("B.luau")).unwrap();

	let daemon = restart(daemon).await;

	let a = child_ref_in(&daemon, "ServerScriptService", "A").await.unwrap();
	let c = child_ref_in(&daemon, "ServerScriptService", "C").await.unwrap();

	// Studio still holds exactly what was agreed: A v1, B v1, no C
	let plugin = studio(&daemon, &[(&a, "A", V1_A), (STUDIO_B_REF, "B", V1_B)]).await;

	let receipt = compare(
		&daemon,
		"sub-reconnect",
		PLACE,
		json!([update(&a, "A", V1_A), add(&c, "C"), remove(STUDIO_B_REF, "B", V1_B)]),
	)
	.await;

	assert_eq!(receipt["threeWay"], true, "receipt: {receipt}");
	assert_eq!(receipt["toStudio"], 3, "receipt: {receipt}");
	assert_eq!(receipt["backlogged"], 0, "nothing was a conflict: {receipt}");

	// Disk is untouched: nothing of Studio's older state came back
	assert_eq!(fs::read_to_string(sss(&daemon).join("A.luau")).unwrap(), DISK_A);
	assert_eq!(fs::read_to_string(sss(&daemon).join("C.luau")).unwrap(), NEW_C);
	assert!(!sss(&daemon).join("B.luau").exists(), "the disk deletion stands");
	assert!(backlog_paths(&daemon).is_empty());

	// And Studio receives all three, bracketed as one transaction
	plugin
		.wait_for(Duration::from_secs(10), |frame| {
			frame["type"] == "request" && frame["op"] == "transaction_finish"
		})
		.await
		.expect("the disk-wins replay never finished");

	let (additions, updates, removals) = synced(&plugin);

	assert!(
		updates.iter().any(|update| update["id"] == a.as_str()),
		"A's disk edit is pushed: {updates:?}"
	);
	assert!(
		additions.iter().any(|addition| addition["name"] == "C"),
		"C is created in Studio: {additions:?}"
	);
	assert!(
		removals.iter().any(|removal| removal == STUDIO_B_REF),
		"B is deleted in Studio: {removals:?}"
	);

	assert!(
		!plugin.events().iter().any(|event| event == "read_subtree"),
		"nothing Studio holds was needed on disk"
	);
}

#[tokio::test]
async fn both_sides_moved_is_a_conflict_studio_wins_and_disk_is_kept() {
	let daemon = project();
	tokio::time::sleep(SETTLE).await;

	agree(&daemon).await;

	fs::write(sss(&daemon).join("A.luau"), DISK_A).unwrap();

	let daemon = restart(daemon).await;
	let a = child_ref_in(&daemon, "ServerScriptService", "A").await.unwrap();
	let b = child_ref_in(&daemon, "ServerScriptService", "B").await.unwrap();

	let _plugin = studio(&daemon, &[(&a, "A", STUDIO_A), (&b, "B", V1_B)]).await;

	let receipt = compare(&daemon, "sub-conflict", PLACE, json!([update(&a, "A", STUDIO_A)])).await;

	assert_eq!(receipt["toStudio"], 0, "receipt: {receipt}");
	assert_eq!(receipt["backlogged"], 1, "receipt: {receipt}");
	assert_eq!(fs::read_to_string(sss(&daemon).join("A.luau")).unwrap(), STUDIO_A);
	assert_eq!(backlog_paths(&daemon), vec!["src/SSS/A.luau".to_owned()]);
}

#[tokio::test]
async fn a_studio_only_change_wins_without_a_backlog_entry() {
	let daemon = project();
	tokio::time::sleep(SETTLE).await;

	agree(&daemon).await;

	let daemon = restart(daemon).await;
	let a = child_ref_in(&daemon, "ServerScriptService", "A").await.unwrap();
	let b = child_ref_in(&daemon, "ServerScriptService", "B").await.unwrap();

	let _plugin = studio(&daemon, &[(&a, "A", STUDIO_A), (&b, "B", V1_B)]).await;

	let receipt = compare(&daemon, "sub-studio", PLACE, json!([update(&a, "A", STUDIO_A)])).await;

	assert_eq!(receipt["backlogged"], 0, "disk had not moved: {receipt}");
	assert_eq!(fs::read_to_string(sss(&daemon).join("A.luau")).unwrap(), STUDIO_A);
	assert_eq!(
		receipt["rehydrate"], true,
		"a root swap re-reads ids the plugin must re-pair"
	);
}

#[tokio::test]
async fn another_place_never_reads_this_base() {
	let daemon = project();
	tokio::time::sleep(SETTLE).await;

	agree(&daemon).await;

	fs::write(sss(&daemon).join("C.luau"), NEW_C).unwrap();

	let daemon = restart(daemon).await;
	let c = child_ref_in(&daemon, "ServerScriptService", "C").await.unwrap();

	// A different place: without a base, a disk-only file loses to Studio
	// and is kept in the backlog — the Studio-first rule
	let receipt = compare(&daemon, "sub-other-place", PLACE + 1, json!([add(&c, "C")])).await;

	assert_eq!(receipt["threeWay"], false, "receipt: {receipt}");
	assert_eq!(receipt["toStudio"], 0, "receipt: {receipt}");
	assert!(!sss(&daemon).join("C.luau").exists());
	assert_eq!(backlog_paths(&daemon), vec!["src/SSS/C.luau".to_owned()]);
}

#[tokio::test]
async fn a_file_rewritten_where_the_backlog_took_one_reaches_studio() {
	let daemon = project();
	tokio::time::sleep(SETTLE).await;

	// No base: the disk-only file D loses to Studio and is backlogged
	fs::write(sss(&daemon).join("D.luau"), NEW_C).unwrap();
	tokio::time::sleep(SETTLE).await;

	let d = child_ref_in(&daemon, "ServerScriptService", "D").await.unwrap();
	let plugin = studio(&daemon, &[]).await;

	let receipt = compare(&daemon, "sub-no-base", PLACE, json!([add(&d, "D")])).await;

	assert_eq!(receipt["backlogged"], 1, "receipt: {receipt}");
	assert!(!sss(&daemon).join("D.luau").exists());

	// The tree let go of it along with the disk: no stale instance is left
	// to answer for the path
	assert!(
		child_ref_in(&daemon, "ServerScriptService", "D").await.is_none(),
		"the backlogged instance must leave the daemon tree"
	);

	// Writing the file again is a brand-new instance for Studio — an
	// addition, not an update to an id Studio never received. (Past the
	// watcher's post-resume grace window first: the connect paused it)
	tokio::time::sleep(Duration::from_secs(1)).await;
	fs::write(sss(&daemon).join("D.luau"), "return \"rewritten\"\n").unwrap();

	let frame = plugin
		.wait_for(Duration::from_secs(10), |frame| {
			frame["type"] == "sync"
				&& frame["additions"]
					.as_array()
					.is_some_and(|additions| additions.iter().any(|addition| addition["name"] == "D"))
		})
		.await;

	assert!(
		frame.is_some(),
		"the rewritten file never reached Studio as an addition"
	);

	let (_, updates, removals) = synced(&plugin);

	assert!(
		!updates.iter().any(|update| update["id"] == d.as_str()),
		"nothing may target the backlogged instance's old id: {updates:?}"
	);
	assert!(!removals.contains(&d), "nor remove it: {removals:?}");
}

#[tokio::test]
async fn a_path_studio_also_claims_is_not_backlogged_after_studio_wrote_it() {
	let daemon = project();
	tokio::time::sleep(SETTLE).await;

	// The pairing failed (the report's stale duplicate): the connect sees the
	// disk file as disk-only AND Studio's instance at the same path as
	// Studio-only. Studio's copy is written, and must stay written
	let a = child_ref_in(&daemon, "ServerScriptService", "A").await.unwrap();
	let b = child_ref_in(&daemon, "ServerScriptService", "B").await.unwrap();
	let studio_a = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa9";

	let _plugin = studio(&daemon, &[(studio_a, "A", V1_A), (&b, "B", V1_B)]).await;

	let receipt = compare(
		&daemon,
		"sub-unpaired",
		PLACE,
		json!([add(&a, "A"), remove(studio_a, "A", V1_A)]),
	)
	.await;

	assert_eq!(
		fs::read_to_string(sss(&daemon).join("A.luau")).unwrap(),
		V1_A,
		"Studio's copy was written and then swept away as 'disk-only': {receipt}"
	);
	assert!(
		backlog_paths(&daemon).is_empty(),
		"identical bytes are not a loss worth a backlog entry"
	);
}

#[tokio::test]
async fn a_daemon_that_died_applying_a_connect_restarts_in_safe_mode() {
	let dir = scratch_project_scoped(
		json!({
			"$className": "DataModel",
			"ServerScriptService": { "$path": "src/SSS" },
		}),
		&[("src/SSS/A.luau", V1_A), ("src/SSS/B.luau", V1_B)],
		None,
	);

	// What a daemon that crashed mid-apply leaves behind
	fs::create_dir_all(dir.path().join(".wsync-backups")).unwrap();
	fs::write(
		dir.path().join(".wsync-backups/connect-in-progress.json"),
		r#"{"bootId":"crashed","pid":1,"startedAt":1}"#,
	)
	.unwrap();

	let daemon = start_daemon_in(dir);
	tokio::time::sleep(SETTLE).await;

	let (_, hello) = common::get_json(&daemon, "/hello").await;
	assert_eq!(hello["safeMode"]["previousBootId"], "crashed", "hello: {hello}");

	let a = child_ref_in(&daemon, "ServerScriptService", "A").await.unwrap();
	let b = child_ref_in(&daemon, "ServerScriptService", "B").await.unwrap();
	let _plugin = studio(&daemon, &[(&a, "A", STUDIO_A), (&b, "B", V1_B)]).await;

	let body = json!({
		"submissionId": "sub-safe",
		"chunkIndex": 0,
		"finalChunk": true,
		"restart": true,
		"place": place(PLACE),
		"entries": [update(&a, "A", STUDIO_A)],
	});

	let (status, refused) = post_json(&daemon, "/compare", &body).await;

	assert_eq!(status, 423, "refused: {refused}");
	assert_eq!(refused["ok"], false);
	assert_eq!(
		fs::read_to_string(sss(&daemon).join("A.luau")).unwrap(),
		V1_A,
		"safe mode applies nothing"
	);

	let report: Value =
		serde_json::from_str(&fs::read_to_string(daemon.root.join(".wsync-backups/safe-mode-report.json")).unwrap())
			.unwrap();

	assert_eq!(report["differences"][0]["instancePath"], "ServerScriptService/A");

	let (status, resumed) = post_json(&daemon, "/safe-mode/resume", &json!({})).await;

	assert_eq!(status, 200);
	assert_eq!(resumed["wasInSafeMode"], true);

	let (status, receipt) = post_json(&daemon, "/compare", &body).await;

	assert_eq!(status, 200, "receipt: {receipt}");
	assert_eq!(fs::read_to_string(sss(&daemon).join("A.luau")).unwrap(), STUDIO_A);
}
