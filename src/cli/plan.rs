//! `plan` — a read-only JSON plan for a mutating command, without executing
//! it (plan.json; Design §10.5).
//!
//! Each subcommand accepts exactly the flags of the command it explains,
//! validates them the same way (a malformed `--value` fails here exactly as
//! it would there), and prints `{mutates, requires, risks, executeCommand}`
//! without connecting to anything — no daemon, no plugin, no filesystem
//! writes. The output is deterministic: two identical invocations print
//! identical bytes, so plans diff cleanly.
//!
//! `plan` is a dry-run explanation offered before a risky write; the
//! registry is explicit that it is not a mandatory ritual before every
//! mutation.

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use serde_json::{json, Value};
use std::path::PathBuf;

use crate::cli::client::kind_of;

/// Build a read-only JSON plan for a mutating command without executing it
#[derive(Parser)]
pub struct Plan {
	#[command(subcommand)]
	command: PlanCommand,
}

#[derive(Subcommand)]
enum PlanCommand {
	/// Plan a Studio property write
	Set(PlanSet),
	/// Plan creating a new instance
	New(PlanNew),
	/// Plan destroying an instance
	Rm(PlanRm),
	/// Plan reparenting an instance
	Mv(PlanMv),
	/// Plan resolving a parked conflict
	Resolve(PlanResolve),
}

impl Plan {
	pub fn main(self) -> Result<()> {
		let plan = match self.command {
			PlanCommand::Set(command) => command.plan()?,
			PlanCommand::New(command) => command.plan()?,
			PlanCommand::Rm(command) => command.plan(),
			PlanCommand::Mv(command) => command.plan(),
			PlanCommand::Resolve(command) => command.plan()?,
		};

		println!("{}", serde_json::to_string_pretty(&plan)?);

		Ok(())
	}
}

#[derive(Parser)]
struct PlanSet {
	/// Studio path, `/`-separated
	#[arg(long, value_name = "STUDIO-PATH")]
	path: String,

	/// Property name
	#[arg(long, value_name = "PROPERTY")]
	prop: String,

	/// Value as a JSON literal or tagged value
	#[arg(long, value_name = "JSON")]
	value: String,
}

impl PlanSet {
	fn plan(self) -> Result<Value> {
		// The same codec `wsync set` applies: a JSON literal, with bare text
		// falling back to a string — so the plan and the execution agree on
		// what the value will be
		let value: Value = serde_json::from_str(&self.value).unwrap_or_else(|_| Value::String(self.value.clone()));

		let mut risks = Vec::new();

		if self.prop == "Parent" {
			risks.push(
				"raw Parent writes are refused by `wsync set` — use `wsync mv`, or `--force-parent` \
				 for an intentional single raw assignment"
					.to_owned(),
			);
		}

		Ok(plan_record(
			"set",
			json!({ "path": self.path, "prop": self.prop, "value": value }),
			&["studio"],
			&["daemon", "studio-plugin"],
			risks,
			format!(
				"wsync set --path {} --prop {} --value {}",
				shell_quote(&self.path),
				shell_quote(&self.prop),
				shell_quote(&self.value)
			),
		))
	}
}

#[derive(Parser)]
struct PlanNew {
	/// Parent Studio path
	#[arg(long, value_name = "PARENT-PATH")]
	path: String,

	/// Class to instantiate
	#[arg(long, value_name = "CLASS")]
	class: String,

	/// Name for the created instance
	#[arg(long, value_name = "NAME")]
	name: Option<String>,

	/// JSON object of property writes to apply on creation
	#[arg(long, value_name = "JSON-OBJECT")]
	props: Option<String>,
}

impl PlanNew {
	fn plan(self) -> Result<Value> {
		let props = match self.props.as_deref() {
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

		let mut command = format!(
			"wsync new --path {} --class {}",
			shell_quote(&self.path),
			shell_quote(&self.class)
		);

		if let Some(name) = &self.name {
			command.push_str(&format!(" --name {}", shell_quote(name)));
		}

		if let Some(props) = &self.props {
			command.push_str(&format!(" --props {}", shell_quote(props)));
		}

		Ok(plan_record(
			"new",
			json!({ "parentPath": self.path, "class": self.class, "name": self.name, "props": props }),
			&["studio"],
			&["daemon", "studio-plugin"],
			Vec::new(),
			command,
		))
	}
}

#[derive(Parser)]
struct PlanRm {
	/// Studio path, `/`-separated
	#[arg(long, value_name = "STUDIO-PATH")]
	path: String,
}

impl PlanRm {
	fn plan(self) -> Value {
		plan_record(
			"rm",
			json!({ "path": self.path }),
			&["studio"],
			&["daemon", "studio-plugin"],
			vec!["destructive: destroys the target instance in Studio".to_owned()],
			format!("wsync rm --path {}", shell_quote(&self.path)),
		)
	}
}

#[derive(Parser)]
struct PlanMv {
	/// Studio path of the instance to move
	#[arg(long, value_name = "STUDIO-PATH")]
	from: String,

