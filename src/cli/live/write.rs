//! Live writes: `set`, `new`, `rm`, `mv`, `attr`, `tag`, `call`, `eval`
//! (set.json, new.json, rm.json, mv.json, attr.json, tag.json, call.json,
//! eval.json; Design §10.2, §10.6).
//!
//! These are user-initiated escape hatches, never autonomous tools, so the
//! guardrails are enforced twice — once here, before a socket is opened, and
//! once again in the plugin (Remote/WriteRules). The CLI half is what makes a
//! refusal *free*: nothing is sent, so nothing can half-apply.
//!
//! * `set --prop Parent` is refused without `--force-parent`, loudly, naming
//!   `wsync mv` (set.json);
//! * a `--batch` file is read and screened whole — one guarded entry rejects
//!   the file before any network call, and `--keep-going` only relaxes
//!   *runtime* failures the plugin reports per entry (set.json). A batched
//!   Parent write is refused unconditionally: the `set_batch` op has no
//!   Parent escape at all (the registry note says "no escape"), so not even
//!   `--force-parent` sends one;
//! * `mv` across a top-level service boundary is refused without `--force`,
//!   using the same first-path-segment rule the plugin applies (mv.json).
//!
//! `--raw` prints the op's value with `ok` prepended ([`print_ok`]) so a
//! machine caller reads success and failure off one key; a failed op still
//! prints the daemon's own envelope before the non-zero exit.

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use colored::Colorize;
use serde_json::{json, Value};
use std::{fs, path::PathBuf};

use crate::{
	cli::client::{clip, field, human_value, kind_of, parse_literal, print_ok, Client, Targeting},
	wsync_info, wsync_warn,
};

// ---------------------------------------------------------------------------
// set
// ---------------------------------------------------------------------------

/// Set one property, or apply a batch of property writes, in Studio
#[derive(Parser)]
pub struct Set {
	#[command(flatten)]
	targeting: Targeting,

	/// Studio path, `/`-separated (e.g. `Workspace/Camera`)
	#[arg(long, value_name = "STUDIO-PATH", conflicts_with = "batch")]
	path: Option<String>,

	/// Property name
	#[arg(long, value_name = "PROPERTY", conflicts_with = "batch")]
	prop: Option<String>,

	/// Value as a JSON literal or tagged value; bare text that is not valid
	/// JSON is taken as a string
	#[arg(long, value_name = "JSON", conflicts_with = "batch")]
	value: Option<String>,

	/// Apply a JSON array of `{path, prop, value}` writes from this file
	#[arg(long, value_name = "FILE")]
	batch: Option<PathBuf>,

	/// In a batch, keep applying after an entry fails (never relaxes the
	/// pre-flight guards)
	#[arg(long = "keep-going")]
	keep_going: bool,

	/// Bracket the writes in a named change-history waypoint, so one Studio
	/// undo reverses the batch
	#[arg(long, value_name = "NAME")]
	waypoint: Option<String>,

	/// Allow a raw `Parent` assignment instead of refusing and pointing at
	/// `wsync mv` (single writes only — a batch has no escape)
	#[arg(long = "force-parent")]
	force_parent: bool,

	/// Print machine-readable JSON
	#[arg(long)]
	raw: bool,
}

/// One screened `--batch` entry, kept alongside the request so per-entry
/// results (which the plugin returns positionally, without echoing the
/// target) can be rendered against the write they belong to
struct BatchWrite {
	path: String,
	prop: String,
	value: Value,
}

impl Set {
	pub fn main(self) -> Result<()> {
		match &self.batch {
			Some(batch) => {
				let writes = self.read_batch(batch)?;

				self.apply_batch(writes)
			}
			None => self.apply_single(),
		}
	}

