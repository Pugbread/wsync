//! The mutation audit log (Design §6.4, §11, §12): every remote op that
//! mutated Studio appends one JSON line to `writes.log` in the state
//! directory. The file rotates to `writes.log.1` (one generation kept) at
//! 10 MiB, and `/hello` exposes the resolved path as `writesLog` so the CLI's
//! `status` can report write-log health.

use chrono::{SecondsFormat, Utc};
use log::warn;
use serde_json::{json, Map, Value};
use std::{
	fs::{self, OpenOptions},
	io::Write,
	path::{Path, PathBuf},
	sync::Mutex,
};

use crate::{constants::WRITES_LOG_ROTATE_BYTES, lock};

/// Meta keys copied verbatim from a mutating response's `meta` object into
/// the audit line (the pinned line shape: `target`, `class?`, `prop?`, `to?`,
/// `count?`)
const META_FIELDS: [&str; 5] = ["target", "class", "prop", "to", "count"];

/// Append-only JSON-lines audit sink. Opening never fails — the resolved path
/// is always exposed and append errors degrade to a warning, because a broken
/// audit disk must not take the op surface down with it
#[derive(Debug)]
pub struct WritesLog {
	path: PathBuf,
	// Serializes append+rotate so two mutating ops cannot interleave a
	// rotation between the size check and the write
	guard: Mutex<()>,
}

impl WritesLog {
	pub fn new(state_dir: &Path) -> Self {
		Self {
			path: state_dir.join("writes.log"),
			guard: Mutex::new(()),
		}
	}

	/// The resolved `writes.log` path (surfaced by `/hello` as `writesLog`)
	pub fn path(&self) -> &Path {
		&self.path
	}

	/// Builds the pinned audit line for one mutating `/request` response and
	/// appends it. `meta` is the plugin's response meta (already proven to
	/// carry `mutation: true` by the caller); the pinned fields live in its
	/// nested `audit` object (the plugin's wire shape — Remote/init.luau)
	pub fn record(&self, op: &str, ok: bool, source: &str, meta: &Value) {
		let mut line = Map::new();

		line.insert(
			"ts".into(),
			json!(Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)),
		);
		line.insert("op".into(), json!(op));

		// The plugin reports `{op, target, class?, prop?, to?, count?}` nested
		// under `meta.audit`; a meta carrying the fields at its top level is
		// accepted as a fallback
		let fields = meta.get("audit").filter(|audit| audit.is_object()).unwrap_or(meta);

		for field in META_FIELDS {
			if let Some(value) = fields.get(field) {
				if !value.is_null() {
					line.insert(field.to_owned(), value.clone());
				}
			}
		}

		// `target` is part of the pinned line shape; a mutating op that
		// failed to report one still audits, with an explicit empty target
		line.entry("target").or_insert(json!(""));

		line.insert("ok".into(), json!(ok));
		line.insert("source".into(), json!(source));

		self.append(&Value::Object(line));
	}

	fn append(&self, line: &Value) {
		let _guard = lock!(self.guard);

		if let Err(err) = self.append_inner(line) {
			warn!("Failed to append to {}: {err}", self.path.display());
		}
	}

	fn append_inner(&self, line: &Value) -> std::io::Result<()> {
		if let Some(parent) = self.path.parent() {
			fs::create_dir_all(parent)?;
		}

		// Rotate before appending so the threshold bounds the live file: one
		// `.1` generation is kept, an older one is overwritten
		if let Ok(metadata) = fs::metadata(&self.path) {
			if metadata.len() >= WRITES_LOG_ROTATE_BYTES {
				let rotated = self.path.with_extension("log.1");

				fs::rename(&self.path, rotated)?;
			}
		}

		let mut options = OpenOptions::new();
		options.create(true).append(true);

		#[cfg(unix)]
		{
			use std::os::unix::fs::OpenOptionsExt;
			options.mode(0o600);
		}

		let mut file = options.open(&self.path)?;

		file.write_all(line.to_string().as_bytes())?;
		file.write_all(b"\n")?;

		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn read_lines(path: &Path) -> Vec<Value> {
		fs::read_to_string(path)
			.unwrap_or_default()
			.lines()
			.map(|line| serde_json::from_str(line).unwrap())
			.collect()
	}

	#[test]
	fn record_builds_the_pinned_line_shape() {
		let dir = tempfile::tempdir().unwrap();
		let log = WritesLog::new(dir.path());

		// The plugin's wire shape: pinned fields nested under `meta.audit`
		log.record(
			"set",
			true,
			"cli",
			&json!({
				"op": "set",
				"durationMs": 3,
				"protocol": 1,
				"mutation": true,
				"audit": {
					"op": "set",
					"target": "Workspace/Part",
					"prop": "Anchored",
					"to": true,
				},
			}),
		);

		let lines = read_lines(log.path());
		assert_eq!(lines.len(), 1);

		let line = &lines[0];
		assert_eq!(line["op"], "set");
		assert_eq!(line["target"], "Workspace/Part");
		assert_eq!(line["prop"], "Anchored");
		assert_eq!(line["to"], true);
		assert_eq!(line["ok"], true);
		assert_eq!(line["source"], "cli");
		assert!(line.get("class").is_none());
		assert!(line.get("count").is_none());
		assert!(line.get("durationMs").is_none(), "only pinned fields are copied");
		assert!(line.get("audit").is_none(), "the audit object is flattened, not copied");

		// The timestamp is real RFC 3339
		let ts = line["ts"].as_str().unwrap();
		assert!(chrono::DateTime::parse_from_rfc3339(ts).is_ok(), "bad ts: {ts}");
	}

	#[test]
	fn record_accepts_top_level_meta_fields_as_a_fallback() {
		let dir = tempfile::tempdir().unwrap();
		let log = WritesLog::new(dir.path());

		log.record(
			"set",
			true,
			"cli",
			&json!({
				"mutation": true,
				"target": "Workspace/Part",
				"prop": "Anchored",
				"durationMs": 3,
			}),
		);

		let lines = read_lines(log.path());
		assert_eq!(lines.len(), 1);
		assert_eq!(lines[0]["target"], "Workspace/Part");
		assert_eq!(lines[0]["prop"], "Anchored");
		assert!(lines[0].get("durationMs").is_none(), "only pinned fields are copied");
	}

	#[test]
	fn rotates_once_past_the_threshold() {
		let dir = tempfile::tempdir().unwrap();
		let log = WritesLog::new(dir.path());

		// A pre-existing oversized log rotates before the next line lands
		fs::write(log.path(), vec![b'x'; WRITES_LOG_ROTATE_BYTES as usize]).unwrap();

		log.record(
			"rm",
			true,
			"cli",
			&json!({ "mutation": true, "audit": { "op": "rm", "target": "X", "count": 2 } }),
		);

		let rotated = log.path().with_extension("log.1");
		assert_eq!(fs::metadata(&rotated).unwrap().len(), WRITES_LOG_ROTATE_BYTES);

		let lines = read_lines(log.path());
		assert_eq!(lines.len(), 1);
		assert_eq!(lines[0]["count"], 2);
	}
}
