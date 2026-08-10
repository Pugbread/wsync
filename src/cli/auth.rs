//! `auth` — the CLI Roblox Open Cloud credential store (auth.json).
//!
//! The credential is never accepted as a command-line value and never
//! printed: `set` reads it from exactly one of stdin, a file, or a named
//! environment variable; `status` reports presence plus a masked tail only;
//! `clear` removes it. Nothing here touches the network.
//!
//! The store is `secrets.json` in the WSync state directory (or an explicit
//! `--data-dir`), holding the `robloxCloudApiKey` key. On Unix both the file
//! and every write go through mode 0600 — filesystem-permission protection,
//! which auth.json is explicit is a fallback tier below an OS keychain. The
//! file is written atomically (0600 temp + rename), so a torn write can
//! never leave a world-readable or half-written secret behind.

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use colored::Colorize;
use serde_json::{json, Map, Value};
use std::{
	fs,
	io::{Read, Write},
	path::{Path, PathBuf},
};

use crate::{cli::client::print_json, daemon, wsync_info};

/// The store's key for the Open Cloud credential (auth.json)
const CREDENTIAL_KEY: &str = "robloxCloudApiKey";

/// The store's file name inside the state directory
const SECRETS_FILE: &str = "secrets.json";

/// Credentials are small; anything past this is a mistake, not a key
const MAX_CREDENTIAL_BYTES: u64 = 64 * 1024;

/// Store, inspect, or clear the CLI Roblox Open Cloud credential
#[derive(Parser)]
pub struct Auth {
	#[command(subcommand)]
	command: AuthCommand,
}

#[derive(Subcommand)]
enum AuthCommand {
	/// Store a credential from stdin, a file, or an environment variable
	Set(AuthSet),
	/// Report whether a credential is stored (never prints the credential)
	Status(AuthStatus),
	/// Remove the stored credential
	Clear(AuthClear),
}

impl Auth {
	pub fn main(self) -> Result<()> {
		match self.command {
			AuthCommand::Set(command) => command.main(),
			AuthCommand::Status(command) => command.main(),
			AuthCommand::Clear(command) => command.main(),
		}
	}
}

#[derive(Parser)]
struct AuthSet {
	/// Read the credential from stdin (the credential itself is never
	/// accepted as an argument)
	#[arg(long = "from-stdin", conflicts_with_all = ["file", "from_env"])]
	from_stdin: bool,

	/// Read the credential from this file
	#[arg(long, value_name = "PATH", conflicts_with_all = ["from_stdin", "from_env"])]
	file: Option<PathBuf>,

	/// Read the credential from this environment variable
	#[arg(long = "from-env", value_name = "NAME", conflicts_with_all = ["from_stdin", "file"])]
	from_env: Option<String>,

	/// Override the WSync state directory holding the store
	#[arg(long = "data-dir", value_name = "PATH")]
	data_dir: Option<PathBuf>,

	/// Print machine-readable JSON
	#[arg(long)]
	raw: bool,
}

impl AuthSet {
	fn main(self) -> Result<()> {
		// clap's conflicts_with rules out pairs; this rules out none
		let sources =
			usize::from(self.from_stdin) + usize::from(self.file.is_some()) + usize::from(self.from_env.is_some());

		if sources != 1 {
			bail!(
				"`{}` needs exactly one of --from-stdin, --file, or --from-env",
				"wsync auth set".bold()
			);
		}

		let credential = if self.from_stdin {
			let mut value = String::new();

			// The +1 makes an oversized paste detectable instead of silently
			// truncated into a wrong-but-stored key
			std::io::stdin()
				.take(MAX_CREDENTIAL_BYTES + 1)
				.read_to_string(&mut value)
				.context("Failed to read the credential from stdin")?;

			value
		} else if let Some(path) = &self.file {
			let metadata =
				fs::metadata(path).with_context(|| format!("Failed to inspect the --file {}", path.display()))?;

			if metadata.len() > MAX_CREDENTIAL_BYTES {
				bail!("The credential file exceeds 64 KiB — that is not an API key");
			}

			fs::read_to_string(path).with_context(|| format!("Failed to read the --file {}", path.display()))?
		} else {
			let name = self.from_env.as_deref().unwrap_or_default();

			match std::env::var(name) {
				Ok(value) => value,
				Err(_) => bail!("The environment variable `{name}` is not set (or is not valid UTF-8)"),
			}
		};

		let credential = credential.trim();

		if credential.is_empty() {
			bail!("The credential is empty");
		}

		if credential.len() as u64 > MAX_CREDENTIAL_BYTES {
			bail!("The credential exceeds 64 KiB — that is not an API key");
		}

		let path = store_path(self.data_dir.as_deref())?;

		write_credential(&path, credential)?;

		report(
			&path,
			true,
			Some(masked_tail(credential)),
			self.raw,
			"Credential stored",
		)
	}
}

#[derive(Parser)]
struct AuthStatus {
	/// Override the WSync state directory holding the store
	#[arg(long = "data-dir", value_name = "PATH")]
	data_dir: Option<PathBuf>,

	/// Print machine-readable JSON
	#[arg(long)]
	raw: bool,
}

impl AuthStatus {
	fn main(self) -> Result<()> {
		let path = store_path(self.data_dir.as_deref())?;
		let credential = read_credential(&path)?;

		match credential {
			Some(credential) => report(
				&path,
				true,
				Some(masked_tail(&credential)),
				self.raw,
				"A credential is stored",
			),
			None => report(&path, false, None, self.raw, "No credential stored"),
		}
	}
}

