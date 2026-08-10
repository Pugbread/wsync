//! Shared Roblox Open Cloud plumbing for the cloud command family
//! (`upload`, `monetization`).
//!
//! Nothing here talks to the daemon: cloud commands are plain HTTPS against
//! `https://apis.roblox.com` (or `WSYNC_CLOUD_BASE_URL`, which is how the
//! integration tests point the whole family at a local stub).
//!
//! The credential chain is fixed by upload.json/monetization.json: the
//! `wsync auth` store first, then the environment variable named by
//! `--api-key-env`, then `ROBLOX_API_KEY`, `CLOUD_API_KEY`, and
//! `ROBLOX_OPEN_CLOUD_API_KEY`, and finally (where a workspace is known)
//! the project env files `.env`, `.env.local`, and `info.env`. The
//! credential value itself is never printed and never accepted as a
//! command-line argument.

use anyhow::{bail, Context, Result};
use reqwest::blocking::multipart::Form;
use serde_json::Value;
use std::{
	env, fs,
	path::{Path, PathBuf},
	time::{Duration, Instant},
};

use crate::cli::{auth, client::truncate};

/// Environment override for the Open Cloud origin — the integration tests'
/// door to a local stub
pub const BASE_URL_ENV: &str = "WSYNC_CLOUD_BASE_URL";

const DEFAULT_BASE_URL: &str = "https://apis.roblox.com";

/// The fallback API-key environment variables, in resolution order
pub const KEY_ENV_VARS: [&str; 3] = ["ROBLOX_API_KEY", "CLOUD_API_KEY", "ROBLOX_OPEN_CLOUD_API_KEY"];

/// Project env files consulted last, in this order
const ENV_FILES: [&str; 3] = [".env", ".env.local", "info.env"];

/// Per-request deadline — cloud endpoints are slow but not minutes-slow
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// How long an asset operation may stay pending before the poller gives up
const OPERATION_DEADLINE: Duration = Duration::from_secs(120);

/// The Open Cloud origin every cloud command targets
pub fn base_url() -> String {
	let base = env::var(BASE_URL_ENV)
		.ok()
		.filter(|value| !value.trim().is_empty())
		.unwrap_or_else(|| DEFAULT_BASE_URL.to_owned());

	base.trim_end_matches('/').to_owned()
}

/// A resolved credential plus where it came from — the source is reportable,
/// the value never is
pub struct Credential {
	pub value: String,
	pub source: String,
}

/// Walks the documented credential chain. `workspace` enables the trailing
/// project-env-file tier (monetization.json); `upload` passes `None` there
pub fn resolve_credential(api_key_env: Option<&str>, workspace: Option<&Path>) -> Result<Credential> {
	if let Some(found) = find_credential(api_key_env, workspace)? {
		return Ok(found);
	}

	let named = api_key_env.map_or_else(String::new, |name| format!("`{name}`, "));

	bail!(
		"No Roblox Open Cloud credential found. Store one with `wsync auth set`, or set {named}{}, or {}",
		KEY_ENV_VARS.join(", "),
		"put one of those keys in the project's .env/info.env",
	)
}

/// [`resolve_credential`] without the failure — `discover` reports presence
/// instead of requiring it
pub fn find_credential(api_key_env: Option<&str>, workspace: Option<&Path>) -> Result<Option<Credential>> {
	if let Some(value) = auth::stored_credential()? {
		return Ok(Some(Credential {
			value,
			source: "auth store".to_owned(),
		}));
	}

	let mut names: Vec<&str> = Vec::new();

	names.extend(api_key_env);
	names.extend(KEY_ENV_VARS);

	for name in &names {
		if let Some(value) = non_empty_env(name) {
			return Ok(Some(Credential {
				value,
				source: format!("environment variable {name}"),
			}));
		}
	}

	if let Some(workspace) = workspace {
		if let Some((value, key, file)) = env_file_value(workspace, &names) {
			return Ok(Some(Credential {
				value,
				source: format!("{key} in {}", file.display()),
			}));
		}
	}

	Ok(None)
}

fn non_empty_env(name: &str) -> Option<String> {
	env::var(name)
		.ok()
		.map(|value| value.trim().to_owned())
		.filter(|value| !value.is_empty())
}

