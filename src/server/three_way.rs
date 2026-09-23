//! Connect-time three-way classification: which side wins each difference
//! the plugin reported, judged against the sync base (see `sync_base`).
//!
//! Kept pure — no tree, no disk, no plugin — so every rule is a unit test.

use std::collections::BTreeMap;

use super::sync_base::{BaseView, Key, Lookup};

/// The divergence states of Design §7.2
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
	/// Disk has it, Studio does not
	OnlyOnDisk,
	/// Both have it and they differ
	Differs,
	/// Studio has it, disk does not
	MissingOnDisk,
}

/// Who wins one difference
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Winner {
	/// Studio's state lands on disk. Nothing on disk is lost: the disk side is
	/// either absent or exactly what both sides last agreed on
	Studio,
	/// Studio's state lands on disk and the disk side goes to the backlog —
	/// both sides moved, or the base cannot tell
	StudioKeepDisk,
	/// Disk's state is pushed to Studio
	Disk,
}

/// One difference, with what each side looks like now
#[derive(Debug, Clone)]
pub struct Input<'a> {
	pub state: State,
	pub instance_path: &'a str,
	/// The daemon-side key (disk), for `OnlyOnDisk` and `Differs`
	pub disk: Option<&'a Key>,
	/// The Studio-side key; `None` when the plugin could not say (an older
	/// plugin sends no source hashes)
	pub studio: Option<&'a Key>,
	/// Studio's descendants of a `MissingOnDisk` root, keyed relative to it;
	/// `None` when the plugin did not send them or had to truncate
	pub studio_descendants: Option<&'a BTreeMap<String, Option<Key>>>,
	/// The path is not unique right now (same-named siblings on either side,
	/// or several differences share it): nothing here can be matched by path
	pub ambiguous: bool,
}

