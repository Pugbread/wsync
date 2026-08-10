//! Integration coverage for the registry surface (`commands`, `context`),
//! the path tools (`path`, `meta`, `where`), and the `tail` alias-command:
//! the real binary against the embedded bundle, a daemon-less project, and a
//! real daemon whose `path`/`meta`/`where` ops answer without any plugin.

mod common;

use serde_json::Value;
use std::{
	fs,
	io::{BufRead, BufReader},
	net::TcpListener,
	sync::{mpsc, Arc},
	thread,
	time::{Duration, Instant},
};

use common::{
	cli_json, cli_stderr, cli_stdout, journal_args, journal_ops, scratch_project, spawn_cli_plugin, start_daemon,
	CliAnswer, CliSandbox,
};

/// A localhost port nothing serves, so offline tests cannot accidentally hit
/// a real daemon on the developer's machine
fn dead_port() -> String {
	let listener = TcpListener::bind("127.0.0.1:0").unwrap();
	let port = listener.local_addr().unwrap().port();

	drop(listener);

	port.to_string()
}

// ---------------------------------------------------------------------------
// commands — fully offline
// ---------------------------------------------------------------------------

#[test]
fn commands_prints_the_embedded_bundle_verbatim() {
	let sandbox = CliSandbox::new();
	let output = sandbox.run(&["commands"]);

	assert!(output.status.success(), "commands failed: {}", cli_stderr(&output));

	// Byte-for-byte the repository bundle (commands.json: "embedded verbatim")
	let bundle_path = concat!(env!("CARGO_MANIFEST_DIR"), "/docs/client-commands.generated.json");
	let bundle = fs::read_to_string(bundle_path).unwrap();

	assert_eq!(cli_stdout(&output).trim_end(), bundle.trim_end());

	let parsed = cli_json(&output);

	assert_eq!(parsed["schemaVersion"], 1);
	assert!(
		parsed["commands"].as_array().unwrap().len() >= 58,
		"the bundle lost commands"
	);
}

#[test]
fn commands_serves_one_entry_and_the_compact_index() {
	let sandbox = CliSandbox::new();

	// One command's full entry
	let output = sandbox.run(&["commands", "get"]);

	assert!(output.status.success());

	let entry = cli_json(&output);

	assert_eq!(entry["name"], "get");
	assert!(entry["usage"].as_str().unwrap().starts_with("wsync get"));
	assert!(entry.get("notes").is_some(), "the full entry keeps its notes");

	// One command, compact: exactly the choosing fields
	let output = sandbox.run(&["commands", "get", "--compact"]);
	let compact = cli_json(&output);
	let keys: Vec<&String> = compact.as_object().unwrap().keys().collect();

	assert_eq!(keys, ["name", "category", "description", "usage"]);

	// The compact index groups name + description by category and carries
	// every implemented command
	let output = sandbox.run(&["commands", "--compact"]);

	assert!(output.status.success());

	let index = cli_json(&output);
	let categories = index["categories"].as_array().unwrap();

	assert!(!categories.is_empty());

	let mut listed = Vec::new();

	for category in categories {
		for command in category["commands"].as_array().unwrap() {
			assert!(command.get("name").is_some() && command.get("description").is_some());
			assert!(
				command.get("usage").is_none() && command.get("notes").is_none(),
				"the compact index must stay compact: {command}"
			);

			listed.push(command["name"].as_str().unwrap().to_owned());
		}
	}

	let manifest_path = concat!(env!("CARGO_MANIFEST_DIR"), "/scripts/data/implemented-commands.json");
	let manifest: Value = serde_json::from_str(&fs::read_to_string(manifest_path).unwrap()).unwrap();

	for name in manifest["commands"].as_array().unwrap() {
		let name = name.as_str().unwrap();

		assert!(
			listed.iter().any(|listed| listed == name),
			"implemented command `{name}` is missing from the compact index"
		);
	}

	// Unknown names fail with a pointer at the index
	let output = sandbox.run(&["commands", "no-such-command"]);

	assert!(!output.status.success());
	assert!(cli_stderr(&output).contains("--compact"));
}

// ---------------------------------------------------------------------------
// context — offline and online
// ---------------------------------------------------------------------------

