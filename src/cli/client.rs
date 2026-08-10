//! Shared plumbing for the live command surface (Design §10.1–10.2).
//!
//! Every live command targets a daemon the same way: `--project` selects the
//! project, the project's runtime record supplies the port, and `GET /hello`
//! confirms the daemon on that port actually serves that project before a
//! single op is sent. `--port` overrides the discovery chain (a deliberately
//! targeted daemon is only warned about, never refused, on identity
//! mismatch).
//!
//! Remote ops go through `POST /request` with the 5 s default remote-op
//! timeout. Transport failures, plugin errors, and daemon-side timeouts all
//! surface as hard errors, so "non-zero exit = the request did not complete"
//! holds for every command built on this module.
//!
//! `--raw` conventions kept stable across the surface:
//!
//! * commands that wrap exactly one remote op print that op's `value` as one
//!   JSON line (`capabilities` is the documented exception — capabilities.json
//!   asks for the complete correlated envelope);
//! * commands that compose several sources print a CLI-authored object whose
//!   first key is `ok`;
//! * live writes print their op's value with `ok` prepended ([`print_ok`]),
//!   so a machine caller reads success and failure off the same key;
//! * streams (`logs --tail`, `diff --raw`) print NDJSON, one record per line,
//!   flushed immediately;
//! * a failed op still prints its error envelope on stdout before the command
//!   exits non-zero, so a machine caller always gets a parseable line.

use anyhow::{bail, Context, Result};
use clap::Args;
use colored::Colorize;
use log::debug;
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
	io::{self, Write},
	path::PathBuf,
	time::Duration,
};

use crate::{
	config::Config,
	daemon::{self, RuntimeRecord},
	ext::PathExt,
	project::{self, Project},
	wsync_warn,
};

/// Default remote-op timeout (Design §10.1, Appendix C)
pub const REMOTE_TIMEOUT_MS: u64 = 5000;

/// Extra head-room the HTTP hop gets over the remote deadline, so the
/// daemon's own `TIMEOUT` envelope wins the race against a client-side abort
/// (a daemon-authored timeout names the op; a torn socket does not)
const HTTP_SLACK_MS: u64 = 3000;

/// How long `GET /hello` discovery probes wait
const PROBE_TIMEOUT: Duration = Duration::from_millis(1500);

/// `--project`/`--port` targeting, flattened into every live command
#[derive(Args, Debug, Clone, Default)]
pub struct Targeting {
	/// Project path (defaults to the current directory)
	#[arg(long, value_name = "PATH")]
	pub project: Option<PathBuf>,

	/// Daemon port (defaults to the project's runtime record)
	#[arg(short = 'P', long, value_name = "PORT")]
	pub port: Option<u16>,
}

/// Where the port a command talks to came from — reported in diagnostics so
/// "nothing answers" is always traceable to a concrete guess
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortSource {
	Flag,
	Record,
	ProjectFile,
	Config,
}

impl PortSource {
	pub fn as_str(self) -> &'static str {
		match self {
			PortSource::Flag => "--port",
			PortSource::Record => "runtime record",
			PortSource::ProjectFile => "project file",
			PortSource::Config => "config default",
		}
	}
}

/// The resolved project + port a live command addresses, before any daemon
/// is known to answer
pub struct Target {
	pub project_path: PathBuf,
	pub project_exists: bool,
	/// `None` when the project file does not exist (nothing to canonicalize)
	pub canonical: Option<String>,
	pub state_dir: PathBuf,
	pub record: Option<RuntimeRecord>,
	pub port: u16,
	pub port_source: PortSource,
}

