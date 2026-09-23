use anyhow::{anyhow, Result};
use colored::Colorize;
use log::{debug, trace};
use rbx_dom_weak::{
	types::{Enum, Variant},
	ustr,
};
use serde::{Deserialize, Serialize};
use std::{
	cell::Cell,
	fmt::{self, Display, Formatter},
	path::{Path, PathBuf},
};

use self::data::DataSnapshot;
use crate::{
	core::{
		meta::{Context, Source},
		snapshot::Snapshot,
	},
	crash_handler::{self, Breadcrumb},
	ext::{PathExt, ResultExt},
	vfs::Vfs,
	wsync_warn, Properties,
};

mod helpers;

pub mod csv;
pub mod data;
pub mod dir;
pub mod json;
pub mod json_model;
pub mod layout;
pub mod luau;
pub mod md;
pub mod msgpack;
pub mod project;
pub mod rbxm;
pub mod rbxmx;
pub mod toml;
pub mod txt;
pub mod yaml;

/// The script middlewares encode class *and* RunContext in the file suffix —
/// one suffix, one meaning, no mode flag (the Argon fork's `legacyScripts`
/// flag multiplexed `.server`/`.client` onto two meanings each, which made
/// mixed-RunContext places impossible to round-trip):
///
///   `Name.luau`            ModuleScript
///   `Name.server.luau`     Script, RunContext = Legacy (the classic script)
///   `Name.client.luau`     Script, RunContext = Client
///   `Name.local.luau`      LocalScript
///   `Name.runserver.luau`  Script, RunContext = Server
///
/// RunContext = Plugin has no suffix; it falls back to `.server.luau` with
/// the RunContext preserved in the instance-data sidecar.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum Middleware {
	Project,
	InstanceData,

	ServerScript,
	ClientScript,
	LocalScript,
	RunServerScript,
	ModuleScript,

	StringValue,
	RichStringValue,
	LocalizationTable,

	JsonModule,
	TomlModule,
	YamlModule,
	MsgpackModule,

	JsonModel,
	RbxmModel,
	RbxmxModel,
}

impl Display for Middleware {
	fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
		write!(f, "{self:?}")
	}
}

impl Middleware {
	fn read(&self, path: &Path, context: &Context, vfs: &Vfs) -> Result<Snapshot> {
		match self {
			Middleware::Project => project::read_project(path, Some(context), vfs),
			Middleware::InstanceData => unreachable!(),
			//
			Middleware::ServerScript
			| Middleware::ClientScript
			| Middleware::LocalScript
			| Middleware::RunServerScript
			| Middleware::ModuleScript => luau::read_luau(path, vfs, self.clone().into()),
			//
			Middleware::StringValue => txt::read_txt(path, vfs),
			Middleware::RichStringValue => md::read_md(path, vfs),
			Middleware::LocalizationTable => csv::read_csv(path, vfs),
			//
			Middleware::JsonModule => json::read_json(path, vfs),
			Middleware::TomlModule => toml::read_toml(path, vfs),
			Middleware::YamlModule => yaml::read_yaml(path, vfs),
			Middleware::MsgpackModule => msgpack::read_msgpack(path, vfs),
			//
			Middleware::JsonModel => json_model::read_json_model(path, vfs),
			Middleware::RbxmModel => rbxm::read_rbxm(path, vfs),
			Middleware::RbxmxModel => rbxmx::read_rbxmx(path, vfs),
		}
		.map_err(|err| {
			// A runaway walk already names its path; prefixing it once per
			// nested project level would bury that under dozens of lines
			if err.is::<SnapshotTooDeep>() {
				return err;
			}

			anyhow!(
				"Failed to read {} at {}: {err}",
				self.to_string().bold(),
				path.display().to_string().bold()
			)
		})
	}

	pub fn write(&self, properties: Properties, path: &Path, vfs: &Vfs) -> Result<Properties> {
		match self {
			Middleware::ServerScript
			| Middleware::ClientScript
			| Middleware::LocalScript
			| Middleware::RunServerScript
			| Middleware::ModuleScript => luau::write_luau(properties, path, vfs),
			Middleware::StringValue => txt::write_txt(properties, path, vfs),
			Middleware::LocalizationTable => csv::write_csv(properties, path, vfs),
			// TODO: Add support for other middleware
			_ => unimplemented!(),
		}
		.with_desc(|| {
			format!(
				"Failed to write {} at {}",
				self.to_string().bold(),
				path.display().to_string().bold()
			)
		})
	}

