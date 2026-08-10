//! Fenced Keep-Studio apply tests (Design §7.4-A) against the fake WS
//! plugin: the server-driven pull (structure + verified sources), middleware
//! staging, fence refusal, swap with backups, baseline stamping, partial
//! receipts, backup pruning — plus Keep-Disk's transaction bracketing
//! (§7.4-B).

mod common;

use chrono::Utc;
use serde_json::{json, Value};
use std::{
	collections::{HashMap, HashSet},
	fs,
	path::Path,
	time::Duration,
};

use common::{
	child_ref_in, get_json, post_json, scratch_project_custom, service_ref, source_claims, spawn_fake_plugin,
	start_daemon, start_daemon_in, FakePluginScript, TestDaemon,
};

const SETTLE: Duration = Duration::from_millis(600);
const STUDIO_ONLY_REF: &str = "ffffffffffffffffffffffffffffffff";

/// Fresh refs for instances that exist only in the fake Studio
const EXTRA_REF: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const CFG_REF: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn script_node(reference: &str, name: &str, class: &str) -> Value {
	// Script sources are elided from the structure per the pinned contract
	json!({ "id": reference, "name": name, "class": class, "properties": {}, "children": [] })
}

/// Waits until a file under the daemon's workspace has the given content
async fn wait_for_file(path: &Path, expected: &str) {
	let deadline = tokio::time::Instant::now() + Duration::from_secs(15);

	loop {
		if fs::read_to_string(path)
			.map(|content| content == expected)
			.unwrap_or(false)
		{
			return;
		}

		assert!(
			tokio::time::Instant::now() < deadline,
			"{} never reached the expected content",
			path.display()
		);
		tokio::time::sleep(Duration::from_millis(100)).await;
	}
}

/// Writes a second module and waits until the watcher ingested it
async fn grow_second_module(daemon: &TestDaemon) -> String {
	tokio::time::sleep(SETTLE).await;
	fs::write(daemon.src_dir().join("ModA.luau"), "return \"mod-a\"\n").unwrap();

	let deadline = tokio::time::Instant::now() + Duration::from_secs(15);

	loop {
		if let Some(id) = child_ref_in(daemon, "ReplicatedStorage", "ModA").await {
			return id;
		}

		assert!(tokio::time::Instant::now() < deadline, "ModA never appeared");
		tokio::time::sleep(Duration::from_millis(100)).await;
	}
}