impl Target {
	/// Discovery chain: `--port` first, then the project's runtime record
	/// (the authoritative registry), then the project file's `port`, then the
	/// config default
	pub fn resolve(targeting: &Targeting) -> Result<Self> {
		let project_path = project::resolve(targeting.project.clone().unwrap_or_default())?;
		let project_exists = project_path.exists();

		if project_exists {
			Config::load_workspace(project_path.get_parent());
		}

		let state_dir = daemon::state_dir(None)?;

		let canonical = if project_exists {
			daemon::canonical_project(&project_path).ok()
		} else {
			None
		};

		let record = canonical.as_ref().and_then(|canonical| {
			let paths = daemon::runtime_paths(&state_dir, canonical);

			daemon::read_record(&paths.record).unwrap_or_default()
		});

		let (port, port_source) = match targeting.port {
			Some(port) => (port, PortSource::Flag),
			None => match &record {
				Some(record) => (record.port, PortSource::Record),
				None => {
					let project_port = if project_exists {
						Project::load(&project_path).ok().and_then(|project| project.port)
					} else {
						None
					};

					match project_port {
						Some(port) => (port, PortSource::ProjectFile),
						None => (Config::new().port, PortSource::Config),
					}
				}
			},
		};

		debug!(
			"Live target: project {} on port {port} (from the {})",
			project_path.to_string(),
			port_source.as_str()
		);

		Ok(Self {
			project_path,
			project_exists,
			canonical,
			state_dir,
			record,
			port,
			port_source,
		})
	}

	/// The daemon's runtime record matches the daemon that actually answers
	pub fn record_is_live(&self, hello: &Hello) -> bool {
		match &self.record {
			Some(record) => record.boot_id == hello.boot_id && record.canonical_project == hello.canonical_project,
			None => false,
		}
	}

	pub fn probe(&self) -> Result<Hello> {
		let response = reqwest::blocking::Client::builder()
			.timeout(PROBE_TIMEOUT)
			// Unknown routes answer with a redirect to `/`; following it
			// would turn "endpoint not implemented" into a bogus 200
			.redirect(reqwest::redirect::Policy::none())
			.build()?
			.get(format!("http://localhost:{}/hello", self.port))
			.send()?
			.error_for_status()?;

		Ok(response.json()?)
	}

	pub fn writes_log(&self) -> PathBuf {
		self.state_dir.join("writes.log")
	}

	/// Default `wsync sourcemap` output location for this project
	pub fn sourcemap(&self) -> PathBuf {
		self.project_path.get_parent().join("sourcemap.json")
	}
}

/// `GET /hello` — the daemon's JSON identity (Design §5.2)
#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Hello {
	#[serde(default)]
	pub name: String,
	#[serde(default)]
	pub version: String,
	#[serde(default)]
	pub protocol: u8,
	#[serde(default)]
	pub project: String,
	#[serde(default)]
	pub canonical_project: String,
	#[serde(default)]
	pub game_id: Option<u64>,
	#[serde(default)]
	pub place_ids: Vec<u64>,
	#[serde(default)]
	pub boot_id: String,
	#[serde(default)]
	pub pid: u32,
	#[serde(default)]
	pub port: u16,
	#[serde(default)]
	pub managed_by: String,
	/// Where this daemon appends the mutation audit log. Absent on a daemon
	/// build that predates the audit writer, so `status` falls back to the
	/// state-dir default rather than claiming a location the daemon never
	/// confirmed
	#[serde(default)]
	pub writes_log: Option<String>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct OpError {
	pub code: String,
	pub message: String,
}

/// A parsed `POST /request` reply. `ok == false` is a completed HTTP
/// round-trip carrying a plugin-side failure — never a transport error
#[derive(Debug, Clone)]
pub struct Envelope {
	pub ok: bool,
	pub value: Value,
	pub error: Option<OpError>,
	pub duration_ms: Option<u64>,
	pub protocol: Option<u64>,
	/// The complete envelope exactly as the daemon sent it
	pub body: Value,
}

impl Envelope {
	fn parse(op: &str, body: Value) -> Result<Self> {
		let ok = body
			.get("ok")
			.and_then(Value::as_bool)
			.with_context(|| format!("Daemon answered op `{op}` without an `ok` field"))?;

		Ok(Self {
			ok,
			value: body.get("value").cloned().unwrap_or(Value::Null),
			error: body
				.get("error")
				.and_then(|error| serde_json::from_value::<OpError>(error.clone()).ok()),
			duration_ms: body.pointer("/meta/durationMs").and_then(Value::as_u64),
			protocol: body.pointer("/meta/protocol").and_then(Value::as_u64),
			body,
		})
	}