	/// The middleware that names a file for this class — for `Script`, the
	/// RunContext is consumed from `properties` because the suffix encodes it
	/// (see the scheme on the enum). Callers that cannot supply properties get
	/// the Legacy `.server` fallback.
	pub fn from_class(class: &str, properties: Option<&mut Properties>) -> Option<Self> {
		// TODO: Implement matcher for detecting remaining middleware
		match class {
			"Script" => {
				if let Some(properties) = properties {
					if let Some(Variant::Enum(run_context)) = properties.remove(&ustr("RunContext")) {
						let run_context = run_context.to_u32();

						return Some(match run_context {
							1 => Middleware::RunServerScript,
							2 => Middleware::ClientScript,
							// Legacy (0) is the `.server` suffix itself
							0 => Middleware::ServerScript,
							_ => {
								// RunContext.Plugin (or a future value) has no
								// suffix: keep it as a property so the data
								// sidecar preserves it across the round trip
								properties.insert(ustr("RunContext"), Variant::Enum(Enum::from_u32(run_context)));

								Middleware::ServerScript
							}
						});
					}
				}

				Some(Middleware::ServerScript)
			}
			"LocalScript" => Some(Middleware::LocalScript),
			"ModuleScript" => Some(Middleware::ModuleScript),
			"StringValue" => Some(Middleware::StringValue),
			"LocalizationTable" => Some(Middleware::LocalizationTable),
			_ => None,
		}
	}
}

/// Whether a resolved sync rule may produce an instance under this context's
/// scope (Design §7.0). Code scope reads project files, `.luau` sources and
/// the instance-data sidecars that identify them — every other file is
/// outside the projection: ignored, never deleted. Full scope admits
/// everything
fn scope_allows_rule(context: &Context, middleware: &Middleware, path: &Path) -> bool {
	if !context.scope().is_code() {
		return true;
	}

	match middleware {
		Middleware::Project => true,
		Middleware::ServerScript
		| Middleware::ClientScript
		| Middleware::LocalScript
		| Middleware::RunServerScript
		| Middleware::ModuleScript => path.extension().is_some_and(|extension| extension == "luau"),
		_ => false,
	}
}

/// How deep the snapshot walk may nest. Every directory, init folder and
/// project node (with its `$path`) is one level. Real projects stay far below
/// it; a cyclic layout — a project `$path` pointing back at an ancestor —
/// hits it and fails with the offending path instead of recursing until the
/// stack overflows. That matters beyond the error itself: a stack overflow
/// aborts the process without running the panic hook, so without this guard
/// a runaway walk dies with nothing in the daemon log
pub const MAX_SNAPSHOT_DEPTH: usize = 128;

/// How much stack one snapshot walk may use, measured from where it began —
/// the second half of the guard, since frame sizes vary by build: a debug
/// build spends 12–16 KiB per level, so 128 levels alone would not fit the
/// processor thread's default 2 MiB stack. The budget leaves the other half
/// for what runs at the deepest level (a model file's own parse)
pub const SNAPSHOT_STACK_BUDGET: usize = 1024 * 1024;

/// How deep the instances inside one model file (`.rbxm`, `.rbxmx`,
/// `.model.json`) — or a subtree Studio sends — may nest before it is
/// refused. Both are untrusted input, and their walks recurse per instance
pub const MAX_MODEL_DEPTH: usize = 256;

/// The stack one model file's walk may use on top of the snapshot walk's own
/// budget
pub const MODEL_STACK_BUDGET: usize = 512 * 1024;

/// Whether a per-instance model walk that began at stack position `base`
/// may descend to `depth`
pub(crate) fn model_depth_allowed(depth: usize, base: usize) -> bool {
	depth <= MAX_MODEL_DEPTH && base.saturating_sub(stack_position()) <= MODEL_STACK_BUDGET
}

thread_local! {
	/// Current nesting of the snapshot walk on this thread
	static SNAPSHOT_DEPTH: Cell<usize> = const { Cell::new(0) };
	/// Stack position where this thread's current walk began
	static SNAPSHOT_STACK_BASE: Cell<usize> = const { Cell::new(0) };
}

/// Approximates the stack pointer by a local's address. Recursive walks
/// compare it against their starting position to bound their stack use
#[inline(never)]
pub(crate) fn stack_position() -> usize {
	let marker = 0u8;

	std::hint::black_box(&marker) as *const u8 as usize
}

/// The snapshot walk nested past `MAX_SNAPSHOT_DEPTH` or its stack budget.
/// A distinct type so the per-level error descriptions pass it through
/// unwrapped instead of prefixing it once per level
#[derive(Debug)]
pub struct SnapshotTooDeep {
	path: PathBuf,
	depth: usize,
	stack: usize,
	started: Option<String>,
}

