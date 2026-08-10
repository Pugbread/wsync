//! `run` — schema-v1 JSON workflows over one daemon session (run.json;
//! Design §10.2, "Agent runtime").
//!
//! The module is split the same way Ro-Sync's workflow engine was: a typed
//! schema boundary that deserializes and validates the *entire* plan before a
//! socket is opened, and an executor that resolves references immediately
//! before each ordered step. Nothing executes on a workflow that merely
//! parsed — cross-step invariants (duplicate ids, forward/self references,
//! non-contiguous atomic transactions, forbidden-in-atomic operations) are
//! all validation failures, and validation is free.
//!
//! References: a string that is exactly `$stepId.value.path` resolves against
//! an earlier step's stored response while preserving its JSON type; `$$` is
//! the literal-`$` escape. A referenced value is inserted verbatim and never
//! re-scanned, so data returned by Studio can never become workflow syntax.
//! Because substitution can turn a valid placeholder into a forbidden value
//! (`property: "$x.value.prop"` resolving to `Parent`), every per-step check
//! runs again after resolution.
//!
//! Atomic transaction groups bracket their contiguous member steps in one
//! Studio change-history recording (`transaction_begin {name}` /
//! `transaction_finish {commit}`); any failure inside the group cancels the
//! recording and ends the run — `--keep-going` only relaxes failures outside
//! atomic groups.
//!
//! A successful run with an `idempotencyKey` is journalled under
//! `<workspace>/.wsync-workflows/<sha256(key)>.json`; re-running the same key
//! with the same workflow content replays the stored result with
//! `replayed: true` and performs no side effects. The same key with different
//! content is a collision and a hard error.
//!
//! Ops map onto the live command machinery's wire spellings (`set_attr`,
//! `add_tag`, …). The `playtest` step drives the `playtest run` internals
//! (start → poll → record-driven outcome → auto-stop); the `upload` step
//! fails cleanly — the Open Cloud surface is not available in this build.

use anyhow::{bail, Context as _, Result};
use clap::Parser;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::{
	collections::{BTreeMap, HashMap},
	fs,
	io::Write as _,
	path::{Path, PathBuf},
	thread,
	time::{Duration, Instant},
};

use crate::{
	cli::client::{print_json, Client, Envelope, Target, Targeting},
	cli::live::{capture, playtest},
	wsync_info, wsync_warn,
};

pub const WORKFLOW_VERSION: u32 = 1;
const MAX_WORKFLOW_STEPS: usize = 1024;
const MAX_STEP_TIMEOUT_MS: u64 = 600_000;
const DEFAULT_STEP_TIMEOUT_MS: u64 = 30_000;
const DEFAULT_WAIT_POLL_MS: u64 = 250;
const MIN_WAIT_POLL_MS: u64 = 50;
const CAPTURE_MAX_AXIS: u32 = 4096;
const CAPTURE_MAX_PIXELS: u64 = 16_777_216;
const PLAYTEST_DEADLINE_GRACE: Duration = Duration::from_secs(10);

/// Validate and execute a versioned JSON workflow against the live session
#[derive(Parser)]
pub struct Run {
	#[command(flatten)]
	targeting: Targeting,

	/// Workflow JSON file using schema version 1
	#[arg(long, value_name = "FILE")]
	file: PathBuf,

	/// Validate and print the normalized plan without executing anything
	#[arg(long = "dry-run")]
	dry_run: bool,

	/// Continue after a failed step outside atomic transaction groups
	#[arg(long = "keep-going")]
	keep_going: bool,

	/// Print the outcome as one JSON line
	#[arg(long)]
	raw: bool,
}

////////////////////////////////////////////////////////////////////////////////
// Schema
////////////////////////////////////////////////////////////////////////////////

