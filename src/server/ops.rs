//! Daemon-answered remote ops (registry contracts `path`, `meta`, `where`):
//! path-tool queries resolved against the live tree and `Meta` by the daemon
//! itself in the request router — never forwarded to the plugin, so they
//! answer with or without a Studio connection.

use path_clean::PathClean;
use rbx_dom_weak::types::Ref;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

use crate::{
	constants::WHERE_MATCH_LIMIT,
	core::{
		conflict::content::{instance_path, relative_path},
		meta::{SourceEntry, SourceKind},
		tree::Tree,
		Core,
	},
	middleware::Middleware,
	server::ws::frames::codes,
};

/// A daemon-op failure: protocol error code plus message
pub type OpFailure = (&'static str, String);

/// Answers `op` locally when it is a daemon-side op. `None` means the op is
/// not daemon-answered and must be routed to the plugin
pub fn try_answer(core: &Core, op: &str, args: &Value) -> Option<Result<Value, OpFailure>> {
	match op {
		"path" => Some(answer_path(core, args)),
		"meta" => Some(answer_meta(core, args)),
		"where" => Some(answer_where(core, args)),
		_ => None,
	}
}

fn arg_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
	args.get(key).and_then(Value::as_str).filter(|value| !value.is_empty())
}

fn require_target(args: &Value) -> Result<&str, OpFailure> {
	arg_str(args, "target").ok_or((codes::INVALID_ARGUMENT, "target must be a non-empty string".to_owned()))
}

/// Resolves a slash-joined Studio path (names below the served root; an
/// optional leading `game` segment is tolerated) to a tree ref
fn resolve_studio(tree: &Tree, target: &str) -> Option<Ref> {
	let mut segments = target.split('/').filter(|segment| !segment.is_empty()).peekable();

	if segments.peek().copied() == Some("game") {
		segments.next();
	}

	segments.peek()?;

	let mut current = tree.root_ref();

	for segment in segments {
		let instance = tree.get_instance(current)?;

		current = instance
			.children()
			.iter()
			.copied()
			.find(|child| tree.get_instance(*child).is_some_and(|child| child.name == segment))?;
	}

	Some(current)
}

/// Resolves a workspace-relative filesystem path to a tree ref through the
/// path index (absolute paths inside the workspace are accepted too)
fn resolve_fs(core: &Core, tree: &Tree, target: &str) -> Option<Ref> {
	let workspace_dir = core.project().workspace_dir.clone();
	let path = Path::new(target);

	let absolute: PathBuf = if path.is_absolute() {
		path.to_path_buf().clean()
	} else {
		workspace_dir.join(path).clean()
	};

	tree.get_ids(&absolute).and_then(|ids| ids.first().copied())
}

#[derive(Debug)]
enum From {
	Auto,
	Studio,
	Fs,
}

fn parse_from(args: &Value) -> Result<From, OpFailure> {
	match args.get("from").and_then(Value::as_str) {
		None | Some("auto") => Ok(From::Auto),
		Some("studio") => Ok(From::Studio),
		Some("fs") => Ok(From::Fs),
		Some(other) => Err((
			codes::INVALID_ARGUMENT,
			format!("from must be \"auto\", \"studio\" or \"fs\", got {other:?}"),
		)),
	}
}

/// Resolves `target` per `from`, with the not-projected refusal message noting
/// the target is Studio-only (registry contract)
fn resolve_target(core: &Core, tree: &Tree, target: &str, from: &From) -> Result<Ref, OpFailure> {
	let id = match from {
		From::Studio => resolve_studio(tree, target),
		From::Fs => resolve_fs(core, tree, target),
		From::Auto => resolve_studio(tree, target).or_else(|| resolve_fs(core, tree, target)),
	};

	id.ok_or_else(|| {
		(
			codes::NOT_FOUND,
			format!(
				"{target:?} is not in the projected tree — it is Studio-only, outside the mapped roots, or does not exist"
			),
		)
	})
}

/// Workspace-relative string forms of an instance's relevant source entries,
/// in source order (file before folder/data/project)
fn source_paths(tree: &Tree, id: Ref, workspace_dir: &Path) -> Vec<String> {
	let Some(meta) = tree.get_meta(id) else {
		return Vec::new();
	};

	let mut entries = meta.source.relevant().clone();
	entries.sort_by_key(SourceEntry::index);

	entries
		.iter()
		.map(|entry| {
			let path = entry.path();

			path.strip_prefix(workspace_dir)
				.unwrap_or(path)
				.to_string_lossy()
				.into_owned()
		})
		.collect()
}