#[test]
fn context_answers_offline_from_config_and_disk_facts() {
	let project_dir = scratch_project();
	let sandbox = CliSandbox::new();
	let project = project_dir.path().to_string_lossy().into_owned();
	let port = dead_port();

	let output = sandbox.run(&["context", "--project", &project, "--port", &port]);

	assert!(output.status.success(), "context failed: {}", cli_stderr(&output));

	let snapshot = cli_json(&output);

	assert_eq!(snapshot["ok"], true);
	assert_eq!(snapshot["project"]["name"], "wsync-fixture");
	assert_eq!(snapshot["project"]["parses"], true);
	assert_eq!(snapshot["project"]["gameId"], 5550001);
	assert_eq!(
		snapshot["syncedRoots"][0],
		serde_json::json!({ "instancePath": "ReplicatedStorage", "path": "src" })
	);
	assert_eq!(snapshot["daemon"]["reachable"], false);
	assert_eq!(snapshot["plugin"]["connected"], false);
	assert_eq!(snapshot["conflicts"], Value::Null);
	assert_eq!(snapshot["generatedDocs"]["wsync.md"], false);

	// Registry pointers, not the registry
	assert!(snapshot["commands"]["index"].as_str().unwrap().contains("--compact"));
	assert!(snapshot["commands"].get("commands").is_none());

	// The llmPolicy block carries the command budget (Design 10.6)
	let policy = serde_json::to_string(&snapshot["llmPolicy"]).unwrap();

	for expected in [
		"commands --compact",
		"lint --path",
		"startup ritual",
		"wsync mv",
		"escape hatches",
	] {
		assert!(policy.contains(expected), "llmPolicy must mention `{expected}`");
	}

	// `--raw` is the same snapshot as one ok-first line
	let output = sandbox.run(&["context", "--project", &project, "--port", &port, "--raw"]);
	let text = cli_stdout(&output);
	let line = text.lines().find(|line| !line.trim().is_empty()).unwrap_or_default();

	assert!(line.starts_with(r#"{"ok":"#), "context --raw must lead with ok: {line}");
}

#[test]
fn context_reports_the_live_daemon_plugin_and_conflicts() {
	let daemon = start_daemon(None);

	spawn_cli_plugin(
		&daemon,
		"context-plugin",
		Arc::new(|op, _args| match op {
			"version" => CliAnswer::Value(serde_json::json!({
				"pluginVersion": "9.9.9-test",
				"protocol": 1,
				"studioVersion": "0.999.0",
			})),
			_ => CliAnswer::Failure("UNKNOWN_OP", "the fake plugin does not implement this op"),
		}),
	);

	let sandbox = CliSandbox::new();
	let project = daemon.root.to_string_lossy().into_owned();
	let port = daemon.port.to_string();

	let output = sandbox.run(&["context", "--project", &project, "--port", &port]);

	assert!(output.status.success(), "context failed: {}", cli_stderr(&output));

	let snapshot = cli_json(&output);

	assert_eq!(snapshot["daemon"]["reachable"], true);
	assert_eq!(snapshot["daemon"]["servesThisProject"], true);
	assert_eq!(snapshot["plugin"]["connected"], true);
	assert_eq!(snapshot["plugin"]["version"], "9.9.9-test");
	// The real daemon serves GET /resolve: no parked conflicts on a fresh
	// project is the number zero, not "unanswerable"
	assert_eq!(snapshot["conflicts"], 0);

	// `--full-commands` embeds the complete registry in the same snapshot
	let output = sandbox.run(&["context", "--project", &project, "--port", &port, "--full-commands"]);
	let snapshot = cli_json(&output);

	assert!(
		snapshot["commands"]["commands"].as_array().unwrap().len() >= 58,
		"--full-commands must embed the registry"
	);
}

// ---------------------------------------------------------------------------
// path / meta / where — daemon ops, no plugin
// ---------------------------------------------------------------------------

#[test]
fn path_meta_and_where_answer_through_the_daemon_without_a_plugin() {
	let daemon = start_daemon(None);

	// Let the initial scan settle before asking the tree
	thread::sleep(Duration::from_millis(300));

	let sandbox = CliSandbox::new();
	let project = daemon.root.to_string_lossy().into_owned();
	let port = daemon.port.to_string();
	let base = ["--project", project.as_str(), "--port", port.as_str()];

	let run = |extra: &[&str]| {
		let mut args: Vec<&str> = extra.to_vec();

		args.extend_from_slice(&base);

		sandbox.run(&args)
	};

	// path: studio → fs, --raw is the op value verbatim
	let output = run(&["path", "ReplicatedStorage/Hello", "--raw"]);

	assert!(output.status.success(), "path failed: {}", cli_stderr(&output));

	let value = cli_json(&output);

	assert_eq!(value["studioPath"], "ReplicatedStorage/Hello");
	assert_eq!(value["fsPaths"], serde_json::json!(["src/Hello.luau"]));
	assert_eq!(value["kind"], "file");

	// path: fs → studio
	let output = run(&["path", "src/Hello.luau", "--from", "fs", "--raw"]);

	assert_eq!(cli_json(&output)["studioPath"], "ReplicatedStorage/Hello");

	// path, human: both sides on their labelled lines
	let output = run(&["path", "ReplicatedStorage/Hello"]);
	let text = cli_stdout(&output);

	assert!(
		text.contains("Studio") && text.contains("ReplicatedStorage/Hello") && text.contains("src/Hello.luau"),
		"unexpected human rendering:\n{text}"
	);

	// meta
	let output = run(&["meta", "ReplicatedStorage/Hello", "--raw"]);
	let value = cli_json(&output);

	assert_eq!(value["instancePath"], "ReplicatedStorage/Hello");
	assert_eq!(value["class"], "ModuleScript");
	assert_eq!(value["middleware"], "ModuleScript");
	assert_eq!(value["sourcePaths"], serde_json::json!(["src/Hello.luau"]));

	let output = run(&["meta", "ReplicatedStorage/Hello"]);
	let text = cli_stdout(&output);

	assert!(
		text.contains("ModuleScript") && text.contains("src/Hello.luau"),
		"unexpected human rendering:\n{text}"
	);

	// where: substring match with fs resolution, scoped and unscoped
	let output = run(&["where", "hell", "--raw"]);
	let value = cli_json(&output);

	assert_eq!(value["matches"][0]["instancePath"], "ReplicatedStorage/Hello");
	assert_eq!(value["matches"][0]["fsPath"], "src/Hello.luau");
	assert_eq!(value["truncated"], false);

	let output = run(&["where", "Hello", "--under", "ReplicatedStorage", "--raw"]);

	assert_eq!(cli_json(&output)["matches"].as_array().unwrap().len(), 1);

	let output = run(&["where", "hell"]);

	assert!(cli_stdout(&output).contains("match(es)"));

	// NOT_FOUND surfaces the daemon's Studio-only message and a non-zero exit
	let output = run(&["path", "ReplicatedStorage/Nope"]);

	assert!(!output.status.success(), "a missing target exited zero");

	let message = cli_stderr(&output);

	assert!(
		message.contains("Studio-only") && message.contains("NOT_FOUND"),
		"the daemon's refusal must surface: {message}"
	);
}

// ---------------------------------------------------------------------------
// tail — the logs --tail alias as a real subcommand
// ---------------------------------------------------------------------------

#[test]
fn tail_streams_log_entries_and_pages_with_the_cursor() {
	let daemon = start_daemon(None);

	// First poll (no cursor) has one entry; later polls (cursor present)
	// have none
	let journal = spawn_cli_plugin(
		&daemon,
		"tail-plugin",
		Arc::new(|op, args| match op {
			"logs" => {
				let entries = if args.get("sinceSeq").is_some() {
					serde_json::json!([])
				} else {
					serde_json::json!([{ "seq": 1, "t": 5.0, "level": "info", "message": "hello-from-tail" }])
				};

				CliAnswer::Value(serde_json::json!({
					"entries": entries,
					"now": 5.0,
					"wall": 1_700_000_000.0,
					"buffer": { "newestSeq": 1 },
				}))
			}
			_ => CliAnswer::Failure("UNKNOWN_OP", "the fake plugin does not implement this op"),
		}),
	);

	let sandbox = CliSandbox::new();
	let project = daemon.root.to_string_lossy().into_owned();
	let port = daemon.port.to_string();

	let mut child = sandbox.spawn(&["tail", "--project", &project, "--port", &port, "--raw"]);
	let stdout = child.stdout.take().unwrap();
	let (line_tx, line_rx) = mpsc::channel();

	thread::spawn(move || {
		for line in BufReader::new(stdout).lines().map_while(Result::ok) {
			if line_tx.send(line).is_err() {
				break;
			}
		}
	});

	// The streamed NDJSON record arrives…
	let deadline = Instant::now() + Duration::from_secs(10);
	let record = loop {
		let remaining = deadline.saturating_duration_since(Instant::now());
		let line = line_rx
			.recv_timeout(remaining)
			.expect("tail never printed the log entry");

		if line.contains("hello-from-tail") {
			break serde_json::from_str::<Value>(&line).expect("tail --raw must print NDJSON");
		}
	};

	assert_eq!(record["message"], "hello-from-tail");
	assert_eq!(record["seq"], 1);

	// …and the poll loop keeps running, paging forward from the cursor so
	// the entry is never re-printed
	let deadline = Instant::now() + Duration::from_secs(10);

	loop {
		let polls = journal.lock().unwrap().clone();
		let cursored = polls
			.iter()
			.filter(|(op, args)| op == "logs" && args.get("sinceSeq").is_some())
			.count();

		if cursored >= 1 {
			assert_eq!(
				polls
					.iter()
					.find(|(op, args)| op == "logs" && args.get("sinceSeq").is_some())
					.map(|(_, args)| args["sinceSeq"].clone()),
				Some(serde_json::json!(1))
			);

			break;
		}

		assert!(Instant::now() < deadline, "tail never polled with the cursor");
		thread::sleep(Duration::from_millis(100));
	}

	// The first poll really was cursor-less
	assert!(journal_args(&journal, "logs").get("sinceSeq").is_none());
	assert!(journal_ops(&journal).iter().all(|op| op == "logs"));

	child.kill().ok();
	child.wait().ok();
}
