//! `upload` — Roblox Open Cloud asset uploads from files or directories
//! (upload.json).
//!
//! Every file becomes one multipart `POST /assets/v1/assets` (a `request`
//! JSON part plus the `fileContent` bytes), then — unless `--no-wait` — the
//! returned operation is polled to completion so the caller gets a real
//! asset id, not a handle. Failures are per-file: the batch continues,
//! every file gets its record (NDJSON under `--raw`, `--manifest` for the
//! whole batch), and the command exits non-zero when any file failed.
//!
//! Asset types are inferred from the extension; `--asset-type` overrides
//! the inference for the decal-vs-image and Model-vs-Animation (`.rbxm`)
//! ambiguities the extension cannot settle. Directory recursion skips
//! unsupported files instead of failing on them; a file named explicitly
//! must upload or count as a failure.

use anyhow::{bail, Context, Result};
use clap::{Parser, ValueEnum};
use colored::Colorize;
use reqwest::blocking::multipart::{Form, Part};
use serde_json::{json, Value};
use std::{
	fs,
	path::{Path, PathBuf},
};

use crate::{
	cli::client::{print_json, truncate},
	cli::cloud::{self, CloudClient},
	ext::PathExt,
	project::{self, Project},
	wsync_info, wsync_warn,
};

/// Upload Roblox assets through Open Cloud from files or directories
#[derive(Parser)]
pub struct Upload {
	/// Files or directories to upload (directories recurse)
	#[arg(value_name = "FILES-OR-DIRECTORIES", required = true)]
	targets: Vec<PathBuf>,

	/// Project path (supplies the default group creator)
	#[arg(long, value_name = "PATH")]
	project: Option<PathBuf>,

	/// Creator context, `user:<id>` or `group:<id>` (default: the project's
	/// groupId)
	#[arg(long, value_name = "USER-OR-GROUP")]
	creator: Option<String>,

	/// Display name for the uploaded asset(s) (default: the file stem)
	#[arg(long, value_name = "NAME")]
	name: Option<String>,

	/// Override the inferred asset type (decal, and the Model-vs-Animation
	/// `.rbxm` ambiguity)
	#[arg(long = "asset-type", value_enum, value_name = "TYPE")]
	asset_type: Option<AssetType>,

	/// Write the complete per-file result manifest to this JSON file
	#[arg(long, value_name = "FILE")]
	manifest: Option<PathBuf>,

	/// Credential mode: `api-key` sends x-api-key, `bearer` sends an OAuth
	/// Authorization header
	#[arg(long, value_enum, value_name = "MODE", default_value = "api-key")]
	auth: AuthMode,

	/// Environment variable to read the API key from (after the auth store)
	#[arg(long = "api-key-env", value_name = "NAME")]
	api_key_env: Option<String>,

	/// Return operation handles instead of polling each asset to completion
	#[arg(long = "no-wait")]
	no_wait: bool,

	/// Print one machine-readable JSON line per file (NDJSON)
	#[arg(long)]
	raw: bool,
}

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
enum AuthMode {
	ApiKey,
	Bearer,
}

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
enum AssetType {
	Animation,
	Audio,
	Image,
	Decal,
	Mesh,
	Model,
	Video,
}

impl AssetType {
	/// The Open Cloud `assetType` string
	fn cloud_name(self) -> &'static str {
		match self {
			AssetType::Animation => "Animation",
			AssetType::Audio => "Audio",
			AssetType::Image => "Image",
			AssetType::Decal => "Decal",
			AssetType::Mesh => "Mesh",
			AssetType::Model => "Model",
			AssetType::Video => "Video",
		}
	}
}

