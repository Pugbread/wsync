//! The Studio → disk write path's flat ↔ folder reshapes and depth guard,
//! driven through the library (`processor::write`) against a real
//! filesystem: a script gaining its first child becomes a folder, losing its
//! last child makes it flat again — each as one crash-safe transaction, with
//! no staging left behind and no second form of the script on disk.

use rbx_dom_weak::{
	types::{Ref, Variant},
	ustr, HashMapExt, UstrMap,
};
use std::{
	fs,
	path::{Path, PathBuf},
	thread,
};

use wsync::{
	core::{
		meta::Meta,
		processor::write,
		snapshot::{AddedSnapshot, Snapshot},
		tree::Tree,
		Core,
	},
	middleware::new_snapshot,
	project::Project,
	vfs::Vfs,
	Properties,
};

struct Fixture {
	_dir: tempfile::TempDir,
	root: PathBuf,
	tree: Tree,
	vfs: Vfs,
}

impl Fixture {
	fn new(files: &[(&str, &str)]) -> Self {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path().canonicalize().unwrap();

		for (path, contents) in files {
			let path = root.join(path);
			fs::create_dir_all(path.parent().unwrap()).unwrap();
			fs::write(path, contents).unwrap();
		}

		fs::write(
			root.join("default.project.json"),
			r#"{"name":"reshape","tree":{"$className":"DataModel","ReplicatedStorage":{"$path":"src"}},"scope":"full"}"#,
		)
		.unwrap();

		let project = Project::load(&root.join("default.project.json")).unwrap();
		let meta = Meta::from_project(&project);
		let vfs = Vfs::new(false);
		let snapshot = new_snapshot(&project.path, &meta.context, &vfs).unwrap().unwrap();

		Self {
			_dir: dir,
			root,
			tree: Tree::new(snapshot),
			vfs,
		}
	}

	fn id_of(&self, path: &str) -> Ref {
		self.tree
			.get_ids(&self.root.join(path))
			.unwrap_or_else(|| panic!("no instance is sourced from {path}"))[0]
	}

	/// Every entry under `src`, relative and sorted
	fn listing(&self) -> Vec<String> {
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

		let src = self.root.join("src");
		let mut out = Vec::new();
		walk(&src, &src, &mut out);
		out.sort();
		out
	}
}

fn module(name: &str, source: &str, parent: Ref) -> AddedSnapshot {
	let mut properties: Properties = UstrMap::new();
	properties.insert(ustr("Source"), Variant::String(source.into()));

	AddedSnapshot {
		id: Ref::new(),
		meta: Meta::new(),
		parent,
		name: name.into(),
		class: ustr("ModuleScript"),
		properties,
		children: Vec::new(),
	}
}

#[test]
fn a_first_child_promotes_and_a_last_removal_demotes_atomically() {
	let mut fixture = Fixture::new(&[("src/Video.luau", "return 'video'")]);
	let video = fixture.id_of("src/Video.luau");

	let child = module("Frame", "return 'frame'", video);
	let child_id = child.id;

	write::apply_addition(child, &mut fixture.tree, &fixture.vfs).unwrap();

	assert_eq!(
		fixture.listing(),
		vec!["Video/", "Video/Frame.luau", "Video/init.luau"],
		"one folder form, no flat leftover, no staging"
	);
	assert_eq!(
		fs::read_to_string(fixture.root.join("src/Video/init.luau")).unwrap(),
		"return 'video'"
	);

	write::apply_removal(child_id, &mut fixture.tree, &fixture.vfs).unwrap();

	assert_eq!(fixture.listing(), vec!["Video.luau"], "one flat form, no empty folder");
	assert_eq!(
		fs::read_to_string(fixture.root.join("src/Video.luau")).unwrap(),
		"return 'video'"
	);
}

