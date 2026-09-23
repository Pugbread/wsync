//! Disk-side hazard scanner: layout problems in a project's mapped
//! directories that make the projection ambiguous, lossy or fragile — found
//! from the files alone, with no daemon or Studio involved.
//!
//! It reads directories through `middleware::layout::read_layout`, the same
//! resolution the snapshot walk uses, so a hazard it reports is exactly what
//! the projection does (and which file wins).
//!
//! # API
//!
//! ```ignore
//! use wsync::{core::hazards, project::Project};
//!
//! let project = Project::load(&project_path)?;
//! let report = hazards::scan(&project);
//!
//! for hazard in &report.hazards {
//!     // hazard.kind      HazardKind — see below; serializes kebab-case
//!     // hazard.severity  Severity::Error | Severity::Warning
//!     // hazard.paths     every path involved (absolute), the winner first
//!     //                  when WSync resolves the ambiguity itself
//!     // hazard.winner    the path WSync projects, if it picks one
//!     // hazard.message   what is wrong, naming the paths
//!     // hazard.fix       the suggested fix
//! }
//!
//! report.errors();    // hazards of Severity::Error
//! report.to_json();   // { roots, directories, files, errors, warnings, hazards: [...] }
//! ```
//!
//! `scan` never fails: an unreadable directory or a cyclic layout becomes a
//! `scan-error` hazard. It follows the project's `$path` mappings and nested
//! project files, and honors the project's ignore globs and sync rules
//! (including code scope, where only `.luau` sources project).
//!
//! Hazard kinds:
//!
//! * `ambiguous-init` (error) — several init files in one folder
//!   (`init.client.luau` beside `init.luau`). The first sync rule wins
//!   (`winner`); the others are ignored.
//! * `flat-folder-pair` (error) — a flat file beside a same-named folder
//!   with an init file (`X.luau` beside `X/init.luau`): the leftover of an
//!   interrupted file ↔ folder reshape. The folder wins.
//! * `empty-folder` (error) — an empty folder beside a same-named flat file
//!   (`X/` beside `X.luau`); it is ignored instead of projecting a duplicate
//!   `X` Folder.
//! * `reshape-leftover` (error) — a `.X.wsync-reshape` staging directory of
//!   an interrupted reshape; the daemon finishes or rolls it back on start.
//! * `duplicate-sibling` (warning) — different entries projecting
//!   same-named siblings (`Foo.luau` beside `Foo.server.luau`, or beside a
//!   `Foo/` folder without an init file). Roblox allows it, but `Parent.Foo`
//!   resolves to either.
//! * `oversized-file` (warning) — an unignored file over
//!   `SOURCE_SIZE_LIMIT` (scripts and data) or `BINARY_SIZE_LIMIT` (models,
//!   and files no sync rule reads, which are still watched).
//! * `scan-error` (error) — a directory that could not be read, or a walk
//!   past `MAX_SNAPSHOT_DEPTH` (a cyclic symlink or `$path`).

use path_clean::PathClean;
use serde::Serialize;
use serde_json::{json, Value};
use std::{
	collections::{BTreeMap, HashSet},
	fmt::{self, Display, Formatter},
	fs,
	path::{Path, PathBuf},
};

use crate::{
	core::meta::{Context, Meta},
	ext::PathExt,
	middleware::{
		layout::{self, DiskFs, ShadowReason},
		Middleware, MAX_SNAPSHOT_DEPTH,
	},
	project::{Project, ProjectNode},
};

/// Scripts and data files (every text format a sync rule reads) over this
/// size are flagged: Studio's script editor and the sync protocol both
/// struggle with them, and they are usually generated or vendored content
pub const SOURCE_SIZE_LIMIT: u64 = 1024 * 1024;

/// Binary models, and files no sync rule reads (still watched, never
/// synced), over this size are flagged
pub const BINARY_SIZE_LIMIT: u64 = 16 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HazardKind {
	AmbiguousInit,
	FlatFolderPair,
	EmptyFolder,
	ReshapeLeftover,
	DuplicateSibling,
	OversizedFile,
	ScanError,
}

impl HazardKind {
	pub fn as_str(&self) -> &'static str {
		match self {
			HazardKind::AmbiguousInit => "ambiguous-init",
			HazardKind::FlatFolderPair => "flat-folder-pair",
			HazardKind::EmptyFolder => "empty-folder",
			HazardKind::ReshapeLeftover => "reshape-leftover",
			HazardKind::DuplicateSibling => "duplicate-sibling",
			HazardKind::OversizedFile => "oversized-file",
			HazardKind::ScanError => "scan-error",
		}
	}

	pub fn severity(&self) -> Severity {
		match self {
			HazardKind::DuplicateSibling | HazardKind::OversizedFile => Severity::Warning,
			_ => Severity::Error,
		}
	}
}