/// Decides one difference. Without a base this is exactly the old
/// Studio-first rule; with one, a side that did not move since the base
/// yields to the side that did
pub fn classify(base: Option<&BaseView>, input: &Input) -> Winner {
	let studio_first = match input.state {
		State::OnlyOnDisk | State::Differs => Winner::StudioKeepDisk,
		State::MissingOnDisk => Winner::Studio,
	};

	let Some(base) = base else {
		return studio_first;
	};

	if input.ambiguous {
		return studio_first;
	}

	let agreed = match base.lookup(input.instance_path) {
		Lookup::Unknown => return studio_first,
		Lookup::Absent => None,
		Lookup::Known(key) => Some(key),
	};

	match input.state {
		// Disk has something Studio lacks: new on disk since the last sync,
		// or Studio deleted it. A deletion keeps the disk copy in the backlog
		// whatever the disk did with it — nothing else holds those bytes
		State::OnlyOnDisk => match agreed {
			None => Winner::Disk,
			Some(_) => Winner::StudioKeepDisk,
		},

		State::Differs => {
			let Some(agreed) = agreed else {
				// Both sides created the same path independently
				return Winner::StudioKeepDisk;
			};

			let disk_moved = input.disk != Some(agreed);
			let studio_moved = input.studio != Some(agreed);

			match (disk_moved, studio_moved) {
				(false, true) => Winner::Studio,
				(true, false) => Winner::Disk,
				// Both moved, or the difference is outside the key (a
				// property, an attribute) and neither side provably moved
				_ => Winner::StudioKeepDisk,
			}
		}

		// Studio has something disk lacks: created in Studio since the last
		// sync, or deleted on disk. The deletion only wins when Studio's whole
		// subtree is still exactly what was agreed — otherwise it would take
		// a Studio edit down with it
		State::MissingOnDisk => {
			let Some(agreed) = agreed else {
				return Winner::Studio;
			};

			if input.studio != Some(agreed) {
				return Winner::Studio;
			}

			let (Some(studio), Some(agreed_below)) = (input.studio_descendants, base.descendants(input.instance_path))
			else {
				return Winner::Studio;
			};

			let unchanged = studio.len() == agreed_below.len()
				&& agreed_below
					.iter()
					.all(|(path, key)| studio.get(path).is_some_and(|studio| studio.as_ref() == Some(*key)));

			if unchanged {
				Winner::Disk
			} else {
				Winner::Studio
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use std::collections::BTreeSet;

	use super::*;
	use crate::server::sync_base::source_hash;

	fn script(source: &str) -> Key {
		Key {
			class: "ModuleScript".into(),
			source: Some(source_hash(source)),
		}
	}

	fn folder() -> Key {
		Key {
			class: "Folder".into(),
			source: None,
		}
	}

	fn base(entries: &[(&str, Key)]) -> BaseView {
		BaseView::from_entries(
			entries
				.iter()
				.map(|(path, key)| (path.to_string(), key.clone()))
				.collect(),
			BTreeSet::new(),
		)
	}

	fn input<'a>(state: State, path: &'a str, disk: Option<&'a Key>, studio: Option<&'a Key>) -> Input<'a> {
		Input {
			state,
			instance_path: path,
			disk,
			studio,
			studio_descendants: None,
			ambiguous: false,
		}
	}

	#[test]
	fn without_a_base_studio_wins_as_before() {
		let a = script("a");

		assert_eq!(
			classify(None, &input(State::OnlyOnDisk, "S/A", Some(&a), None)),
			Winner::StudioKeepDisk
		);
		assert_eq!(
			classify(None, &input(State::Differs, "S/A", Some(&a), Some(&a))),
			Winner::StudioKeepDisk
		);
		assert_eq!(
			classify(None, &input(State::MissingOnDisk, "S/A", None, Some(&a))),
			Winner::Studio
		);
	}

	#[test]
	fn a_file_created_on_disk_is_pushed() {
		let base = base(&[("S", folder())]);
		let new = script("new");

		assert_eq!(
			classify(Some(&base), &input(State::OnlyOnDisk, "S/New", Some(&new), None)),
			Winner::Disk
		);
	}

	#[test]
	fn a_file_studio_deleted_is_removed_but_kept_in_the_backlog() {
		let base = base(&[("S/Gone", script("gone"))]);
		let gone = script("gone");

		assert_eq!(
			classify(Some(&base), &input(State::OnlyOnDisk, "S/Gone", Some(&gone), None)),
			Winner::StudioKeepDisk
		);
	}

	#[test]
	fn only_the_side_that_moved_wins() {
		let base = base(&[("S/A", script("v1"))]);
		let v1 = script("v1");
		let disk = script("disk edit");
		let studio = script("studio edit");

		assert_eq!(
			classify(Some(&base), &input(State::Differs, "S/A", Some(&disk), Some(&v1))),
			Winner::Disk,
			"disk moved ahead while the daemon was down"
		);
		assert_eq!(
			classify(Some(&base), &input(State::Differs, "S/A", Some(&v1), Some(&studio))),
			Winner::Studio
		);
		assert_eq!(
			classify(Some(&base), &input(State::Differs, "S/A", Some(&disk), Some(&studio))),
			Winner::StudioKeepDisk,
			"a real conflict"
		);
		assert_eq!(
			classify(Some(&base), &input(State::Differs, "S/A", Some(&v1), Some(&v1))),
			Winner::StudioKeepDisk,
			"a difference outside the key is never 'one side moved'"
		);
		assert_eq!(
			classify(Some(&base), &input(State::Differs, "S/A", Some(&disk), None)),
			Winner::StudioKeepDisk,
			"an older plugin sent no hash: cannot tell"
		);
	}

	#[test]
	fn a_class_change_counts_as_a_move() {
		let base = base(&[("S/A", folder())]);
		let module = script("");
		let folder = folder();

		assert_eq!(
			classify(Some(&base), &input(State::Differs, "S/A", Some(&module), Some(&folder))),
			Winner::Disk
		);
	}

	#[test]
	fn a_disk_deletion_wins_only_over_an_untouched_studio_subtree() {
		let base = base(&[("S/Dir", folder()), ("S/Dir/A", script("a")), ("S/Dir/B", script("b"))]);
		let dir = folder();

		let same: BTreeMap<String, Option<Key>> =
			[("A".into(), Some(script("a"))), ("B".into(), Some(script("b")))].into();
		let edited: BTreeMap<String, Option<Key>> =
			[("A".into(), Some(script("a2"))), ("B".into(), Some(script("b")))].into();
		let grown: BTreeMap<String, Option<Key>> = [
			("A".into(), Some(script("a"))),
			("B".into(), Some(script("b"))),
			("C".into(), Some(script("c"))),
		]
		.into();

		let mut deleted = input(State::MissingOnDisk, "S/Dir", None, Some(&dir));

		deleted.studio_descendants = Some(&same);
		assert_eq!(classify(Some(&base), &deleted), Winner::Disk);

		deleted.studio_descendants = Some(&edited);
		assert_eq!(classify(Some(&base), &deleted), Winner::Studio);

		deleted.studio_descendants = Some(&grown);
		assert_eq!(classify(Some(&base), &deleted), Winner::Studio);

		deleted.studio_descendants = None;
		assert_eq!(classify(Some(&base), &deleted), Winner::Studio, "truncated or not sent");
	}

	#[test]
	fn a_studio_creation_is_written_to_disk() {
		let base = base(&[("S", folder())]);
		let new = script("new");

		assert_eq!(
			classify(Some(&base), &input(State::MissingOnDisk, "S/New", None, Some(&new))),
			Winner::Studio
		);
	}

	#[test]
	fn ambiguous_paths_fall_back_to_studio_first() {
		let base = base(&[("S/A", script("v1"))]);
		let v1 = script("v1");
		let disk = script("disk edit");

		let mut differs = input(State::Differs, "S/A", Some(&disk), Some(&v1));
		differs.ambiguous = true;

		assert_eq!(classify(Some(&base), &differs), Winner::StudioKeepDisk);

		let mut added = input(State::OnlyOnDisk, "S/New", Some(&disk), None);
		added.ambiguous = true;

		assert_eq!(classify(Some(&base), &added), Winner::StudioKeepDisk);
	}
}
