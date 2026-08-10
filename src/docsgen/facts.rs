//! Deterministic inputs to the agent-doc renderer (Design §10.6).
//!
//! Everything the renderer needs is captured here as plain data so that
//! rendering is a pure function: no filesystem reads, no clock, no globals.
//! The three fact types are:
//!
//! - [`ProjectFacts`] — what this project actually syncs (tree roots, sync
//!   rules, flags). Never a hardcoded service list.
//! - [`RegistryFacts`] — the generated command registry
//!   (`docs/client-commands.generated.json`), plus which of those commands are
//!   wired into the running binary.
//! - [`EnvFacts`] — versions and state-directory paths.

use anyhow::{Context as _, Result};
use serde::Deserialize;
use std::{
	collections::{BTreeMap, BTreeSet},
	path::Path,
};

use crate::{
	config::Config,
	constants::default_sync_rules,
	core::meta::SyncRule,
	middleware::Middleware,
	project::{Project, ProjectNode, ProjectPath},
};

////////////////////////////////////////////////////////////////////////////////
// Project facts
////////////////////////////////////////////////////////////////////////////////

/// One `$path` mapping in the project tree: a Studio path and the directory or
/// file on disk that projects into it
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncedRoot {
	/// `/`-separated Studio path, empty for the tree root itself
	pub studio_path: String,
	/// Project-relative filesystem path, always with `/` separators
	pub fs_path: String,
	/// `$path: { "optional": … }` — absent on disk is not an error
	pub optional: bool,
	/// Explicit `$className`, when the project pins one
	pub class_name: Option<String>,
}

impl SyncedRoot {
	/// Studio path for display; the tree root has no name of its own
	pub fn label(&self) -> &str {
		if self.studio_path.is_empty() {
			"<project root>"
		} else {
			&self.studio_path
		}
	}
}

/// Coarse grouping used to lay out the filesystem-conventions tables. Derived
/// from the middleware, so a custom `syncRules` entry lands in the right table
/// without any extra configuration
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RuleGroup {
	Scripts,
	Values,
	Modules,
	Models,
	Meta,
}

impl RuleGroup {
	pub fn of(middleware: &Middleware) -> Self {
		match middleware {
			Middleware::ServerScript
			| Middleware::ClientScript
			| Middleware::LocalScript
			| Middleware::RunServerScript
			| Middleware::ModuleScript => Self::Scripts,
			Middleware::StringValue | Middleware::RichStringValue | Middleware::LocalizationTable => Self::Values,
			Middleware::JsonModule | Middleware::TomlModule | Middleware::YamlModule | Middleware::MsgpackModule => {
				Self::Modules
			}
			Middleware::JsonModel | Middleware::RbxmModel | Middleware::RbxmxModel => Self::Models,
			Middleware::Project | Middleware::InstanceData => Self::Meta,
		}
	}

	pub fn heading(&self) -> &'static str {
		match self {
			Self::Scripts => "Scripts",
			Self::Values => "Values and localization",
			Self::Modules => "Data modules",
			Self::Models => "Model files",
			Self::Meta => "Project and metadata files",
		}
	}
}

/// One active sync rule, flattened for rendering
#[derive(Debug, Clone, PartialEq)]
pub struct SyncRuleFact {
	pub middleware: Middleware,
	pub group: RuleGroup,
	pub pattern: Option<String>,
	pub child_pattern: Option<String>,
	pub suffix: Option<String>,
	pub exclude: Vec<String>,
}

impl SyncRuleFact {
	fn from_rule(rule: &SyncRule) -> Self {
		Self {
			middleware: rule.middleware.clone(),
			group: RuleGroup::of(&rule.middleware),
			pattern: rule.pattern.as_ref().map(|glob| glob.as_str().to_owned()),
			child_pattern: rule.child_pattern.as_ref().map(|glob| glob.as_str().to_owned()),
			suffix: rule.suffix.clone(),
			exclude: rule.exclude.iter().map(|glob| glob.as_str().to_owned()).collect(),
		}
	}

	/// A representative filename for this rule, e.g. `*.server.luau` → `Foo.server.luau`
	pub fn example_file(&self) -> Option<String> {
		self.pattern.as_ref().map(|pattern| pattern.replace('*', "Foo"))
	}

	/// The `init`-style filename this rule uses for an instance with children
	pub fn example_init_file(&self) -> Option<String> {
		self.child_pattern.clone()
	}

