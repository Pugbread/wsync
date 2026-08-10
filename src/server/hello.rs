use axum::{extract::State, Json};
use log::trace;
use serde::Serialize;
use std::{fs, process};

use crate::{constants::PROTOCOL_VERSION, ext::PathExt, server::AppState};

/// JSON identity for daemon discovery (Design §5.2): the Studio plugin and
/// the desktop app port-scan `GET /hello` to find the daemon that serves the
/// project matching the open place
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Hello {
	name: String,
	version: &'static str,
	/// Short commit hash the binary was built from (`unknown` outside git)
	commit: &'static str,
	/// Whether the build tree carried uncommitted changes
	dirty: bool,
	protocol: u8,
	project: String,
	canonical_project: String,
	/// Sync scope (Design §7.0): `"code"` or `"full"`; absent on older
	/// daemons, which clients read as full
	scope: &'static str,
	game_id: Option<u64>,
	place_ids: Vec<u64>,
	boot_id: String,
	pid: u32,
	port: u16,
	managed_by: String,
	project_init: bool,
	/// Resolved mutation audit log path, so `wsync status` can report
	/// write-log health (Design §6.4/§12)
	writes_log: String,
}

pub async fn main(State(state): State<AppState>) -> Json<Hello> {
	trace!("Received request: hello");

	let (name, project_path, game_id, place_ids, scope) = {
		let project = state.core.project();

		(
			project.name.clone(),
			project.path.clone(),
			project.game_id,
			project.place_ids.clone(),
			project.scope(),
		)
	};

	let canonical_project = fs::canonicalize(&project_path)
		.map(|path| path.to_string())
		.unwrap_or_else(|_| project_path.to_string());

	Json(Hello {
		name,
		// `/hello` and the WS hello always report the real version; only
		// msgpack `GET /details` may report the Argon compat constant
		version: env!("CARGO_PKG_VERSION"),
		commit: env!("WSYNC_BUILD_COMMIT"),
		dirty: env!("WSYNC_BUILD_DIRTY") == "true",
		protocol: PROTOCOL_VERSION,
		project: project_path.to_string(),
		canonical_project,
		scope: scope.as_str(),
		game_id,
		place_ids,
		boot_id: state.identity.boot_id.clone(),
		pid: process::id(),
		port: state.identity.port,
		managed_by: state.identity.managed_by.clone(),
		// TODO(phase-6): Studio-authorized project creation (the desktop
		// broker flow) is not available before the desktop app lands
		project_init: false,
		writes_log: state.audit.path().to_string(),
	})
}
