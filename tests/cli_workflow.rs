//! Integration coverage for `wsync run` (run.json) and `wsync plan`
//! (plan.json): schema-v1 validation is exercised offline through the real
//! binary, and execution against a real daemon with a journalling fake
//! plugin proves the wire mapping, transactions, verification, waiting, and
//! idempotent replay end to end.

mod common;

use serde_json::{json, Value};
use std::{
	fs,
	path::{Path, PathBuf},
	sync::{Arc, Mutex},
};

use common::{
	cli_json, cli_stderr, cli_stdout, journal_args, journal_ops, spawn_cli_plugin, start_daemon, CliAnswer, CliJournal,
	CliSandbox, TestDaemon,
};

fn write_workflow(sandbox: &CliSandbox, name: &str, workflow: &Value) -> PathBuf {
	let path = sandbox.work.path().join(name);

	fs::write(&path, serde_json::to_string_pretty(workflow).unwrap()).unwrap();

	path
}

fn run_workflow(sandbox: &CliSandbox, daemon: &TestDaemon, file: &Path, extra: &[&str]) -> std::process::Output {
	let project = daemon.root.to_string_lossy().into_owned();
	let port = daemon.port.to_string();
	let file = file.to_string_lossy().into_owned();

	let mut args = vec!["run", "--file", &file, "--project", &project, "--port", &port];

	args.extend(extra.iter());

	sandbox.run(&args)
}

/// A fake plugin whose `get` answers from a scripted map and whose writes
/// succeed with echo values; `attr_ls` serves a scripted attribute map
fn workflow_plugin(daemon: &TestDaemon, name: &'static str, gets: Value, attrs: Value) -> CliJournal {
	spawn_cli_plugin(
		daemon,
		name,
		Arc::new(move |op, args| match op {
			"get" => {
				let path = args["path"].as_str().unwrap_or_default();
				let prop = args["prop"].as_str();

				let view = gets.get(path).cloned();

				match (view, prop) {
					(Some(view), None) => CliAnswer::Value(view),
					(Some(view), Some(prop)) => match view.get("properties").and_then(|props| props.get(prop)) {
						Some(value) => CliAnswer::Value(value.clone()),
						None => CliAnswer::Failure("NOT_FOUND", "no such property"),
					},
					(None, _) => CliAnswer::Failure("NOT_FOUND", "no such instance"),
				}
			}
			"attr_ls" => CliAnswer::Value(json!({ "path": args["path"], "attributes": attrs, "count": 1 })),
			"set" => CliAnswer::Value(json!({ "path": args["path"], "prop": args["prop"], "value": args["value"] })),
			"set_attr" | "rm_attr" => CliAnswer::Value(json!({ "path": args["path"], "name": args["name"] })),
			"add_tag" | "rm_tag" => CliAnswer::Value(json!({ "path": args["path"], "tag": args["tag"] })),
			"new" => CliAnswer::Value(json!({
				"path": format!("{}/{}", args["path"].as_str().unwrap_or_default(), args["name"].as_str().unwrap_or("New")),
				"class": args["class"],
			})),
			"rm" => CliAnswer::Value(json!({ "path": args["path"] })),
			"mv" => CliAnswer::Value(json!({ "from": args["from"], "path": args["to"] })),
			"eval" | "call" => CliAnswer::Value(json!({ "count": 0 })),
			"transaction_begin" | "transaction_finish" => CliAnswer::Value(json!({})),
			_ => CliAnswer::Failure("UNKNOWN_OP", "the fake plugin does not implement this op"),
		}),
	)
}

// ---------------------------------------------------------------------------
// Validation — fully offline
// ---------------------------------------------------------------------------