	/// The single-write path: exactly one `set` op (set.json's first form)
	fn apply_single(self) -> Result<()> {
		let (Some(path), Some(prop), Some(literal)) = (&self.path, &self.prop, &self.value) else {
			bail!(
				"`{}` needs --path, --prop and --value together, or --batch <file> instead",
				"wsync set".bold()
			);
		};

		// Pre-network, so a refused Parent write never opens a socket
		if prop.as_str() == "Parent" && !self.force_parent {
			return Err(parent_refusal(&format!("on {path}")));
		}

		let value = parse_literal(literal);

		// Also pre-network: a Name that breaks the path grammar is refused
		// before a socket is opened (the plugin refuses it again)
		if prop.as_str() == "Name" {
			if let Some(name) = value.as_str() {
				screen_name(name, &format!("on {path}"))?;
			}
		}

		if self.keep_going {
			wsync_warn!("--keep-going only affects a --batch run; a single write has nothing to keep going past");
		}

		let client = Client::connect(&self.targeting)?;

		// The `set` op carries no waypoint name (only `set_batch` does), so a
		// single write gets the name as a change-history marker *before* the
		// write: the plugin already seals every mutation into its own
		// `WSync: set "<path>"` waypoint, so one undo still reverses it — the
		// marker just gives the boundary the caller's own label. Best-effort,
		// exactly like Ro-Sync: a dropped marker costs undo labelling, never
		// the write
		if let Some(name) = &self.waypoint {
			mark_waypoint(&client, name);
		}

		let args = json!({
			"path": path,
			"prop": prop,
			"value": value,
			"forceParent": self.force_parent,
		});

		let value = client.value("set", args, self.raw)?;

		if self.raw {
			print_ok(&value);

			return Ok(());
		}

		let target = field(&value, "path");
		let target = if target.is_empty() { path.as_str() } else { target };

		// The readback is the value Studio actually kept, so a normalized
		// write (a clamped number, a coerced enum) is visible rather than
		// silently assumed
		match (value.get("value"), value.get("bytes").and_then(Value::as_u64)) {
			(Some(Value::Null) | None, Some(bytes)) => {
				wsync_info!("Set {}.{} ({bytes} bytes)", target.bold(), prop.bold());
			}
			(Some(readback), _) => {
				wsync_info!(
					"Set {}.{} to {}",
					target.bold(),
					prop.bold(),
					human_value(readback).bold()
				);
			}
			(None, None) => wsync_info!("Set {}.{}", target.bold(), prop.bold()),
		}

		Ok(())
	}

	/// Reads and screens a `--batch` file. Every failure here happens before
	/// the daemon is even resolved, so a rejected file leaves Studio untouched
	fn read_batch(&self, batch: &PathBuf) -> Result<Vec<BatchWrite>> {
		let text = fs::read_to_string(batch)
			.with_context(|| format!("Failed to read the --batch file {}", batch.display()))?;

		let parsed: Value = serde_json::from_str(&text)
			.with_context(|| format!("--batch file {} is not valid JSON", batch.display()))?;

		let Some(entries) = parsed.as_array() else {
			bail!(
				"--batch file {} must contain a JSON array of {{path, prop, value}} objects, not {}",
				batch.display(),
				kind_of(&parsed)
			);
		};

		if entries.is_empty() {
			bail!("--batch file {} contains no writes", batch.display());
		}

		let mut writes = Vec::with_capacity(entries.len());

		for (index, entry) in entries.iter().enumerate() {
			// 1-based: the number a human counts to in the file
			let number = index + 1;

			if !entry.is_object() {
				bail!(
					"--batch entry {number} must be an object with `path`, `prop` and `value`, not {}",
					kind_of(entry)
				);
			}

			let (Some(path), Some(prop)) = (
				entry.get("path").and_then(Value::as_str),
				entry.get("prop").and_then(Value::as_str),
			) else {
				bail!("--batch entry {number} needs string `path` and `prop` fields");
			};

			// The whole file is rejected on one guarded entry (set.json), and
			// the screen runs over every entry first so the message names the
			// offender rather than "somewhere in this file". Unconditional:
			// the `set_batch` op has no Parent escape (the plugin refuses the
			// whole batch whatever the caller forced), so not even
			// `--force-parent` turns this into a doomed network call
			if prop == "Parent" {
				return Err(batch_parent_refusal(number, path));
			}

			// The Name screen runs over the whole file too: an entry that would
			// strand its instance outside the path grammar rejects the batch
			// before any network call, exactly like a Parent entry
			if prop == "Name" {
				if let Some(name) = entry.get("value").and_then(Value::as_str) {
					screen_name(name, &format!("in entry {number} (on {path})"))?;
				}
			}

			writes.push(BatchWrite {
				path: path.to_owned(),
				prop: prop.to_owned(),
				value: entry.get("value").cloned().unwrap_or(Value::Null),
			});
		}

		Ok(writes)
	}