/// The first of `keys` found in the workspace's env files, with the file it
/// came from. Files are read in the fixed [`ENV_FILES`] order
pub fn env_file_value(workspace: &Path, keys: &[&str]) -> Option<(String, String, PathBuf)> {
	for file in ENV_FILES {
		let path = workspace.join(file);

		let Ok(text) = fs::read_to_string(&path) else {
			continue;
		};

		for key in keys {
			if let Some(value) = parse_env_text(&text, key) {
				return Some((value, (*key).to_owned(), path));
			}
		}
	}

	None
}

/// Minimal KEY=VALUE parsing: comments, `export ` prefixes, and surrounding
/// quotes tolerated; `=` inside the value preserved
fn parse_env_text(text: &str, wanted: &str) -> Option<String> {
	for line in text.lines() {
		let line = line.trim();

		if line.is_empty() || line.starts_with('#') {
			continue;
		}

		let line = line.strip_prefix("export ").unwrap_or(line).trim_start();

		let Some((key, value)) = line.split_once('=') else {
			continue;
		};

		if key.trim() != wanted {
			continue;
		}

		let value = value.trim();
		let value = value
			.strip_prefix('"')
			.and_then(|inner| inner.strip_suffix('"'))
			.or_else(|| value.strip_prefix('\'').and_then(|inner| inner.strip_suffix('\'')))
			.unwrap_or(value);

		if !value.is_empty() {
			return Some(value.to_owned());
		}
	}

	None
}

/// A completed Open Cloud round-trip: the HTTP status plus the body both
/// parsed and raw, so error paths can always show what actually came back
pub struct CloudResponse {
	pub status: u16,
	pub json: Option<Value>,
	pub text: String,
}

impl CloudResponse {
	pub fn success(&self) -> bool {
		(200..300).contains(&self.status)
	}

	pub fn value(&self) -> Value {
		self.json.clone().unwrap_or(Value::Null)
	}

	/// The most specific upstream error message available
	pub fn error_message(&self) -> String {
		let detail = self
			.json
			.as_ref()
			.and_then(|json| {
				json.get("message")
					.or_else(|| json.pointer("/errors/0/message"))
					.and_then(Value::as_str)
			})
			.map(str::to_owned)
			.unwrap_or_else(|| truncate(self.text.trim(), 200));

		if detail.is_empty() {
			format!("HTTP {}", self.status)
		} else {
			format!("HTTP {}: {detail}", self.status)
		}
	}
}

/// One authenticated client for a cloud command run
pub struct CloudClient {
	http: reqwest::blocking::Client,
	base: String,
	credential: String,
	bearer: bool,
}

impl CloudClient {
	pub fn new(credential: Credential, bearer: bool) -> Result<Self> {
		Ok(Self {
			http: reqwest::blocking::Client::builder().timeout(REQUEST_TIMEOUT).build()?,
			base: base_url(),
			credential: credential.value,
			bearer,
		})
	}

	fn apply_auth(&self, request: reqwest::blocking::RequestBuilder) -> reqwest::blocking::RequestBuilder {
		if self.bearer {
			request.header("Authorization", format!("Bearer {}", self.credential))
		} else {
			request.header("x-api-key", &self.credential)
		}
	}

	fn finish(response: reqwest::blocking::Response) -> CloudResponse {
		let status = response.status().as_u16();
		let text = response.text().unwrap_or_default();

		CloudResponse {
			status,
			json: serde_json::from_str(&text).ok(),
			text,
		}
	}

	pub fn get(&self, path: &str) -> Result<CloudResponse> {
		let response = self
			.apply_auth(self.http.get(format!("{}{path}", self.base)))
			.send()
			.with_context(|| format!("Failed to reach {}{path}", self.base))?;

		Ok(Self::finish(response))
	}

	pub fn post_multipart(&self, path: &str, form: Form) -> Result<CloudResponse> {
		let response = self
			.apply_auth(self.http.post(format!("{}{path}", self.base)))
			.multipart(form)
			.send()
			.with_context(|| format!("Failed to reach {}{path}", self.base))?;

		Ok(Self::finish(response))
	}

	pub fn patch_multipart(&self, path: &str, form: Form) -> Result<CloudResponse> {
		let response = self
			.apply_auth(self.http.patch(format!("{}{path}", self.base)))
			.multipart(form)
			.send()
			.with_context(|| format!("Failed to reach {}{path}", self.base))?;

		Ok(Self::finish(response))
	}

