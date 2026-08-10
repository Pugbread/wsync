//! Integration coverage for `wsync copy` / `wsync paste` (copy.json,
//! paste.json): the real binary against real daemons with fake plugins
//! serving the clipboard op family, so the chunk pump, the atomic state-dir
//! clipboard replace, the cross-project reuse, and the guardrails are
//! exercised end to end.
//!
//! The sandbox pins `WSYNC_STATE_DIR`, so every run through one sandbox
//! shares one private clipboard — exactly the cross-project surface the
//! commands promise.

mod common;

use base64::Engine;
use serde_json::{json, Value};
use std::{fs, sync::Arc};

use common::{
	chunk_answer, cli_json, cli_stderr, cli_stdout, journal_args, journal_ops, spawn_cli_plugin, start_daemon,
	CliAnswer, CliJournal, CliSandbox, TestDaemon,
};

/// Big enough for two 64 KiB chunks
fn payload(seed: u8) -> Vec<u8> {
	(0..100_000)
		.map(|index| ((index * 17 + usize::from(seed)) % 253) as u8)
		.collect()
}

fn sha256_hex(bytes: &[u8]) -> String {
	use sha2::{Digest, Sha256};

	format!("{:x}", Sha256::digest(bytes))
}

/// A fake plugin serving `clipboard_copy` + `clipboard_read` for one payload.
/// `advertised_sha` lets a test lie about the digest
fn copy_plugin(
	daemon: &TestDaemon,
	name: &'static str,
	bytes: Vec<u8>,
	advertised_sha: String,
	roots: Value,
) -> CliJournal {
	spawn_cli_plugin(
		daemon,
		name,
		Arc::new(move |op, args| match op {
			"clipboard_copy" => CliAnswer::Value(json!({
				"clipId": "clip-1",
				"bytes": bytes.len(),
				"sha256": advertised_sha,
				"roots": roots,
			})),
			"clipboard_read" => CliAnswer::Value(chunk_answer(&bytes, args)),
			_ => CliAnswer::Failure("UNKNOWN_OP", "the fake plugin does not implement this op"),
		}),
	)
}

/// A fake plugin serving the paste family; the journal carries every chunk
fn paste_plugin(daemon: &TestDaemon, name: &'static str, pasted_roots: Value) -> CliJournal {
	spawn_cli_plugin(
		daemon,
		name,
		Arc::new(move |op, _args| match op {
			"clipboard_paste_begin" => CliAnswer::Value(json!({ "clipId": "paste-1" })),
			"clipboard_paste_chunk" => CliAnswer::Value(json!({})),
			"clipboard_paste_commit" => CliAnswer::Value(json!({ "roots": pasted_roots })),
			_ => CliAnswer::Failure("UNKNOWN_OP", "the fake plugin does not implement this op"),
		}),
	)
}

fn run_copy(sandbox: &CliSandbox, daemon: &TestDaemon, extra: &[&str]) -> std::process::Output {
	let project = daemon.root.to_string_lossy().into_owned();
	let port = daemon.port.to_string();

	let mut args = vec!["copy"];

	args.extend_from_slice(extra);
	args.extend(["--project", &project, "--port", &port, "--raw"]);

	sandbox.run(&args)
}

