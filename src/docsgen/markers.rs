//! The marker-block engine.
//!
//! Every generated section lives between an HTML comment pair:
//!
//! ```text
//! <!-- wsync:project-memory:start -->
//! …generated…
//! <!-- wsync:project-memory:end -->
//! ```
//!
//! Rules the whole docs pipeline depends on:
//!
//! - Bytes outside the block are never touched. Line endings, trailing
//!   whitespace and blank runs in user notes survive verbatim.
//! - Re-rendering with unchanged inputs produces a byte-identical file.
//! - A file with no block gets one appended; a missing file gets a preamble
//!   plus the block.
//! - Duplicate blocks (an older writer, a bad merge) collapse to one.
//! - An unterminated start marker is dropped rather than swallowing whatever
//!   follows it, so user text below a truncated write is never deleted.
//! - Marker blocks owned by other tools are opaque text and stay untouched.

use std::cmp::Reverse;

/// The full generated tool reference inside `wsync.md`
pub const PROJECT_MEMORY: &str = "wsync:project-memory";
/// The embedded copy inside `AGENTS.md`
pub const AGENT_CONTEXT: &str = "wsync:agent-context";
/// The `@AGENTS.md` import inside `CLAUDE.md`
pub const AGENTS_INCLUDE: &str = "wsync:agents-include";

pub fn start_tag(marker: &str) -> String {
	format!("<!-- {marker}:start -->")
}

pub fn end_tag(marker: &str) -> String {
	format!("<!-- {marker}:end -->")
}

/// Renders one complete block. `body` is normalized to sit on its own lines
fn block(marker: &str, body: &str) -> String {
	let body = body.trim_matches('\n');

	if body.is_empty() {
		format!("{}\n{}", start_tag(marker), end_tag(marker))
	} else {
		format!("{}\n{}\n{}", start_tag(marker), body, end_tag(marker))
	}
}

/// A `start..end` byte span in the file being merged
type Span = (usize, usize);

/// Byte spans of every complete block of `marker`, plus the spans of any
/// unterminated start markers
fn scan(text: &str, marker: &str) -> (Vec<Span>, Vec<Span>) {
	let start_tag = start_tag(marker);
	let end_tag = end_tag(marker);

	let mut blocks = Vec::new();
	let mut orphans = Vec::new();
	let mut cursor = 0;

	while let Some(offset) = text[cursor..].find(&start_tag) {
		let start = cursor + offset;
		let after_start = start + start_tag.len();

		match text[after_start..].find(&end_tag) {
			Some(offset) => {
				let end = after_start + offset + end_tag.len();
				blocks.push((start, end));
				cursor = end;
			}
			None => {
				orphans.push((start, after_start));
				cursor = after_start;
			}
		}
	}

	(blocks, orphans)
}

/// Reads the generated body out of a file, without the marker lines. Used by
/// the `AGENTS.md` embedding and by tests
pub fn extract(text: &str, marker: &str) -> Option<String> {
	let (blocks, _) = scan(text, marker);
	let (start, end) = *blocks.first()?;

	let inner_start = start + start_tag(marker).len();
	let inner_end = end - end_tag(marker).len();

	Some(text[inner_start..inner_end].trim_matches('\n').to_owned())
}

/// Extends a deletion span over one trailing newline so removing a block does
/// not leave the blank line that separated it
fn with_trailing_newline(text: &str, span: Span) -> Span {
	let (start, end) = span;

	if text[end..].starts_with('\n') {
		(start, end + 1)
	} else {
		(start, end)
	}
}

/// Replaces (or creates) the `marker` block in `existing`.
///
/// `preamble` is only used when there is no file yet — it becomes the heading
/// above a brand new block.
pub fn upsert(existing: Option<&str>, marker: &str, body: &str, preamble: &str) -> String {
	let block = block(marker, body);

	let Some(existing) = existing.filter(|text| !text.trim().is_empty()) else {
		let preamble = preamble.trim_matches('\n');

		return if preamble.is_empty() {
			format!("{block}\n")
		} else {
			format!("{preamble}\n\n{block}\n")
		};
	};

	let (blocks, orphans) = scan(existing, marker);

	// One edit list, applied back to front so earlier offsets stay valid
	let mut edits: Vec<(usize, usize, &str)> = Vec::new();

	for span in orphans {
		let (start, end) = with_trailing_newline(existing, span);
		edits.push((start, end, ""));
	}

	// A second block would silently shadow the first on the next read
	for span in blocks.iter().skip(1) {
		let (start, end) = with_trailing_newline(existing, *span);
		edits.push((start, end, ""));
	}

	if let Some((start, end)) = blocks.first() {
		edits.push((*start, *end, block.as_str()));
	}

	edits.sort_by_key(|(start, _, _)| Reverse(*start));

	let mut text = existing.to_owned();

	for (start, end, replacement) in edits {
		text.replace_range(start..end, replacement);
	}

	if blocks.is_empty() {
		if !text.is_empty() && !text.ends_with('\n') {
			text.push('\n');
		}

		if !text.is_empty() {
			text.push('\n');
		}

		text.push_str(&block);
	}

	if !text.ends_with('\n') {
		text.push('\n');
	}

	text
}