	/// The batch path: exactly one `set_batch` op, whose per-entry results
	/// come back positionally against the writes that were sent. Parent
	/// writes cannot reach here — [`Self::read_batch`] refuses them
	/// unconditionally
	fn apply_batch(&self, writes: Vec<BatchWrite>) -> Result<()> {
		let mut args = json!({
			"writes": writes
				.iter()
				.map(|write| json!({ "path": write.path, "prop": write.prop, "value": write.value }))
				.collect::<Vec<_>>(),
			"keepGoing": self.keep_going,
		});

		if let Some(name) = &self.waypoint {
			args["waypoint"] = json!(name);
		}

		let client = Client::connect(&self.targeting)?;
		let value = client.value("set_batch", args, self.raw)?;

		let empty = Vec::new();
		let results = value.get("results").and_then(Value::as_array).unwrap_or(&empty);
		let failed = value
			.get("failed")
			.and_then(Value::as_u64)
			.unwrap_or_else(|| results.iter().filter(|result| !result_ok(result)).count() as u64);

		if self.raw {
			print_ok(&value);
		} else {
			report_batch(&writes, results, &value);
		}

		// A partially applied batch is a failed command whether or not
		// `--keep-going` let the run continue: `--keep-going` decides how far
		// the batch gets, never whether the caller is told it broke
		if failed > 0 {
			bail!(
				"{failed} of {} batch write(s) failed",
				value
					.get("total")
					.and_then(Value::as_u64)
					.unwrap_or(writes.len() as u64)
			);
		}

		Ok(())
	}
}

fn result_ok(result: &Value) -> bool {
	result.get("ok").and_then(Value::as_bool) == Some(true)
}

/// The `set Parent` refusal (set.json, Design §10.6): loud, because a raw
/// reparent is the single most common way to corrupt a DataModel, and it
/// always names `wsync mv`. The banner carries the explanation; the returned
/// error is the one line a machine caller reads before the non-zero exit
fn parent_refusal(location: &str) -> anyhow::Error {
	let banner = "─".repeat(72);

	eprintln!("{}", banner.red());
	eprintln!("  {}", "wsync set: refusing to assign .Parent".red().bold());
	eprintln!();
	eprintln!("  Reparenting through a raw property write is the single most");
	eprintln!("  common way to corrupt a DataModel. Reparent safely with");
	eprintln!("  `{}`, or re-run", "wsync mv --from <path> --to <parent>".bold());
	eprintln!(
		"  with `{}` when the raw assignment really is intentional.",
		"--force-parent".bold()
	);
	eprintln!("{}", banner.red());

	anyhow::anyhow!("Refusing to set .Parent {location} without --force-parent — use `wsync mv` instead")
}