	/// True when the failure means "no usable Studio plugin" rather than a
	/// plugin-side op error — the tolerated state for `status`/`doctor`
	pub fn plugin_missing(&self) -> bool {
		!self.ok && self.error.as_ref().is_some_and(|error| error.code == "PLUGIN_ERROR")
	}

	pub fn error_code(&self) -> &str {
		self.error.as_ref().map_or("PLUGIN_ERROR", |error| error.code.as_str())
	}

	pub fn error_message(&self) -> &str {
		self.error
			.as_ref()
			.map_or("the daemon reported failure without error detail", |error| {
				error.message.as_str()
			})
	}

	/// The op's value, or a hard error naming the plugin's code and message.
	/// Under `--raw` the failure envelope is printed first so machine callers
	/// always get one parseable line before the non-zero exit
	pub fn into_value(self, raw: bool) -> Result<Value> {
		if self.ok {
			return Ok(self.value);
		}

		if raw {
			print_json(&self.body);
		}

		bail!("{} [{}]", self.error_message(), self.error_code())
	}
}

/// A non-`/request` daemon endpoint reply (`/resolve`, `/choice`), kept as
/// status + optionally-parsed JSON so callers can act on 409 and on
/// endpoints this daemon build does not serve yet
pub struct Endpoint {
	pub status: u16,
	pub json: Option<Value>,
	pub text: String,
}

impl Endpoint {
	/// Unknown routes answer with a temporary redirect to `/`, so a daemon
	/// build that predates an endpoint is a redirect — never JSON. A bodyless
	/// 404 is treated the same way; a 404 *with* a JSON body is a real
	/// daemon-authored error and keeps its own message
	pub fn unavailable(&self) -> bool {
		(300..400).contains(&self.status) || (self.status == 404 && self.json.is_none())
	}

	pub fn json(&self, path: &str) -> Result<&Value> {
		if self.unavailable() {
			bail!(
				"This daemon build does not serve {path} yet, so the conflict surface is unavailable. \
				 Update the daemon (`wsync daemon restart --project <path>`) once the conflict engine ships"
			);
		}

		let Some(json) = &self.json else {
			bail!(
				"Daemon answered {path} with a non-JSON body (HTTP {}): {}",
				self.status,
				truncate(&self.text, 200)
			);
		};

		// A rejected request must never be read as data; callers that handle
		// a specific status (409 on `POST /choice`) check it before asking
		if self.status >= 400 {
			bail!(
				"Daemon rejected {path} (HTTP {}): {}",
				self.status,
				json.get("error")
					.and_then(Value::as_str)
					.map_or_else(|| json.to_string(), str::to_owned)
			);
		}

		Ok(json)
	}
}

/// A live connection to one project's daemon
pub struct Client {
	pub target: Target,
	pub hello: Hello,
	http: reqwest::blocking::Client,
	timeout_ms: u64,
}

impl Client {
	/// Resolve, probe, and verify identity. Fails when nothing answers — the
	/// state every live command except `status`/`doctor` treats as fatal
	pub fn connect(targeting: &Targeting) -> Result<Self> {
		let target = Target::resolve(targeting)?;

		let hello = target.probe().with_context(|| {
			format!(
				"No WSync daemon answers on port {} (from the {}). Start one with `{}`",
				target.port,
				target.port_source.as_str(),
				"wsync daemon start --project <path>".bold()
			)
		})?;

		Self::open(target, hello)
	}

	/// Wraps an already-probed daemon (the `status`/`doctor` path, which must
	/// report an unreachable daemon instead of failing on it)
	pub fn open(target: Target, hello: Hello) -> Result<Self> {
		if let Some(canonical) = &target.canonical {
			if &hello.canonical_project != canonical {
				let complaint = format!(
					"Port {} serves a different project ({}), not {}",
					target.port,
					hello.project.bold(),
					target.project_path.to_string().bold()
				);

				// An explicitly targeted port is the caller's decision; only
				// a discovered one is refused
				if target.port_source == PortSource::Flag {
					wsync_warn!("{}", complaint);
				} else {
					bail!("{complaint}");
				}
			}
		}

		let timeout_ms = REMOTE_TIMEOUT_MS;
		let http = reqwest::blocking::Client::builder()
			.timeout(Duration::from_millis(timeout_ms + HTTP_SLACK_MS))
			.redirect(reqwest::redirect::Policy::none())
			.build()?;

		Ok(Self {
			target,
			hello,
			http,
			timeout_ms,
		})
	}