#[test]
fn validation_rejects_the_documented_failure_matrix() {
	let sandbox = CliSandbox::new();

	// (workflow, expected issue fragment). Every case must fail before any
	// network work — there is no daemon anywhere in this test
	let cases: Vec<(Value, &str)> = vec![
		(
			json!({ "version": 2, "steps": [{ "id": "a", "op": "get", "path": "W" }] }),
			"unsupported_version",
		),
		(json!({ "version": 1, "steps": [] }), "empty_workflow"),
		(
			json!({ "version": 1, "steps": [
				{ "id": "a", "op": "get", "path": "$b.value.path" },
				{ "id": "b", "op": "get", "path": "W" },
			] }),
			"forward_reference",
		),
		(
			json!({ "version": 1, "steps": [{ "id": "a", "op": "get", "path": "$a.value.path" }] }),
			"self_reference",
		),
		(
			json!({ "version": 1, "steps": [{ "id": "a", "op": "get", "path": "$ghost.value" }] }),
			"unknown_reference",
		),
		(
			json!({ "version": 1, "steps": [
				{ "id": "a", "op": "get", "path": "W" },
				{ "id": "a", "op": "get", "path": "W" },
			] }),
			"duplicate_step_id",
		),
		(
			json!({ "version": 1, "transactions": [{ "id": "tx" }], "steps": [
				{ "id": "a", "op": "set", "path": "W/A", "property": "Name", "value": "x", "transaction": "tx" },
				{ "id": "b", "op": "get", "path": "W" },
				{ "id": "c", "op": "set", "path": "W/C", "property": "Name", "value": "y", "transaction": "tx" },
			] }),
			"non_contiguous_atomic_transaction",
		),
		(
			json!({ "version": 1, "transactions": [{ "id": "tx" }], "steps": [
				{ "id": "a", "op": "eval", "source": "return 1", "transaction": "tx" },
			] }),
			"unsafe_atomic_operation",
		),
		(
			json!({ "version": 1, "steps": [
				{ "id": "a", "op": "set", "path": "W/A", "property": "Name", "value": "x", "transaction": "tx" },
			] }),
			"unknown_transaction",
		),
		(
			json!({ "version": 1, "steps": [
				{ "id": "a", "op": "set", "path": "W/A", "property": "Parent", "value": "W/B" },
			] }),
			"parent_write_refused",
		),
		(
			json!({ "version": 1, "steps": [
				{ "id": "a", "op": "mv", "from": "Workspace/A", "to": "ReplicatedStorage" },
			] }),
			"mv_crosses_service",
		),
		(
			json!({ "version": 1, "steps": [{ "id": "a", "op": "get", "path": "W", "surprise": 1 }] }),
			"unknown",
		),
		(
			json!({ "version": 1, "steps": [{ "id": "a", "op": "playtest", "context": "server" }] }),
			"invalid_playtest_script",
		),
		(
			json!({ "version": 1, "steps": [
				{ "id": "a", "op": "eval", "source": "return 1", "verify": true },
			] }),
			"verify_unsupported",
		),
	];

	for (workflow, fragment) in cases {
		let file = write_workflow(&sandbox, "reject.json", &workflow);
		let file_flag = file.to_string_lossy().into_owned();
		let output = sandbox.run(&["run", "--file", &file_flag]);

		assert!(!output.status.success(), "{workflow} must be rejected");
		assert!(
			cli_stderr(&output).contains(fragment),
			"expected `{fragment}` in the refusal for {workflow}: {}",
			cli_stderr(&output)
		);
	}
}

