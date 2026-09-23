use std::path::Path;

use log::{error, trace};
use rbx_dom_weak::types::Ref;

use crate::{
	core::{
		changes::Changes,
		meta::{Context, SourceKind},
		snapshot::{Snapshot, UpdatedSnapshot},
		tree::Tree,
	},
	ext::PathExt,
	middleware::{new_snapshot, project::new_snapshot_node},
	stats, util,
	vfs::Vfs,
};

pub fn process_changes(id: Ref, tree: &mut Tree, vfs: &Vfs) -> Option<Changes> {
	trace!("Processing changes for instance: {id:?}");

	let mut changes = Changes::new();

	let meta = tree.get_meta(id)?;
	let source = meta.source.get();

	let process_path = |path: &Path| -> Option<Option<Snapshot>> {
		match new_snapshot(path, &meta.context, vfs) {
			Ok(snapshot) => Some(snapshot),
			Err(err) => {
				error!("Failed to process changes: {err}, source: {source:?}");
				None
			}
		}
	};

	let snapshot = match source {
		SourceKind::Project(name, path, node, node_path) => {
			if node_path.is_root() {
				process_path(path)?
			} else {
				match new_snapshot_node(name, path, *node.clone(), node_path.clone(), &meta.context, vfs) {
					Ok(snapshot) => Some(snapshot),
					Err(err) => {
						error!("Failed to process changes: {err}, source: {source:?}");
						return Some(changes);
					}
				}
			}
		}
		SourceKind::Path(path) => process_path(path)?,
		SourceKind::None => panic!(
			"Fatal processing error: `SourceKind::None` should not be present in the tree! Id: {id:?}, meta: {meta:#?}"
		),
	};

	// A source that is gone may have been renamed to a sibling that carries
	// the same instance under another class (`find_class_change`)
	let snapshot = snapshot.or_else(|| match source {
		SourceKind::Path(path) => {
			let name = &tree.get_instance(id)?.name;
			find_class_change(path, name, &meta.context, tree, vfs)
		}
		_ => None,
	});

	// Handle additions, modifications and child removals
	if let Some(snapshot) = snapshot {
		process_child_changes(id, snapshot, &mut changes, tree);
	// Handle regular removals
	} else {
		tree.remove_instance(id);
		changes.remove(id);
	}

	Some(changes)
}

fn process_child_changes(id: Ref, mut snapshot: Snapshot, changes: &mut Changes, tree: &mut Tree) {
	// Process instance changes
	let mut updated_snapshot = UpdatedSnapshot::new(id);

	updated_snapshot.meta = if snapshot.meta != *tree.get_meta(id).expect("Instance meta not found") {
		tree.update_meta(id, snapshot.meta.clone());
		Some(snapshot.meta)
	} else {
		None
	};

	let instance = tree.get_instance_mut(id).unwrap();

	updated_snapshot.name = if snapshot.name != instance.name {
		instance.name.clone_from(&snapshot.name);
		Some(snapshot.name)
	} else {
		None
	};

	updated_snapshot.class = if snapshot.class != instance.class {
		instance.class.clone_from(&snapshot.class);
		Some(snapshot.class)
	} else {
		None
	};

	updated_snapshot.properties = if snapshot.properties != instance.properties {
		instance.properties.clone_from(&snapshot.properties);
		Some(snapshot.properties)
	} else {
		None
	};

	if !updated_snapshot.is_empty() {
		// Track `lines_synced` stat
		if let Some(properties) = &updated_snapshot.properties {
			let loc = util::count_loc_from_properties(properties);
			stats::lines_synced(loc as u32);
		}

		changes.update(updated_snapshot);
	}

	let mut hydrated = vec![false; snapshot.children.len()];
	let mut unpaired = Vec::new();

	// Pair instances by name and class
	#[allow(clippy::unnecessary_to_owned)]
	for child_id in instance.children().to_owned() {
		let instance = tree.get_instance(child_id).unwrap();

		if let Some(index) = pair_child(&snapshot.children, &hydrated, &instance.name, Some(&instance.class)) {
			hydrated[index] = true;
			snapshot.children[index].set_id(child_id);
		} else {
			unpaired.push(child_id);
		}
	}

	// A child left without a partner that has a same-named snapshot left over
	// is one instance that changed class — a directory gaining or losing its
	// `init.luau`, a script renamed to another suffix — not a removal and an
	// unrelated addition. Pairing them keeps the ref, so the change reaches
	// Studio as one class-change update the plugin swaps in place, children
	// and all; a removal/addition pair instead replaces the subtree, and
	// leaves a duplicate beside it whenever the removal does not land. Only
	// what is still unpaired now is really gone
	for child_id in unpaired {
		let name = &tree.get_instance(child_id).unwrap().name;

		if let Some(index) = pair_child(&snapshot.children, &hydrated, name, None) {
			hydrated[index] = true;
			snapshot.children[index].set_id(child_id);
		} else {
			tree.remove_instance(child_id);
			changes.remove(child_id);
		}
	}

	// Process child changes and find new children
	for child in snapshot.children {
		if child.id.is_some() {
			process_child_changes(child.id, child, changes, tree);
		} else {
			let mut child = child;

			insert_children(&mut child, id, tree);

			// Track `lines_synced` stat
			let loc = util::count_loc_from_properties(&child.properties);
			stats::lines_synced(loc as u32);

			changes.add(child, id);
		}
	}
}