	/// Argon-legacy `.src.*` forms are read but, in `rojoMode`, never written
	pub fn is_legacy_source_form(&self) -> bool {
		self.child_pattern
			.as_ref()
			.is_some_and(|pattern| pattern.starts_with(".src"))
	}

	/// `.data.json` sidecars are read but, in `rojoMode`, never written
	pub fn is_legacy_data_form(&self) -> bool {
		self.pattern
			.as_ref()
			.or(self.child_pattern.as_ref())
			.is_some_and(|pattern| pattern.ends_with(".data.json"))
	}
}

/// What this project actually syncs. Built from the loaded project plus the
/// resolved configuration — never assumed
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectFacts {
	pub name: String,
	/// File name of the project config, e.g. `default.project.json`
	pub project_file: String,
	/// `$className: DataModel` at the tree root
	pub is_place: bool,
	pub roots: Vec<SyncedRoot>,
	/// The rules actually in effect: the project's `syncRules` when it defines
	/// any (they replace the defaults wholesale), otherwise the built-in set
	pub sync_rules: Vec<SyncRuleFact>,
	/// True when `syncRules` in the project replaced the built-in defaults
	pub custom_sync_rules: bool,
	pub ignore_globs: Vec<String>,
	pub syncback_ignore_globs: Vec<String>,
	pub syncback_ignore_names: Vec<String>,
	pub syncback_ignore_classes: Vec<String>,
	/// `rojoMode` — write Rojo-shaped `init.*` files and `.meta.json` sidecars
	pub rojo_mode: bool,
	/// `luaExtension` — emit `.lua` instead of `.luau` on syncback
	pub lua_extension: bool,
	/// `renameInstances` — rename instances whose names a path cannot express
	pub rename_instances: bool,
	/// `keepDuplicates` — keep same-named siblings by suffixing the file name
	pub keep_duplicates: bool,
	/// `conflictEngine` — park racing edits instead of letting one side win
	pub conflict_engine: bool,
	/// Sync scope (Design §7.0): code-first unless the project opts into
	/// `"scope": "full"`
	pub scope: crate::project::Scope,
	pub game_id: Option<u64>,
	pub place_ids: Vec<u64>,
	pub group_id: Option<u64>,
}

impl ProjectFacts {
	/// Reads every fact out of an already-loaded project and configuration.
	/// Touches no files
	pub fn from_project(project: &Project, config: &Config) -> Self {
		let rules: Vec<SyncRule> = if project.sync_rules.is_empty() {
			default_sync_rules().clone()
		} else {
			project.sync_rules.clone()
		};

		let mut roots = Vec::new();
		collect_root(&project.node, "", &mut roots);
		collect_roots(&project.node, "", &mut roots);

		let syncback = project.syncback.as_ref();

		Self {
			name: project.name.clone(),
			project_file: project
				.path
				.file_name()
				.map(|name| name.to_string_lossy().into_owned())
				.unwrap_or_else(|| String::from("default.project.json")),
			is_place: project.is_place(),
			roots,
			sync_rules: rules.iter().map(SyncRuleFact::from_rule).collect(),
			custom_sync_rules: !project.sync_rules.is_empty(),
			ignore_globs: project.ignore_globs.iter().map(|g| g.as_str().to_owned()).collect(),
			syncback_ignore_globs: syncback
				.map(|s| s.ignore_globs.iter().map(|g| g.as_str().to_owned()).collect())
				.unwrap_or_default(),
			syncback_ignore_names: syncback.map(|s| s.ignore_names.clone()).unwrap_or_default(),
			syncback_ignore_classes: syncback.map(|s| s.ignore_classes.clone()).unwrap_or_default(),
			rojo_mode: config.rojo_mode,
			lua_extension: config.lua_extension,
			rename_instances: config.rename_instances,
			keep_duplicates: config.keep_duplicates,
			conflict_engine: config.conflict_engine,
			scope: project.scope(),
			game_id: project.game_id,
			place_ids: project.place_ids.clone(),
			group_id: project.group_id,
		}
	}

	/// Active rules in one conventions group, in rule order (first match wins)
	pub fn rules_in(&self, group: RuleGroup) -> Vec<&SyncRuleFact> {
		self.sync_rules.iter().filter(|rule| rule.group == group).collect()
	}

