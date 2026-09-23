//! Crash-safe flat ↔ folder reshapes.
//!
//! When a Studio instance gains its first child, its flat file `X.luau`
//! becomes the folder `X/init.luau` (promotion); when it loses its last child
//! the folder becomes a flat file again (demotion). Each form is several
//! filesystem entries (the script plus an optional data sidecar) and no single
//! rename turns one into the other, so a crash between the renames used to
//! leave both forms on disk (`X.luau` beside `X/init.luau`, or beside an empty
//! `X/`). The reshape therefore runs as a small journaled transaction inside a
//! sibling staging directory, `.X.wsync-reshape`:
//!
//! 1. The manifest (`reshape.json`: the old and new top-level entries and the
//!    file moves between them) is written atomically. Nothing has moved yet.
//! 2. The old form's top-level entries move into `old/`, the *primary* entry
//!    (the flat script, or the folder) first. From here the old form is gone.
//! 3. The files move from `old/` to their places in the new form under
//!    `new/`.
//! 4. The new form's entries move from `new/` into place, the primary entry
//!    (the folder, or the flat script) last — that rename commits.
//! 5. The staging directory, by now holding only emptied leftovers, goes.
//!
//! A crash at any point therefore leaves at most one resolvable form of the
//! instance, plus possibly the staging directory. The middleware never
//! projects staging (`middleware::layout::RESHAPE_SUFFIX`), the hazard
//! scanner reports it, and `recover` resolves it on the daemon's next start:
//! an old primary still in place means nothing had moved, so the transaction
//! is discarded; otherwise it is rolled forward to the committed state. Every
//! step checks before it moves, so recovery is idempotent.
//!
//! The daemon runs reshapes on its processor thread with the VFS watcher
//! paused, so none of these renames echo back as file changes.

use anyhow::{bail, Context as _, Result};
use log::debug;
use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};

use crate::{
	constants::BLACKLISTED_PATHS, core::hazards, ext::PathExt, middleware::layout::RESHAPE_SUFFIX, project::Project,
	vfs::Vfs, wsync_info, wsync_warn,
};

/// The transaction journal inside a staging directory
const MANIFEST: &str = "reshape.json";

const MANIFEST_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Manifest {
	version: u32,
	/// Top-level entries of the old form, relative to the parent directory;
	/// the first is the primary
	old: Vec<PathBuf>,
	/// Top-level entries of the new form, relative to the parent directory;
	/// the last is the primary
	new: Vec<PathBuf>,
	/// Files moving from the old form into the new one, relative to the
	/// parent directory
	moves: Vec<(PathBuf, PathBuf)>,
}

/// How `recover` resolved a leftover staging directory
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recovery {
	/// Nothing had moved yet: the old form stands and staging was discarded
	Discarded,
	/// The old form was already out of place: the new form was completed
	RolledForward,
}

/// The staging directory of a reshape of instance `name` in `parent`
pub fn staging_dir(parent: &Path, name: &str) -> PathBuf {
	parent.join(format!(".{name}{RESHAPE_SUFFIX}"))
}

/// One planned flat ↔ folder reshape
#[derive(Debug)]
pub struct Reshape {
	parent: PathBuf,
	name: String,
	manifest: Manifest,
}

impl Reshape {
	/// Plans a promotion: the flat script `flat` becomes the folder `folder`.
	/// `moves` are absolute file paths, old → new (the script into the init
	/// file, the data sidecar into the folder's); a move whose source does
	/// not exist (an absent sidecar) is dropped
	pub fn to_folder(flat: &Path, folder: &Path, moves: Vec<(PathBuf, PathBuf)>, vfs: &Vfs) -> Result<Self> {
		Self::plan(flat, folder, folder.get_name(), moves, vfs)
	}

	/// Plans a demotion: the folder `folder` becomes the flat script `flat`
	pub fn to_flat(folder: &Path, flat: &Path, moves: Vec<(PathBuf, PathBuf)>, vfs: &Vfs) -> Result<Self> {
		Self::plan(folder, flat, folder.get_name(), moves, vfs)
	}

