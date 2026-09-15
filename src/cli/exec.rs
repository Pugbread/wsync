use anyhow::{bail, Context, Result};
use clap::Parser;
use reqwest::{blocking::Client, header::CONTENT_TYPE};
use serde::Serialize;
use std::{
	fs,
	path::{Path, PathBuf},
	time::Duration,
};

use crate::{
	cli::client::{self, Targeting},
	ext::PathExt,
	sessions, wsync_info,
};

/// Execute Luau code in Roblox Studio (requires running session)
#[derive(Parser)]
pub struct Exec {
	/// Luau code to execute (can be file path)
	#[arg(allow_hyphen_values = true)]
	code: String,

	/// Bind the broadcast to this project's daemon
	#[arg(long, value_name = "PATH", conflicts_with_all = ["session", "host", "standalone"])]
	project: Option<PathBuf>,

	/// Print queue acknowledgement as JSON; execution is fire-and-forget
	#[arg(long)]
	raw: bool,

	/// Session identifier
	#[arg()]
	session: Option<String>,

	/// Focus Roblox Studio window when executing code
	#[arg(short, long)]
	focus: bool,

	/// Launch Roblox Studio, run code and return the result
	#[arg(short, long)]
	standalone: bool,

	/// Server host name
	#[arg(short = 'H', long)]
	host: Option<String>,

	/// Server port
	#[arg(short = 'P', long)]
	port: Option<u16>,
}

impl Exec {
	pub fn main(self) -> Result<()> {
		let code = if let Some(path) = Path::new(&self.code)
			.resolve()
			.ok()
			.and_then(|path| path.exists().then_some(path))
		{
			fs::read_to_string(path)?
		} else {
			self.code
		};

		if self.standalone {
			bail!("--standalone is not implemented; use `playtest run` for a managed test or `eval` in a connected Studio");
		}
		let (address, expected_project, expected_boot_id) = if let Some(project) = self.project {
			let live = client::Client::connect(&Targeting {
				project: Some(project),
				port: self.port,
			})?;
			(
				format!("http://localhost:{}", live.port()),
				live.target.canonical.clone(),
				Some(live.hello.boot_id.clone()),
			)
		} else {
			let session = sessions::get(self.session, self.host, self.port)?
				.context("No running session was found; pass --project <path> to select a daemon")?;
			(
				session.get_address().context("The selected session has no address")?,
				None,
				None,
			)
		};
		let body = rmp_serde::to_vec_named(&Request {
			code,
			focus: self.focus && !cfg!(target_os = "windows"),
			expected_project,
			expected_boot_id,
		})?;
		let response = Client::builder()
			.no_proxy()
			.timeout(Duration::from_secs(10))
			.redirect(reqwest::redirect::Policy::none())
			.build()?
			.post(format!("{address}/exec"))
			.header(CONTENT_TYPE, "application/msgpack")
			.body(body)
			.send()
			.and_then(reqwest::blocking::Response::error_for_status);
		if let Err(error) = response {
			if self.raw {
				client::print_json(
					&serde_json::json!({"ok":false,"error":{"code":"EXEC_DELIVERY_FAILED","message":error.to_string()},"meta":{"completion":"unknown","retrySafe":false}}),
				);
			}
			bail!("Exec delivery failed: {error}. Execution may have started; inspect affected state before retrying");
		}
		if self.raw {
			client::print_json(
				&serde_json::json!({"ok":true,"queued":true,"meta":{"completion":"unknown","retrySafe":false}}),
			);
		} else {
			wsync_info!(
				"Code queued for broadcast. Execution is not confirmed; use `wsync eval` when you need a result."
			);
		}
		#[cfg(target_os = "windows")]
		if self.focus {
			if let Err(error) = crate::studio::focus(None) {
				crate::wsync_warn!("Code was queued, but Studio could not be focused: {error}");
			}
		}

		Ok(())
	}
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Request {
	code: String,
	focus: bool,
	#[serde(skip_serializing_if = "Option::is_none")]
	expected_project: Option<String>,
	#[serde(skip_serializing_if = "Option::is_none")]
	expected_boot_id: Option<String>,
}
