//! Canonical per-instance content states for the conflict engine.
//!
//! A baseline (Design §6.3) is the hash of the last state both sides agreed
//! on. The canonical content of an instance is its name, class and property
//! map — scripts are dominated by their normalized `Source` (CRLF folded to
//! LF, matching `ignore_line_endings`), everything else by the encoded
//! property map. `Meta` is daemon bookkeeping and never part of the hash,
//! and the plugin's `ArgonEmpty` wire marker is stripped before hashing so
//! it can never manufacture a phantom difference.

use rbx_dom_weak::types::{Ref, Variant};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::path::Path;

use crate::{
	core::{meta::SourceEntry, snapshot::UpdatedSnapshot, tree::Tree},
	util, Properties,
};

pub type ContentHash = [u8; 32];

pub fn hash_hex(hash: &ContentHash) -> String {
	let mut hex = String::with_capacity(64);

	for byte in hash {
		hex.push_str(&format!("{byte:02x}"));
	}

	hex
}

/// One side's canonical content: everything the engine needs to decide,
/// park, render and re-apply an instance state
#[derive(Debug, Clone)]
pub struct ContentState {
	pub name: String,
	pub class: String,
	/// Property map with `ArgonEmpty` stripped and `Source` normalized
	pub properties: Vec<(String, Value)>,
	pub hash: ContentHash,
}

impl ContentState {
	pub fn new(name: &str, class: &str, properties: &Properties) -> Self {
		let mut encoded: Vec<(String, Value)> = properties
			.iter()
			.filter(|(key, _)| key.as_str() != "ArgonEmpty")
			.map(|(key, value)| {
				let value = if key.as_str() == "Source" {
					normalize_source(value)
				} else {
					value.clone()
				};

				(
					key.as_str().to_owned(),
					serde_json::to_value(&value).unwrap_or(Value::Null),
				)
			})
			.collect();

		encoded.sort_by(|(a, _), (b, _)| a.cmp(b));

		let mut hasher = Sha256::new();
		hasher.update(class.as_bytes());
		hasher.update([0]);
		hasher.update(name.as_bytes());
		hasher.update([0]);

		for (key, value) in &encoded {
			hasher.update(key.as_bytes());
			hasher.update([0]);
			hasher.update(value.to_string().as_bytes());
			hasher.update([0]);
		}

		Self {
			name: name.to_owned(),
			class: class.to_owned(),
			properties: encoded,
			hash: hasher.finalize().into(),
		}
	}

	pub fn is_script(&self) -> bool {
		util::is_script(&self.class)
	}

	/// The normalized script source, when this content is a script
	pub fn source(&self) -> Option<String> {
		if !self.is_script() {
			return None;
		}

		self.properties
			.iter()
			.find(|(key, _)| key == "Source")
			.and_then(|(_, value)| value.get("String"))
			.and_then(Value::as_str)
			.map(str::to_owned)
	}

	/// The tagged property map as a JSON object (the same codec the sync
	/// frames use), for `kind: "properties"` conflict records
	pub fn props_json(&self) -> Value {
		Value::Object(self.properties.iter().cloned().collect())
	}

	/// This content with an `UpdatedSnapshot` overlaid — the state the other
	/// side reports after applying the update to this base
	pub fn with_update(&self, update: &UpdatedSnapshot, base_properties: &Properties) -> Self {
		let name = update.name.as_deref().unwrap_or(&self.name);
		let class = update.class.map(|class| class.to_string());
		let class = class.as_deref().unwrap_or(&self.class);

		match &update.properties {
			Some(properties) => Self::new(name, class, properties),
			None => Self::new(name, class, base_properties),
		}
	}
}

fn normalize_source(value: &Variant) -> Variant {
	match value {
		Variant::String(source) if source.contains('\r') => Variant::String(source.replace("\r\n", "\n")),
		_ => value.clone(),
	}
}

/// Identity captured alongside content so a parked conflict can be listed
/// and resolved after the tree has moved on
#[derive(Debug, Clone)]
pub struct InstanceInfo {
	pub id: Ref,
	pub parent: Ref,
	pub instance_path: String,
	/// Repo-relative backing file, when one exists
	pub rel_path: Option<String>,
}

/// Captured pre-state of one instance: identity, canonical content, and the
/// raw property map (kept so a parked or tombstoned state can be re-applied
/// through the write middleware later)
#[derive(Debug, Clone)]
pub struct Captured {
	pub info: InstanceInfo,
	pub content: ContentState,
	pub properties: Properties,
}

/// Slash-joined instance names from (exclusive) the tree root down to `id`
pub fn instance_path(tree: &Tree, id: Ref) -> String {
	let mut names = Vec::new();
	let mut current = id;

	while current != tree.root_ref() {
		let Some(instance) = tree.get_instance(current) else {
			break;
		};

		names.push(instance.name.clone());
		current = instance.parent();
	}

	names.reverse();
	names.join("/")
}

/// Repo-relative backing file for `id`, preferring the source file over
/// folder/data entries
pub fn relative_path(tree: &Tree, id: Ref, workspace_dir: &Path) -> Option<String> {
	let meta = tree.get_meta(id)?;

	let mut entries = meta.source.relevant().clone();
	entries.sort_by_key(SourceEntry::index);

	entries.first().map(|entry| {
		let path = entry.path();

		path.strip_prefix(workspace_dir)
			.unwrap_or(path)
			.to_string_lossy()
			.into_owned()
	})
}

/// Captures one instance (no children) from the tree
pub fn capture_instance(tree: &Tree, id: Ref, workspace_dir: &Path) -> Option<Captured> {
	let instance = tree.get_instance(id)?;

	Some(Captured {
		info: InstanceInfo {
			id,
			parent: instance.parent(),
			instance_path: instance_path(tree, id),
			rel_path: relative_path(tree, id, workspace_dir),
		},
		content: ContentState::new(&instance.name, &instance.class, &instance.properties),
		properties: instance.properties.clone(),
	})
}

/// Captures `id` and every descendant — the pre-state walk the FS path runs
/// before `read::process_changes` mutates the tree, sized by the changed
/// subtree rather than the project
pub fn capture_subtree(tree: &Tree, id: Ref, workspace_dir: &Path) -> Vec<Captured> {
	let mut captured = Vec::new();

	fn walk(tree: &Tree, id: Ref, workspace_dir: &Path, captured: &mut Vec<Captured>) {
		if let Some(capture) = capture_instance(tree, id, workspace_dir) {
			captured.push(capture);
		}

		if let Some(instance) = tree.get_instance(id) {
			for child in instance.children().to_owned() {
				walk(tree, child, workspace_dir, captured);
			}
		}
	}

	walk(tree, id, workspace_dir, &mut captured);

	captured
}

/// Refs of `id` and every descendant (used to forget baselines when a
/// subtree removal is applied)
pub fn subtree_refs(tree: &Tree, id: Ref) -> Vec<Ref> {
	let mut refs = vec![id];
	let mut index = 0;

	while index < refs.len() {
		if let Some(instance) = tree.get_instance(refs[index]) {
			refs.extend(instance.children().iter().copied());
		}

		index += 1;
	}

	refs
}