	/// A reshape from the form whose primary entry is `from` to the one whose
	/// primary entry is `to`, for the instance `name`
	fn plan(from: &Path, to: &Path, name: &str, moves: Vec<(PathBuf, PathBuf)>, vfs: &Vfs) -> Result<Self> {
		let parent = from.get_parent().to_owned();

		if to.get_parent() != parent {
			bail!(
				"A reshape stays within one directory: {} and {} differ",
				from.display(),
				to.display()
			);
		}

		let relative = |path: &Path| -> Result<PathBuf> {
			let relative = path
				.strip_prefix(&parent)
				.with_context(|| format!("{} is outside {}", path.display(), parent.display()))?;

			if relative
				.components()
				.any(|component| !matches!(component, Component::Normal(_)))
			{
				bail!("{} is not a plain path inside {}", path.display(), parent.display());
			}

			Ok(relative.to_owned())
		};

		let top_level = |path: &Path| -> PathBuf { path.components().take(1).collect() };

		let old_primary = relative(from)?;
		let new_primary = relative(to)?;

		let mut old = vec![old_primary.clone()];
		let mut new = Vec::new();
		let mut planned = Vec::new();

		for (source, target) in moves {
			if !vfs.exists(&source) {
				continue;
			}

			let source = relative(&source)?;
			let target = relative(&target)?;

			let old_entry = top_level(&source);
			let new_entry = top_level(&target);

			if !old.contains(&old_entry) {
				old.push(old_entry);
			}

			if new_entry != new_primary && !new.contains(&new_entry) {
				new.push(new_entry);
			}

			planned.push((source, target));
		}

		new.push(new_primary);

		if old.iter().any(|entry| new.contains(entry)) {
			bail!("The reshape of {} would replace an entry with itself", from.display());
		}

		Ok(Self {
			name: name.to_owned(),
			parent,
			manifest: Manifest {
				version: MANIFEST_VERSION,
				old,
				new,
				moves: planned,
			},
		})
	}

	/// Where the new form would land, for a caller checking for collisions
	/// before committing to the reshape
	pub fn targets(&self) -> Vec<PathBuf> {
		self.manifest.new.iter().map(|entry| self.parent.join(entry)).collect()
	}

	/// Runs the transaction (see the module docs)
	pub fn run(self, vfs: &Vfs) -> Result<()> {
		let staging = staging_dir(&self.parent, &self.name);

		// A leftover of an earlier interrupted reshape of this same instance
		// settles first, so the two never interleave
		if vfs.exists(&staging) {
			recover(&staging, vfs)?;
		}

		let old_primary = self.parent.join(&self.manifest.old[0]);

		if !vfs.exists(&old_primary) {
			bail!("Nothing to reshape: {} does not exist", old_primary.display());
		}

		for target in self.targets() {
			if vfs.exists(&target) {
				bail!(
					"Cannot reshape {} into {}: it already exists",
					old_primary.display(),
					target.display()
				);
			}
		}

		debug!(
			"Reshaping {} into {}",
			old_primary.display(),
			self.parent.join(self.manifest.new.last().unwrap()).display()
		);

		vfs.create_dir(&staging.join("old"))?;
		vfs.create_dir(&staging.join("new"))?;

		// Written aside and renamed in, so a manifest is either whole or
		// absent — and absent means nothing has moved
		let manifest = serde_json::to_vec_pretty(&self.manifest)?;
		let pending = staging.join(format!("{MANIFEST}.tmp"));

		vfs.write(&pending, &manifest)?;
		vfs.rename(&pending, &staging.join(MANIFEST))?;

		advance(&staging, &self.parent, &self.manifest, vfs)
	}
}

