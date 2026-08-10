use anyhow::{Context, Result};
use colored::Colorize;
use directories::UserDirs;
use env_logger::WriteStyle;
use json_formatter::JsonFormatter;
use log::LevelFilter;
use rbx_dom_weak::types::Variant;
use rbx_reflection::{ClassTag, ReflectionDatabase};
use roblox_install::RobloxStudio;
use serde::Serialize;
use std::{env, path::PathBuf, process::Command};

use crate::{ext::ResultExt, Properties};

/// Returns the `.wsync` directory
pub fn get_wsync_dir() -> Result<PathBuf> {
	let user_dirs = UserDirs::new().context("Failed to get user directory")?;

	Ok(user_dirs.home_dir().join(".wsync"))
}

/// Returns the Git or local username of the current user
pub fn get_username() -> String {
	if let Ok(output) = Command::new("git").arg("config").arg("user.name").output() {
		let username = String::from_utf8_lossy(&output.stdout).trim().to_owned();

		if !username.is_empty() {
			return username;
		}
	}

	whoami::username()
}

pub fn get_plugin_path() -> Result<PathBuf> {
	Ok(RobloxStudio::locate()?.plugins_path().join("WSync.rbxm"))
}

/// Checks if the given `class` is a service
pub fn is_service(class: &str) -> bool {
	let descriptor = get_reflection_database().classes.get(class);

	let has_tag = if let Some(descriptor) = descriptor {
		descriptor.tags.contains(&ClassTag::Service)
	} else {
		false
	};

	has_tag || class == "StarterPlayerScripts" || class == "StarterCharacterScripts"
}

/// Checks if the given `class` is a script
pub fn is_script(class: &str) -> bool {
	class == "Script" || class == "LocalScript" || class == "ModuleScript"
}

/// Kills the process with the given `pid`
pub fn kill_process(pid: u32) {
	#[cfg(not(target_os = "windows"))]
	{
		// Kill main process
		Command::new("kill").arg(pid.to_string()).output().ok();

		// Kill child processes
		Command::new("pkill").arg("-P").arg(pid.to_string()).output().ok();
	}

	// Kill both main and child processes
	#[cfg(target_os = "windows")]
	Command::new("TASKKILL")
		.arg("/F")
		.arg("/T")
		.args(["/PID", &pid.to_string()])
		.output()
		.ok();
}

pub fn process_exists(pid: u32) -> bool {
	#[cfg(not(target_os = "windows"))]
	{
		if let Ok(output) = Command::new("kill").arg("-0").arg(pid.to_string()).output() {
			output.status.success()
		} else {
			false
		}
	}

	#[cfg(target_os = "windows")]
	{
		let output = Command::new("TASKLIST")
			.arg("/NH")
			.args(["/FI", &format!("PID eq {}", pid)])
			.output();

		if let Ok(output) = output {
			String::from_utf8_lossy(&output.stdout).contains("wsync.exe")
		} else {
			false
		}
	}
}

/// Returns progress bar styling
pub fn get_progress_style() -> (String, String) {
	let mut template = match env_log_style() {
		WriteStyle::Always => "PROGRESS: ".magenta().bold().to_string(),
		_ => "PROGRESS: ".into(),
	};
	template.push_str("[{bar:40}] ({bytes}/{total_bytes})");

	(template, String::from("=>-"))
}

/// Returns the `RUST_VERBOSE` environment variable
pub fn env_verbosity() -> LevelFilter {
	let verbosity = env::var("RUST_VERBOSE").unwrap_or("ERROR".into());

	match verbosity.as_str() {
		"OFF" => LevelFilter::Off,
		"ERROR" => LevelFilter::Error,
		"WARN" => LevelFilter::Warn,
		"INFO" => LevelFilter::Info,
		"DEBUG" => LevelFilter::Debug,
		"TRACE" => LevelFilter::Trace,
		_ => LevelFilter::Error,
	}
}