/// The incident's second half: an empty `X/` beside `X.luau` blocked the
/// next promotion ("already exists"). It holds nothing, so it yields
#[test]
fn an_empty_leftover_folder_does_not_block_a_promotion() {
	let mut fixture = Fixture::new(&[
		("src/EditableVideo.luau", "return 1"),
		("src/EditableVideo/.DS_Store", ""),
	]);

	let video = fixture.id_of("src/EditableVideo.luau");

	write::apply_addition(module("Child", "return 2", video), &mut fixture.tree, &fixture.vfs).unwrap();

	assert_eq!(
		fixture.listing(),
		vec!["EditableVideo/", "EditableVideo/Child.luau", "EditableVideo/init.luau"]
	);
}

/// A folder holding anything besides the script (here an unprojected note)
/// keeps its folder form: demoting it would delete that file with it
#[test]
fn a_folder_with_other_files_keeps_its_folder_form() {
	let mut fixture = Fixture::new(&[
		("src/Tool/init.luau", "return 'tool'"),
		("src/Tool/Handle.luau", "return 'handle'"),
		("src/Tool/NOTES", "keep me"),
	]);

	let handle = fixture.id_of("src/Tool/Handle.luau");

	write::apply_removal(handle, &mut fixture.tree, &fixture.vfs).unwrap();

	assert_eq!(fixture.listing(), vec!["Tool/", "Tool/NOTES", "Tool/init.luau"]);
}

/// A Studio subtree deeper than the guard fails with an error on the
/// processor thread's stack instead of overflowing it
#[test]
fn a_pathologically_deep_studio_subtree_is_refused_not_overflowed() {
	let handle = thread::Builder::new()
		.stack_size(2 * 1024 * 1024)
		.spawn(|| {
			let mut fixture = Fixture::new(&[("src/Keep.luau", "return 1")]);
			let parent = fixture.tree.get_ids(&fixture.root.join("src")).unwrap()[0];

			let mut chain = Snapshot::new().with_id(Ref::new()).with_name("Leaf");

			for level in 0..400 {
				chain = Snapshot::new()
					.with_id(Ref::new())
					.with_name(if level % 2 == 0 { "a" } else { "b" })
					.with_children(vec![chain]);
			}

			let added = chain.as_new(parent);

			write::apply_addition(added, &mut fixture.tree, &fixture.vfs)
				.err()
				.map(|err| err.to_string())
		})
		.unwrap();

	let error = handle.join().expect("the write path overflowed or panicked");
	let error = error.expect("a 400-level subtree must be refused");

	assert!(error.contains("nested deeper than this write can follow"), "{error}");
}

/// A daemon that died mid-promotion — the flat script already moved into
/// the staging directory, the folder not yet in place — left no resolvable
/// form of the script. The next daemon start rolls it forward before its
/// first snapshot, so the instance is back
#[test]
fn a_daemon_start_finishes_an_interrupted_reshape() {
	let fixture = Fixture::new(&[("src/Keep.luau", "return 'keep'")]);
	let src = fixture.root.join("src");
	let staging = src.join(".Video.wsync-reshape");

	fs::create_dir_all(staging.join("old")).unwrap();
	fs::create_dir_all(staging.join("new")).unwrap();
	fs::write(staging.join("old/Video.luau"), "return 'video'").unwrap();
	fs::write(
		staging.join("reshape.json"),
		r#"{"version":1,"old":["Video.luau"],"new":["Video"],"moves":[["Video.luau","Video/init.luau"]]}"#,
	)
	.unwrap();

	// Staging is never projected: offline, the script is simply missing
	assert!(fixture.tree.get_ids(&src.join("Video/init.luau")).is_none());

	let project = Project::load(&fixture.root.join("default.project.json")).unwrap();
	let core = Core::new(project, true).unwrap();

	assert_eq!(fixture.listing(), vec!["Keep.luau", "Video/", "Video/init.luau"]);

	let tree = core.tree();
	let ids = tree
		.get_ids(&src.join("Video/init.luau"))
		.expect("the rolled-forward script is in the daemon's tree");
	let video = tree.get_instance(ids[0]).unwrap();

	assert_eq!(video.name, "Video");
	assert_eq!(video.class, "ModuleScript");
}
