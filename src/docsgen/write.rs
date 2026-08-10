//! The only part of `docsgen` that touches the filesystem.
//!
//! Rendering is pure (see [`super::render`]); this module reads the four
//! managed files, merges the generated blocks into them, and writes back only
//! what actually changed.

use anyhow::{Context as _, Result};
use std::{
	fs,
	io::ErrorKind,
	path::{Path, PathBuf},
};

use super::{agents_md, claude_md, codex_config, wsync_md, GeneratedDocs};

/// How much of an existing managed file survives a refresh
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Preserve {
	/// Keep every byte outside WSync's marker blocks. The `wsync refresh`
	/// default and the only mode used on plugin connect
	#[default]
	UserNotes,
	/// Rewrite the managed markdown files from scratch, dropping text outside
	/// the markers. `.codex/config.toml` is never rewritten this way — a
	/// user's MCP configuration is not WSync's to discard
	Regenerate,
}

/// One of the four files `wsync refresh` manages
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocFile {
	WsyncMd,
	AgentsMd,
	ClaudeMd,
	CodexConfig,
}

impl DocFile {
	pub const ALL: [DocFile; 4] = [
		DocFile::WsyncMd,
		DocFile::AgentsMd,
		DocFile::ClaudeMd,
		DocFile::CodexConfig,
	];

	pub fn relative_path(&self) -> &'static str {
		match self {
			Self::WsyncMd => super::WSYNC_MD,
			Self::AgentsMd => super::AGENTS_MD,
			Self::ClaudeMd => super::CLAUDE_MD,
			Self::CodexConfig => super::CODEX_CONFIG,
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteStatus {
	Created,
	Updated,
	/// Already byte-identical; the file was not touched
	Unchanged,
	/// Deliberately left alone, with the reason for the report
	Skipped(String),
}

impl WriteStatus {
	pub fn as_str(&self) -> &'static str {
		match self {
			Self::Created => "created",
			Self::Updated => "updated",
			Self::Unchanged => "unchanged",
			Self::Skipped(_) => "skipped",
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileOutcome {
	pub file: DocFile,
	pub path: PathBuf,
	pub status: WriteStatus,
}

/// Writes `wsync.md`, `AGENTS.md`, `CLAUDE.md` and `.codex/config.toml` into
/// `workspace_dir`, preserving everything outside WSync's marker blocks.
///
/// Idempotent: running it twice with the same [`GeneratedDocs`] leaves every
/// file byte-identical and reports [`WriteStatus::Unchanged`].
pub fn write_all(workspace_dir: &Path, docs: &GeneratedDocs, preserve: Preserve) -> Result<Vec<FileOutcome>> {
	let mut outcomes = Vec::with_capacity(DocFile::ALL.len());

	for file in DocFile::ALL {
		let path = workspace_dir.join(file.relative_path());
		let existing = read_existing(&path)?;

		// A `Regenerate` refresh rebuilds the markdown from scratch, but the
		// Codex config always merges: it is mostly the user's own settings
		let base = match (preserve, file) {
			(Preserve::Regenerate, DocFile::CodexConfig) => existing.as_deref(),
			(Preserve::Regenerate, _) => None,
			(Preserve::UserNotes, _) => existing.as_deref(),
		};

		let rendered = match file {
			DocFile::WsyncMd => Some(wsync_md(base, docs)),
			DocFile::AgentsMd => Some(agents_md(base, docs)),
			DocFile::ClaudeMd => Some(claude_md(base)),
			DocFile::CodexConfig => match codex_config(base) {
				Ok(rendered) => Some(rendered),
				Err(skip) => {
					outcomes.push(FileOutcome {
						file,
						path,
						status: WriteStatus::Skipped(skip.to_string()),
					});

					continue;
				}
			},
		};

		let Some(rendered) = rendered else {
			continue;
		};

		let status = if existing.as_deref() == Some(rendered.as_str()) {
			WriteStatus::Unchanged
		} else {
			if let Some(parent) = path.parent() {
				fs::create_dir_all(parent)
					.with_context(|| format!("Failed to create directory: {}", parent.display()))?;
			}

			fs::write(&path, &rendered).with_context(|| format!("Failed to write: {}", path.display()))?;

			if existing.is_some() {
				WriteStatus::Updated
			} else {
				WriteStatus::Created
			}
		};

		outcomes.push(FileOutcome { file, path, status });
	}

	Ok(outcomes)
}

/// `None` when the file does not exist. Any other read failure is an error
/// rather than a silent overwrite of content we could not see
fn read_existing(path: &Path) -> Result<Option<String>> {
	match fs::read_to_string(path) {
		Ok(text) => Ok(Some(text)),
		Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
		Err(error) => Err(error).with_context(|| format!("Failed to read: {}", path.display())),
	}
}
