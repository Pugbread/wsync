//! `.codex/config.toml` — ensure Codex falls back to the WSync agent docs
//! without disturbing anything else the user configured.
//!
//! The edit is textual on purpose. The `toml` crate in the tree is a
//! serde front end: a parse/serialize round trip preserves keys and values but
//! discards comments, key order and inline formatting, and this file usually
//! holds hand-written MCP server config. So the value is spliced into the raw
//! text, and the result is only accepted after it parses back to the expected
//! value. If anything does not line up, the file is left exactly as it was.

use std::fmt;

/// Filenames WSync requires, in the order Codex should try them
pub const FALLBACK_FILENAMES: [&str; 3] = ["wsync.md", "AGENTS.md", "CLAUDE.md"];

const KEY: &str = "project_doc_fallback_filenames";

const NEW_FILE_PREAMBLE: &str = "\
# Codex reads these project docs, in order, when it starts in this directory.
# WSync manages the `project_doc_fallback_filenames` key; every other key here
# is yours and is preserved across `wsync refresh`.";

/// Why a `.codex/config.toml` was left untouched
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodexSkip {
	/// The existing file is not valid TOML; rewriting it would risk the user's
	/// MCP configuration
	Unparseable,
	/// The key could not be placed without changing the file's meaning
	Unplaceable,
}

impl fmt::Display for CodexSkip {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::Unparseable => write!(formatter, "existing .codex/config.toml is not valid TOML"),
			Self::Unplaceable => write!(
				formatter,
				"could not set `{KEY}` in .codex/config.toml without rewriting it"
			),
		}
	}
}

/// Returns the contents `.codex/config.toml` should have.
///
/// `Ok(text)` may be byte-identical to `existing` when the key is already
/// correct. `Err` means the file must be left alone.
pub fn ensure_fallbacks(existing: Option<&str>) -> Result<String, CodexSkip> {
	let Some(existing) = existing.filter(|text| !text.trim().is_empty()) else {
		return Ok(format!("{NEW_FILE_PREAMBLE}\n{}\n", assignment(&required())));
	};

	let table: toml::Table = toml::from_str(existing).map_err(|_| CodexSkip::Unparseable)?;

	let current = table.get(KEY).and_then(string_array);
	let merged = merge(current.as_deref().unwrap_or_default());

	if current.as_deref() == Some(merged.as_slice()) {
		// Already correct: do not touch a single byte
		return Ok(ensure_trailing_newline(existing));
	}

	let assignment = assignment(&merged);

	// Preferred: replace the existing top-level assignment in place, keeping
	// surrounding comments and ordering
	if let Some((start, end)) = find_top_level_assignment(existing) {
		let mut candidate = existing.to_owned();
		candidate.replace_range(start..end, &assignment);
		let candidate = ensure_trailing_newline(&candidate);

		if verify(&candidate, &merged) {
			return Ok(candidate);
		}
	}

	// Otherwise insert it as a top-level key, which TOML requires to sit above
	// the first table header
	let insert_at = first_table_header(existing).unwrap_or(existing.len());
	let mut candidate = existing.to_owned();
	candidate.insert_str(insert_at, &format!("{assignment}\n"));
	let candidate = ensure_trailing_newline(&candidate);

	if verify(&candidate, &merged) {
		return Ok(candidate);
	}

	// Last resort for files whose shape defeated the scan (a table header
	// inside a multi-line string, say): append and re-verify
	let mut candidate = ensure_trailing_newline(existing);
	candidate.push_str(&format!("{assignment}\n"));

	if verify(&candidate, &merged) {
		return Ok(candidate);
	}

	Err(CodexSkip::Unplaceable)
}

fn required() -> Vec<String> {
	FALLBACK_FILENAMES.iter().map(|name| (*name).to_owned()).collect()
}

/// WSync's filenames first, then whatever else the user had, de-duplicated.
/// Stable, so a second refresh is a no-op
fn merge(current: &[String]) -> Vec<String> {
	let mut merged = required();

	for name in current {
		if !merged.iter().any(|existing| existing == name) {
			merged.push(name.clone());
		}
	}

	merged
}

fn assignment(values: &[String]) -> String {
	let values = values
		.iter()
		.map(|value| format!("{:?}", value))
		.collect::<Vec<String>>()
		.join(", ");

	format!("{KEY} = [{values}]")
}

fn string_array(value: &toml::Value) -> Option<Vec<String>> {
	let array = value.as_array()?;

	array
		.iter()
		.map(|item| item.as_str().map(|item| item.to_owned()))
		.collect()
}

fn verify(candidate: &str, expected: &[String]) -> bool {
	toml::from_str::<toml::Table>(candidate)
		.ok()
		.and_then(|table| table.get(KEY).and_then(string_array))
		.is_some_and(|value| value == expected)
}

fn ensure_trailing_newline(text: &str) -> String {
	if text.ends_with('\n') {
		text.to_owned()
	} else {
		format!("{text}\n")
	}
}

/// Byte offset of the first line that opens a table (`[x]` or `[[x]]`).
/// Top-level keys must be inserted above it
fn first_table_header(text: &str) -> Option<usize> {
	let mut offset = 0;

	for line in text.split_inclusive('\n') {
		if line.trim_start().starts_with('[') {
			return Some(offset);
		}

		offset += line.len();
	}

	None
}

/// Byte span of a top-level `project_doc_fallback_filenames = …` assignment,
/// value included
fn find_top_level_assignment(text: &str) -> Option<(usize, usize)> {
	let mut offset = 0;

	for line in text.split_inclusive('\n') {
		let trimmed = line.trim_start();

		// Table headers end the top-level section
		if trimmed.starts_with('[') {
			return None;
		}

		if let Some(rest) = trimmed.strip_prefix(KEY) {
			if rest.trim_start().starts_with('=') {
				let start = offset + (line.len() - trimmed.len());
				let equals = start + KEY.len() + rest.find('=')?;

				return value_end(text, equals + 1).map(|end| (start, end));
			}
		}

		offset += line.len();
	}

	None
}

/// End of the value that starts at `from`, following an array across lines
fn value_end(text: &str, from: usize) -> Option<usize> {
	let bytes = text.as_bytes();
	let mut index = from;

	while index < bytes.len() && (bytes[index] == b' ' || bytes[index] == b'\t') {
		index += 1;
	}

	if index >= bytes.len() {
		return None;
	}

	if bytes[index] != b'[' {
		// Scalar value: the assignment ends with the line
		let end = text[index..].find('\n').map(|end| index + end).unwrap_or(bytes.len());
		return Some(end);
	}

	let mut depth = 0usize;
	let mut quote: Option<u8> = None;

	while index < bytes.len() {
		let byte = bytes[index];

		match quote {
			Some(open) => {
				if byte == b'\\' && open == b'"' {
					index += 1;
				} else if byte == open {
					quote = None;
				}
			}
			None => match byte {
				b'"' | b'\'' => quote = Some(byte),
				b'[' => depth += 1,
				b']' => {
					depth -= 1;

					if depth == 0 {
						return Some(index + 1);
					}
				}
				b'#' => {
					// A comment inside an array runs to end of line
					let end = text[index..].find('\n').map(|end| index + end)?;
					index = end;
				}
				_ => {}
			},
		}

		index += 1;
	}

	None
}