/// The batched `set Parent` refusal (set.json): unconditional, because the
/// `set_batch` op has no Parent escape — the plugin would refuse the whole
/// batch whatever the CLI forwarded, so the CLI never forwards it. The
/// message names the offending entry, the safe route, and the only real
/// escape (a single forced `set`)
fn batch_parent_refusal(number: usize, path: &str) -> anyhow::Error {
	let banner = "─".repeat(72);

	eprintln!("{}", banner.red());
	eprintln!(
		"  {}",
		"wsync set: refusing a --batch that assigns .Parent".red().bold()
	);
	eprintln!();
	eprintln!("  Entry {number} (on {path}) sets Parent, which a batch never may:");
	eprintln!("  the `set_batch` op has no Parent escape, and --force-parent does");
	eprintln!(
		"  not reach a batch. Reparent safely with `{}`,",
		"wsync mv --from <path> --to <parent>".bold()
	);
	eprintln!(
		"  or apply that one write on its own with `{}`.",
		"wsync set --prop Parent --force-parent".bold()
	);
	eprintln!("{}", banner.red());

	anyhow::anyhow!(
		"Refusing the batch: entry {number} (on {path}) sets .Parent, which a batch never may — use `wsync mv` \
		 or a single forced `wsync set`"
	)
}

/// The Name screen (set.json, new.json; the plugin refuses the same write
/// again — Remote/WriteRules.checkNameWrite): instance paths are
/// `/`-separated names with no escaping, so an empty or slashed Name would
/// leave the instance unaddressable by every path-based command afterwards —
/// only `wsync eval` could still reach it. Pre-network, like every other
/// guard in this file
fn screen_name(name: &str, location: &str) -> Result<()> {
	if name.is_empty() {
		bail!(
			"Refusing the empty instance name {location}: an empty segment cannot appear in a `/`-separated path, \
			 so nothing could address the instance afterwards"
		);
	}

	if name.contains('/') {
		bail!(
			"Refusing the instance name {} {location}: paths are `/`-separated with no escaping, so a name \
			 containing `/` could never be addressed by a path-based command",
			format!("{name:?}").bold()
		);
	}

	Ok(())
}

/// Best-effort change-history marker for a single `--waypoint` write
fn mark_waypoint(client: &Client, name: &str) {
	match client.request("waypoint", json!({ "name": name })) {
		Ok(envelope) if envelope.ok => {}
		Ok(envelope) => wsync_warn!("Could not set the `{name}` waypoint: {}", envelope.error_message()),
		Err(err) => wsync_warn!("Could not set the `{name}` waypoint: {err}"),
	}
}

/// Per-entry batch results as a table, joined positionally with the writes
/// that produced them
fn report_batch(writes: &[BatchWrite], results: &[Value], value: &Value) {
	println!(
		"{}",
		format!("{:>4}  {:<9} {:<40} {:<24} {}", "#", "RESULT", "PATH", "PROP", "DETAIL").bold()
	);

	for (index, write) in writes.iter().enumerate() {
		let (status, detail) = match results.get(index) {
			Some(result) if result_ok(result) => ("ok", String::new()),
			Some(result) if result.get("skipped").and_then(Value::as_bool) == Some(true) => {
				("skipped", "not attempted — the batch stopped earlier".to_owned())
			}
			Some(result) => (
				"failed",
				match result.get("error") {
					Some(error) => format!("{}: {}", field(error, "code"), field(error, "message")),
					None => "the plugin reported failure without detail".to_owned(),
				},
			),
			// Fewer results than writes: reported rather than silently
			// rendered as success
			None => ("unknown", "the plugin returned no result for this write".to_owned()),
		};

		// Padded before coloring: escape codes count towards a format
		// width and would knock the column out of alignment
		let cell = format!("{status:<9}");
		let painted = match status {
			"ok" => cell.green(),
			"failed" => cell.red(),
			"skipped" => cell.dimmed(),
			_ => cell.yellow(),
		};

		println!(
			"{:>4}  {painted} {:<40} {:<24} {detail}",
			index + 1,
			clip(&write.path, 40),
			clip(&write.prop, 24),
		);
	}

	let count = |key: &str| value.get(key).and_then(Value::as_u64);

	println!(
		"\n{} applied, {} failed of {} write(s){}",
		count("applied").unwrap_or(0),
		count("failed").unwrap_or(0),
		count("total").unwrap_or(writes.len() as u64),
		if value.get("stopped").and_then(Value::as_bool) == Some(true) {
			" — the batch stopped at the first failure (pass --keep-going to continue)"
		} else {
			""
		}
	);
}

