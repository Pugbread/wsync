//! Conflict resolution and the Studio-first disk review: `conflicts`,
//! `resolve`, `decision`, `diff` (conflicts.json, resolve.json, decision.json,
//! diff.json).
//!
//! Two distinct objects live here and are deliberately never merged:
//!
//! * *parked conflicts* — instances the running daemon refused to propagate
//!   because both sides changed (`GET`/`POST /resolve`);
//! * the *pending disk review* (Design §7.0) — the disk-side entries the
//!   connect-time Studio-first apply left behind, shown by `diff` and
//!   answered by `decision` (`GET /review`, `GET /review/details`,
//!   `POST /review/push`, `POST /review/dismiss`). On a `"scope": "full"`
//!   project the connect-time comparison still freezes a divergence *choice*
//!   instead; that surface is answered from the desktop app.
//!
//! Neither surface exists on a daemon build that predates the conflict
//! engine; unknown routes answer with a redirect, which `Endpoint` reports as
//! "not served by this build" instead of silently rendering the daemon's home
//! page as data.

use anyhow::{bail, Result};
use clap::Parser;
use colored::Colorize;
use path_clean::PathClean;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

use crate::{
	cli::client::{clip, field, print_json, print_line, Client, Targeting},
	ext::PathExt,
	wsync_info,
};

/// Records per `GET /review/details` page (Design Appendix C caps it at 1024)
const DETAILS_PAGE_LIMIT: u32 = 1024;

/// Guards the paging loop against a daemon that keeps handing back a cursor
const DETAILS_MAX_PAGES: u32 = 4096;

/// List parked conflicts waiting for a Keep Disk or Keep Studio decision
#[derive(Parser)]
pub struct Conflicts {
	#[command(flatten)]
	targeting: Targeting,

	/// Print machine-readable JSON
	#[arg(long)]
	raw: bool,
}

impl Conflicts {
	pub fn main(self) -> Result<()> {
		let client = Client::connect(&self.targeting)?;
		let conflicts = fetch_conflicts(&client)?;

		if self.raw {
			print_json(&json!({
				"ok": true,
				"count": conflicts.len(),
				"conflicts": conflicts,
			}));

			return Ok(());
		}

		if conflicts.is_empty() {
			wsync_info!("No parked conflicts");

			return Ok(());
		}

		println!(
			"{}",
			format!("{:<10} {:<14} {:<18} {}", "KIND", "CLASS", "CLASSIFICATION", "PATH").bold()
		);

		for conflict in &conflicts {
			println!(
				"{:<10} {:<14} {:<18} {}",
				clip(field(conflict, "kind"), 10),
				clip(field(conflict, "class"), 14),
				clip(field(conflict, "classification"), 18),
				field(conflict, "path"),
			);

			println!("           {}", field(conflict, "instancePath").dimmed());
		}

		println!(
			"\n{} parked conflict(s). Resolve one with `{}`",
			conflicts.len(),
			"wsync resolve --path <file> (--disk|--studio)".bold()
		);

		Ok(())
	}
}

fn fetch_conflicts(client: &Client) -> Result<Vec<Value>> {
	let endpoint = client.get_endpoint("/resolve")?;
	let body = endpoint.json("/resolve")?;

	Ok(body
		.get("conflicts")
		.and_then(Value::as_array)
		.cloned()
		.unwrap_or_default())
}

/// Resolve one parked conflict by keeping either disk or Studio content
#[derive(Parser)]
pub struct Resolve {
	#[command(flatten)]
	targeting: Targeting,

	/// Filesystem path the conflicting instance projects to
	#[arg(long, value_name = "FILESYSTEM-PATH")]
	path: PathBuf,

	/// Push the on-disk state back to Studio
	#[arg(long, conflicts_with = "studio")]
	disk: bool,

	/// Write the Studio state to disk
	#[arg(long)]
	studio: bool,

	/// Print machine-readable JSON
	#[arg(long)]
	raw: bool,
}

