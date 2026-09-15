//! Regression cases taken from real command retry loops. Test the complete
//! CLI/daemon boundary with a fake Studio; never mutate a user's place.
mod common;
use clap::Parser;
use common::{
	cli_json, cli_stderr, cli_stdout, journal_ops, spawn_cli_plugin, start_daemon_with_state_dir, CliAnswer,
	CliSandbox, TestDaemon,
};
use serde_json::{json, Value};
use std::{fs, sync::Arc};
use wsync::cli::Cli;

fn run(sandbox: &CliSandbox, daemon: &TestDaemon, args: &[&str]) -> std::process::Output {
	let mut args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
	args.extend([
		"--project".into(),
		daemon.root.to_string_lossy().into_owned(),
		"--port".into(),
		daemon.port.to_string(),
	]);
	sandbox.run(&args.iter().map(String::as_str).collect::<Vec<_>>())
}

#[test]
fn historical_argument_forms_parse_without_option_guessing() {
	assert!(matches!(
		Cli::try_parse_from(["wsync", "exec", "--help"]).err().map(|e| e.kind()),
		Some(clap::error::ErrorKind::DisplayHelp)
	));
	for args in [
		vec!["source", "ReplicatedStorage/Code", "--output", "code.luau"],
		vec!["capture", "model", "Workspace/Box", "--direction", "-1,0,0"],
		vec![
			"capture",
			"model",
			"Workspace/Box",
			"--camera-cframe",
			"-1,0,0,1,0,0,0,1,0,0,0,1",
		],
		vec!["query", "Workspace/**", "--raw", "--props", "Name"],
		vec!["build", "--project", ".", "--output", "game.rbxl"],
		vec!["attr", "list", "--path", "Workspace/Box"],
		vec!["eval", "--source", "--!strict\nreturn Vector3.new(1,2,3)"],
		vec!["eval", "--source-file", "probe.luau"],
		vec!["eval", "--stdin"],
		vec![
			"set",
			"--path",
			"Workspace/Box",
			"--prop",
			"Transparency",
			"--value",
			"-1",
		],
		vec!["backlog", "restore", "entry", "--project", ".", "--raw"],
	] {
		let full = std::iter::once("wsync").chain(args.iter().copied()).collect::<Vec<_>>();
		assert!(Cli::try_parse_from(&full).is_ok(), "{full:?}");
	}
	assert!(Cli::try_parse_from(["wsync", "eval", "--stdin", "--source", "return 1"]).is_err());
	assert!(Cli::try_parse_from(["wsync", "build", ".", "--project", "."]).is_err());
}

#[test]
fn tagged_writes_send_exact_values_and_reject_bad_batches_before_delivery() {
	let daemon = start_daemon_with_state_dir();
	let journal = spawn_cli_plugin(
		&daemon,
		"typed-writes",
		Arc::new(|op, args| match op {
			"set" | "set_attr" => CliAnswer::Value(json!({"value":args["value"],"path":args["path"]})),
			_ => CliAnswer::Failure("UNEXPECTED", "invalid batch must not be delivered"),
		}),
	);
	let sandbox = CliSandbox::new();
	for value in [
		json!({"__type":"Vector3","x":1,"y":2,"z":3}),
		json!({"__type":"Color3","r":0.1,"g":0.2,"b":0.3}),
	] {
		let output = run(
			&sandbox,
			&daemon,
			&[
				"set",
				"--path",
				"Workspace/Box",
				"--prop",
				"Size",
				"--value",
				&value.to_string(),
				"--raw",
			],
		);
		assert!(output.status.success(), "{}", cli_stderr(&output));
		assert_eq!(cli_json(&output)["value"], value);
	}
	assert_eq!(journal_ops(&journal).iter().filter(|op| *op == "set").count(), 2);
	let batch = sandbox.work.path().join("bad-batch.json");
	fs::write(
		&batch,
		json!([
			{"path":"Workspace/Box","prop":"Name","value":"Good"},
			{"path":"Workspace/Box","prop":"Color","value":{"__type":"Color3","R":255,"G":0,"B":0}}
		])
		.to_string(),
	)
	.unwrap();
	let output = run(&sandbox, &daemon, &["set", "--batch", batch.to_str().unwrap(), "--raw"]);
	assert!(!output.status.success());
	assert_eq!(cli_json(&output)["meta"]["completion"], "notStarted");
	assert!(cli_stderr(&output).contains("Color3"));
	assert!(!journal_ops(&journal).iter().any(|op| op == "set_batch"));
}

#[test]
fn malformed_type_has_offline_actionable_raw_error() {
	let sandbox = CliSandbox::new();
	let output = sandbox.run(&[
		"set",
		"--path",
		"Workspace/Box",
		"--prop",
		"Size",
		"--value",
		r#"{"__type":"Vector3","value":[1,2,3]}"#,
		"--raw",
	]);
	assert!(!output.status.success());
	let result = cli_json(&output);
	assert_eq!(result["error"]["code"], "INVALID_ARGUMENT");
	assert_eq!(result["meta"]["completion"], "notStarted");
	assert!(result["error"]["message"].as_str().unwrap().contains("Vector3"));
	assert!(!cli_stderr(&output).contains("No WSync daemon"));
}