/// The first snapshot child not yet paired that has this name — and this
/// class, when one is given
fn pair_child(children: &[Snapshot], hydrated: &[bool], name: &str, class: Option<&str>) -> Option<usize> {
	children.iter().enumerate().position(|(index, child)| {
		!hydrated[index] && child.name == name && class.is_none_or(|class| child.class == class)
	})
}

/// The instance whose source is gone may not be gone at all. A rename that
/// changes a script's suffix (`X.server.luau` → `X.luau`) reaches the
/// processor as the old path's deletion and the new path's creation, and the
/// deletion is usually processed first — read on its own, that is a removal
/// followed by an unrelated addition: a new ref, the Studio-side children of
/// the old instance lost, and a duplicate left beside the new one whenever
/// the removal is held back while the addition goes through. When a sibling
/// that nothing in the tree claims yet now resolves to the same instance
/// name, the instance changed class instead: its snapshot, processed against
/// the same ref, becomes one class-change update.
///
/// Cheap on the common path — a real deletion costs one directory listing:
/// names are resolved from the sync rules before any candidate is read.
fn find_class_change(path: &Path, name: &str, context: &Context, tree: &Tree, vfs: &Vfs) -> Option<Snapshot> {
	let parent = path.parent()?;
	let entries = vfs.read_dir(parent).ok()?;

	for entry in entries {
		if entry == path || tree.get_ids(&entry).is_some() {
			continue;
		}

		let candidate = if vfs.is_dir(&entry) {
			Some(entry.get_name().to_owned())
		} else {
			context
				.sync_rules()
				.iter()
				.find_map(|rule| rule.resolve(&entry))
				.map(|resolved| resolved.name)
		};

		if candidate.as_deref() != Some(name) {
			continue;
		}

		match new_snapshot(&entry, context, vfs) {
			Ok(Some(snapshot)) if snapshot.name == name => {
				trace!(
					"{} replaces {}: same instance, new class",
					entry.display(),
					path.display()
				);
				return Some(snapshot);
			}
			Ok(_) => {}
			Err(err) => error!("Failed to read a possible replacement of {}: {err}", path.display()),
		}
	}

	None
}

fn insert_children(snapshot: &mut Snapshot, parent: Ref, tree: &mut Tree) {
	let id = tree.insert_instance(snapshot.clone(), parent);

	snapshot.set_id(id);

	for child in snapshot.children.iter_mut() {
		insert_children(child, id, tree);
	}
}

#[cfg(test)]
mod tests {
	//! A class change on disk — a directory gaining or losing `init.luau`, a
	//! script renamed to another suffix — must reach Studio as one update
	//! that keeps the instance's ref, never as an addition beside a removal
	//! (which leaves a duplicate in Studio whenever the removal does not land).
	//! Each test drives `process_changes` the way the processor's VFS handler
	//! does: the event path resolves to the nearest tracked ancestor-or-self.

	use super::*;
	use crate::{core::meta::Meta, project::Project};
	use std::{fs, path::PathBuf};

	struct Fixture {
		_dir: tempfile::TempDir,
		src: PathBuf,
		tree: Tree,
		vfs: Vfs,
	}

