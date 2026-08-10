use anyhow::{bail, Context, Result};
use clap::Parser;
use colored::Colorize;
use std::{
	env, fs,
	io::{self, IsTerminal},
};

use crate::{ext::PathExt, installer, logger, util, wsync_info};

/// Install this wsync binary to ~/.wsync/bin and add it to PATH
///
/// The source binary is never deleted. Non-interactive runs require --yes.
#[derive(Parser)]
pub struct Install {}

impl Install {
	pub fn main(self) -> Result<()> {
		if installer::is_managed() {
			bail!("This wsync binary is managed by a package manager; update it through that instead");
		}

		let source = env::current_exe().context("Failed to locate the running executable")?;
		let bin_dir = util::get_wsync_dir()?.join("bin");

		#[cfg(not(target_os = "windows"))]
		let target = bin_dir.join("wsync");

		#[cfg(target_os = "windows")]
		let target = bin_dir.join("wsync.exe");

		if source == target {
			wsync_info!("wsync is already running from {}", target.to_string().bold());
			return Ok(());
		}

		// The explicit consent gate that replaced the first-run self
		// installer: never prompt-less in a pipe, never delete the source
		if !io::stdin().is_terminal() && !util::env_yes() {
			bail!(
				"Refusing to install without a terminal. Re-run with {} to confirm non-interactively",
				"--yes".bold()
			);
		}

		let prompt = format!(
			"Install {} to {} and add it to PATH?",
			source.to_string().bold(),
			target.to_string().bold()
		);

		if !logger::prompt(&prompt, true) {
			wsync_info!("Installation cancelled");
			return Ok(());
		}

		fs::create_dir_all(&bin_dir)?;
		fs::copy(&source, &target).with_context(|| format!("Failed to copy binary to {}", target.display()))?;

		globenv::set_path(&bin_dir.to_string()).context("Failed to add the install directory to PATH")?;

		wsync_info!(
			"Installed wsync to {}. Open a new terminal for PATH changes to apply",
			target.to_string().bold()
		);

		Ok(())
	}
}