impl Display for SnapshotTooDeep {
	fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
		if self.depth > MAX_SNAPSHOT_DEPTH {
			write!(
				f,
				"The snapshot walk nested deeper than {MAX_SNAPSHOT_DEPTH} levels at {}",
				self.path.display()
			)?;
		} else {
			write!(
				f,
				"The snapshot walk used over {} KiB of stack ({} levels deep) at {}",
				self.stack / 1024,
				self.depth,
				self.path.display()
			)?;
		}

		if let Some(started) = &self.started {
			write!(f, " ({started})")?;
		}

		write!(
			f,
			". The layout is cyclic (a symlink or a project `$path` pointing at an ancestor) or nests unreasonably deep"
		)
	}
}

impl std::error::Error for SnapshotTooDeep {}

/// One level of the snapshot walk: counts toward `MAX_SNAPSHOT_DEPTH` and
/// leaves a crash breadcrumb naming the path, so a panic anywhere below
/// reports what was being snapshotted
pub(crate) struct SnapshotFrame {
	_breadcrumb: Breadcrumb,
}

impl SnapshotFrame {
	pub(crate) fn enter(path: &Path, label: impl FnOnce() -> String) -> Result<Self> {
		let depth = SNAPSHOT_DEPTH.get() + 1;
		let position = stack_position();

		if depth == 1 {
			SNAPSHOT_STACK_BASE.set(position);
		}

		// Stacks grow down on every supported target; were one to grow up,
		// the saturating difference only disables the budget half
		let stack = SNAPSHOT_STACK_BASE.get().saturating_sub(position);

		if depth > MAX_SNAPSHOT_DEPTH || stack > SNAPSHOT_STACK_BUDGET {
			let started = crash_handler::breadcrumbs()
				.into_iter()
				.find(|breadcrumb| breadcrumb.starts_with("snapshotting"))
				.map(|outermost| format!("the walk started {outermost}"));

			return Err(SnapshotTooDeep {
				path: path.to_owned(),
				depth,
				stack,
				started,
			}
			.into());
		}

		SNAPSHOT_DEPTH.set(depth);

		Ok(Self {
			_breadcrumb: crash_handler::breadcrumb(label()),
		})
	}
}

impl Drop for SnapshotFrame {
	fn drop(&mut self) {
		SNAPSHOT_DEPTH.set(SNAPSHOT_DEPTH.get().saturating_sub(1));
	}
}

/// Returns a snapshot of the given path, `None` if path no longer exists.
///
/// A path snapshotted on its own (a VFS event re-reading one instance) that
/// a same-named sibling form shadows resolves to that sibling, exactly as
/// listing the parent would (see `layout`)
pub fn new_snapshot(path: &Path, context: &Context, vfs: &Vfs) -> Result<Option<Snapshot>> {
	snapshot_path(path, context, vfs, true)
}

/// Snapshot of a path a project node maps with `$path`: the project names
/// this exact path, so no sibling form overrides it
pub(crate) fn new_snapshot_mapped(path: &Path, context: &Context, vfs: &Vfs) -> Result<Option<Snapshot>> {
	snapshot_path(path, context, vfs, false)
}

fn snapshot_path(path: &Path, context: &Context, vfs: &Vfs, redirect: bool) -> Result<Option<Snapshot>> {
	if layout::is_excluded(path, context) {
		trace!("Snapshot of {} not created: ignored or blacklisted", path.display());
		return Ok(None);
	}

	if !vfs.exists(path) {
		trace!("Snapshot of {} not created: path does not exist", path.display());

		vfs.unwatch(path)?;

		return Ok(None);
	}

	let _frame = SnapshotFrame::enter(path, || format!("snapshotting {}", path.display()))?;

	trace!("Creating snapshot of {}", path.display());

	if redirect {
		if let Some(form) = layout::shadowing_form(path, context, vfs)? {
			debug!(
				"{} is shadowed by the same-named {}; snapshotting that instead",
				path.display(),
				form.display()
			);

			if let Some(parent) = path.parent() {
				layout::report(&layout::read_layout(parent, context, vfs)?, None);
			}

			return snapshot_path(&form, context, vfs, false);
		}
	}

	if vfs.is_file(path) {
		// An init file read directly (the root project file, or a `$path`
		// naming one) defines its folder
		if let Some(candidate) = layout::init_candidate(path, context) {
			if let Some(snapshot) = snapshot_init(path.get_parent(), &candidate, None, context, vfs)? {
				return Ok(Some(snapshot));
			}
		}

		match new_snapshot_file(path, context, vfs)? {
			Some(snapshot) => Ok(Some(snapshot)),
			None => {
				trace!("Snapshot of {} not created: no middleware matched", path.display());
				Ok(None)
			}
		}
	} else {
		snapshot_dir(path, context, vfs)
	}
}