/// Extension inference: the type plus the content type the part is tagged
/// with. `None` means "not an uploadable asset file"
fn infer(extension: &str) -> Option<(AssetType, &'static str)> {
	let inferred = match extension.to_ascii_lowercase().as_str() {
		"png" => (AssetType::Image, "image/png"),
		"jpg" | "jpeg" => (AssetType::Image, "image/jpeg"),
		"bmp" => (AssetType::Image, "image/bmp"),
		"tga" => (AssetType::Image, "image/tga"),
		"mp3" => (AssetType::Audio, "audio/mpeg"),
		"ogg" => (AssetType::Audio, "audio/ogg"),
		"wav" => (AssetType::Audio, "audio/wav"),
		"flac" => (AssetType::Audio, "audio/flac"),
		"fbx" => (AssetType::Model, "model/fbx"),
		// `.rbxm` is the documented ambiguity: it uploads as Model unless
		// --asset-type animation overrides
		"rbxm" => (AssetType::Model, "application/octet-stream"),
		"rbxmx" => (AssetType::Model, "application/xml"),
		"obj" => (AssetType::Mesh, "model/obj"),
		"mp4" => (AssetType::Video, "video/mp4"),
		"mov" => (AssetType::Video, "video/quicktime"),
		"webm" => (AssetType::Video, "video/webm"),
		_ => return None,
	};

	Some(inferred)
}

/// `user:<id>` / `group:<id>`, already validated
enum Creator {
	User(u64),
	Group(u64),
}

impl Creator {
	fn parse(text: &str) -> Result<Self> {
		let (kind, id) = text
			.split_once(':')
			.with_context(|| format!("--creator must be user:<id> or group:<id>, not `{text}`"))?;

		let id: u64 = id
			.trim()
			.parse()
			.with_context(|| format!("--creator id must be numeric, not `{id}`"))?;

		match kind.trim().to_ascii_lowercase().as_str() {
			"user" => Ok(Creator::User(id)),
			"group" => Ok(Creator::Group(id)),
			other => bail!("--creator kind must be `user` or `group`, not `{other}`"),
		}
	}

	fn context(&self) -> Value {
		match self {
			// Open Cloud int64 fields travel as strings
			Creator::User(id) => json!({ "creator": { "userId": id.to_string() } }),
			Creator::Group(id) => json!({ "creator": { "groupId": id.to_string() } }),
		}
	}

	fn describe(&self) -> String {
		match self {
			Creator::User(id) => format!("user:{id}"),
			Creator::Group(id) => format!("group:{id}"),
		}
	}
}

/// One file the walk decided about — either uploadable with a settled type,
/// or recorded as skipped/failed before any network work
struct Planned {
	path: PathBuf,
	asset_type: AssetType,
	content_type: &'static str,
}

