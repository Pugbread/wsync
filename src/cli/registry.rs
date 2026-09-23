//! Command registry surface: `commands` and `context` (commands.json,
//! context.json; Design §10.5–10.6).
//!
//! `commands` is the LLM-facing replacement for scraping `--help`: it prints
//! the embedded registry bundle (or one entry, or a compact index) and never
//! needs a daemon. `context` is the compact machine snapshot — project
//! config, synced roots, daemon/plugin status, conflict count, generated-doc
//! presence, and the `llmPolicy` block that keeps the command budget in front
//! of agents that read nothing else. Both commands degrade cleanly with no
//! daemon: `commands` never asks for one, and `context` reports offline
//! status from config and disk facts alone.

use anyhow::{bail, Result};
use clap::{CommandFactory, Parser};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{fs, path::PathBuf};

use crate::{
	cli::client::{print_json, Client, Target, Targeting},
	cli::registry_bundle,
	ext::PathExt,
	project::{Project, ProjectNode},
};

// ---------------------------------------------------------------------------
// commands
// ---------------------------------------------------------------------------

/// Print compact command discovery, focused command help, or offline value schemas
#[derive(Parser)]
pub struct Commands {
	/// Command and optional nested subcommand; `types [TypeName]` discovers JSON values
	#[arg(value_name = "COMMAND", num_args = 0..)]
	name: Vec<String>,

	/// Print only choosing fields (the default for the command index)
	#[arg(long, conflicts_with = "full")]
	compact: bool,

	/// Explicitly print the complete embedded registry
	#[arg(long)]
	full: bool,

	/// Accepted for consistent project-scoped invocations; discovery remains offline
	#[arg(long, value_name = "PATH")]
	project: Option<PathBuf>,
}

impl Commands {
	pub fn main(self) -> Result<()> {
		let names: Vec<&str> = self.name.iter().flat_map(|name| name.split_whitespace()).collect();
		if self.full {
			if !names.is_empty() {
				bail!("Use `wsync commands --full` without a command name");
			}
			print!("{}", registry_bundle::CLIENT_COMMANDS);
			if !registry_bundle::CLIENT_COMMANDS.ends_with('\n') {
				println!();
			}
			return Ok(());
		}
		let Some(bundle) = registry_bundle::parsed() else {
			bail!("The embedded command registry does not parse — this binary was built from a broken docs bundle");
		};
		if names
			.first()
			.is_some_and(|name| matches!(*name, "types" | "values" | "value-types"))
		{
			let payload = match names.as_slice() {
				[_] => super::value_schema::catalog(),
				[_, name] => super::value_schema::lookup(name).ok_or_else(|| {
					anyhow::anyhow!("Unknown value type `{name}` — run `wsync commands types` for supported tags")
				})?,
				_ => bail!("Usage: wsync commands types [TypeName]"),
			};
			println!("{}", serde_json::to_string_pretty(&payload)?);
			return Ok(());
		}
		let empty = Vec::new();
		let commands = bundle.get("commands").and_then(Value::as_array).unwrap_or(&empty);
		if let Some(name) = names.first() {
			let Some(entry) = commands
				.iter()
				.find(|entry| entry.get("name").and_then(Value::as_str) == Some(name))
			else {
				bail!("No command named `{name}` in the registry — run `wsync commands --compact` for the index");
			};
			let payload = if names.len() > 1 {
				{
					let nested = nested_entry(entry, &names)?;
					if self.compact {
						compact_entry(&nested)
					} else {
						nested
					}
				}
			} else if self.compact {
				compact_entry(entry)
			} else {
				entry.clone()
			};
			println!("{}", serde_json::to_string_pretty(&payload)?);
			return Ok(());
		}
		println!("{}", serde_json::to_string_pretty(&compact_index(&bundle, commands))?);
		Ok(())
	}
}