impl Resolve {
	pub fn main(self) -> Result<()> {
		// Which side wins is never inferred: exactly one flag, checked before
		// anything is sent
		let keep = match (self.disk, self.studio) {
			(true, false) => "local",
			(false, true) => "studio",
			_ => bail!("Pass exactly one of --disk (keep local files) or --studio (keep the Studio state)"),
		};

		let client = Client::connect(&self.targeting)?;
		let workspace = client.target.project_path.get_parent().to_owned();
		let conflicts = fetch_conflicts(&client)?;

		let matched: Vec<&Value> = conflicts
			.iter()
			.filter(|conflict| same_path(field(conflict, "path"), &self.path, &workspace))
			.collect();

		let conflict = match matched.as_slice() {
			[conflict] => *conflict,
			[] => bail!(
				"No parked conflict for {}. List them with `{}`",
				self.path.to_string().bold(),
				"wsync conflicts".bold()
			),
			many => bail!(
				"{} parked conflicts match {} ({}) — pass the exact path from `wsync conflicts --raw`",
				many.len(),
				self.path.to_string().bold(),
				many.iter()
					.map(|conflict| field(conflict, "path"))
					.collect::<Vec<_>>()
					.join(", ")
			),
		};

		let id = field(conflict, "id");
		let path = field(conflict, "path");

		let endpoint = client.post_endpoint(
			"/resolve",
			&json!({ "id": id, "path": path, "keep": keep, "choice": keep }),
		)?;
		let body = endpoint.json("/resolve")?;

		// The daemon answers `resolved` with the conflict id it settled; a
		// boolean is accepted too so either shape reads as success
		let resolved = match body.get("resolved") {
			Some(Value::Bool(resolved)) => *resolved,
			Some(Value::String(id)) => !id.is_empty(),
			_ => false,
		};

		if body.get("ok").and_then(Value::as_bool) == Some(false) || !resolved {
			if self.raw {
				print_json(body);
			}

			bail!(
				"The daemon did not resolve {path}: {}",
				body.get("error").map_or_else(|| body.to_string(), Value::to_string)
			);
		}

		if self.raw {
			print_json(&json!({
				"ok": true,
				"resolved": true,
				"id": id,
				"path": path,
				"keep": keep,
			}));

			return Ok(());
		}

		wsync_info!(
			"Resolved {} by keeping {}",
			path.bold(),
			if keep == "local" { "the disk copy" } else { "Studio" }
		);

		Ok(())
	}
}

/// True when a conflict record's path and the user's `--path` name the same
/// file. Records may carry absolute or workspace-relative paths, so both are
/// normalized against the workspace before comparison, and a relative
/// argument may also match as a trailing segment run
fn same_path(record: &str, wanted: &Path, workspace: &Path) -> bool {
	if record.is_empty() {
		return false;
	}

	let absolute = |path: &Path| -> PathBuf {
		if path.is_absolute() {
			path.clean()
		} else {
			workspace.join(path).clean()
		}
	};

	let record_path = Path::new(record);

	if record_path == wanted || absolute(record_path) == absolute(wanted) {
		return true;
	}

	if wanted.is_absolute() {
		return false;
	}

	// Segment-boundary suffix match, so `UIController.client.luau` never
	// matches `MyUIController.client.luau`
	let record_parts: Vec<_> = record_path.components().collect();
	let wanted_parts: Vec<_> = wanted.components().collect();

	record_parts.len() >= wanted_parts.len()
		&& !wanted_parts.is_empty()
		&& record_parts[record_parts.len() - wanted_parts.len()..] == wanted_parts[..]
}

/// Show or act on the pending Studio-first disk review
#[derive(Parser)]
pub struct Decision {
	#[command(flatten)]
	targeting: Targeting,

	/// The review to act on (optional when one is pending)
	#[arg(long, alias = "choice-id", value_name = "ID")]
	review_id: Option<String>,

	/// Push every remaining review entry back to Studio (disk wins)
	#[arg(long, conflicts_with_all = ["studio", "cancel"])]
	disk: bool,

	/// Informational: Studio already won at connect (Studio-first sync)
	#[arg(long, conflicts_with = "cancel")]
	studio: bool,

	/// Dismiss the review, deleting the preserved disk copies
	#[arg(long)]
	cancel: bool,

	/// Print machine-readable JSON
	#[arg(long)]
	raw: bool,
}