#[test]
fn dry_run_prints_the_normalized_plan_offline() {
	let sandbox = CliSandbox::new();

	let workflow = json!({
		"version": 1,
		"name": "demo",
		"transactions": [{ "id": "tx" }],
		"steps": [
			{ "id": "a", "op": "set", "path": "Workspace/Box", "property": "Name", "value": "Crate", "transaction": "tx" },
			{ "id": "b", "op": "get", "path": "$a.value.path" },
			{ "id": "c", "op": "assert", "actual": "$b.value.class", "check": { "op": "equals", "expected": "Part" } },
		],
	});

	let file = write_workflow(&sandbox, "plan.json", &workflow);
	let file_flag = file.to_string_lossy().into_owned();

	// No daemon exists; --dry-run must not want one
	let output = sandbox.run(&["run", "--file", &file_flag, "--dry-run", "--raw"]);

	assert!(output.status.success(), "dry-run failed: {}", cli_stderr(&output));

	let plan = cli_json(&output);

	assert_eq!(plan["ok"], true);
	assert_eq!(plan["dryRun"], true);
	assert_eq!(plan["steps"].as_array().unwrap().len(), 3);

	// The wire mapping and the dependency graph are part of the plan
	assert_eq!(plan["steps"][0]["wire"]["op"], "set");
	assert_eq!(plan["steps"][0]["wire"]["args"]["forceParent"], false);
	assert_eq!(plan["steps"][1]["dependencies"], json!(["a"]));
	assert_eq!(plan["steps"][2]["dependencies"], json!(["b"]));
	assert_eq!(plan["steps"][2]["executor"], "local");
	assert_eq!(plan["steps"][0]["transaction"], "tx");
}

// ---------------------------------------------------------------------------
// Execution against the fake plugin
// ---------------------------------------------------------------------------

#[test]
fn happy_path_resolves_references_and_asserts() {
	let daemon = start_daemon(None);
	let journal = workflow_plugin(
		&daemon,
		"wf-happy",
		json!({
			"Workspace/Box": { "path": "Workspace/Box", "class": "Part", "properties": { "Name": "Crate" } },
		}),
		json!({}),
	);

	let sandbox = CliSandbox::new();
	let workflow = json!({
		"version": 1,
		"steps": [
			{ "id": "rename", "op": "set", "path": "Workspace/Box", "property": "Name", "value": "Crate" },
			{ "id": "view", "op": "get", "path": "$rename.value.path" },
			{ "id": "class-is-part", "op": "assert", "actual": "$view.value.class",
			  "check": { "op": "equals", "expected": "Part" } },
			{ "id": "escape", "op": "set", "path": "Workspace/Box", "property": "Name", "value": "$$literal" },
		],
	});
	let file = write_workflow(&sandbox, "happy.json", &workflow);

	let output = run_workflow(&sandbox, &daemon, &file, &["--raw"]);

	assert!(output.status.success(), "workflow failed: {}", cli_stderr(&output));

	let outcome = cli_json(&output);

	assert_eq!(outcome["ok"], true);
	assert_eq!(outcome["schema"], "wsync.workflow-result.v1");
	assert_eq!(outcome["replayed"], false);
	assert_eq!(outcome["steps"].as_array().unwrap().len(), 4);
	assert!(outcome["steps"]
		.as_array()
		.unwrap()
		.iter()
		.all(|step| step["ok"] == true));

	// The reference fed the earlier result's path into the later op, and the
	// `$$` escape reached the plugin as a literal `$`
	let ops = journal_ops(&journal);

	assert_eq!(ops, vec!["set", "get", "set"], "assert is local — never an op");

	let get = journal_args(&journal, "get");

	assert_eq!(get["path"], "Workspace/Box");

	let sets: Vec<Value> = journal
		.lock()
		.unwrap()
		.iter()
		.filter(|(op, _)| op == "set")
		.map(|(_, args)| args.clone())
		.collect();

	assert_eq!(
		sets[1]["value"], "$literal",
		"`$$literal` must reach the wire as `$literal`"
	);
}

