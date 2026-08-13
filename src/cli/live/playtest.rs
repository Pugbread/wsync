//! The playtest surface: `playtest run` plus the low-level
//! `start|status|contexts|wait|exec|logs|ui|input|capture|stop|request`
//! controls (playtest.json; Design §10.2, "Agent runtime").
//!
//! `run` owns a complete foreground session: it reads the playscript locally
//! (only source text ever travels), starts a run-owned job
//! (`playtest_run_start`), pages the job's record stream forward with
//! `playtest_run_poll {jobId, sinceSeq}`, and stops the playtest when the
//! job ends unless `--keep-open`. Every option is validated before a socket
//! is opened, so a malformed `--args` or an out-of-range `--timeout` never
//! starts a job.
//!
//! The record stream is the contract: `started`, `ready`, `event`, `log`,
//! `clientResult`, `dropped`, `aborted`, and `result` records, each an
//! independent JSON object. Under `--raw` the CLI is a pure NDJSON pass-through
//! — one record per line, never deduplicated or sampled, so source rate
//! limiting stays visible through accurate `dropped` counts. The exit code is
//! record-driven: main return or `done` exits 0, a script error or `fail`
//! exits 2, the hard timeout exits 3, an externally ended job exits 4 with its
//! fetched job status, and a boot or start failure exits 5.
//!
//! The CLI keeps its own wall-clock deadline (the plugin owns the real one):
//! when `--timeout` plus a grace window passes without a terminal record, the
//! job is cancelled and the run exits 3 — a vanished Studio can never hang the
//! command forever.
//!
//! `playtest capture` accepts only a PlayClient context and reuses the edit
//! capture pipeline verbatim: the same `capture_read` chunk pump, the same
//! SHA-256 verification, and the same encode-then-decode-back PNG proof.

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};
use colored::Colorize;
use serde_json::{json, Value};
use std::{
	fs,
	path::PathBuf,
	process, thread,
	time::{Duration, Instant},
};

use crate::{
	cli::client::{human_value, kind_of, print_json, print_line, Client, Envelope, Targeting},
	cli::live::capture,
	wsync_info, wsync_warn,
};

/// How long the run poll loop sleeps when a poll returned no records
const RUN_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Transport retries before a poll failure aborts the run
const RUN_POLL_RETRIES: u32 = 3;

/// Grace the CLI-side deadline gets over `--timeout`, so the plugin's own
/// timeout record (which names the job) wins the race against the local one
const RUN_DEADLINE_GRACE: Duration = Duration::from_secs(10);

/// `playtest_run_start` boots a Studio playtest, so it gets head-room over
/// the 5 s op default
const RUN_START_TIMEOUT_MS: u64 = 15_000;

/// A stop walks the whole teardown ladder — wait for a booting PlayServer, ask
/// it to end the test, then confirm Studio's DataModels drained — so it needs
/// far more than the 5 s surface default. Must stay above the plugin's own
/// `stopBootWaitSec` + `stopDrainSec` budget, or the client gives up while the
/// plugin is still tearing down.
const STOP_TIMEOUT_MS: u64 = 45_000;

/// The input contract's action cap (playtest.json)
const INPUT_MAX_ACTIONS: usize = 200;

/// Run playscript-owned playtest sessions or control lower-level Studio
/// playtests and runtime agents
#[derive(Parser)]
pub struct Playtest {
	#[command(subcommand)]
	command: PlaytestCommand,
}

#[derive(Subcommand)]
enum PlaytestCommand {
	/// Run one playscript-owned playtest session to completion
	Run(PlaytestRun),
	/// Start Play, Run, or a local multiplayer test as an asynchronous job
	Start(PlaytestStart),
	/// Print the active playtest job and its connected contexts
	Status(PlaytestStatus),
	/// List PlayServer and PlayClient runtime contexts
	Contexts(PlaytestContexts),
	/// Wait until the requested runtime contexts are connected
	Wait(PlaytestWait),
	/// Execute Luau in a PlayServer or PlayClient context
	Exec(PlaytestExec),
	/// Read output from a PlayServer or PlayClient context
	Logs(PlaytestLogs),
	/// Inspect resolved GUI geometry and text in a PlayClient context
	Ui(PlaytestUi),
	/// Send a JSON action sequence through PlayClient virtual input
	Input(PlaytestInput),
	/// Capture a PlayClient viewport as a locally verified PNG
	Capture(PlaytestCapture),
	/// Stop the active playtest
	Stop(PlaytestStop),
	/// Send an advanced runtime operation directly to a playtest context
	Request(PlaytestRequest),
}

impl Playtest {
	pub fn main(self) -> Result<()> {
		match self.command {
			PlaytestCommand::Run(command) => command.main(),
			PlaytestCommand::Start(command) => command.main(),
			PlaytestCommand::Status(command) => command.main(),
			PlaytestCommand::Contexts(command) => command.main(),
			PlaytestCommand::Wait(command) => command.main(),
			PlaytestCommand::Exec(command) => command.main(),
			PlaytestCommand::Logs(command) => command.main(),
			PlaytestCommand::Ui(command) => command.main(),
			PlaytestCommand::Input(command) => command.main(),
			PlaytestCommand::Capture(command) => command.main(),
			PlaytestCommand::Stop(command) => command.main(),
			PlaytestCommand::Request(command) => command.main(),
		}
	}
}

// ---------------------------------------------------------------------------
// Shared enums and validation
// ---------------------------------------------------------------------------

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
enum Mode {
	Play,
	Run,
	Multiplayer,
}

impl Mode {
	fn as_str(self) -> &'static str {
		match self {
			Mode::Play => "play",
			Mode::Run => "run",
			Mode::Multiplayer => "multiplayer",
		}
	}
}

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
enum Identity {
	Game,
	Plugin,
}