impl Decision {
	pub fn main(self) -> Result<()> {
		let client = Client::connect(&self.targeting)?;
		let pending = fetch_review(&client)?;
		let is_pending = pending.get("pending").and_then(Value::as_bool) == Some(true);

		match (self.disk, self.studio, self.cancel) {
			(false, false, false) => return self.show(&client, &pending),
			(false, true, false) => return self.studio_won(is_pending),
			(true, false, false) | (false, false, true) => {}
			_ => bail!("Pass at most one of --disk, --studio or --cancel"),
		}

		let review_id = match (&self.review_id, pending.get("reviewId").and_then(Value::as_str)) {
			(Some(id), _) => id.clone(),
			(None, Some(id)) => id.to_owned(),
			(None, None) => bail!(
				"No disk review is pending, so there is nothing to answer. The Studio-first comparison runs when \
				 the Studio plugin connects"
			),
		};

		if self.cancel {
			return self.dismiss(&client, &review_id);
		}

		self.push_all(&client, &review_id)
	}

	/// `--disk`: push every remaining entry back to Studio. The CLI always
	/// pushes whole; hand-picking a subset is a desktop-app surface
	/// (decision.json)
	fn push_all(&self, client: &Client, review_id: &str) -> Result<()> {
		let endpoint = client.post_endpoint("/review/push", &json!({ "reviewId": review_id, "mode": "all" }))?;

		if endpoint.status == 404 {
			if self.raw {
				print_json(endpoint.json("/review/push").unwrap_or(&Value::Null));
			}

			bail!("Review {review_id} is stale or already handled; nothing was pushed");
		}

		let body = endpoint.json("/review/push")?;

		if body.get("ok").and_then(Value::as_bool) != Some(true) {
			if self.raw {
				print_json(body);
			}

			bail!(
				"The daemon rejected the push: {}",
				body.get("error").map_or_else(|| body.to_string(), Value::to_string)
			);
		}

		let pushed = body.get("pushed").and_then(Value::as_u64).unwrap_or(0);
		let remaining = body.get("remaining").and_then(Value::as_u64).unwrap_or(0);

		if self.raw {
			print_json(&json!({
				"ok": true,
				"reviewId": review_id,
				"pushed": pushed,
				"remaining": remaining,
			}));

			return Ok(());
		}

		wsync_info!(
			"Pushed {} review entr(ies) back to Studio ({} remaining)",
			pushed.to_string().bold(),
			remaining
		);

		Ok(())
	}

	/// `--cancel`: dismiss the review and delete the preserved copies
	fn dismiss(&self, client: &Client, review_id: &str) -> Result<()> {
		let endpoint = client.post_endpoint("/review/dismiss", &json!({ "reviewId": review_id }))?;

		if endpoint.status == 404 {
			if self.raw {
				print_json(endpoint.json("/review/dismiss").unwrap_or(&Value::Null));
			}

			bail!("Review {review_id} is stale or already handled; nothing was dismissed");
		}

		let body = endpoint.json("/review/dismiss")?;

		if body.get("ok").and_then(Value::as_bool) != Some(true) {
			if self.raw {
				print_json(body);
			}

			bail!(
				"The daemon rejected the dismissal: {}",
				body.get("error").map_or_else(|| body.to_string(), Value::to_string)
			);
		}

		if self.raw {
			print_json(&json!({ "ok": true, "reviewId": review_id, "dismissed": true }));

			return Ok(());
		}

		wsync_info!(
			"Dismissed review {}; the preserved disk copies were deleted and Studio's version stands",
			review_id.bold()
		);

		Ok(())
	}

	/// `--studio`: purely informational — under the Studio-first ruling
	/// Studio's version already landed on disk at connect (exit 0)
	fn studio_won(&self, is_pending: bool) -> Result<()> {
		if self.raw {
			print_json(&json!({ "ok": true, "studioFirst": true, "pending": is_pending }));

			return Ok(());
		}

		if is_pending {
			wsync_info!(
				"Studio already won at connect (Studio-first sync). The pending review only offers pushing disk \
				 entries back ({}) or dismissing them ({})",
				"wsync decision --disk".bold(),
				"wsync decision --cancel".bold()
			);
		} else {
			wsync_info!("Studio already won at connect (Studio-first sync); nothing is pending");
		}

		Ok(())
	}