	impl Fixture {
		/// A full-scope place project with `src/` synced as ServerScriptService
		fn new(files: &[(&str, &str)]) -> Self {
			let dir = tempfile::tempdir().unwrap();
			let root = dir.path().canonicalize().unwrap();

			for (path, contents) in files {
				let path = root.join("src").join(path);
				fs::create_dir_all(path.parent().unwrap()).unwrap();
				fs::write(path, contents).unwrap();
			}

			fs::write(
				root.join("default.project.json"),
				r#"{"name":"fixture","tree":{"$className":"DataModel","ServerScriptService":{"$path":"src"}},"scope":"full"}"#,
			)
			.unwrap();

			let project = Project::load(&root.join("default.project.json")).unwrap();
			let vfs = Vfs::new(false);
			let meta = Meta::from_project(&project);
			let snapshot = new_snapshot(&project.path, &meta.context, &vfs).unwrap().unwrap();

			Self {
				_dir: dir,
				src: root.join("src"),
				tree: Tree::new(snapshot),
				vfs,
			}
		}

		fn path(&self, relative: &str) -> PathBuf {
			self.src.join(relative)
		}

		/// What the VFS handler does with an event at `relative`
		fn event(&mut self, relative: &str) -> Changes {
			let path = self.path(relative);
			let mut current = path.as_path();

			let ids = loop {
				if let Some(ids) = self.tree.get_ids(current) {
					break ids.to_owned();
				}

				current = current.parent().expect("no tracked ancestor");
			};

			let mut changes = Changes::new();

			for id in ids {
				if let Some(processed) = process_changes(id, &mut self.tree, &self.vfs) {
					changes.extend(processed);
				}
			}

			changes
		}

		/// The ref of the instance at `ServerScriptService/<name>`
		fn child(&self, name: &str) -> Ref {
			self.child_of(self.service(), name)
		}

		fn child_of(&self, parent: Ref, name: &str) -> Ref {
			let parent = self.tree.get_instance(parent).unwrap();

			*parent
				.children()
				.iter()
				.find(|child| self.tree.get_instance(**child).unwrap().name == name)
				.unwrap_or_else(|| panic!("no child {name}"))
		}

		fn service(&self) -> Ref {
			*self.tree.get_ids(&self.src).unwrap().first().unwrap()
		}

		fn class_of(&self, id: Ref) -> String {
			self.tree.get_instance(id).unwrap().class.to_string()
		}

		fn count_named(&self, name: &str) -> usize {
			let service = self.tree.get_instance(self.service()).unwrap();

			service
				.children()
				.iter()
				.filter(|child| self.tree.get_instance(**child).unwrap().name == name)
				.count()
		}
	}

	/// Exactly one update — a class change to `class` on `id` — and nothing else
	fn assert_class_change(changes: &Changes, id: Ref, class: &str) {
		assert!(
			changes.additions.is_empty() && changes.removals.is_empty(),
			"a class change must not be an addition/removal pair: {changes:#?}"
		);
		assert_eq!(changes.updates.len(), 1, "{changes:#?}");
		assert_eq!(changes.updates[0].id, id, "the update keeps the instance's ref");
		assert_eq!(changes.updates[0].class.as_deref(), Some(class));
	}

	#[test]
	fn a_directory_gaining_and_losing_init_luau_is_one_class_change() {
		let mut fixture = Fixture::new(&[("Tests/A.luau", "return 1"), ("Tests/Sub/B.server.luau", "print(1)")]);
		let tests = fixture.child("Tests");
		let a = fixture.child_of(tests, "A");
		let sub = fixture.child_of(tests, "Sub");

		fs::write(fixture.path("Tests/init.luau"), "return {}").unwrap();
		let changes = fixture.event("Tests/init.luau");

		assert_class_change(&changes, tests, "ModuleScript");
		assert_eq!(fixture.class_of(tests), "ModuleScript");
		assert_eq!(fixture.child_of(tests, "A"), a, "children keep their refs");
		assert_eq!(fixture.child_of(tests, "Sub"), sub);

		fs::remove_file(fixture.path("Tests/init.luau")).unwrap();
		let changes = fixture.event("Tests/init.luau");

		assert_class_change(&changes, tests, "Folder");
		assert_eq!(fixture.count_named("Tests"), 1);
	}