/// Snapshot of an entry `layout::read_layout` returned as a child: already
/// known to be projected, not an init file and not shadowed
fn snapshot_child(path: &Path, context: &Context, vfs: &Vfs) -> Result<Option<Snapshot>> {
	if !vfs.exists(path) {
		vfs.unwatch(path)?;
		return Ok(None);
	}

	let _frame = SnapshotFrame::enter(path, || format!("snapshotting {}", path.display()))?;

	if vfs.is_file(path) {
		new_snapshot_file(path, context, vfs)
	} else {
		snapshot_dir(path, context, vfs)
	}
}

/// Create a snapshot of a directory: the instance its winning init file
/// defines, or a plain folder
fn snapshot_dir(path: &Path, context: &Context, vfs: &Vfs) -> Result<Option<Snapshot>> {
	let layout = layout::read_layout(path, context, vfs)?;

	// Winner first; a candidate that yields nothing (a project file with an
	// empty tree) passes to the next, as before
	for candidate in &layout.inits {
		if let Some(snapshot) = snapshot_init(path, candidate, Some(&layout), context, vfs)? {
			layout::report(&layout, Some(&candidate.path));
			return Ok(Some(snapshot));
		}
	}

	layout::report(&layout, None);

	new_snapshot_dir(path, &layout, context, vfs)
}

/// Create a snapshot of a regular file,
/// example: `foo/bar.luau`
fn new_snapshot_file(path: &Path, context: &Context, vfs: &Vfs) -> Result<Option<Snapshot>> {
	if let Some((_, resolved)) = layout::file_rule(path, context) {
		let middleware = resolved.middleware;
		let name = resolved.name;

		let mut snapshot = middleware.read(path, context, vfs)?;

		if middleware != Middleware::Project {
			snapshot.set_name(&name);
			snapshot.meta.set_context(context);
			snapshot.meta.set_source(Source::file(path));
		} else if snapshot.class == "Folder" && snapshot.children.is_empty() {
			return Ok(None);
		}

		if let Some(instance_data) = get_instance_data(&name, Some(&snapshot.class), path, context, vfs)? {
			snapshot.apply_data(instance_data);
		}

		Ok(Some(snapshot))
	} else {
		Ok(None)
	}
}

/// Create a snapshot of a directory from one of its init files,
/// example: `foo/bar/init.luau`. The folder's other entries become children
/// — never its other init files: each would snapshot this folder again (the
/// unbounded recursion two init files in one folder used to cause)
fn snapshot_init(
	dir: &Path,
	candidate: &layout::InitCandidate,
	layout: Option<&layout::DirLayout>,
	context: &Context,
	vfs: &Vfs,
) -> Result<Option<Snapshot>> {
	let path = candidate.path.as_path();

	if path.contains(&[".src.luau"]) || path.contains(&[".src.lua"]) {
		wsync_warn!(
			"Your project uses legacy {} files which won't be supported in future versions of WSync. \
			Make sure to rename {} file to {} for future compatibility!",
			".src".bold(),
			path.to_string().bold(),
			path.to_string().replace(".src", "init").bold()
		);
	}

	let middleware = &candidate.middleware;
	let name = &candidate.name;

	let mut snapshot = middleware.read(path, context, vfs)?;

	if *middleware != Middleware::Project {
		snapshot.set_name(name);
		snapshot.meta.set_context(context);
		snapshot.meta.set_source(Source::child_file(dir, path));

		let read;
		let layout = match layout {
			Some(layout) => layout,
			None => {
				read = layout::read_layout(dir, context, vfs)?;
				&read
			}
		};

		for entry in &layout.children {
			if let Some(child_snapshot) = snapshot_child(entry, context, vfs)? {
				snapshot.add_child(child_snapshot);
			}
		}
	} else if snapshot.class == "Folder" && snapshot.children.is_empty() {
		return Ok(None);
	}

	if let Some(instance_data) = get_instance_data(name, Some(&snapshot.class), dir, context, vfs)? {
		snapshot.apply_data(instance_data);
	}

	Ok(Some(snapshot))
}

/// The class a directory takes from its own name, for the containers Studio
/// already provides inside a service — `StarterPlayerScripts` and its
/// character twin under `StarterPlayer`.
///
/// Such a directory *is* that container, not a Folder that happens to share
/// its name: syncing it as a Folder puts a second instance beside the real one,
/// and everything inside lands in the copy Roblox never runs. Only these
/// containers are inferred — an ordinary directory is still a Folder, and true
/// services are not, since a `Lighting` folder nested in code is a folder and
/// could not be parented there as a service anyway.
fn dir_container_class(name: &str) -> Option<&'static str> {
	match name {
		"StarterPlayerScripts" => Some("StarterPlayerScripts"),
		"StarterCharacterScripts" => Some("StarterCharacterScripts"),
		_ => None,
	}
}