#[test]
fn keep_going_controls_how_far_a_failing_run_gets() {
	for (keep_going, expected_ops) in [(false, vec!["rm"]), (true, vec!["rm", "get"])] {
		let daemon = start_daemon(None);
		let journal = spawn_cli_plugin(
			&daemon,
			"wf-keep",
			Arc::new(|op, args| match op {
				"rm" => CliAnswer::Failure("LOCKED", "this instance refuses to die"),
				"get" => CliAnswer::Value(json!({ "path": args["path"], "class": "Folder" })),
				_ => CliAnswer::Failure("UNKNOWN_OP", "unexpected op"),
			}),
		);

		let sandbox = CliSandbox::new();
		let workflow = json!({
			"version": 1,
			"steps": [
				{ "id": "doomed", "op": "rm", "path": "Workspace/Locked" },
				{ "id": "after", "op": "get", "path": "Workspace" },
			],
		});
		let file = write_workflow(&sandbox, "keep.json", &workflow);

		let extra: Vec<&str> = if keep_going {
			vec!["--raw", "--keep-going"]
		} else {
			vec!["--raw"]
		};
		let output = run_workflow(&sandbox, &daemon, &file, &extra);

		// A failed step fails the run either way; --keep-going only decides
		// how far it got
		assert!(!output.status.success());

		let outcome: Value = serde_json::from_str(cli_stdout(&output).lines().next().unwrap()).unwrap();

		assert_eq!(outcome["ok"], false);
		assert_eq!(journal_ops(&journal), expected_ops, "keep_going={keep_going}");
		assert_eq!(
			outcome["steps"].as_array().unwrap().len(),
			expected_ops.len(),
			"one report per attempted step"
		);
		assert_eq!(outcome["steps"][0]["error"]["code"], "LOCKED");
	}
}

#[test]
fn atomic_transactions_bracket_commit_and_cancel() {
	// Success: begin → member ops → finish {commit: true}
	let daemon = start_daemon(None);
	let journal = workflow_plugin(&daemon, "wf-tx-ok", json!({}), json!({}));
	let sandbox = CliSandbox::new();

	let workflow = json!({
		"version": 1,
		"name": "tx demo",
		"transactions": [{ "id": "tx" }],
		"steps": [
			{ "id": "a", "op": "set", "path": "W/A", "property": "Name", "value": "x", "transaction": "tx" },
			{ "id": "b", "op": "set", "path": "W/B", "property": "Name", "value": "y", "transaction": "tx" },
		],
	});
	let file = write_workflow(&sandbox, "tx.json", &workflow);
	let output = run_workflow(&sandbox, &daemon, &file, &["--raw"]);

	assert!(
		output.status.success(),
		"transactional run failed: {}",
		cli_stderr(&output)
	);
	assert_eq!(
		journal_ops(&journal),
		vec!["transaction_begin", "set", "set", "transaction_finish"]
	);
	assert_eq!(journal_args(&journal, "transaction_finish")["commit"], true);
	assert!(
		journal_args(&journal, "transaction_begin")["name"]
			.as_str()
			.unwrap()
			.contains("tx demo"),
		"the recording is named after the workflow"
	);

	// Failure inside the group: begin → failing op → finish {commit: false},
	// and nothing after the group runs
	let daemon = start_daemon(None);
	let journal = spawn_cli_plugin(
		&daemon,
		"wf-tx-cancel",
		Arc::new(|op, _args| match op {
			"transaction_begin" | "transaction_finish" => CliAnswer::Value(json!({})),
			"set" => CliAnswer::Failure("REFUSED", "no"),
			"get" => CliAnswer::Value(json!({ "class": "Folder" })),
			_ => CliAnswer::Failure("UNKNOWN_OP", "unexpected op"),
		}),
	);

	let workflow = json!({
		"version": 1,
		"transactions": [{ "id": "tx" }],
		"steps": [
			{ "id": "a", "op": "set", "path": "W/A", "property": "Name", "value": "x", "transaction": "tx" },
			{ "id": "after", "op": "get", "path": "W" },
		],
	});
	let file = write_workflow(&sandbox, "tx-cancel.json", &workflow);
	let output = run_workflow(&sandbox, &daemon, &file, &["--raw", "--keep-going"]);

	assert!(!output.status.success());
	assert_eq!(
		journal_ops(&journal),
		vec!["transaction_begin", "set", "transaction_finish"],
		"an atomic failure cancels and ends the run even under --keep-going"
	);
	assert_eq!(journal_args(&journal, "transaction_finish")["commit"], false);

	let outcome: Value = serde_json::from_str(cli_stdout(&output).lines().next().unwrap()).unwrap();

	assert_eq!(outcome["rolledBack"], true);
}

