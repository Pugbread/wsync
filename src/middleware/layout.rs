//! Directory layout resolution: which entries of a directory define its own
//! instance (init files), which become its children, and which are hidden
//! because a same-named sibling form wins.
//!
//! The snapshot walk (through the `Vfs`) and the disk hazard scanner
//! (`core::hazards`, through the plain filesystem) both read directories
//! through `read_layout`, so what the scanner reports is exactly what the
//! projection does.
//!
//! The rules, all deterministic:
//!
//! * **Several init files in one folder** (`init.client.luau` beside
//!   `init.luau`): the first sync rule that matches wins — the same order
//!   that resolves a single init file — and the others are ignored. Before
//!   this existed each init file snapshotted its siblings, the other init
//!   file snapshotted *its* siblings, and the walk recursed until the stack
//!   overflowed.
//! * **A flat file and a same-named folder** (`X.luau` beside `X/`, the
//!   leftovers of an interrupted file ↔ folder reshape): a folder with an
//!   init file wins over the flat file; an *empty* folder (nothing that
//!   would project) yields to the flat file instead of projecting a
//!   duplicate `X` Folder. A folder with real children but no init is a
//!   genuine second instance and is kept — the hazard scanner reports the
//!   duplicate name.
//! * **Reshape staging** (`.X.wsync-reshape`, see `core::helpers::reshape`)
//!   is never projected.

use colored::Colorize;
use std::{
	collections::{BTreeMap, HashSet},
	fs, io,
	path::{Path, PathBuf},
	sync::Mutex,
};

use super::{scope_allows_rule, Middleware};
use crate::{
	constants::BLACKLISTED_PATHS,
	core::meta::{Context, ResolvedSyncRule},
	ext::PathExt,
	vfs::Vfs,
	wsync_warn,
};

/// Name suffix of a flat ↔ folder reshape transaction directory
/// (`.<name>.wsync-reshape`). Staging is never projected; the daemon rolls
/// a leftover forward or back on its next start
pub const RESHAPE_SUFFIX: &str = ".wsync-reshape";

/// The directory operations layout resolution needs, so the same rules run
/// over the daemon's `Vfs` and over the plain filesystem
pub trait LayoutFs {
	fn list(&self, dir: &Path) -> io::Result<Vec<PathBuf>>;
	fn is_dir(&self, path: &Path) -> bool;
	fn is_file(&self, path: &Path) -> bool;
}

impl LayoutFs for Vfs {
	fn list(&self, dir: &Path) -> io::Result<Vec<PathBuf>> {
		self.read_dir(dir)
	}

	fn is_dir(&self, path: &Path) -> bool {
		Vfs::is_dir(self, path)
	}

	fn is_file(&self, path: &Path) -> bool {
		Vfs::is_file(self, path)
	}
}

/// The plain filesystem, for callers without a `Vfs` (the hazard scanner)
pub struct DiskFs;

impl LayoutFs for DiskFs {
	fn list(&self, dir: &Path) -> io::Result<Vec<PathBuf>> {
		fs::read_dir(dir)?
			.map(|entry| entry.map(|entry| entry.path()))
			.collect()
	}

	fn is_dir(&self, path: &Path) -> bool {
		path.is_dir()
	}

	fn is_file(&self, path: &Path) -> bool {
		path.is_file()
	}
}