impl Upload {
	pub fn main(self) -> Result<()> {
		let project_path = project::resolve(self.project.clone().unwrap_or_default())?;
		let project = if project_path.exists() {
			Project::load(&project_path).ok()
		} else {
			None
		};
		let workspace = project_path.get_parent().to_owned();

		let creator = match &self.creator {
			Some(creator) => Creator::parse(creator)?,
			None => match project.as_ref().and_then(|project| project.group_id) {
				Some(group_id) => Creator::Group(group_id),
				None => bail!(
					"No creator context: pass {} or set `groupId` in the project file",
					"--creator user:<id>|group:<id>".bold()
				),
			},
		};

		let mut records: Vec<Value> = Vec::new();
		let mut planned: Vec<Planned> = Vec::new();

		for target in &self.targets {
			let target = target.resolve()?;

			if target.is_dir() {
				self.walk(&target, &mut planned, &mut records)?;
			} else if target.is_file() {
				match self.plan_file(&target, true) {
					Ok(plan) => planned.push(plan),
					Err(err) => records.push(self.emit(record_failed(&target, &err.to_string()))),
				}
			} else {
				bail!("{} does not exist", target.to_string().bold());
			}
		}

		if planned.is_empty() && records.iter().all(|record| record["status"] != "failed") {
			bail!("Nothing to upload — no supported asset files in the given paths");
		}

		let credential = cloud::resolve_credential(self.api_key_env.as_deref(), Some(&workspace))?;
		let client = CloudClient::new(credential, self.auth == AuthMode::Bearer)?;

		for plan in &planned {
			let record = match self.upload_one(&client, plan, &creator) {
				Ok(record) => record,
				Err(err) => record_failed(&plan.path, &err.to_string()),
			};

			records.push(self.emit(record));
		}

		let failed = count(&records, "failed");
		let uploaded = count(&records, "uploaded");
		let pending = count(&records, "pending");
		let skipped = count(&records, "skipped");

		if let Some(manifest) = &self.manifest {
			let manifest = manifest.resolve()?;
			let body = json!({
				"ok": failed == 0,
				"creator": creator.describe(),
				"uploaded": uploaded,
				"pending": pending,
				"failed": failed,
				"skipped": skipped,
				"results": records,
			});

			if let Some(parent) = manifest.parent() {
				fs::create_dir_all(parent).ok();
			}

			fs::write(&manifest, format!("{}\n", serde_json::to_string_pretty(&body)?))
				.with_context(|| format!("Failed to write the manifest at {}", manifest.to_string()))?;

			if !self.raw {
				wsync_info!("Manifest written to {}", manifest.to_string().bold());
			}
		}

		if !self.raw {
			wsync_info!(
				"{} uploaded, {} pending, {} failed, {} skipped (creator {})",
				uploaded.to_string().bold(),
				pending,
				failed,
				skipped,
				creator.describe()
			);
		}

		if failed > 0 {
			bail!("{failed} of {} upload(s) failed", uploaded + pending + failed);
		}

		Ok(())
	}

	/// Recursive directory walk in deterministic order. Unsupported files
	/// are skipped records (upload.json); dot-entries are ignored outright
	fn walk(&self, dir: &Path, planned: &mut Vec<Planned>, records: &mut Vec<Value>) -> Result<()> {
		let mut entries: Vec<PathBuf> = fs::read_dir(dir)
			.with_context(|| format!("Failed to read the directory {}", dir.to_string()))?
			.filter_map(Result::ok)
			.map(|entry| entry.path())
			.filter(|path| !path.get_name().starts_with('.'))
			.collect();

		entries.sort();

		for entry in entries {
			if entry.is_dir() {
				self.walk(&entry, planned, records)?;
			} else {
				match self.plan_file(&entry, false) {
					Ok(plan) => planned.push(plan),
					Err(reason) => {
						records.push(self.emit(json!({
							"ok": true,
							"file": entry.to_string(),
							"status": "skipped",
							"reason": reason.to_string(),
						})));
					}
				}
			}
		}

		Ok(())
	}

	/// Settles a file's asset type before any network work. `explicit` files
	/// must resolve; walked files may be skipped
	fn plan_file(&self, path: &Path, explicit: bool) -> Result<Planned> {
		let inferred = infer(path.get_ext());

		let (asset_type, content_type) = match (self.asset_type, inferred) {
			// The override wins the type; the extension still names the bytes
			(Some(kind), Some((_, content_type))) => (kind, content_type),
			(Some(kind), None) if explicit => (kind, "application/octet-stream"),
			(None, Some(inferred)) => inferred,
			(Some(_), None) | (None, None) => bail!(
				"unsupported extension `.{}`{}",
				path.get_ext(),
				if explicit {
					" — pass --asset-type to force it"
				} else {
					""
				}
			),
		};

		Ok(Planned {
			path: path.to_owned(),
			asset_type,
			content_type,
		})
	}