#[test]
fn wait_polls_until_the_assertion_passes() {
	let daemon = start_daemon(None);
	let reads = Mutex::new(0_u32);
	let journal = spawn_cli_plugin(
		&daemon,
		"wf-wait",
		Arc::new(move |op, _args| match op {
			"get" => {
				let mut reads = reads.lock().unwrap();

				*reads += 1;

				// The first two polls see the old value
				CliAnswer::Value(json!(if *reads >= 3 { "Ready" } else { "Loading" }))
			}
			_ => CliAnswer::Failure("UNKNOWN_OP", "unexpected op"),
		}),
	);

	let sandbox = CliSandbox::new();
	let workflow = json!({
		"version": 1,
		"steps": [
			{ "id": "ready", "op": "wait", "path": "ReplicatedStorage/State", "property": "Value",
			  "check": { "op": "equals", "expected": "Ready" },
			  "timeoutMs": 10_000, "pollIntervalMs": 50 },
		],
	});
	let file = write_workflow(&sandbox, "wait.json", &workflow);
	let output = run_workflow(&sandbox, &daemon, &file, &["--raw"]);

	assert!(output.status.success(), "wait failed: {}", cli_stderr(&output));

	let outcome = cli_json(&output);

	assert_eq!(outcome["results"]["ready"]["value"]["passed"], true);
	assert_eq!(outcome["results"]["ready"]["value"]["polls"], 3);
	assert!(journal_ops(&journal).iter().all(|op| op == "get"));
}

#[test]
fn verify_reads_supported_writes_back() {
	// Honest plugin: the readback matches, verify passes
	let daemon = start_daemon(None);
	let journal = workflow_plugin(
		&daemon,
		"wf-verify-ok",
		json!({
			"Workspace/Box": { "path": "Workspace/Box", "class": "Part", "properties": { "Name": "Crate" } },
		}),
		json!({}),
	);

	let sandbox = CliSandbox::new();
	let workflow = json!({
		"version": 1,
		"steps": [
			{ "id": "rename", "op": "set", "path": "Workspace/Box", "property": "Name", "value": "Crate",
			  "verify": true },
		],
	});
	let file = write_workflow(&sandbox, "verify.json", &workflow);
	let output = run_workflow(&sandbox, &daemon, &file, &["--raw"]);

	assert!(output.status.success(), "verified set failed: {}", cli_stderr(&output));
	assert_eq!(
		journal_ops(&journal),
		vec!["set", "get"],
		"verify:true reads the write back"
	);

	let outcome = cli_json(&output);

	assert_eq!(outcome["steps"][0]["verified"], true);

	// Lying plugin: the write claims success but reads back wrong — the step
	// fails VERIFY_FAILED even though the op said ok
	let daemon = start_daemon(None);
	let _journal = spawn_cli_plugin(
		&daemon,
		"wf-verify-bad",
		Arc::new(|op, args| match op {
			"set" => CliAnswer::Value(json!({ "path": args["path"] })),
			"get" => CliAnswer::Value(json!("SomethingElse")),
			_ => CliAnswer::Failure("UNKNOWN_OP", "unexpected op"),
		}),
	);

	let output = run_workflow(&sandbox, &daemon, &file, &["--raw"]);

	assert!(!output.status.success());

	let outcome: Value = serde_json::from_str(cli_stdout(&output).lines().next().unwrap()).unwrap();

	assert_eq!(outcome["steps"][0]["error"]["code"], "VERIFY_FAILED");
	assert_eq!(outcome["steps"][0]["verified"], false);
}