/// An init file: a file whose name matches a sync rule's child pattern, so
/// it defines its folder's instance
#[derive(Debug, Clone)]
pub struct InitCandidate {
	pub path: PathBuf,
	/// Index of the first matching sync rule — lower wins
	pub rule: usize,
	pub middleware: Middleware,
	/// The instance name (the folder's name)
	pub name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShadowReason {
	/// A flat file is hidden by a same-named folder that has an init file
	FolderHasInit,
	/// An empty folder is hidden by a same-named flat file
	EmptyFolder,
}

/// A directory entry hidden because a sibling form of the same instance
/// wins
#[derive(Debug, Clone)]
pub struct Shadowed {
	pub hidden: PathBuf,
	pub by: PathBuf,
	pub name: String,
	pub reason: ShadowReason,
}

/// How one directory projects
#[derive(Debug, Clone, Default)]
pub struct DirLayout {
	pub dir: PathBuf,
	/// Init files, winner first (sync-rule order, then path)
	pub inits: Vec<InitCandidate>,
	/// Entries that project as children, sorted
	pub children: Vec<PathBuf>,
	/// Entries hidden by a same-named sibling form
	pub shadowed: Vec<Shadowed>,
	/// Reshape transaction leftovers (never projected)
	pub staging: Vec<PathBuf>,
}

impl DirLayout {
	/// More than one init file: all but the winner are ignored
	pub fn is_init_ambiguous(&self) -> bool {
		self.inits.len() > 1
	}
}

/// What a folder holds, for flat ↔ folder pairing
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FolderShape {
	/// It has an init file, so it defines a script (or other) instance
	HasInit,
	/// Nothing in it would project
	Empty,
	/// No init file, but children that project
	Content,
}

pub fn is_staging(path: &Path) -> bool {
	path.get_name().ends_with(RESHAPE_SUFFIX)
}

/// Blacklisted, ignored by the project's ignore globs, or reshape staging
pub fn is_excluded(path: &Path, context: &Context) -> bool {
	BLACKLISTED_PATHS.iter().any(|blacklisted| path.ends_with(blacklisted))
		|| is_staging(path)
		|| context.ignore_rules().iter().any(|rule| rule.matches(path))
}

/// The init-file reading of a file, if its name matches a child pattern
/// (first matching rule wins, exactly as for a lone init file)
pub fn init_candidate(path: &Path, context: &Context) -> Option<InitCandidate> {
	context.sync_rules().iter().enumerate().find_map(|(index, rule)| {
		rule.resolve_child(path)
			.filter(|resolved| scope_allows_rule(context, &resolved.middleware, path))
			.map(|resolved| InitCandidate {
				path: path.to_owned(),
				rule: index,
				middleware: resolved.middleware,
				name: resolved.name,
			})
	})
}

/// The regular (non-init) reading of a file: the first sync rule whose
/// pattern matches, with its index
pub fn file_rule(path: &Path, context: &Context) -> Option<(usize, ResolvedSyncRule)> {
	context.sync_rules().iter().enumerate().find_map(|(index, rule)| {
		rule.resolve(path)
			.filter(|resolved| scope_allows_rule(context, &resolved.middleware, path))
			.map(|resolved| (index, resolved))
	})
}

/// The instance name a flat file projects under, for pairing with a
/// same-named folder. Project files name themselves, so they never pair
fn flat_name(path: &Path, context: &Context) -> Option<(usize, String)> {
	file_rule(path, context)
		.filter(|(_, resolved)| resolved.middleware != Middleware::Project)
		.map(|(index, resolved)| (index, resolved.name))
}

pub fn folder_shape(dir: &Path, context: &Context, fs: &impl LayoutFs) -> io::Result<FolderShape> {
	let mut shape = FolderShape::Empty;

	for entry in fs.list(dir)? {
		if is_excluded(&entry, context) {
			continue;
		}

		if fs.is_dir(&entry) {
			shape = FolderShape::Content;
			continue;
		}

		if init_candidate(&entry, context).is_some() {
			return Ok(FolderShape::HasInit);
		}

		// Data sidecars never resolve here, so a folder holding only its
		// `init.meta.json` still counts as empty
		if file_rule(&entry, context).is_some() {
			shape = FolderShape::Content;
		}
	}

	Ok(shape)
}

/// Resolves how `dir` projects: its init files (winner first), its
/// children, and the entries a same-named sibling form hides
pub fn read_layout(dir: &Path, context: &Context, fs: &impl LayoutFs) -> io::Result<DirLayout> {
	let mut entries = fs.list(dir)?;

	// Directory order is filesystem-defined; sorting makes every decision
	// below (and the children's order) reproducible
	entries.sort();

	let mut layout = DirLayout {
		dir: dir.to_owned(),
		..DirLayout::default()
	};

	let mut files = Vec::new();
	let mut dirs = Vec::new();

	for entry in entries {
		if is_staging(&entry) {
			layout.staging.push(entry);
			continue;
		}

		if is_excluded(&entry, context) {
			continue;
		}

		if fs.is_file(&entry) {
			if let Some(candidate) = init_candidate(&entry, context) {
				layout.inits.push(candidate);
			} else {
				files.push(entry);
			}
		} else {
			dirs.push(entry);
		}
	}

	layout
		.inits
		.sort_by(|a, b| a.rule.cmp(&b.rule).then_with(|| a.path.cmp(&b.path)));

	let mut hidden: HashSet<PathBuf> = HashSet::new();

	if !dirs.is_empty() && !files.is_empty() {
		// Only a file named after a sibling folder can pair with it; the
		// prefix test keeps the rule resolution off every other file
		let dir_names: HashSet<&str> = dirs.iter().map(|dir| dir.get_name()).collect();
		let mut flats: BTreeMap<String, Vec<(usize, &PathBuf)>> = BTreeMap::new();

		for file in &files {
			let name = file.get_name();

			// An instance name is always a prefix of its file name (the
			// suffix or extension stripped)
			if !dir_names.iter().any(|dir| name.starts_with(dir)) {
				continue;
			}

			if let Some((rule, instance)) = flat_name(file, context) {
				if dir_names.contains(instance.as_str()) {
					flats.entry(instance).or_default().push((rule, file));
				}
			}
		}

		for folder in &dirs {
			let Some(pairs) = flats.get_mut(folder.get_name()) else {
				continue;
			};

			pairs.sort();

			match folder_shape(folder, context, fs)? {
				FolderShape::HasInit => {
					for (_, flat) in pairs.iter() {
						hidden.insert((*flat).to_owned());
						layout.shadowed.push(Shadowed {
							hidden: (*flat).to_owned(),
							by: folder.to_owned(),
							name: folder.get_name().to_owned(),
							reason: ShadowReason::FolderHasInit,
						});
					}
				}
				FolderShape::Empty => {
					hidden.insert(folder.to_owned());
					layout.shadowed.push(Shadowed {
						hidden: folder.to_owned(),
						by: pairs[0].1.to_owned(),
						name: folder.get_name().to_owned(),
						reason: ShadowReason::EmptyFolder,
					});
				}
				FolderShape::Content => {}
			}
		}
	}

	let mut children: Vec<PathBuf> = files
		.into_iter()
		.chain(dirs)
		.filter(|entry| !hidden.contains(entry))
		.collect();
	children.sort();

	layout.children = children;

	Ok(layout)
}

/// For a path snapshotted on its own (a VFS event re-reading one instance),
/// the sibling form that wins over it, if any: an empty folder yields to a
/// same-named flat file, a flat file yields to a same-named folder with an
/// init file. Listing a whole directory applies the same rule through
/// `read_layout`; this keeps a single-path re-read consistent with it
pub fn shadowing_form(path: &Path, context: &Context, fs: &impl LayoutFs) -> io::Result<Option<PathBuf>> {
	let Some(parent) = path.parent() else {
		return Ok(None);
	};

	if fs.is_dir(path) {
		if folder_shape(path, context, fs)? != FolderShape::Empty {
			return Ok(None);
		}

		let name = path.get_name();
		let mut flats: Vec<(usize, PathBuf)> = Vec::new();

		for entry in fs.list(parent)? {
			if !fs.is_file(&entry) || is_excluded(&entry, context) || init_candidate(&entry, context).is_some() {
				continue;
			}

			if let Some((rule, instance)) = flat_name(&entry, context) {
				if instance == name {
					flats.push((rule, entry));
				}
			}
		}

		flats.sort();

		return Ok(flats.into_iter().next().map(|(_, flat)| flat));
	}

	if init_candidate(path, context).is_some() {
		return Ok(None);
	}

	let Some((_, name)) = flat_name(path, context) else {
		return Ok(None);
	};

	let folder = parent.join(name);

	if fs.is_dir(&folder)
		&& !is_excluded(&folder, context)
		&& folder_shape(&folder, context, fs)? == FolderShape::HasInit
	{
		return Ok(Some(folder));
	}

	Ok(None)
}

/// What each directory with an ambiguity last warned about, so a hazard is
/// reported once per occurrence rather than on every re-snapshot an event
/// storm triggers. Cleared when the directory resolves cleanly again, so a
/// recurrence warns anew
static REPORTED: Mutex<BTreeMap<PathBuf, String>> = Mutex::new(BTreeMap::new());

/// Human messages for a layout's ambiguities. `winner` is the init file
/// that actually produced the instance (a project file with an empty tree
/// yields to the next candidate)
pub fn describe(layout: &DirLayout, winner: Option<&Path>) -> Vec<String> {
	let mut messages = Vec::new();

	if layout.is_init_ambiguous() {
		let winner = winner.unwrap_or(&layout.inits[0].path);
		let ignored: Vec<String> = layout
			.inits
			.iter()
			.filter(|candidate| candidate.path != winner)
			.map(|candidate| candidate.path.get_name().to_owned())
			.collect();

		messages.push(format!(
			"{} has {} init files: {} defines the instance, ignoring {}. Keep exactly one init file per folder",
			layout.dir.display(),
			layout.inits.len(),
			winner.get_name(),
			ignored.join(", ")
		));
	}

	for shadowed in &layout.shadowed {
		messages.push(match shadowed.reason {
			ShadowReason::FolderHasInit => format!(
				"{} and {} both define `{}`: the folder wins (it has an init file), ignoring {}. Delete the leftover flat file",
				shadowed.hidden.display(),
				shadowed.by.display(),
				shadowed.name,
				shadowed.hidden.get_name()
			),
			ShadowReason::EmptyFolder => format!(
				"{} is an empty folder beside {}, which defines the same `{}`: ignoring the empty folder. Delete it",
				shadowed.hidden.display(),
				shadowed.by.display(),
				shadowed.name
			),
		});
	}

	messages
}

/// Warns about a directory's ambiguities once per occurrence; returns
/// whether it warned
pub fn report(layout: &DirLayout, winner: Option<&Path>) -> bool {
	let messages = describe(layout, winner);
	let mut reported = REPORTED.lock().unwrap_or_else(|poisoned| poisoned.into_inner());

	if messages.is_empty() {
		reported.remove(&layout.dir);
		return false;
	}

	let signature = messages.join("\n");

	if reported.get(&layout.dir) == Some(&signature) {
		return false;
	}

	for message in &messages {
		wsync_warn!("{}", message.yellow());
	}

	reported.insert(layout.dir.clone(), signature);

	true
}

#[cfg(test)]
mod tests {
	use super::*;