/// Create snapshot of a directory,
/// example: `foo/bar`
fn new_snapshot_dir(path: &Path, layout: &layout::DirLayout, context: &Context, vfs: &Vfs) -> Result<Option<Snapshot>> {
	let mut snapshot = dir::read_dir(path, &layout.children, context, vfs)?;

	if let Some(class) = dir_container_class(&snapshot.name) {
		snapshot.set_class(class);
	}

	// A data sidecar still wins: it is the explicit statement of what the
	// instance is, and this inference is only a default
	if let Some(instance_data) = get_instance_data(&snapshot.name, Some(&snapshot.class), path, context, vfs)? {
		snapshot.apply_data(instance_data);
	}

	Ok(Some(snapshot))
}

fn get_instance_data(
	name: &str,
	class: Option<&str>,
	path: &Path,
	context: &Context,
	vfs: &Vfs,
) -> Result<Option<DataSnapshot>> {
	// Data sidecars ride along in every scope: code scope needs them for the
	// state a suffix cannot encode (attributes, tags, RunContext.Plugin), or
	// those instances re-flag the connect-time review forever
	for sync_rule in context.sync_rules_of_type(&Middleware::InstanceData, false) {
		if let Some(data_path) = sync_rule.locate(path, name, vfs.is_dir(path)) {
			if vfs.exists(&data_path) {
				let data = data::read_data(&data_path, class, vfs).with_desc(|| {
					format!(
						"Failed to get instance data at {}",
						data_path.display().to_string().bold()
					)
				})?;

				return Ok(Some(data));
			}
		}
	}

	Ok(None)
}

// ------------------------------------------------------------------- tests --

#[cfg(test)]
mod tests {
	use super::*;
	use crate::constants::default_sync_rules;
	use rbx_dom_weak::HashMapExt as _;

	/// The whole suffix scheme, read direction: one suffix, one meaning.
	#[test]
	fn every_script_suffix_resolves_to_exactly_one_middleware() {
		let resolve = |file: &str| -> Option<(Middleware, String)> {
			default_sync_rules()
				.iter()
				.find_map(|rule| rule.resolve(Path::new(file)))
				.map(|resolved| (resolved.middleware, resolved.name))
		};

		for (file, middleware) in [
			("Foo.server.luau", Middleware::ServerScript),
			("Foo.client.luau", Middleware::ClientScript),
			("Foo.local.luau", Middleware::LocalScript),
			("Foo.runserver.luau", Middleware::RunServerScript),
			("Foo.luau", Middleware::ModuleScript),
			("Foo.server.lua", Middleware::ServerScript),
			("Foo.local.lua", Middleware::LocalScript),
			("Foo.runserver.lua", Middleware::RunServerScript),
		] {
			let (resolved, name) = resolve(file).unwrap_or_else(|| panic!("{file} resolves to nothing"));
			assert_eq!(resolved, middleware, "{file}");
			assert_eq!(name, "Foo", "{file} must strip its whole suffix");
		}

		// A name that merely *contains* a suffix word is not that suffix:
		// `runserver` has no dot before `server`, so the `.server` rule must
		// not shadow it, and an unrelated tail stays a module.
		let (resolved, name) = resolve("Observer.luau").unwrap();
		assert_eq!(resolved, Middleware::ModuleScript);
		assert_eq!(name, "Observer");
	}

	/// The write direction consumes RunContext into the suffix choice — the
	/// exact inverse of the table above.
	#[test]
	fn from_class_encodes_run_context_in_the_middleware() {
		use rbx_dom_weak::types::{Enum, Variant};

		let script_with = |run_context: u32| -> (Option<Middleware>, Properties) {
			let mut properties = Properties::new();
			properties.insert(ustr("RunContext"), Variant::Enum(Enum::from_u32(run_context)));
			let middleware = Middleware::from_class("Script", Some(&mut properties));
			(middleware, properties)
		};

		let (middleware, leftovers) = script_with(0);
		assert_eq!(middleware, Some(Middleware::ServerScript));
		assert!(leftovers.is_empty(), "Legacy is the suffix itself");

		let (middleware, leftovers) = script_with(1);
		assert_eq!(middleware, Some(Middleware::RunServerScript));
		assert!(leftovers.is_empty());

		let (middleware, leftovers) = script_with(2);
		assert_eq!(middleware, Some(Middleware::ClientScript));
		assert!(leftovers.is_empty());

		// RunContext.Plugin has no suffix: fall back to `.server` and keep the
		// value as a property for the data sidecar to carry.
		let (middleware, leftovers) = script_with(3);
		assert_eq!(middleware, Some(Middleware::ServerScript));
		assert!(leftovers.contains_key(&ustr("RunContext")));

		let mut empty = Properties::new();
		assert_eq!(
			Middleware::from_class("LocalScript", Some(&mut empty)),
			Some(Middleware::LocalScript)
		);
		assert_eq!(
			Middleware::from_class("Script", None),
			Some(Middleware::ServerScript),
			"callers without properties get the Legacy fallback"
		);
	}