impl Identity {
	fn as_str(self) -> &'static str {
		match self {
			Identity::Game => "game",
			Identity::Plugin => "plugin",
		}
	}
}

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, ValueEnum)]
enum RunLogs {
	Off,
	Error,
	Warn,
	Info,
}

impl RunLogs {
	fn as_str(self) -> &'static str {
		match self {
			RunLogs::Off => "off",
			RunLogs::Error => "error",
			RunLogs::Warn => "warn",
			RunLogs::Info => "info",
		}
	}

	/// Whether a record at `level` passes this threshold
	fn admits(self, level: &str) -> bool {
		let rank = match level {
			"error" => RunLogs::Error,
			"warn" => RunLogs::Warn,
			_ => RunLogs::Info,
		};

		self != RunLogs::Off && rank <= self
	}
}

/// `server` or `client:N` (N ≥ 1) — validated before any network work
pub(crate) fn check_context(context: &str) -> Result<()> {
	if context == "server" {
		return Ok(());
	}

	if let Some(index) = context.strip_prefix("client:") {
		if index.parse::<u8>().map(|index| index >= 1).unwrap_or(false) {
			return Ok(());
		}
	}

	bail!("--context must be `server` or `client:N` (N starting at 1), not `{context}`")
}

fn check_client_context(context: &str) -> Result<()> {
	check_context(context)?;

	if !context.starts_with("client:") {
		bail!("`playtest capture` accepts only a PlayClient context (`client:N`), not `{context}`");
	}

	Ok(())
}

// ---------------------------------------------------------------------------
// playtest run — the foreground playscript session
// ---------------------------------------------------------------------------

#[derive(Parser)]
struct PlaytestRun {
	#[command(flatten)]
	targeting: Targeting,

	/// Main playscript file; its first completion ends the session
	#[arg(long, value_name = "FILE")]
	script: PathBuf,

	/// Runtime context for the main playscript
	#[arg(long, value_name = "CTX", default_value = "server")]
	context: String,

	/// Companion playscript run once in every PlayClient as it becomes ready
	#[arg(long = "client-script", value_name = "FILE")]
	client_script: Option<PathBuf>,

	/// Playtest kind to start
	#[arg(long, value_enum, default_value = "play")]
	mode: Mode,

	/// Number of PlayClients in multiplayer mode
	#[arg(long, default_value = "1", value_parser = clap::value_parser!(u8).range(1..=8))]
	players: u8,

	/// JSON value exposed to playscripts as `playtest.args`
	#[arg(long, value_name = "JSON", default_value = "{}")]
	args: String,

	/// Hard wall-clock deadline for the complete session, in seconds
	#[arg(long, value_name = "SECONDS", default_value = "600", value_parser = clap::value_parser!(u64).range(1..=3600))]
	timeout: u64,

	/// game runs through a temporary Script/LocalScript; plugin uses the
	/// plugin sandbox
	#[arg(long, value_enum, default_value = "game")]
	identity: Identity,

	/// Interleave Studio output from the runtime contexts at this level
	#[arg(long, value_enum, default_value = "off")]
	logs: RunLogs,

	/// Skip the automatic stop and print the job id for later use
	#[arg(long = "keep-open")]
	keep_open: bool,

	/// Suppress progress records; print only the terminal result
	///
	/// Counted (not boolean) on purpose: the id `quiet` shadows the global
	/// `-q/--quiet` logging flag inside this subcommand, and the flattened
	/// `Verbosity` reader downcasts whatever value the id carries — a bool
	/// here panics it. Sharing the count shape keeps both meanings coherent:
	/// `--quiet` suppresses progress records *and* the log chatter around them
	#[arg(long, action = clap::ArgAction::Count)]
	quiet: u8,

	/// Live NDJSON pass-through of the job records, one object per line
	#[arg(long)]
	raw: bool,
}

/// Everything `playtest_run_start` needs, validated and file contents already
/// read. Shared with the workflow `playtest` step so the two paths cannot
/// drift
pub(crate) struct RunSpec {
	pub script: String,
	pub client_script: Option<String>,
	pub context: String,
	pub mode: String,
	pub players: u8,
	pub args: Value,
	pub timeout_sec: u64,
	pub identity: String,
	pub logs: String,
}

impl RunSpec {
	pub(crate) fn start_args(&self) -> Value {
		let mut args = json!({
			"script": self.script,
			"context": self.context,
			"mode": self.mode,
			"players": self.players,
			"args": self.args,
			"timeoutSec": self.timeout_sec,
			"identity": self.identity,
			"logs": self.logs,
		});

		if let Some(client_script) = &self.client_script {
			args["clientScript"] = json!(client_script);
		}

		args
	}
}

/// How a run ended, before the terminal record is rendered
pub(crate) struct RunTerminal {
	pub exit_code: i32,
	/// The terminal record — plugin-authored when one arrived, CLI-synthesized
	/// (timeout, aborted transport, missing terminal) otherwise
	pub record: Value,
}

pub(crate) enum RunEnd {
	/// The job reported completion through its record stream
	Done(RunTerminal),
	/// The CLI-side deadline expired without a terminal record
	DeadlineExpired,
}