#[test]
fn preconditions_reject_stale_targets() {
	let daemon = start_daemon(None);
	let journal = workflow_plugin(
		&daemon,
		"wf-precond",
		json!({
			"Workspace/Box": { "path": "Workspace/Box", "class": "Part", "etag": "v2",
			  "properties": { "Name": "Crate" } },
		}),
		json!({}),
	);

	let sandbox = CliSandbox::new();

	// expectedClass mismatch: the write never happens
	let workflow = json!({
		"version": 1,
		"steps": [
			{ "id": "rename", "op": "set", "path": "Workspace/Box", "property": "Name", "value": "x",
			  "expectedClass": "Model" },
		],
	});
	let file = write_workflow(&sandbox, "precond.json", &workflow);
	let output = run_workflow(&sandbox, &daemon, &file, &["--raw"]);

	assert!(!output.status.success());

	let outcome: Value = serde_json::from_str(cli_stdout(&output).lines().next().unwrap()).unwrap();

	assert_eq!(outcome["steps"][0]["error"]["code"], "PRECONDITION_FAILED");
	assert_eq!(
		journal_ops(&journal),
		vec!["get"],
		"the guarded write never reached the wire"
	);

	// A matching etag lets the write through; a stale one refuses it
	let workflow = json!({
		"version": 1,
		"steps": [
			{ "id": "rename", "op": "set", "path": "Workspace/Box", "property": "Name", "value": "x",
			  "expectedClass": "Part", "etag": "v2" },
		],
	});
	let file = write_workflow(&sandbox, "precond-ok.json", &workflow);
	let output = run_workflow(&sandbox, &daemon, &file, &["--raw"]);

	assert!(
		output.status.success(),
		"matching preconditions failed: {}",
		cli_stderr(&output)
	);

	let workflow = json!({
		"version": 1,
		"steps": [
			{ "id": "rename", "op": "set", "path": "Workspace/Box", "property": "Name", "value": "x",
			  "etag": "v1" },
		],
	});
	let file = write_workflow(&sandbox, "precond-stale.json", &workflow);
	let output = run_workflow(&sandbox, &daemon, &file, &["--raw"]);

	assert!(!output.status.success(), "a stale etag must refuse the write");
}

#[test]
fn upload_steps_fail_cleanly_in_this_build() {
	let daemon = start_daemon(None);
	let journal = workflow_plugin(&daemon, "wf-upload", json!({}), json!({}));

	let sandbox = CliSandbox::new();
	let workflow = json!({
		"version": 1,
		"steps": [
			{ "id": "ship", "op": "upload", "paths": ["Workspace/Model"] },
		],
	});
	let file = write_workflow(&sandbox, "upload.json", &workflow);
	let output = run_workflow(&sandbox, &daemon, &file, &["--raw"]);

	assert!(!output.status.success());

	let outcome: Value = serde_json::from_str(cli_stdout(&output).lines().next().unwrap()).unwrap();

	assert_eq!(outcome["steps"][0]["error"]["code"], "NOT_AVAILABLE");
	assert!(outcome["steps"][0]["error"]["message"]
		.as_str()
		.unwrap()
		.contains("not available in this build"),);
	assert!(journal_ops(&journal).is_empty(), "an unavailable step sends nothing");
}

#[test]
fn idempotency_replays_without_repeating_side_effects() {
	let daemon = start_daemon(None);
	let journal = workflow_plugin(&daemon, "wf-idem", json!({}), json!({}));

	let sandbox = CliSandbox::new();
	let workflow = json!({
		"version": 1,
		"idempotencyKey": "deploy-42",
		"steps": [
			{ "id": "rename", "op": "set", "path": "Workspace/Box", "property": "Name", "value": "Crate" },
		],
	});
	let file = write_workflow(&sandbox, "idem.json", &workflow);

	// First run executes and journals the result under the *project's*
	// workspace
	let output = run_workflow(&sandbox, &daemon, &file, &["--raw"]);

	assert!(output.status.success(), "first run failed: {}", cli_stderr(&output));
	assert_eq!(journal_ops(&journal), vec!["set"]);

	let journal_dir = daemon.root.join(".wsync-workflows");
	let records: Vec<_> = fs::read_dir(&journal_dir)
		.expect("the idempotency journal directory exists")
		.filter_map(Result::ok)
		.filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
		.collect();

	assert_eq!(records.len(), 1, "one record per key");

	// Second run replays: the outcome returns with replayed:true and the
	// plugin journal proves no op was repeated
	let output = run_workflow(&sandbox, &daemon, &file, &["--raw"]);

	assert!(output.status.success(), "replay failed: {}", cli_stderr(&output));

	let outcome = cli_json(&output);

	assert_eq!(outcome["replayed"], true);
	assert_eq!(outcome["ok"], true);
	assert_eq!(
		journal_ops(&journal),
		vec!["set"],
		"the replay must not repeat side effects — the journal proves it"
	);

	// The same key with different content is a collision, never a replay
	let changed = json!({
		"version": 1,
		"idempotencyKey": "deploy-42",
		"steps": [
			{ "id": "rename", "op": "set", "path": "Workspace/Box", "property": "Name", "value": "Different" },
		],
	});
	let file = write_workflow(&sandbox, "idem-changed.json", &changed);
	let output = run_workflow(&sandbox, &daemon, &file, &["--raw"]);

	assert!(!output.status.success());
	assert!(
		cli_stderr(&output).contains("collision"),
		"a reused key with new content must name the collision: {}",
		cli_stderr(&output)
	);
	assert_eq!(journal_ops(&journal), vec!["set"], "a collision executes nothing");
}