	/// Read and write agree: reading the file a class+RunContext writes to
	/// produces that class+RunContext back.
	#[test]
	fn the_suffix_scheme_round_trips_class_and_run_context() {
		use crate::middleware::luau::ScriptType;

		for (middleware, class, run_context) in [
			(Middleware::ServerScript, "Script", Some(0)),
			(Middleware::RunServerScript, "Script", Some(1)),
			(Middleware::ClientScript, "Script", Some(2)),
			(Middleware::LocalScript, "LocalScript", None),
			(Middleware::ModuleScript, "ModuleScript", None),
		] {
			let script_type: ScriptType = middleware.clone().into();
			let (read_class, read_run_context) = match &script_type {
				ScriptType::Legacy => ("Script", Some(0)),
				ScriptType::Server => ("Script", Some(1)),
				ScriptType::Client => ("Script", Some(2)),
				ScriptType::Local => ("LocalScript", None),
				ScriptType::Module => ("ModuleScript", None),
			};
			assert_eq!(read_class, class, "{middleware:?}");
			assert_eq!(read_run_context, run_context, "{middleware:?}");

			// And the writer picks the same middleware back.
			if class == "ModuleScript" {
				continue;
			}
			let mut properties = Properties::new();
			if let Some(run_context) = run_context {
				properties.insert(
					ustr("RunContext"),
					rbx_dom_weak::types::Variant::Enum(rbx_dom_weak::types::Enum::from_u32(run_context)),
				);
			}
			assert_eq!(
				Middleware::from_class(class, Some(&mut properties)),
				Some(middleware.clone()),
				"{middleware:?} must be its own inverse"
			);
		}
	}

	// ------------------------------------------------ layout and depth guard --

	use crate::{core::meta::Meta, project::Project};
	use std::{fs, thread};

	/// The processor thread runs with the default spawned-thread stack; the
	/// walk must fit it in a debug build, where frames are largest
	const PROCESSOR_STACK: usize = 2 * 1024 * 1024;