/// Follow the real clap tree, so nested discovery cannot describe an invented path.
fn nested_entry(entry: &Value, path: &[&str]) -> Result<Value> {
	let mut command = super::Cli::command();
	command.build();
	let mut parent_options = Vec::new();
	for (index, part) in path.iter().enumerate() {
		if index > 0 {
			for arg in command.get_arguments().filter(|arg| !arg.is_global_set()) {
				parent_options.push(json!({ "scope": format!("wsync {}", path[..index].join(" ")), "long": arg.get_long(), "short": arg.get_short(), "help": arg.get_help().map(ToString::to_string), "position": "before the next subcommand" }));
			}
		}
		let next_command = command
			.get_subcommands()
			.find(|child| child.get_name() == *part)
			.cloned()
			.ok_or_else(|| {
				anyhow::anyhow!(
					"No subcommand `{}` — run `wsync commands {}` for valid commands",
					path.join(" "),
					path[0]
				)
			})?;
		command = next_command;
	}
	let invocation = format!("wsync {}", path.join(" "));
	command = command.bin_name(&invocation);
	let subpath = path[1..].join(" ");
	let examples: Vec<Value> = entry
		.get("examples")
		.and_then(Value::as_array)
		.into_iter()
		.flatten()
		.filter(|example| {
			example
				.as_str()
				.is_some_and(|example| example == invocation || example.starts_with(&format!("{invocation} ")))
		})
		.cloned()
		.collect();
	Ok(json!({
		"name": path.join(" "),
		"category": entry.get("category"),
		"description": command.get_about().map(ToString::to_string),
		"usage": command.render_usage().to_string().trim_start_matches("Usage: "),
		"help": command.render_long_help().to_string(),
		"effects": entry.get("subcommandMetadata").and_then(|metadata| metadata.get(&subpath)),
		"subcommands": command.get_subcommands().filter(|child| child.get_name() != "help").map(|child| child.get_name()).collect::<Vec<_>>(),
		"examples": examples,
		"parentOptions": parent_options,
		"notes": entry.get("notes"),
		"registryIdentity": registry_identity(),
	}))
}

/// No network or runtime probing: identifies the exact embedded schema and build.
fn registry_identity() -> Value {
	json!({
		"cliVersion": env!("CARGO_PKG_VERSION"),
		"commit": env!("WSYNC_BUILD_COMMIT"),
		"dirty": env!("WSYNC_BUILD_DIRTY") == "true",
		"schemaVersion": registry_bundle::parsed().and_then(|bundle| bundle.get("schemaVersion").cloned()),
		"commandFingerprint": format!("sha256:{:x}", Sha256::digest(registry_bundle::CLIENT_COMMANDS.as_bytes())),
		"valueFingerprint": format!("sha256:{:x}", Sha256::digest(include_str!("../../docs/value-types.json").as_bytes())),
	})
}

/// One command reduced to the fields that pick it out of a family
fn compact_entry(entry: &Value) -> Value {
	json!({
		"name": entry.get("name"),
		"category": entry.get("category"),
		"description": entry.get("description"),
		"usage": entry.get("usage"),
	})
}

/// The compact index: name + one-line description, grouped by category in the
/// bundle's own category order (commands.json)
fn compact_index(bundle: &Value, commands: &[Value]) -> Value {
	let empty = Vec::new();
	let categories = bundle.get("categories").and_then(Value::as_array).unwrap_or(&empty);

	// The bundle's declared order first, then any category an entry names
	// that the header list missed — nothing silently dropped
	let mut order: Vec<&str> = categories.iter().filter_map(Value::as_str).collect();

	for entry in commands {
		if let Some(category) = entry.get("category").and_then(Value::as_str) {
			if !order.contains(&category) {
				order.push(category);
			}
		}
	}

	let grouped: Vec<Value> = order
		.iter()
		.filter_map(|category| {
			let members: Vec<Value> = commands
				.iter()
				.filter(|entry| entry.get("category").and_then(Value::as_str) == Some(*category))
				.map(|entry| {
					json!({
						"name": entry.get("name"),
						"description": entry.get("description"),
					})
				})
				.collect();

			// A category with no commands is header noise, not information
			(!members.is_empty()).then(|| json!({ "category": category, "commands": members }))
		})
		.collect();

	json!({
		"total": commands.len(),
		"hint": "Use `wsync commands <name> [subcommand...]` for focused usage; `wsync commands types [TypeName]` for value schemas; `wsync commands --full` for the full registry",
		"registryIdentity": registry_identity(),
		"categories": grouped,
	})
}

