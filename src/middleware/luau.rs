use anyhow::Result;
use rbx_dom_weak::{
	types::{Enum, Variant},
	ustr, HashMapExt, UstrMap,
};
use std::path::Path;

use super::Middleware;
use crate::{core::snapshot::Snapshot, vfs::Vfs, Properties};

/// One variant per suffix, one meaning per variant — see the scheme on the
/// `Middleware` enum. There is no mode flag: `.server` is always the classic
/// Legacy script and `.client` is always a RunContext = Client `Script`.
#[derive(Debug, Clone, PartialEq)]
pub enum ScriptType {
	/// `.server` — `Script`, RunContext = Legacy.
	Legacy,
	/// `.runserver` — `Script`, RunContext = Server.
	Server,
	/// `.client` — `Script`, RunContext = Client.
	Client,
	/// `.local` — `LocalScript`.
	Local,
	/// plain `.luau` — `ModuleScript`.
	Module,
}

impl From<Middleware> for ScriptType {
	fn from(middleware: Middleware) -> Self {
		match middleware {
			Middleware::ServerScript => ScriptType::Legacy,
			Middleware::RunServerScript => ScriptType::Server,
			Middleware::ClientScript => ScriptType::Client,
			Middleware::LocalScript => ScriptType::Local,
			Middleware::ModuleScript => ScriptType::Module,
			_ => panic!("Cannot convert {middleware:?} to ScriptType"),
		}
	}
}

#[profiling::function]
pub fn read_luau(path: &Path, vfs: &Vfs, script_type: ScriptType) -> Result<Snapshot> {
	let (class_name, run_context) = match &script_type {
		ScriptType::Legacy => ("Script", Some(Variant::Enum(Enum::from_u32(0)))),
		ScriptType::Server => ("Script", Some(Variant::Enum(Enum::from_u32(1)))),
		ScriptType::Client => ("Script", Some(Variant::Enum(Enum::from_u32(2)))),
		ScriptType::Local => ("LocalScript", None),
		ScriptType::Module => ("ModuleScript", None),
	};

	let mut snapshot = Snapshot::new().with_class(class_name);
	let mut properties = UstrMap::new();

	let source = vfs.read_to_string(path)?;

	if let Some(run_context) = run_context {
		properties.insert(ustr("RunContext"), run_context);
	}

	properties.insert(ustr("Source"), Variant::String(source));
	snapshot.set_properties(properties);

	Ok(snapshot)
}

#[profiling::function]
pub fn write_luau(mut properties: Properties, path: &Path, vfs: &Vfs) -> Result<Properties> {
	let source = if let Some(Variant::String(source)) = properties.remove(&ustr("Source")) {
		source
	} else {
		String::new()
	};

	vfs.write(path, source.as_bytes())?;

	Ok(properties)
}