#[derive(Parser)]
struct AuthClear {
	/// Override the WSync state directory holding the store
	#[arg(long = "data-dir", value_name = "PATH")]
	data_dir: Option<PathBuf>,

	/// Print machine-readable JSON
	#[arg(long)]
	raw: bool,
}

impl AuthClear {
	fn main(self) -> Result<()> {
		let path = store_path(self.data_dir.as_deref())?;

		remove_credential(&path)?;

		report(&path, false, None, self.raw, "Credential cleared")
	}
}

/// Presence plus location — never the secret. The masked tail is the last
/// four characters, enough to tell keys apart and useless to an attacker
fn report(path: &Path, configured: bool, masked: Option<String>, raw: bool, message: &str) -> Result<()> {
	if raw {
		let mut record = json!({
			"ok": true,
			"configured": configured,
			"key": CREDENTIAL_KEY,
			"path": path.to_string_lossy(),
		});

		if let Some(masked) = &masked {
			record["maskedTail"] = json!(masked);
		}

		print_json(&record);

		return Ok(());
	}

	wsync_info!(
		"{message}{} ({})",
		match &masked {
			Some(masked) => format!(" — {}", masked.bold()),
			None => String::new(),
		},
		path.display()
	);

	Ok(())
}

fn masked_tail(credential: &str) -> String {
	let tail: String = credential
		.chars()
		.rev()
		.take(4)
		.collect::<Vec<_>>()
		.into_iter()
		.rev()
		.collect();

	format!("…{tail}")
}

fn store_path(data_dir: Option<&Path>) -> Result<PathBuf> {
	Ok(daemon::state_dir(data_dir)?.join(SECRETS_FILE))
}

/// The parsed store, tolerating a missing file. A store that exists but does
/// not parse is a hard error — silently overwriting it could destroy another
/// tool's secret
fn read_store(path: &Path) -> Result<Map<String, Value>> {
	let text = match fs::read_to_string(path) {
		Ok(text) => text,
		Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Map::new()),
		Err(err) => return Err(err).with_context(|| format!("Failed to read {}", path.display())),
	};

	let parsed: Value =
		serde_json::from_str(&text).with_context(|| format!("{} exists but is not valid JSON", path.display()))?;

	match parsed {
		Value::Object(map) => Ok(map),
		other => bail!(
			"{} exists but holds {}, not a JSON object",
			path.display(),
			crate::cli::client::kind_of(&other)
		),
	}
}

fn read_credential(path: &Path) -> Result<Option<String>> {
	Ok(read_store(path)?
		.get(CREDENTIAL_KEY)
		.and_then(Value::as_str)
		.map(str::to_owned))
}

/// The stored Open Cloud credential, if any — the first tier of the cloud
/// credential chain (upload.json, monetization.json). The value is handed to
/// the cloud client and never printed
pub(crate) fn stored_credential() -> Result<Option<String>> {
	read_credential(&store_path(None)?)
}

fn write_credential(path: &Path, credential: &str) -> Result<()> {
	let mut store = read_store(path)?;

	store.insert(CREDENTIAL_KEY.to_owned(), json!(credential));
	write_store(path, &store)
}

/// Removing the last key removes the file; other tools' keys survive
fn remove_credential(path: &Path) -> Result<()> {
	let mut store = read_store(path)?;

	if store.remove(CREDENTIAL_KEY).is_none() {
		return Ok(());
	}

	if store.is_empty() {
		match fs::remove_file(path) {
			Ok(()) => Ok(()),
			Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
			Err(err) => Err(err).with_context(|| format!("Failed to remove {}", path.display())),
		}
	} else {
		write_store(path, &store)
	}
}

/// Atomic 0600 write: the temp file is created with the final mode, so the
/// secret is never readable by another user for even a moment, and the
/// rename means no reader ever sees a partial store
fn write_store(path: &Path, store: &Map<String, Value>) -> Result<()> {
	let parent = path.parent().context("The credential store path has no parent")?;

	fs::create_dir_all(parent).with_context(|| format!("Failed to create {}", parent.display()))?;

	let temp = path.with_extension(format!("tmp-{}", std::process::id()));
	let mut options = fs::OpenOptions::new();

	options.write(true).create_new(true);

	#[cfg(unix)]
	{
		use std::os::unix::fs::OpenOptionsExt;

		options.mode(0o600);
	}

	let written = (|| -> Result<()> {
		let mut file = options
			.open(&temp)
			.with_context(|| format!("Failed to create {}", temp.display()))?;

		serde_json::to_writer_pretty(&mut file, &Value::Object(store.clone()))?;
		file.write_all(b"\n")?;
		file.sync_all()?;

		fs::rename(&temp, path)
			.with_context(|| format!("Failed to move the store into place at {}", path.display()))?;

		// An existing store predating this write keeps whatever mode it had;
		// pin it, so upgrading an old file also tightens it
		#[cfg(unix)]
		{
			use std::os::unix::fs::PermissionsExt;

			fs::set_permissions(path, fs::Permissions::from_mode(0o600))
				.with_context(|| format!("Failed to set permissions on {}", path.display()))?;
		}

		Ok(())
	})();

	if written.is_err() {
		fs::remove_file(&temp).ok();
	}

	written
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn masked_tails_never_leak_more_than_four_characters() {
		assert_eq!(masked_tail("abcdefgh"), "…efgh");
		assert_eq!(masked_tail("abc"), "…abc");
		assert_eq!(masked_tail("k"), "…k");
	}
}
