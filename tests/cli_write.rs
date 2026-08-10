//! Integration coverage for the live *write* CLI surface (Design §10.2,
//! §10.6): the real `wsync` binary runs against a real daemon with a fake
//! Studio plugin on the WebSocket, so the guardrails, the op arguments that
//! actually reach the plugin, the `--raw` shapes, and the exit codes are
//! exercised end to end.
//!
//! The fixture that carries this file is the journalled fake plugin: every
//! `request` frame it receives is recorded before it is answered. That is
//! what makes "refused *before* the network" a testable claim rather than a
//! comment — a pre-network guard leaves the journal empty, and a guard that
//! only the plugin enforces does not.

mod common;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::{
	fs,
	path::{Path, PathBuf},
	process::{Command, Output},
	sync::{Arc, Mutex},
	thread,
	time::Duration,
};
use tempfile::TempDir;
use tokio_tungstenite::tungstenite::Message;

use common::{start_daemon, TestDaemon};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// Isolated environment for one CLI invocation: `$HOME` carries a config that
/// turns off the update check and the plugin installer, so the child process
/// is fast, offline, and cannot disturb a real installation
struct Sandbox {
	home: TempDir,
	state: TempDir,
	/// Scratch space for `--batch` files
	work: TempDir,
}

impl Sandbox {
	fn new() -> Self {
		let base = Path::new(env!("CARGO_TARGET_TMPDIR"));
		fs::create_dir_all(base).unwrap();

		let temp = |prefix: &str| {
			tempfile::Builder::new()
				.prefix(prefix)
				.tempdir_in(base)
				.expect("failed to create a scratch directory")
		};

		let home = temp("wsync-write-home-");
		let config_dir = home.path().join(".wsync");

		fs::create_dir_all(&config_dir).unwrap();
		fs::write(
			config_dir.join("config.toml"),
			"check_updates = false\ninstall_plugin = false\n",
		)
		.unwrap();

		Self {
			home,
			state: temp("wsync-write-state-"),
			work: temp("wsync-write-work-"),
		}
	}

	fn run(&self, args: &[&str]) -> Output {
		Command::new(env!("CARGO_BIN_EXE_wsync"))
			.args(args)
			.env("HOME", self.home.path())
			.env("USERPROFILE", self.home.path())
			.env("WSYNC_STATE_DIR", self.state.path())
			.env("NO_COLOR", "1")
			.env_remove("RUST_LOG")
			.env_remove("RUST_VERBOSE")
			.output()
			.expect("failed to run the wsync binary")
	}

	/// Writes a `--batch` file and returns its path
	fn batch(&self, name: &str, entries: Value) -> PathBuf {
		let path = self.work.path().join(name);

		fs::write(&path, serde_json::to_string_pretty(&entries).unwrap()).unwrap();

		path
	}
}

fn stdout(output: &Output) -> String {
	String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
	String::from_utf8_lossy(&output.stderr).into_owned()
}

/// The single JSON line a `--raw` command prints on stdout
fn raw_json(output: &Output) -> Value {
	let text = stdout(output);
	let line = text
		.lines()
		.find(|line| !line.trim().is_empty())
		.unwrap_or_else(|| panic!("no stdout to parse as JSON; stderr was:\n{}", stderr(output)));

	serde_json::from_str(line).unwrap_or_else(|err| panic!("stdout is not JSON ({err}): {line}"))
}