/// Steps 2–5, each skipping what is already done — the forward path and the
/// roll-forward recovery are the same code
fn advance(staging: &Path, parent: &Path, manifest: &Manifest, vfs: &Vfs) -> Result<()> {
	let old_dir = staging.join("old");
	let new_dir = staging.join("new");

	// 2. The old form out of place, primary first
	for entry in &manifest.old {
		let source = parent.join(entry);
		let target = old_dir.join(entry);

		if vfs.exists(&source) && !vfs.exists(&target) {
			vfs.rename(&source, &target)?;
		}
	}

	// 3. The files into the new form
	for (source, target) in &manifest.moves {
		let source = old_dir.join(source);
		let target = new_dir.join(target);

		if vfs.exists(&source) && !vfs.exists(&target) {
			vfs.create_dir(target.get_parent())?;
			vfs.rename(&source, &target)?;
		}
	}

	// 4. The new form into place, primary last: that rename commits
	for entry in &manifest.new {
		let source = new_dir.join(entry);
		let target = parent.join(entry);

		if !vfs.exists(&source) {
			continue;
		}

		if vfs.exists(&target) {
			bail!(
				"Cannot finish the reshape into {}: it already exists. The interrupted reshape stays in {} — \
				move what you need out of it and delete it",
				target.display(),
				staging.display()
			);
		}

		vfs.rename(&source, &target)?;
	}

	// 5. Only emptied leftovers remain
	vfs.remove(staging)?;

	Ok(())
}

/// Resolves a staging directory an interrupted reshape left behind: discards
/// it when nothing had moved yet, otherwise rolls the reshape forward
pub fn recover(staging: &Path, vfs: &Vfs) -> Result<Recovery> {
	let parent = staging.get_parent();
	let manifest_path = staging.join(MANIFEST);

	if !vfs.exists(staging) {
		return Ok(Recovery::Discarded);
	}

	// The manifest lands before anything moves: without it the old form is
	// untouched and staging holds nothing but empty directories
	if !vfs.exists(&manifest_path) {
		vfs.remove(staging)?;
		return Ok(Recovery::Discarded);
	}

	let manifest: Manifest = serde_json::from_slice(&vfs.read(&manifest_path)?)
		.with_context(|| format!("Corrupted reshape manifest {}", manifest_path.display()))?;

	if manifest.version != MANIFEST_VERSION || manifest.old.is_empty() || manifest.new.is_empty() {
		bail!("Unsupported reshape manifest {}", manifest_path.display());
	}

	// The primary leaves first, so while it is in place nothing has moved
	if vfs.exists(&parent.join(&manifest.old[0])) {
		vfs.remove(staging)?;
		return Ok(Recovery::Discarded);
	}

	advance(staging, parent, &manifest, vfs)?;

	Ok(Recovery::RolledForward)
}

/// Settles every reshape an earlier daemon left interrupted in the project's
/// mapped directories. Run once at daemon start, before the first snapshot
pub fn recover_project(project: &Project, vfs: &Vfs) {
	for staging in hazards::reshape_leftovers(project) {
		match recover(&staging, vfs) {
			Ok(Recovery::Discarded) => wsync_info!(
				"Discarded an interrupted flat/folder reshape that had not started: {}",
				staging.display()
			),
			Ok(Recovery::RolledForward) => {
				wsync_info!("Finished an interrupted flat/folder reshape: {}", staging.display())
			}
			Err(err) => wsync_warn!(
				"Failed to recover the interrupted flat/folder reshape {}: {err}",
				staging.display()
			),
		}
	}
}

/// Whether `folder` holds nothing but the files a demotion moves out (and
/// OS junk). Anything else — an ignored or unprojected file — would be
/// deleted with the folder, so such a folder keeps its folder form
pub fn holds_only(folder: &Path, files: &[&Path], vfs: &Vfs) -> Result<bool> {
	for entry in vfs.read_dir(folder)? {
		if files.contains(&entry.as_path()) || BLACKLISTED_PATHS.iter().any(|junk| entry.ends_with(junk)) {
			continue;
		}

		return Ok(false);
	}

	Ok(true)
}