	fn show(&self, client: &Client, pending: &Value) -> Result<()> {
		let is_pending = pending.get("pending").and_then(Value::as_bool).unwrap_or(false);

		if self.raw {
			print_json(&if is_pending {
				json!({
					"ok": true,
					"pending": true,
					"reviewId": pending.get("reviewId"),
					"stats": pending.get("stats"),
				})
			} else {
				json!({ "ok": true, "pending": false })
			});

			return Ok(());
		}

		if !is_pending {
			wsync_info!("No pending disk review (the Studio-first comparison runs when the Studio plugin connects)");

			// A "full"-scope project still uses the divergence-choice flow;
			// point at its surface instead of silently reporting nothing
			if let Ok(choice) = fetch_choice(client) {
				if choice.get("pending").and_then(Value::as_bool) == Some(true) {
					wsync_info!(
						"A full-scope divergence choice {} is pending; answer it from the desktop app",
						field(&choice, "choiceId").bold()
					);
				}
			}

			return Ok(());
		}

		println!("Review    {}", field(pending, "reviewId").bold());

		if let Some(stats) = pending.get("stats").and_then(Value::as_object) {
			let count = |key: &str| stats.get(key).and_then(Value::as_u64).unwrap_or(0);

			println!("Total     {}", count("total"));
			println!("+ only on disk       {}", count("diskOnly"));
			println!("~ differs (preserved) {}", count("differs"));
		}

		println!(
			"\nPage the entries with `{}`, push them back with `{}`, or dismiss with `{}`",
			"wsync diff".bold(),
			"wsync decision --disk".bold(),
			"wsync decision --cancel".bold()
		);

		Ok(())
	}
}

fn fetch_review(client: &Client) -> Result<Value> {
	let endpoint = client.get_endpoint("/review")?;

	Ok(endpoint.json("/review")?.clone())
}

fn fetch_choice(client: &Client) -> Result<Value> {
	let endpoint = client.get_endpoint("/choice")?;

	Ok(endpoint.json("/choice")?.clone())
}

/// List the pending Studio-first disk review — the disk entries that
/// survived the connect-time apply
#[derive(Parser)]
pub struct Diff {
	#[command(flatten)]
	targeting: Targeting,

	/// Only list entries at most this many instance-path segments deep
	#[arg(long, value_name = "N")]
	depth: Option<usize>,

	/// Print NDJSON, one review entry per line
	#[arg(long)]
	raw: bool,
}

/// Review what stayed disk-side after connect (identical to `wsync diff`)
///
/// The two names exist so both a review question and a resync question read
/// naturally (changes.json); delegation pins the output contracts together,
/// exactly like `tail` over `logs`.
#[derive(Parser)]
pub struct Changes {
	#[command(flatten)]
	targeting: Targeting,

	/// Only list entries at most this many instance-path segments deep
	#[arg(long, value_name = "N")]
	depth: Option<usize>,

	/// Print NDJSON, one review entry per line
	#[arg(long)]
	raw: bool,
}

impl Changes {
	pub fn main(self) -> Result<()> {
		Diff {
			targeting: self.targeting,
			depth: self.depth,
			raw: self.raw,
		}
		.main()
	}
}

impl Diff {
	pub fn main(self) -> Result<()> {
		let client = Client::connect(&self.targeting)?;
		let pending = fetch_review(&client)?;

		if pending.get("pending").and_then(Value::as_bool) != Some(true) {
			if !self.raw {
				wsync_info!(
					"No pending disk review. The Studio-first comparison runs when the Studio plugin connects — a \
					 clean connect leaves nothing behind to review"
				);
			}

			return Ok(());
		}

		let review_id = pending
			.get("reviewId")
			.and_then(Value::as_str)
			.unwrap_or_default()
			.to_owned();

		let (items, total) = self.fetch_details(&client, &review_id)?;

		if self.raw {
			for item in &items {
				let mut record = item.clone();

				if let Some(object) = record.as_object_mut() {
					// diff.json: entries carry the reviewId they belong to
					object.insert("reviewId".to_owned(), json!(review_id));
				}

				print_line(&record.to_string());
			}

			return Ok(());
		}

		println!("Review    {}", review_id.bold());

		if let Some(stats) = pending.get("stats").and_then(Value::as_object) {
			let count = |key: &str| stats.get(key).and_then(Value::as_u64).unwrap_or(0);

			println!(
				"{} disk-side entr(ies): + {} only on disk (untouched), ~ {} differ (disk original preserved)",
				count("total"),
				count("diskOnly"),
				count("differs"),
			);
		}

		println!();

		let mut shown: usize = 0;

		for item in &items {
			// Disk-only entries may carry no instance path; depth filters on
			// whichever path the entry has
			let located = {
				let instance_path = field(item, "instancePath");

				if instance_path.is_empty() {
					field(item, "path")
				} else {
					instance_path
				}
			};

			if let Some(depth) = self.depth {
				let segments = located.split('/').filter(|part| !part.is_empty()).count();

				if segments > depth {
					continue;
				}
			}

			shown += 1;

			println!(
				"{} {:<14} {:<44} {}",
				marker(field(item, "state")),
				clip(field(item, "class"), 14),
				clip(located, 44),
				field(item, "path"),
			);
		}

		let omitted = items.len() - shown;

		println!("\n{shown} of {total} entr(ies) listed");

		if omitted > 0 {
			println!("{omitted} entr(ies) hidden by --depth {}", self.depth.unwrap_or(0));
		}

		println!(
			"\nPush everything back with `{}` or dismiss with `{}`",
			"wsync decision --disk".bold(),
			"wsync decision --cancel".bold()
		);

		Ok(())
	}