impl PlaytestRun {
	fn main(self) -> Result<()> {
		// Everything below `Client::connect` may contact Studio; every parse,
		// range check, and file read happens first
		check_context(&self.context)?;

		let args: Value = serde_json::from_str(&self.args)
			.with_context(|| format!("--args must be valid JSON, not `{}`", self.args))?;

		let script = read_script(&self.script, "--script")?;
		let client_script = self
			.client_script
			.as_ref()
			.map(|path| read_script(path, "--client-script"))
			.transpose()?;

		if self.players > 1 && self.mode != Mode::Multiplayer {
			wsync_warn!("--players only takes effect with --mode multiplayer");
		}

		let spec = RunSpec {
			script,
			client_script,
			context: self.context.clone(),
			mode: self.mode.as_str().to_owned(),
			players: self.players,
			args,
			timeout_sec: self.timeout,
			identity: self.identity.as_str().to_owned(),
			logs: self.logs.as_str().to_owned(),
		};

		let client = Client::connect(&self.targeting)?;
		let started = Instant::now();

		// A start the plugin refuses is the boot-failure exit (5), not a
		// generic CLI error: the caller asked for a run and gets a run outcome
		let start = client.request_with_timeout("playtest_run_start", spec.start_args(), RUN_START_TIMEOUT_MS)?;

		if !start.ok {
			let record = boot_failure_record(&start, started.elapsed());

			self.print_terminal(&record);

			process::exit(5);
		}

		let Some(job_id) = start.value.get("jobId").and_then(Value::as_str).map(str::to_owned) else {
			let record = json!({
				"type": "result",
				"ok": false,
				"kind": "bootFailure",
				"exitCode": 5,
				"error": "playtest_run_start answered without a jobId",
				"jobStatus": "unavailable",
				"elapsed": started.elapsed().as_secs_f64(),
			});

			self.print_terminal(&record);

			process::exit(5);
		};

		let deadline = started + Duration::from_secs(self.timeout) + RUN_DEADLINE_GRACE;

		let polled = poll_run(&client, &job_id, deadline, |record| {
			self.print_progress(record);

			Ok(())
		});

		let terminal = match polled {
			Ok(RunEnd::Done(terminal)) => terminal,
			Ok(RunEnd::DeadlineExpired) => {
				// The plugin's own timeout record never arrived, so the CLI
				// cancels and authors the terminal itself
				client.request("playtest_run_cancel", json!({ "jobId": job_id })).ok();

				RunTerminal {
					exit_code: 3,
					record: json!({
						"type": "result",
						"ok": false,
						"kind": "timeout",
						"exitCode": 3,
						"error": format!("no terminal record within --timeout {}s (plus grace)", self.timeout),
						"jobStatus": fetch_job_status(&client),
						"elapsed": started.elapsed().as_secs_f64(),
					}),
				}
			}
			Err(err) => {
				// The poll transport died and stayed dead: the job's fate is
				// unknown, which is the aborted exit, never a fake success
				client.request("playtest_run_cancel", json!({ "jobId": job_id })).ok();

				RunTerminal {
					exit_code: 4,
					record: json!({
						"type": "aborted",
						"reason": format!("lost the run stream: {err:#}"),
						"jobStatus": fetch_job_status(&client),
						"exitCode": 4,
						"elapsed": started.elapsed().as_secs_f64(),
					}),
				}
			}
		};

		if self.keep_open {
			if !self.raw {
				print_line(&format!("playtest left running (job {job_id})"));
			}
		} else {
			// Best-effort: a failed stop is reported but never rewrites the
			// run's own outcome
			match client.request("playtest_stop", json!({})) {
				Ok(envelope) if envelope.ok => {}
				Ok(envelope) => wsync_warn!(
					"Failed to stop the playtest after the run: {}",
					envelope.error_message()
				),
				Err(err) => wsync_warn!("Failed to stop the playtest after the run: {err}"),
			}
		}

		self.print_terminal(&terminal.record);

		if terminal.exit_code == 0 {
			Ok(())
		} else {
			process::exit(terminal.exit_code);
		}
	}

	/// One progress record: raw prints it verbatim, human renders it, quiet
	/// suppresses it either way
	fn print_progress(&self, record: &Value) {
		if self.quiet > 0 {
			return;
		}

		if self.raw {
			print_line(&record.to_string());

			return;
		}

		match record.get("type").and_then(Value::as_str).unwrap_or_default() {
			"started" => print_line(&format!(
				"▶ started job {} (mode {})",
				record.get("jobId").and_then(Value::as_str).unwrap_or("?").bold(),
				record.get("mode").and_then(Value::as_str).unwrap_or("?"),
			)),
			"ready" => print_line(&format!(
				"● context {} ready",
				record.get("context").and_then(Value::as_str).unwrap_or("?").bold()
			)),
			"event" => print_line(&format!("event {}", record.get("data").cloned().unwrap_or(Value::Null))),
			"log" => {
				let level = record.get("level").and_then(Value::as_str).unwrap_or("info");

				if self.logs.admits(level) {
					print_line(&format!(
						"[{level}] {} {}",
						record.get("context").and_then(Value::as_str).unwrap_or(""),
						record.get("message").and_then(Value::as_str).unwrap_or_default(),
					));
				}
			}
			"clientResult" => print_line(&format!(
				"client {} returned {}",
				record.get("context").and_then(Value::as_str).unwrap_or("?"),
				human_value(record.get("value").unwrap_or(&Value::Null)),
			)),
			"dropped" => wsync_warn!(
				"{} record(s) dropped by source rate limiting",
				record.get("count").and_then(Value::as_u64).unwrap_or(0)
			),
			other => print_line(&format!("{other} {record}")),
		}
	}

	/// The terminal record: always printed, even under `--quiet`
	fn print_terminal(&self, record: &Value) {
		if self.raw {
			print_line(&record.to_string());

			return;
		}

		let elapsed = record.get("elapsed").and_then(Value::as_f64).unwrap_or(0.0);

		if record.get("type").and_then(Value::as_str) == Some("aborted") {
			print_line(&format!(
				"✖ aborted ({elapsed:.1}s): {} (job: {})",
				record
					.get("reason")
					.and_then(Value::as_str)
					.unwrap_or("job ended externally"),
				record.get("jobStatus").and_then(Value::as_str).unwrap_or("unknown"),
			));

			return;
		}

		if record.get("ok").and_then(Value::as_bool) == Some(true) {
			print_line(&format!("✔ result ({elapsed:.1}s):"));
			print_line(&serde_json::to_string_pretty(record.get("value").unwrap_or(&Value::Null)).unwrap_or_default());

			return;
		}

		print_line(&format!(
			"✖ {} ({elapsed:.1}s): {}{}",
			record.get("kind").and_then(Value::as_str).unwrap_or("failure"),
			record
				.get("error")
				.and_then(Value::as_str)
				.unwrap_or("playtest run failed"),
			match record.get("jobStatus").and_then(Value::as_str) {
				Some(status) => format!(" (job: {status})"),
				None => String::new(),
			},
		));

		if let Some(traceback) = record.get("traceback").and_then(Value::as_str) {
			print_line(traceback);
		}
	}
}

