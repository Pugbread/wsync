//! Integration coverage for `wsync refresh` (refresh.json) and its
//! plugin-connect auto-refresh hook, plus the rest of this slice's small
//! surface: `auth` (auth.json), `snapshot` (snapshot.json), `changes`
//! (changes.json), `services` (services.json), and `open` (open.json).

mod common;

use serde_json::{json, Value};
use std::{fs, sync::Arc, time::Duration};

use common::{
	cli_json, cli_stderr, cli_stdout, journal_args, scratch_project, spawn_cli_plugin, start_daemon, start_daemon_in,
	CliAnswer, CliSandbox,
};

// ---------------------------------------------------------------------------
// refresh — the CLI command
// ---------------------------------------------------------------------------

#[test]
fn refresh_writes_all_four_files_and_is_idempotent() {
	let project_dir = scratch_project();
	let sandbox = CliSandbox::new();
	let project = project_dir.path().to_string_lossy().into_owned();

	let output = sandbox.run(&["refresh", "--project", &project, "--raw"]);

	assert!(output.status.success(), "refresh failed: {}", cli_stderr(&output));

	let report = cli_json(&output);
	let files = report["files"].as_array().unwrap();

	assert_eq!(report["ok"], true);
	assert_eq!(files.len(), 4);

	for (index, name) in ["wsync.md", "AGENTS.md", "CLAUDE.md", ".codex/config.toml"]
		.iter()
		.enumerate()
	{
		assert_eq!(files[index]["file"], *name);
		assert_eq!(files[index]["status"], "created", "{name} is created on first run");
		assert!(project_dir.path().join(name).is_file(), "{name} exists on disk");
	}

	// The generated reference sits inside its marker block and renders from
	// the project's real facts
	let wsync_md = fs::read_to_string(project_dir.path().join("wsync.md")).unwrap();

	assert!(wsync_md.contains("wsync:project-memory:start"));
	assert!(wsync_md.contains("ReplicatedStorage"));

	// Byte-identical rerun: every file reports unchanged
	let output = sandbox.run(&["refresh", "--project", &project, "--raw"]);
	let report = cli_json(&output);

	for file in report["files"].as_array().unwrap() {
		assert_eq!(file["status"], "unchanged", "{file}");
	}
}

#[test]
fn refresh_preserves_user_notes_and_skips_unparseable_codex_config() {
	let project_dir = scratch_project();
	let sandbox = CliSandbox::new();
	let project = project_dir.path().to_string_lossy().into_owned();

	// Hand-written content that must survive: a CLAUDE.md of the user's own,
	// and a .codex/config.toml that does not parse as TOML
	fs::write(project_dir.path().join("CLAUDE.md"), "My own claude notes.\n").unwrap();
	fs::create_dir_all(project_dir.path().join(".codex")).unwrap();
	fs::write(project_dir.path().join(".codex/config.toml"), "not [valid toml\n").unwrap();

	let output = sandbox.run(&["refresh", "--project", &project, "--raw"]);

	assert!(output.status.success(), "refresh failed: {}", cli_stderr(&output));

	let report = cli_json(&output);
	let files = report["files"].as_array().unwrap();

	// The user's notes live outside the marker block and survive byte for byte
	let claude_md = fs::read_to_string(project_dir.path().join("CLAUDE.md")).unwrap();

	assert!(claude_md.contains("My own claude notes."));
	assert!(claude_md.contains("@AGENTS.md"));

	let claude = files.iter().find(|file| file["file"] == "CLAUDE.md").unwrap();

	assert_eq!(claude["status"], "updated");

	// The unparseable Codex config is skipped with its reason — and untouched
	let codex = files.iter().find(|file| file["file"] == ".codex/config.toml").unwrap();

	assert_eq!(codex["status"], "skipped");
	assert!(
		codex["reason"].as_str().unwrap().contains("not valid TOML"),
		"the skip carries its reason: {codex}"
	);
	assert_eq!(
		fs::read_to_string(project_dir.path().join(".codex/config.toml")).unwrap(),
		"not [valid toml\n",
		"a skipped file is left byte-identical"
	);
}