	/// Rules that describe a file form this build actually writes back: the
	/// Argon-legacy `.src.*` and `.data.json` forms are read-only under
	/// `rojoMode`, and duplicate patterns never win a second time
	pub fn primary_rules_in(&self, group: RuleGroup) -> Vec<&SyncRuleFact> {
		let mut seen: BTreeSet<&str> = BTreeSet::new();
		let mut primary = Vec::new();

		for rule in self.rules_in(group) {
			if self.rojo_mode && (rule.is_legacy_source_form() || rule.is_legacy_data_form()) {
				continue;
			}

			let Some(pattern) = rule.pattern.as_deref().or(rule.child_pattern.as_deref()) else {
				continue;
			};

			if seen.insert(pattern) {
				primary.push(rule);
			}
		}

		primary
	}

	/// Whether any rule in effect uses this middleware. Custom `syncRules` can
	/// drop whole middleware families, and the docs must not describe a file
	/// form this project has no rule for
	pub fn has_middleware(&self, middleware: &Middleware) -> bool {
		self.sync_rules.iter().any(|rule| rule.middleware == *middleware)
	}

	/// Whether any rule in effect matches files with this extension
	pub fn has_extension(&self, extension: &str) -> bool {
		self.sync_rules.iter().any(|rule| {
			rule.pattern
				.as_deref()
				.or(rule.child_pattern.as_deref())
				.is_some_and(|pattern| pattern.ends_with(extension))
		})
	}

	/// Whether any rule in effect is one of the Argon-legacy read-only forms
	pub fn has_legacy_forms(&self) -> bool {
		self.sync_rules
			.iter()
			.any(|rule| rule.is_legacy_source_form() || rule.is_legacy_data_form())
	}

	/// The metadata sidecar pattern in effect, when the rules define one
	pub fn metadata_pattern(&self) -> Option<&str> {
		self.sync_rules
			.iter()
			.filter(|rule| rule.middleware == Middleware::InstanceData)
			.filter(|rule| !(self.rojo_mode && rule.is_legacy_data_form()))
			.find_map(|rule| rule.pattern.as_deref())
	}

	/// Class name a script-flavored rule projects to — fixed per suffix, no
	/// mode flag (see the scheme on the `Middleware` enum)
	pub fn script_class(&self, middleware: &Middleware) -> &'static str {
		match middleware {
			Middleware::ServerScript | Middleware::ClientScript | Middleware::RunServerScript => "Script",
			Middleware::LocalScript => "LocalScript",
			_ => "ModuleScript",
		}
	}
}

/// The tree root can carry its own `$path` (the model/plugin/package templates
/// do); it has no Studio name of its own
fn collect_root(node: &ProjectNode, prefix: &str, out: &mut Vec<SyncedRoot>) {
	if let Some(path) = &node.path {
		out.push(root_from(prefix.to_owned(), path, node));
	}
}

fn collect_roots(node: &ProjectNode, prefix: &str, out: &mut Vec<SyncedRoot>) {
	// `BTreeMap` iteration keeps the output stable regardless of the order the
	// keys appear in the project file
	for (name, child) in &node.tree {
		let studio_path = if prefix.is_empty() {
			name.clone()
		} else {
			format!("{prefix}/{name}")
		};

		if let Some(path) = &child.path {
			out.push(root_from(studio_path.clone(), path, child));
		}

		// Nested `$path` nodes below a mapped root are real roots too
		// (`ReplicatedStorage/Packages` in the stock place template)
		collect_roots(child, &studio_path, out);
	}
}

fn root_from(studio_path: String, path: &ProjectPath, node: &ProjectNode) -> SyncedRoot {
	SyncedRoot {
		studio_path,
		fs_path: path.path().to_string_lossy().replace('\\', "/"),
		optional: matches!(path, ProjectPath::Optional { .. }),
		class_name: node.class_name.map(|class| class.as_str().to_owned()),
	}
}

////////////////////////////////////////////////////////////////////////////////
// Registry facts
////////////////////////////////////////////////////////////////////////////////

/// Which registry commands the calling binary actually implements. The docs
/// must never present a documented-but-unbuilt command as runnable
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Shipped {
	/// Every documented command is wired in (docs-site rendering, tests)
	All,
	/// Only these command names are wired in
	Only(BTreeSet<String>),
}