fn read_script(path: &PathBuf, flag: &str) -> Result<String> {
	fs::read_to_string(path).with_context(|| format!("Failed to read the {flag} file {}", path.display()))
}

fn boot_failure_record(envelope: &Envelope, elapsed: Duration) -> Value {
	json!({
		"type": "result",
		"ok": false,
		"kind": "bootFailure",
		"exitCode": 5,
		"error": format!("{} [{}]", envelope.error_message(), envelope.error_code()),
		"jobStatus": "unavailable",
		"elapsed": elapsed.as_secs_f64(),
	})
}

/// The final observed job status for a failure report — best-effort, so a
/// dead daemon degrades to `unavailable` instead of masking the real error
fn fetch_job_status(client: &Client) -> String {
	match client.request("playtest_status", json!({})) {
		Ok(envelope) if envelope.ok => envelope
			.value
			.get("jobStatus")
			.or_else(|| envelope.value.get("status"))
			.and_then(Value::as_str)
			.unwrap_or("unknown")
			.to_owned(),
		_ => "unavailable".to_owned(),
	}
}

/// Pages `playtest_run_poll` forward until the job reports `done`, feeding
/// every non-terminal record to `on_record` in arrival order. Shared by
/// `playtest run` and the workflow `playtest` step.
///
/// `Err` means the poll transport failed and stayed failed across the retry
/// budget — the record stream is lost, not merely quiet.
pub(crate) fn poll_run(
	client: &Client,
	job_id: &str,
	deadline: Instant,
	mut on_record: impl FnMut(&Value) -> Result<()>,
) -> Result<RunEnd> {
	let mut since_seq: u64 = 0;
	let mut retries = 0;
	let mut terminal: Option<Value> = None;

	while Instant::now() < deadline {
		let envelope = match client.request("playtest_run_poll", json!({ "jobId": job_id, "sinceSeq": since_seq })) {
			Ok(envelope) if envelope.ok => {
				retries = 0;
				envelope
			}
			Ok(envelope) => {
				retries += 1;

				if retries > RUN_POLL_RETRIES {
					bail!("{} [{}]", envelope.error_message(), envelope.error_code());
				}

				thread::sleep(RUN_POLL_INTERVAL);
				continue;
			}
			Err(err) => {
				retries += 1;

				if retries > RUN_POLL_RETRIES {
					return Err(err);
				}

				thread::sleep(RUN_POLL_INTERVAL);
				continue;
			}
		};

		let value = envelope.value;
		let empty = Vec::new();
		let records = value.get("records").and_then(Value::as_array).unwrap_or(&empty);

		for record in records {
			if is_terminal_record(record) {
				// Rendering the terminal is the caller's job, exactly once —
				// a stream that carries it in `records` cannot duplicate it
				terminal = Some(record.clone());
			} else {
				on_record(record)?;
			}
		}

		if let Some(next) = value.get("nextSeq").and_then(Value::as_u64) {
			since_seq = next;
		}

		if value.get("done").and_then(Value::as_bool) == Some(true) {
			let record = terminal.unwrap_or_else(|| {
				json!({
					"type": "aborted",
					"reason": "the job ended without a terminal record",
					"jobStatus": "unknown",
					"exitCode": 4,
				})
			});

			// The poll's own `exit` wins when it is a valid code; the record
			// mapping is the fallback contract
			let exit_code = value
				.get("exit")
				.and_then(Value::as_i64)
				.filter(|code| (0..=5).contains(code))
				.map(|code| code as i32)
				.unwrap_or_else(|| record_exit_code(&record));

			return Ok(RunEnd::Done(RunTerminal { exit_code, record }));
		}

		if records.is_empty() {
			thread::sleep(RUN_POLL_INTERVAL);
		}
	}

	Ok(RunEnd::DeadlineExpired)
}

pub(crate) fn is_terminal_record(record: &Value) -> bool {
	matches!(
		record.get("type").and_then(Value::as_str),
		Some("result") | Some("aborted")
	)
}

/// The record-driven exit mapping (playtest.json): 0 success, 2 failure,
/// 3 timeout, 4 aborted, 5 boot failure
pub(crate) fn record_exit_code(record: &Value) -> i32 {
	if record.get("type").and_then(Value::as_str) == Some("aborted") {
		return 4;
	}

	if let Some(code) = record.get("exitCode").and_then(Value::as_i64) {
		if (0..=5).contains(&code) {
			return code as i32;
		}
	}

	match record.get("kind").and_then(Value::as_str) {
		Some("success") => 0,
		Some("timeout") => 3,
		Some("aborted") => 4,
		Some("bootFailure") => 5,
		Some(_) => 2,
		None => {
			if record.get("ok").and_then(Value::as_bool) == Some(true) {
				0
			} else {
				2
			}
		}
	}
}

// ---------------------------------------------------------------------------
// Low-level controls
// ---------------------------------------------------------------------------

/// The flags every low-level playtest command shares
#[derive(Args)]
struct Common {
	#[command(flatten)]
	targeting: Targeting,

	/// Print machine-readable JSON
	#[arg(long)]
	raw: bool,
}