#[test]
fn refresh_fails_cleanly_without_a_project() {
	let sandbox = CliSandbox::new();
	let empty = sandbox.work.path().join("nowhere");

	fs::create_dir_all(&empty).unwrap();

	let output = sandbox.run(&["refresh", "--project", &empty.to_string_lossy()]);

	assert!(!output.status.success());
	assert!(cli_stderr(&output).contains("wsync init"));
}

// ---------------------------------------------------------------------------
// The auto-refresh hook — both plugin-connect transports
// ---------------------------------------------------------------------------

/// Polls until the file exists (the hook runs off the handshake path)
fn wait_for_file(path: &std::path::Path, timeout: Duration) -> bool {
	let deadline = std::time::Instant::now() + timeout;

	while std::time::Instant::now() < deadline {
		if path.is_file() {
			return true;
		}

		std::thread::sleep(Duration::from_millis(50));
	}

	false
}

#[tokio::test]
async fn ws_plugin_connect_refreshes_docs_and_debounces_reconnects() {
	let daemon = start_daemon_in(scratch_project());
	let wsync_md = daemon.root.join("wsync.md");

	assert!(!wsync_md.exists(), "the scratch project starts without docs");

	// First plugin connect: the docs appear without any CLI invocation
	let (socket, _hello) = common::connect_client(&daemon, "plugin", "refresh-plugin").await;

	assert!(
		wait_for_file(&wsync_md, Duration::from_secs(10)),
		"plugin connect must regenerate the agent docs"
	);
	assert!(daemon.root.join("AGENTS.md").is_file());
	assert!(daemon.root.join("CLAUDE.md").is_file());
	assert!(daemon.root.join(".codex/config.toml").is_file());

	// Reconnect inside the debounce window: the docs are NOT rewritten, so a
	// reconnect storm cannot thrash the workspace
	drop(socket);
	tokio::time::sleep(Duration::from_millis(300)).await;
	fs::remove_file(&wsync_md).unwrap();

	// The single plugin slot frees asynchronously after the drop
	let mut reconnected = None;

	for _ in 0..40 {
		let (socket, hello) = common::connect_client(&daemon, "plugin", "refresh-plugin-2").await;

		if hello["type"] == "hello" {
			reconnected = Some(socket);
			break;
		}

		tokio::time::sleep(Duration::from_millis(100)).await;
	}

	let _socket = reconnected.expect("the plugin slot frees after the first socket closes");

	assert!(
		!wait_for_file(&wsync_md, Duration::from_secs(2)),
		"a reconnect within the debounce window must not rewrite the docs"
	);
}

#[tokio::test]
async fn msgpack_subscribe_also_refreshes_docs() {
	let daemon = start_daemon_in(scratch_project());
	let wsync_md = daemon.root.join("wsync.md");

	assert!(!wsync_md.exists());

	// The long-poll plugin connect: msgpack POST /subscribe
	let body = rmp_serde::to_vec_named(&json!({ "clientId": 7, "name": "compat-plugin" })).unwrap();
	let response = reqwest::Client::new()
		.post(daemon.http("/subscribe"))
		.header("Content-Type", "application/msgpack")
		.body(body)
		.send()
		.await
		.unwrap();

	assert!(
		response.status().is_success(),
		"subscribe failed: {}",
		response.status()
	);
	assert!(
		wait_for_file(&wsync_md, Duration::from_secs(10)),
		"the msgpack subscribe connect must regenerate the agent docs too"
	);
}

// ---------------------------------------------------------------------------
// auth — never a secret in argv, never a secret printed
// ---------------------------------------------------------------------------

