//! Generated agent docs (Design §10.6).
//!
//! WSync treats LLM agents as a first-class user: every project carries a
//! generated tool reference that is regenerated on every Studio plugin connect
//! and by `wsync refresh`, while hand-written notes survive in place.
//!
//! Four files are managed:
//!
//! | File | Marker | Contents |
//! | --- | --- | --- |
//! | `wsync.md` | `wsync:project-memory` | the generated tool reference |
//! | `AGENTS.md` | `wsync:agent-context` | a concise contract linking the reference |
//! | `CLAUDE.md` | `wsync:agents-include` | an `@AGENTS.md` import |
//! | `.codex/config.toml` | — | `project_doc_fallback_filenames` |
//!
//! The split is deliberate:
//!
//! - [`facts`] captures every input as plain data — the project's real tree and
//!   sync rules, the generated command registry, versions and paths.
//! - [`render`] turns those facts into markdown. Pure, deterministic, and
//!   timestamp-free, so an unchanged project re-renders byte-identically.
//! - [`markers`] merges a rendered block into an existing file without
//!   disturbing a byte outside it.
//! - [`codex`] performs the value-preserving `.codex/config.toml` edit.
//! - [`write_all`] is the only function that touches the filesystem.
//!
//! ```no_run
//! use wsync::docsgen::{self, DocsInput, EnvFacts, Preserve, ProjectFacts, RegistryFacts, Shipped};
//! # use std::path::Path;
//! # fn demo(project: &wsync::project::Project, registry_json: &str) -> anyhow::Result<()> {
//! let config = wsync::config::Config::new();
//! let project_facts = ProjectFacts::from_project(project, &config);
//! let registry = RegistryFacts::from_generated_json(registry_json)?.with_shipped(Shipped::only(["status"]));
//! let env = EnvFacts::new("0.1.0", 1, Path::new("/state"));
//!
//! let docs = docsgen::render(&DocsInput::new(&project_facts, &registry, &env));
//! docsgen::write_all(Path::new("."), &docs, Preserve::UserNotes)?;
//! # Ok(())
//! # }
//! ```

pub mod codex;
pub mod facts;
pub mod markers;
pub mod render;
pub mod write;

pub use codex::{CodexSkip, FALLBACK_FILENAMES};
pub use facts::{
	CommandFact, DocsInput, EnvFacts, ProjectFacts, RegistryFacts, RuleGroup, Safety, Shipped, SyncRuleFact, SyncedRoot,
};
pub use markers::{AGENTS_INCLUDE, AGENT_CONTEXT, PROJECT_MEMORY};
pub use write::{write_all, DocFile, FileOutcome, Preserve, WriteStatus};

/// Project-relative path of the generated tool reference
pub const WSYNC_MD: &str = "wsync.md";
/// Project-relative path of the concise agent context
pub const AGENTS_MD: &str = "AGENTS.md";
/// Project-relative path of the Claude import
pub const CLAUDE_MD: &str = "CLAUDE.md";
/// Project-relative path of the Codex fallback configuration
pub const CODEX_CONFIG: &str = ".codex/config.toml";

/// Heading written above the block when `wsync.md` does not exist yet
const WSYNC_MD_PREAMBLE: &str = "# WSync project memory";

/// Heading written above the block when `AGENTS.md` does not exist yet.
/// Everything here sits *outside* the marker block, so it is written once and
/// then belongs to the user
const AGENTS_MD_PREAMBLE: &str = "\
# Agent notes

Project-specific notes live in this file, outside the WSync marker block below.
Anything outside the markers is preserved across `wsync refresh`.";

/// The rendered docs, as strings. No file has been touched at this point
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedDocs {
	/// The tool reference, without its marker lines
	pub project_memory: String,
	/// Canonical `wsync.md`: preamble, markers, and the detailed reference
	pub wsync_md: String,
	/// Bounded always-loaded instructions; detailed tables remain in `wsync.md`.
	pub agent_contract: String,
}

/// Renders the generated docs from facts. Pure: no I/O, no clock
pub fn render(input: &DocsInput) -> GeneratedDocs {
	let project_memory = render::project_memory(input);
	let wsync_md = markers::upsert(None, PROJECT_MEMORY, &project_memory, WSYNC_MD_PREAMBLE);

	GeneratedDocs {
		project_memory,
		wsync_md,
		agent_contract: render::agent_contract(input),
	}
}

/// Merges the tool reference into `wsync.md`, preserving notes around the block
pub fn wsync_md(existing: Option<&str>, docs: &GeneratedDocs) -> String {
	markers::upsert(existing, PROJECT_MEMORY, &docs.project_memory, WSYNC_MD_PREAMBLE)
}

/// Merges the concise contract into `AGENTS.md` without duplicating the reference.
pub fn agents_md(existing: Option<&str>, docs: &GeneratedDocs) -> String {
	markers::upsert_first(existing, AGENT_CONTEXT, &docs.agent_contract, AGENTS_MD_PREAMBLE)
}

/// Generated contract budget; independent of preserved user notes.
pub const AGENT_CONTRACT_MAX_BYTES: usize = 8192;
/// Common client instruction-load limit. Report large files; never truncate notes.
pub const AGENT_FILE_WARNING_BYTES: usize = 32768;

/// Human-readable warning for instruction files likely to exceed client budgets.
pub fn agent_size_warning(text: &str) -> Option<String> {
	(text.len() > AGENT_FILE_WARNING_BYTES).then(|| format!(
		"AGENTS.md is {} bytes, exceeding the {}-byte instruction warning budget; preserved user notes may cause client truncation. Move optional detail into linked files. WSync has preserved all notes.",
		text.len(), AGENT_FILE_WARNING_BYTES
	))
}

/// Merges the `@AGENTS.md` import into `CLAUDE.md`
pub fn claude_md(existing: Option<&str>) -> String {
	markers::upsert_first(existing, AGENTS_INCLUDE, &format!("@{AGENTS_MD}"), "")
}

/// Ensures `.codex/config.toml` lists the WSync docs as project-doc fallbacks,
/// preserving every other key. `Err` means the file must be left untouched
pub fn codex_config(existing: Option<&str>) -> Result<String, CodexSkip> {
	codex::ensure_fallbacks(existing)
}