/// Results keyed by step id. A reference like `$camera.value.path` walks the
/// JSON stored for step `camera`
type StepResults = BTreeMap<String, Value>;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Workflow {
	version: u32,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	name: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	idempotency_key: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	expected_mode: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	expected_place_id: Option<String>,
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	transactions: Vec<TransactionGroup>,
	steps: Vec<WorkflowStep>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct TransactionGroup {
	id: String,
	/// Atomic groups map to one Studio change-history recording; `false`
	/// declares a purely logical result group
	#[serde(default = "default_true")]
	atomic: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkflowStep {
	id: String,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	timeout_ms: Option<u64>,
	#[serde(default, skip_serializing_if = "is_false")]
	verify: bool,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	expected_class: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	etag: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	transaction: Option<String>,
	#[serde(flatten)]
	operation: StepOperation,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(
	tag = "op",
	rename_all = "kebab-case",
	rename_all_fields = "camelCase",
	deny_unknown_fields
)]
enum StepOperation {
	Get {
		path: String,
		#[serde(default, alias = "prop", skip_serializing_if = "Option::is_none")]
		property: Option<String>,
	},
	Set {
		path: String,
		#[serde(alias = "prop")]
		property: String,
		value: Value,
	},
	New {
		/// Parent instance path
		path: String,
		class: String,
		#[serde(default, skip_serializing_if = "Option::is_none")]
		name: Option<String>,
		#[serde(default, skip_serializing_if = "Option::is_none")]
		props: Option<Value>,
	},
	Rm {
		path: String,
	},
	Mv {
		from: String,
		to: String,
		#[serde(default, skip_serializing_if = "is_false")]
		force: bool,
	},
	AttrSet {
		path: String,
		name: String,
		value: Value,
	},
	AttrRm {
		path: String,
		name: String,
	},
	AttrLs {
		path: String,
	},
	TagAdd {
		path: String,
		tag: String,
	},
	TagRm {
		path: String,
		tag: String,
	},
	Assert {
		actual: Value,
		check: Assertion,
		#[serde(default, skip_serializing_if = "Option::is_none")]
		message: Option<String>,
	},
	Wait {
		path: String,
		#[serde(default, alias = "prop", skip_serializing_if = "Option::is_none")]
		property: Option<String>,
		check: Assertion,
		#[serde(default, skip_serializing_if = "Option::is_none")]
		poll_interval_ms: Option<u64>,
	},
	Eval {
		source: String,
	},
	Capture {
		#[serde(default, skip_serializing_if = "Option::is_none")]
		target: Option<String>,
		#[serde(default, skip_serializing_if = "Option::is_none")]
		path: Option<String>,
		/// PlayClient context — routes through `playtest_capture`
		#[serde(default, skip_serializing_if = "Option::is_none")]
		context: Option<String>,
		#[serde(default, skip_serializing_if = "Option::is_none")]
		size: Option<CaptureSize>,
		#[serde(default, skip_serializing_if = "Option::is_none")]
		region: Option<CaptureRegion>,
		#[serde(default, skip_serializing_if = "Option::is_none")]
		ui: Option<String>,
		#[serde(default, skip_serializing_if = "is_false")]
		skybox: bool,
		#[serde(default, skip_serializing_if = "is_false")]
		world: bool,
		#[serde(default, skip_serializing_if = "is_false")]
		framed: bool,
		#[serde(default, skip_serializing_if = "Option::is_none")]
		output: Option<String>,
	},
	/// Method calls stay outside atomic groups: the executor cannot know
	/// whether an arbitrary method yields or mutates
	Call {
		path: String,
		method: String,
		#[serde(default, skip_serializing_if = "Vec::is_empty")]
		args: Vec<Value>,
	},
	/// One complete playscript-owned playtest session (`playtest run`
	/// internals: start → poll → record-driven outcome → auto-stop)
	Playtest {
		#[serde(default, skip_serializing_if = "Option::is_none")]
		script: Option<String>,
		#[serde(default, skip_serializing_if = "Option::is_none")]
		script_file: Option<String>,
		#[serde(default, skip_serializing_if = "Option::is_none")]
		client_script: Option<String>,
		#[serde(default, skip_serializing_if = "Option::is_none")]
		client_script_file: Option<String>,
		#[serde(default = "default_context")]
		context: String,
		#[serde(default = "default_mode")]
		mode: String,
		#[serde(default = "default_players")]
		players: u8,
		#[serde(default, skip_serializing_if = "Value::is_null")]
		args: Value,
		#[serde(default = "default_playtest_timeout")]
		timeout_sec: u64,
		#[serde(default = "default_identity")]
		identity: String,
		#[serde(default = "default_logs")]
		logs: String,
	},
	Upload {
		paths: Vec<String>,
		#[serde(default, skip_serializing_if = "Option::is_none")]
		asset_type: Option<String>,
	},
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(
	tag = "op",
	rename_all = "kebab-case",
	rename_all_fields = "camelCase",
	deny_unknown_fields
)]
enum Assertion {
	Equals {
		expected: Value,
	},
	NotEquals {
		expected: Value,
	},
	Exists {
		#[serde(default = "default_true")]
		expected: bool,
	},
	Truthy {
		#[serde(default = "default_true")]
		expected: bool,
	},
	Contains {
		expected: Value,
	},
	GreaterThan {
		expected: f64,
	},
	GreaterThanOrEqual {
		expected: f64,
	},
	LessThan {
		expected: f64,
	},
	LessThanOrEqual {
		expected: f64,
	},
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CaptureSize {
	width: u32,
	height: u32,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CaptureRegion {
	x: u32,
	y: u32,
	width: u32,
	height: u32,
}

fn default_true() -> bool {
	true
}

fn is_false(value: &bool) -> bool {
	!value
}

fn default_context() -> String {
	"server".to_owned()
}

fn default_mode() -> String {
	"play".to_owned()
}

fn default_players() -> u8 {
	1
}

fn default_playtest_timeout() -> u64 {
	600
}

fn default_identity() -> String {
	"game".to_owned()
}

fn default_logs() -> String {
	"off".to_owned()
}

////////////////////////////////////////////////////////////////////////////////
// Validation
////////////////////////////////////////////////////////////////////////////////

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct Issue {
	code: String,
	location: String,
	message: String,
}

fn issue(issues: &mut Vec<Issue>, code: &str, location: impl Into<String>, message: impl Into<String>) {
	issues.push(Issue {
		code: code.to_owned(),
		location: location.into(),
		message: message.into(),
	});
}

impl Workflow {
	fn parse(source: &str) -> Result<Self> {
		// The serde detail is folded into the message: the top-level error is
		// all `wsync_error!` prints, and "invalid JSON" without the offending
		// field or position is useless to fix from
		let workflow: Workflow =
			serde_json::from_str(source).map_err(|err| anyhow::anyhow!("Invalid workflow JSON: {err}"))?;
		let issues = workflow.validation_issues();

		if issues.is_empty() {
			return Ok(workflow);
		}

		let mut report = format!("Workflow validation failed with {} issue(s):", issues.len());

		for entry in &issues {
			report.push_str(&format!(
				"\n  - {} at {}: {}",
				entry.code, entry.location, entry.message
			));
		}

		bail!("{report}")
	}

	fn validation_issues(&self) -> Vec<Issue> {
		let mut issues = Vec::new();

		if self.version != WORKFLOW_VERSION {
			issue(
				&mut issues,
				"unsupported_version",
				"$.version",
				format!("expected workflow version {WORKFLOW_VERSION}, got {}", self.version),
			);
		}

		if let Some(name) = &self.name {
			if name.is_empty() || name.chars().count() > 128 {
				issue(&mut issues, "invalid_name", "$.name", "name must be 1-128 characters");
			}
		}

		if let Some(key) = &self.idempotency_key {
			if key.is_empty() || key.chars().count() > 256 {
				issue(
					&mut issues,
					"invalid_idempotency_key",
					"$.idempotencyKey",
					"idempotencyKey must be 1-256 characters",
				);
			}
		}

		if let Some(mode) = &self.expected_mode {
			if !["edit", "play", "run"].contains(&mode.as_str()) {
				issue(
					&mut issues,
					"invalid_expected_mode",
					"$.expectedMode",
					format!("expectedMode must be edit, play, or run, not `{mode}`"),
				);
			}
		}

		if let Some(place_id) = &self.expected_place_id {
			if place_id.is_empty() || !place_id.bytes().all(|byte| byte.is_ascii_digit()) {
				issue(
					&mut issues,
					"invalid_place_id",
					"$.expectedPlaceId",
					"expectedPlaceId must be a decimal Roblox PlaceId string",
				);
			}
		}

		if self.steps.is_empty() {
			issue(
				&mut issues,
				"empty_workflow",
				"$.steps",
				"a workflow must contain at least one step",
			);
		} else if self.steps.len() > MAX_WORKFLOW_STEPS {
			issue(
				&mut issues,
				"too_many_steps",
				"$.steps",
				format!("at most {MAX_WORKFLOW_STEPS} steps are allowed"),
			);
		}

		// Transaction declarations: valid, unique ids
		let mut transaction_by_id: HashMap<&str, &TransactionGroup> = HashMap::new();

		for (index, transaction) in self.transactions.iter().enumerate() {
			let location = format!("$.transactions[{index}].id");

			check_identifier(&transaction.id, &location, &mut issues);

			if transaction_by_id.insert(transaction.id.as_str(), transaction).is_some() {
				issue(
					&mut issues,
					"duplicate_transaction_id",
					location,
					format!("transaction id `{}` is declared more than once", transaction.id),
				);
			}
		}

		// Step ids: valid, unique — and remembered by index for reference checks
		let mut first_step_index: HashMap<&str, usize> = HashMap::new();

		for (index, step) in self.steps.iter().enumerate() {
			let location = format!("$.steps[{index}].id");

			check_identifier(&step.id, &location, &mut issues);

			if let Some(first) = first_step_index.insert(step.id.as_str(), index) {
				issue(
					&mut issues,
					"duplicate_step_id",
					location,
					format!("step id `{}` was first used at index {first}", step.id),
				);
			}
		}

		let mut transaction_members: HashMap<&str, Vec<usize>> = HashMap::new();

		for (index, step) in self.steps.iter().enumerate() {
			let base = format!("$.steps[{index}]");

			validate_step(step, &base, &mut issues);

			if let Some(transaction_id) = &step.transaction {
				transaction_members
					.entry(transaction_id.as_str())
					.or_default()
					.push(index);

				match transaction_by_id.get(transaction_id.as_str()) {
					None => issue(
						&mut issues,
						"unknown_transaction",
						format!("{base}.transaction"),
						format!("transaction `{transaction_id}` is not declared"),
					),
					Some(transaction) if transaction.atomic && !step.operation.atomic_safe() => issue(
						&mut issues,
						"unsafe_atomic_operation",
						format!("{base}.op"),
						format!(
							"operation `{}` cannot run inside atomic transaction `{transaction_id}` — \
							 eval, call, wait, capture, playtest, and upload are rejected there",
							step.operation.op_name()
						),
					),
					_ => {}
				}
			}

			// Reference discipline: every `$stepId…` string in the step body
			// must name an earlier, different step
			let value = serde_json::to_value(step).expect("workflow steps always serialize");
			let mut references = Vec::new();

			scan_references(&value, &base, &mut references, &mut issues);

			for (location, step_id) in references {
				match first_step_index.get(step_id.as_str()).copied() {
					None => issue(
						&mut issues,
						"unknown_reference",
						location,
						format!("reference names unknown step `{step_id}`"),
					),
					Some(dependency) if dependency == index => issue(
						&mut issues,
						"self_reference",
						location,
						format!("step `{}` cannot reference its own result", step.id),
					),
					Some(dependency) if dependency > index => issue(
						&mut issues,
						"forward_reference",
						location,
						format!("step `{step_id}` appears later; workflows execute in order"),
					),
					_ => {}
				}
			}
		}

		// Atomic groups: non-empty and contiguous
		for (index, transaction) in self.transactions.iter().enumerate() {
			let members = transaction_members
				.get(transaction.id.as_str())
				.map(Vec::as_slice)
				.unwrap_or(&[]);

			if members.is_empty() {
				issue(
					&mut issues,
					"empty_transaction",
					format!("$.transactions[{index}]"),
					format!("transaction `{}` has no member steps", transaction.id),
				);
			}

			if transaction.atomic && !contiguous(members) {
				issue(
					&mut issues,
					"non_contiguous_atomic_transaction",
					format!("$.transactions[{index}]"),
					format!(
						"atomic transaction `{}` must occupy one contiguous range of steps",
						transaction.id
					),
				);
			}
		}

		issues
	}

	/// Direct dependencies of one step, in first-seen order
	fn dependencies_for(&self, step: &WorkflowStep) -> Vec<String> {
		let value = serde_json::to_value(step).expect("workflow steps always serialize");
		let mut ordered = Vec::new();

		collect_dependencies(&value, &mut ordered);

		ordered
	}
}

fn contiguous(members: &[usize]) -> bool {
	members.windows(2).all(|window| window[1] == window[0] + 1)
}

fn check_identifier(id: &str, location: &str, issues: &mut Vec<Issue>) {
	if !is_valid_identifier(id) {
		issue(
			issues,
			"invalid_identifier",
			location,
			format!("`{id}` must be 1-64 characters of letters, digits, `_` or `-`, starting with a letter or `_`"),
		);
	}
}

fn is_valid_identifier(id: &str) -> bool {
	let mut bytes = id.bytes();

	let Some(first) = bytes.next() else { return false };

	if !(first.is_ascii_alphabetic() || first == b'_') {
		return false;
	}

	id.len() <= 64 && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

/// True when the string is workflow reference syntax (so a literal-only check
/// must be skipped and re-run after resolution)
fn is_reference(text: &str) -> bool {
	matches!(parse_reference(text), Ok(Some(_)))
}

/// Per-step checks. Everything here runs twice: once statically and once
/// again after reference resolution, since resolution can inject any value
fn validate_step(step: &WorkflowStep, base: &str, issues: &mut Vec<Issue>) {
	if let Some(timeout_ms) = step.timeout_ms {
		if timeout_ms == 0 || timeout_ms > MAX_STEP_TIMEOUT_MS {
			issue(
				issues,
				"invalid_timeout",
				format!("{base}.timeoutMs"),
				format!("timeoutMs must be 1-{MAX_STEP_TIMEOUT_MS}"),
			);
		}
	}

	if step.verify && !step.operation.supports_verify() {
		issue(
			issues,
			"verify_unsupported",
			format!("{base}.verify"),
			format!("operation `{}` has no read-back verification", step.operation.op_name()),
		);
	}

	if (step.expected_class.is_some() || step.etag.is_some()) && step.operation.target_path().is_none() {
		issue(
			issues,
			"precondition_unsupported",
			base.to_owned(),
			format!(
				"operation `{}` has no target path for expectedClass/etag preconditions",
				step.operation.op_name()
			),
		);
	}

	validate_operation(&step.operation, base, issues);
}

fn validate_operation(operation: &StepOperation, base: &str, issues: &mut Vec<Issue>) {
	let require = |issues: &mut Vec<Issue>, field: &str, value: &str| {
		if value.is_empty() {
			issue(
				issues,
				"missing_field",
				format!("{base}.{field}"),
				format!("`{field}` must not be empty"),
			);
		}
	};

	match operation {
		StepOperation::Get { path, .. } | StepOperation::Rm { path } | StepOperation::AttrLs { path } => {
			require(issues, "path", path);
		}
		StepOperation::Set { path, property, .. } => {
			require(issues, "path", path);
			require(issues, "property", property);

			// The same guardrail `wsync set` enforces: a raw Parent write is
			// refused, loudly, naming `mv` — and a workflow has no
			// `--force-parent` escape at all
			if property == "Parent" {
				issue(
					issues,
					"parent_write_refused",
					format!("{base}.property"),
					"assigning .Parent through `set` is refused — use an `mv` step instead",
				);
			}
		}
		StepOperation::New { path, class, props, .. } => {
			require(issues, "path", path);
			require(issues, "class", class);

			if let Some(props) = props {
				if !props.is_object() && !matches!(props, Value::String(text) if is_reference(text)) {
					issue(
						issues,
						"invalid_props",
						format!("{base}.props"),
						"`props` must be a JSON object keyed by property name",
					);
				}
			}
		}
		StepOperation::Mv { from, to, force } => {
			require(issues, "from", from);
			require(issues, "to", to);

			// Literal-only: a referenced path is re-checked post-resolution
			if !force && !is_reference(from) && !is_reference(to) && service_of(from) != service_of(to) {
				issue(
					issues,
					"mv_crosses_service",
					format!("{base}.to"),
					format!(
						"moving `{from}` to `{to}` crosses a top-level service boundary — set `force: true` \
						 only when that is intentional"
					),
				);
			}
		}
		StepOperation::AttrSet { path, name, .. } | StepOperation::AttrRm { path, name } => {
			require(issues, "path", path);
			require(issues, "name", name);
		}
		StepOperation::TagAdd { path, tag } | StepOperation::TagRm { path, tag } => {
			require(issues, "path", path);
			require(issues, "tag", tag);
		}
		StepOperation::Assert { .. } => {}
		StepOperation::Wait {
			path, poll_interval_ms, ..
		} => {
			require(issues, "path", path);

			if let Some(poll) = poll_interval_ms {
				if *poll < MIN_WAIT_POLL_MS {
					issue(
						issues,
						"invalid_poll_interval",
						format!("{base}.pollIntervalMs"),
						format!("pollIntervalMs must be at least {MIN_WAIT_POLL_MS}"),
					);
				}
			}
		}
		StepOperation::Eval { source } => {
			require(issues, "source", source);
		}
		StepOperation::Capture {
			target,
			context,
			size,
			region,
			ui,
			..
		} => {
			if let Some(target) = target {
				if !["viewport", "ui", "model"].contains(&target.as_str()) && !is_reference(target) {
					issue(
						issues,
						"invalid_capture_target",
						format!("{base}.target"),
						format!("capture target must be viewport, ui, or model, not `{target}`"),
					);
				}
			}

			if let Some(context) = context {
				if !is_reference(context) && !context.starts_with("client:") {
					issue(
						issues,
						"invalid_capture_context",
						format!("{base}.context"),
						"a capture `context` must be a PlayClient context (`client:N`)",
					);
				}
			}

			if let Some(ui) = ui {
				if !["none", "overlay"].contains(&ui.as_str()) && !is_reference(ui) {
					issue(
						issues,
						"invalid_capture_ui",
						format!("{base}.ui"),
						format!("capture ui must be none or overlay, not `{ui}`"),
					);
				}
			}

			if let Some(size) = size {
				check_capture_dimensions(size.width, size.height, &format!("{base}.size"), issues);
			}

			if let Some(region) = region {
				check_capture_dimensions(region.width, region.height, &format!("{base}.region"), issues);
			}
		}
		StepOperation::Call { path, method, .. } => {
			require(issues, "path", path);
			require(issues, "method", method);
		}
		StepOperation::Playtest {
			script,
			script_file,
			client_script,
			client_script_file,
			context,
			mode,
			players,
			timeout_sec,
			identity,
			logs,
			..
		} => {
			if usize::from(script.is_some()) + usize::from(script_file.is_some()) != 1 {
				issue(
					issues,
					"invalid_playtest_script",
					format!("{base}.script"),
					"a playtest step needs exactly one of `script` (inline source) or `scriptFile`",
				);
			}

			if client_script.is_some() && client_script_file.is_some() {
				issue(
					issues,
					"invalid_playtest_client_script",
					format!("{base}.clientScript"),
					"use at most one of `clientScript` and `clientScriptFile`",
				);
			}

			if !is_reference(context) && playtest::check_context(context).is_err() {
				issue(
					issues,
					"invalid_playtest_context",
					format!("{base}.context"),
					format!("`{context}` must be `server` or `client:N`"),
				);
			}

			if !["play", "run", "multiplayer"].contains(&mode.as_str()) && !is_reference(mode) {
				issue(
					issues,
					"invalid_playtest_mode",
					format!("{base}.mode"),
					format!("mode must be play, run, or multiplayer, not `{mode}`"),
				);
			}

			if !(1..=8).contains(players) {
				issue(
					issues,
					"invalid_playtest_players",
					format!("{base}.players"),
					"players must be 1-8",
				);
			}

			if !(1..=3600).contains(timeout_sec) {
				issue(
					issues,
					"invalid_playtest_timeout",
					format!("{base}.timeoutSec"),
					"timeoutSec must be 1-3600",
				);
			}

			if !["game", "plugin"].contains(&identity.as_str()) && !is_reference(identity) {
				issue(
					issues,
					"invalid_playtest_identity",
					format!("{base}.identity"),
					format!("identity must be game or plugin, not `{identity}`"),
				);
			}

			if !["off", "info", "warn", "error"].contains(&logs.as_str()) && !is_reference(logs) {
				issue(
					issues,
					"invalid_playtest_logs",
					format!("{base}.logs"),
					format!("logs must be off, info, warn, or error, not `{logs}`"),
				);
			}
		}
		StepOperation::Upload { paths, .. } => {
			if paths.is_empty() {
				issue(
					issues,
					"missing_field",
					format!("{base}.paths"),
					"`paths` must name at least one target",
				);
			}
		}
	}
}

fn check_capture_dimensions(width: u32, height: u32, location: &str, issues: &mut Vec<Issue>) {
	if width == 0
		|| height == 0
		|| width > CAPTURE_MAX_AXIS
		|| height > CAPTURE_MAX_AXIS
		|| u64::from(width) * u64::from(height) > CAPTURE_MAX_PIXELS
	{
		issue(
			issues,
			"invalid_capture_dimensions",
			location.to_owned(),
			format!(
				"capture axes must be 1-{CAPTURE_MAX_AXIS} with at most {CAPTURE_MAX_PIXELS} total pixels \
				 ({width}x{height} requested)"
			),
		);
	}
}

/// The top-level service that owns a `/`-separated Studio path (the same
/// first-segment rule `wsync mv` and the plugin apply)
fn service_of(path: &str) -> &str {
	path.split('/').find(|segment| !segment.is_empty()).unwrap_or("")
}

impl StepOperation {
	fn op_name(&self) -> &'static str {
		match self {
			Self::Get { .. } => "get",
			Self::Set { .. } => "set",
			Self::New { .. } => "new",
			Self::Rm { .. } => "rm",
			Self::Mv { .. } => "mv",
			Self::AttrSet { .. } => "attr-set",
			Self::AttrRm { .. } => "attr-rm",
			Self::AttrLs { .. } => "attr-ls",
			Self::TagAdd { .. } => "tag-add",
			Self::TagRm { .. } => "tag-rm",
			Self::Assert { .. } => "assert",
			Self::Wait { .. } => "wait",
			Self::Eval { .. } => "eval",
			Self::Capture { .. } => "capture",
			Self::Call { .. } => "call",
			Self::Playtest { .. } => "playtest",
			Self::Upload { .. } => "upload",
		}
	}

	/// Safe means bounded, non-yielding, and with understood change-history
	/// behavior. Runtime and unknown-side-effect operations are excluded even
	/// when a particular invocation looks harmless
	fn atomic_safe(&self) -> bool {
		!matches!(
			self,
			Self::Eval { .. }
				| Self::Call { .. }
				| Self::Wait { .. }
				| Self::Capture { .. }
				| Self::Playtest { .. }
				| Self::Upload { .. }
		)
	}

	fn supports_verify(&self) -> bool {
		matches!(
			self,
			Self::Set { .. }
				| Self::New { .. }
				| Self::Rm { .. }
				| Self::Mv { .. }
				| Self::AttrSet { .. }
				| Self::AttrRm { .. }
				| Self::TagAdd { .. }
				| Self::TagRm { .. }
		)
	}

	/// The primary target path an expectedClass/etag precondition inspects
	fn target_path(&self) -> Option<&str> {
		match self {
			Self::Get { path, .. }
			| Self::Set { path, .. }
			| Self::New { path, .. }
			| Self::Rm { path }
			| Self::AttrSet { path, .. }
			| Self::AttrRm { path, .. }
			| Self::AttrLs { path }
			| Self::TagAdd { path, .. }
			| Self::TagRm { path, .. }
			| Self::Wait { path, .. }
			| Self::Call { path, .. } => Some(path),
			Self::Mv { from, .. } => Some(from),
			_ => None,
		}
	}

	/// The remote op this operation maps onto, in the live command machinery's
	/// wire spelling — `None` for locally executed steps
	fn wire_parts(&self) -> Option<(&'static str, Value)> {
		match self {
			Self::Get { path, property } => {
				let mut args = json!({ "path": path });

				if let Some(property) = property {
					args["prop"] = json!(property);
				}

				Some(("get", args))
			}
			Self::Set { path, property, value } => Some((
				"set",
				json!({ "path": path, "prop": property, "value": value, "forceParent": false }),
			)),
			Self::New {
				path,
				class,
				name,
				props,
			} => {
				let mut args = json!({ "path": path, "class": class });

				if let Some(name) = name {
					args["name"] = json!(name);
				}

				if let Some(props) = props {
					args["props"] = props.clone();
				}

				Some(("new", args))
			}
			Self::Rm { path } => Some(("rm", json!({ "path": path }))),
			Self::Mv { from, to, force } => Some(("mv", json!({ "from": from, "to": to, "force": force }))),
			Self::AttrSet { path, name, value } => {
				Some(("set_attr", json!({ "path": path, "name": name, "value": value })))
			}
			Self::AttrRm { path, name } => Some(("rm_attr", json!({ "path": path, "name": name }))),
			Self::AttrLs { path } => Some(("attr_ls", json!({ "path": path }))),
			Self::TagAdd { path, tag } => Some(("add_tag", json!({ "path": path, "tag": tag }))),
			Self::TagRm { path, tag } => Some(("rm_tag", json!({ "path": path, "tag": tag }))),
			Self::Eval { source } => Some(("eval", json!({ "source": source }))),
			Self::Call { path, method, args } => {
				let mut wire = json!({ "path": path, "method": method });

				if !args.is_empty() {
					wire["args"] = json!(args);
				}

				Some(("call", wire))
			}
			_ => None,
		}
	}
}

////////////////////////////////////////////////////////////////////////////////
// References
////////////////////////////////////////////////////////////////////////////////

/// Parses reference syntax. `Ok(None)` for plain strings (including currency
/// look-alikes such as `$100`); `Err` for malformed references
fn parse_reference(text: &str) -> Result<Option<(String, Vec<String>)>, String> {
	if !text.starts_with('$') || text.starts_with("$$") {
		return Ok(None);
	}

	let body = &text[1..];
	let mut parts = body.split('.');
	let step_id = parts.next().unwrap_or_default();

	if !step_id
		.bytes()
		.next()
		.is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
	{
		return Ok(None);
	}

	if !is_valid_identifier(step_id) {
		return Err("step id must use letters, digits, `_` or `-`".into());
	}

	let path: Vec<String> = parts.map(str::to_owned).collect();

	if path.iter().any(String::is_empty) {
		return Err("reference path contains an empty segment".into());
	}

	Ok(Some((step_id.to_owned(), path)))
}

/// Resolves references in place. Object keys are never templated, and a
/// resolved value is inserted verbatim without a second scan — Studio data
/// can never become workflow syntax
fn resolve_references(value: &mut Value, results: &StepResults) -> Result<(), String> {
	match value {
		Value::String(text) if text.starts_with("$$") => {
			text.remove(0);
			Ok(())
		}
		Value::String(text) => {
			let Some((step_id, path)) = parse_reference(text)? else {
				return Ok(());
			};

			let mut current = results
				.get(&step_id)
				.ok_or_else(|| format!("reference `{text}` has no result for step `{step_id}`"))?;

			for segment in &path {
				current = match current {
					Value::Object(object) => object.get(segment),
					Value::Array(array) => segment.parse::<usize>().ok().and_then(|index| array.get(index)),
					_ => None,
				}
				.ok_or_else(|| format!("reference `{text}` is missing path segment `{segment}`"))?;
			}

			*value = current.clone();

			Ok(())
		}
		Value::Array(values) => values
			.iter_mut()
			.try_for_each(|value| resolve_references(value, results)),
		Value::Object(values) => values
			.values_mut()
			.try_for_each(|value| resolve_references(value, results)),
		_ => Ok(()),
	}
}

fn scan_references(value: &Value, location: &str, references: &mut Vec<(String, String)>, issues: &mut Vec<Issue>) {
	match value {
		Value::String(text) if !text.starts_with("$$") => match parse_reference(text) {
			Ok(Some((step_id, _))) => references.push((location.to_owned(), step_id)),
			Ok(None) => {}
			Err(reason) => issue(
				issues,
				"invalid_reference",
				location,
				format!("invalid reference `{text}`: {reason}"),
			),
		},
		Value::Array(values) => {
			for (index, value) in values.iter().enumerate() {
				scan_references(value, &format!("{location}[{index}]"), references, issues);
			}
		}
		Value::Object(values) => {
			for (key, value) in values {
				scan_references(value, &format!("{location}.{key}"), references, issues);
			}
		}
		_ => {}
	}
}

fn collect_dependencies(value: &Value, ordered: &mut Vec<String>) {
	match value {
		Value::String(text) if !text.starts_with("$$") => {
			if let Ok(Some((step_id, _))) = parse_reference(text) {
				if !ordered.contains(&step_id) {
					ordered.push(step_id);
				}
			}
		}
		Value::Array(values) => values.iter().for_each(|value| collect_dependencies(value, ordered)),
		Value::Object(values) => values.values().for_each(|value| collect_dependencies(value, ordered)),
		_ => {}
	}
}

impl WorkflowStep {
	/// Substitutes references, then re-runs every per-step check on the
	/// resolved step — substitution can inject a forbidden value (a resolved
	/// `property` of `Parent`) that the static pass could not see
	fn resolve(&self, results: &StepResults) -> Result<Self, String> {
		let mut value = serde_json::to_value(self).expect("workflow steps always serialize");

		resolve_references(&mut value, results)?;

		let resolved: Self =
			serde_json::from_value(value).map_err(|err| format!("resolved step no longer matches schema: {err}"))?;

		let mut issues = Vec::new();

		validate_step(&resolved, "$.resolvedStep", &mut issues);

		if issues.is_empty() {
			return Ok(resolved);
		}

		let report: Vec<String> = issues
			.iter()
			.map(|entry| format!("{} at {}: {}", entry.code, entry.location, entry.message))
			.collect();

		Err(format!(
			"resolved step failed {} validation check(s): {}",
			report.len(),
			report.join("; ")
		))
	}
}

////////////////////////////////////////////////////////////////////////////////
// Assertions
////////////////////////////////////////////////////////////////////////////////

/// JSON equality with numeric tolerance for integer/float spellings of the
/// same number (`1` vs `1.0`)
fn loose_eq(left: &Value, right: &Value) -> bool {
	if left == right {
		return true;
	}

	match (left.as_f64(), right.as_f64()) {
		(Some(left), Some(right)) => left == right,
		_ => false,
	}
}

/// Luau truthiness: nil and false are falsy, everything else is truthy
fn truthy(value: Option<&Value>) -> bool {
	match value {
		None | Some(Value::Null) | Some(Value::Bool(false)) => false,
		Some(_) => true,
	}
}

impl Assertion {
	/// `actual: None` means the target could not be read (a missing instance
	/// or property) — which satisfies `exists: false` and fails the rest
	fn check(&self, actual: Option<&Value>) -> Result<(), String> {
		let describe = |value: Option<&Value>| match value {
			Some(value) => value.to_string(),
			None => "<missing>".to_owned(),
		};

		let numeric = |actual: Option<&Value>| -> Result<f64, String> {
			actual
				.and_then(Value::as_f64)
				.ok_or_else(|| format!("actual value {} is not a number", describe(actual)))
		};

		match self {
			Self::Equals { expected } => match actual {
				Some(actual) if loose_eq(actual, expected) => Ok(()),
				_ => Err(format!("expected {expected}, got {}", describe(actual))),
			},
			Self::NotEquals { expected } => match actual {
				Some(actual) if loose_eq(actual, expected) => Err(format!("expected anything but {expected}")),
				_ => Ok(()),
			},
			Self::Exists { expected } => {
				let exists = matches!(actual, Some(value) if !value.is_null());

				if exists == *expected {
					Ok(())
				} else {
					Err(format!("expected exists={expected}, got exists={exists}"))
				}
			}
			Self::Truthy { expected } => {
				let actual_truthy = truthy(actual);

				if actual_truthy == *expected {
					Ok(())
				} else {
					Err(format!("expected truthy={expected}, got {}", describe(actual)))
				}
			}
			Self::Contains { expected } => {
				let contained = match actual {
					Some(Value::String(text)) => expected.as_str().is_some_and(|needle| text.contains(needle)),
					Some(Value::Array(items)) => items.iter().any(|item| loose_eq(item, expected)),
					Some(Value::Object(map)) => expected.as_str().is_some_and(|key| map.contains_key(key)),
					_ => false,
				};

				if contained {
					Ok(())
				} else {
					Err(format!("{} does not contain {expected}", describe(actual)))
				}
			}
			Self::GreaterThan { expected } => {
				let value = numeric(actual)?;

				if value > *expected {
					Ok(())
				} else {
					Err(format!("expected > {expected}, got {value}"))
				}
			}
			Self::GreaterThanOrEqual { expected } => {
				let value = numeric(actual)?;

				if value >= *expected {
					Ok(())
				} else {
					Err(format!("expected >= {expected}, got {value}"))
				}
			}
			Self::LessThan { expected } => {
				let value = numeric(actual)?;

				if value < *expected {
					Ok(())
				} else {
					Err(format!("expected < {expected}, got {value}"))
				}
			}
			Self::LessThanOrEqual { expected } => {
				let value = numeric(actual)?;

				if value <= *expected {
					Ok(())
				} else {
					Err(format!("expected <= {expected}, got {value}"))
				}
			}
		}
	}
}

////////////////////////////////////////////////////////////////////////////////
// The command
////////////////////////////////////////////////////////////////////////////////

impl Run {
	pub fn main(self) -> Result<()> {
		let source = fs::read_to_string(&self.file)
			.with_context(|| format!("Failed to read the workflow file {}", self.file.display()))?;
		let workflow = Workflow::parse(&source)?;

		if self.dry_run {
			return self.print_dry_run(&workflow);
		}

		// The workspace (idempotency home) is known before any daemon probe
		let target = Target::resolve(&self.targeting)?;
		let workspace = target.project_path.parent().unwrap_or(Path::new(".")).to_path_buf();

		let workflow_hash = content_hash(&workflow);
		let record_path = workflow
			.idempotency_key
			.as_deref()
			.map(|key| idempotency_path(&workspace, key));

		if let Some(path) = &record_path {
			if let Some(previous) = replay(path, &workflow_hash)? {
				self.print_outcome(&previous)?;

				return Ok(());
			}
		}

		// Serialize executions for one key: without the lock, two agents can
		// both miss the record and each repeat every side effect
		let _lock = record_path.as_deref().map(IdempotencyLock::acquire).transpose()?;

		if let Some(path) = &record_path {
			if let Some(previous) = replay(path, &workflow_hash)? {
				self.print_outcome(&previous)?;

				return Ok(());
			}
		}

		let client = Client::connect(&self.targeting)?;

		check_environment(&client, &workflow)?;

		let outcome = self.execute(&client, &workflow, &workflow_hash)?;
		let ok = outcome.get("ok").and_then(Value::as_bool) == Some(true);

		if ok {
			if let Some(path) = &record_path {
				write_json_atomic(path, &outcome)
					.with_context(|| format!("Failed to write the idempotency record {}", path.display()))?;
			}
		}

		self.print_outcome(&outcome)?;

		if !ok {
			bail!("Workflow failed — inspect the step results above");
		}

		Ok(())
	}

	fn print_dry_run(&self, workflow: &Workflow) -> Result<()> {
		let steps: Vec<Value> = workflow
			.steps
			.iter()
			.map(|step| {
				let wire = step
					.operation
					.wire_parts()
					.map(|(op, args)| json!({ "op": op, "args": args }));

				json!({
					"id": step.id,
					"op": step.operation.op_name(),
					"executor": if wire.is_some() { "plugin" } else { "local" },
					"wire": wire,
					"transaction": step.transaction,
					"atomicSafe": step.operation.atomic_safe(),
					"dependencies": workflow.dependencies_for(step),
				})
			})
			.collect();

		let plan = json!({
			"ok": true,
			"dryRun": true,
			"name": workflow.name,
			"version": workflow.version,
			"idempotencyKey": workflow.idempotency_key,
			"transactions": workflow.transactions.iter().map(|group| json!({
				"id": group.id,
				"atomic": group.atomic,
			})).collect::<Vec<_>>(),
			"steps": steps,
		});

		if self.raw {
			print_json(&plan);
		} else {
			println!("{}", serde_json::to_string_pretty(&plan)?);
		}

		Ok(())
	}

	fn print_outcome(&self, outcome: &Value) -> Result<()> {
		if self.raw {
			print_json(outcome);

			return Ok(());
		}

		if outcome.get("replayed").and_then(Value::as_bool) == Some(true) {
			wsync_info!("Replayed the recorded result for this idempotencyKey — no side effects were repeated");
		}

		let empty = Vec::new();

		for report in outcome.get("steps").and_then(Value::as_array).unwrap_or(&empty) {
			let ok = report.get("ok").and_then(Value::as_bool) == Some(true);

			println!(
				"{:<24} {:<8} {:>6}ms{}",
				report.get("id").and_then(Value::as_str).unwrap_or("?"),
				if ok { "ok" } else { "failed" },
				report.get("durationMs").and_then(Value::as_u64).unwrap_or(0),
				match report.pointer("/error/message").and_then(Value::as_str) {
					Some(message) => format!("  {message}"),
					None => String::new(),
				}
			);
		}

		let count = |key: &str| {
			outcome
				.get(key)
				.and_then(Value::as_array)
				.map(Vec::len)
				.unwrap_or_default()
		};

		if count("rollbackErrors") > 0 || count("transactionErrors") > 0 {
			wsync_warn!("Transaction cleanup reported errors — inspect the outcome with --raw");
		}

		Ok(())
	}

	/// The ordered execution loop. Never returns `Err` for a step failure —
	/// failures are part of the outcome; `Err` means the run itself could not
	/// proceed (a transaction bracket that failed to even send)
	fn execute(&self, client: &Client, workflow: &Workflow, workflow_hash: &str) -> Result<Value> {
		let transaction_defs: HashMap<&str, bool> = workflow
			.transactions
			.iter()
			.map(|group| (group.id.as_str(), group.atomic))
			.collect();

		let workspace = Target::resolve(&self.targeting)
			.ok()
			.map(|target| target.project_path.parent().unwrap_or(Path::new(".")).to_path_buf())
			.unwrap_or_else(|| PathBuf::from("."));
		let workflow_dir = self
			.file
			.parent()
			.filter(|parent| !parent.as_os_str().is_empty())
			.map(Path::to_path_buf)
			.unwrap_or_else(|| PathBuf::from("."));

		let mut results = StepResults::new();
		let mut reports = Vec::with_capacity(workflow.steps.len());
		let mut active_atomic: Option<String> = None;
		let mut failed = false;
		let mut rollback_errors: Vec<String> = Vec::new();
		let mut transaction_errors: Vec<String> = Vec::new();
		let mut transaction_outcomes: Vec<Value> = Vec::new();

		for original in &workflow.steps {
			let entering_atomic = original
				.transaction
				.as_deref()
				.is_some_and(|id| transaction_defs.get(id).copied().unwrap_or(false));

			// Resolution happens against completed results only; a resolution
			// failure is this step's failure
			let step = match original.resolve(&results) {
				Ok(step) => step,
				Err(reason) => {
					let response = error_response("REFERENCE_RESOLUTION", &reason);

					results.insert(original.id.clone(), response.clone());
					reports.push(step_report(original, &response, 0, None));
					failed = true;

					let inside_atomic = entering_atomic || active_atomic.is_some();

					if let Some(id) = active_atomic.take() {
						if let Err(err) = finish_transaction(client, &id, false, &mut transaction_outcomes) {
							rollback_errors.push(format!("{id}: {err}"));
						}
					}

					// An atomic group is one unit — running a suffix of it in
					// a fresh recording would commit half a transaction
					if inside_atomic || !self.keep_going {
						break;
					}

					continue;
				}
			};

			// Transaction boundary transitions: commit the previous group,
			// begin the next
			let desired_atomic = step
				.transaction
				.as_deref()
				.filter(|id| transaction_defs.get(*id).copied().unwrap_or(false))
				.map(str::to_owned);

			if active_atomic != desired_atomic {
				if let Some(id) = active_atomic.take() {
					if let Err(err) = finish_transaction(client, &id, true, &mut transaction_outcomes) {
						failed = true;
						transaction_errors.push(format!("{id}: commit failed: {err}"));

						if let Err(err) = finish_transaction(client, &id, false, &mut transaction_outcomes) {
							rollback_errors.push(format!("{id}: {err}"));
						}

						break;
					}
				}

				if let Some(id) = desired_atomic.clone() {
					let name = match &workflow.name {
						Some(name) => format!("{name}: {id}"),
						None => format!("WSync workflow: {id}"),
					};

					let begun = client
						.request("transaction_begin", json!({ "name": name }))
						.map_err(|err| anyhow::anyhow!("Failed to begin transaction `{id}`: {err}"))?;

					if !begun.ok {
						bail!(
							"Failed to begin transaction `{id}`: {} [{}]",
							begun.error_message(),
							begun.error_code()
						);
					}

					active_atomic = desired_atomic;
				}
			}

			// Preconditions, then the operation, then verification
			let started = Instant::now();
			let timeout_ms = step.timeout_ms.unwrap_or(DEFAULT_STEP_TIMEOUT_MS);
			let mut verified = None;

			let response = match check_step_preconditions(client, &step) {
				Err(reason) => error_response("PRECONDITION_FAILED", &reason),
				Ok(()) => {
					let response = execute_step(client, &step, timeout_ms, &workspace, &workflow_dir);

					if step.verify && response.get("ok").and_then(Value::as_bool) == Some(true) {
						match verify_step(client, &step, &response) {
							Ok(()) => {
								verified = Some(true);
								response
							}
							Err(reason) => {
								verified = Some(false);
								error_response("VERIFY_FAILED", &reason)
							}
						}
					} else {
						response
					}
				}
			};

			let duration_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
			let step_ok = response.get("ok").and_then(Value::as_bool) == Some(true);

			results.insert(step.id.clone(), response.clone());
			reports.push(step_report(&step, &response, duration_ms, verified));

			if !step_ok {
				failed = true;

				if let Some(id) = active_atomic.take() {
					if let Err(err) = finish_transaction(client, &id, false, &mut transaction_outcomes) {
						rollback_errors.push(format!("{id}: {err}"));
					}

					break;
				}

				if !self.keep_going {
					break;
				}
			}
		}

		// A trailing successful atomic group commits even after an unrelated
		// earlier failure under --keep-going; failures inside it cancelled
		// above
		if let Some(id) = active_atomic.take() {
			if let Err(err) = finish_transaction(client, &id, true, &mut transaction_outcomes) {
				failed = true;
				transaction_errors.push(format!("{id}: commit failed: {err}"));

				if let Err(err) = finish_transaction(client, &id, false, &mut transaction_outcomes) {
					rollback_errors.push(format!("{id}: {err}"));
				}
			}
		}

		let rolled_back = transaction_outcomes.iter().any(|outcome| {
			outcome.get("commit").and_then(Value::as_bool) == Some(false)
				&& outcome.get("ok").and_then(Value::as_bool) == Some(true)
		});

		Ok(json!({
			"ok": !failed,
			"schema": "wsync.workflow-result.v1",
			"name": workflow.name,
			"idempotencyKey": workflow.idempotency_key,
			"workflowHash": workflow_hash,
			"steps": reports,
			"results": results,
			"transactions": transaction_outcomes,
			"transactionErrors": transaction_errors,
			"rollbackErrors": rollback_errors,
			"rolledBack": rolled_back,
			"replayed": false,
		}))
	}
}

////////////////////////////////////////////////////////////////////////////////
// Execution helpers
////////////////////////////////////////////////////////////////////////////////

/// A response-shaped value for CLI-detected failures, so every stored result
/// reads the same way (`ok` / `value` / `error`)
fn error_response(code: &str, message: &str) -> Value {
	json!({
		"ok": false,
		"value": Value::Null,
		"error": { "code": code, "message": message },
	})
}

fn envelope_response(envelope: &Envelope) -> Value {
	let mut response = json!({ "ok": envelope.ok, "value": envelope.value });

	if !envelope.ok {
		response["error"] = json!({
			"code": envelope.error_code(),
			"message": envelope.error_message(),
		});
	}

	response
}

fn step_report(step: &WorkflowStep, response: &Value, duration_ms: u64, verified: Option<bool>) -> Value {
	let mut report = json!({
		"id": step.id,
		"op": step.operation.op_name(),
		"transaction": step.transaction,
		"ok": response.get("ok").and_then(Value::as_bool).unwrap_or(false),
		"durationMs": duration_ms,
		"error": response.get("error"),
	});

	if let Some(verified) = verified {
		report["verified"] = json!(verified);
	}

	report
}

/// One remote op as a response value — transport failures become response
/// shapes too, so a step's outcome always has the same anatomy
fn request_response(client: &Client, op: &str, args: Value, timeout_ms: u64) -> Value {
	match client.request_with_timeout(op, args, timeout_ms) {
		Ok(envelope) => envelope_response(&envelope),
		Err(err) => error_response("STEP_TRANSPORT", &format!("{err:#}")),
	}
}

/// Workflow-level preconditions, checked once before the first step
fn check_environment(client: &Client, workflow: &Workflow) -> Result<()> {
	if workflow.expected_mode.is_none() && workflow.expected_place_id.is_none() {
		return Ok(());
	}

	let capabilities = client
		.request("capabilities", json!({}))
		.context("Failed to check the workflow's environment preconditions")?;

	if !capabilities.ok {
		bail!(
			"Environment precondition failed: the capabilities op answered {} [{}]",
			capabilities.error_message(),
			capabilities.error_code()
		);
	}

	if let Some(expected) = &workflow.expected_place_id {
		let actual = match capabilities.value.get("placeId") {
			Some(Value::String(text)) => text.clone(),
			Some(Value::Number(number)) => number.to_string(),
			_ => bail!("Place precondition failed: the plugin did not report a placeId"),
		};

		if &actual != expected {
			bail!("Place precondition failed: expected {expected}, connected to {actual}");
		}
	}

	if let Some(expected) = &workflow.expected_mode {
		let actual = capabilities
			.value
			.get("mode")
			.or_else(|| capabilities.value.get("hostDataModelType"))
			.and_then(Value::as_str)
			.map(str::to_ascii_lowercase);

		match actual {
			Some(actual) if &actual == expected => {}
			Some(actual) => bail!("Mode precondition failed: expected {expected}, connected to {actual}"),
			None => bail!("Mode precondition failed: the plugin did not report its mode"),
		}
	}

	Ok(())
}

/// Step-level `expectedClass`/`etag` staleness rejection: one `get` on the
/// target, compared before the operation runs
fn check_step_preconditions(client: &Client, step: &WorkflowStep) -> Result<(), String> {
	if step.expected_class.is_none() && step.etag.is_none() {
		return Ok(());
	}

	let Some(path) = step.operation.target_path() else {
		return Err("this operation has no target path for preconditions".to_owned());
	};

	let view = request_response(client, "get", json!({ "path": path }), DEFAULT_STEP_TIMEOUT_MS);

	if view.get("ok").and_then(Value::as_bool) != Some(true) {
		return Err(format!(
			"stale target: `{path}` could not be read ({})",
			view.pointer("/error/message").and_then(Value::as_str).unwrap_or("gone")
		));
	}

	if let Some(expected) = &step.expected_class {
		match view.pointer("/value/class").and_then(Value::as_str) {
			Some(actual) if actual == expected => {}
			Some(actual) => return Err(format!("stale target: `{path}` is {actual}, expected {expected}")),
			None => return Err(format!("stale target: the plugin did not report a class for `{path}`")),
		}
	}

	if let Some(expected) = &step.etag {
		match view.pointer("/value/etag").and_then(Value::as_str) {
			Some(actual) if actual == expected => {}
			Some(actual) => {
				return Err(format!(
					"stale target: `{path}` etag is {actual}, expected {expected} — the instance changed"
				))
			}
			None => return Err(format!("stale target: the plugin did not report an etag for `{path}`")),
		}
	}

	Ok(())
}

fn execute_step(client: &Client, step: &WorkflowStep, timeout_ms: u64, workspace: &Path, workflow_dir: &Path) -> Value {
	// The plain remote ops
	if let Some((op, args)) = step.operation.wire_parts() {
		return request_response(client, op, args, timeout_ms);
	}

	match &step.operation {
		StepOperation::Assert { actual, check, message } => match check.check(Some(actual)) {
			Ok(()) => json!({ "ok": true, "value": { "passed": true } }),
			Err(reason) => error_response(
				"ASSERTION_FAILED",
				&match message {
					Some(message) => format!("{message}: {reason}"),
					None => reason,
				},
			),
		},
		StepOperation::Wait {
			path,
			property,
			check,
			poll_interval_ms,
		} => execute_wait(client, path, property.as_deref(), check, timeout_ms, *poll_interval_ms),
		StepOperation::Capture { .. } => execute_capture(client, step, timeout_ms, workspace),
		StepOperation::Playtest { .. } => execute_playtest(client, step, workflow_dir),
		StepOperation::Upload { .. } => error_response(
			"NOT_AVAILABLE",
			"the `upload` command is not available in this build — the Open Cloud surface has not shipped yet",
		),
		_ => error_response("INTERNAL", "unroutable step operation"),
	}
}

/// Polls one path/property until the assertion passes or the step's timeout
/// expires. A read failure counts as a missing target (satisfying
/// `exists: false`), never as a hard error — only the deadline fails the step
fn execute_wait(
	client: &Client,
	path: &str,
	property: Option<&str>,
	check: &Assertion,
	timeout_ms: u64,
	poll_interval_ms: Option<u64>,
) -> Value {
	let deadline = Instant::now() + Duration::from_millis(timeout_ms);
	let interval = Duration::from_millis(poll_interval_ms.unwrap_or(DEFAULT_WAIT_POLL_MS).max(MIN_WAIT_POLL_MS));
	let mut polls: u64 = 0;

	loop {
		polls += 1;

		let mut args = json!({ "path": path });

		if let Some(property) = property {
			args["prop"] = json!(property);
		}

		let response = request_response(client, "get", args, DEFAULT_STEP_TIMEOUT_MS);
		let actual = if response.get("ok").and_then(Value::as_bool) == Some(true) {
			response.get("value")
		} else {
			None
		};

		let reason = match check.check(actual) {
			Ok(()) => {
				return json!({
					"ok": true,
					"value": {
						"passed": true,
						"polls": polls,
						"actual": actual.cloned().unwrap_or(Value::Null),
					},
				});
			}
			Err(reason) => reason,
		};

		if Instant::now() + interval > deadline {
			return error_response(
				"WAIT_TIMEOUT",
				&format!("condition not met within {timeout_ms}ms after {polls} poll(s): {reason}"),
			);
		}

		thread::sleep(interval);
	}
}

fn execute_capture(client: &Client, step: &WorkflowStep, timeout_ms: u64, workspace: &Path) -> Value {
	let StepOperation::Capture {
		target,
		path,
		context,
		size,
		region,
		ui,
		skybox,
		world,
		framed,
		output,
	} = &step.operation
	else {
		return error_response("INTERNAL", "unroutable step operation");
	};

	let mut options = Map::new();

	if let Some(size) = size {
		options.insert("size".to_owned(), json!({ "width": size.width, "height": size.height }));
	}

	if let Some(region) = region {
		options.insert(
			"region".to_owned(),
			json!({ "x": region.x, "y": region.y, "width": region.width, "height": region.height }),
		);
	}

	if let Some(ui) = ui {
		options.insert("ui".to_owned(), json!(ui));
	}

	for (flag, on) in [("skybox", *skybox), ("world", *world), ("framed", *framed)] {
		if on {
			options.insert(flag.to_owned(), json!(true));
		}
	}

	let output = match output {
		Some(output) => PathBuf::from(output),
		None => workspace.join(format!("wsync-capture-{}.png", step.id)),
	};

	let (prepare_op, mut args) = match context {
		Some(context) => ("playtest_capture", json!({ "context": context })),
		None => (
			"capture_prepare",
			json!({ "kind": target.as_deref().unwrap_or("viewport") }),
		),
	};

	if context.is_none() {
		if let Some(path) = path {
			args["target"] = json!(path);
		}
	}

	if !options.is_empty() {
		args["options"] = Value::Object(options);
	}

	match capture::perform_prepared(client, prepare_op, args, &output, timeout_ms.max(60_000), false) {
		Ok(summary) => json!({ "ok": true, "value": summary }),
		Err(err) => error_response("CAPTURE_FAILED", &format!("{err:#}")),
	}
}

/// One complete playscript session through the shared `playtest run`
/// machinery: start, poll the record stream (kept as a bounded tail), map the
/// terminal record to the run exit contract, and always stop the playtest
fn execute_playtest(client: &Client, step: &WorkflowStep, workflow_dir: &Path) -> Value {
	let StepOperation::Playtest {
		script,
		script_file,
		client_script,
		client_script_file,
		context,
		mode,
		players,
		args,
		timeout_sec,
		identity,
		logs,
	} = &step.operation
	else {
		return error_response("INTERNAL", "unroutable step operation");
	};

	let read_file = |file: &str| -> Result<String, String> {
		let path = workflow_dir.join(file);

		fs::read_to_string(&path).map_err(|err| format!("failed to read {}: {err}", path.display()))
	};

	let main_script = match (script, script_file) {
		(Some(source), None) => source.clone(),
		(None, Some(file)) => match read_file(file) {
			Ok(source) => source,
			Err(reason) => return error_response("PLAYTEST_SCRIPT", &reason),
		},
		_ => {
			return error_response(
				"PLAYTEST_SCRIPT",
				"exactly one of `script` and `scriptFile` is required",
			)
		}
	};

	let companion = match (client_script, client_script_file) {
		(Some(source), _) => Some(source.clone()),
		(None, Some(file)) => match read_file(file) {
			Ok(source) => Some(source),
			Err(reason) => return error_response("PLAYTEST_SCRIPT", &reason),
		},
		(None, None) => None,
	};

	let spec = playtest::RunSpec {
		script: main_script,
		client_script: companion,
		context: context.clone(),
		mode: mode.clone(),
		players: *players,
		args: if args.is_null() { json!({}) } else { args.clone() },
		timeout_sec: *timeout_sec,
		identity: identity.clone(),
		logs: logs.clone(),
	};

	let start = match client.request_with_timeout("playtest_run_start", spec.start_args(), 15_000) {
		Ok(envelope) => envelope,
		Err(err) => return error_response("STEP_TRANSPORT", &format!("{err:#}")),
	};

	if !start.ok {
		return error_response(
			"PLAYTEST_BOOT",
			&format!("{} [{}]", start.error_message(), start.error_code()),
		);
	}

	let Some(job_id) = start.value.get("jobId").and_then(Value::as_str).map(str::to_owned) else {
		return error_response("PLAYTEST_BOOT", "playtest_run_start answered without a jobId");
	};

	let deadline = Instant::now() + Duration::from_secs(*timeout_sec) + PLAYTEST_DEADLINE_GRACE;

	// Progress records are kept as a bounded tail so a chatty run cannot grow
	// the workflow outcome without limit
	const RECORD_TAIL: usize = 64;
	let mut records: Vec<Value> = Vec::new();
	let mut record_count: u64 = 0;

	let polled = playtest::poll_run(client, &job_id, deadline, |record| {
		record_count += 1;

		if records.len() == RECORD_TAIL {
			records.remove(0);
		}

		records.push(record.clone());

		Ok(())
	});

	// The workflow owns the session — the playtest never outlives the step
	match client.request("playtest_stop", json!({})) {
		Ok(envelope) if envelope.ok => {}
		Ok(envelope) => wsync_warn!(
			"Failed to stop the playtest after the step: {}",
			envelope.error_message()
		),
		Err(err) => wsync_warn!("Failed to stop the playtest after the step: {err}"),
	}

	let terminal = match polled {
		Ok(playtest::RunEnd::Done(terminal)) => terminal,
		Ok(playtest::RunEnd::DeadlineExpired) => {
			client.request("playtest_run_cancel", json!({ "jobId": job_id })).ok();

			return error_response(
				"PLAYTEST_TIMEOUT",
				&format!("no terminal record within timeoutSec {timeout_sec} (plus grace)"),
			);
		}
		Err(err) => {
			client.request("playtest_run_cancel", json!({ "jobId": job_id })).ok();

			return error_response("PLAYTEST_ABORTED", &format!("lost the run stream: {err:#}"));
		}
	};

	let value = json!({
		"jobId": job_id,
		"exitCode": terminal.exit_code,
		"terminal": terminal.record,
		"recordCount": record_count,
		"records": records,
	});

	if terminal.exit_code == 0 {
		json!({ "ok": true, "value": value })
	} else {
		let mut response = error_response(
			"PLAYTEST_FAILED",
			&format!(
				"playtest run exited {} ({})",
				terminal.exit_code,
				terminal
					.record
					.get("kind")
					.or_else(|| terminal.record.get("type"))
					.and_then(Value::as_str)
					.unwrap_or("failure")
			),
		);

		response["value"] = value;
		response
	}
}

/// Read-back verification for the supported writes (run.json `verify: true`)
fn verify_step(client: &Client, step: &WorkflowStep, response: &Value) -> Result<(), String> {
	let read = |op: &str, args: Value| -> Value { request_response(client, op, args, DEFAULT_STEP_TIMEOUT_MS) };
	let read_ok = |view: &Value| view.get("ok").and_then(Value::as_bool) == Some(true);

	match &step.operation {
		StepOperation::Set { path, property, value } => {
			let view = read("get", json!({ "path": path, "prop": property }));

			if !read_ok(&view) {
				return Err(format!("read-back of {path}.{property} failed"));
			}

			let actual = view.get("value").unwrap_or(&Value::Null);

			if loose_eq(actual, value) {
				Ok(())
			} else {
				Err(format!(
					"read-back of {path}.{property} returned {actual}, expected {value}"
				))
			}
		}
		StepOperation::New { path, name, .. } => {
			// The op reports the created path; the fallback is parent/name
			let created = response
				.pointer("/value/path")
				.and_then(Value::as_str)
				.map(str::to_owned)
				.or_else(|| name.as_ref().map(|name| format!("{path}/{name}")))
				.ok_or_else(|| "the plugin did not report the created path".to_owned())?;

			let view = read("get", json!({ "path": created }));

			if read_ok(&view) {
				Ok(())
			} else {
				Err(format!("the created instance `{created}` does not read back"))
			}
		}
		StepOperation::Rm { path } => {
			let view = read("get", json!({ "path": path }));

			if read_ok(&view) {
				Err(format!("`{path}` still reads back after rm"))
			} else {
				Ok(())
			}
		}
		StepOperation::Mv { from, to, .. } => {
			let moved = response
				.pointer("/value/path")
				.and_then(Value::as_str)
				.map(str::to_owned)
				.unwrap_or_else(|| {
					let name = from.rsplit('/').next().unwrap_or(from);

					format!("{to}/{name}")
				});

			let view = read("get", json!({ "path": moved }));

			if read_ok(&view) {
				Ok(())
			} else {
				Err(format!("the moved instance `{moved}` does not read back"))
			}
		}
		StepOperation::AttrSet { path, name, value } => {
			let view = read("attr_ls", json!({ "path": path }));

			if !read_ok(&view) {
				return Err(format!("read-back of {path} attributes failed"));
			}

			match view.pointer("/value/attributes").and_then(|map| map.get(name)) {
				Some(actual) if loose_eq(actual, value) => Ok(()),
				Some(actual) => Err(format!("attribute {name} read back as {actual}, expected {value}")),
				None => Err(format!("attribute {name} is missing after the write")),
			}
		}
		StepOperation::AttrRm { path, name } => {
			let view = read("attr_ls", json!({ "path": path }));

			if !read_ok(&view) {
				return Err(format!("read-back of {path} attributes failed"));
			}

			match view.pointer("/value/attributes").and_then(|map| map.get(name)) {
				Some(_) => Err(format!("attribute {name} still present after the removal")),
				None => Ok(()),
			}
		}
		StepOperation::TagAdd { path, tag } | StepOperation::TagRm { path, tag } => {
			let view = read("get", json!({ "path": path }));

			if !read_ok(&view) {
				return Err(format!("read-back of {path} failed"));
			}

			let tagged = view
				.pointer("/value/tags")
				.and_then(Value::as_array)
				.map(|tags| tags.iter().any(|entry| entry.as_str() == Some(tag)))
				.unwrap_or(false);

			let expect_tagged = matches!(&step.operation, StepOperation::TagAdd { .. });

			if tagged == expect_tagged {
				Ok(())
			} else if expect_tagged {
				Err(format!("tag {tag} is missing after the write"))
			} else {
				Err(format!("tag {tag} still present after the removal"))
			}
		}
		_ => Ok(()),
	}
}

/// `transaction_finish {commit}` with the outcome journalled for the report
fn finish_transaction(client: &Client, id: &str, commit: bool, outcomes: &mut Vec<Value>) -> Result<()> {
	let result = client
		.request("transaction_finish", json!({ "commit": commit }))
		.map_err(|err| anyhow::anyhow!("{err:#}"))
		.and_then(|envelope| {
			if envelope.ok {
				Ok(())
			} else {
				bail!("{} [{}]", envelope.error_message(), envelope.error_code())
			}
		});

	outcomes.push(json!({
		"id": id,
		"commit": commit,
		"ok": result.is_ok(),
		"error": result.as_ref().err().map(ToString::to_string),
	}));

	result
}

////////////////////////////////////////////////////////////////////////////////
// Idempotency
////////////////////////////////////////////////////////////////////////////////

fn idempotency_path(workspace: &Path, key: &str) -> PathBuf {
	let digest = format!("{:x}", Sha256::digest(key.as_bytes()));

	workspace.join(".wsync-workflows").join(format!("{digest}.json"))
}

fn content_hash(workflow: &Workflow) -> String {
	let normalized = serde_json::to_vec(workflow).expect("workflows always serialize");

	format!("{:x}", Sha256::digest(&normalized))
}

/// `Some(outcome)` replays the stored result (marked `replayed: true`)
/// without re-running side effects. The same key with different workflow
/// content is a collision and a hard error — silently replaying it would
/// claim work that never happened
fn replay(path: &Path, expected_hash: &str) -> Result<Option<Value>> {
	if !path.is_file() {
		return Ok(None);
	}

	let mut previous: Value = serde_json::from_slice(
		&fs::read(path).with_context(|| format!("Failed to read the idempotency record {}", path.display()))?,
	)
	.with_context(|| format!("Failed to parse the idempotency record {}", path.display()))?;

	let recorded = previous.get("workflowHash").and_then(Value::as_str).with_context(|| {
		format!(
			"The idempotency record {} carries no workflow hash — remove it or choose a new idempotencyKey",
			path.display()
		)
	})?;

	if recorded != expected_hash {
		bail!(
			"idempotencyKey collision at {}: this key was already used for a different workflow",
			path.display()
		);
	}

	previous["replayed"] = json!(true);

	Ok(Some(previous))
}

/// One-per-key execution lock (`<record>.lock`, created exclusively).
/// Dropped — and the file removed — when the run ends either way
struct IdempotencyLock {
	path: PathBuf,
}

impl IdempotencyLock {
	fn acquire(record_path: &Path) -> Result<Self> {
		let parent = record_path.parent().context("Invalid idempotency record path")?;

		fs::create_dir_all(parent)
			.with_context(|| format!("Failed to create the workflow journal directory {}", parent.display()))?;

		let path = record_path.with_extension("lock");
		let mut options = fs::OpenOptions::new();

		options.write(true).create_new(true);

		#[cfg(unix)]
		{
			use std::os::unix::fs::OpenOptionsExt;

			options.mode(0o600);
		}

		let mut file = options.open(&path).map_err(|err| {
			if err.kind() == std::io::ErrorKind::AlreadyExists {
				anyhow::anyhow!(
					"A workflow with this idempotencyKey is already running (lock {}). Remove the lock only \
					 after confirming no run is active",
					path.display()
				)
			} else {
				anyhow::anyhow!("Failed to create the idempotency lock {}: {err}", path.display())
			}
		})?;

		writeln!(file, "pid={}", std::process::id()).ok();

		Ok(Self { path })
	}
}

impl Drop for IdempotencyLock {
	fn drop(&mut self) {
		fs::remove_file(&self.path).ok();
	}
}

/// 0600 atomic JSON write (temp + rename), so a torn run never leaves a
/// half-written idempotency record that a later run would replay
fn write_json_atomic(path: &Path, value: &Value) -> Result<()> {
	if let Some(parent) = path.parent() {
		fs::create_dir_all(parent)?;
	}

	let temp = path.with_extension(format!("tmp-{}", std::process::id()));
	let mut options = fs::OpenOptions::new();

	options.write(true).create_new(true);

	#[cfg(unix)]
	{
		use std::os::unix::fs::OpenOptionsExt;

		options.mode(0o600);
	}

	let result = (|| -> Result<()> {
		let mut file = options.open(&temp)?;

		serde_json::to_writer_pretty(&mut file, value)?;
		file.write_all(b"\n")?;
		file.sync_all()?;
		fs::rename(&temp, path)?;

		Ok(())
	})();

	if result.is_err() {
		fs::remove_file(&temp).ok();
	}

	result
}

#[cfg(test)]
mod tests {
	use super::*;

	fn parse_issues(source: &str) -> Vec<String> {
		let workflow: Workflow = serde_json::from_str(source).expect("fixture deserializes");

		workflow
			.validation_issues()
			.into_iter()
			.map(|issue| issue.code)
			.collect()
	}

	#[test]
	fn version_and_shape_are_validated() {
		let issues = parse_issues(r#"{ "version": 2, "steps": [] }"#);

		assert!(issues.contains(&"unsupported_version".to_owned()));
		assert!(issues.contains(&"empty_workflow".to_owned()));
	}

	#[test]
	fn duplicate_forward_and_self_references_are_rejected() {
		let issues = parse_issues(
			r#"{
				"version": 1,
				"steps": [
					{ "id": "a", "op": "get", "path": "$b.value.path" },
					{ "id": "a", "op": "get", "path": "Workspace" },
					{ "id": "b", "op": "get", "path": "$b.value.path" },
					{ "id": "c", "op": "get", "path": "$nope.value" }
				]
			}"#,
		);

		assert!(issues.contains(&"duplicate_step_id".to_owned()));
		assert!(issues.contains(&"forward_reference".to_owned()));
		assert!(issues.contains(&"self_reference".to_owned()));
		assert!(issues.contains(&"unknown_reference".to_owned()));
	}

	#[test]
	fn atomic_transactions_must_be_contiguous_and_safe() {
		let issues = parse_issues(
			r#"{
				"version": 1,
				"transactions": [{ "id": "tx" }],
				"steps": [
					{ "id": "a", "op": "set", "path": "Workspace/A", "property": "Name", "value": "x", "transaction": "tx" },
					{ "id": "b", "op": "get", "path": "Workspace" },
					{ "id": "c", "op": "eval", "source": "return 1", "transaction": "tx" }
				]
			}"#,
		);

		assert!(issues.contains(&"non_contiguous_atomic_transaction".to_owned()));
		assert!(issues.contains(&"unsafe_atomic_operation".to_owned()));
	}

	#[test]
	fn parent_writes_and_cross_service_moves_are_refused() {
		let issues = parse_issues(
			r#"{
				"version": 1,
				"steps": [
					{ "id": "a", "op": "set", "path": "Workspace/A", "property": "Parent", "value": "x" },
					{ "id": "b", "op": "mv", "from": "Workspace/A", "to": "ReplicatedStorage" }
				]
			}"#,
		);

		assert!(issues.contains(&"parent_write_refused".to_owned()));
		assert!(issues.contains(&"mv_crosses_service".to_owned()));
	}

	#[test]
	fn resolution_rechecks_injected_values() {
		let step: WorkflowStep = serde_json::from_value(json!({
			"id": "b",
			"op": "set",
			"path": "Workspace/A",
			"property": "$a.value.prop",
			"value": 1,
		}))
		.unwrap();

		let mut results = StepResults::new();

		results.insert("a".into(), json!({ "ok": true, "value": { "prop": "Parent" } }));

		let err = step.resolve(&results).unwrap_err();

		assert!(err.contains("parent_write_refused"), "unexpected error: {err}");
	}

	#[test]
	fn dollar_escape_and_reference_resolution_work() {
		let mut results = StepResults::new();

		results.insert("a".into(), json!({ "ok": true, "value": { "path": "Workspace/Box" } }));

		let mut value = json!({
			"literal": "$$100",
			"plain": "$100",
			"reference": "$a.value.path",
			"nested": ["$a.ok"],
		});

		resolve_references(&mut value, &results).unwrap();

		assert_eq!(value["literal"], "$100");
		assert_eq!(value["plain"], "$100");
		assert_eq!(value["reference"], "Workspace/Box");
		assert_eq!(value["nested"][0], json!(true));
	}

	#[test]
	fn missing_reference_paths_fail_precisely() {
		let mut results = StepResults::new();

		results.insert("a".into(), json!({ "ok": true, "value": {} }));

		let mut value = json!("$a.value.path");
		let err = resolve_references(&mut value, &results).unwrap_err();

		assert!(err.contains("path"), "unexpected error: {err}");

		let mut value = json!("$missing.value");
		let err = resolve_references(&mut value, &results).unwrap_err();

		assert!(err.contains("missing"), "unexpected error: {err}");
	}

	#[test]
	fn assertions_cover_the_documented_families() {
		let equals = Assertion::Equals { expected: json!(5) };

		assert!(equals.check(Some(&json!(5))).is_ok());
		assert!(equals.check(Some(&json!(5.0))).is_ok());
		assert!(equals.check(Some(&json!(6))).is_err());
		assert!(equals.check(None).is_err());

		assert!(Assertion::Exists { expected: false }.check(None).is_ok());
		assert!(Assertion::Exists { expected: true }.check(Some(&json!(0))).is_ok());
		assert!(Assertion::Truthy { expected: true }.check(Some(&json!(0))).is_ok());
		assert!(Assertion::Truthy { expected: false }.check(Some(&json!(false))).is_ok());

		let contains = Assertion::Contains { expected: json!("box") };

		assert!(contains.check(Some(&json!("a box here"))).is_ok());
		assert!(contains.check(Some(&json!(["box", "cat"]))).is_ok());
		assert!(contains.check(Some(&json!({ "box": 1 }))).is_ok());
		assert!(contains.check(Some(&json!("nothing"))).is_err());

		assert!(Assertion::GreaterThan { expected: 1.0 }.check(Some(&json!(2))).is_ok());
		assert!(Assertion::LessThanOrEqual { expected: 2.0 }
			.check(Some(&json!(2)))
			.is_ok());
		assert!(Assertion::GreaterThan { expected: 1.0 }
			.check(Some(&json!("2")))
			.is_err());
	}

	#[test]
	fn wire_parts_use_the_live_command_spellings() {
		let step: WorkflowStep = serde_json::from_value(json!({
			"id": "a",
			"op": "attr-set",
			"path": "Workspace/A",
			"name": "Speed",
			"value": 5,
		}))
		.unwrap();

		let (op, args) = step.operation.wire_parts().unwrap();

		assert_eq!(op, "set_attr");
		assert_eq!(args, json!({ "path": "Workspace/A", "name": "Speed", "value": 5 }));

		let step: WorkflowStep = serde_json::from_value(json!({
			"id": "b",
			"op": "tag-add",
			"path": "Workspace/A",
			"tag": "Enemy",
		}))
		.unwrap();

		assert_eq!(step.operation.wire_parts().unwrap().0, "add_tag");
	}

	#[test]
	fn unknown_step_fields_are_rejected() {
		let result = serde_json::from_value::<WorkflowStep>(json!({
			"id": "a",
			"op": "get",
			"path": "Workspace",
			"unexpected": true,
		}));

		assert!(result.is_err());
	}
}