// ---------------------------------------------------------------------------
// context
// ---------------------------------------------------------------------------

/// Print a compact LLM-oriented project context snapshot as JSON
#[derive(Parser)]
pub struct Context {
	#[command(flatten)]
	targeting: Targeting,

	/// Embed the complete command registry instead of registry pointers
	#[arg(long = "full-commands")]
	full_commands: bool,

	/// Print the snapshot as one JSON line instead of pretty-printed
	#[arg(long)]
	raw: bool,
}

impl Context {
	pub fn main(self) -> Result<()> {
		let target = Target::resolve(&self.targeting)?;

		let project = if target.project_exists {
			Project::load(&target.project_path).ok()
		} else {
			None
		};

		// Everything live is optional: `context` must answer from config and
		// disk facts alone when nothing is running (context.json)
		let hello = target.probe().ok();
		let client = Client::open_probed(&target, hello.clone());

		let served_project_matches = match (&hello, &target.canonical) {
			(Some(hello), Some(canonical)) => Some(&hello.canonical_project == canonical),
			_ => None,
		};

		let daemon = match &hello {
			Some(hello) => json!({
				"reachable": true,
				"port": target.port,
				"portSource": target.port_source.as_str(),
				"version": hello.version,
				"protocol": hello.protocol,
				"pid": hello.pid,
				"managedBy": hello.managed_by,
				// `false` is the mismatch warning: the answering daemon serves a
				// different project than the one this snapshot describes
				"servesThisProject": served_project_matches,
				"servedProject": hello.project,
			}),
			None => json!({
				"reachable": false,
				"port": target.port,
				"portSource": target.port_source.as_str(),
				"note": "no daemon answers — start one with `wsync daemon start --project <path>`",
			}),
		};

		let plugin = match &client {
			Some(client) => match client.request("version", json!({})) {
				Ok(envelope) if envelope.ok => json!({
					"connected": true,
					"version": envelope.value.get("pluginVersion"),
					"studioVersion": envelope.value.get("studioVersion"),
				}),
				Ok(envelope) => json!({ "connected": false, "reason": envelope.error_message() }),
				Err(_) => json!({ "connected": false, "reason": "the daemon did not answer the version op" }),
			},
			None => json!({ "connected": false, "reason": "no daemon to ask" }),
		};

		// Read-only backlog snapshot; unavailable/mismatched daemon remains unknown.
		let backlog = client.as_ref().and_then(|client| {
			let endpoint = client.get_endpoint("/backlog").ok()?;
			if endpoint.unavailable() {
				return None;
			}
			endpoint.json.as_ref()?.get("entries")?.as_array().map(Vec::len)
		});

		let workspace = target.project_path.get_parent();
		let generated = |file: &str| workspace.join(file).exists();

		let commands = if self.full_commands {
			registry_bundle::parsed()
				.unwrap_or_else(|| json!({ "error": "the embedded command registry does not parse in this build" }))
		} else {
			json!({
				"index": "wsync commands --compact",
				"command": "wsync commands <name>",
				"fullRegistry": "wsync commands --full",
				"valueSchemas": "wsync commands types [TypeName]",
			})
		};

		let snapshot = json!({
			"ok": true,
			"project": {
				"path": target.project_path.to_string(),
				"canonicalPath": target.canonical,
				"exists": target.project_exists,
				"parses": project.is_some(),
				"name": project.as_ref().map(|project| project.name.clone()),
				"gameId": project.as_ref().and_then(|project| project.game_id),
				"placeIds": project.as_ref().map(|project| project.place_ids.clone()),
			},
			"syncedRoots": project.as_ref().map(synced_roots).unwrap_or_default(),
			"daemon": daemon,
			"plugin": plugin,
			"backlog": { "entries": backlog, "readOnly": true },
			"registryIdentity": registry_identity(),
			"instructionBudget": instruction_budget(&workspace.join("AGENTS.md")),
			"generatedDocs": {
				"wsync.md": generated("wsync.md"),
				"AGENTS.md": generated("AGENTS.md"),
				"CLAUDE.md": generated("CLAUDE.md"),
				"sourcemap.json": target.sourcemap().exists(),
			},
			"commands": commands,
			"llmPolicy": llm_policy(),
		});

		if self.raw {
			print_json(&snapshot);
		} else {
			println!("{}", serde_json::to_string_pretty(&snapshot)?);
		}

		Ok(())
	}
}

