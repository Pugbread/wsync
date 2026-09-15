//! Job identity stays attached from the CLI through runtime/lifecycle requests.
mod common;

use common::{cli_json, cli_stderr, journal_args, spawn_cli_plugin, start_daemon, CliAnswer, CliSandbox};
use serde_json::json;
use std::{fs, process::Command, sync::Arc};

#[test]
fn low_level_commands_forward_explicit_job_and_wait_context() {
	let daemon = start_daemon(None);
	let journal = spawn_cli_plugin(
		&daemon,
		"job-identity",
		Arc::new(|op, _| match op {
			"playtest_exec" => CliAnswer::Value(json!({"ok": true, "value": 7, "count": 1})),
			_ => CliAnswer::Value(
				json!({"jobId": "pinned-job", "contexts": [], "ready": [], "entries": [], "elements": [], "performed": 1}),
			),
		}),
	);
	let sandbox = CliSandbox::new();
	let project = daemon.root.to_string_lossy().to_string();
	let port = daemon.port.to_string();
	for (subcommand, extra) in [
		("status", vec![]),
		("contexts", vec![]),
		("wait", vec!["--context", "client:2"]),
		("exec", vec!["--context", "server", "--source", "return 7"]),
		("logs", vec!["--context", "server"]),
		("ui", vec!["--context", "client:1"]),
		(
			"input",
			vec![
				"--context",
				"client:1",
				"--actions",
				"[{\"type\":\"wait\",\"duration\":0.01}]",
			],
		),
		("stop", vec![]),
		("request", vec!["--context", "server", "--op", "logs"]),
	] {
		let mut args = vec![
			"playtest",
			subcommand,
			"--project",
			&project,
			"--port",
			&port,
			"--job",
			"pinned-job",
			"--raw",
		];
		args.extend(extra);
		let output = sandbox.run(&args);
		assert!(output.status.success(), "{subcommand}: {}", cli_stderr(&output));
		assert_eq!(
			journal_args(&journal, &format!("playtest_{subcommand}"))["jobId"],
			"pinned-job"
		);
		if subcommand == "exec" {
			assert!(
				cli_json(&output)["meta"]["operationId"].is_string(),
				"execution must preserve its receipt"
			);
		}
	}
	assert_eq!(journal_args(&journal, "playtest_wait")["context"], "client:2");
}

#[test]
fn start_wait_is_pinned_to_the_job_returned_by_start() {
	let daemon = start_daemon(None);
	let journal = spawn_cli_plugin(
		&daemon,
		"start-job-identity",
		Arc::new(|op, _| match op {
			"playtest_start" => CliAnswer::Value(json!({"jobId": "new-job", "status": "starting"})),
			"playtest_wait" => CliAnswer::Value(json!({"ready": ["server"], "contexts": []})),
			_ => CliAnswer::Failure("UNEXPECTED", "unexpected op"),
		}),
	);
	let sandbox = CliSandbox::new();
	let project = daemon.root.to_string_lossy().to_string();
	let port = daemon.port.to_string();
	let output = sandbox.run(&[
		"playtest",
		"start",
		"--project",
		&project,
		"--port",
		&port,
		"--mode",
		"run",
		"--wait",
		"--raw",
	]);
	assert!(output.status.success(), "{}", cli_stderr(&output));
	assert_eq!(journal_args(&journal, "playtest_wait")["jobId"], "new-job");
	let invalid = sandbox.run(&["playtest", "start", "--job", "old-job", "--project", "/missing"]);
	assert!(!invalid.status.success());
	assert!(cli_stderr(&invalid).contains("start creates a new generation"));
}

