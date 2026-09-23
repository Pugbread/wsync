//! Integration coverage for the playtest surface (playtest.json): the real
//! binary against a real daemon with a fake plugin serving the run-owned job
//! ops, so the start → poll → record-driven exit contract, the `--raw`
//! NDJSON pass-through, the auto-stop/`--keep-open` split, and the
//! pre-network validation are exercised end to end.

mod common;

use serde_json::{json, Value};
use std::{
	fs,
	path::PathBuf,
	sync::{Arc, Mutex},
};

use common::{
	cli_stderr, cli_stdout, journal_args, journal_ops, spawn_cli_plugin, start_daemon, CliAnswer, CliJournal,
	CliSandbox, TestDaemon,
};

/// A scripted run: `playtest_run_start` answers with a job id, each
/// `playtest_run_poll` serves the next batch, and control ops succeed. Boxed
/// batches are `(records, done, exit)`
fn run_plugin(daemon: &TestDaemon, name: &'static str, batches: Vec<(Vec<Value>, bool, Option<i64>)>) -> CliJournal {
	let cursor = Mutex::new(0_usize);

	spawn_cli_plugin(
		daemon,
		name,
		Arc::new(move |op, _args| match op {
			"playtest_run_start" => CliAnswer::Value(json!({ "jobId": "job-1" })),
			"playtest_run_poll" => {
				let mut cursor = cursor.lock().unwrap();
				let index = (*cursor).min(batches.len().saturating_sub(1));
				let (records, done, exit) = &batches[index];

				*cursor += 1;

				let mut value = json!({
					"records": records,
					"nextSeq": (index + 1) as u64,
					"done": done,
				});

				if let Some(exit) = exit {
					value["exit"] = json!(exit);
				}

				CliAnswer::Value(value)
			}
			"playtest_run_cancel" | "playtest_stop" | "playtest_status" => CliAnswer::Value(json!({})),
			_ => CliAnswer::Failure("UNKNOWN_OP", "the fake plugin does not implement this op"),
		}),
	)
}

/// Writes a playscript into the sandbox and returns its path
fn script_file(sandbox: &CliSandbox, name: &str, source: &str) -> PathBuf {
	let path = sandbox.work.path().join(name);

	fs::write(&path, source).unwrap();

	path
}

fn run_args<'a>(daemon: &'a TestDaemon, script: &'a str, extra: &[&'a str]) -> Vec<String> {
	let mut args: Vec<String> = ["playtest", "run", "--script", script].map(str::to_owned).to_vec();

	args.extend(["--project".to_owned(), daemon.root.to_string_lossy().into_owned()]);
	args.extend(["--port".to_owned(), daemon.port.to_string()]);
	args.extend(extra.iter().map(|arg| (*arg).to_owned()));

	args
}

/// stdout parsed as NDJSON, one JSON object per physical line
fn ndjson_lines(output: &std::process::Output) -> Vec<Value> {
	cli_stdout(output)
		.lines()
		.filter(|line| !line.trim().is_empty())
		.map(|line| serde_json::from_str(line).unwrap_or_else(|err| panic!("stdout line is not JSON ({err}): {line}")))
		.collect()
}

// ---------------------------------------------------------------------------
// playtest run — the record-driven foreground session
// ---------------------------------------------------------------------------

