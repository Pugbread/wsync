//! A class change on disk reaches the plugin as one update that keeps the
//! instance's ref — never as an addition beside a removal. The incident this
//! guards: `ServerScriptService/Tests` was a Folder, `Tests/init.luau` was
//! added, and Studio ended up with a second `Tests` (a ModuleScript) beside
//! the old Folder and its stale children, because the removal half never
//! landed. End to end: a real daemon, its real file watcher, and the `sync`
//! frames a connected plugin receives.

mod common;

use serde_json::{json, Value};
use std::{fs, time::Duration};

use common::{
	child_ref_in, connect_client, scratch_project_custom, start_daemon_in, wait_for_frame, TestDaemon, WsClient,
};

/// Startup + debounce grace: writes before it can be swallowed by the
/// watcher's self-write window (see tests/protocol.rs)
const SETTLE: Duration = Duration::from_millis(600);

/// How long to keep listening after the first frame: a rename is two watcher
/// events, and each may be its own frame
const TRAILING: Duration = Duration::from_millis(1500);

fn fixture(files: &[(&str, &str)]) -> TestDaemon {
	let tree = json!({
		"$className": "DataModel",
		"ServerScriptService": { "$path": "src" },
	});

	start_daemon_in(scratch_project_custom(tree, files))
}

/// Every `sync` frame from the first one until the stream goes quiet
async fn sync_frames(plugin: &mut WsClient) -> Vec<Value> {
	let mut frames = Vec::new();
	let mut timeout = Duration::from_secs(15);

	while let Some(frame) = wait_for_frame(plugin, timeout, |frame| frame["type"] == "sync").await {
		frames.push(frame);
		timeout = TRAILING;
	}

	frames
}

/// The frames carry exactly one change for the instance: a class change to
/// `class` under its existing ref — no removal of that ref, and no addition
/// of a same-named instance
fn assert_one_class_change(frames: &[Value], id: &str, name: &str, class: &str) {
	assert!(!frames.is_empty(), "no sync frame after the change");

	let ops = |key: &str| -> Vec<Value> {
		frames
			.iter()
			.flat_map(|frame| frame[key].as_array().cloned().unwrap_or_default())
			.collect()
	};

	let removals = ops("removals");
	let additions = ops("additions");
	let updates = ops("updates");

	assert!(
		!removals.iter().any(|removal| removal == id),
		"the instance's ref must not be removed: {frames:#?}"
	);
	assert!(
		!additions.iter().any(|addition| addition["name"] == name),
		"no second '{name}' may be added: {frames:#?}"
	);

	let class_changes: Vec<&Value> = updates
		.iter()
		.filter(|update| update["id"] == id && !update["class"].is_null())
		.collect();

	assert_eq!(class_changes.len(), 1, "one class-change update: {frames:#?}");
	assert_eq!(class_changes[0]["class"], class);
}

#[tokio::test]
async fn adding_init_luau_to_a_folder_is_one_class_change() {
	let daemon = fixture(&[("src/Tests/A.luau", "return 1\n")]);
	let tests = child_ref_in(&daemon, "ServerScriptService", "Tests")
		.await
		.expect("no Tests");
	let (mut plugin, _) = connect_client(&daemon, "plugin", "class-change-plugin").await;

	tokio::time::sleep(SETTLE).await;

	fs::write(daemon.root.join("src/Tests/init.luau"), "return {}\n").unwrap();

	assert_one_class_change(&sync_frames(&mut plugin).await, &tests, "Tests", "ModuleScript");
	assert_eq!(
		child_ref_in(&daemon, "ServerScriptService", "Tests").await.as_deref(),
		Some(tests.as_str()),
		"the daemon keeps the same ref for Tests"
	);

	fs::remove_file(daemon.root.join("src/Tests/init.luau")).unwrap();

	assert_one_class_change(&sync_frames(&mut plugin).await, &tests, "Tests", "Folder");
}

#[tokio::test]
async fn renaming_a_flat_script_to_another_suffix_is_one_class_change() {
	let daemon = fixture(&[("src/X.server.luau", "print('x')\n")]);
	let x = child_ref_in(&daemon, "ServerScriptService", "X").await.expect("no X");
	let (mut plugin, _) = connect_client(&daemon, "plugin", "class-change-plugin").await;

	tokio::time::sleep(SETTLE).await;

	fs::rename(daemon.root.join("src/X.server.luau"), daemon.root.join("src/X.luau")).unwrap();

	assert_one_class_change(&sync_frames(&mut plugin).await, &x, "X", "ModuleScript");
	assert_eq!(
		child_ref_in(&daemon, "ServerScriptService", "X").await.as_deref(),
		Some(x.as_str()),
		"the daemon keeps the same ref for X"
	);
}
