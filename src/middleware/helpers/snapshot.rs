use anyhow::{bail, Result};
use rbx_dom_weak::{types::Ref, AHashMap, Instance, WeakDom};

use crate::{
	core::{meta::Meta, snapshot::Snapshot},
	middleware::{model_depth_allowed, stack_position},
};

// Based on Rojo's InstanceSnapshot::from_tree (https://github.com/rojo-rbx/rojo/blob/master/src/snapshot/instance_snapshot.rs#L105)
pub fn snapshot_from_dom(dom: WeakDom, id: Ref) -> Result<Snapshot> {
	let (_, mut raw_dom) = dom.into_raw();

	fn walk(id: Ref, raw_dom: &mut AHashMap<Ref, Instance>, depth: usize, base: usize) -> Result<Snapshot> {
		// A model file is untrusted input: a pathologically deep one must
		// fail to load, not overflow the stack
		if !model_depth_allowed(depth, base) {
			bail!("The model nests instances deeper than WSync follows ({depth} levels)");
		}

		let instance = raw_dom
			.remove(&id)
			.expect("Provided ID does not exist in the current DOM");

		let children = instance
			.children()
			.iter()
			.map(|&child_id| walk(child_id, raw_dom, depth + 1, base))
			.collect::<Result<Vec<_>>>()?;

		let mut meta = Meta::new();

		if instance.class == "MeshPart" {
			meta.set_mesh_source(super::save_mesh(&instance.properties));
		}

		Ok(Snapshot::new()
			.with_meta(meta)
			.with_name(&instance.name)
			.with_class(&instance.class)
			.with_properties(instance.properties)
			.with_children(children))
	}

	walk(id, &mut raw_dom, 0, stack_position())
}