	/// Studio path of the destination parent
	#[arg(long, value_name = "PARENT-PATH")]
	to: String,

	/// Allow a move across a top-level service boundary
	#[arg(long)]
	force: bool,
}

impl PlanMv {
	fn plan(self) -> Value {
		let mut risks = Vec::new();

		// The same first-segment rule `wsync mv` refuses on, surfaced as the
		// plan's risk rather than discovered at execution time
		if service_of(&self.from) != service_of(&self.to) {
			risks.push(if self.force {
				"crosses a top-level service boundary (forced) — this usually means losing replication".to_owned()
			} else {
				"crosses a top-level service boundary — `wsync mv` will refuse this without --force".to_owned()
			});
		}

		let mut command = format!(
			"wsync mv --from {} --to {}",
			shell_quote(&self.from),
			shell_quote(&self.to)
		);

		if self.force {
			command.push_str(" --force");
		}

		plan_record(
			"mv",
			json!({ "from": self.from, "to": self.to, "force": self.force }),
			&["studio"],
			&["daemon", "studio-plugin"],
			risks,
			command,
		)
	}
}

#[derive(Parser)]
struct PlanResolve {
	/// Filesystem path the conflicting instance projects to
	#[arg(long, value_name = "FILESYSTEM-PATH")]
	path: PathBuf,

	/// Push the on-disk state back to Studio
	#[arg(long, conflicts_with = "studio")]
	disk: bool,

	/// Keep the Studio state and write it to disk
	#[arg(long, conflicts_with = "disk")]
	studio: bool,
}

impl PlanResolve {
	fn plan(self) -> Result<Value> {
		let choice = match (self.disk, self.studio) {
			(true, false) => "disk",
			(false, true) => "studio",
			_ => bail!("`wsync plan resolve` needs exactly one of --disk or --studio"),
		};

		let path = self.path.to_string_lossy().into_owned();

		Ok(plan_record(
			"resolve",
			json!({ "path": path, "choice": choice }),
			&["disk", "studio"],
			&["daemon", "parked-conflict"],
			vec![format!(
				"overwrites the {} side of the conflict — the discarded content survives only in backups",
				if choice == "disk" { "Studio" } else { "disk" }
			)],
			format!("wsync resolve --path {} --{choice}", shell_quote(&path)),
		))
	}
}

/// The plan shape (plan.json): `mutates`, `requires`, `risks`, and an
/// `executeCommand` string, marked read-only. Deliberately timestamp-free so
/// two identical plans are byte-identical
fn plan_record(
	operation: &str,
	args: Value,
	mutates: &[&str],
	requires: &[&str],
	risks: Vec<String>,
	command: String,
) -> Value {
	json!({
		"ok": true,
		"schema": "wsync.plan.v1",
		"readOnly": true,
		"operation": operation,
		"args": args,
		"mutates": mutates,
		"requires": requires,
		"risks": risks,
		"executeCommand": command,
		"notes": [
			"This plan does not execute anything.",
			"Review `mutates`, `requires`, and `risks` before running `executeCommand`.",
		],
	})
}

/// The top-level service that owns a `/`-separated Studio path (the same
/// first-segment rule `wsync mv` applies)
fn service_of(path: &str) -> &str {
	path.split('/').find(|segment| !segment.is_empty()).unwrap_or("")
}

fn shell_quote(value: &str) -> String {
	format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn shell_quoting_survives_embedded_quotes() {
		assert_eq!(shell_quote("plain"), "'plain'");
		assert_eq!(shell_quote("it's"), "'it'\\''s'");
	}

	#[test]
	fn plans_carry_the_documented_shape() {
		let plan = PlanRm {
			path: "Workspace/OldPart".into(),
		}
		.plan();

		assert_eq!(plan["ok"], true);
		assert_eq!(plan["readOnly"], true);
		assert_eq!(plan["mutates"], json!(["studio"]));
		assert!(plan["risks"][0].as_str().unwrap().contains("destructive"));
		assert_eq!(plan["executeCommand"], "wsync rm --path 'Workspace/OldPart'");
	}

	#[test]
	fn cross_service_moves_carry_the_risk() {
		let plan = PlanMv {
			from: "Workspace/A".into(),
			to: "ReplicatedStorage".into(),
			force: false,
		}
		.plan();

		assert!(plan["risks"][0].as_str().unwrap().contains("--force"));

		let plan = PlanMv {
			from: "Workspace/A".into(),
			to: "Workspace/B".into(),
			force: false,
		}
		.plan();

		assert_eq!(plan["risks"].as_array().unwrap().len(), 0);
	}
}