// ---------------------------------------------------------------------------
// new / rm / mv
// ---------------------------------------------------------------------------

/// Create a new instance under a live Studio parent
#[derive(Parser)]
pub struct New {
	#[command(flatten)]
	targeting: Targeting,

	/// Parent Studio path, `/`-separated
	#[arg(long, value_name = "PARENT-PATH")]
	path: String,

	/// Class to instantiate (e.g. `Part`, `RemoteEvent`)
	#[arg(long, value_name = "CLASS")]
	class: String,

	/// Name for the created instance (defaults to the class default)
	#[arg(long, value_name = "NAME")]
	name: Option<String>,

	/// JSON object of property writes to apply on creation
	#[arg(long, value_name = "JSON-OBJECT")]
	props: Option<String>,

	/// Print machine-readable JSON
	#[arg(long)]
	raw: bool,
}

impl New {
	pub fn main(self) -> Result<()> {
		// Validated before connecting: a malformed --props is a usage error,
		// not a half-built instance in Studio
		let props = match &self.props {
			Some(text) => {
				let parsed: Value =
					serde_json::from_str(text).context("--props must be a JSON object of property writes")?;

				if !parsed.is_object() {
					bail!(
						"--props must be a JSON object keyed by property name, not {}",
						kind_of(&parsed)
					);
				}

				Some(parsed)
			}
			None => None,
		};

		// Both name routes are screened pre-network (the plugin refuses them
		// again): a slashed or empty Name would strand the created instance
		// outside the `/`-separated path grammar the moment it exists
		if let Some(name) = &self.name {
			screen_name(name, "for --name")?;
		}

		if let Some(props) = &props {
			if let Some(name) = props.get("Name").and_then(Value::as_str) {
				screen_name(name, "for Name in --props")?;
			}
		}

		let mut args = json!({ "path": self.path, "class": self.class });

		if let Some(name) = &self.name {
			args["name"] = json!(name);
		}

		if let Some(props) = props {
			args["props"] = props;
		}

		let client = Client::connect(&self.targeting)?;
		let value = client.value("new", args, self.raw)?;

		if self.raw {
			print_ok(&value);

			return Ok(());
		}

		// The created path is the point of the command: it is what every
		// follow-up read or write addresses
		println!("{}", field(&value, "path"));

		wsync_info!(
			"Created {} {}",
			field(&value, "class").bold(),
			field(&value, "name").bold()
		);

		Ok(())
	}
}

/// Destroy a live Studio instance
#[derive(Parser)]
pub struct Rm {
	#[command(flatten)]
	targeting: Targeting,

	/// Studio path, `/`-separated
	#[arg(long, value_name = "STUDIO-PATH")]
	path: String,

	/// Print machine-readable JSON
	#[arg(long)]
	raw: bool,
}

impl Rm {
	pub fn main(self) -> Result<()> {
		let client = Client::connect(&self.targeting)?;
		let value = client.value("rm", json!({ "path": self.path }), self.raw)?;

		if self.raw {
			print_ok(&value);

			return Ok(());
		}

		let path = field(&value, "path");

		wsync_info!(
			"Destroyed {} {}",
			field(&value, "class").bold(),
			if path.is_empty() { self.path.as_str() } else { path }.bold()
		);

		Ok(())
	}
}

/// Reparent a live Studio instance
#[derive(Parser)]
pub struct Mv {
	#[command(flatten)]
	targeting: Targeting,

	/// Studio path of the instance to move
	#[arg(long, value_name = "STUDIO-PATH")]
	from: String,

	/// Studio path of the destination parent
	#[arg(long, value_name = "PARENT-PATH")]
	to: String,

	/// Allow a move across a top-level service boundary
	#[arg(long)]
	force: bool,

	/// Print machine-readable JSON
	#[arg(long)]
	raw: bool,
}

