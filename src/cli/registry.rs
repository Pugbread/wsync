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
use clap::Parser;
use serde_json::{json, Map, Value};

use crate::{
	cli::client::{print_json, Client, Target, Targeting},
	cli::registry_bundle,
	ext::PathExt,
	project::{Project, ProjectNode},
};

// ---------------------------------------------------------------------------
// commands
// ---------------------------------------------------------------------------

/// Print generated command docs as JSON: the full registry, one command, or a
/// compact index
#[derive(Parser)]
pub struct Commands {
	/// Print only this command's registry entry
	#[arg(value_name = "COMMAND")]
	name: Option<String>,

	/// Print the compact index (name and description per category) instead of
	/// complete entries
	#[arg(long)]
	compact: bool,
}

impl Commands {
	pub fn main(self) -> Result<()> {
		// The full dump is the embedded file verbatim (commands.json), so the
		// bytes agents read match the published docs exactly
		if self.name.is_none() && !self.compact {
			print!("{}", registry_bundle::CLIENT_COMMANDS);

			if !registry_bundle::CLIENT_COMMANDS.ends_with('\n') {
				println!();
			}

			return Ok(());
		}

		let Some(bundle) = registry_bundle::parsed() else {
			bail!("The embedded command registry does not parse — this binary was built from a broken docs bundle");
		};

		let empty = Vec::new();
		let commands = bundle.get("commands").and_then(Value::as_array).unwrap_or(&empty);

		if let Some(name) = &self.name {
			let Some(entry) = commands
				.iter()
				.find(|entry| entry.get("name").and_then(Value::as_str) == Some(name.as_str()))
			else {
				bail!("No command named `{name}` in the registry — run `wsync commands --compact` for the index");
			};

			let payload = if self.compact {
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
		"hint": "run `wsync commands <name>` for one command's full usage",
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

		// Conflict count from `GET /resolve`; a daemon build without the
		// endpoint (or no daemon at all) is `null` — "not answerable", which
		// is different from zero
		let conflicts = client.as_ref().and_then(|client| {
			let endpoint = client.get_endpoint("/resolve").ok()?;

			if endpoint.unavailable() {
				return None;
			}

			let count = endpoint
				.json
				.as_ref()?
				.get("conflicts")
				.and_then(Value::as_array)
				.map(Vec::len)?;

			Some(count)
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
				"fullRegistry": "wsync commands (full dump — explicit user request only)",
			})
		};

		let snapshot = json!({
			"ok": true,
			"project": {
				"path": target.project_path.to_string(),
				"exists": target.project_exists,
				"parses": project.is_some(),
				"name": project.as_ref().map(|project| project.name.clone()),
				"gameId": project.as_ref().and_then(|project| project.game_id),
				"placeIds": project.as_ref().map(|project| project.place_ids.clone()),
			},
			"syncedRoots": project.as_ref().map(synced_roots).unwrap_or_default(),
			"daemon": daemon,
			"plugin": plugin,
			"conflicts": conflicts,
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
	let mut policy = Map::new();

	policy.insert(
		"budget".to_owned(),
		json!([
			"Run `wsync context` once, and only when project context is not already in AGENTS.md.",
			"Prefer local file reads and cheap offline commands for normal code work.",
			"Use focused live reads (`tree --depth`, `ls`, `meta`, `get --prop`) for Explorer shape and \
			 Studio-owned objects — the live tree is authoritative; the disk view is narrower by design.",
			"Use `wsync commands --compact` only when choosing between command families and \
			 `wsync commands <name>` only for the command about to be run; dump the full registry \
			 only on explicit user request.",
			"Disk-only inference never overrides live Studio reads.",
			"Before mutating Studio: inspect the exact live target with read-only commands, then confirm \
			 explicit user intent. `plan` is available for dry-run explanations but is not a mandatory ritual.",
			"Order reads by cost: cheap-first discovery (`context`, `status --raw`, `query --format paths`, \
			 `path`, `meta`) before targeted reads (`tree --depth`, `ls`, `props`, `get --prop`) before \
			 higher-token reads (`changes`, wide `tree`, `find`, `logs --limit`) before backup/debug reads \
			 (`snapshot`).",
			"Do not run `diff`, `changes`, `conflicts`, or live `source` as a startup ritual: `source` is a \
			 loose divergence diagnostic, not a verification step; `conflicts` is for resolving observed \
			 conflicts, not a health poll; `changes`/`diff` are noisy on drifty projects.",
			"Post-edit verification is the narrowest `wsync lint --path` plus a local file read, never \
			 unrelated global diff output.",
		]),
	);

	policy.insert(
		"writes".to_owned(),
		json!([
			"Bracket multi-write changes with `--waypoint <name>` so one Studio undo reverses the batch.",
			"`set Parent` is refused — reparent with `wsync mv`; `--force-parent` exists only for an \
			 intentional single raw assignment, and batches have no escape.",
		]),
	);

	policy.insert(
		"safety".to_owned(),
		json!(
			"Live mutating commands are user-initiated escape hatches, never autonomous tools; \
			 every write is audited to writes.log."
		),
	);

	Value::Object(policy)
}