/// The `$path`-mapped mount points of the project tree, in tree order — the
/// list of Studio locations the filesystem projection actually backs
fn synced_roots(project: &Project) -> Vec<Value> {
	let mut roots = Vec::new();

	collect_roots(&project.node, "", &mut roots);

	roots
}

fn collect_roots(node: &ProjectNode, prefix: &str, roots: &mut Vec<Value>) {
	if let Some(path) = &node.path {
		roots.push(json!({
			// The root node has no Studio path of its own (a model project
			// mounts wherever it is placed), so it is named as such instead of
			// being given an invented path
			"instancePath": if prefix.is_empty() { "(project root)" } else { prefix },
			"path": path.path().to_string_lossy(),
		}));
	}

	for (name, child) in &node.tree {
		let child_prefix = if prefix.is_empty() {
			name.clone()
		} else {
			format!("{prefix}/{name}")
		};

		collect_roots(child, &child_prefix, roots);
	}
}

/// The LLM-first command budget (Design §10.6), embedded so agents that read
/// only `context` still get steered
fn llm_policy() -> Value {
	json!({
		"budget": [
			"Reuse known context; fetch context only when required project or connection facts are missing/stale, and refresh after project/config/binary changes or reconnects.",
			"No startup ritual: prefer local reads, focused live inspection, and narrow `wsync lint --path` verification.",
			"Use `wsync commands --compact` for discovery, `wsync commands <name> [subcommand...]` for exact syntax/effects, and `wsync commands types <TypeName>` for offline value schemas.",
			"--project selects the project; file inputs remain relative to the shell working directory unless a flag explicitly documents otherwise. Prefer absolute files across directories.",
			"After one schema/syntax error consult the relevant command/type schema; do not guess alternate JSON forms.",
			"Studio is authoritative for live objects; local mapped source is the normal editing surface. Preserve canonical project path, target paths, schema identity, completed edits and outstanding operation IDs in handoffs."
		],
		"writes": [
			"Use existing user authorization within scope; inspect exact targets when needed. Ask only for material ambiguity or additional destructive/external effects beyond that scope.",
			"One agent owns shared Studio playtest/eval/capture lifecycle. Coordinate live writes; parallelize independent file work.",
			"Reparent with `wsync mv`, not raw `set Parent`.",
			"Non-zero/timeout is not proof of no effect. Check completion/operation receipts and actual targets before retrying partial or unknown writes. Treat stdout as data only after validating success."
		],
		"reference": "wsync.md",
	})
}

fn instruction_budget(path: &std::path::Path) -> Value {
	match fs::read_to_string(path) {
		Ok(text) => json!({
			"file": "AGENTS.md", "bytes": text.len(),
			"generatedBudgetBytes": crate::docsgen::AGENT_CONTRACT_MAX_BYTES,
			"warningThresholdBytes": crate::docsgen::AGENT_FILE_WARNING_BYTES,
			"warning": crate::docsgen::agent_size_warning(&text),
		}),
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
			json!({ "file": "AGENTS.md", "bytes": null, "warning": null })
		}
		Err(error) => json!({ "file": "AGENTS.md", "bytes": null, "warning": error.to_string() }),
	}
}
