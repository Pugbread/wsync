use anyhow::Result;
use std::path::{Path, PathBuf};

use super::snapshot_child;
use crate::{
	core::{
		meta::{Context, Meta, Source},
		snapshot::Snapshot,
	},
	ext::PathExt,
	vfs::Vfs,
};

/// A plain folder: `children` are the entries `layout::read_layout` kept —
/// init files, shadowed forms and ignored paths are already filtered out
#[profiling::function]
pub fn read_dir(path: &Path, children: &[PathBuf], context: &Context, vfs: &Vfs) -> Result<Snapshot> {
	let name = path.get_name();

	let mut snapshot = Snapshot::new()
		.with_name(name)
		.with_meta(Meta::new().with_context(context).with_source(Source::directory(path)));

	for path in children {
		if let Some(child_snapshot) = snapshot_child(path, context, vfs)? {
			snapshot.add_child(child_snapshot);
		}
	}

	Ok(snapshot)
}

#[profiling::function]
pub fn write_dir(path: &Path, vfs: &Vfs) -> Result<()> {
	vfs.create_dir(path)?;

	Ok(())
}