impl Display for HazardKind {
	fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
		f.write_str(self.as_str())
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Severity {
	/// The projection is ambiguous or a half-finished operation is on disk
	Error,
	/// Legal but fragile
	Warning,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Hazard {
	pub kind: HazardKind,
	pub severity: Severity,
	/// Every path involved, absolute; the winner first when WSync picks one
	pub paths: Vec<PathBuf>,
	/// The path WSync projects when it resolves the ambiguity itself
	pub winner: Option<PathBuf>,
	/// What is wrong, naming the paths
	pub message: String,
	/// The suggested fix
	pub fix: String,
}

impl Hazard {
	fn new(kind: HazardKind, paths: Vec<PathBuf>, message: String, fix: String) -> Self {
		Self {
			kind,
			severity: kind.severity(),
			paths,
			winner: None,
			message,
			fix,
		}
	}

	fn with_winner(mut self, winner: &Path) -> Self {
		self.winner = Some(winner.to_owned());
		self
	}
}

/// The result of one scan
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanReport {
	/// The mapped paths the scan started from
	pub roots: Vec<PathBuf>,
	/// Directories and files examined
	pub directories: usize,
	pub files: usize,
	pub hazards: Vec<Hazard>,
}

impl ScanReport {
	pub fn errors(&self) -> usize {
		self.count(Severity::Error)
	}

	pub fn warnings(&self) -> usize {
		self.count(Severity::Warning)
	}

	pub fn is_clean(&self) -> bool {
		self.hazards.is_empty()
	}

	fn count(&self, severity: Severity) -> usize {
		self.hazards.iter().filter(|hazard| hazard.severity == severity).count()
	}

	pub fn to_json(&self) -> Value {
		json!({
			"roots": self.roots,
			"directories": self.directories,
			"files": self.files,
			"errors": self.errors(),
			"warnings": self.warnings(),
			"hazards": self.hazards,
		})
	}
}

/// Scans every directory the project maps (see the module docs)
pub fn scan(project: &Project) -> ScanReport {
	let mut scanner = Scanner::default();
	let context = Meta::from_project(project).context;

	scanner.scan_project(project, &context, 0);

	let mut report = scanner.report;

	// Stable output: by kind, then by first path
	report
		.hazards
		.sort_by(|a, b| a.kind.cmp(&b.kind).then_with(|| a.paths.cmp(&b.paths)));

	report
}

/// The staging directories interrupted flat ↔ folder reshapes left in the
/// project's mapped directories
pub fn reshape_leftovers(project: &Project) -> Vec<PathBuf> {
	scan(project)
		.hazards
		.into_iter()
		.filter(|hazard| hazard.kind == HazardKind::ReshapeLeftover)
		.filter_map(|hazard| hazard.paths.into_iter().next())
		.collect()
}

#[derive(Default)]
struct Scanner {
	report: ScanReport,
	/// Canonical directories and project files already scanned: two
	/// mappings of one directory are reported once
	visited: HashSet<PathBuf>,
	/// Canonical directories and project files on the current walk: meeting
	/// one again is a cycle the snapshot walk would refuse
	ancestors: Vec<PathBuf>,
}

impl Scanner {
	/// Enters a directory or project file; `false` when it was scanned
	/// already (a cycle is reported as a hazard on the way)
	fn enter(&mut self, path: &Path) -> bool {
		let canonical = path.canonicalize().unwrap_or_else(|_| path.to_owned());

		if self.ancestors.contains(&canonical) {
			self.report.hazards.push(Hazard::new(
				HazardKind::ScanError,
				vec![path.to_owned()],
				format!(
					"{} contains itself (through a symlink or a project `$path`); the snapshot walk refuses the cycle",
					path.display()
				),
				"Point the symlink or `$path` somewhere that is not an ancestor".into(),
			));

			return false;
		}

		if !self.visited.insert(canonical.clone()) {
			return false;
		}

		self.ancestors.push(canonical);

		true
	}

	fn leave(&mut self) {
		self.ancestors.pop();
	}

	fn scan_project(&mut self, project: &Project, context: &Context, depth: usize) {
		if !self.enter(&project.path) {
			return;
		}

		let mut mapped = Vec::new();
		collect_paths(&project.node, &project.path, &mut mapped);

		for path in mapped {
			if !path.exists() {
				// A missing `$path` is the middleware's warning to give
				continue;
			}

			self.report.roots.push(path.clone());
			self.scan_path(&path, context, depth + 1);
		}

		self.leave();
	}

	fn scan_path(&mut self, path: &Path, context: &Context, depth: usize) {
		if layout::is_excluded(path, context) {
			return;
		}

		if path.is_dir() {
			self.scan_dir(path, context, depth);
		} else if path.is_file() {
			// A mapped nested project file maps paths of its own
			if let Some((_, resolved)) = layout::file_rule(path, context) {
				if resolved.middleware == Middleware::Project {
					self.scan_nested_project(path, context, depth);
					return;
				}
			}

			self.check_size(path, context);
		}
	}

	fn scan_nested_project(&mut self, path: &Path, parent: &Context, depth: usize) {
		match Project::load(path) {
			Ok(project) => {
				let mut context = Meta::from_project(&project).context;

				// A nested project without its own scope inherits the
				// parent's, as in the middleware
				if project.scope.is_none() {
					context.set_scope(parent.scope());
				}

				self.scan_project(&project, &context, depth + 1);
			}
			Err(err) => self.report.hazards.push(Hazard::new(
				HazardKind::ScanError,
				vec![path.to_owned()],
				format!("{} does not load: {err}", path.display()),
				"Fix the nested project file".into(),
			)),
		}
	}

	fn scan_dir(&mut self, dir: &Path, context: &Context, depth: usize) {
		if depth > MAX_SNAPSHOT_DEPTH {
			self.report.hazards.push(Hazard::new(
				HazardKind::ScanError,
				vec![dir.to_owned()],
				format!(
					"{} is nested deeper than {MAX_SNAPSHOT_DEPTH} levels below its mapping; the snapshot walk refuses it",
					dir.display()
				),
				"Look for a symlink or project `$path` pointing at an ancestor directory, or flatten the layout".into(),
			));
			return;
		}

		if !self.enter(dir) {
			return;
		}

		self.walk_dir(dir, context, depth);
		self.leave();
	}

	fn walk_dir(&mut self, dir: &Path, context: &Context, depth: usize) {
		let layout = match layout::read_layout(dir, context, &DiskFs) {
			Ok(layout) => layout,
			Err(err) => {
				self.report.hazards.push(Hazard::new(
					HazardKind::ScanError,
					vec![dir.to_owned()],
					format!("{} could not be read: {err}", dir.display()),
					"Check the directory's permissions".into(),
				));
				return;
			}
		};

		self.report.directories += 1;

		self.check_layout(&layout);

		for init in &layout.inits {
			self.check_size(&init.path, context);
		}

		// A folder defined by a project file projects that project's tree,
		// never its own entries
		if let Some(winner) = layout.inits.first() {
			if winner.middleware == Middleware::Project {
				self.scan_nested_project(&winner.path, context, depth);
				return;
			}
		}

		self.check_duplicates(dir, &layout, context);

		for child in &layout.children {
			if child.is_dir() {
				self.scan_dir(child, context, depth + 1);
			} else {
				self.check_size(child, context);
			}
		}
	}

	/// The ambiguities `read_layout` resolved: init files, flat/folder
	/// pairs, reshape staging
	fn check_layout(&mut self, layout: &layout::DirLayout) {
		let dir = &layout.dir;

		if layout.is_init_ambiguous() {
			let winner = &layout.inits[0].path;
			let ignored: Vec<&str> = layout.inits[1..].iter().map(|init| init.path.get_name()).collect();

			self.report.hazards.push(
				Hazard::new(
					HazardKind::AmbiguousInit,
					layout.inits.iter().map(|init| init.path.clone()).collect(),
					format!(
						"{} has {} init files: WSync uses {} and ignores {}",
						dir.display(),
						layout.inits.len(),
						winner.get_name(),
						ignored.join(", ")
					),
					format!(
						"Keep exactly one init file in {}: delete or rename {}",
						dir.display(),
						ignored.join(", ")
					),
				)
				.with_winner(winner),
			);
		}

		for shadowed in &layout.shadowed {
			let hazard = match shadowed.reason {
				ShadowReason::FolderHasInit => Hazard::new(
					HazardKind::FlatFolderPair,
					vec![shadowed.by.clone(), shadowed.hidden.clone()],
					format!(
						"{} and {} both define `{}`: WSync uses the folder (it has an init file) and ignores the flat file",
						shadowed.hidden.display(),
						shadowed.by.display(),
						shadowed.name
					),
					format!(
						"Delete {} once {}'s init file holds its current source",
						shadowed.hidden.display(),
						shadowed.by.display()
					),
				),
				ShadowReason::EmptyFolder => Hazard::new(
					HazardKind::EmptyFolder,
					vec![shadowed.by.clone(), shadowed.hidden.clone()],
					format!(
						"{} is an empty folder beside {}, which defines the same `{}`: WSync ignores the folder",
						shadowed.hidden.display(),
						shadowed.by.display(),
						shadowed.name
					),
					format!("Delete the empty folder {}", shadowed.hidden.display()),
				),
			};

			self.report.hazards.push(hazard.with_winner(&shadowed.by));
		}

		for staging in &layout.staging {
			let name = staging
				.get_name()
				.trim_start_matches('.')
				.trim_end_matches(layout::RESHAPE_SUFFIX);

			self.report.hazards.push(Hazard::new(
				HazardKind::ReshapeLeftover,
				vec![staging.clone()],
				format!(
					"{} is an interrupted flat/folder reshape of `{name}`; WSync does not project it",
					staging.display()
				),
				"Start the daemon (`wsync daemon start`), which finishes or rolls back interrupted reshapes; \
				or recover what you need from it and delete it"
					.into(),
			));
		}
	}

	/// Children that project as same-named siblings
	fn check_duplicates(&mut self, dir: &Path, layout: &layout::DirLayout, context: &Context) {
		let mut names: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();

		for child in &layout.children {
			let name = if child.is_dir() {
				Some(child.get_name().to_owned())
			} else {
				// Project files name themselves; data sidecars and unread
				// files project nothing
				layout::file_rule(child, context)
					.filter(|(_, resolved)| resolved.middleware != Middleware::Project)
					.map(|(_, resolved)| resolved.name)
			};

			if let Some(name) = name {
				names.entry(name).or_default().push(child.clone());
			}
		}

		for (name, paths) in names {
			if paths.len() < 2 {
				continue;
			}

			let listed: Vec<&str> = paths.iter().map(|path| path.get_name()).collect();
			let folder = paths.iter().find(|path| path.is_dir());

			let fix = match folder {
				Some(folder) => format!(
					"Rename all but one; if {} holds `{name}`'s children, move the script into it as its init file",
					folder.display()
				),
				None => "Rename or delete all but one".to_owned(),
			};

			self.report.hazards.push(Hazard::new(
				HazardKind::DuplicateSibling,
				paths.clone(),
				format!(
					"{} all project as `{name}` in {}; Roblox keeps same-named siblings, but `Parent.{name}` resolves to either",
					listed.join(", "),
					dir.display()
				),
				fix,
			));
		}
	}

	fn check_size(&mut self, path: &Path, context: &Context) {
		self.report.files += 1;

		let Ok(metadata) = fs::metadata(path) else {
			return;
		};

		let middleware = layout::init_candidate(path, context)
			.map(|init| init.middleware)
			.or_else(|| layout::file_rule(path, context).map(|(_, resolved)| resolved.middleware));

		let (limit, what, fix) = match middleware {
			Some(Middleware::RbxmModel | Middleware::RbxmxModel) => (
				BINARY_SIZE_LIMIT,
				"binary models",
				"Split the model, or keep it in Studio and add the file to the project's `ignoreGlobs`",
			),
			Some(_) => (
				SOURCE_SIZE_LIMIT,
				"scripts and data",
				"Split it, or add it to the project's `ignoreGlobs` if Studio does not need it",
			),
			None => (
				BINARY_SIZE_LIMIT,
				"files WSync does not sync",
				"WSync watches it without syncing it: add it to the project's `ignoreGlobs`",
			),
		};

		if metadata.len() > limit {
			self.report.hazards.push(Hazard::new(
				HazardKind::OversizedFile,
				vec![path.to_owned()],
				format!(
					"{} is {}, over the {} limit for {what}",
					path.display(),
					human_size(metadata.len()),
					human_size(limit)
				),
				fix.into(),
			));
		}
	}
}

/// Every `$path` a project maps, absolute
fn collect_paths(node: &ProjectNode, project_path: &Path, out: &mut Vec<PathBuf>) {
	if let Some(path) = &node.path {
		out.push(project_path.with_file_name(path.path()).clean());
	}

	for child in node.tree.values() {
		collect_paths(child, project_path, out);
	}
}

fn human_size(bytes: u64) -> String {
	const MIB: f64 = 1024.0 * 1024.0;

	if bytes >= 1024 * 1024 {
		format!("{:.1} MiB", bytes as f64 / MIB)
	} else {
		format!("{:.1} KiB", bytes as f64 / 1024.0)
	}
}