impl Mv {
	pub fn main(self) -> Result<()> {
		// The same first-segment rule the plugin applies
		// (WriteRules.crossesServiceBoundary), run here so a cross-service
		// refusal costs no socket at all (mv.json)
		let (from_service, to_service) = (service_of(&self.from), service_of(&self.to));

		if from_service != to_service && !self.force {
			bail!(
				"Moving {} from {} to {} crosses a top-level service boundary. \
				 Pass {} only when the cross-service move is intentional — it usually means losing replication",
				self.from.bold(),
				name_service(from_service).bold(),
				name_service(to_service).bold(),
				"--force".bold()
			);
		}

		let client = Client::connect(&self.targeting)?;
		let args = json!({ "from": self.from, "to": self.to, "force": self.force });
		let value = client.value("mv", args, self.raw)?;

		if self.raw {
			print_ok(&value);

			return Ok(());
		}

		wsync_info!(
			"Moved {} to {}",
			field(&value, "from").bold(),
			field(&value, "path").bold()
		);

		// A same-name sibling makes the reported path resolve ambiguously,
		// so every later command addressing it may hit the wrong instance
		if value.get("duplicateSiblings").and_then(Value::as_bool) == Some(true) {
			wsync_warn!(
				"The destination already had a child with this name — `{}` now resolves ambiguously",
				field(&value, "path")
			);
		}

		Ok(())
	}
}

/// The top-level service that owns a `/`-separated Studio path. The empty
/// path (the DataModel itself) owns the empty service, which never matches a
/// real one — a move to the DataModel root always counts as crossing
fn service_of(path: &str) -> &str {
	path.split('/').find(|segment| !segment.is_empty()).unwrap_or("")
}

fn name_service(service: &str) -> &str {
	if service.is_empty() {
		"the DataModel root"
	} else {
		service
	}
}

// ---------------------------------------------------------------------------
// attr
// ---------------------------------------------------------------------------

/// List, set, or remove attributes on a live Studio instance
#[derive(Parser)]
pub struct Attr {
	#[command(subcommand)]
	command: AttrCommand,
}

#[derive(Subcommand)]
enum AttrCommand {
	/// Set one attribute
	Set(AttrSet),
	/// Clear one attribute
	Rm(AttrRm),
	/// List every attribute on an instance (read-only)
	Ls(AttrLs),
}

#[derive(Parser)]
struct AttrSet {
	#[command(flatten)]
	targeting: Targeting,

	/// Studio path, `/`-separated
	#[arg(long, value_name = "STUDIO-PATH")]
	path: String,

	/// Attribute name
	#[arg(long, value_name = "ATTRIBUTE")]
	name: String,

	/// Value as a JSON literal or tagged value; bare text that is not valid
	/// JSON is taken as a string
	#[arg(long, value_name = "JSON")]
	value: String,

	/// Print machine-readable JSON
	#[arg(long)]
	raw: bool,
}

#[derive(Parser)]
struct AttrRm {
	#[command(flatten)]
	targeting: Targeting,

	/// Studio path, `/`-separated
	#[arg(long, value_name = "STUDIO-PATH")]
	path: String,

	/// Attribute name
	#[arg(long, value_name = "ATTRIBUTE")]
	name: String,

	/// Print machine-readable JSON
	#[arg(long)]
	raw: bool,
}

#[derive(Parser)]
struct AttrLs {
	#[command(flatten)]
	targeting: Targeting,

	/// Studio path, `/`-separated
	#[arg(long, value_name = "STUDIO-PATH")]
	path: String,

	/// Print machine-readable JSON
	#[arg(long)]
	raw: bool,
}

impl Attr {
	pub fn main(self) -> Result<()> {
		match self.command {
			AttrCommand::Set(command) => command.main(),
			AttrCommand::Rm(command) => command.main(),
			AttrCommand::Ls(command) => command.main(),
		}
	}
}