#[test]
fn auth_set_status_clear_round_trip_with_stdin() {
	let sandbox = CliSandbox::new();

	// `set --from-stdin`: the secret arrives on stdin only
	let output = sandbox.run_with_stdin(&["auth", "set", "--from-stdin", "--raw"], b"rbx_secret_key_0042\n");

	assert!(output.status.success(), "auth set failed: {}", cli_stderr(&output));

	let record = cli_json(&output);

	assert_eq!(record["ok"], true);
	assert_eq!(record["configured"], true);
	assert_eq!(record["key"], "robloxCloudApiKey");
	assert_eq!(record["maskedTail"], "…0042", "only the masked tail is ever shown");
	assert!(
		!cli_stdout(&output).contains("rbx_secret_key"),
		"the credential itself must never be printed"
	);

	// The store is the state dir's secrets.json, mode 0600 on Unix
	let store = sandbox.state.path().join("secrets.json");

	assert!(store.is_file());

	#[cfg(unix)]
	{
		use std::os::unix::fs::PermissionsExt;

		let mode = fs::metadata(&store).unwrap().permissions().mode() & 0o777;

		assert_eq!(mode, 0o600, "the credential store must be private to the user");
	}

	let stored: Value = serde_json::from_str(&fs::read_to_string(&store).unwrap()).unwrap();

	assert_eq!(stored["robloxCloudApiKey"], "rbx_secret_key_0042", "stdin is trimmed");

	// status: presence + masked tail, nothing else
	let output = sandbox.run(&["auth", "status", "--raw"]);
	let record = cli_json(&output);

	assert_eq!(record["configured"], true);
	assert_eq!(record["maskedTail"], "…0042");
	assert!(!cli_stdout(&output).contains("rbx_secret_key"));

	// clear: the key goes away, and with it the file
	let output = sandbox.run(&["auth", "clear", "--raw"]);

	assert!(output.status.success());
	assert!(!store.exists(), "clearing the only key removes the store");

	let output = sandbox.run(&["auth", "status", "--raw"]);
	let record = cli_json(&output);

	assert_eq!(record["configured"], false);
	assert!(record.get("maskedTail").is_none());
}

#[test]
fn auth_set_requires_exactly_one_source_and_supports_env_and_file() {
	let sandbox = CliSandbox::new();

	// No source at all
	let output = sandbox.run(&["auth", "set"]);

	assert!(!output.status.success());
	assert!(cli_stderr(&output).contains("exactly one"));

	// Two sources is a clap conflict
	let output = sandbox.run(&["auth", "set", "--from-stdin", "--from-env", "SOME_KEY"]);

	assert!(!output.status.success());

	// An empty credential is refused
	let output = sandbox.run_with_stdin(&["auth", "set", "--from-stdin"], b"   \n");

	assert!(!output.status.success());
	assert!(cli_stderr(&output).contains("empty"));

	// --file works and trims
	let key_file = sandbox.work.path().join("key.txt");

	fs::write(&key_file, "file_key_9War\n").unwrap();

	let output = sandbox.run(&["auth", "set", "--file", &key_file.to_string_lossy(), "--raw"]);

	assert!(
		output.status.success(),
		"auth set --file failed: {}",
		cli_stderr(&output)
	);
	assert_eq!(cli_json(&output)["maskedTail"], "…9War");

	// A missing env var is a clean failure that names the variable
	let output = sandbox.run(&["auth", "set", "--from-env", "WSYNC_TEST_NO_SUCH_VAR"]);

	assert!(!output.status.success());
	assert!(cli_stderr(&output).contains("WSYNC_TEST_NO_SUCH_VAR"));
}

// ---------------------------------------------------------------------------
// snapshot / changes / services / open
// ---------------------------------------------------------------------------

#[test]
fn snapshot_exports_the_live_tree_to_a_file() {
	let daemon = start_daemon(None);
	let sandbox = CliSandbox::new();
	let project = daemon.root.to_string_lossy().into_owned();
	let port = daemon.port.to_string();
	let output_path = sandbox.work.path().join("exports").join("tree.json");

	let output = sandbox.run(&[
		"snapshot",
		"--project",
		&project,
		"--port",
		&port,
		"-o",
		&output_path.to_string_lossy(),
		"--raw",
	]);

	assert!(output.status.success(), "snapshot failed: {}", cli_stderr(&output));

	let record = cli_json(&output);

	assert_eq!(record["ok"], true);
	assert_eq!(record["path"], output_path.to_string_lossy().into_owned());

	// The export is the daemon's own tree projection: the scratch project's
	// service and script are in it
	let exported: Value = serde_json::from_str(&fs::read_to_string(&output_path).unwrap()).unwrap();
	let children = exported["children"].as_array().expect("the export has children");

	assert!(
		children.iter().any(|child| child["name"] == "ReplicatedStorage"),
		"the mapped service is part of the export"
	);
	assert_eq!(
		record["bytes"].as_u64().unwrap(),
		fs::metadata(&output_path).unwrap().len()
	);

	// A directory output receives the default file name
	let out_dir = sandbox.work.path().join("snapdir");

	fs::create_dir_all(&out_dir).unwrap();

	let output = sandbox.run(&[
		"snapshot",
		"--project",
		&project,
		"--port",
		&port,
		"-o",
		&out_dir.to_string_lossy(),
		"--raw",
	]);

	assert!(output.status.success());

	let written = cli_json(&output)["path"].as_str().unwrap().to_owned();

	assert!(
		written.contains("wsync-snapshot-") && written.ends_with(".json"),
		"directory outputs use the default name: {written}"
	);
}

