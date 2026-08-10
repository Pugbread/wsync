//! Instance inspection: `get`, `ls`, `tree`, `props`, `services` (get.json,
//! ls.json, tree.json, props.json, services.json).
//!
//! `get`/`ls`/`tree`/`props` each wrap exactly one remote op, so `--raw`
//! prints that op's value verbatim as one JSON line. `services` composes the
//! project file's `$path` mappings with `ls` listings, so its `--raw` output
//! is a CLI-authored object led by `ok`.

use anyhow::{Context, Result};
use clap::Parser;
use colored::Colorize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

use crate::{
	cli::client::{clip, field, human_value, print_json, Client, Targeting},
	config::Config,
	docsgen::ProjectFacts,
	ext::PathExt,
	project::Project,
	wsync_warn,
};

/// Read an instance view, or one property, from the live Studio session
#[derive(Parser)]
pub struct Get {
	#[command(flatten)]
	targeting: Targeting,

	/// Studio path, `/`-separated (e.g. `Workspace/Camera`)
	#[arg(long, value_name = "STUDIO-PATH")]
	path: String,

	/// Read only this property instead of the whole instance view
	#[arg(long, value_name = "PROPERTY")]
	prop: Option<String>,

	/// Print machine-readable JSON
	#[arg(long)]
	raw: bool,
}

impl Get {
	pub fn main(self) -> Result<()> {
		let client = Client::connect(&self.targeting)?;

		let args = match &self.prop {
			Some(prop) => json!({ "path": self.path, "prop": prop }),
			None => json!({ "path": self.path }),
		};

		let value = client.value("get", args, self.raw)?;

		if self.raw {
			print_json(&value);

			return Ok(());
		}

		// A single-property read prints just the value, so it composes with
		// shell pipelines; strings print unquoted and unwrapped
		if self.prop.is_some() {
			match &value {
				Value::String(text) => println!("{text}"),
				other => println!("{}", human_value(other)),
			}

			return Ok(());
		}

		println!("{} {}", field(&value, "class").bold(), field(&value, "path"));

		if let Some(properties) = value.get("properties").and_then(Value::as_object) {
			let mut names: Vec<&String> = properties.keys().collect();
			names.sort();

			println!("\nProperties ({})", names.len());

			for name in names {
				println!("  {name:<28} {}", human_value(&properties[name]));
			}
		}

		if let Some(attributes) = value.get("attributes").and_then(Value::as_object) {
			println!("\nAttributes ({})", attributes.len());

			let mut names: Vec<&String> = attributes.keys().collect();
			names.sort();

			for name in names {
				println!("  {name:<28} {}", human_value(&attributes[name]));
			}
		}

		if let Some(tags) = value.get("tags").and_then(Value::as_array) {
			println!(
				"\nTags       {}",
				tags.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", ")
			);
		}

		let children = value.get("childrenCount").and_then(Value::as_u64).unwrap_or(0);

		println!("\nChildren   {children}");

		if value.get("childrenTruncated").and_then(Value::as_bool) == Some(true) {
			wsync_warn!("The child list was truncated by the plugin — use `wsync ls` for the full listing");
		}

		Ok(())
	}
}

/// List the direct children of a live Studio instance
#[derive(Parser)]
pub struct Ls {
	#[command(flatten)]
	targeting: Targeting,

	/// Studio path; empty (the default) lists the DataModel services
	#[arg(long, value_name = "STUDIO-PATH", default_value = "")]
	path: String,

	/// Print machine-readable JSON
	#[arg(long)]
	raw: bool,
}

impl Ls {
	pub fn main(self) -> Result<()> {
		let client = Client::connect(&self.targeting)?;
		let value = client.value("ls", json!({ "path": self.path }), self.raw)?;

		if self.raw {
			print_json(&value);

			return Ok(());
		}

		let children = value
			.get("children")
			.and_then(Value::as_array)
			.cloned()
			.unwrap_or_default();

		for child in &children {
			println!(
				"{} {}",
				format!("{:<28}", clip(field(child, "class"), 28)).bold(),
				field(child, "name")
			);
		}

		let total = value
			.get("total")
			.and_then(Value::as_u64)
			.unwrap_or(children.len() as u64);

		println!("\n{} child(ren) of {}", total, field(&value, "path"));

		if value.get("truncated").and_then(Value::as_bool) == Some(true) {
			wsync_warn!("Listing truncated by the plugin's child limit — narrow the path");
		}

		Ok(())
	}
}