/// Whether `path` is a directory with nothing in it but OS junk
pub fn is_empty_dir(path: &Path, vfs: &Vfs) -> Result<bool> {
	if !vfs.is_dir(path) {
		return Ok(false);
	}

	holds_only(path, &[], vfs)
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::fs;

	fn setup(files: &[(&str, &str)]) -> (tempfile::TempDir, PathBuf, Vfs) {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path().canonicalize().unwrap();

		for (path, contents) in files {
			let path = root.join(path);
			fs::create_dir_all(path.parent().unwrap()).unwrap();
			fs::write(path, contents).unwrap();
		}

		(dir, root, Vfs::new(false))
	}

	fn promotion(root: &Path, vfs: &Vfs) -> Reshape {
		Reshape::to_folder(
			&root.join("X.luau"),
			&root.join("X"),
			vec![
				(root.join("X.luau"), root.join("X/init.luau")),
				(root.join("X.meta.json"), root.join("X/init.meta.json")),
			],
			vfs,
		)
		.unwrap()
	}

	fn demotion(root: &Path, vfs: &Vfs) -> Reshape {
		Reshape::to_flat(
			&root.join("X"),
			&root.join("X.luau"),
			vec![
				(root.join("X/init.luau"), root.join("X.luau")),
				(root.join("X/init.meta.json"), root.join("X.meta.json")),
			],
			vfs,
		)
		.unwrap()
	}

	/// Every entry under `root`, relative, sorted — the whole observable
	/// state of a scenario
	fn tree(root: &Path) -> Vec<String> {
		fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
			for entry in fs::read_dir(dir).unwrap() {
				let path = entry.unwrap().path();
				let relative = path.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");

				if path.is_dir() {
					out.push(format!("{relative}/"));
					walk(root, &path, out);
				} else {
					out.push(relative);
				}
			}
		}

		let mut out = Vec::new();
		walk(root, root, &mut out);
		out.sort();
		out
	}

	#[test]
	fn promotion_and_demotion_move_script_and_sidecar_together() {
		let (_dir, root, vfs) = setup(&[("X.luau", "return 1"), ("X.meta.json", "{}")]);

		promotion(&root, &vfs).run(&vfs).unwrap();
		assert_eq!(tree(&root), vec!["X/", "X/init.luau", "X/init.meta.json"]);
		assert_eq!(fs::read_to_string(root.join("X/init.luau")).unwrap(), "return 1");

		demotion(&root, &vfs).run(&vfs).unwrap();
		assert_eq!(tree(&root), vec!["X.luau", "X.meta.json"]);
		assert_eq!(fs::read_to_string(root.join("X.luau")).unwrap(), "return 1");
	}

	#[test]
	fn a_missing_sidecar_is_not_part_of_the_reshape() {
		let (_dir, root, vfs) = setup(&[("X.luau", "return 1")]);

		promotion(&root, &vfs).run(&vfs).unwrap();
		assert_eq!(tree(&root), vec!["X/", "X/init.luau"]);
	}

	#[test]
	fn a_reshape_never_overwrites_an_existing_target() {
		let (_dir, root, vfs) = setup(&[("X.luau", "return 1"), ("X/Other.luau", "return 2")]);

		assert!(promotion(&root, &vfs).run(&vfs).is_err());
		assert_eq!(tree(&root), vec!["X.luau", "X/", "X/Other.luau"], "nothing moved");
	}

	/// Simulates a crash after each step of both directions and checks the
	/// on-disk state is resolvable (at most one form of `X`, plus staging),
	/// then that recovery lands on exactly the pre- or post-reshape state
	#[test]
	fn a_crash_at_any_step_recovers_to_one_whole_form() {
		for direction in ["promote", "demote"] {
			for crash_after in 0..=5 {
				let initial: &[(&str, &str)] = if direction == "promote" {
					&[("X.luau", "return 1"), ("X.meta.json", "{\"tags\":[]}")]
				} else {
					&[("X/init.luau", "return 1"), ("X/init.meta.json", "{\"tags\":[]}")]
				};

				let (_dir, root, vfs) = setup(initial);
				let before = tree(&root);
				let reshape = if direction == "promote" {
					promotion(&root, &vfs)
				} else {
					demotion(&root, &vfs)
				};

				let parent = reshape.parent.clone();
				let manifest = reshape.manifest.clone();
				let staging = staging_dir(&parent, "X");

				// Replay the transaction by hand up to the crash point
				if crash_after >= 1 {
					vfs.create_dir(&staging.join("old")).unwrap();
					vfs.create_dir(&staging.join("new")).unwrap();
					vfs.write(&staging.join(MANIFEST), &serde_json::to_vec(&manifest).unwrap())
						.unwrap();
				}

				if crash_after >= 2 {
					// Only the primary: the most fragile moment
					let entry = &manifest.old[0];
					vfs.rename(&parent.join(entry), &staging.join("old").join(entry))
						.unwrap();
				}

				if crash_after >= 3 {
					for entry in &manifest.old[1..] {
						vfs.rename(&parent.join(entry), &staging.join("old").join(entry))
							.unwrap();
					}

					for (source, target) in &manifest.moves {
						let target = staging.join("new").join(target);
						fs::create_dir_all(target.parent().unwrap()).unwrap();
						vfs.rename(&staging.join("old").join(source), &target).unwrap();
					}
				}

				if crash_after >= 4 {
					// Everything but the committing primary
					for entry in &manifest.new[..manifest.new.len() - 1] {
						vfs.rename(&staging.join("new").join(entry), &parent.join(entry))
							.unwrap();
					}
				}

				if crash_after >= 5 {
					// Committed; only the staging cleanup is missing
					let entry = manifest.new.last().unwrap();
					vfs.rename(&staging.join("new").join(entry), &parent.join(entry))
						.unwrap();
				}

				// The crashed state: never two resolvable forms of X
				let crashed = tree(&root);
				let flat = crashed.iter().any(|entry| entry == "X.luau");
				let folder = crashed.iter().any(|entry| entry == "X/");
				assert!(
					!(flat && folder),
					"{direction} crash after step {crash_after} left both forms: {crashed:?}"
				);

				let recovery = recover(&staging, &vfs).unwrap();
				let after = tree(&root);

				let expected_after: Vec<String> = if direction == "promote" {
					vec!["X/".into(), "X/init.luau".into(), "X/init.meta.json".into()]
				} else {
					vec!["X.luau".into(), "X.meta.json".into()]
				};

				if crash_after < 2 {
					assert_eq!(recovery, Recovery::Discarded, "{direction} after step {crash_after}");
					assert_eq!(after, before, "{direction} after step {crash_after}");
				} else {
					assert_eq!(
						recovery,
						Recovery::RolledForward,
						"{direction} after step {crash_after}"
					);
					assert_eq!(after, expected_after, "{direction} after step {crash_after}");
				}

				// Recovery is idempotent: no staging remains to recover
				assert!(!staging.exists());
			}
		}
	}

	#[test]
	fn a_new_reshape_settles_a_leftover_of_the_same_instance_first() {
		let (_dir, root, vfs) = setup(&[("X.luau", "return 1")]);

		// A leftover whose manifest never landed
		fs::create_dir_all(staging_dir(&root, "X").join("old")).unwrap();

		promotion(&root, &vfs).run(&vfs).unwrap();
		assert_eq!(tree(&root), vec!["X/", "X/init.luau"]);
	}

	#[test]
	fn holds_only_ignores_os_junk_but_nothing_else() {
		let (_dir, root, vfs) = setup(&[("X/init.luau", ""), ("X/.DS_Store", "")]);
		let init = root.join("X/init.luau");

		assert!(holds_only(&root.join("X"), &[&init], &vfs).unwrap());

		fs::write(root.join("X/notes.txt"), "keep me").unwrap();
		assert!(!holds_only(&root.join("X"), &[&init], &vfs).unwrap());

		fs::create_dir_all(root.join("Empty")).unwrap();
		assert!(is_empty_dir(&root.join("Empty"), &vfs).unwrap());
		assert!(!is_empty_dir(&root.join("X"), &vfs).unwrap());
	}
}