	/// Polls one Assets API operation to completion and returns its
	/// `response` payload. An operation-level `error` or a blown deadline is
	/// a hard error naming the operation id, so the caller can keep the
	/// handle in its report
	pub fn poll_operation(&self, operation_id: &str) -> Result<Value> {
		let started = Instant::now();
		let mut interval = Duration::from_millis(500);

		loop {
			let response = self.get(&format!("/assets/v1/operations/{operation_id}"))?;

			if !response.success() {
				bail!("Operation {operation_id} poll failed ({})", response.error_message());
			}

			let value = response.value();

			if let Some(error) = value.get("error").filter(|error| !error.is_null()) {
				let message = error
					.get("message")
					.and_then(Value::as_str)
					.map_or_else(|| error.to_string(), str::to_owned);

				bail!("Operation {operation_id} failed: {message}");
			}

			if value.get("done").and_then(Value::as_bool) == Some(true) {
				return Ok(value.get("response").cloned().unwrap_or(Value::Null));
			}

			if started.elapsed() >= OPERATION_DEADLINE {
				bail!(
					"Operation {operation_id} did not complete within {} s — retry later or use --no-wait",
					OPERATION_DEADLINE.as_secs()
				);
			}

			std::thread::sleep(interval);
			interval = (interval * 3 / 2).min(Duration::from_secs(3));
		}
	}
}

/// The trailing id of an operation handle — accepts a bare id, an
/// `operations/<id>` path, or a full URL tail
pub fn operation_id(handle: &str) -> &str {
	handle.rsplit('/').next().unwrap_or(handle)
}

/// The items of a list endpoint's answer, tolerating a top-level array or
/// the array living under a well-known (or any) object key
pub fn extract_items(value: &Value) -> Vec<Value> {
	if let Some(items) = value.as_array() {
		return items.clone();
	}

	if let Some(map) = value.as_object() {
		for key in ["gamePasses", "developerProducts", "data", "items"] {
			if let Some(items) = map.get(key).and_then(Value::as_array) {
				return items.clone();
			}
		}

		// A single-array-field object is unambiguous whatever the key is
		let arrays: Vec<&Vec<Value>> = map.values().filter_map(Value::as_array).collect();

		if arrays.len() == 1 {
			return arrays[0].clone();
		}
	}

	Vec::new()
}

/// The first of `keys` present on `item`, as text — cloud surfaces mix
/// numbers and numeric strings freely
pub fn field_text(item: &Value, keys: &[&str]) -> Option<String> {
	for key in keys {
		match item.get(key) {
			Some(Value::String(text)) if !text.is_empty() => return Some(text.clone()),
			Some(Value::Number(number)) => return Some(number.to_string()),
			_ => continue,
		}
	}

	None
}

#[cfg(test)]
mod tests {
	use super::*;
	use serde_json::json;

	#[test]
	fn env_text_parsing_handles_exports_quotes_and_equals() {
		assert_eq!(parse_env_text("KEY=abc", "KEY").as_deref(), Some("abc"));
		assert_eq!(parse_env_text("export KEY=abc", "KEY").as_deref(), Some("abc"));
		assert_eq!(parse_env_text("KEY=\"a=b=c\"", "KEY").as_deref(), Some("a=b=c"));
		assert_eq!(parse_env_text("KEY='abc'", "KEY").as_deref(), Some("abc"));
		assert_eq!(parse_env_text("# KEY=abc", "KEY"), None);
		assert_eq!(parse_env_text("OTHER=abc", "KEY"), None);
		assert_eq!(parse_env_text("KEY=", "KEY"), None);
	}

	#[test]
	fn list_items_tolerate_common_shapes() {
		assert_eq!(extract_items(&json!([1, 2])).len(), 2);
		assert_eq!(extract_items(&json!({ "gamePasses": [1] })).len(), 1);
		assert_eq!(extract_items(&json!({ "developerProducts": [1, 2, 3] })).len(), 3);
		assert_eq!(extract_items(&json!({ "whatever": [1], "count": 1 })).len(), 1);
		assert_eq!(extract_items(&json!({ "a": [1], "b": [2] })).len(), 0);
		assert_eq!(extract_items(&json!("nope")).len(), 0);
	}

	#[test]
	fn operation_handles_reduce_to_ids() {
		assert_eq!(operation_id("operations/op-1"), "op-1");
		assert_eq!(operation_id("op-1"), "op-1");
		assert_eq!(operation_id("assets/v1/operations/xyz"), "xyz");
	}
}