impl AttrSet {
	fn main(self) -> Result<()> {
		let client = Client::connect(&self.targeting)?;

		let args = json!({
			"path": self.path,
			"name": self.name,
			"value": parse_literal(&self.value),
		});

		let value = client.value("set_attr", args, self.raw)?;

		if self.raw {
			print_ok(&value);

			return Ok(());
		}

		wsync_info!(
			"Set attribute {} on {}",
			field(&value, "name").bold(),
			field(&value, "path").bold()
		);

		Ok(())
	}
}

impl AttrRm {
	fn main(self) -> Result<()> {
		let client = Client::connect(&self.targeting)?;
		let args = json!({ "path": self.path, "name": self.name });
		let value = client.value("rm_attr", args, self.raw)?;

		if self.raw {
			print_ok(&value);

			return Ok(());
		}

		wsync_info!(
			"Cleared attribute {} on {}",
			field(&value, "name").bold(),
			field(&value, "path").bold()
		);

		Ok(())
	}
}

impl AttrLs {
	fn main(self) -> Result<()> {
		let client = Client::connect(&self.targeting)?;
		let value = client.value("attr_ls", json!({ "path": self.path }), self.raw)?;

		if self.raw {
			print_ok(&value);

			return Ok(());
		}

		// `attributes` is omitted entirely when the instance has none (the
		// plugin avoids JSON's empty-table ambiguity), so an absent key is
		// "no attributes", never a malformed answer
		if let Some(attributes) = value.get("attributes").and_then(Value::as_object) {
			let mut names: Vec<&String> = attributes.keys().collect();
			names.sort();

			for name in names {
				println!("  {name:<28} {}", human_value(&attributes[name]));
			}
		}

		println!(
			"\n{} attribute(s) on {}",
			value.get("count").and_then(Value::as_u64).unwrap_or(0),
			field(&value, "path")
		);

		Ok(())
	}
}

// ---------------------------------------------------------------------------
// tag
// ---------------------------------------------------------------------------

/// Add or remove CollectionService tags on a live Studio instance
#[derive(Parser)]
pub struct Tag {
	#[command(subcommand)]
	command: TagCommand,
}

// tag.json documents exactly two subcommands. The plugin also implements a
// read-only `tag_ls` op, but the registry does not expose it as a
// subcommand, so neither does the CLI — `wsync get --path <p>` already
// prints an instance's tags
#[derive(Subcommand)]
enum TagCommand {
	/// Add a tag (idempotent)
	Add(TagAdd),
	/// Remove a tag
	Rm(TagRm),
}

#[derive(Parser)]
struct TagAdd {
	#[command(flatten)]
	targeting: Targeting,

	/// Studio path, `/`-separated
	#[arg(long, value_name = "STUDIO-PATH")]
	path: String,

	/// Tag name
	#[arg(long, value_name = "TAG")]
	tag: String,

	/// Print machine-readable JSON
	#[arg(long)]
	raw: bool,
}

#[derive(Parser)]
struct TagRm {
	#[command(flatten)]
	targeting: Targeting,

	/// Studio path, `/`-separated
	#[arg(long, value_name = "STUDIO-PATH")]
	path: String,

	/// Tag name
	#[arg(long, value_name = "TAG")]
	tag: String,

	/// Print machine-readable JSON
	#[arg(long)]
	raw: bool,
}

impl Tag {
	pub fn main(self) -> Result<()> {
		match self.command {
			TagCommand::Add(command) => command.main(),
			TagCommand::Rm(command) => command.main(),
		}
	}
}

impl TagAdd {
	fn main(self) -> Result<()> {
		let client = Client::connect(&self.targeting)?;
		let args = json!({ "path": self.path, "tag": self.tag });
		let value = client.value("add_tag", args, self.raw)?;

		if self.raw {
			print_ok(&value);

			return Ok(());
		}

		// Idempotent by contract: re-tagging is a success that changed
		// nothing, and saying so keeps a no-op distinguishable from a write
		if value.get("already").and_then(Value::as_bool) == Some(true) {
			wsync_info!(
				"{} already carries the tag {}",
				field(&value, "path").bold(),
				field(&value, "tag").bold()
			);

			return Ok(());
		}

		wsync_info!(
			"Tagged {} with {}",
			field(&value, "path").bold(),
			field(&value, "tag").bold()
		);

		Ok(())
	}
}