// ---------------------------------------------------------------------------
// plan — read-only, offline
// ---------------------------------------------------------------------------

#[test]
fn plan_prints_the_documented_shape_without_connecting() {
	let sandbox = CliSandbox::new();

	let output = sandbox.run(&[
		"plan",
		"set",
		"--path",
		"ReplicatedStorage/Config",
		"--prop",
		"Source",
		"--value",
		"\"return {}\"",
	]);

	assert!(output.status.success(), "plan set failed: {}", cli_stderr(&output));

	let plan = cli_json(&output);

	assert_eq!(plan["ok"], true);
	assert_eq!(plan["readOnly"], true);
	assert_eq!(plan["operation"], "set");
	assert_eq!(plan["mutates"], json!(["studio"]));
	assert_eq!(plan["requires"], json!(["daemon", "studio-plugin"]));
	assert_eq!(plan["risks"], json!([]));
	assert!(plan["executeCommand"].as_str().unwrap().starts_with("wsync set --path"));

	// A planned Parent write carries the guardrail as a risk
	let output = sandbox.run(&["plan", "set", "--path", "W/A", "--prop", "Parent", "--value", "\"W/B\""]);
	let plan = cli_json(&output);

	assert!(plan["risks"][0].as_str().unwrap().contains("wsync mv"));

	// new / rm / mv / resolve all answer with the same anatomy
	let output = sandbox.run(&[
		"plan",
		"new",
		"--path",
		"ReplicatedStorage",
		"--class",
		"Folder",
		"--name",
		"Shared",
	]);
	let plan = cli_json(&output);

	assert_eq!(plan["operation"], "new");
	assert_eq!(plan["args"]["class"], "Folder");

	let output = sandbox.run(&["plan", "rm", "--path", "Workspace/OldPart"]);
	let plan = cli_json(&output);

	assert!(plan["risks"][0].as_str().unwrap().contains("destructive"));

	let output = sandbox.run(&["plan", "mv", "--from", "Workspace/A", "--to", "ReplicatedStorage"]);
	let plan = cli_json(&output);

	assert!(plan["risks"][0].as_str().unwrap().contains("--force"));

	let output = sandbox.run(&["plan", "resolve", "--path", "src/Foo.luau", "--studio"]);
	let plan = cli_json(&output);

	assert_eq!(plan["operation"], "resolve");
	assert_eq!(plan["args"]["choice"], "studio");
	assert_eq!(plan["mutates"], json!(["disk", "studio"]));

	// resolve needs exactly one side
	let output = sandbox.run(&["plan", "resolve", "--path", "src/Foo.luau"]);

	assert!(!output.status.success());
	assert!(cli_stderr(&output).contains("--disk or --studio"));

	// A malformed --props fails plan new exactly as it would fail `wsync new`
	let output = sandbox.run(&["plan", "new", "--path", "RS", "--class", "Folder", "--props", "[1]"]);

	assert!(!output.status.success());
	assert!(cli_stderr(&output).contains("--props"));
}