#[test]
fn copy_then_paste_round_trips_across_projects() {
	// Project A: the source of the copy
	let daemon_a = start_daemon(None);
	let bytes = payload(1);
	let sha = sha256_hex(&bytes);
	let roots = json!([{ "path": "Workspace/Boss", "class": "Model", "name": "Boss" }]);
	let copy_journal = copy_plugin(&daemon_a, "copy-plugin", bytes.clone(), sha.clone(), roots);

	let sandbox = CliSandbox::new();
	let output = run_copy(
		&sandbox,
		&daemon_a,
		&["Workspace/Boss", "--path", "ReplicatedStorage/BossConfig"],
	);

	assert!(output.status.success(), "copy failed: {}", cli_stderr(&output));

	// Positional paths and `--path` flags reach the op merged, in order
	assert_eq!(
		journal_args(&copy_journal, "clipboard_copy")["paths"],
		json!(["Workspace/Boss", "ReplicatedStorage/BossConfig"])
	);

	// The pump really paged
	assert!(
		journal_ops(&copy_journal)
			.iter()
			.filter(|op| *op == "clipboard_read")
			.count() >= 2,
		"a 100 KB payload must take more than one 64 KiB chunk"
	);

	// The state-dir clipboard: payload byte-for-byte, sidecar describing it
	let clipboard = sandbox.state.path().join("clipboard.rbxm");
	let sidecar_path = sandbox.state.path().join("clipboard.json");

	assert_eq!(fs::read(&clipboard).unwrap(), bytes);

	let sidecar: Value = serde_json::from_str(&fs::read_to_string(&sidecar_path).unwrap()).unwrap();
	let source_project = daemon_a.root.join("default.project.json");

	assert_eq!(sidecar["sha256"], sha);
	assert_eq!(sidecar["bytes"], bytes.len() as u64);
	assert_eq!(sidecar["roots"][0]["path"], "Workspace/Boss");
	assert_eq!(sidecar["sourceProject"], source_project.to_string_lossy().into_owned());
	assert!(
		sidecar["copiedAt"].as_str().unwrap_or_default().contains('T'),
		"copiedAt must be a timestamp: {sidecar}"
	);

	// Private files (unix)
	#[cfg(unix)]
	{
		use std::os::unix::fs::PermissionsExt;

		for file in [&clipboard, &sidecar_path] {
			let mode = fs::metadata(file).unwrap().permissions().mode() & 0o777;

			assert_eq!(mode, 0o600, "{} must be 0600, is {mode:o}", file.display());
		}
	}

	// `--raw` reports the stored clipboard
	let raw = cli_json(&output);

	assert_eq!(raw["ok"], true);
	assert_eq!(raw["path"], clipboard.to_string_lossy().into_owned());
	assert_eq!(raw["bytes"], bytes.len() as u64);
	assert_eq!(raw["sha256"], sha);

	// Project B: a different daemon, a different scratch project, the same
	// sandbox — the clipboard crosses projects
	let daemon_b = start_daemon(None);
	let pasted_roots = json!([{ "path": "Workspace/Imported/Boss", "class": "Model", "name": "Boss" }]);
	let paste_journal = paste_plugin(&daemon_b, "paste-plugin", pasted_roots);

	let project_b = daemon_b.root.to_string_lossy().into_owned();
	let port_b = daemon_b.port.to_string();

	let output = sandbox.run(&[
		"paste",
		"--to",
		"Workspace/Imported",
		"--no-select",
		"--project",
		&project_b,
		"--port",
		&port_b,
		"--raw",
	]);

	assert!(output.status.success(), "paste failed: {}", cli_stderr(&output));

	// begin declared exactly the stored payload
	let begin = journal_args(&paste_journal, "clipboard_paste_begin");

	assert_eq!(begin["bytes"], bytes.len() as u64);
	assert_eq!(begin["sha256"], sha);

	// The chunks reassemble to the exact payload, in offset order
	let entries = paste_journal.lock().unwrap().clone();
	let mut reassembled = Vec::new();

	for (op, args) in &entries {
		if op != "clipboard_paste_chunk" {
			continue;
		}

		assert_eq!(args["clipId"], "paste-1");
		assert_eq!(
			args["offset"].as_u64().unwrap(),
			reassembled.len() as u64,
			"chunks must arrive in order"
		);

		reassembled.extend(
			base64::engine::general_purpose::STANDARD
				.decode(args["data"].as_str().unwrap())
				.unwrap(),
		);
	}

	assert_eq!(reassembled, bytes, "the pasted bytes must match the stored clipboard");

	// commit carried the destination and the selection choice
	let commit = journal_args(&paste_journal, "clipboard_paste_commit");

	assert_eq!(commit["to"], "Workspace/Imported");
	assert_eq!(commit["noSelect"], true);

	// The write-family raw shape: ok first, then the op's value
	let line = cli_stdout(&output);
	let line = line.lines().find(|line| !line.trim().is_empty()).unwrap_or_default();

	assert!(line.starts_with(r#"{"ok":"#), "paste --raw must lead with ok: {line}");

	let raw = cli_json(&output);

	assert_eq!(raw["roots"][0]["path"], "Workspace/Imported/Boss");

	// Paste does not consume the clipboard
	assert_eq!(fs::read(&clipboard).unwrap(), bytes);
}

#[test]
fn a_second_copy_atomically_replaces_the_clipboard() {
	let sandbox = CliSandbox::new();

	// First copy
	let daemon_a = start_daemon(None);
	let first = payload(1);

	copy_plugin(
		&daemon_a,
		"first-copy-plugin",
		first.clone(),
		sha256_hex(&first),
		json!([{ "path": "Workspace/A", "class": "Folder", "name": "A" }]),
	);

	let output = run_copy(&sandbox, &daemon_a, &["Workspace/A"]);

	assert!(output.status.success(), "first copy failed: {}", cli_stderr(&output));

	// Second copy, different payload, no paths — the selection form
	let daemon_b = start_daemon(None);
	let second = payload(2);
	let journal_b = copy_plugin(
		&daemon_b,
		"second-copy-plugin",
		second.clone(),
		sha256_hex(&second),
		json!([{ "path": "Workspace/B", "class": "Folder", "name": "B" }]),
	);

	let output = run_copy(&sandbox, &daemon_b, &[]);

	assert!(output.status.success(), "second copy failed: {}", cli_stderr(&output));

	// A pathless copy asks for the current selection: no `paths` argument
	assert!(
		journal_args(&journal_b, "clipboard_copy").get("paths").is_none(),
		"a selection copy must not send a paths argument"
	);

	// Fully replaced, sidecar in step, nothing half-written left around
	let clipboard = sandbox.state.path().join("clipboard.rbxm");
	let sidecar: Value =
		serde_json::from_str(&fs::read_to_string(sandbox.state.path().join("clipboard.json")).unwrap()).unwrap();

	assert_eq!(fs::read(&clipboard).unwrap(), second);
	assert_eq!(sidecar["sha256"], sha256_hex(&second));
	assert_eq!(sidecar["roots"][0]["path"], "Workspace/B");

	let leftovers: Vec<_> = fs::read_dir(sandbox.state.path())
		.unwrap()
		.filter_map(Result::ok)
		.filter(|entry| entry.file_name().to_string_lossy().contains(".tmp-"))
		.collect();

	assert!(leftovers.is_empty(), "temp files left behind: {leftovers:?}");
}

#[test]
fn a_corrupted_copy_aborts_and_preserves_the_previous_clipboard() {
	let sandbox = CliSandbox::new();

	// Seed a good clipboard
	let daemon_a = start_daemon(None);
	let good = payload(1);

	copy_plugin(
		&daemon_a,
		"good-copy-plugin",
		good.clone(),
		sha256_hex(&good),
		json!([{ "path": "Workspace/Good", "class": "Folder", "name": "Good" }]),
	);

	assert!(run_copy(&sandbox, &daemon_a, &["Workspace/Good"]).status.success());

	// A second copy whose advertised digest does not match the served bytes
	let daemon_b = start_daemon(None);
	let corrupt = payload(2);

	copy_plugin(
		&daemon_b,
		"corrupt-copy-plugin",
		corrupt,
		sha256_hex(b"not those bytes"),
		json!([{ "path": "Workspace/Bad", "class": "Folder", "name": "Bad" }]),
	);

	let output = run_copy(&sandbox, &daemon_b, &["Workspace/Bad"]);

	assert!(!output.status.success(), "a corrupted copy was accepted");

	let message = cli_stderr(&output);

	assert!(
		message.contains("SHA-256") || message.contains("corrupted"),
		"the digest mismatch must be named: {message}"
	);

	// The previous clipboard survives untouched
	assert_eq!(fs::read(sandbox.state.path().join("clipboard.rbxm")).unwrap(), good);

	let sidecar: Value =
		serde_json::from_str(&fs::read_to_string(sandbox.state.path().join("clipboard.json")).unwrap()).unwrap();

	assert_eq!(sidecar["roots"][0]["path"], "Workspace/Good");
}

#[test]
fn the_root_bound_is_enforced_before_the_network() {
	let daemon = start_daemon(None);
	let bytes = payload(1);
	let sha = sha256_hex(&bytes);
	let journal = copy_plugin(&daemon, "bound-copy-plugin", bytes, sha, json!([]));

	let sandbox = CliSandbox::new();
	let many: Vec<String> = (0..257).map(|index| format!("Workspace/Item{index}")).collect();

	let project = daemon.root.to_string_lossy().into_owned();
	let port = daemon.port.to_string();
	let mut args: Vec<&str> = vec!["copy"];

	args.extend(many.iter().map(String::as_str));
	args.extend(["--project", &project, "--port", &port]);

	let output = sandbox.run(&args);

	assert!(!output.status.success(), "257 roots were accepted");
	assert!(
		cli_stderr(&output).contains("256"),
		"the bound must be named: {}",
		cli_stderr(&output)
	);
	assert!(
		journal_ops(&journal).is_empty(),
		"an over-bounded copy reached the plugin"
	);
}

#[test]
fn paste_with_an_empty_clipboard_is_a_clear_local_error() {
	// No daemon at all: the missing clipboard is diagnosed before any
	// connection is attempted
	let sandbox = CliSandbox::new();
	let project = sandbox.work.path().to_string_lossy().into_owned();

	let output = sandbox.run(&["paste", "--project", &project, "--port", "1"]);

	assert!(!output.status.success(), "an empty clipboard pasted");

	let message = cli_stderr(&output);

	assert!(
		message.contains("clipboard is empty") && message.contains("wsync copy"),
		"the error must say what to do: {message}"
	);
}

#[test]
fn a_torn_clipboard_is_refused_before_the_network() {
	let sandbox = CliSandbox::new();

	// A payload and a sidecar that do not describe each other — the state an
	// interrupted replace could leave
	fs::write(sandbox.state.path().join("clipboard.rbxm"), b"actual bytes").unwrap();
	fs::write(
		sandbox.state.path().join("clipboard.json"),
		json!({ "sha256": sha256_hex(b"other bytes"), "bytes": 11, "roots": [] }).to_string(),
	)
	.unwrap();

	let project = sandbox.work.path().to_string_lossy().into_owned();
	let output = sandbox.run(&["paste", "--project", &project, "--port", "1"]);

	assert!(!output.status.success(), "a torn clipboard pasted");
	assert!(
		cli_stderr(&output).contains("does not match its sidecar"),
		"the torn state must be named: {}",
		cli_stderr(&output)
	);
}