/// Execute the production coordinator guards with a real Jobs state machine.
/// Only Roblox transport/time services are replaced. Rust-only CI may lack Luau;
/// the dedicated plugin job installs it, and local verification runs this probe.
#[test]
fn coordinator_rejects_old_jobs_and_waits_for_the_named_context() {
	let luau = std::env::var("LUAU").unwrap_or_else(|_| "luau".into());
	if Command::new(&luau).arg("--help").output().is_err() {
		eprintln!("SKIP coordinator Luau behavior probe: install luau or set LUAU");
		return;
	}
	let source = include_str!("../plugin/src/Playtest/init.luau");
	let extract = |start: &str, end: &str| source.split_once(start).unwrap().1.split_once(end).unwrap().0;
	let guard = format!(
		"local function requireContext({}",
		extract(
			"local function requireContext(",
			"--------------------------------------------------------------------------------\n-- Correlated RPC"
		)
	);
	let wait = format!(
		"function Playtest.waitForContexts({}",
		extract("function Playtest.waitForContexts(", "--- `playtest_stop`")
	);
	let jobs = include_str!("../plugin/src/Playtest/Jobs.luau");
	let harness = format!(
		r#"
local Jobs = (function()
{jobs}
end)()
local registry = Jobs.newRegistry()
local links = {{}}
local Playtest = {{}}
local Error = {{CONFLICT="CONFLICT",NOT_FOUND="NOT_FOUND",INVALID_ARGUMENT="INVALID_ARGUMENT",TIMEOUT="TIMEOUT",new=function(code,message) return code .. ": " .. message end}}
local function requireAvailable() end
local function pruneDeadContexts() end
{guard}
local ticks = 0
local onWait = function() end
local task = {{wait=function() ticks += 1; onWait() end}}
{wait}
local old = {{id="old",status="stopped"}}
local current = {{id="new",status="running"}}
registry.jobs.old = old
registry.jobs.new = current
registry.latestJobId = "new"
local function context(name, runtime, job)
 local value = {{name=name,runtimeId=runtime,jobId=job,alive=true,role="client"}}
 registry.contextsByName[name] = value
 links[runtime] = {{connection={{Connected=true}}}}
 return value
end
context("client:1", "r1", "new")
local ok, err = pcall(requireContext, "client:1", "old")
assert(not ok and string.find(tostring(err), "CONFLICT"), "old job must never select new context")
local selected = requireContext("client:1", "new")
assert(selected.jobId == "new")
-- Even an omitted job cannot select a leftover old generation context.
context("server", "old-server", "old")
old.status = "running"
local oldOk = pcall(requireContext, "server", nil)
assert(not oldOk, "leftover old live context must be rejected")
old.status = "stopped"
registry.contextsByName.server = nil
-- A sufficient count is not sufficient for a specific requested context.
onWait = function() context("client:2", "r2", "new") end
local result = Playtest.waitForContexts(1, 1000, "new", "client:2")
assert(ticks == 1 and result.jobId == "new", "wait must yield until client:2 arrives")
registry.contextsByName["client:2"] = nil
onWait = function() registry.latestJobId = "replacement" end
local changed, changedError = pcall(Playtest.waitForContexts, 1, 1000, "new", "client:2")
assert(not changed and string.find(tostring(changedError), "CONFLICT"), "wait must reject replacement generation")
print("playtest generation and named-context guards passed")
"#
	);
	let dir = tempfile::TempDir::new().unwrap();
	let path = dir.path().join("playtest-identity.luau");
	fs::write(&path, harness).unwrap();
	let output = Command::new(luau).arg(&path).output().unwrap();
	assert!(
		output.status.success(),
		"{}\n{}",
		String::from_utf8_lossy(&output.stdout),
		String::from_utf8_lossy(&output.stderr)
	);
}

#[test]
fn start_status_failure_emits_one_acknowledgement_with_unverified_evidence() {
	let daemon = start_daemon(None);
	let journal = spawn_cli_plugin(
		&daemon,
		"start-status-failure",
		Arc::new(|op, _| match op {
			"playtest_start" => CliAnswer::Value(json!({"jobId": "accepted-job", "status": "starting"})),
			"playtest_status" => CliAnswer::Failure("PLUGIN_ERROR", "lost status after accepted start"),
			_ => CliAnswer::Failure("UNEXPECTED", "no automatic lifecycle recovery is allowed"),
		}),
	);
	let sandbox = CliSandbox::new();
	let project = daemon.root.to_string_lossy().to_string();
	let port = daemon.port.to_string();
	let output = sandbox.run(&["playtest", "start", "--project", &project, "--port", &port, "--raw"]);
	assert!(output.status.success(), "{}", cli_stderr(&output));
	let stdout = common::cli_stdout(&output);
	assert_eq!(stdout.lines().filter(|line| !line.is_empty()).count(), 1, "{stdout}");
	let result = cli_json(&output);
	assert_eq!(result["ok"], true);
	assert_eq!(result["jobId"], "accepted-job");
	assert_eq!(result["verification"]["state"], "unverified");
	assert_eq!(result["verification"]["probe"]["ok"], false);
	assert!(result["verification"]["probe"]["meta"]["operationId"].is_string());
	assert!(result["meta"]["operationId"].is_string());
	assert!(!common::journal_ops(&journal).iter().any(|op| op == "playtest_stop"));
}