#[test]
fn changes_is_the_diff_alias() {
	let daemon = start_daemon(None);
	let sandbox = CliSandbox::new();
	let project = daemon.root.to_string_lossy().into_owned();
	let port = daemon.port.to_string();

	// No plugin has connected, so no disk review is pending: the alias
	// answers exactly like `diff` — cleanly, exit 0, and silently under --raw
	for command in ["changes", "diff"] {
		let output = sandbox.run(&[command, "--project", &project, "--port", &port, "--raw"]);

		assert!(output.status.success(), "{command} failed: {}", cli_stderr(&output));
		assert_eq!(cli_stdout(&output).trim(), "", "{command} --raw prints no entries");
	}

	// The human rendering names the missing review instead of failing
	let output = sandbox.run(&["changes", "--project", &project, "--port", &port]);

	assert!(output.status.success());
	assert!(cli_stderr(&output).contains("No pending disk review"));
}

#[test]
fn services_lists_roots_with_presence_on_both_sides() {
	let daemon = start_daemon(None);
	let journal = spawn_cli_plugin(
		&daemon,
		"services-plugin",
		Arc::new(|op, args| match op {
			"ls" if args["path"] == "" => CliAnswer::Value(json!({
				"path": "",
				"children": [
					{ "name": "Workspace", "class": "Workspace" },
					{ "name": "ReplicatedStorage", "class": "ReplicatedStorage" },
				],
				"total": 2,
			})),
			_ => CliAnswer::Failure("UNKNOWN_OP", "unexpected op"),
		}),
	);

	let sandbox = CliSandbox::new();
	let project = daemon.root.to_string_lossy().into_owned();
	let port = daemon.port.to_string();

	let output = sandbox.run(&["services", "--project", &project, "--port", &port, "--raw"]);

	assert!(output.status.success(), "services failed: {}", cli_stderr(&output));

	let record = cli_json(&output);

	assert_eq!(record["ok"], true);
	assert_eq!(record["count"], 1, "the scratch project maps exactly one root");

	let root = &record["roots"][0];

	assert_eq!(root["studioPath"], "ReplicatedStorage");
	assert_eq!(root["path"], "src");
	assert_eq!(root["onDisk"], true, "src/ exists in the scratch project");
	assert_eq!(root["inStudio"], true, "the fake DataModel lists ReplicatedStorage");

	// Exactly one listing call: every depth-1 root shares the DataModel `ls`
	assert_eq!(journal_args(&journal, "ls")["path"], "");
}

#[test]
fn open_selects_paths_through_select_set() {
	let daemon = start_daemon(None);
	let journal = spawn_cli_plugin(
		&daemon,
		"open-plugin",
		Arc::new(|op, args| match op {
			"select_set" => CliAnswer::Value(json!({ "count": args["paths"].as_array().map(Vec::len).unwrap_or(0) })),
			_ => CliAnswer::Failure("UNKNOWN_OP", "unexpected op"),
		}),
	);

	let sandbox = CliSandbox::new();
	let project = daemon.root.to_string_lossy().into_owned();
	let port = daemon.port.to_string();

	let output = sandbox.run(&[
		"open",
		"Workspace/Baseplate",
		"ReplicatedStorage/Client",
		"--project",
		&project,
		"--port",
		&port,
		"--raw",
	]);

	assert!(output.status.success(), "open failed: {}", cli_stderr(&output));
	assert_eq!(cli_json(&output)["count"], 2);

	// The positional paths became `select set`'s JSON array — one shared
	// implementation, per open.json
	assert_eq!(
		journal_args(&journal, "select_set")["paths"],
		json!(["Workspace/Baseplate", "ReplicatedStorage/Client"])
	);

	// No paths is a usage error, not an empty selection
	let output = sandbox.run(&["open", "--project", &project, "--port", &port]);

	assert!(!output.status.success());
}