	pub fn port(&self) -> u16 {
		self.target.port
	}

	fn url(&self, path: &str) -> String {
		format!("http://localhost:{}{path}", self.target.port)
	}

	/// One remote op over `POST /request`. `Err` means the round-trip itself
	/// failed; a plugin-side failure comes back as `Ok(envelope)` with
	/// `ok == false`
	pub fn request(&self, op: &str, args: Value) -> Result<Envelope> {
		self.request_with_timeout(op, args, self.timeout_ms)
	}

	/// [`Self::request`] with a per-op deadline for the transfer surfaces
	/// (`capture`, `copy`, `paste`), whose prepare/commit ops legitimately
	/// outlast the 5 s default. The HTTP hop keeps the same slack rule as the
	/// default path, so the daemon's own `TIMEOUT` envelope still wins the race
	pub fn request_with_timeout(&self, op: &str, args: Value, timeout_ms: u64) -> Result<Envelope> {
		let body = json!({ "op": op, "args": args, "timeoutMs": timeout_ms });

		let response = self
			.http
			.post(self.url("/request"))
			.timeout(Duration::from_millis(timeout_ms + HTTP_SLACK_MS))
			.json(&body)
			.send()
			.with_context(|| format!("Failed to send op `{op}` to the daemon on port {}", self.target.port))?
			.error_for_status()?;

		let body: Value = response
			.json()
			.with_context(|| format!("Daemon returned a malformed reply for op `{op}`"))?;

		Envelope::parse(op, body)
	}

	/// The common path: run one op and take its value, failing the command
	/// when the plugin reports an error
	pub fn value(&self, op: &str, args: Value, raw: bool) -> Result<Value> {
		self.request(op, args)?.into_value(raw)
	}

	pub fn get_endpoint(&self, path: &str) -> Result<Endpoint> {
		let response = self
			.http
			.get(self.url(path))
			.send()
			.with_context(|| format!("Failed to reach {path} on the daemon at port {}", self.target.port))?;

		Ok(Self::endpoint(response))
	}

	pub fn post_endpoint(&self, path: &str, body: &Value) -> Result<Endpoint> {
		let response = self
			.http
			.post(self.url(path))
			.json(body)
			.send()
			.with_context(|| format!("Failed to reach {path} on the daemon at port {}", self.target.port))?;

		Ok(Self::endpoint(response))
	}

	fn endpoint(response: reqwest::blocking::Response) -> Endpoint {
		let status = response.status().as_u16();
		let text = response.text().unwrap_or_default();

		Endpoint {
			status,
			json: serde_json::from_str(&text).ok(),
			text,
		}
	}
}

/// One JSON line on stdout, flushed (live output is usually piped, and Rust
/// block-buffers stdout there)
pub fn print_json(value: &Value) {
	println!("{value}");
	io::stdout().flush().ok();
}

/// A live write's `--raw` line: the op's value with `ok` first. Writes are
/// the one family where success and failure have to be told apart from the
/// payload alone, and a failed op prints the daemon's own `ok:false`
/// envelope ([`Envelope::into_value`]) — so both shapes lead with the same
/// discriminator
pub fn print_ok(value: &Value) {
	let mut record = serde_json::Map::new();

	record.insert("ok".to_owned(), Value::Bool(true));

	match value {
		// An op that answered with its own `ok` never gets to overwrite the
		// CLI's, so the first key stays authoritative
		Value::Object(fields) => record.extend(
			fields
				.iter()
				.filter(|(key, _)| *key != "ok")
				.map(|(key, field)| (key.clone(), field.clone())),
		),
		// Every write op answers with an object; anything else is reported
		// under `value` rather than dropped
		other => {
			record.insert("value".to_owned(), other.clone());
		}
	}

	print_json(&Value::Object(record));
}