impl Common {
	/// One op, one value, the shared `--raw` convention: the op's value as one
	/// JSON line, or a compact human rendering
	fn run_op(&self, op: &str, args: Value, timeout_ms: Option<u64>) -> Result<Value> {
		let client = Client::connect(&self.targeting)?;

		let envelope = match timeout_ms {
			Some(timeout_ms) => client.request_with_timeout(op, args, timeout_ms)?,
			None => client.request(op, args)?,
		};

		envelope.into_value(self.raw)
	}
}

#[derive(Parser)]
struct PlaytestStart {
	#[command(flatten)]
	common: Common,

	/// Playtest kind to start
	#[arg(long, value_enum, default_value = "play")]
	mode: Mode,

	/// Number of PlayClients in multiplayer mode
	#[arg(long, default_value = "1", value_parser = clap::value_parser!(u8).range(1..=8))]
	players: u8,

	/// Wait for the runtime contexts after starting
	#[arg(long)]
	wait: bool,

	/// Seconds to wait for contexts with --wait
	#[arg(long, value_name = "SECONDS", default_value = "45", value_parser = clap::value_parser!(u64).range(1..=600))]
	timeout: u64,

	/// Force-clear any active generation first (recovers a wedged playtest).
	///
	/// If a previous test left the job stuck active — start reports one is
	/// already running when none is — this force-stops it before starting, so
	/// you no longer need to restart Studio. It also aborts a genuinely running
	/// playtest, so only pass it when you mean to replace whatever is active.
	#[arg(long)]
	force: bool,
}

/// Studio's own refusal when its internal test state is wedged. The engine
/// keeps a "test in progress" flag that nothing reachable from the plugin can
/// clear: `LeaveTest` only works from a running test's *client* DataModel and
/// `EndTest` only from its *server* one, and by the time the flag is stuck both
/// DataModels are gone. Restarting Studio is the only recovery.
const STUDIO_WEDGE_MARKER: &str = "previous one is still in progress";

const STUDIO_WEDGE_HELP: &str = "Studio's own test state is wedged — it still believes a test is running when none is. \
WSync cannot clear this (LeaveTest/EndTest only work from inside a running test's DataModel, and it is already gone), \
and `playtest stop --force` only clears WSync's own job record. Restart Roblox Studio to recover.";

/// Attach the wedge explanation when a failure carries Studio's marker, so the
/// opaque engine refusal always arrives with its only real remedy.
fn explain_studio_wedge(detail: &str) -> Option<String> {
	detail
		.contains(STUDIO_WEDGE_MARKER)
		.then(|| format!("{detail}\n\n{STUDIO_WEDGE_HELP}"))
}

impl PlaytestStart {
	/// `playtest_start` answers as soon as the launch is dispatched, but the
	/// launch itself can die moments later — most often Studio refusing outright
	/// because its test state is wedged. Without this check `start` cheerfully
	/// reports success for a generation that is already dead.
	fn settle_check(client: &Client, raw: bool) -> Result<()> {
		std::thread::sleep(std::time::Duration::from_millis(1200));

		let Ok(status) = client.request("playtest_status", json!({})) else {
			return Ok(());
		};

		let Ok(status) = status.into_value(raw) else {
			return Ok(());
		};

		if status.get("jobStatus").and_then(Value::as_str) != Some("failed") {
			return Ok(());
		}

		let detail = status.get("error").and_then(Value::as_str).unwrap_or_default();

		match explain_studio_wedge(detail) {
			Some(explained) => bail!("{explained}"),
			None if !detail.is_empty() => bail!("the playtest failed to start: {detail}"),
			None => Ok(()),
		}
	}

	fn main(self) -> Result<()> {
		let client = Client::connect(&self.common.targeting)?;

		if self.force {
			// A forced stop always drives the active job terminal (even one
			// Studio can't confirm leaving), so this unblocks the start below.
			// Ignore its outcome — "nothing to stop" and "stopped but Studio
			// couldn't confirm" both leave the registry clear, which is all we
			// need here.
			let _ = client.request_with_timeout("playtest_stop", json!({ "force": true }), RUN_START_TIMEOUT_MS);
		}

		let started = client
			.request_with_timeout(
				"playtest_start",
				json!({ "mode": self.mode.as_str(), "players": self.players }),
				RUN_START_TIMEOUT_MS,
			)?
			.into_value(self.common.raw)
			.map_err(|error| {
				if error.to_string().contains("already active") {
					// Both readings are possible here and they want opposite
					// actions, so name them rather than pushing --force at
					// someone whose playtest is simply still running
					error.context(
						"stop the running playtest first (`wsync playtest stop`); \
						 if nothing is actually running, that generation is wedged — \
						 `wsync playtest stop --force` clears it without restarting Studio",
					)
				} else {
					error
				}
			})?;

		let waited = if self.wait {
			// Multiplayer waits for the server plus every client; play for
			// server and one client; run for the server context alone
			let minimum = match self.mode {
				Mode::Multiplayer => u64::from(self.players) + 1,
				Mode::Play => 2,
				Mode::Run => 1,
			};

			let outcome = client
				.request_with_timeout(
					"playtest_wait",
					json!({ "minimum": minimum, "timeoutMs": self.timeout * 1000 }),
					self.timeout * 1000,
				)
				.and_then(|envelope| envelope.into_value(self.common.raw));

			match outcome {
				Ok(value) => Some(value),
				Err(error) => {
					// "contexts never connected" is the symptom; the launch's own
					// failure is the cause, so report that (and the wedge remedy)
					// in preference to the timeout wording
					Self::settle_check(&client, self.common.raw)?;

					return Err(error);
				}
			}
		} else {
			Self::settle_check(&client, self.common.raw)?;

			None
		};

		if self.common.raw {
			let mut record = json!({ "ok": true });

			merge_into(&mut record, &started);

			if let Some(waited) = &waited {
				record["wait"] = waited.clone();
			}

			print_json(&record);

			return Ok(());
		}

		wsync_info!(
			"Started a {} playtest{}",
			self.mode.as_str().bold(),
			match &waited {
				Some(value) => format!(
					" — {} context(s) connected",
					value
						.get("contexts")
						.and_then(Value::as_array)
						.map(Vec::len)
						.unwrap_or_default()
				),
				None => String::new(),
			}
		);

		Ok(())
	}
}

