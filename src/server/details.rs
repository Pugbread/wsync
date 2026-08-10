use axum::extract::State;
use log::trace;

use crate::{
	config::Config,
	constants::ARGON_COMPAT_VERSION,
	project::ProjectDetails,
	server::{AppState, MsgPack},
};

/// msgpack `GET /details` — the Argon-protocol identity endpoint. When
/// `compat_argon` is enabled it reports `ARGON_COMPAT_VERSION` so a stock
/// Argon plugin passes its own semver gate against this daemon; otherwise it
/// reports the real WSync version. The endpoint itself is served regardless
/// of the flag — the msgpack surface is protocol v1's fallback transport
/// (Design §5.1)
pub async fn main(State(state): State<AppState>) -> MsgPack<ProjectDetails> {
	trace!("Received request: details");

	// Lock order matches the processor (tree before project) to avoid a
	// lock-order inversion with `on_vfs_event`
	let details = {
		let tree = state.core.tree();
		let project = state.core.project();

		ProjectDetails::from_project(&project, &tree)
	};

	let details = if Config::new().compat_argon {
		details.with_version(ARGON_COMPAT_VERSION)
	} else {
		details
	};

	MsgPack(details)
}