/// The `--value` codec shared by `set`, `attr set`, `find-attr`, `new
/// --props`, and `call --args`: a JSON literal, including the tagged objects
/// the plugin's codec understands (`{"__type":"Vector3",…}`). Bare text that
/// is not valid JSON is taken as a string, so `--value Red` does not have to
/// be spelled `--value '"Red"'`
pub fn parse_literal(literal: &str) -> Value {
	serde_json::from_str(literal).unwrap_or_else(|_| Value::String(literal.to_owned()))
}

/// Names a JSON value's type for usage errors ("--args must be a JSON array,
/// not a string")
pub fn kind_of(value: &Value) -> &'static str {
	match value {
		Value::Null => "null",
		Value::Bool(_) => "a boolean",
		Value::Number(_) => "a number",
		Value::String(_) => "a string",
		Value::Array(_) => "an array",
		Value::Object(_) => "an object",
	}
}

pub fn print_line(line: &str) {
	println!("{line}");
	io::stdout().flush().ok();
}

pub fn truncate(text: &str, max: usize) -> String {
	if text.chars().count() <= max {
		return text.to_owned();
	}

	let kept: String = text.chars().take(max).collect();

	format!("{kept}… ({} bytes)", text.len())
}

/// Clips to an exact display width for table cells (unlike [`truncate`],
/// which annotates the cut and is meant for prose)
pub fn clip(text: &str, width: usize) -> String {
	if text.chars().count() <= width {
		return text.to_owned();
	}

	text.chars().take(width.saturating_sub(1)).chain(['…']).collect()
}

/// Renders a tagged-codec value for human output. Tagged objects
/// (`{"__type":"Vector3",…}`) become `Vector3(x=1, y=2, z=3)`; everything
/// else falls back to compact JSON with plain strings unquoted
pub fn human_value(value: &Value) -> String {
	match value {
		Value::Null => "nil".to_owned(),
		Value::Bool(value) => value.to_string(),
		Value::Number(value) => value.to_string(),
		Value::String(text) => truncate(text, 120),
		Value::Array(items) => {
			let rendered: Vec<String> = items.iter().take(8).map(human_value).collect();

			if items.len() > rendered.len() {
				format!("[{}, … {} more]", rendered.join(", "), items.len() - rendered.len())
			} else {
				format!("[{}]", rendered.join(", "))
			}
		}
		Value::Object(map) => match map.get("__type").and_then(Value::as_str) {
			Some(kind) => {
				let mut fields: Vec<(&String, &Value)> =
					map.iter().filter(|(key, _)| key.as_str() != "__type").collect();

				fields.sort_by_key(|(key, _)| key.as_str());

				let rendered: Vec<String> = fields
					.iter()
					.map(|(key, value)| format!("{key}={}", human_value(value)))
					.collect();

				if rendered.is_empty() {
					kind.to_owned()
				} else {
					format!("{kind}({})", rendered.join(", "))
				}
			}
			None => truncate(&value.to_string(), 160),
		},
	}
}

/// `value.get(key)` as a `&str`, for the many `{path, class, name}` records
pub fn field<'a>(value: &'a Value, key: &str) -> &'a str {
	value.get(key).and_then(Value::as_str).unwrap_or("")
}

/// Reports the plugin's truncation on stderr — the machine-readable
/// `truncationReason` turned into an actionable sentence (query.json)
pub fn truncation_advice(reason: Option<&str>) -> String {
	match reason {
		Some("matches") => "result truncated at the match limit — raise --limit or narrow the selector".to_owned(),
		Some("nodes") => {
			"search stopped at the plugin's node budget — scope the search with a narrower root".to_owned()
		}
		Some("time") => "search stopped at the plugin's time budget — scope the search with a narrower root".to_owned(),
		Some("response-bytes") => {
			"result truncated at the plugin's response-byte budget — project fewer properties or lower --limit"
				.to_owned()
		}
		Some(other) => format!("result truncated by the plugin ({other})"),
		None => "result truncated by the plugin".to_owned(),
	}
}