impl Shipped {
	pub fn only<I, S>(names: I) -> Self
	where
		I: IntoIterator<Item = S>,
		S: AsRef<str>,
	{
		Self::Only(names.into_iter().map(|name| name.as_ref().to_owned()).collect())
	}

	fn contains(&self, name: &str) -> bool {
		match self {
			Self::All => true,
			Self::Only(names) => names.contains(name),
		}
	}
}

/// Whether a command reads or changes live Studio state. The generated
/// registry only carries `safety` for subcommands, so the tier split is
/// derived from the command's category plus a short, explicit exception list
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Safety {
	ReadOnly,
	Mutating,
}

/// Commands whose category does not imply their tier
const SAFETY_EXCEPTIONS: [(&str, Safety); 6] = [
	// Reads Studio, writes only WSync's own private clipboard
	("copy", Safety::ReadOnly),
	// Renders into a file the caller names; changes nothing in Studio
	("capture", Safety::ReadOnly),
	// Negotiation only
	("capabilities", Safety::ReadOnly),
	// Listing parked conflicts changes nothing
	("conflicts", Safety::ReadOnly),
	// Selection is Studio state
	("open", Safety::Mutating),
	// Pastes instances into the connected place
	("paste", Safety::Mutating),
];

/// Categories whose commands need a running daemon and a connected plugin.
/// §6 of the generated docs is built from exactly these
const LIVE_CATEGORIES: [&str; 6] = [
	"Live inspection",
	"Path tools",
	"Live writes",
	"Studio clipboard",
	"Studio control",
	"Conflict resolution",
];

/// One command as the generated registry describes it
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandFact {
	pub name: String,
	pub category: String,
	/// First sentence of the registry description
	pub summary: String,
	pub usage: String,
	pub examples: Vec<String>,
	pub subcommands: Vec<String>,
	/// False when the registry documents the command but this build does not
	/// implement it yet
	pub shipped: bool,
}

impl CommandFact {
	pub fn safety(&self) -> Safety {
		if let Some((_, safety)) = SAFETY_EXCEPTIONS.iter().find(|(name, _)| *name == self.name) {
			return *safety;
		}

		match self.category.as_str() {
			"Live writes" | "Studio control" | "Studio clipboard" | "Open Cloud" | "Conflict resolution" => {
				Safety::Mutating
			}
			_ => Safety::ReadOnly,
		}
	}

	pub fn is_live(&self) -> bool {
		LIVE_CATEGORIES.contains(&self.category.as_str())
	}

	/// Examples with `--project .` kept as the registry wrote them
	pub fn first_examples(&self, count: usize) -> &[String] {
		let count = count.min(self.examples.len());
		&self.examples[..count]
	}
}

#[derive(Debug, Deserialize)]
struct RawRegistry {
	#[serde(default, rename = "schemaVersion")]
	schema_version: u32,
	#[serde(default)]
	categories: Vec<String>,
	#[serde(default)]
	commands: Vec<RawCommand>,
}

#[derive(Debug, Deserialize)]
struct RawCommand {
	name: String,
	#[serde(default)]
	category: String,
	#[serde(default)]
	description: String,
	#[serde(default)]
	usage: String,
	#[serde(default)]
	examples: Vec<String>,
	#[serde(default)]
	subcommands: Vec<String>,
}

/// The generated command registry, as the docs consume it
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryFacts {
	pub schema_version: u32,
	pub categories: Vec<String>,
	/// Registry order, which the generator keeps stable
	order: Vec<String>,
	commands: BTreeMap<String, CommandFact>,
}

impl RegistryFacts {
	/// Parses `docs/client-commands.generated.json`. Every documented command
	/// starts out marked as shipped; call [`RegistryFacts::with_shipped`] with
	/// the binary's real command list to mark the rest as pending
	pub fn from_generated_json(json: &str) -> Result<Self> {
		let raw: RawRegistry = serde_json::from_str(json).context("Failed to parse the generated command registry")?;

		let mut order = Vec::with_capacity(raw.commands.len());
		let mut commands = BTreeMap::new();

		for command in raw.commands {
			order.push(command.name.clone());

			commands.insert(
				command.name.clone(),
				CommandFact {
					summary: first_sentence(&command.description),
					name: command.name,
					category: command.category,
					usage: command.usage,
					examples: command.examples,
					subcommands: command.subcommands,
					shipped: true,
				},
			);
		}

		Ok(Self {
			schema_version: raw.schema_version,
			categories: raw.categories,
			order,
			commands,
		})
	}