	#[test]
	fn renaming_init_server_luau_to_init_luau_is_one_class_change() {
		let mut fixture = Fixture::new(&[("Tests/init.server.luau", "print(0)"), ("Tests/A.luau", "return 1")]);
		let tests = fixture.child("Tests");

		assert_eq!(fixture.class_of(tests), "Script");

		fs::rename(fixture.path("Tests/init.server.luau"), fixture.path("Tests/init.luau")).unwrap();

		let deleted = fixture.event("Tests/init.server.luau");
		let created = fixture.event("Tests/init.luau");

		assert_class_change(&deleted, tests, "ModuleScript");
		assert!(
			created.is_empty(),
			"the second half of the rename is already applied: {created:#?}"
		);

		fs::rename(fixture.path("Tests/init.luau"), fixture.path("Tests/init.server.luau")).unwrap();

		let deleted = fixture.event("Tests/init.luau");
		let created = fixture.event("Tests/init.server.luau");

		assert_class_change(&deleted, tests, "Script");
		assert!(created.is_empty(), "{created:#?}");
	}

	#[test]
	fn a_flat_suffix_rename_is_one_class_change_whichever_event_comes_first() {
		let mut fixture = Fixture::new(&[("X.server.luau", "print('x')")]);
		let x = fixture.child("X");

		// The deletion first — the usual order for a rename
		fs::rename(fixture.path("X.server.luau"), fixture.path("X.luau")).unwrap();

		let deleted = fixture.event("X.server.luau");
		let created = fixture.event("X.luau");

		assert_class_change(&deleted, x, "ModuleScript");
		assert!(created.is_empty(), "{created:#?}");
		assert_eq!(fixture.count_named("X"), 1);

		// The creation first: it lands on the parent directory, which must
		// pair the renamed file with the instance by name
		fs::rename(fixture.path("X.luau"), fixture.path("X.server.luau")).unwrap();

		let created = fixture.event("X.server.luau");
		let deleted = fixture.event("X.luau");

		assert_class_change(&created, x, "Script");
		assert!(deleted.is_empty(), "{deleted:#?}");
		assert_eq!(fixture.count_named("X"), 1);
	}

	#[test]
	fn a_class_change_seen_from_the_parent_directory_keeps_the_ref() {
		let mut fixture = Fixture::new(&[("Tests/A.luau", "return 1")]);
		let tests = fixture.child("Tests");
		let a = fixture.child_of(tests, "A");

		fs::write(fixture.path("Tests/init.luau"), "return {}").unwrap();

		// An event on the service directory itself (a coalesced or
		// directory-level watcher event) re-reads the whole directory
		let service = fixture.service();
		let changes = process_changes(service, &mut fixture.tree, &fixture.vfs).unwrap();

		assert_class_change(&changes, tests, "ModuleScript");
		assert_eq!(fixture.child_of(tests, "A"), a);
		assert_eq!(fixture.count_named("Tests"), 1);
	}

	#[test]
	fn a_real_deletion_is_still_a_removal() {
		let mut fixture = Fixture::new(&[("X.server.luau", "print('x')"), ("Y.luau", "return 1")]);
		let x = fixture.child("X");

		// An unrelated new sibling is not a replacement
		fs::write(fixture.path("Z.luau"), "return 2").unwrap();
		fs::remove_file(fixture.path("X.server.luau")).unwrap();

		let changes = fixture.event("X.server.luau");

		assert!(
			changes.additions.is_empty() && changes.updates.is_empty(),
			"{changes:#?}"
		);
		assert_eq!(changes.removals, vec![x]);
		assert!(!fixture.tree.exists(x));
	}

	#[test]
	fn a_same_named_sibling_next_to_the_original_is_an_addition() {
		let mut fixture = Fixture::new(&[("X.server.luau", "print('x')")]);
		let x = fixture.child("X");

		// Both files exist: two instances named X, the original untouched
		fs::write(fixture.path("X.luau"), "return 'x'").unwrap();

		let changes = fixture.event("X.luau");

		assert!(
			changes.removals.is_empty() && changes.updates.is_empty(),
			"{changes:#?}"
		);
		assert_eq!(changes.additions.len(), 1);
		assert_eq!(changes.additions[0].class, "ModuleScript");
		assert_eq!(fixture.class_of(x), "Script");
		assert_eq!(fixture.count_named("X"), 2);
	}
}