/// Returns the `RUST_LOG_STYLE` environment variable
pub fn env_log_style() -> WriteStyle {
	let log_style = env::var("RUST_LOG_STYLE").unwrap_or("auto".into());

	match log_style.as_str() {
		"always" => WriteStyle::Always,
		"never" => WriteStyle::Never,
		_ => WriteStyle::Auto,
	}
}

/// Returns the `RUST_BACKTRACE` environment variable
pub fn env_backtrace() -> bool {
	let backtrace = env::var("RUST_BACKTRACE").unwrap_or("0".into());
	backtrace == "1"
}

/// Returns the `RUST_YES` environment variable
pub fn env_yes() -> bool {
	let yes = env::var("RUST_YES").unwrap_or("0".into());
	yes == "1"
}

/// Returns line of code count from snapshot's properties
pub fn count_loc_from_properties(properties: &Properties) -> usize {
	let mut loc = 0;

	for value in properties.values() {
		if let Variant::String(value) = value {
			loc += value.lines().count();
		}
	}

	loc
}

/// Serializes value to a pretty-printed and sorted JSON
pub fn serialize_json<T: Serialize>(value: &T) -> Result<Vec<u8>> {
	let formatter = JsonFormatter::new()
		.with_array_breaks(false)
		.with_extra_newline(true)
		.with_sorted_keys(true)
		.with_max_decimals(4);

	formatter.to_vec(value).desc("Failed to serialize JSON")
}

/// Returns local or bundled reflection database
pub fn get_reflection_database() -> &'static ReflectionDatabase<'static> {
	rbx_reflection_database::get_local()
		.ok()
		.flatten()
		.unwrap_or_else(rbx_reflection_database::get_bundled)
}

/// Formats a `Ref` as protocol v1's canonical 32-char lowercase hex string
/// (the all-zeros string is the root sentinel). This matches the `Ref` serde
/// human-readable representation, so refs inside serialized JSON frames come
/// out identically
pub fn format_ref(id: rbx_dom_weak::types::Ref) -> String {
	id.to_string()
}

/// Strictly parses a protocol v1 JSON ref: exactly 32 lowercase hex chars.
/// The all-zeros string parses to the none/root sentinel. Anything else
/// (wrong length, uppercase, non-hex) is rejected with a clear error so a
/// frame containing a malformed ref is never partially applied
pub fn parse_ref(hex: &str) -> Result<rbx_dom_weak::types::Ref> {
	if hex.len() != 32 || !hex.bytes().all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f')) {
		anyhow::bail!("Malformed ref {hex:?}: expected exactly 32 lowercase hex characters");
	}

	hex.parse()
		.map_err(|err| anyhow::anyhow!("Malformed ref {hex:?}: {err}"))
}

#[cfg(test)]
mod tests {
	use super::*;
	use rbx_dom_weak::types::Ref;

	#[test]
	fn ref_hex_round_trips() {
		let id = Ref::new();
		let hex = format_ref(id);

		assert_eq!(hex.len(), 32);
		assert!(hex.bytes().all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f')));
		assert_eq!(parse_ref(&hex).unwrap(), id);
	}

	#[test]
	fn all_zeros_is_the_root_sentinel() {
		let root = parse_ref("00000000000000000000000000000000").unwrap();

		assert!(root.is_none());
		assert_eq!(format_ref(Ref::none()), "00000000000000000000000000000000");
	}

	#[test]
	fn malformed_refs_are_rejected() {
		// Wrong length
		assert!(parse_ref("abc123").is_err());
		assert!(parse_ref("").is_err());
		assert!(parse_ref(&"a".repeat(33)).is_err());
		// Uppercase
		assert!(parse_ref("ABCDEF00000000000000000000000000").is_err());
		// Non-hex
		assert!(parse_ref("zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz").is_err());
		// Sign or whitespace smuggled past a lax integer parser
		assert!(parse_ref("+1234567890123456789012345678901").is_err());
		assert!(parse_ref(" 1234567890123456789012345678901").is_err());
	}

	#[test]
	fn json_serialization_matches_the_codec() {
		let id = Ref::new();

		assert_eq!(
			serde_json::to_value(id).unwrap(),
			serde_json::Value::String(format_ref(id))
		);
	}
}