	fn candidate(dir: &Path, name: &str, rule: usize) -> InitCandidate {
		InitCandidate {
			path: dir.join(name),
			rule,
			middleware: Middleware::ModuleScript,
			name: dir.get_name().to_owned(),
		}
	}

	#[test]
	fn an_ambiguity_warns_once_per_occurrence() {
		let dir = PathBuf::from("/virtual/warn-once/CarFlareClient");
		let ambiguous = DirLayout {
			dir: dir.clone(),
			inits: vec![candidate(&dir, "init.client.luau", 4), candidate(&dir, "init.luau", 7)],
			..DirLayout::default()
		};
		let resolved = DirLayout {
			dir: dir.clone(),
			inits: vec![candidate(&dir, "init.client.luau", 4)],
			..DirLayout::default()
		};

		assert!(report(&ambiguous, None), "the first sighting warns");
		assert!(!report(&ambiguous, None), "an event storm over it does not");
		assert!(!report(&ambiguous, Some(&dir.join("init.client.luau"))));

		assert!(!report(&resolved, None), "a clean folder never warns");
		assert!(report(&ambiguous, None), "a recurrence after it resolved warns again");

		let message = &describe(&ambiguous, None)[0];
		assert!(message.contains("CarFlareClient has 2 init files"), "{message}");
		assert!(
			message.contains("init.client.luau defines the instance, ignoring init.luau"),
			"{message}"
		);
	}
}