/// Commits a one-row divergence set (update Hello) and returns the choiceId
async fn commit_hello_update(daemon: &TestDaemon, hello: &str, submission: &str) -> String {
	let (status, receipt) = post_json(
		daemon,
		"/compare",
		&json!({
			"submissionId": submission,
			"chunkIndex": 0,
			"finalChunk": true,
			"restart": true,
			"entries": [
				{ "ref": hello, "change": "update", "class": "ModuleScript", "name": "Hello", "instancePath": "ReplicatedStorage/Hello" },
			],
		}),
	)
	.await;

	assert_eq!(status, 200);
	assert_eq!(receipt["committed"], true);

	receipt["choiceId"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn keep_studio_applies_with_staging_backups_and_baselines() {
	let daemon = start_daemon(None);
	let mod_a = grow_second_module(&daemon).await;
	let hello = child_ref_in(&daemon, "ReplicatedStorage", "Hello").await.unwrap();
	let root = service_ref(&daemon, "ReplicatedStorage").await.unwrap();

	let hello_source = "return \"studio hello\"\n".to_owned();
	let mod_a_source = "return \"studio mod-a\"\n".to_owned();
	// Big enough to need three source_read chunks (> 2 × 64 KiB)
	let extra_source = format!("-- big\n{}", "x".repeat(150 * 1024));

	let sources = HashMap::from([
		(hello.clone(), hello_source.clone().into_bytes()),
		(mod_a.clone(), mod_a_source.clone().into_bytes()),
		(EXTRA_REF.to_owned(), extra_source.clone().into_bytes()),
	]);

	let subtree = json!({
		"id": root,
		"name": "ReplicatedStorage",
		"class": "ReplicatedStorage",
		"properties": {},
		"children": [
			script_node(&hello, "Hello", "ModuleScript"),
			script_node(&mod_a, "ModA", "ModuleScript"),
			script_node(EXTRA_REF, "Extra", "ModuleScript"),
			{ "id": CFG_REF, "name": "Cfg", "class": "Configuration", "properties": {}, "children": [] },
		],
		"sources": source_claims(&sources),
	});

	let plugin = spawn_fake_plugin(
		&daemon,
		"studio-plugin",
		FakePluginScript {
			subtrees: HashMap::from([(root.clone(), subtree)]),
			sources,
			..FakePluginScript::default()
		},
	)
	.await;

	let choice_id = commit_hello_update(&daemon, &hello, "sub-happy").await;

	let (status, receipt) = post_json(
		&daemon,
		"/choice",
		&json!({ "choiceId": choice_id, "choice": "studio", "mode": "all" }),
	)
	.await;

	// Pinned success receipt
	assert_eq!(status, 200);
	assert_eq!(receipt["ok"], true, "receipt: {receipt}");
	assert_eq!(receipt["applied"], true);
	assert_eq!(receipt["direction"], "studio");

	let backups = receipt["backups"].as_array().unwrap();
	assert_eq!(backups.len(), 1);

	let backup = backups[0].as_str().unwrap();
	assert!(backup.ends_with("/ReplicatedStorage"), "backup name: {backup}");

	// Exact on-disk results, projected through the write middleware
	let src = daemon.src_dir();
	assert_eq!(fs::read_to_string(src.join("Hello.luau")).unwrap(), hello_source);
	assert_eq!(fs::read_to_string(src.join("ModA.luau")).unwrap(), mod_a_source);
	assert_eq!(fs::read_to_string(src.join("Extra.luau")).unwrap(), extra_source);

	// Non-script instances materialize as dir + data sidecar
	let cfg_dir = src.join("Cfg");
	assert!(cfg_dir.is_dir(), "Configuration projects as a directory");

	let sidecar = fs::read_dir(&cfg_dir)
		.unwrap()
		.filter_map(|entry| entry.ok())
		.find(|entry| entry.file_name().to_string_lossy().ends_with(".json"))
		.expect("Configuration must carry a data sidecar");
	assert!(
		fs::read_to_string(sidecar.path()).unwrap().contains("Configuration"),
		"sidecar must pin the class"
	);

	// Backups: the pre-apply generation, preserved under the transfer dir
	let backup_dir = daemon.root.join(".wsync-backups").join(backup);
	assert_eq!(
		fs::read_to_string(backup_dir.join("Hello.luau")).unwrap(),
		"return \"hello\"\n"
	);
	assert_eq!(
		fs::read_to_string(backup_dir.join("ModA.luau")).unwrap(),
		"return \"mod-a\"\n"
	);

	// The transfer is marked complete and its staging leftovers are gone
	let transfer_dir = backup_dir.parent().unwrap();
	assert!(transfer_dir.join("complete.json").is_file());
	assert!(!transfer_dir.join(".stage").exists());

	// The daemon tree was refreshed from the swapped root
	let (_, pending) = get_json(&daemon, "/choice").await;
	assert_eq!(pending["pending"], false);

	assert!(child_ref_in(&daemon, "ReplicatedStorage", "Extra").await.is_some());
	assert!(child_ref_in(&daemon, "ReplicatedStorage", "Cfg").await.is_some());

	// The pull happened over the op surface: one structure read, chunked
	// source reads (the oversized Extra alone needs three)
	let events = plugin.events();
	assert_eq!(events.iter().filter(|event| *event == "read_subtree").count(), 1);
	assert!(
		events.iter().filter(|event| *event == "source_read").count() >= 5,
		"chunked source pulls expected: {events:?}"
	);

	// Baselines stamped: a Studio push against the applied content applies
	// cleanly instead of parking a both-edited conflict
	let mod_a = child_ref_in(&daemon, "ReplicatedStorage", "ModA").await.unwrap();

	plugin.send(&json!({
		"type": "push",
		"ops": {
			"additions": [],
			"updates": [{ "id": mod_a, "properties": { "Source": { "String": "return \"pushed\"\n" } } }],
			"removals": [],
		},
	}));

	let result = plugin
		.wait_for(Duration::from_secs(10), |frame| frame["type"] == "push-result")
		.await
		.expect("no push-result");

	assert_eq!(result["ok"], true, "push-result: {result}");
	assert_eq!(result["applied"], 1);
	assert_eq!(result["conflicts"], 0, "stamped baselines must accept the push");

	wait_for_file(&src.join("ModA.luau"), "return \"pushed\"\n").await;

	// The watcher survived the swap (unwatch → rename → rewatch): a fresh
	// disk edit still enters the live sync pipeline. Settle past the
	// post-write anti-echo window first, like every watcher-driven test
	tokio::time::sleep(SETTLE).await;
	fs::write(src.join("PostSwap.luau"), "return \"post\"\n").unwrap();

	let deadline = tokio::time::Instant::now() + Duration::from_secs(15);

	loop {
		if child_ref_in(&daemon, "ReplicatedStorage", "PostSwap").await.is_some() {
			break;
		}

		assert!(
			tokio::time::Instant::now() < deadline,
			"the watcher must stay live after the swap"
		);
		tokio::time::sleep(Duration::from_millis(100)).await;
	}

	plugin.abort();
}

#[tokio::test]
async fn sha256_mismatch_aborts_without_touching_the_live_root() {
	let daemon = start_daemon(None);
	tokio::time::sleep(SETTLE).await;

	let hello = child_ref_in(&daemon, "ReplicatedStorage", "Hello").await.unwrap();
	let root = service_ref(&daemon, "ReplicatedStorage").await.unwrap();

	let sources = HashMap::from([(hello.clone(), b"return \"studio hello\"\n".to_vec())]);

	let subtree = json!({
		"id": root,
		"name": "ReplicatedStorage",
		"class": "ReplicatedStorage",
		"properties": {},
		"children": [script_node(&hello, "Hello", "ModuleScript")],
		"sources": source_claims(&sources),
	});

	let plugin = spawn_fake_plugin(
		&daemon,
		"corrupt-plugin",
		FakePluginScript {
			subtrees: HashMap::from([(root.clone(), subtree)]),
			corrupt: HashSet::from([hello.clone()]),
			sources,
			..FakePluginScript::default()
		},
	)
	.await;

	let choice_id = commit_hello_update(&daemon, &hello, "sub-corrupt").await;

	let (status, receipt) = post_json(
		&daemon,
		"/choice",
		&json!({ "choiceId": choice_id, "choice": "studio", "mode": "all" }),
	)
	.await;

	// Pinned partial receipt: HTTP 200, replayable and honest
	assert_eq!(status, 200);
	assert_eq!(receipt["ok"], false);
	assert_eq!(receipt["action"], "partial");
	assert_eq!(receipt["recoveryRequired"], true);
	assert_eq!(receipt["failedRoot"], "ReplicatedStorage");
	assert_eq!(receipt["committedRoots"], json!([]));
	assert!(
		receipt["error"].as_str().unwrap().contains("SHA-256 mismatch"),
		"error: {receipt}"
	);

	// The live root was never swapped
	assert_eq!(
		fs::read_to_string(daemon.src_dir().join("Hello.luau")).unwrap(),
		"return \"hello\"\n"
	);

	// Nothing was marked complete
	let backups_root = daemon.root.join(".wsync-backups");

	if let Ok(entries) = fs::read_dir(&backups_root) {
		for entry in entries.filter_map(|entry| entry.ok()) {
			assert!(
				!entry.path().join("complete.json").exists(),
				"an aborted transfer must not mark complete"
			);
		}
	}

	plugin.abort();
}

#[tokio::test]
async fn fence_refuses_when_the_disk_moves_mid_apply() {
	let daemon = start_daemon(None);
	tokio::time::sleep(SETTLE).await;

	let hello = child_ref_in(&daemon, "ReplicatedStorage", "Hello").await.unwrap();
	let root = service_ref(&daemon, "ReplicatedStorage").await.unwrap();

	let sources = HashMap::from([(hello.clone(), b"return \"studio hello\"\n".to_vec())]);

	let subtree = json!({
		"id": root,
		"name": "ReplicatedStorage",
		"class": "ReplicatedStorage",
		"properties": {},
		"children": [script_node(&hello, "Hello", "ModuleScript")],
		"sources": source_claims(&sources),
	});

	// The concurrent edit lands when read_subtree arrives: after the fence
	// generation was captured, before the swap
	let concurrent = "-- concurrent edit\nreturn \"concurrent\"\n";

	let plugin = spawn_fake_plugin(
		&daemon,
		"fence-plugin",
		FakePluginScript {
			subtrees: HashMap::from([(root.clone(), subtree)]),
			sources,
			touch_on_read_subtree: vec![(daemon.src_dir().join("Hello.luau"), concurrent.to_owned())],
			..FakePluginScript::default()
		},
	)
	.await;

	let choice_id = commit_hello_update(&daemon, &hello, "sub-fence").await;

	let (status, receipt) = post_json(
		&daemon,
		"/choice",
		&json!({ "choiceId": choice_id, "choice": "studio", "mode": "all" }),
	)
	.await;

	assert_eq!(status, 200);
	assert_eq!(receipt["ok"], false);
	assert_eq!(receipt["action"], "partial");
	assert_eq!(receipt["failedRoot"], "ReplicatedStorage");
	assert!(
		receipt["error"].as_str().unwrap().contains("changed while the apply"),
		"error: {receipt}"
	);

	// The concurrent edit survives — the root was refused, not overwritten
	assert_eq!(
		fs::read_to_string(daemon.src_dir().join("Hello.luau")).unwrap(),
		concurrent
	);

	// The refusal happened before the backup rename: no root backup exists
	let backups_root = daemon.root.join(".wsync-backups");

	if let Ok(entries) = fs::read_dir(&backups_root) {
		for entry in entries.filter_map(|entry| entry.ok()) {
			assert!(
				!entry.path().join("ReplicatedStorage").exists(),
				"a refused root must not leave a backup"
			);
		}
	}

	plugin.abort();
}

#[tokio::test]
async fn partial_failure_keeps_committed_roots_and_their_backups() {
	let dir = scratch_project_custom(
		json!({
			"$className": "DataModel",
			"ReplicatedStorage": { "$path": "src" },
			"ServerScriptService": { "$path": "server" },
		}),
		&[
			("src/Hello.luau", "return \"hello\"\n"),
			("server/Main.server.luau", "print(\"main\")\n"),
		],
	);

	let daemon = start_daemon_in(dir);
	tokio::time::sleep(SETTLE).await;

	let hello = child_ref_in(&daemon, "ReplicatedStorage", "Hello").await.unwrap();
	let main = child_ref_in(&daemon, "ServerScriptService", "Main").await.unwrap();
	let rs_root = service_ref(&daemon, "ReplicatedStorage").await.unwrap();
	let sss_root = service_ref(&daemon, "ServerScriptService").await.unwrap();

	let hello_studio = "return \"studio hello\"\n".to_owned();

	let sources = HashMap::from([
		(hello.clone(), hello_studio.clone().into_bytes()),
		(main.clone(), b"print(\"studio main\")\n".to_vec()),
	]);

	let rs_sources = HashMap::from([(hello.clone(), hello_studio.clone().into_bytes())]);
	let sss_sources = HashMap::from([(main.clone(), b"print(\"studio main\")\n".to_vec())]);

	let subtrees = HashMap::from([
		(
			rs_root.clone(),
			json!({
				"id": rs_root,
				"name": "ReplicatedStorage",
				"class": "ReplicatedStorage",
				"properties": {},
				"children": [script_node(&hello, "Hello", "ModuleScript")],
				"sources": source_claims(&rs_sources),
			}),
		),
		(
			sss_root.clone(),
			json!({
				"id": sss_root,
				"name": "ServerScriptService",
				"class": "ServerScriptService",
				"properties": {},
				"children": [script_node(&main, "Main", "Script")],
				"sources": source_claims(&sss_sources),
			}),
		),
	]);

	// ServerScriptService's source is corrupted; roots apply in name order,
	// so ReplicatedStorage commits first and the transfer then fails
	let plugin = spawn_fake_plugin(
		&daemon,
		"partial-plugin",
		FakePluginScript {
			subtrees: subtrees.clone(),
			sources: sources.clone(),
			corrupt: HashSet::from([main.clone()]),
			..FakePluginScript::default()
		},
	)
	.await;

	let (status, receipt) = post_json(
		&daemon,
		"/compare",
		&json!({
			"submissionId": "sub-partial",
			"chunkIndex": 0,
			"finalChunk": true,
			"entries": [
				{ "ref": hello, "change": "update", "class": "ModuleScript", "name": "Hello", "instancePath": "ReplicatedStorage/Hello" },
				{ "ref": main, "change": "update", "class": "Script", "name": "Main", "instancePath": "ServerScriptService/Main" },
			],
		}),
	)
	.await;
	assert_eq!(status, 200);
	let choice_id = receipt["choiceId"].as_str().unwrap().to_owned();

	let (status, receipt) = post_json(
		&daemon,
		"/choice",
		&json!({ "choiceId": choice_id, "choice": "studio", "mode": "all" }),
	)
	.await;

	// Pinned partial receipt with the committed root listed for recovery
	assert_eq!(status, 200);
	assert_eq!(receipt["ok"], false);
	assert_eq!(receipt["action"], "partial");
	assert_eq!(receipt["recoveryRequired"], true);
	assert_eq!(receipt["failedRoot"], "ServerScriptService");

	let committed = receipt["committedRoots"].as_array().unwrap();
	assert_eq!(committed.len(), 1);
	assert_eq!(committed[0]["root"], "ReplicatedStorage");
	assert_eq!(committed[0]["recoveryAction"], "restoreBackup");

	let rs_backup = committed[0]["backup"].as_str().unwrap().to_owned();

	// Earlier committed roots are not rolled back; the failed root is intact
	assert_eq!(
		fs::read_to_string(daemon.root.join("src").join("Hello.luau")).unwrap(),
		hello_studio
	);
	assert_eq!(
		fs::read_to_string(daemon.root.join("server").join("Main.server.luau")).unwrap(),
		"print(\"main\")\n"
	);

	// The committed root's backup holds the pre-apply generation
	let backups_root = daemon.root.join(".wsync-backups");
	let partial_transfer = backups_root.join(&rs_backup).parent().unwrap().to_owned();

	assert_eq!(
		fs::read_to_string(backups_root.join(&rs_backup).join("Hello.luau")).unwrap(),
		"return \"hello\"\n"
	);
	assert!(
		!partial_transfer.join("complete.json").exists(),
		"a partial transfer never marks complete"
	);

	// A later fully successful transfer must not prune the partial one
	plugin.abort();
	tokio::time::sleep(Duration::from_millis(200)).await;

	let plugin = spawn_fake_plugin(
		&daemon,
		"partial-plugin-2",
		FakePluginScript {
			subtrees,
			sources,
			..FakePluginScript::default()
		},
	)
	.await;

	let (status, receipt) = post_json(
		&daemon,
		"/compare",
		&json!({
			"submissionId": "sub-partial-2",
			"chunkIndex": 0,
			"finalChunk": true,
			"restart": true,
			"entries": [
				{ "ref": hello, "change": "update", "class": "ModuleScript", "name": "Hello", "instancePath": "ReplicatedStorage/Hello" },
				{ "ref": main, "change": "update", "class": "Script", "name": "Main", "instancePath": "ServerScriptService/Main" },
			],
		}),
	)
	.await;
	assert_eq!(status, 200);
	let choice_id = receipt["choiceId"].as_str().unwrap().to_owned();

	let (_, receipt) = post_json(
		&daemon,
		"/choice",
		&json!({ "choiceId": choice_id, "choice": "studio", "mode": "all" }),
	)
	.await;
	assert_eq!(receipt["ok"], true, "second transfer should succeed: {receipt}");
	assert_eq!(receipt["backups"].as_array().unwrap().len(), 2);

	assert_eq!(
		fs::read_to_string(daemon.root.join("server").join("Main.server.luau")).unwrap(),
		"print(\"studio main\")\n"
	);

	// The earlier partial transfer (recovery material) survived the pruning
	// pass of the successful transfer
	assert!(
		backups_root.join(&rs_backup).join("Hello.luau").exists(),
		"partial-transfer backups are never pruned"
	);

	plugin.abort();
}

#[tokio::test]
async fn prune_removes_only_completed_transfers_by_age_and_count() {
	let daemon = start_daemon(None);
	tokio::time::sleep(SETTLE).await;

	let hello = child_ref_in(&daemon, "ReplicatedStorage", "Hello").await.unwrap();
	let root = service_ref(&daemon, "ReplicatedStorage").await.unwrap();

	// Fabricate backup history: an ancient completed transfer, an ancient
	// partial one, and exactly 32 recent completed ones
	let backups_root = daemon.root.join(".wsync-backups");

	let fabricate = |name: &str, complete: bool| {
		let dir = backups_root.join(name);
		fs::create_dir_all(&dir).unwrap();
		fs::write(dir.join("payload"), b"x").unwrap();

		if complete {
			fs::write(dir.join("complete.json"), b"{}").unwrap();
		}
	};

	fabricate("2000-01-01T00-00-00Z-ancient-complete", true);
	fabricate("2000-01-01T00-00-01Z-ancient-partial", false);

	for index in 0..32u32 {
		let stamp = (Utc::now() - chrono::Duration::minutes(i64::from(index) + 1)).format("%Y-%m-%dT%H-%M-%SZ");

		fabricate(&format!("{stamp}-recent-{index:02}"), true);
	}

	let sources = HashMap::from([(hello.clone(), b"return \"studio hello\"\n".to_vec())]);

	let subtree = json!({
		"id": root,
		"name": "ReplicatedStorage",
		"class": "ReplicatedStorage",
		"properties": {},
		"children": [script_node(&hello, "Hello", "ModuleScript")],
		"sources": source_claims(&sources),
	});

	let plugin = spawn_fake_plugin(
		&daemon,
		"prune-plugin",
		FakePluginScript {
			subtrees: HashMap::from([(root.clone(), subtree)]),
			sources,
			..FakePluginScript::default()
		},
	)
	.await;

	let choice_id = commit_hello_update(&daemon, &hello, "sub-prune").await;

	let (_, receipt) = post_json(
		&daemon,
		"/choice",
		&json!({ "choiceId": choice_id, "choice": "studio", "mode": "all" }),
	)
	.await;
	assert_eq!(receipt["ok"], true, "receipt: {receipt}");

	// Age rule: the ancient completed transfer is gone; the ancient partial
	// one is untouchable recovery material
	assert!(!backups_root.join("2000-01-01T00-00-00Z-ancient-complete").exists());
	assert!(backups_root.join("2000-01-01T00-00-01Z-ancient-partial").exists());

	// Count rule: with the new transfer committed there were 33 completed
	// transfers; only the newest 32 remain and the new one is among them
	let completed: Vec<String> = fs::read_dir(&backups_root)
		.unwrap()
		.filter_map(|entry| entry.ok())
		.filter(|entry| entry.path().join("complete.json").is_file())
		.map(|entry| entry.file_name().to_string_lossy().into_owned())
		.collect();

	assert_eq!(completed.len(), 32, "completed transfers: {completed:?}");
	assert!(
		completed.iter().any(|name| name.contains(&choice_id)),
		"the new transfer must survive"
	);
	assert!(
		!completed.iter().any(|name| name.ends_with("-recent-31")),
		"the oldest completed transfer must be pruned by count"
	);

	plugin.abort();
}

#[tokio::test]
async fn keep_disk_brackets_the_replay_in_one_transaction() {
	let daemon = start_daemon(None);
	let mod_a = grow_second_module(&daemon).await;
	let hello = child_ref_in(&daemon, "ReplicatedStorage", "Hello").await.unwrap();

	let plugin = spawn_fake_plugin(&daemon, "bracket-plugin", FakePluginScript::default()).await;

	let (status, receipt) = post_json(
		&daemon,
		"/compare",
		&json!({
			"submissionId": "sub-bracket",
			"chunkIndex": 0,
			"finalChunk": true,
			"entries": [
				{ "ref": mod_a, "change": "add", "class": "ModuleScript", "name": "ModA", "instancePath": "ReplicatedStorage/ModA" },
				{ "ref": hello, "change": "update", "class": "ModuleScript", "name": "Hello", "instancePath": "ReplicatedStorage/Hello" },
				{ "ref": STUDIO_ONLY_REF, "change": "remove", "class": "Folder", "name": "StudioOnly", "instancePath": "ReplicatedStorage/StudioOnly" },
			],
		}),
	)
	.await;
	assert_eq!(status, 200);
	let choice_id = receipt["choiceId"].as_str().unwrap().to_owned();

	let (status, outcome) = post_json(
		&daemon,
		"/choice",
		&json!({ "choiceId": choice_id, "choice": "disk", "mode": "all" }),
	)
	.await;
	assert_eq!(status, 200);
	assert_eq!(outcome["ok"], true);
	assert_eq!(outcome["applied"], true);

	// The replay arrives over the live sync channel...
	let sync = plugin
		.wait_for(Duration::from_secs(10), |frame| {
			frame["type"] == "sync" && !frame["additions"].as_array().unwrap_or(&Vec::new()).is_empty()
		})
		.await
		.expect("no replay sync frame");

	assert_eq!(sync["additions"].as_array().unwrap()[0]["name"], "ModA");
	assert_eq!(sync["updates"].as_array().unwrap()[0]["id"], hello.as_str());
	assert_eq!(sync["removals"].as_array().unwrap()[0], STUDIO_ONLY_REF);

	// ...strictly bracketed: begin before the first frame, commit after the
	// last (the fake plugin records protocol order)
	let bracket: Vec<String> = plugin
		.events()
		.into_iter()
		.filter(|event| event == "transaction_begin" || event == "sync" || event.starts_with("transaction_finish"))
		.collect();

	assert_eq!(
		bracket,
		vec![
			"transaction_begin".to_owned(),
			"sync".to_owned(),
			"transaction_finish:true".to_owned(),
		],
		"bracket order"
	);

	plugin.abort();
}