#[test]
fn run_happy_path_streams_records_and_exits_zero() {
	let daemon = start_daemon(None);

	// Two identical `event` records prove the pass-through never deduplicates
	let records = vec![
		json!({ "type": "started", "jobId": "job-1", "mode": "play", "seq": 1 }),
		json!({ "type": "ready", "context": "server", "seq": 2 }),
		json!({ "type": "event", "data": { "lap": 1 }, "seq": 3 }),
		json!({ "type": "event", "data": { "lap": 1 }, "seq": 4 }),
		json!({ "type": "log", "level": "warn", "context": "server", "message": "low fuel", "seq": 5 }),
	];
	let terminal = json!({
		"type": "result", "ok": true, "kind": "success", "exitCode": 0,
		"value": { "laps": 3 }, "jobStatus": "completed", "elapsed": 1.25, "seq": 6,
	});

	let journal = run_plugin(
		&daemon,
		"run-happy",
		vec![(records.clone(), false, None), (vec![terminal.clone()], true, Some(0))],
	);

	let sandbox = CliSandbox::new();
	let script = script_file(&sandbox, "check.server.luau", "return playtest.args.laps");
	let args = run_args(
		&daemon,
		&script.to_string_lossy(),
		&["--args", r#"{"laps":3}"#, "--logs", "warn", "--raw"],
	);
	let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();

	let output = sandbox.run(&arg_refs);

	assert!(output.status.success(), "playtest run failed: {}", cli_stderr(&output));

	// `--raw` is a pure NDJSON pass-through: every record verbatim, in
	// arrival order, terminal last, nothing deduplicated
	let lines = ndjson_lines(&output);
	let mut expected = records.clone();

	expected.push(terminal);
	assert_eq!(
		lines, expected,
		"the NDJSON stream must match the records byte-for-byte"
	);

	// The job was started with the *source text* (never a path) and the
	// validated flags, and auto-stopped after completion
	let start = journal_args(&journal, "playtest_run_start");

	assert_eq!(start["script"], "return playtest.args.laps");
	assert_eq!(start["context"], "server");
	assert_eq!(start["mode"], "play");
	assert_eq!(start["players"], 1);
	assert_eq!(start["args"], json!({ "laps": 3 }));
	assert_eq!(start["timeoutSec"], 600);
	assert_eq!(start["identity"], "game");
	assert_eq!(start["logs"], "warn");
	assert!(start.get("clientScript").is_none());

	let ops = journal_ops(&journal);

	assert!(
		ops.contains(&"playtest_stop".to_owned()),
		"the run must auto-stop: {ops:?}"
	);
}

#[test]
fn run_failure_timeout_and_aborted_map_to_their_exit_codes() {
	let cases = [
		(
			json!({ "type": "result", "ok": false, "kind": "failure", "error": "boom", "jobStatus": "completed" }),
			2,
		),
		(
			json!({ "type": "result", "ok": false, "kind": "timeout", "error": "hard deadline", "jobStatus": "expired" }),
			3,
		),
		(
			json!({ "type": "aborted", "reason": "stopped from Studio", "jobStatus": "ended" }),
			4,
		),
	];

	for (terminal, expected_exit) in cases {
		let daemon = start_daemon(None);
		let _journal = run_plugin(&daemon, "run-exit", vec![(vec![terminal.clone()], true, None)]);

		let sandbox = CliSandbox::new();
		let script = script_file(&sandbox, "repro.server.luau", "return 1");
		let args = run_args(&daemon, &script.to_string_lossy(), &["--raw"]);
		let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();

		let output = sandbox.run(&arg_refs);

		assert_eq!(
			output.status.code(),
			Some(expected_exit),
			"terminal {terminal} must exit {expected_exit}; stderr: {}",
			cli_stderr(&output)
		);

		// The terminal record itself is the last NDJSON line
		let lines = ndjson_lines(&output);

		assert_eq!(lines.last(), Some(&terminal));
	}
}

const CLIENT_TRACEBACK: &str = "Workspace.Door:12: attempt to index nil with 'Open'\nRunner:3 function __wsync_run";

/// A client's companion script failing while the main script keeps waiting —
/// a client error never ends the session, so the server times out
fn failed_client_record(seq: u64) -> Value {
	json!({
		"type": "clientResult", "context": "client:1", "ok": false,
		"error": "Workspace.Door:12: attempt to index nil with 'Open'",
		"traceback": CLIENT_TRACEBACK, "seq": seq, "t": 3.2,
	})
}

/// The plugin's timeout terminal as a plugin without `clientFailures` wrote it
fn plugin_timeout_record(seq: u64) -> Value {
	json!({
		"type": "result", "ok": false, "outcome": "timeout",
		"error": "playtest run timed out after 60s", "elapsed": 61.0, "seq": seq,
	})
}

#[test]
fn run_timeout_repeats_client_failures_in_the_final_raw_line() {
	let daemon = start_daemon(None);

	let progress = vec![
		json!({ "type": "started", "jobId": "job-1", "mode": "play", "seq": 1 }),
		failed_client_record(2),
		json!({ "type": "log", "context": "server", "level": "error", "message": "server noise", "seq": 3 }),
	];
	let _journal = run_plugin(
		&daemon,
		"run-timeout-clients",
		vec![
			(progress.clone(), false, None),
			(vec![plugin_timeout_record(4)], true, Some(3)),
		],
	);

	let sandbox = CliSandbox::new();
	let script = script_file(&sandbox, "wait.server.luau", "playtest.await('door-open')");
	let args = run_args(&daemon, &script.to_string_lossy(), &["--raw"]);
	let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();

	let output = sandbox.run(&arg_refs);

	assert_eq!(
		output.status.code(),
		Some(3),
		"a timeout stays exit 3: {}",
		cli_stderr(&output)
	);

	// Progress still passes through verbatim; the terminal is the plugin's
	// own record plus the client failures it lacked, so a caller reading only
	// the last line sees the client traceback
	let lines = ndjson_lines(&output);

	assert_eq!(lines[..3], progress[..]);

	let mut expected = plugin_timeout_record(4);

	expected["clientFailures"] = json!([{
		"context": "client:1", "source": "clientResult",
		"error": "Workspace.Door:12: attempt to index nil with 'Open'",
		"traceback": CLIENT_TRACEBACK, "seq": 2, "t": 3.2,
	}]);
	assert_eq!(lines.last(), Some(&expected));
}

#[test]
fn run_keeps_a_plugin_authored_client_failure_list_verbatim() {
	let daemon = start_daemon(None);

	let mut terminal = plugin_timeout_record(3);

	terminal["exitCode"] = json!(3);
	terminal["jobStatus"] = json!("stopped");
	terminal["clientFailures"] = json!([
		{ "context": "client:1", "source": "clientResult", "error": "boom", "traceback": "boom\nRunner:1", "seq": 2, "t": 3.2 },
		// Evicted telemetry the CLI never saw — only the plugin knows it
		{ "context": "client:2", "source": "log", "error": "LocalScript:4: nope", "count": 9, "t": 4.0 },
	]);

	let _journal = run_plugin(
		&daemon,
		"run-plugin-failures",
		vec![(vec![failed_client_record(2), terminal.clone()], true, Some(3))],
	);

	let sandbox = CliSandbox::new();
	let script = script_file(&sandbox, "wait.server.luau", "playtest.await('door-open')");
	let args = run_args(&daemon, &script.to_string_lossy(), &["--raw"]);
	let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();

	let output = sandbox.run(&arg_refs);

	assert_eq!(output.status.code(), Some(3));
	assert_eq!(ndjson_lines(&output).last(), Some(&terminal));
}

#[test]
fn run_human_timeout_summarizes_client_failures_without_repeating_tracebacks() {
	let daemon = start_daemon(None);
	let batches = vec![(vec![failed_client_record(1), plugin_timeout_record(2)], true, Some(3))];

	// Streaming: the traceback prints once, as progress, and the summary
	// points back at it
	let _journal = run_plugin(&daemon, "run-human-clients", batches.clone());
	let sandbox = CliSandbox::new();
	let script = script_file(&sandbox, "wait.server.luau", "playtest.await('door-open')");
	let args = run_args(&daemon, &script.to_string_lossy(), &[]);
	let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();

	let output = sandbox.run(&arg_refs);
	let stdout = cli_stdout(&output);

	assert_eq!(output.status.code(), Some(3));
	assert!(
		stdout.contains("✖ client client:1 failed: Workspace.Door:12"),
		"{stdout}"
	);
	assert!(
		stdout.contains("✖ timeout (61.0s): playtest run timed out after 60s"),
		"{stdout}"
	);
	assert!(
		stdout.contains("1 client failure(s) reported during the run:"),
		"{stdout}"
	);
	assert!(stdout.contains("  ✖ client:1 at 3.2s: Workspace.Door:12"), "{stdout}");
	assert!(stdout.contains("(traceback printed above)"), "{stdout}");
	assert_eq!(
		stdout.matches("Runner:3 function __wsync_run").count(),
		1,
		"the traceback must not print twice: {stdout}"
	);

	// --quiet streamed nothing, so the summary carries the traceback itself
	let daemon = start_daemon(None);
	let _journal = run_plugin(&daemon, "run-human-quiet", batches);
	let args = run_args(&daemon, &script.to_string_lossy(), &["--quiet"]);
	let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();

	let output = sandbox.run(&arg_refs);
	let stdout = cli_stdout(&output);

	assert_eq!(output.status.code(), Some(3));
	assert!(
		!stdout.contains("✖ client client:1 failed"),
		"--quiet suppresses progress: {stdout}"
	);
	assert!(stdout.contains("    Runner:3 function __wsync_run"), "{stdout}");
	assert!(!stdout.contains("(traceback printed above)"), "{stdout}");
}

#[test]
fn run_cli_deadline_timeout_carries_client_failures() {
	// The plugin's own terminal never arrives (Studio wedged mid-teardown):
	// the CLI's deadline — `--timeout` plus its 10 s grace — authors the
	// timeout, and it must still carry the client failure that streamed first
	let daemon = start_daemon(None);
	let progress = vec![
		json!({ "type": "started", "jobId": "job-1", "mode": "play", "seq": 1 }),
		failed_client_record(2),
	];
	let polls = Mutex::new(0_usize);
	let first_page = progress.clone();

	let journal = spawn_cli_plugin(
		&daemon,
		"run-cli-deadline",
		Arc::new(move |op, _args| match op {
			"playtest_run_start" => CliAnswer::Value(json!({ "jobId": "job-1" })),
			"playtest_run_poll" => {
				let mut polls = polls.lock().unwrap();

				*polls += 1;

				let records = if *polls == 1 { first_page.clone() } else { Vec::new() };

				CliAnswer::Value(json!({ "records": records, "nextSeq": 2, "done": false }))
			}
			"playtest_status" => CliAnswer::Value(json!({ "jobStatus": "stopping" })),
			"playtest_run_cancel" | "playtest_stop" => CliAnswer::Value(json!({})),
			_ => CliAnswer::Failure("UNKNOWN_OP", "the fake plugin does not implement this op"),
		}),
	);

	let sandbox = CliSandbox::new();
	let script = script_file(&sandbox, "wait.server.luau", "playtest.await('door-open')");
	let args = run_args(&daemon, &script.to_string_lossy(), &["--timeout", "1", "--raw"]);
	let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();

	let output = sandbox.run(&arg_refs);

	assert_eq!(
		output.status.code(),
		Some(3),
		"the CLI deadline exits 3: {}",
		cli_stderr(&output)
	);
	assert!(journal_ops(&journal).contains(&"playtest_run_cancel".to_owned()));

	let lines = ndjson_lines(&output);

	assert_eq!(lines[..2], progress[..]);

	let terminal = lines.last().unwrap();

	assert_eq!(terminal["type"], "result");
	assert_eq!(terminal["kind"], "timeout");
	assert_eq!(terminal["outcome"], "timeout");
	assert_eq!(terminal["exitCode"], 3);
	assert_eq!(terminal["jobStatus"], "stopping");
	assert_eq!(
		terminal["clientFailures"],
		json!([{
			"context": "client:1", "source": "clientResult",
			"error": "Workspace.Door:12: attempt to index nil with 'Open'",
			"traceback": CLIENT_TRACEBACK, "seq": 2, "t": 3.2,
		}])
	);
}

#[test]
fn run_boot_failure_exits_five() {
	let daemon = start_daemon(None);

	let _journal = spawn_cli_plugin(
		&daemon,
		"run-boot",
		Arc::new(|op, _args| match op {
			"playtest_run_start" => CliAnswer::Failure("PLAYTEST_BOOT", "Studio refused to enter play mode"),
			_ => CliAnswer::Value(json!({})),
		}),
	);

	let sandbox = CliSandbox::new();
	let script = script_file(&sandbox, "boot.server.luau", "return 1");
	let args = run_args(&daemon, &script.to_string_lossy(), &["--raw"]);
	let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();

	let output = sandbox.run(&arg_refs);

	assert_eq!(
		output.status.code(),
		Some(5),
		"a refused start is the boot-failure exit"
	);

	let lines = ndjson_lines(&output);
	let terminal = lines.last().expect("a terminal record is always printed");

	assert_eq!(terminal["type"], "result");
	assert_eq!(terminal["kind"], "bootFailure");
	assert_eq!(terminal["exitCode"], 5);
	assert!(
		terminal["error"].as_str().unwrap().contains("Studio refused"),
		"the boot failure names the plugin's reason: {terminal}"
	);
}

#[test]
fn run_keep_open_skips_the_stop_and_prints_the_job_id() {
	let daemon = start_daemon(None);

	let terminal = json!({
		"type": "result", "ok": true, "kind": "success", "exitCode": 0,
		"value": 42, "jobStatus": "running", "elapsed": 0.5,
	});
	let journal = run_plugin(&daemon, "run-keep", vec![(vec![terminal], true, Some(0))]);

	let sandbox = CliSandbox::new();
	let script = script_file(&sandbox, "keep.server.luau", "return 42");
	let args = run_args(&daemon, &script.to_string_lossy(), &["--keep-open"]);
	let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();

	let output = sandbox.run(&arg_refs);

	assert!(output.status.success(), "keep-open run failed: {}", cli_stderr(&output));

	let ops = journal_ops(&journal);

	assert!(
		!ops.contains(&"playtest_stop".to_owned()),
		"--keep-open must skip the auto-stop: {ops:?}"
	);
	assert!(
		cli_stdout(&output).contains("job-1"),
		"--keep-open prints the job id for later use: {}",
		cli_stdout(&output)
	);
}

#[test]
fn run_quiet_suppresses_progress_but_never_the_terminal() {
	let daemon = start_daemon(None);

	let progress = json!({ "type": "event", "data": 1, "seq": 1 });
	let terminal = json!({ "type": "result", "ok": true, "kind": "success", "exitCode": 0, "value": 7 });
	let _journal = run_plugin(
		&daemon,
		"run-quiet",
		vec![(vec![progress, terminal.clone()], true, Some(0))],
	);

	let sandbox = CliSandbox::new();
	let script = script_file(&sandbox, "quiet.server.luau", "return 7");
	let args = run_args(&daemon, &script.to_string_lossy(), &["--quiet", "--raw"]);
	let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();

	let output = sandbox.run(&arg_refs);

	assert!(output.status.success());
	assert_eq!(
		ndjson_lines(&output),
		vec![terminal],
		"quiet leaves exactly the terminal"
	);
}

#[test]
fn run_validates_everything_before_any_network_work() {
	let sandbox = CliSandbox::new();
	let script = script_file(&sandbox, "ok.server.luau", "return 1");
	let script_flag = script.to_string_lossy().into_owned();

	// (extra args, expected stderr fragment) — none of these may touch a
	// daemon, so no daemon exists to touch
	let cases: Vec<(Vec<&str>, &str)> = vec![
		(vec!["--args", "{not json"], "--args"),
		(vec!["--timeout", "4000"], "--timeout"),
		(vec!["--players", "9"], "--players"),
		(vec!["--players", "0"], "--players"),
		(vec!["--mode", "solo"], "--mode"),
		(vec!["--context", "client:0"], "--context"),
		(vec!["--logs", "loud"], "--logs"),
		(vec!["--identity", "root"], "--identity"),
	];

	for (extra, fragment) in cases {
		let mut args = vec!["playtest", "run", "--script", &script_flag];

		args.extend(extra.iter());

		let output = sandbox.run(&args);

		assert!(!output.status.success(), "{extra:?} must be refused");
		assert!(
			cli_stderr(&output).contains(fragment),
			"{extra:?} should complain about {fragment}: {}",
			cli_stderr(&output)
		);
	}

	// A missing script file is a pre-network failure too
	let output = sandbox.run(&["playtest", "run", "--script", "/nonexistent/nope.server.luau"]);

	assert!(!output.status.success());
	assert!(cli_stderr(&output).contains("--script"));
}

// ---------------------------------------------------------------------------
// The low-level controls
// ---------------------------------------------------------------------------

/// A permissive fake plugin that echoes every playtest op's args back as its
/// value, so arg-mapping assertions read naturally off the journal
fn echo_plugin(daemon: &TestDaemon, name: &'static str) -> CliJournal {
	spawn_cli_plugin(
		daemon,
		name,
		Arc::new(|op, args| match op {
			"playtest_start" => CliAnswer::Value(json!({ "jobId": "echo-job", "status": "starting" })),
			op if op.starts_with("playtest_") => CliAnswer::Value(json!({ "echo": op, "args": args })),
			_ => CliAnswer::Failure("UNKNOWN_OP", "the fake plugin does not implement this op"),
		}),
	)
}

#[test]
fn low_level_commands_map_their_flags_onto_the_op_contracts() {
	let daemon = start_daemon(None);
	let journal = echo_plugin(&daemon, "low-level");
	let sandbox = CliSandbox::new();

	let project = daemon.root.to_string_lossy().into_owned();
	let port = daemon.port.to_string();
	let source_file = script_file(&sandbox, "probe.luau", "return #game.Players:GetPlayers()");
	let source_flag = source_file.to_string_lossy().into_owned();
	let actions_file = script_file(
		&sandbox,
		"input.json",
		r#"[{ "kind": "tap", "x": 100, "y": 200 }, { "kind": "wait", "ms": 50 }]"#,
	);
	let actions_flag = actions_file.to_string_lossy().into_owned();

	let commands: Vec<Vec<&str>> = vec![
		vec!["playtest", "start", "--mode", "multiplayer", "--players", "2"],
		vec!["playtest", "status"],
		vec!["playtest", "contexts"],
		vec![
			"playtest",
			"wait",
			"--context",
			"client:2",
			"--minimum",
			"3",
			"--timeout",
			"5",
		],
		vec![
			"playtest",
			"exec",
			"--context",
			"server",
			"--source-file",
			&source_flag,
			"--identity",
			"plugin",
			"--timeout",
			"20",
		],
		vec![
			"playtest",
			"logs",
			"--context",
			"client:1",
			"--since-seq",
			"7",
			"--limit",
			"50",
		],
		vec![
			"playtest",
			"ui",
			"--context",
			"client:1",
			"--class",
			"TextButton",
			"--limit",
			"200",
		],
		vec![
			"playtest",
			"input",
			"--context",
			"client:1",
			"--file",
			&actions_flag,
			"--timeout",
			"5",
		],
		vec!["playtest", "stop"],
		vec![
			"playtest",
			"request",
			"--context",
			"server",
			"--op",
			"physics_stats",
			"--args",
			r#"{"detail":true}"#,
		],
	];

	for command in &commands {
		let mut args: Vec<&str> = command.clone();

		args.extend(["--project", &project, "--port", &port, "--raw"]);

		let output = sandbox.run(&args);

		assert!(output.status.success(), "{command:?} failed: {}", cli_stderr(&output));
	}

	// The journalled args carry each command's validated mapping
	assert_eq!(
		journal_args(&journal, "playtest_start"),
		json!({ "mode": "multiplayer", "players": 2 })
	);
	assert_eq!(
		journal_args(&journal, "playtest_wait"),
		json!({ "context": "client:2", "minimum": 3, "timeoutMs": 5000 })
	);

	let exec = journal_args(&journal, "playtest_exec");

	assert_eq!(exec["context"], "server");
	assert_eq!(exec["source"], "return #game.Players:GetPlayers()");
	assert_eq!(exec["identity"], "plugin");
	assert_eq!(exec["timeoutMs"], 20_000);

	assert_eq!(
		journal_args(&journal, "playtest_logs"),
		json!({ "context": "client:1", "sinceSeq": 7, "limit": 50 })
	);

	let ui = journal_args(&journal, "playtest_ui");

	assert_eq!(ui["context"], "client:1");
	assert_eq!(ui["class"], "TextButton");
	assert_eq!(ui["limit"], 200);

	let input = journal_args(&journal, "playtest_input");

	assert_eq!(input["context"], "client:1");
	assert_eq!(input["actions"].as_array().unwrap().len(), 2);
	assert_eq!(input["timeoutMs"], 5000);

	let request = journal_args(&journal, "playtest_request");

	assert_eq!(request["op"], "physics_stats");
	assert_eq!(request["args"], json!({ "detail": true }));
	assert_eq!(request["timeoutMs"], 30_000);
}

#[test]
fn input_screens_the_action_array_before_the_network() {
	let sandbox = CliSandbox::new();

	// 201 actions — one past the contract's cap
	let oversized: Vec<Value> = (0..201).map(|index| json!({ "kind": "wait", "ms": index })).collect();
	let oversized = serde_json::to_string(&oversized).unwrap();

	let cases: Vec<(&str, &str)> = vec![
		(&oversized, "200"),
		(r#"{"kind":"tap"}"#, "array"),
		(r#"[]"#, "empty"),
		(r#"[1, 2]"#, "object"),
	];

	for (actions, fragment) in cases {
		let output = sandbox.run(&["playtest", "input", "--context", "client:1", "--actions", actions]);

		assert!(!output.status.success(), "{actions:.40} must be refused");
		assert!(
			cli_stderr(&output).contains(fragment),
			"the refusal should mention {fragment}: {}",
			cli_stderr(&output)
		);
	}

	// The input timeout is capped at the contract's 30 s ceiling by clap
	let output = sandbox.run(&[
		"playtest",
		"input",
		"--context",
		"client:1",
		"--actions",
		r#"[{"kind":"tap"}]"#,
		"--timeout",
		"31",
	]);

	assert!(!output.status.success());
	assert!(cli_stderr(&output).contains("--timeout"));
}

#[test]
fn playtest_capture_reads_the_engine_screenshot_file() {
	// A running playtest can't read a screenshot back into pixels, so
	// `playtest capture` triggers a CaptureScreenshot and reads the PNG the
	// engine drops in Roblox's tmp-capture-storage. Here the fake plugin plays
	// the engine: on `playtest_screenshot` it writes a wob-* PNG into the
	// directory we point the CLI at with WSYNC_TMP_CAPTURE_DIR.
	const WIDTH: u32 = 8;
	const HEIGHT: u32 = 6;

	let png_bytes = {
		let mut buf = Vec::new();
		let mut encoder = png::Encoder::new(&mut buf, WIDTH, HEIGHT);

		encoder.set_color(png::ColorType::Rgba);
		encoder.set_depth(png::BitDepth::Eight);

		let mut writer = encoder.write_header().unwrap();

		writer
			.write_image_data(&vec![0x7fu8; (WIDTH * HEIGHT * 4) as usize])
			.unwrap();
		writer.finish().unwrap();

		buf
	};

	let wob = tempfile::tempdir().expect("temp wob dir");
	let wob_path = wob.path().to_path_buf();

	let daemon = start_daemon(None);
	let plugin_png = png_bytes.clone();
	let plugin_dir = wob_path.clone();
	let journal = spawn_cli_plugin(
		&daemon,
		"playtest-screenshot",
		Arc::new(move |op, _args| match op {
			"playtest_screenshot" => {
				// The engine's side effect: a fresh frame lands on disk
				fs::write(plugin_dir.join("wob-1"), &plugin_png).unwrap();

				CliAnswer::Value(json!({ "ok": true, "contentId": "rbxtemp://1" }))
			}
			_ => CliAnswer::Failure("UNKNOWN_OP", "the fake plugin does not implement this op"),
		}),
	);

	let sandbox = CliSandbox::new();
	let output_path = sandbox.work.path().join("captures").join("client-1.png");
	let project = daemon.root.to_string_lossy().into_owned();
	let port = daemon.port.to_string();

	let output = sandbox.run_with_envs(
		&[
			"playtest",
			"capture",
			"--context",
			"client:1",
			"-o",
			&output_path.to_string_lossy(),
			"--project",
			&project,
			"--port",
			&port,
			"--raw",
		],
		&[("WSYNC_TMP_CAPTURE_DIR", wob_path.to_str().unwrap())],
	);

	assert!(
		output.status.success(),
		"playtest capture failed: {}",
		cli_stderr(&output)
	);

	// It triggered a screenshot — not the old render/pull pipeline
	let ops = journal_ops(&journal);

	assert!(ops.contains(&"playtest_screenshot".to_owned()), "ops: {ops:?}");
	assert!(!ops.iter().any(|op| op == "capture_read" || op == "playtest_capture"));

	// The output is the PNG the engine wrote, verified and copied out
	let encoded = fs::read(&output_path).expect("the capture PNG exists");
	let reader = png::Decoder::new(encoded.as_slice())
		.read_info()
		.expect("the written PNG decodes");

	assert_eq!((reader.info().width, reader.info().height), (WIDTH, HEIGHT));

	// The engine's temp file is removed once we've consumed it
	assert!(
		!wob_path.join("wob-1").exists(),
		"the wob temp file should be cleaned up after read"
	);

	// A non-PlayClient context is refused before any network work
	let output = sandbox.run(&["playtest", "capture", "--context", "server"]);

	assert!(!output.status.success());
	assert!(cli_stderr(&output).contains("PlayClient"));
}

/// The workflow `playtest` step shares the poll loop: when its own deadline
/// passes without a terminal record, the step's error response still carries
/// the client failures (with tracebacks) that streamed first
#[test]
fn workflow_playtest_timeout_carries_client_failures() {
	let daemon = start_daemon(None);
	let polls = Mutex::new(0_usize);

	spawn_cli_plugin(
		&daemon,
		"workflow-deadline",
		Arc::new(move |op, _args| match op {
			"playtest_run_start" => CliAnswer::Value(json!({ "jobId": "job-1" })),
			"playtest_run_poll" => {
				let mut polls = polls.lock().unwrap();

				*polls += 1;

				let records = if *polls == 1 {
					vec![
						json!({ "type": "started", "jobId": "job-1", "mode": "play", "seq": 1 }),
						failed_client_record(2),
					]
				} else {
					Vec::new()
				};

				CliAnswer::Value(json!({ "records": records, "nextSeq": 2, "done": false }))
			}
			"playtest_run_cancel" | "playtest_stop" | "playtest_status" => CliAnswer::Value(json!({})),
			_ => CliAnswer::Failure("UNKNOWN_OP", "the fake plugin does not implement this op"),
		}),
	);

	let sandbox = CliSandbox::new();
	let workflow = sandbox.work.path().join("playtest.json");

	fs::write(
		&workflow,
		json!({
			"version": 1,
			"steps": [{ "id": "run", "op": "playtest", "script": "playtest.await('door-open')", "timeoutSec": 1 }],
		})
		.to_string(),
	)
	.unwrap();

	let output = sandbox.run(&[
		"run",
		"--file",
		&workflow.to_string_lossy(),
		"--project",
		&daemon.root.to_string_lossy(),
		"--port",
		&daemon.port.to_string(),
		"--raw",
	]);

	assert!(!output.status.success(), "a timed-out step must fail the workflow");

	let outcome: Value = serde_json::from_str(cli_stdout(&output).trim())
		.unwrap_or_else(|err| panic!("stdout is not JSON ({err}): {}", cli_stdout(&output)));
	let text = outcome.to_string();

	assert!(text.contains("PLAYTEST_TIMEOUT"), "unexpected outcome: {outcome}");
	assert!(
		text.contains("\"clientFailures\"") && text.contains("Runner:3 function __wsync_run"),
		"the step's error response must carry the client failure and its traceback: {outcome}"
	);
}