fn merge_into(record: &mut Value, value: &Value) {
	if let (Some(record), Some(fields)) = (record.as_object_mut(), value.as_object()) {
		for (key, field) in fields {
			if key != "ok" {
				record.insert(key.clone(), field.clone());
			}
		}
	}
}

#[derive(Parser)]
struct PlaytestStatus {
	#[command(flatten)]
	common: Common,
}

impl PlaytestStatus {
	fn main(self) -> Result<()> {
		let value = self.common.run_op("playtest_status", json!({}), None)?;

		if self.common.raw {
			print_json(&value);

			return Ok(());
		}

		let active = value.get("active").and_then(Value::as_bool) == Some(true);

		if !active {
			wsync_info!("No active playtest");

			return Ok(());
		}

		println!(
			"Playtest  {} ({})",
			value.get("mode").and_then(Value::as_str).unwrap_or("unknown").bold(),
			value
				.get("jobStatus")
				.or_else(|| value.get("status"))
				.and_then(Value::as_str)
				.unwrap_or("running"),
		);

		print_contexts(&value);

		Ok(())
	}
}

#[derive(Parser)]
struct PlaytestContexts {
	#[command(flatten)]
	common: Common,
}

impl PlaytestContexts {
	fn main(self) -> Result<()> {
		let value = self.common.run_op("playtest_contexts", json!({}), None)?;

		if self.common.raw {
			print_json(&value);

			return Ok(());
		}

		print_contexts(&value);

		Ok(())
	}
}

fn print_contexts(value: &Value) {
	let empty = Vec::new();
	let contexts = value.get("contexts").and_then(Value::as_array).unwrap_or(&empty);

	for context in contexts {
		println!(
			"  {:<12} {}",
			context.get("context").and_then(Value::as_str).unwrap_or("?"),
			context.get("status").and_then(Value::as_str).unwrap_or("connected"),
		);
	}

	println!("\n{} context(s)", contexts.len());
}

#[derive(Parser)]
struct PlaytestWait {
	#[command(flatten)]
	common: Common,

	/// Wait for this specific context (e.g. `client:2`) instead of a count
	#[arg(long, value_name = "CTX")]
	context: Option<String>,

	/// Minimum number of connected runtime contexts
	#[arg(long, default_value = "1", value_parser = clap::value_parser!(u8).range(1..=9))]
	minimum: u8,

	/// Seconds to wait before giving up
	#[arg(long, value_name = "SECONDS", default_value = "45", value_parser = clap::value_parser!(u64).range(1..=600))]
	timeout: u64,
}

impl PlaytestWait {
	fn main(self) -> Result<()> {
		if let Some(context) = &self.context {
			check_context(context)?;
		}

		let mut args = json!({ "minimum": self.minimum, "timeoutMs": self.timeout * 1000 });

		if let Some(context) = &self.context {
			args["context"] = json!(context);
		}

		let value = self.common.run_op("playtest_wait", args, Some(self.timeout * 1000))?;

		if self.common.raw {
			print_json(&value);

			return Ok(());
		}

		wsync_info!(
			"{} context(s) connected",
			value
				.get("contexts")
				.and_then(Value::as_array)
				.map(Vec::len)
				.unwrap_or_default()
				.to_string()
				.bold()
		);

		Ok(())
	}
}

#[derive(Parser)]
struct PlaytestExec {
	#[command(flatten)]
	common: Common,

	/// Runtime context from `playtest contexts`, e.g. `server` or `client:1`
	#[arg(long, value_name = "CTX")]
	context: String,

	/// Luau source; use `return …` for a value back
	#[arg(long, value_name = "LUAU", conflicts_with = "source_file")]
	source: Option<String>,

	/// Read the Luau source from a file
	#[arg(long = "source-file", value_name = "FILE", conflicts_with = "source")]
	source_file: Option<PathBuf>,

	/// game runs through a temporary Script/LocalScript; plugin uses the
	/// plugin sandbox
	#[arg(long, value_enum, default_value = "game")]
	identity: Identity,

	/// Seconds the execution may run
	#[arg(long, value_name = "SECONDS", default_value = "15", value_parser = clap::value_parser!(u64).range(1..=600))]
	timeout: u64,
}

impl PlaytestExec {
	fn main(self) -> Result<()> {
		check_context(&self.context)?;

		let source = match (&self.source, &self.source_file) {
			(Some(source), None) => source.clone(),
			(None, Some(path)) => read_script(path, "--source-file")?,
			_ => bail!("`playtest exec` needs exactly one of --source or --source-file"),
		};

		let args = json!({
			"context": self.context,
			"source": source,
			"identity": self.identity.as_str(),
			"timeoutMs": self.timeout * 1000,
		});

		let value = self.common.run_op("playtest_exec", args, Some(self.timeout * 1000))?;

		if self.common.raw {
			print_json(&value);

			return Ok(());
		}

		let empty = Vec::new();
		let results = value.get("results").and_then(Value::as_array).unwrap_or(&empty);

		for (index, result) in results.iter().enumerate() {
			println!("{:>3}  {}", index + 1, human_value(result));
		}

		wsync_info!(
			"exec in {} returned {} value(s)",
			self.context.bold(),
			value
				.get("count")
				.and_then(Value::as_u64)
				.unwrap_or(results.len() as u64)
		);

		Ok(())
	}
}

#[derive(Parser)]
struct PlaytestLogs {
	#[command(flatten)]
	common: Common,

	/// Runtime context to read from
	#[arg(long, value_name = "CTX")]
	context: String,