	/// Marks which documented commands this build actually implements
	pub fn with_shipped(mut self, shipped: Shipped) -> Self {
		for (name, command) in self.commands.iter_mut() {
			command.shipped = shipped.contains(name);
		}

		self
	}

	pub fn get(&self, name: &str) -> Option<&CommandFact> {
		self.commands.get(name)
	}

	/// Documented *and* built into this binary
	pub fn is_shipped(&self, name: &str) -> bool {
		self.commands.get(name).is_some_and(|command| command.shipped)
	}

	pub fn is_documented(&self, name: &str) -> bool {
		self.commands.contains_key(name)
	}

	/// The named commands that this build ships, in the order asked for.
	/// Anything unknown or not yet built is dropped, so a doc section can name
	/// its ideal command set and still never promise a command that is missing
	pub fn pick<'facts>(&'facts self, names: &[&str]) -> Vec<&'facts CommandFact> {
		names
			.iter()
			.filter_map(|name| self.commands.get(*name))
			.filter(|command| command.shipped)
			.collect()
	}

	/// The named commands that are documented but not built yet
	pub fn pending<'names>(&self, names: &[&'names str]) -> Vec<&'names str> {
		names
			.iter()
			.filter(|name| self.commands.get(**name).is_some_and(|command| !command.shipped))
			.copied()
			.collect()
	}

	/// Every shipped live command with the requested safety tier, in registry
	/// order. This is what keeps §6 honest: the tier lists are the registry's
	/// command set, never a list typed into the renderer
	pub fn live_tier(&self, safety: Safety) -> Vec<&CommandFact> {
		self.order
			.iter()
			.filter_map(|name| self.commands.get(name))
			.filter(|command| command.shipped && command.is_live() && command.safety() == safety)
			.collect()
	}

	pub fn commands(&self) -> impl Iterator<Item = &CommandFact> {
		self.order.iter().filter_map(|name| self.commands.get(name))
	}
}

/// The registry's descriptions are one or more sentences; the docs only quote
/// the first one
fn first_sentence(description: &str) -> String {
	let description = description.trim();
	let line = description.lines().next().unwrap_or_default().trim();

	match line.find(". ") {
		Some(end) => line[..=end].trim().to_owned(),
		None => line.to_owned(),
	}
}

////////////////////////////////////////////////////////////////////////////////
// Environment facts
////////////////////////////////////////////////////////////////////////////////

/// Versions and paths the docs quote. Deliberately timestamp-free: two
/// refreshes with the same inputs must produce byte-identical files
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvFacts {
	pub cli_version: String,
	pub protocol_version: u8,
	pub plugin_version: Option<String>,
	/// Whether a Studio plugin was connected when the docs were generated
	pub plugin_connected: bool,
	/// Platform-native state directory holding `writes.log` and daemon records
	pub state_dir: String,
	/// Full path of the mutation audit log
	pub writes_log: String,
}

impl EnvFacts {
	/// Derives `writes.log` from the state directory, keeping the two in step
	pub fn new(cli_version: impl Into<String>, protocol_version: u8, state_dir: &Path) -> Self {
		Self {
			cli_version: cli_version.into(),
			protocol_version,
			plugin_version: None,
			plugin_connected: false,
			state_dir: display_path(state_dir),
			writes_log: display_path(&state_dir.join("writes.log")),
		}
	}

	pub fn with_plugin(mut self, version: Option<String>, connected: bool) -> Self {
		self.plugin_version = version;
		self.plugin_connected = connected;
		self
	}
}

fn display_path(path: &Path) -> String {
	path.to_string_lossy().into_owned()
}

////////////////////////////////////////////////////////////////////////////////
// Bundle
////////////////////////////////////////////////////////////////////////////////

/// Everything the renderer reads
#[derive(Debug, Clone)]
pub struct DocsInput<'facts> {
	pub project: &'facts ProjectFacts,
	pub registry: &'facts RegistryFacts,
	pub env: &'facts EnvFacts,
}

impl<'facts> DocsInput<'facts> {
	pub fn new(project: &'facts ProjectFacts, registry: &'facts RegistryFacts, env: &'facts EnvFacts) -> Self {
		Self { project, registry, env }
	}
}