/// Print a class and name tree below a live Studio instance
#[derive(Parser)]
pub struct Tree {
	#[command(flatten)]
	targeting: Targeting,

	/// Studio path; empty (the default) starts at the DataModel
	#[arg(long, value_name = "STUDIO-PATH", default_value = "")]
	path: String,

	/// How many levels below the root to walk (0 = the root only)
	#[arg(long, default_value = "3")]
	depth: u32,

	/// Print machine-readable JSON
	#[arg(long)]
	raw: bool,
}

impl Tree {
	pub fn main(self) -> Result<()> {
		let client = Client::connect(&self.targeting)?;
		let value = client.value("tree", json!({ "path": self.path, "depth": self.depth }), self.raw)?;

		if self.raw {
			print_json(&value);

			return Ok(());
		}

		if let Some(root) = value.get("root") {
			print_node(root, "", true, true);
		}

		println!(
			"\n{} node(s) visited, depth {}",
			value.get("visitedNodes").and_then(Value::as_u64).unwrap_or(0),
			value.get("depth").and_then(Value::as_u64).unwrap_or(0)
		);

		if value.get("depthClamped").and_then(Value::as_bool) == Some(true) {
			wsync_warn!("--depth was clamped to the plugin's maximum tree depth");
		}

		if value.get("truncated").and_then(Value::as_bool) == Some(true) {
			wsync_warn!("The tree was truncated — lower --depth or start from a narrower path");
		}

		Ok(())
	}
}

/// Renders one tree node with box-drawing connectors. Nodes the plugin cut
/// off carry an explicit marker so a truncated branch is never mistaken for
/// a leaf
fn print_node(node: &Value, prefix: &str, last: bool, root: bool) {
	let connector = if root {
		String::new()
	} else if last {
		format!("{prefix}└─ ")
	} else {
		format!("{prefix}├─ ")
	};

	let truncated = if node.get("truncated").and_then(Value::as_bool) == Some(true) {
		" …truncated".yellow().to_string()
	} else {
		String::new()
	};

	println!(
		"{connector}{} {}{truncated}",
		field(node, "name"),
		format!("({})", field(node, "class")).dimmed()
	);

	let Some(children) = node.get("children").and_then(Value::as_array) else {
		return;
	};

	let child_prefix = if root {
		String::new()
	} else if last {
		format!("{prefix}   ")
	} else {
		format!("{prefix}│  ")
	};

	for (index, child) in children.iter().enumerate() {
		print_node(child, &child_prefix, index + 1 == children.len(), false);
	}
}

/// Print inspectable live Studio properties for one instance
#[derive(Parser)]
pub struct Props {
	#[command(flatten)]
	targeting: Targeting,

	/// Studio path, `/`-separated
	#[arg(long, value_name = "STUDIO-PATH")]
	path: String,

	/// Print machine-readable JSON
	#[arg(long)]
	raw: bool,
}

impl Props {
	pub fn main(self) -> Result<()> {
		let client = Client::connect(&self.targeting)?;
		let value = client.value("props", json!({ "path": self.path }), self.raw)?;

		if self.raw {
			print_json(&value);

			return Ok(());
		}

		println!(
			"{} {} ({} properties, source {})",
			field(&value, "class").bold(),
			field(&value, "path"),
			value.get("count").and_then(Value::as_u64).unwrap_or(0),
			field(&value, "source"),
		);

		let mut category = String::new();
		let empty = Vec::new();

		for record in value.get("properties").and_then(Value::as_array).unwrap_or(&empty) {
			let record_category = field(record, "category");

			if record_category != category {
				record_category.clone_into(&mut category);
				println!("\n{}", category.bold());
			}

			println!(
				"  {:<28} {}",
				field(record, "name"),
				human_value(record.get("value").unwrap_or(&Value::Null))
			);
		}

		Ok(())
	}
}