/// `--raw` for a write leads with `ok` — literally, not just as a key
/// somewhere in the object, so a machine caller can dispatch on the first
/// field of the line it reads
fn assert_leads_with_ok(output: &Output) -> Value {
	let text = stdout(output);
	let line = text.lines().find(|line| !line.trim().is_empty()).unwrap_or_default();

	assert!(
		line.starts_with(r#"{"ok":"#),
		"--raw line does not lead with `ok`: {line}"
	);

	raw_json(output)
}

/// What the fake plugin does with one `request` frame
enum Answer {
	Value(Value),
	Failure(&'static str, &'static str),
}

type Responder = Arc<dyn Fn(&str, &Value) -> Answer + Send + Sync>;

/// Every `(op, args)` the fake plugin received, in order
type Journal = Arc<Mutex<Vec<(String, Value)>>>;

/// Connects a WS client into the daemon's plugin slot, journals every op it
/// is asked for, and answers from `responder` until the connection drops.
/// Returns once the handshake is complete, so the CLI can be launched
/// immediately afterwards
fn spawn_plugin(daemon: &TestDaemon, name: &'static str, responder: Responder) -> Journal {
	let journal: Journal = Arc::new(Mutex::new(Vec::new()));
	let recorder = Arc::clone(&journal);
	let url = daemon.ws_url();
	let (ready_tx, ready_rx) = std::sync::mpsc::channel();

	thread::spawn(move || {
		let runtime = tokio::runtime::Builder::new_current_thread()
			.enable_all()
			.build()
			.unwrap();

		runtime.block_on(async move {
			let (mut socket, _) = tokio_tungstenite::connect_async(url).await.unwrap();

			let hello = json!({
				"type": "hello",
				"clientId": format!("test-{name}"),
				"role": "plugin",
				"protocol": 1,
				"name": name,
			});

			socket.send(Message::Text(hello.to_string())).await.unwrap();

			// The daemon's hello answer means the plugin slot is ours
			let _ = socket.next().await;
			ready_tx.send(()).unwrap();

			loop {
				let message = match tokio::time::timeout(Duration::from_millis(250), socket.next()).await {
					Ok(Some(Ok(message))) => message,
					// Idle tick: nothing to read yet
					Err(_) => continue,
					_ => break,
				};

				let Message::Text(text) = message else {
					continue;
				};

				let Ok(frame) = serde_json::from_str::<Value>(&text) else {
					continue;
				};

				match frame["type"].as_str() {
					Some("ping") => {
						socket.send(Message::Text(r#"{"type":"pong"}"#.into())).await.ok();
					}
					Some("request") => {
						let op = frame["op"].as_str().unwrap_or_default().to_owned();
						let args = frame["args"].clone();

						// Journalled before it is answered: a test that asserts
						// "nothing was sent" must see everything that was
						recorder.lock().unwrap().push((op.clone(), args.clone()));

						let response = match responder(&op, &args) {
							Answer::Value(value) => json!({
								"type": "response",
								"request_id": frame["request_id"],
								"ok": true,
								"value": value,
							}),
							Answer::Failure(code, message) => json!({
								"type": "response",
								"request_id": frame["request_id"],
								"ok": false,
								"error": { "code": code, "message": message },
							}),
						};

						socket.send(Message::Text(response.to_string())).await.ok();
					}
					_ => {}
				}
			}
		});
	});

	ready_rx
		.recv_timeout(Duration::from_secs(15))
		.expect("the fake plugin never completed its handshake");

	journal
}

/// Answers ops from a fixed `op -> value` table and fails anything else, so a
/// test never passes on an op it did not mean to exercise
fn table_responder(entries: Vec<(&'static str, Value)>) -> Responder {
	Arc::new(move |op, _args| match entries.iter().find(|(name, _)| *name == op) {
		Some((_, value)) => Answer::Value(value.clone()),
		None => Answer::Failure("UNKNOWN_OP", "the fake plugin does not implement this op"),
	})
}

/// Ops the fake plugin received, by name
fn ops(journal: &Journal) -> Vec<String> {
	journal.lock().unwrap().iter().map(|(op, _)| op.clone()).collect()
}

/// The arguments of the first `op` the fake plugin received
fn args_for(journal: &Journal, op: &str) -> Value {
	journal
		.lock()
		.unwrap()
		.iter()
		.find(|(name, _)| name == op)
		.map(|(_, args)| args.clone())
		.unwrap_or_else(|| panic!("the fake plugin never received op `{op}`; it saw {:?}", ops(journal)))
}

fn assert_untouched(journal: &Journal, what: &str) {
	let seen = ops(journal);

	assert!(
		seen.is_empty(),
		"{what} reached the plugin before the guard refused it: {seen:?}"
	);
}

// ---------------------------------------------------------------------------
// Guardrails — refused before the network
// ---------------------------------------------------------------------------

#[test]
fn set_parent_is_refused_before_the_request_is_sent() {
	let daemon = start_daemon(None);
	// A plugin that would happily apply the write is connected the whole
	// time: the refusal has to come from the CLI, not from an absent plugin
	let journal = spawn_plugin(
		&daemon,
		"parent-guard-plugin",
		table_responder(vec![("set", json!({ "path": "Workspace/Box", "prop": "Parent" }))]),
	);

	let sandbox = Sandbox::new();
	let output = sandbox.run(&[
		"set",
		"--project",
		&daemon.root.to_string_lossy(),
		"--port",
		&daemon.port.to_string(),
		"--path",
		"Workspace/Box",
		"--prop",
		"Parent",
		"--value",
		r#"{"__type":"InstancePath","path":"ServerStorage"}"#,
	]);

	assert!(!output.status.success(), "a bare Parent write was accepted");

	let message = stderr(&output);

	assert!(message.contains("--force-parent"), "unexpected message: {message}");
	assert!(
		message.contains("wsync mv"),
		"the refusal must name `mv` as the safe route: {message}"
	);

	assert_untouched(&journal, "the Parent write");
}

#[test]
fn force_parent_lets_a_single_parent_write_through() {
	let daemon = start_daemon(None);
	let journal = spawn_plugin(
		&daemon,
		"forced-parent-plugin",
		table_responder(vec![("set", json!({ "path": "ServerStorage/Box", "prop": "Parent" }))]),
	);

	let sandbox = Sandbox::new();
	let output = sandbox.run(&[
		"set",
		"--project",
		&daemon.root.to_string_lossy(),
		"--port",
		&daemon.port.to_string(),
		"--path",
		"Workspace/Box",
		"--prop",
		"Parent",
		"--value",
		r#"{"__type":"InstancePath","path":"ServerStorage"}"#,
		"--force-parent",
	]);

	assert!(output.status.success(), "the forced write failed: {}", stderr(&output));

	// The plugin enforces the guard again, so the flag has to reach it
	assert_eq!(args_for(&journal, "set")["forceParent"], true);
	assert_eq!(
		args_for(&journal, "set")["value"]["__type"],
		"InstancePath",
		"the tagged value must survive the codec"
	);
}

#[test]
fn a_forced_parent_batch_is_still_refused_before_the_network() {
	let daemon = start_daemon(None);
	// A plugin that would happily accept the batch is connected the whole
	// time: the refusal has to come from the CLI. The real plugin rejects a
	// batched Parent write unconditionally (`set_batch` has no `forceParent`
	// argument to relax — Remote/WriteRules.validateBatch), and set.json says
	// "no escape" — so `--force-parent` must not even open a socket for it
	let journal = spawn_plugin(
		&daemon,
		"forced-batch-plugin",
		table_responder(vec![(
			"set_batch",
			json!({ "results": [{ "ok": true }], "total": 1, "applied": 1, "failed": 0, "stopped": false }),
		)]),
	);

	let sandbox = Sandbox::new();
	let batch = sandbox.batch(
		"forced-parent.json",
		json!([{ "path": "Workspace/Box", "prop": "Parent", "value": null }]),
	);

	let output = sandbox.run(&[
		"set",
		"--project",
		&daemon.root.to_string_lossy(),
		"--port",
		&daemon.port.to_string(),
		"--batch",
		&batch.to_string_lossy(),
		"--force-parent",
	]);

	assert!(!output.status.success(), "a forced batched Parent write was accepted");

	// The refusal names the offender, the safe route, and the only escape
	let message = stderr(&output);

	assert!(
		message.contains("entry 1"),
		"the refusal must name the offending entry: {message}"
	);
	assert!(message.contains("wsync mv"), "unexpected message: {message}");
	assert!(
		message.contains("--force-parent"),
		"the single-write escape must be pointed at: {message}"
	);

	// Unconditional means unconditional: nothing was sent
	assert_untouched(&journal, "the forced Parent batch");
}

#[test]
fn a_batch_containing_a_parent_write_is_rejected_whole() {
	let daemon = start_daemon(None);
	let journal = spawn_plugin(
		&daemon,
		"batch-guard-plugin",
		table_responder(vec![(
			"set_batch",
			json!({ "results": [], "total": 0, "applied": 0, "failed": 0, "stopped": false }),
		)]),
	);

	let sandbox = Sandbox::new();
	let batch = sandbox.batch(
		"parent.json",
		json!([
			{ "path": "Workspace/Box", "prop": "Anchored", "value": true },
			{ "path": "Workspace/Box", "prop": "Parent", "value": null },
			{ "path": "Workspace/Box", "prop": "Name", "value": "Crate" },
		]),
	);

	let output = sandbox.run(&[
		"set",
		"--project",
		&daemon.root.to_string_lossy(),
		"--port",
		&daemon.port.to_string(),
		"--batch",
		&batch.to_string_lossy(),
		// `--keep-going` relaxes runtime failures only; it must never relax a
		// pre-flight guard (set.json)
		"--keep-going",
	]);

	assert!(!output.status.success(), "a batch with a Parent write was accepted");

	let message = stderr(&output);

	assert!(
		message.contains("entry 2"),
		"the refusal must name the offending entry: {message}"
	);
	assert!(message.contains("wsync mv"), "unexpected message: {message}");

	// The whole file is rejected: not one of the innocent entries is applied
	assert_untouched(&journal, "the batch");
}

#[test]
fn new_refuses_a_slashed_name_before_the_request() {
	let daemon = start_daemon(None);
	// A plugin that would happily create the instance is connected the whole
	// time: the refusal has to come from the CLI (the real plugin refuses the
	// same write again — Remote/WriteRules.checkNameWrite)
	let journal = spawn_plugin(
		&daemon,
		"slashed-new-plugin",
		table_responder(vec![(
			"new",
			json!({ "path": "Workspace/Sla/shed", "class": "Folder", "name": "Sla/shed" }),
		)]),
	);

	let sandbox = Sandbox::new();
	let output = sandbox.run(&[
		"new",
		"--project",
		&daemon.root.to_string_lossy(),
		"--port",
		&daemon.port.to_string(),
		"--path",
		"Workspace",
		"--class",
		"Folder",
		"--name",
		"Sla/shed",
	]);

	assert!(!output.status.success(), "a slashed --name was accepted");

	// The refusal names the constraint: the path grammar has no escaping, so
	// the created instance could never be addressed again
	let message = stderr(&output);

	assert!(
		message.contains("`/`-separated"),
		"the refusal must name the path-grammar constraint: {message}"
	);
	assert_untouched(&journal, "the slashed --name");
}

#[test]
fn new_refuses_a_slashed_name_in_props_before_the_request() {
	let daemon = start_daemon(None);
	let journal = spawn_plugin(
		&daemon,
		"slashed-props-plugin",
		table_responder(vec![("new", json!({ "path": "Workspace/Box", "class": "Part" }))]),
	);

	let sandbox = Sandbox::new();
	let output = sandbox.run(&[
		"new",
		"--project",
		&daemon.root.to_string_lossy(),
		"--port",
		&daemon.port.to_string(),
		"--path",
		"Workspace",
		"--class",
		"Part",
		// The second name route: a `Name` property write applied on creation
		"--props",
		r#"{"Name":"Sla/shed"}"#,
	]);

	assert!(!output.status.success(), "a slashed Name in --props was accepted");
	assert!(
		stderr(&output).contains("`/`-separated"),
		"the refusal must name the path-grammar constraint: {}",
		stderr(&output)
	);
	assert_untouched(&journal, "the slashed props Name");
}

#[test]
fn set_refuses_a_slashed_name_before_the_request() {
	let daemon = start_daemon(None);
	let journal = spawn_plugin(
		&daemon,
		"slashed-set-plugin",
		table_responder(vec![(
			"set",
			json!({ "path": "Workspace/Test/Re/named", "prop": "Name", "value": "Re/named" }),
		)]),
	);

	let sandbox = Sandbox::new();
	let output = sandbox.run(&[
		"set",
		"--project",
		&daemon.root.to_string_lossy(),
		"--port",
		&daemon.port.to_string(),
		"--path",
		"Workspace/Test/TestPart",
		"--prop",
		"Name",
		"--value",
		r#""Re/named""#,
	]);

	assert!(!output.status.success(), "a slashed Name write was accepted");
	assert!(
		stderr(&output).contains("`/`-separated"),
		"the refusal must name the path-grammar constraint: {}",
		stderr(&output)
	);
	assert_untouched(&journal, "the slashed Name write");
}

#[test]
fn a_batch_containing_a_slashed_name_write_is_rejected_whole() {
	let daemon = start_daemon(None);
	let journal = spawn_plugin(
		&daemon,
		"slashed-batch-plugin",
		table_responder(vec![(
			"set_batch",
			json!({ "results": [], "total": 0, "applied": 0, "failed": 0, "stopped": false }),
		)]),
	);

	let sandbox = Sandbox::new();
	let batch = sandbox.batch(
		"slashed-name.json",
		json!([
			{ "path": "Workspace/Box", "prop": "Anchored", "value": true },
			{ "path": "Workspace/Box", "prop": "Name", "value": "Sla/shed" },
		]),
	);

	let output = sandbox.run(&[
		"set",
		"--project",
		&daemon.root.to_string_lossy(),
		"--port",
		&daemon.port.to_string(),
		"--batch",
		&batch.to_string_lossy(),
		// Like the Parent screen, `--keep-going` must never relax this guard
		"--keep-going",
	]);

	assert!(!output.status.success(), "a batch with a slashed Name write was accepted");

	let message = stderr(&output);

	assert!(
		message.contains("entry 2"),
		"the refusal must name the offending entry: {message}"
	);
	assert!(
		message.contains("`/`-separated"),
		"the refusal must name the path-grammar constraint: {message}"
	);

	// The whole file is rejected: not one of the innocent entries is applied
	assert_untouched(&journal, "the slashed-Name batch");
}

#[test]
fn mv_refuses_a_cross_service_move_and_force_passes_it_through() {
	let daemon = start_daemon(None);
	let journal = spawn_plugin(
		&daemon,
		"mv-guard-plugin",
		table_responder(vec![(
			"mv",
			json!({ "from": "Workspace/Box", "path": "ServerStorage/Box", "parent": "ServerStorage" }),
		)]),
	);

	let sandbox = Sandbox::new();
	let project = daemon.root.to_string_lossy().into_owned();
	let port = daemon.port.to_string();

	let refused = sandbox.run(&[
		"mv",
		"--project",
		&project,
		"--port",
		&port,
		"--from",
		"Workspace/Box",
		"--to",
		"ServerStorage",
	]);

	assert!(!refused.status.success(), "a cross-service move was accepted");
	assert!(
		stderr(&refused).contains("--force"),
		"unexpected message: {}",
		stderr(&refused)
	);
	assert_untouched(&journal, "the cross-service move");

	// The same move with --force is a normal op, and the flag reaches the
	// plugin so its own guard opens too
	let forced = sandbox.run(&[
		"mv",
		"--project",
		&project,
		"--port",
		&port,
		"--from",
		"Workspace/Box",
		"--to",
		"ServerStorage",
		"--force",
	]);

	assert!(forced.status.success(), "the forced move failed: {}", stderr(&forced));
	assert_eq!(args_for(&journal, "mv")["force"], true);
}

#[test]
fn mv_within_one_service_needs_no_force() {
	let daemon = start_daemon(None);
	let journal = spawn_plugin(
		&daemon,
		"mv-plain-plugin",
		table_responder(vec![(
			"mv",
			json!({ "from": "Workspace/Box", "path": "Workspace/Folder/Box", "parent": "Workspace/Folder" }),
		)]),
	);

	let sandbox = Sandbox::new();
	let output = sandbox.run(&[
		"mv",
		"--project",
		&daemon.root.to_string_lossy(),
		"--port",
		&daemon.port.to_string(),
		"--from",
		"Workspace/Box",
		"--to",
		"Workspace/Folder",
	]);

	assert!(output.status.success(), "mv failed: {}", stderr(&output));
	assert_eq!(args_for(&journal, "mv")["force"], false);
	assert!(
		stderr(&output).contains("Workspace/Folder/Box"),
		"unexpected output: {}",
		stderr(&output)
	);
}

#[test]
fn set_needs_a_complete_single_write_or_a_batch() {
	let daemon = start_daemon(None);
	let journal = spawn_plugin(&daemon, "incomplete-set-plugin", table_responder(vec![]));

	let sandbox = Sandbox::new();
	let output = sandbox.run(&[
		"set",
		"--project",
		&daemon.root.to_string_lossy(),
		"--port",
		&daemon.port.to_string(),
		"--path",
		"Workspace/Box",
	]);

	assert!(!output.status.success(), "an incomplete set was accepted");
	assert!(
		stderr(&output).contains("--value"),
		"unexpected message: {}",
		stderr(&output)
	);
	assert_untouched(&journal, "the incomplete set");
}

#[test]
fn a_malformed_batch_file_never_reaches_the_plugin() {
	let daemon = start_daemon(None);
	let journal = spawn_plugin(&daemon, "malformed-batch-plugin", table_responder(vec![]));

	let sandbox = Sandbox::new();
	let project = daemon.root.to_string_lossy().into_owned();
	let port = daemon.port.to_string();

	// Not an array
	let object = sandbox.batch("object.json", json!({ "path": "Workspace/Box" }));
	let output = sandbox.run(&[
		"set",
		"--project",
		&project,
		"--port",
		&port,
		"--batch",
		&object.to_string_lossy(),
	]);

	assert!(!output.status.success(), "a non-array batch was accepted");
	assert!(
		stderr(&output).contains("JSON array"),
		"unexpected message: {}",
		stderr(&output)
	);

	// An entry with no `prop`
	let incomplete = sandbox.batch("incomplete.json", json!([{ "path": "Workspace/Box", "value": 1 }]));
	let output = sandbox.run(&[
		"set",
		"--project",
		&project,
		"--port",
		&port,
		"--batch",
		&incomplete.to_string_lossy(),
	]);

	assert!(!output.status.success(), "an entry without `prop` was accepted");
	assert!(
		stderr(&output).contains("entry 1"),
		"unexpected message: {}",
		stderr(&output)
	);

	assert_untouched(&journal, "the malformed batches");
}

// ---------------------------------------------------------------------------
// Batches: waypoint passthrough, keep-going, per-entry reporting
// ---------------------------------------------------------------------------

/// `results` are positional against the writes that were sent, so the table
/// can name a target the plugin never echoes back
fn batch_value(results: Value, applied: u64, failed: u64, stopped: bool) -> Value {
	let total = results.as_array().map_or(0, Vec::len) as u64;

	json!({
		"results": results,
		"total": total,
		"applied": applied,
		"failed": failed,
		"stopped": stopped,
	})
}

#[test]
fn a_batch_sends_one_op_carrying_the_waypoint_and_keep_going() {
	let daemon = start_daemon(None);
	let journal = spawn_plugin(
		&daemon,
		"batch-waypoint-plugin",
		table_responder(vec![(
			"set_batch",
			batch_value(json!([{ "ok": true }, { "ok": true }]), 2, 0, false),
		)]),
	);

	let sandbox = Sandbox::new();
	let batch = sandbox.batch(
		"writes.json",
		json!([
			{ "path": "Workspace/Camera", "prop": "FieldOfView", "value": 90 },
			{ "path": "Workspace/Box", "prop": "Anchored", "value": true },
		]),
	);

	let output = sandbox.run(&[
		"set",
		"--project",
		&daemon.root.to_string_lossy(),
		"--port",
		&daemon.port.to_string(),
		"--batch",
		&batch.to_string_lossy(),
		"--waypoint",
		"refactor camera",
		"--keep-going",
	]);

	assert!(output.status.success(), "the batch failed: {}", stderr(&output));

	// One op for the whole file, not one per entry
	assert_eq!(ops(&journal), vec!["set_batch".to_owned()]);

	let args = args_for(&journal, "set_batch");

	assert_eq!(args["waypoint"], "refactor camera");
	assert_eq!(args["keepGoing"], true);
	assert_eq!(args["writes"].as_array().unwrap().len(), 2);
	assert_eq!(args["writes"][0]["path"], "Workspace/Camera");
	assert_eq!(args["writes"][0]["prop"], "FieldOfView");
	assert_eq!(args["writes"][0]["value"], 90);

	// Every write is accounted for in the table
	let table = stdout(&output);

	assert!(table.contains("Workspace/Camera"), "unexpected table: {table}");
	assert!(table.contains("2 applied, 0 failed"), "unexpected table: {table}");
}

#[test]
fn a_batch_that_stops_at_the_first_failure_reports_and_exits_non_zero() {
	let daemon = start_daemon(None);
	let journal = spawn_plugin(
		&daemon,
		"batch-stop-plugin",
		table_responder(vec![(
			"set_batch",
			batch_value(
				json!([
					{ "ok": true },
					{ "ok": false, "error": { "code": "NOT_FOUND", "message": "instance not found" } },
					{ "ok": false, "skipped": true },
				]),
				1,
				1,
				true,
			),
		)]),
	);

	let sandbox = Sandbox::new();
	let batch = sandbox.batch(
		"partial.json",
		json!([
			{ "path": "Workspace/Camera", "prop": "FieldOfView", "value": 90 },
			{ "path": "Workspace/Missing", "prop": "Anchored", "value": true },
			{ "path": "Workspace/Box", "prop": "Name", "value": "Crate" },
		]),
	);

	let output = sandbox.run(&[
		"set",
		"--project",
		&daemon.root.to_string_lossy(),
		"--port",
		&daemon.port.to_string(),
		"--batch",
		&batch.to_string_lossy(),
	]);

	// Without --keep-going the plugin is told to stop on the first failure
	assert_eq!(args_for(&journal, "set_batch")["keepGoing"], false);

	// A partially applied batch is a failed command
	assert!(!output.status.success(), "a partial batch exited zero");

	let table = stdout(&output);

	assert!(table.contains("NOT_FOUND"), "the failure detail is missing: {table}");
	assert!(
		table.contains("Workspace/Missing"),
		"the failing target is missing: {table}"
	);
	assert!(table.contains("skipped"), "the un-attempted write is missing: {table}");
	assert!(table.contains("1 applied, 1 failed"), "the summary is missing: {table}");
	assert!(
		stderr(&output).contains("failed"),
		"unexpected error line: {}",
		stderr(&output)
	);
}

#[test]
fn keep_going_still_exits_non_zero_when_an_entry_failed() {
	let daemon = start_daemon(None);
	let journal = spawn_plugin(
		&daemon,
		"batch-keep-going-plugin",
		table_responder(vec![(
			"set_batch",
			batch_value(
				json!([
					{ "ok": false, "error": { "code": "NOT_FOUND", "message": "instance not found" } },
					{ "ok": true },
				]),
				1,
				1,
				false,
			),
		)]),
	);

	let sandbox = Sandbox::new();
	let batch = sandbox.batch(
		"keep-going.json",
		json!([
			{ "path": "Workspace/Missing", "prop": "Anchored", "value": true },
			{ "path": "Workspace/Box", "prop": "Name", "value": "Crate" },
		]),
	);

	let output = sandbox.run(&[
		"set",
		"--project",
		&daemon.root.to_string_lossy(),
		"--port",
		&daemon.port.to_string(),
		"--batch",
		&batch.to_string_lossy(),
		"--keep-going",
	]);

	assert_eq!(args_for(&journal, "set_batch")["keepGoing"], true);
	assert!(
		!output.status.success(),
		"--keep-going must relax the run, not the exit code"
	);

	let table = stdout(&output);

	// The second write ran despite the first failing — that is what
	// --keep-going buys
	assert!(table.contains("1 applied, 1 failed"), "unexpected table: {table}");
	assert!(!table.contains("skipped"), "nothing should have been skipped: {table}");
}

#[test]
fn a_single_write_marks_the_waypoint_before_the_set() {
	let daemon = start_daemon(None);
	let journal = spawn_plugin(
		&daemon,
		"single-waypoint-plugin",
		table_responder(vec![
			("waypoint", json!({ "name": "before tweak" })),
			(
				"set",
				json!({ "path": "Workspace/Camera", "prop": "FieldOfView", "value": 90 }),
			),
		]),
	);

	let sandbox = Sandbox::new();
	let output = sandbox.run(&[
		"set",
		"--project",
		&daemon.root.to_string_lossy(),
		"--port",
		&daemon.port.to_string(),
		"--path",
		"Workspace/Camera",
		"--prop",
		"FieldOfView",
		"--value",
		"90",
		"--waypoint",
		"before tweak",
	]);

	assert!(output.status.success(), "set failed: {}", stderr(&output));

	// The marker has to precede the write, or it labels the wrong boundary
	assert_eq!(ops(&journal), vec!["waypoint".to_owned(), "set".to_owned()]);
	assert_eq!(args_for(&journal, "waypoint")["name"], "before tweak");
}

// ---------------------------------------------------------------------------
// Happy paths
// ---------------------------------------------------------------------------

#[test]
fn set_reports_the_value_studio_kept() {
	let daemon = start_daemon(None);
	let journal = spawn_plugin(
		&daemon,
		"set-plugin",
		table_responder(vec![(
			"set",
			// The readback is what Studio normalized the write to
			json!({ "path": "Workspace/Camera", "prop": "FieldOfView", "value": 89.5 }),
		)]),
	);

	let sandbox = Sandbox::new();
	let output = sandbox.run(&[
		"set",
		"--project",
		&daemon.root.to_string_lossy(),
		"--port",
		&daemon.port.to_string(),
		"--path",
		"Workspace/Camera",
		"--prop",
		"FieldOfView",
		"--value",
		"90",
	]);

	assert!(output.status.success(), "set failed: {}", stderr(&output));
	assert_eq!(args_for(&journal, "set")["forceParent"], false);
	assert_eq!(args_for(&journal, "set")["value"], 90);
	assert!(
		stderr(&output).contains("89.5"),
		"the readback must be reported: {}",
		stderr(&output)
	);
}

#[test]
fn set_takes_a_bare_string_that_is_not_valid_json() {
	let daemon = start_daemon(None);
	let journal = spawn_plugin(
		&daemon,
		"bare-string-plugin",
		table_responder(vec![(
			"set",
			json!({ "path": "Workspace/Box", "prop": "Name", "value": "Crate" }),
		)]),
	);

	let sandbox = Sandbox::new();
	let output = sandbox.run(&[
		"set",
		"--project",
		&daemon.root.to_string_lossy(),
		"--port",
		&daemon.port.to_string(),
		"--path",
		"Workspace/Box",
		"--prop",
		"Name",
		"--value",
		"Crate",
	]);

	assert!(output.status.success(), "set failed: {}", stderr(&output));
	// The find-attr codec: bare text that is not JSON is a string
	assert_eq!(args_for(&journal, "set")["value"], "Crate");
}

#[test]
fn new_prints_the_created_path() {
	let daemon = start_daemon(None);
	let journal = spawn_plugin(
		&daemon,
		"new-plugin",
		table_responder(vec![(
			"new",
			json!({ "path": "Workspace/Box", "class": "Part", "name": "Box" }),
		)]),
	);

	let sandbox = Sandbox::new();
	let output = sandbox.run(&[
		"new",
		"--project",
		&daemon.root.to_string_lossy(),
		"--port",
		&daemon.port.to_string(),
		"--path",
		"Workspace",
		"--class",
		"Part",
		"--name",
		"Box",
		"--props",
		r#"{"Anchored":true}"#,
	]);

	assert!(output.status.success(), "new failed: {}", stderr(&output));

	// The created path is the whole point: it is what follow-up commands
	// address, so it goes to stdout on its own line
	assert_eq!(stdout(&output).trim(), "Workspace/Box");

	let args = args_for(&journal, "new");

	assert_eq!(args["path"], "Workspace");
	assert_eq!(args["class"], "Part");
	assert_eq!(args["name"], "Box");
	assert_eq!(args["props"]["Anchored"], true);
}

#[test]
fn new_validates_props_before_the_request() {
	let daemon = start_daemon(None);
	let journal = spawn_plugin(&daemon, "new-props-plugin", table_responder(vec![]));

	let sandbox = Sandbox::new();
	let output = sandbox.run(&[
		"new",
		"--project",
		&daemon.root.to_string_lossy(),
		"--port",
		&daemon.port.to_string(),
		"--path",
		"Workspace",
		"--class",
		"Part",
		"--props",
		"[1,2,3]",
	]);

	assert!(!output.status.success(), "an array --props was accepted");
	assert!(
		stderr(&output).contains("JSON object"),
		"unexpected message: {}",
		stderr(&output)
	);
	assert_untouched(&journal, "the malformed --props");
}

#[test]
fn rm_reports_what_it_destroyed() {
	let daemon = start_daemon(None);
	let journal = spawn_plugin(
		&daemon,
		"rm-plugin",
		table_responder(vec![(
			"rm",
			json!({ "path": "Workspace/Box", "class": "Part", "destroyed": true }),
		)]),
	);

	let sandbox = Sandbox::new();
	let output = sandbox.run(&[
		"rm",
		"--project",
		&daemon.root.to_string_lossy(),
		"--port",
		&daemon.port.to_string(),
		"--path",
		"Workspace/Box",
	]);

	assert!(output.status.success(), "rm failed: {}", stderr(&output));
	assert_eq!(args_for(&journal, "rm")["path"], "Workspace/Box");
	assert!(
		stderr(&output).contains("Workspace/Box"),
		"unexpected output: {}",
		stderr(&output)
	);
}

#[test]
fn attr_set_rm_and_ls_map_onto_their_ops() {
	let daemon = start_daemon(None);
	let journal = spawn_plugin(
		&daemon,
		"attr-plugin",
		table_responder(vec![
			("set_attr", json!({ "path": "Workspace/Box", "name": "Speed" })),
			(
				"rm_attr",
				json!({ "path": "Workspace/Box", "name": "Speed", "cleared": true }),
			),
			(
				"attr_ls",
				json!({ "path": "Workspace/Box", "count": 1, "attributes": { "Speed": 12.5 } }),
			),
		]),
	);

	let sandbox = Sandbox::new();
	let project = daemon.root.to_string_lossy().into_owned();
	let port = daemon.port.to_string();

	let set = sandbox.run(&[
		"attr",
		"set",
		"--project",
		&project,
		"--port",
		&port,
		"--path",
		"Workspace/Box",
		"--name",
		"Speed",
		"--value",
		"12.5",
	]);

	assert!(set.status.success(), "attr set failed: {}", stderr(&set));
	assert_eq!(args_for(&journal, "set_attr")["value"], 12.5);

	let removed = sandbox.run(&[
		"attr",
		"rm",
		"--project",
		&project,
		"--port",
		&port,
		"--path",
		"Workspace/Box",
		"--name",
		"Speed",
	]);

	assert!(removed.status.success(), "attr rm failed: {}", stderr(&removed));
	assert_eq!(args_for(&journal, "rm_attr")["name"], "Speed");

	let listed = sandbox.run(&[
		"attr",
		"ls",
		"--project",
		&project,
		"--port",
		&port,
		"--path",
		"Workspace/Box",
	]);

	assert!(listed.status.success(), "attr ls failed: {}", stderr(&listed));
	assert!(
		stdout(&listed).contains("Speed") && stdout(&listed).contains("12.5"),
		"unexpected listing: {}",
		stdout(&listed)
	);
	assert!(stdout(&listed).contains("1 attribute(s)"));
}

#[test]
fn tag_add_is_idempotent_and_says_so() {
	let daemon = start_daemon(None);
	let journal = spawn_plugin(
		&daemon,
		"tag-plugin",
		Arc::new(|op, args| match op {
			// The second add of the same tag is a no-op the plugin reports
			// rather than a write
			"add_tag" if args["tag"] == json!("Enemy") => Answer::Value(json!({
				"path": "Workspace/Box", "tag": "Enemy", "added": false, "already": true,
			})),
			"add_tag" => Answer::Value(json!({ "path": "Workspace/Box", "tag": "Boss", "added": true })),
			"rm_tag" => Answer::Value(json!({ "path": "Workspace/Box", "tag": "Enemy", "removed": true })),
			_ => Answer::Failure("UNKNOWN_OP", "the fake plugin does not implement this op"),
		}),
	);

	let sandbox = Sandbox::new();
	let project = daemon.root.to_string_lossy().into_owned();
	let port = daemon.port.to_string();

	let fresh = sandbox.run(&[
		"tag",
		"add",
		"--project",
		&project,
		"--port",
		&port,
		"--path",
		"Workspace/Box",
		"--tag",
		"Boss",
	]);

	assert!(fresh.status.success(), "tag add failed: {}", stderr(&fresh));
	assert!(
		stderr(&fresh).contains("Tagged"),
		"unexpected output: {}",
		stderr(&fresh)
	);

	let repeat = sandbox.run(&[
		"tag",
		"add",
		"--project",
		&project,
		"--port",
		&port,
		"--path",
		"Workspace/Box",
		"--tag",
		"Enemy",
	]);

	assert!(repeat.status.success(), "an idempotent add must succeed");
	assert!(
		stderr(&repeat).contains("already"),
		"a no-op add must be distinguishable from a write: {}",
		stderr(&repeat)
	);

	let removed = sandbox.run(&[
		"tag",
		"rm",
		"--project",
		&project,
		"--port",
		&port,
		"--path",
		"Workspace/Box",
		"--tag",
		"Enemy",
	]);

	assert!(removed.status.success(), "tag rm failed: {}", stderr(&removed));
	assert_eq!(args_for(&journal, "rm_tag")["tag"], "Enemy");
}

#[test]
fn call_pretty_prints_the_results_array() {
	let daemon = start_daemon(None);
	let journal = spawn_plugin(
		&daemon,
		"call-plugin",
		table_responder(vec![(
			"call",
			json!({
				"path": "Workspace/Folder",
				"method": "FindFirstChild",
				"count": 1,
				"results": [{ "__type": "InstancePath", "path": "Workspace/Folder/Box" }],
			}),
		)]),
	);

	let sandbox = Sandbox::new();
	let output = sandbox.run(&[
		"call",
		"--project",
		&daemon.root.to_string_lossy(),
		"--port",
		&daemon.port.to_string(),
		"--path",
		"Workspace/Folder",
		"--method",
		"FindFirstChild",
		"--args",
		r#"["Box"]"#,
	]);

	assert!(output.status.success(), "call failed: {}", stderr(&output));
	assert_eq!(args_for(&journal, "call")["args"][0], "Box");
	assert!(
		stdout(&output).contains("InstancePath"),
		"the tagged result must be rendered: {}",
		stdout(&output)
	);
}

#[test]
fn call_validates_args_before_the_request() {
	let daemon = start_daemon(None);
	let journal = spawn_plugin(&daemon, "call-args-plugin", table_responder(vec![]));

	let sandbox = Sandbox::new();
	let output = sandbox.run(&[
		"call",
		"--project",
		&daemon.root.to_string_lossy(),
		"--port",
		&daemon.port.to_string(),
		"--path",
		"Workspace/Folder",
		"--method",
		"Destroy",
		"--args",
		r#"{"name":"Box"}"#,
	]);

	assert!(!output.status.success(), "a non-array --args was accepted");
	assert!(
		stderr(&output).contains("JSON array"),
		"unexpected message: {}",
		stderr(&output)
	);
	// A method with side effects must not run on a malformed argument list
	assert_untouched(&journal, "the malformed --args");
}

#[test]
fn eval_returns_values_and_fails_on_a_compile_error() {
	let daemon = start_daemon(None);
	let journal = spawn_plugin(
		&daemon,
		"eval-plugin",
		Arc::new(|op, args| match op {
			"eval" if args["source"].as_str().is_some_and(|source| source.contains("return")) => {
				Answer::Value(json!({ "count": 1, "results": [12] }))
			}
			// The plugin classifies a compile failure as INVALID_ARGUMENT
			"eval" => Answer::Failure("INVALID_ARGUMENT", "compile error: unexpected symbol near '?'"),
			_ => Answer::Failure("UNKNOWN_OP", "the fake plugin does not implement this op"),
		}),
	);

	let sandbox = Sandbox::new();
	let project = daemon.root.to_string_lossy().into_owned();
	let port = daemon.port.to_string();

	let ok = sandbox.run(&[
		"eval",
		"--project",
		&project,
		"--port",
		&port,
		"--source",
		"return #game.Workspace:GetChildren()",
	]);

	assert!(ok.status.success(), "eval failed: {}", stderr(&ok));
	assert!(stdout(&ok).contains("12"), "unexpected output: {}", stdout(&ok));
	assert_eq!(
		args_for(&journal, "eval")["source"],
		"return #game.Workspace:GetChildren()"
	);

	let broken = sandbox.run(&["eval", "--project", &project, "--port", &port, "--source", "?"]);

	assert!(!broken.status.success(), "a compile error exited zero");

	let message = stderr(&broken);

	assert!(message.contains("INVALID_ARGUMENT"), "unexpected message: {message}");
	assert!(message.contains("compile error"), "unexpected message: {message}");
}

#[test]
fn save_waypoint_undo_and_redo_each_wrap_one_op() {
	let daemon = start_daemon(None);
	let journal = spawn_plugin(
		&daemon,
		"history-plugin",
		table_responder(vec![
			("save", json!({ "requested": true })),
			("waypoint", json!({ "name": "before refactor" })),
			("undo", json!({ "requested": true })),
			("redo", json!({ "requested": true })),
		]),
	);

	let sandbox = Sandbox::new();
	let project = daemon.root.to_string_lossy().into_owned();
	let port = daemon.port.to_string();

	for args in [
		vec!["save", "--project", &project, "--port", &port],
		vec![
			"waypoint",
			"--project",
			&project,
			"--port",
			&port,
			"--name",
			"before refactor",
		],
		vec!["undo", "--project", &project, "--port", &port],
		vec!["redo", "--project", &project, "--port", &port],
	] {
		let output = sandbox.run(&args);

		assert!(output.status.success(), "{args:?} failed: {}", stderr(&output));
	}

	assert_eq!(
		ops(&journal),
		vec![
			"save".to_owned(),
			"waypoint".to_owned(),
			"undo".to_owned(),
			"redo".to_owned()
		]
	);
	assert_eq!(args_for(&journal, "waypoint")["name"], "before refactor");
}

// ---------------------------------------------------------------------------
// `--raw` shapes
// ---------------------------------------------------------------------------

#[test]
fn raw_write_output_leads_with_ok() {
	let daemon = start_daemon(None);
	spawn_plugin(
		&daemon,
		"raw-plugin",
		table_responder(vec![
			(
				"set",
				json!({ "path": "Workspace/Camera", "prop": "FieldOfView", "value": 90 }),
			),
			(
				"new",
				json!({ "path": "Workspace/Box", "class": "Part", "name": "Box" }),
			),
			("save", json!({ "requested": true })),
		]),
	);

	let sandbox = Sandbox::new();
	let project = daemon.root.to_string_lossy().into_owned();
	let port = daemon.port.to_string();

	let set = sandbox.run(&[
		"set",
		"--project",
		&project,
		"--port",
		&port,
		"--path",
		"Workspace/Camera",
		"--prop",
		"FieldOfView",
		"--value",
		"90",
		"--raw",
	]);

	assert!(set.status.success(), "set --raw failed: {}", stderr(&set));

	let value = assert_leads_with_ok(&set);

	assert_eq!(value["ok"], true);
	// The op's own fields are merged in, not nested, so a caller reads the
	// same keys the op contract documents
	assert_eq!(value["path"], "Workspace/Camera");
	assert_eq!(value["prop"], "FieldOfView");
	assert_eq!(value["value"], 90);

	let created = sandbox.run(&[
		"new",
		"--project",
		&project,
		"--port",
		&port,
		"--path",
		"Workspace",
		"--class",
		"Part",
		"--raw",
	]);

	assert!(created.status.success(), "new --raw failed: {}", stderr(&created));

	let value = assert_leads_with_ok(&created);

	assert_eq!(value["class"], "Part");

	// An argument-less op still answers with the same discriminator
	let saved = sandbox.run(&["save", "--project", &project, "--port", &port, "--raw"]);

	assert!(saved.status.success(), "save --raw failed: {}", stderr(&saved));
	assert_eq!(assert_leads_with_ok(&saved)["requested"], true);
}

#[test]
fn a_failed_write_still_prints_a_raw_envelope() {
	let daemon = start_daemon(None);
	spawn_plugin(
		&daemon,
		"raw-failure-plugin",
		Arc::new(|_op, _args| Answer::Failure("NOT_FOUND", "instance 'Workspace/Missing' not found")),
	);

	let sandbox = Sandbox::new();
	let output = sandbox.run(&[
		"rm",
		"--project",
		&daemon.root.to_string_lossy(),
		"--port",
		&daemon.port.to_string(),
		"--path",
		"Workspace/Missing",
		"--raw",
	]);

	assert!(!output.status.success(), "a failed write exited zero");

	// A machine caller gets one parseable line even on failure, and it leads
	// with the same key the success shape does
	let value = raw_json(&output);

	assert_eq!(value["ok"], false);
	assert_eq!(value["error"]["code"], "NOT_FOUND");
	assert!(
		stdout(&output).trim_start().starts_with(r#"{"ok":false"#),
		"the failure envelope must lead with `ok`: {}",
		stdout(&output)
	);
}

#[test]
fn a_batch_raw_line_carries_the_parallel_results() {
	let daemon = start_daemon(None);
	spawn_plugin(
		&daemon,
		"raw-batch-plugin",
		table_responder(vec![(
			"set_batch",
			batch_value(
				json!([
					{ "ok": true },
					{ "ok": false, "error": { "code": "NOT_FOUND", "message": "instance not found" } },
				]),
				1,
				1,
				true,
			),
		)]),
	);

	let sandbox = Sandbox::new();
	let batch = sandbox.batch(
		"raw.json",
		json!([
			{ "path": "Workspace/Camera", "prop": "FieldOfView", "value": 90 },
			{ "path": "Workspace/Missing", "prop": "Anchored", "value": true },
		]),
	);

	let output = sandbox.run(&[
		"set",
		"--project",
		&daemon.root.to_string_lossy(),
		"--port",
		&daemon.port.to_string(),
		"--batch",
		&batch.to_string_lossy(),
		"--raw",
	]);

	assert!(!output.status.success(), "a partial batch exited zero");

	let value = assert_leads_with_ok(&output);

	assert_eq!(value["total"], 2);
	assert_eq!(value["applied"], 1);
	assert_eq!(value["failed"], 1);
	assert_eq!(value["stopped"], true);
	assert_eq!(value["results"][0]["ok"], true);
	assert_eq!(value["results"][1]["error"]["code"], "NOT_FOUND");
}

// ---------------------------------------------------------------------------
// `status` write-log reporting
// ---------------------------------------------------------------------------

#[test]
fn status_reports_the_write_log_path_and_whether_it_exists() {
	let daemon = start_daemon(None);
	let sandbox = Sandbox::new();

	let output = sandbox.run(&[
		"status",
		"--project",
		&daemon.root.to_string_lossy(),
		"--port",
		&daemon.port.to_string(),
		"--raw",
	]);

	assert!(output.status.success(), "status failed: {}", stderr(&output));

	let value = raw_json(&output);
	let log = &value["writeLog"];
	let path = log["path"].as_str().expect("writeLog.path must be a string");

	assert!(!path.is_empty(), "the audit-log location is never left unreported");
	assert_eq!(
		log["present"],
		Path::new(path).exists(),
		"`present` must describe the reported path"
	);

	// Additive shape: the read-only slice's keys survive, and `source` says
	// whether the path came from the daemon or from the CLI's own default
	assert!(log.get("pending").is_some(), "`pending` must survive: {log}");
	assert!(
		matches!(log["source"].as_str(), Some("daemon" | "state directory")),
		"unexpected writeLog.source: {log}"
	);

	// A daemon that reports its own location is not pending; one that does
	// not leaves the CLI's default flagged as a guess
	assert_eq!(log["pending"], log["source"] == "state directory");
}
