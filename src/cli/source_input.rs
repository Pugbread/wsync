//! Shared source transport: shell-safe inline text, a file, or stdin.
use anyhow::{bail, Context, Result};
use clap::Args;
use std::{
	fs,
	io::{self, Read},
	path::PathBuf,
};

#[derive(Args)]
#[group(required = true, multiple = false)]
pub(crate) struct SourceInput {
	/// Luau source, including leading comments; use `return ...` for results
	#[arg(long, value_name = "LUAU", allow_hyphen_values = true)]
	pub source: Option<String>,
	/// Read Luau from this file; `-` reads stdin (paths are relative to cwd)
	#[arg(long, value_name = "FILE")]
	pub source_file: Option<PathBuf>,
	/// Read Luau from stdin
	#[arg(long)]
	pub stdin: bool,
}

impl SourceInput {
	pub fn read(&self) -> Result<String> {
		// Both eval and playtest exec cap source at 1 MiB. Bound file/stdin
		// reads too, and reject before opening a Studio connection.
		const MAX_BYTES: usize = 1024 * 1024;
		let source = if let Some(source) = &self.source {
			source.clone()
		} else if self.stdin || self.source_file.as_deref() == Some(std::path::Path::new("-")) {
			let mut source = String::new();
			io::stdin()
				.take((MAX_BYTES + 1) as u64)
				.read_to_string(&mut source)
				.context("Failed to read Luau from stdin")?;
			source
		} else if let Some(path) = &self.source_file {
			let mut source = String::new();
			fs::File::open(path)
				.with_context(|| format!("Failed to open --source-file {}", path.display()))?
				.take((MAX_BYTES + 1) as u64)
				.read_to_string(&mut source)
				.with_context(|| format!("Failed to read --source-file {}", path.display()))?;
			source
		} else {
			bail!("Choose --source <luau>, --source-file <file>, or --stdin")
		};
		if source.is_empty() || source.len() > MAX_BYTES {
			bail!(
				"Source must contain 1–{MAX_BYTES} bytes; got at least {}. Split oversized work into smaller scripts",
				source.len()
			);
		}
		Ok(source)
	}
}