/// List the project's sync roots and their presence on disk and in Studio
#[derive(Parser)]
pub struct Services {
	#[command(flatten)]
	targeting: Targeting,

	/// Print machine-readable JSON
	#[arg(long)]
	raw: bool,
}

impl Services {
	pub fn main(self) -> Result<()> {
		let client = Client::connect(&self.targeting)?;

		// Roots come from the project file's tree and its `$path` mappings
		// (services.json) — never a fixed service allowlist. The docsgen fact
		// collector already extracts exactly this, so both surfaces agree
		let project = Project::load(&client.target.project_path).with_context(|| {
			format!(
				"Failed to load the project file {}",
				client.target.project_path.to_string()
			)
		})?;
		let workspace = client.target.project_path.get_parent().to_path_buf();
		let facts = ProjectFacts::from_project(&project, &Config::new());

		// Studio presence via `ls` of each root's parent, deduplicated: every
		// depth-1 root shares the single DataModel listing, and a nested root
		// (`ReplicatedStorage/Packages`) costs one more listing for its parent
		let parents: BTreeSet<String> = facts
			.roots
			.iter()
			.filter(|root| !root.studio_path.is_empty())
			.map(|root| parent_path(&root.studio_path).to_owned())
			.collect();

		let mut listings: BTreeMap<String, Vec<String>> = BTreeMap::new();

		for parent in parents {
			let names = match client.request("ls", json!({ "path": parent }))? {
				envelope if envelope.ok => envelope
					.value
					.get("children")
					.and_then(Value::as_array)
					.map(|children| {
						children
							.iter()
							.map(|child| field(child, "name").to_owned())
							.collect::<Vec<_>>()
					})
					.unwrap_or_default(),
				// A missing parent means every root below it is absent — a
				// listable fact, not a command failure
				_ => Vec::new(),
			};

			listings.insert(parent, names);
		}

		let roots: Vec<Value> = facts
			.roots
			.iter()
			.map(|root| {
				let on_disk = workspace.join(&root.fs_path).exists();

				// The project-root mapping has no Studio name of its own, so
				// live presence is not a meaningful question for it
				let in_studio = if root.studio_path.is_empty() {
					Value::Null
				} else {
					let name = root.studio_path.rsplit('/').next().unwrap_or(&root.studio_path);

					json!(listings
						.get(parent_path(&root.studio_path))
						.is_some_and(|names| names.iter().any(|listed| listed == name)))
				};

				json!({
					"studioPath": root.label(),
					"path": root.fs_path,
					"className": root.class_name,
					"optional": root.optional,
					"onDisk": on_disk,
					"inStudio": in_studio,
				})
			})
			.collect();

		if self.raw {
			print_json(&json!({
				"ok": true,
				"count": roots.len(),
				"roots": roots,
			}));

			return Ok(());
		}

		println!(
			"{}",
			format!(
				"{:<32} {:<28} {:<6} {}",
				"STUDIO PATH", "PROJECT PATH", "DISK", "STUDIO"
			)
			.bold()
		);

		for root in &roots {
			let presence = |key: &str| match root.get(key) {
				Some(Value::Bool(true)) => "yes".green().to_string(),
				Some(Value::Bool(false)) => "no".red().to_string(),
				_ => "-".dimmed().to_string(),
			};

			println!(
				"{:<32} {:<28} {:<15} {}",
				clip(field(root, "studioPath"), 32),
				clip(field(root, "path"), 28),
				// Colored cells are padded before painting elsewhere in this
				// surface; these two are short enough to pad by hand (the
				// escape codes would defeat a format width)
				presence("onDisk"),
				presence("inStudio"),
			);
		}

		println!("\n{} synced root(s)", roots.len());

		Ok(())
	}
}

/// Everything before the last `/` segment; the empty string is the DataModel
fn parent_path(studio_path: &str) -> &str {
	match studio_path.rsplit_once('/') {
		Some((parent, _)) => parent,
		None => "",
	}
}