impl TagRm {
	fn main(self) -> Result<()> {
		let client = Client::connect(&self.targeting)?;
		let args = json!({ "path": self.path, "tag": self.tag });
		let value = client.value("rm_tag", args, self.raw)?;

		if self.raw {
			print_ok(&value);

			return Ok(());
		}

		wsync_info!(
			"Removed the tag {} from {}",
			field(&value, "tag").bold(),
			field(&value, "path").bold()
		);

		Ok(())
	}
}

// ---------------------------------------------------------------------------
// call / eval
// ---------------------------------------------------------------------------

/// Invoke a method on a live Studio instance
#[derive(Parser)]
pub struct Call {
	#[command(flatten)]
	targeting: Targeting,

	/// Studio path, `/`-separated
	#[arg(long, value_name = "STUDIO-PATH")]
	path: String,

	/// Method to invoke
	#[arg(long, value_name = "METHOD")]
	method: String,

	/// JSON array of arguments, using the same tagged-value codec as
	/// `wsync set --value`
	#[arg(long, value_name = "JSON-ARRAY")]
	args: Option<String>,

	/// Print machine-readable JSON
	#[arg(long)]
	raw: bool,
}

impl Call {
	pub fn main(self) -> Result<()> {
		// Validated before connecting: a malformed --args never reaches a
		// method that may have side effects
		let call_args = match &self.args {
			Some(text) => {
				let parsed: Value =
					serde_json::from_str(text).context("--args must be a JSON array, e.g. '[\"Box\"]'")?;

				if !parsed.is_array() {
					bail!("--args must be a JSON array, not {}", kind_of(&parsed));
				}

				Some(parsed)
			}
			None => None,
		};

		let mut args = json!({ "path": self.path, "method": self.method });

		if let Some(call_args) = call_args {
			args["args"] = call_args;
		}

		let client = Client::connect(&self.targeting)?;
		let value = client.value("call", args, self.raw)?;

		if self.raw {
			print_ok(&value);

			return Ok(());
		}

		print_results(
			&value,
			&format!("{}:{}", field(&value, "path"), field(&value, "method")),
		);

		Ok(())
	}
}

/// Execute Luau inside Studio through the plugin sandbox
#[derive(Parser)]
pub struct Eval {
	#[command(flatten)]
	targeting: Targeting,

	/// Luau source; use `return …` when you need a value back
	#[arg(long, value_name = "LUAU")]
	source: String,

	/// Print machine-readable JSON
	#[arg(long)]
	raw: bool,
}

impl Eval {
	pub fn main(self) -> Result<()> {
		let client = Client::connect(&self.targeting)?;
		let value = client.value("eval", json!({ "source": self.source }), self.raw)?;

		if self.raw {
			print_ok(&value);

			return Ok(());
		}

		print_results(&value, "eval");

		Ok(())
	}
}

/// Shared rendering for the `{count, results?}` shape `call` and `eval`
/// answer with. `results` is omitted when nothing was returned, so an absent
/// key means "no return values", never a malformed answer
fn print_results(value: &Value, label: &str) {
	let empty = Vec::new();
	let results = value.get("results").and_then(Value::as_array).unwrap_or(&empty);

	for (index, result) in results.iter().enumerate() {
		// Multi-return is an array; numbering keeps the positions readable
		println!("{:>3}  {}", index + 1, human_value(result));
	}

	let count = value
		.get("count")
		.and_then(Value::as_u64)
		.unwrap_or(results.len() as u64);

	if count == 0 {
		wsync_info!("{label} returned no values");
	} else {
		wsync_info!("{label} returned {count} value(s)");
	}
}
