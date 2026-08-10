//! The embedded command registry (commands.json, Design §10.5).
//!
//! `docs/client-commands.generated.json` is compiled into the binary verbatim,
//! so `wsync commands` answers offline and always describes the registry the
//! binary shipped with — the desktop Docs view and the published docs render
//! the same bundle. `include_str!` keeps the file a build input: regenerating
//! the bundle recompiles this module.

use serde_json::Value;

/// The generated registry bundle, byte-for-byte as it sits in the repository
pub const CLIENT_COMMANDS: &str = include_str!("../../docs/client-commands.generated.json");

/// The bundle parsed. The registry is a build artifact validated by
/// `scripts/check-command-validation.mjs`, so a bundle that does not parse is
/// a broken build, not a runtime condition — but the caller still gets an
/// `Option` so a hypothetical bad embed fails with a message instead of a
/// panic
pub fn parsed() -> Option<Value> {
	serde_json::from_str(CLIENT_COMMANDS).ok()
}