	/// Pages `GET /review/details` until the cursor runs out
	fn fetch_details(&self, client: &Client, review_id: &str) -> Result<(Vec<Value>, u64)> {
		let mut items = Vec::new();
		let mut cursor: Option<String> = None;
		let mut total = 0;

		for _ in 0..DETAILS_MAX_PAGES {
			let query = match &cursor {
				Some(cursor) => {
					format!("/review/details?reviewId={review_id}&cursor={cursor}&limit={DETAILS_PAGE_LIMIT}")
				}
				None => format!("/review/details?reviewId={review_id}&limit={DETAILS_PAGE_LIMIT}"),
			};

			let endpoint = client.get_endpoint(&query)?;
			let body = endpoint.json("/review/details")?;

			total = body.get("totalCount").and_then(Value::as_u64).unwrap_or(total);

			let page = body.get("items").and_then(Value::as_array).cloned().unwrap_or_default();
			let page_len = page.len();

			items.extend(page);

			// The cursor is an opaque token: the daemon pages by numeric item
			// id, but a string cursor is carried through unchanged
			let next = body.get("nextCursor").and_then(|cursor| match cursor {
				Value::String(cursor) => Some(cursor.clone()),
				Value::Number(cursor) => Some(cursor.to_string()),
				_ => None,
			});

			match next {
				// A cursor that does not advance would page forever
				Some(next) if Some(&next) != cursor.as_ref() && page_len > 0 => cursor = Some(next),
				_ => {
					let total = total.max(items.len() as u64);

					return Ok((items, total));
				}
			}
		}

		bail!("The daemon kept paging /review/details past {DETAILS_MAX_PAGES} pages; refusing to loop further")
	}
}

/// `state` marker (diff.json's classification vocabulary). Normalized so
/// either `diskOnly` or `disk-only` renders the same; the choice-flow states
/// keep their markers for full-scope tooling
fn marker(state: &str) -> &'static str {
	let normalized: String = state
		.chars()
		.filter(char::is_ascii_alphanumeric)
		.map(|character| character.to_ascii_lowercase())
		.collect();

	match normalized.as_str() {
		"diskonly" | "onlyondisk" => "+",
		"differs" => "~",
		"missingondisk" => "-",
		_ => "?",
	}
}

#[cfg(test)]
mod tests {
	use super::{marker, same_path};
	use std::path::Path;

	#[test]
	fn state_markers_tolerate_either_casing() {
		assert_eq!(marker("disk-only"), "+");
		assert_eq!(marker("diskOnly"), "+");
		assert_eq!(marker("onlyOnDisk"), "+");
		assert_eq!(marker("only-on-disk"), "+");
		assert_eq!(marker("differs"), "~");
		assert_eq!(marker("missing-on-disk"), "-");
		assert_eq!(marker("something-else"), "?");
	}

	#[test]
	fn conflict_paths_match_absolute_and_relative_records() {
		let workspace = Path::new("/tmp/place");
		let wanted = Path::new("src/Client/UI.client.luau");

		assert!(same_path("src/Client/UI.client.luau", wanted, workspace));
		assert!(same_path("/tmp/place/src/Client/UI.client.luau", wanted, workspace));
		assert!(!same_path("src/Client/MyUI.client.luau", wanted, workspace));
		assert!(!same_path("", wanted, workspace));
	}
}