	/// Only entries with a sequence number above this
	#[arg(long = "since-seq", value_name = "SEQ")]
	since_seq: Option<u64>,

	/// Maximum entries to return
	#[arg(long, value_name = "N")]
	limit: Option<u32>,
}

impl PlaytestLogs {
	fn main(self) -> Result<()> {
		check_context(&self.context)?;

		let mut args = json!({ "context": self.context });

		if let Some(since_seq) = self.since_seq {
			args["sinceSeq"] = json!(since_seq);
		}

		if let Some(limit) = self.limit {
			args["limit"] = json!(limit);
		}

		let value = self.common.run_op("playtest_logs", args, None)?;

		if self.common.raw {
			print_json(&value);

			return Ok(());
		}

		let empty = Vec::new();
		let entries = value.get("entries").and_then(Value::as_array).unwrap_or(&empty);

		for entry in entries {
			let level = entry.get("level").and_then(Value::as_str).unwrap_or("info");
			let painted = match level {
				"error" => "error".red(),
				"warn" => " warn".yellow(),
				_ => " info".normal(),
			};

			println!(
				"{painted} {}",
				entry.get("message").and_then(Value::as_str).unwrap_or_default()
			);
		}

		println!("\n{} entr(ies) from {}", entries.len(), self.context);

		Ok(())
	}
}

#[derive(Parser)]
struct PlaytestUi {
	#[command(flatten)]
	common: Common,

	/// PlayClient context to inspect
	#[arg(long, value_name = "CTX")]
	context: String,

	/// Restrict to this GUI subtree path
	#[arg(long, value_name = "PATH")]
	root: Option<String>,

	/// Only elements of this class (e.g. `TextButton`)
	#[arg(long, value_name = "CLASS")]
	class: Option<String>,

	/// Only elements with this name
	#[arg(long, value_name = "NAME")]
	name: Option<String>,

	/// Maximum elements to return
	#[arg(long, value_name = "N", default_value = "1000")]
	limit: u32,
}

impl PlaytestUi {
	fn main(self) -> Result<()> {
		check_context(&self.context)?;

		let mut args = json!({ "context": self.context, "limit": self.limit });

		for (key, field) in [("root", &self.root), ("class", &self.class), ("name", &self.name)] {
			if let Some(field) = field {
				args[key] = json!(field);
			}
		}

		let value = self.common.run_op("playtest_ui", args, None)?;

		if self.common.raw {
			print_json(&value);

			return Ok(());
		}

		let empty = Vec::new();
		let elements = value.get("elements").and_then(Value::as_array).unwrap_or(&empty);

		for element in elements {
			println!(
				"{:<14} {:<40} {}",
				element.get("class").and_then(Value::as_str).unwrap_or(""),
				element.get("path").and_then(Value::as_str).unwrap_or(""),
				element.get("text").and_then(Value::as_str).unwrap_or(""),
			);
		}

		println!("\n{} element(s)", elements.len());

		Ok(())
	}
}

#[derive(Parser)]
struct PlaytestInput {
	#[command(flatten)]
	common: Common,

	/// PlayClient context to drive
	#[arg(long, value_name = "CTX")]
	context: String,

	/// JSON array of input actions
	#[arg(long, value_name = "JSON-ARRAY", conflicts_with = "file")]
	actions: Option<String>,

	/// Read the action array from a JSON file
	#[arg(long, value_name = "FILE", conflicts_with = "actions")]
	file: Option<PathBuf>,

	/// Seconds the action sequence may take (the contract caps this at 30)
	#[arg(long, value_name = "SECONDS", default_value = "10", value_parser = clap::value_parser!(u64).range(1..=30))]
	timeout: u64,
}

impl PlaytestInput {
	fn main(self) -> Result<()> {
		check_context(&self.context)?;

		let text = match (&self.actions, &self.file) {
			(Some(actions), None) => actions.clone(),
			(None, Some(path)) => {
				fs::read_to_string(path).with_context(|| format!("Failed to read the --file {}", path.display()))?
			}
			_ => bail!("`playtest input` needs exactly one of --actions or --file"),
		};

		let parsed: Value = serde_json::from_str(&text).context("input actions must be a JSON array of objects")?;

		let Some(actions) = parsed.as_array() else {
			bail!("input actions must be a JSON array, not {}", kind_of(&parsed));
		};

		if actions.is_empty() {
			bail!("the input action array is empty — nothing to send");
		}

		if actions.len() > INPUT_MAX_ACTIONS {
			bail!(
				"the input contract allows at most {INPUT_MAX_ACTIONS} actions per call, not {}",
				actions.len()
			);
		}

		for (index, action) in actions.iter().enumerate() {
			if !action.is_object() {
				bail!("input action {index} must be an object, not {}", kind_of(action));
			}
		}

		let args = json!({
			"context": self.context,
			"actions": parsed,
			"timeoutMs": self.timeout * 1000,
		});

		let value = self.common.run_op("playtest_input", args, Some(self.timeout * 1000))?;

		if self.common.raw {
			print_json(&value);

			return Ok(());
		}

		wsync_info!(
			"Sent {} input action(s) to {}",
			value
				.get("performed")
				.and_then(Value::as_u64)
				.unwrap_or(actions.len() as u64)
				.to_string()
				.bold(),
			self.context.bold()
		);

		Ok(())
	}
}

#[derive(Parser)]
struct PlaytestCapture {
	#[command(flatten)]
	common: Common,

	/// PlayClient context to capture
	#[arg(long, value_name = "CTX")]
	context: String,

	/// Output file
	#[arg(short, long, value_name = "FILE", default_value = "wsync-playtest-capture.png")]
	output: PathBuf,

	/// Seconds to wait for the screenshot
	#[arg(long, value_name = "SECONDS", default_value = "60", value_parser = clap::value_parser!(u64).range(1..=600))]
	timeout: u64,
}