#[test]
fn real_command_help_teaches_vector_and_color_shapes() {
	let sandbox = CliSandbox::new();
	for args in [
		vec!["set", "--help"],
		vec!["new", "--help"],
		vec!["attr", "set", "--help"],
		vec!["call", "--help"],
	] {
		let output = sandbox.run(&args);
		assert!(output.status.success());
		let help = cli_stdout(&output);
		assert!(
			help.contains("\"__type\":\"Vector3\"") && help.contains("\"x\":1"),
			"{help}"
		);
		assert!(
			help.contains("\"__type\":\"Color3\"") && help.contains("\"r\":"),
			"{help}"
		);
		assert!(help.contains("wsync commands types"));
	}
}

#[test]
fn retired_commands_explain_current_recovery_workflow() {
	let output = CliSandbox::new().run(&["conflicts", "--raw"]);
	assert_eq!(output.status.code(), Some(2));
	assert!(cli_stderr(&output).contains("retired conflict-review"));
	assert!(cli_stderr(&output).contains("backlog list"));
}

#[test]
fn source_input_preserves_comments_files_and_stdin() {
	let daemon = start_daemon_with_state_dir();
	let journal = spawn_cli_plugin(
		&daemon,
		"source-input",
		Arc::new(|_, _| CliAnswer::Value(json!({"count":1,"results":[2]}))),
	);
	let sandbox = CliSandbox::new();
	let source = "--!strict\n-- a comment\nreturn 1+1\n";
	let file = sandbox.work.path().join("probe.luau");
	fs::write(&file, source).unwrap();
	for extra in [vec!["--source", source], vec!["--source-file", file.to_str().unwrap()]] {
		let mut args = vec!["eval", "--raw"];
		args.extend(extra);
		let output = run(&sandbox, &daemon, &args);
		assert!(output.status.success(), "{}", cli_stderr(&output));
	}
	let output = sandbox.run_with_stdin(
		&[
			"eval",
			"--stdin",
			"--raw",
			"--project",
			daemon.root.to_str().unwrap(),
			"--port",
			&daemon.port.to_string(),
		],
		source.as_bytes(),
	);
	assert!(output.status.success(), "{}", cli_stderr(&output));
	let inputs: Vec<Value> = journal
		.lock()
		.unwrap()
		.iter()
		.filter(|(op, _)| op == "eval")
		.map(|(_, args)| args.clone())
		.collect();
	assert_eq!(inputs.len(), 3);
	for input in inputs {
		assert_eq!(input["source"], source);
	}
}

#[test]
fn source_export_is_exact_and_failed_reads_preserve_destination() {
	let daemon = start_daemon_with_state_dir();
	spawn_cli_plugin(
		&daemon,
		"source-export",
		Arc::new(|_, args| {
			if args["path"] == "ReplicatedStorage/Good" {
				CliAnswer::Value(json!("return 42"))
			} else {
				CliAnswer::Failure("NOT_FOUND", "script missing")
			}
		}),
	);
	let sandbox = CliSandbox::new();
	let file = sandbox.work.path().join("source.luau");
	let output = run(
		&sandbox,
		&daemon,
		&["source", "ReplicatedStorage/Good", "--output", file.to_str().unwrap()],
	);
	assert!(output.status.success(), "{}", cli_stderr(&output));
	assert_eq!(fs::read_to_string(&file).unwrap(), "return 42");
	let output = run(
		&sandbox,
		&daemon,
		&[
			"source",
			"ReplicatedStorage/Missing",
			"--output",
			file.to_str().unwrap(),
			"--raw",
		],
	);
	assert!(!output.status.success());
	assert_eq!(fs::read_to_string(&file).unwrap(), "return 42");
	assert_eq!(cli_json(&output)["ok"], false);
}

#[test]
fn playtest_exec_prints_actual_value_and_fails_on_source_error() {
	let daemon = start_daemon_with_state_dir();
	spawn_cli_plugin(
		&daemon,
		"exec-result",
		Arc::new(|_, args| {
			if args["source"] == "return 1+1" {
				CliAnswer::Value(json!({"ok":true,"count":1,"value":2}))
			} else {
				CliAnswer::Value(json!({"ok":false,"error":"intentional source failure"}))
			}
		}),
	);
	let sandbox = CliSandbox::new();
	let output = run(
		&sandbox,
		&daemon,
		&["playtest", "exec", "--context", "server", "--source", "return 1+1"],
	);
	assert!(output.status.success(), "{}", cli_stderr(&output));
	assert!(cli_stdout(&output).contains("  1  2"));
	let output = run(
		&sandbox,
		&daemon,
		&[
			"playtest",
			"exec",
			"--context",
			"server",
			"--source",
			"error('bad')",
			"--raw",
		],
	);
	assert!(!output.status.success());
	assert_eq!(cli_json(&output)["ok"], false);
}