	fn upload_one(&self, client: &CloudClient, plan: &Planned, creator: &Creator) -> Result<Value> {
		let bytes =
			fs::read(&plan.path).with_context(|| format!("Failed to read the file {}", plan.path.to_string()))?;

		let display_name = self
			.name
			.clone()
			.unwrap_or_else(|| plan.path.get_stem().to_owned())
			.trim()
			.to_owned();

		let request = json!({
			"assetType": plan.asset_type.cloud_name(),
			"displayName": display_name,
			"description": "",
			"creationContext": creator.context(),
		});

		let part = Part::bytes(bytes)
			.file_name(plan.path.get_name().to_owned())
			.mime_str(plan.content_type)
			.context("Invalid content type for the file part")?;

		let form = Form::new()
			.text("request", request.to_string())
			.part("fileContent", part);

		let response = client.post_multipart("/assets/v1/assets", form)?;

		if !response.success() {
			bail!("create failed ({})", response.error_message());
		}

		let value = response.value();
		let handle = value
			.get("operationId")
			.and_then(Value::as_str)
			.map(str::to_owned)
			.or_else(|| value.get("path").and_then(Value::as_str).map(str::to_owned))
			.context("Open Cloud answered the create without an operation handle")?;

		let operation = cloud::operation_id(&handle).to_owned();

		let mut record = json!({
			"ok": true,
			"file": plan.path.to_string(),
			"name": display_name,
			"assetType": plan.asset_type.cloud_name(),
			"operationId": operation,
		});

		if self.no_wait {
			record["status"] = json!("pending");

			return Ok(record);
		}

		let completed = client.poll_operation(&operation)?;

		record["status"] = json!("uploaded");
		record["assetId"] = completed.get("assetId").cloned().unwrap_or_else(|| json!(Value::Null));

		Ok(record)
	}

	/// Prints (raw mode) and human-reports one record, then hands it back
	/// for the manifest
	fn emit(&self, record: Value) -> Value {
		if self.raw {
			print_json(&record);

			return record;
		}

		let file = record["file"].as_str().unwrap_or_default();

		match record["status"].as_str().unwrap_or_default() {
			"uploaded" => wsync_info!(
				"Uploaded {} ({}) → asset {}",
				file.bold(),
				record["assetType"].as_str().unwrap_or_default(),
				record["assetId"]
					.as_str()
					.map(str::to_owned)
					.unwrap_or_else(|| record["assetId"].to_string())
					.bold()
			),
			"pending" => wsync_info!(
				"Created {} ({}) — operation {} still processing (--no-wait)",
				file.bold(),
				record["assetType"].as_str().unwrap_or_default(),
				record["operationId"].as_str().unwrap_or_default()
			),
			"skipped" => wsync_warn!(
				"Skipped {} ({})",
				file,
				record["reason"].as_str().unwrap_or("unsupported")
			),
			_ => wsync_warn!(
				"Failed {} — {}",
				file.bold(),
				truncate(record["error"].as_str().unwrap_or("unknown error"), 300)
			),
		}

		record
	}
}

fn record_failed(path: &Path, error: &str) -> Value {
	json!({
		"ok": false,
		"file": path.to_string(),
		"status": "failed",
		"error": error,
	})
}

fn count(records: &[Value], status: &str) -> usize {
	records.iter().filter(|record| record["status"] == status).count()
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn extensions_infer_documented_types() {
		assert!(matches!(infer("png"), Some((AssetType::Image, "image/png"))));
		assert!(matches!(infer("JPG"), Some((AssetType::Image, "image/jpeg"))));
		assert!(matches!(infer("mp3"), Some((AssetType::Audio, _))));
		assert!(matches!(infer("rbxm"), Some((AssetType::Model, _))));
		assert!(matches!(infer("fbx"), Some((AssetType::Model, "model/fbx"))));
		assert!(matches!(infer("obj"), Some((AssetType::Mesh, _))));
		assert!(matches!(infer("mp4"), Some((AssetType::Video, _))));
		assert!(infer("txt").is_none());
		assert!(infer("").is_none());
	}

	#[test]
	fn creators_parse_and_reject() {
		assert!(matches!(Creator::parse("user:123"), Ok(Creator::User(123))));
		assert!(matches!(Creator::parse("group:9"), Ok(Creator::Group(9))));
		assert!(Creator::parse("123").is_err());
		assert!(Creator::parse("owner:1").is_err());
		assert!(Creator::parse("user:abc").is_err());
	}
}