impl PlaytestCapture {
	fn main(self) -> Result<()> {
		check_client_context(&self.context)?;

		let client = Client::connect(&self.common.targeting)?;

		// A running playtest cannot read a screenshot back into pixels
		// (`CreateEditableImageAsync` is blocked there), so we trigger a
		// CaptureScreenshot in the PlayClient and read the PNG the engine writes
		// to Roblox's tmp-capture-storage straight off disk — the only capture
		// path that works in play mode
		let summary = capture::capture_via_screenshot_file(
			&client,
			"playtest_screenshot",
			json!({ "context": self.context }),
			&self.output,
			self.timeout * 1000,
			self.common.raw,
		)?;

		if self.common.raw {
			let mut record = json!({ "ok": true });

			merge_into(&mut record, &summary);
			print_json(&record);

			return Ok(());
		}

		wsync_info!(
			"Captured {} {}x{} → {}",
			self.context.bold(),
			summary.get("width").and_then(Value::as_u64).unwrap_or(0),
			summary.get("height").and_then(Value::as_u64).unwrap_or(0),
			summary.get("path").and_then(Value::as_str).unwrap_or_default().bold(),
		);

		Ok(())
	}
}

#[derive(Parser)]
struct PlaytestStop {
	#[command(flatten)]
	common: Common,

	/// Force the stop even when Studio cannot confirm the test ended.
	///
	/// Recovers a wedged generation: a plain stop restores a job Studio won't
	/// leave back to `running`, but a forced stop drives it terminal so
	/// `playtest start` is no longer blocked — use this instead of restarting
	/// Studio when start reports a playtest is already active but none is.
	#[arg(long)]
	force: bool,
}

impl PlaytestStop {
	fn main(self) -> Result<()> {
		// Well above the 5 s surface default: a stop legitimately waits for a
		// still-booting PlayServer to connect, for the test to end, and then for
		// Studio's DataModels to drain. Timing out mid-ladder would abandon the
		// teardown halfway, which is exactly what wedges Studio's play control.
		let value = self
			.common
			.run_op("playtest_stop", json!({ "force": self.force }), Some(STOP_TIMEOUT_MS))?;

		if self.common.raw {
			print_json(&value);

			return Ok(());
		}

		// `alreadyEnded` means WSync's job record was already terminal, so
		// nothing was asked of Studio at all. Saying "Playtest stopped" there
		// reads as "Studio is clear now", which is exactly the false comfort
		// that hides a wedged engine — so report what actually happened.
		if value.get("alreadyEnded").and_then(Value::as_bool) == Some(true) {
			wsync_info!("No active playtest to stop — WSync's job record was already closed");

			return Ok(());
		}

		wsync_info!("Playtest stopped");

		Ok(())
	}
}

#[derive(Parser)]
struct PlaytestRequest {
	#[command(flatten)]
	common: Common,

	/// Runtime context to address
	#[arg(long, value_name = "CTX")]
	context: String,

	/// Runtime operation name
	#[arg(long, value_name = "OP")]
	op: String,

	/// Operation arguments as a JSON object
	#[arg(long, value_name = "JSON-OBJECT", default_value = "{}")]
	args: String,

	/// Seconds the operation may run
	#[arg(long, value_name = "SECONDS", default_value = "30", value_parser = clap::value_parser!(u64).range(1..=600))]
	timeout: u64,
}

impl PlaytestRequest {
	fn main(self) -> Result<()> {
		check_context(&self.context)?;

		let parsed: Value = serde_json::from_str(&self.args).context("--args must be a JSON object")?;

		if !parsed.is_object() {
			bail!("--args must be a JSON object, not {}", kind_of(&parsed));
		}

		let args = json!({
			"context": self.context,
			"op": self.op,
			"args": parsed,
			"timeoutMs": self.timeout * 1000,
		});

		let value = self
			.common
			.run_op("playtest_request", args, Some(self.timeout * 1000))?;

		print_json(&value);

		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn contexts_validate_before_any_network_work() {
		assert!(check_context("server").is_ok());
		assert!(check_context("client:1").is_ok());
		assert!(check_context("client:8").is_ok());

		assert!(check_context("client").is_err());
		assert!(check_context("client:0").is_err());
		assert!(check_context("client:x").is_err());
		assert!(check_context("Server").is_err());

		assert!(check_client_context("client:2").is_ok());
		assert!(check_client_context("server").is_err());
	}

	#[test]
	fn record_exit_codes_follow_the_contract() {
		assert_eq!(
			record_exit_code(&json!({ "type": "result", "ok": true, "kind": "success" })),
			0
		);
		assert_eq!(
			record_exit_code(&json!({ "type": "result", "ok": false, "kind": "failure" })),
			2
		);
		assert_eq!(record_exit_code(&json!({ "type": "result", "kind": "timeout" })), 3);
		assert_eq!(record_exit_code(&json!({ "type": "aborted" })), 4);
		assert_eq!(record_exit_code(&json!({ "type": "result", "kind": "bootFailure" })), 5);

		// An explicit in-range exitCode wins over the kind mapping
		assert_eq!(
			record_exit_code(&json!({ "type": "result", "kind": "failure", "exitCode": 3 })),
			3
		);

		// Out-of-range codes fall back to the kind
		assert_eq!(
			record_exit_code(&json!({ "type": "result", "kind": "failure", "exitCode": 99 })),
			2
		);
	}

	#[test]
	fn log_levels_gate_human_rendering() {
		assert!(!RunLogs::Off.admits("error"));
		assert!(RunLogs::Error.admits("error"));
		assert!(!RunLogs::Error.admits("warn"));
		assert!(RunLogs::Warn.admits("error"));
		assert!(RunLogs::Warn.admits("warn"));
		assert!(!RunLogs::Warn.admits("info"));
		assert!(RunLogs::Info.admits("info"));
	}
}