	fn fixture(tree: &str, files: &[(&str, &str)]) -> (tempfile::TempDir, PathBuf) {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path().canonicalize().unwrap();

		for (path, contents) in files {
			let path = root.join(path);
			fs::create_dir_all(path.parent().unwrap()).unwrap();
			fs::write(path, contents).unwrap();
		}

		fs::write(
			root.join("default.project.json"),
			format!(r#"{{"name":"fixture","tree":{tree},"scope":"full"}}"#),
		)
		.unwrap();

		(dir, root)
	}

	const SRC_TREE: &str = r#"{"$className":"DataModel","ServerScriptService":{"$path":"src"}}"#;

	/// Snapshots the fixture's project on a thread with the processor's stack
	fn snapshot_on_processor_stack(root: &Path) -> Result<Option<Snapshot>> {
		let project_path = root.join("default.project.json");

		thread::Builder::new()
			.stack_size(PROCESSOR_STACK)
			.spawn(move || {
				let project = Project::load(&project_path).unwrap();
				let meta = Meta::from_project(&project);

				new_snapshot(&project.path, &meta.context, &Vfs::new(false))
			})
			.unwrap()
			.join()
			.expect("the snapshot walk overflowed or panicked")
	}

	fn service(snapshot: &Snapshot) -> &Snapshot {
		snapshot
			.children
			.iter()
			.find(|child| child.name == "ServerScriptService")
			.unwrap()
	}

	fn names(snapshot: &Snapshot) -> Vec<(String, String)> {
		let mut names: Vec<(String, String)> = snapshot
			.children
			.iter()
			.map(|child| (child.name.clone(), child.class.to_string()))
			.collect();
		names.sort();
		names
	}

	/// The incident: `init.client.luau` beside `init.luau` recursed until the
	/// stack overflowed. Now the first sync rule wins, as for a lone init file
	#[test]
	fn two_init_files_resolve_by_rule_order_without_recursing() {
		let (_dir, root) = fixture(
			SRC_TREE,
			&[
				("src/CarFlareClient/init.client.luau", "print('client')"),
				("src/CarFlareClient/init.luau", "return {}"),
				("src/CarFlareClient/Child.luau", "return 1"),
			],
		);

		let snapshot = snapshot_on_processor_stack(&root).unwrap().unwrap();
		let flare = &service(&snapshot).children[0];

		assert_eq!(flare.name, "CarFlareClient");
		assert_eq!(flare.class, "Script", "`.client` precedes the plain module rule");
		assert_eq!(names(flare), vec![("Child".into(), "ModuleScript".into())]);
		assert!(
			flare
				.meta
				.source
				.paths()
				.iter()
				.all(|path| path.get_name() != "init.luau"),
			"the ignored init file is no source of the instance"
		);
	}

	#[test]
	fn a_project_file_outranks_an_init_script_in_its_folder() {
		let (_dir, root) = fixture(
			SRC_TREE,
			&[
				(
					"src/Nested/default.project.json",
					r#"{"name":"Nested","tree":{"$className":"Folder","Inner":{"$className":"Folder"}}}"#,
				),
				("src/Nested/init.luau", "return {}"),
			],
		);

		let snapshot = snapshot_on_processor_stack(&root).unwrap().unwrap();
		let nested = &service(&snapshot).children[0];

		assert_eq!(nested.class, "Folder");
		assert_eq!(names(nested), vec![("Inner".into(), "Folder".into())]);
	}

	/// The reshape leftovers of the incident, each resolved to one instance
	#[test]
	fn flat_and_folder_forms_of_one_instance_resolve_deterministically() {
		let (_dir, root) = fixture(
			SRC_TREE,
			&[
				// A folder with an init file wins over the flat file
				("src/EditableVideo.luau", "return 'flat'"),
				("src/EditableVideo/init.luau", "return 'folder'"),
				("src/EditableVideo/Part.luau", "return 1"),
				// An empty folder yields to the flat file
				("src/Moved.luau", "return 'moved'"),
				("src/Moved/.DS_Store", ""),
				// A folder with content but no init is a real second instance
				("src/Assets.luau", "return 'assets'"),
				("src/Assets/Icon.luau", "return 1"),
				// Staging is never projected
				("src/.Staged.wsync-reshape/reshape.json", "{}"),
			],
		);

		let snapshot = snapshot_on_processor_stack(&root).unwrap().unwrap();
		let service = service(&snapshot);

		assert_eq!(
			names(service),
			vec![
				("Assets".into(), "Folder".into()),
				("Assets".into(), "ModuleScript".into()),
				("EditableVideo".into(), "ModuleScript".into()),
				("Moved".into(), "ModuleScript".into()),
			]
		);

		let video = service
			.children
			.iter()
			.find(|child| child.name == "EditableVideo")
			.unwrap();
		assert_eq!(
			video.properties.get(&ustr("Source")),
			Some(&Variant::String("return 'folder'".into()))
		);
		assert_eq!(names(video), vec![("Part".into(), "ModuleScript".into())]);
	}

	/// A single path re-read (a VFS event) resolves the same way listing its
	/// parent does
	#[test]
	fn a_shadowed_path_snapshotted_alone_yields_its_winning_sibling() {
		let (_dir, root) = fixture(
			SRC_TREE,
			&[
				("src/X.luau", "return 'flat'"),
				("src/X/init.luau", "return 'folder'"),
				("src/Y.luau", "return 'y'"),
				("src/Y/.DS_Store", ""),
			],
		);

		let project = Project::load(&root.join("default.project.json")).unwrap();
		let context = Meta::from_project(&project).context;
		let vfs = Vfs::new(false);

		let x = new_snapshot(&root.join("src/X.luau"), &context, &vfs).unwrap().unwrap();
		assert_eq!(
			x.properties.get(&ustr("Source")),
			Some(&Variant::String("return 'folder'".into()))
		);

		let y = new_snapshot(&root.join("src/Y"), &context, &vfs).unwrap().unwrap();
		assert_eq!(y.class, "ModuleScript", "the empty folder yields to Y.luau");
	}

	#[test]
	fn a_deep_but_finite_layout_fits_the_processor_stack() {
		// Deep enough for any real project, shallow enough for the stack
		// budget of a debug build
		let levels = 48;
		let chain = vec!["d"; levels].join("/");
		let (_dir, root) = fixture(SRC_TREE, &[(&format!("src/{chain}/Leaf.luau"), "return 1")]);

		let snapshot = snapshot_on_processor_stack(&root).unwrap().unwrap();

		let mut node = service(&snapshot);
		let mut depth = 0;

		while let Some(child) = node.children.first() {
			node = child;
			depth += 1;
		}

		assert_eq!(depth, levels + 1);
		assert_eq!(node.name, "Leaf");
	}

	#[test]
	fn a_layout_past_the_depth_guard_fails_with_its_path() {
		let chain = vec!["d"; MAX_SNAPSHOT_DEPTH + 8].join("/");
		let (_dir, root) = fixture(SRC_TREE, &[(&format!("src/{chain}/Leaf.luau"), "return 1")]);

		let err = snapshot_on_processor_stack(&root).unwrap_err();
		let message = err.to_string();

		// Which half trips first depends on the build's frame sizes
		assert!(err.is::<SnapshotTooDeep>(), "{message}");
		assert!(
			message.contains(&format!("deeper than {MAX_SNAPSHOT_DEPTH} levels")) || message.contains("KiB of stack"),
			"{message}"
		);
		assert!(message.contains("/d/d/d"), "names the offending path: {message}");
		assert!(message.contains("the walk started snapshotting"), "{message}");
	}

	/// A `$path` naming the project's own directory used to recurse forever
	#[test]
	fn a_project_mapping_its_own_directory_fails_instead_of_overflowing() {
		let (_dir, root) = fixture(
			r#"{"$className":"DataModel","ReplicatedStorage":{"$path":"."}}"#,
			&[("Hello.luau", "return 1")],
		);

		let err = snapshot_on_processor_stack(&root).unwrap_err();

		assert!(err.is::<SnapshotTooDeep>(), "{err}");
		assert_eq!(
			err.to_string().matches("Failed to read").count(),
			0,
			"the depth error is not re-wrapped once per level: {err}"
		);
	}

	/// The OS stops resolving a symlink loop after a few dozen hops (ELOOP),
	/// so the walk ends on its own — it just must not overflow first
	#[cfg(unix)]
	#[test]
	fn a_symlink_back_to_an_ancestor_does_not_overflow() {
		let (_dir, root) = fixture(SRC_TREE, &[("src/A/Hello.luau", "return 1")]);

		std::os::unix::fs::symlink(root.join("src"), root.join("src/A/Loop")).unwrap();

		match snapshot_on_processor_stack(&root) {
			Ok(snapshot) => assert!(snapshot.is_some()),
			Err(err) => assert!(err.is::<SnapshotTooDeep>(), "{err}"),
		}
	}

	#[test]
	fn a_deep_binary_model_is_refused_not_overflowed() {
		use rbx_dom_weak::{InstanceBuilder, WeakDom};

		fn model(depth: usize) -> Vec<u8> {
			let mut builder = InstanceBuilder::new("Folder").with_name("Leaf");

			for _ in 0..depth {
				builder = InstanceBuilder::new("Folder").with_name("L").with_child(builder);
			}

			let dom = WeakDom::new(InstanceBuilder::new("DataModel").with_child(builder));
			let mut bytes = Vec::new();

			rbx_binary::to_writer(&mut bytes, &dom, dom.root().children()).unwrap();
			bytes
		}

		// Nested a few directories deep, where the snapshot walk has already
		// used some of the stack
		let write = |root: &Path, depth: usize| {
			let path = root.join("src/a/b/c/Deep.rbxm");
			fs::create_dir_all(path.parent().unwrap()).unwrap();
			fs::write(path, model(depth)).unwrap();
		};

		// Within the budget of even a debug build (~10 KiB per level there)
		let (_dir, root) = fixture(SRC_TREE, &[]);
		write(&root, 30);
		assert!(snapshot_on_processor_stack(&root).unwrap().is_some());

		let (_dir, root) = fixture(SRC_TREE, &[]);
		write(&root, 5000);
		let err = snapshot_on_processor_stack(&root).unwrap_err();
		assert!(err.to_string().contains("deeper than WSync follows"), "{err}");
	}

	#[test]
	fn a_deep_model_file_is_refused_not_overflowed() {
		fn nest(depth: usize) -> String {
			if depth == 0 {
				return r#"{"className":"Folder"}"#.into();
			}

			format!(r#"{{"className":"Folder","children":[{}]}}"#, nest(depth - 1))
		}

		// serde_json's own recursion limit stops a model nested past ~64
		// levels at parse time; the walk's guard backs it up either way
		let (_dir, root) = fixture(SRC_TREE, &[("src/Deep.model.json", &nest(60))]);
		assert!(snapshot_on_processor_stack(&root).unwrap().is_some());

		let (_dir, root) = fixture(SRC_TREE, &[("src/Deep.model.json", &nest(400))]);
		assert!(snapshot_on_processor_stack(&root).is_err());
	}
}