/// The `kind` classification for the `path` op: `file` (file-backed),
/// `dir` (folder-backed), `project-node` (lives in a project file only)
fn target_kind(tree: &Tree, id: Ref) -> &'static str {
	let Some(meta) = tree.get_meta(id) else {
		return "project-node";
	};

	let has_folder = meta
		.source
		.relevant()
		.iter()
		.any(|entry| matches!(entry, SourceEntry::Folder(_)));

	if has_folder {
		return "dir";
	}

	if meta.source.get_file().is_some() {
		return "file";
	}

	match meta.source.get() {
		SourceKind::Project(..) => "project-node",
		SourceKind::Path(_) => "dir",
		SourceKind::None => "project-node",
	}
}

// path {target, from} → {studioPath, fsPaths, kind}

fn answer_path(core: &Core, args: &Value) -> Result<Value, OpFailure> {
	let target = require_target(args)?;
	let from = parse_from(args)?;

	let tree = core.tree();
	let id = resolve_target(core, &tree, target, &from)?;
	let workspace_dir = core.project().workspace_dir.clone();

	Ok(json!({
		"studioPath": instance_path(&tree, id),
		"fsPaths": source_paths(&tree, id, &workspace_dir),
		"kind": target_kind(&tree, id),
	}))
}

// meta {target, from} → {instancePath, class, sourcePaths, middleware?, keepUnknowns?}

fn answer_meta(core: &Core, args: &Value) -> Result<Value, OpFailure> {
	let target = require_target(args)?;
	let from = parse_from(args)?;

	let tree = core.tree();
	let id = resolve_target(core, &tree, target, &from)?;
	let workspace_dir = core.project().workspace_dir.clone();

	let instance = tree
		.get_instance(id)
		.ok_or_else(|| (codes::NOT_FOUND, format!("{target:?} vanished while resolving")))?;

	let mut value = json!({
		"instancePath": instance_path(&tree, id),
		"class": instance.class.as_str(),
		"sourcePaths": source_paths(&tree, id, &workspace_dir),
	});

	if let Some(middleware) = Middleware::from_class(&instance.class, None) {
		value["middleware"] = json!(middleware.to_string());
	}

	if let Some(meta) = tree.get_meta(id) {
		if meta.keep_unknowns {
			value["keepUnknowns"] = json!(true);
		}
	}

	Ok(value)
}

// where {target, under?} → {matches, truncated}

fn answer_where(core: &Core, args: &Value) -> Result<Value, OpFailure> {
	let target = require_target(args)?;
	let under = arg_str(args, "under");

	let tree = core.tree();
	let workspace_dir = core.project().workspace_dir.clone();

	let scope = match under {
		Some(under) => resolve_studio(&tree, under).ok_or_else(|| {
			(
				codes::NOT_FOUND,
				format!("under: {under:?} is not in the projected tree"),
			)
		})?,
		None => tree.root_ref(),
	};

	let needle = target.to_lowercase();
	let mut matches = Vec::new();
	let mut truncated = false;

	// Breadth-first walk keeps shallow (more relevant) matches first and the
	// result bound makes the search safe on huge projects
	let mut pending = vec![scope];
	let mut index = 0;

	'walk: while index < pending.len() {
		let id = pending[index];
		index += 1;

		let Some(instance) = tree.get_instance(id) else {
			continue;
		};

		pending.extend(instance.children().iter().copied());

		// The scope root itself is context, not a match
		if id == scope && under.is_some() {
			continue;
		}

		if id == tree.root_ref() {
			continue;
		}

		if instance.name.to_lowercase().contains(&needle) {
			if matches.len() >= WHERE_MATCH_LIMIT {
				truncated = true;
				break 'walk;
			}

			let mut entry = json!({ "instancePath": instance_path(&tree, id) });

			// Matches the projection does not back on disk stay listed,
			// marked implicitly by the absent fsPath (registry contract:
			// Studio-only matches are still listed)
			if let Some(rel_path) = relative_path(&tree, id, &workspace_dir) {
				entry["fsPath"] = json!(rel_path);
			}

			matches.push(entry);
		}
	}

	Ok(json!({ "matches": matches, "truncated": truncated }))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn from_parses_strictly() {
		assert!(matches!(parse_from(&json!({})), Ok(From::Auto)));
		assert!(matches!(parse_from(&json!({ "from": "auto" })), Ok(From::Auto)));
		assert!(matches!(parse_from(&json!({ "from": "studio" })), Ok(From::Studio)));
		assert!(matches!(parse_from(&json!({ "from": "fs" })), Ok(From::Fs)));

		let err = parse_from(&json!({ "from": "disk" })).unwrap_err();
		assert_eq!(err.0, codes::INVALID_ARGUMENT);
	}
}
