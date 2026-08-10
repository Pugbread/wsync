use anyhow::{bail, Context, Result};
use clap::Parser;
use colored::Colorize;
use serde_json::Value;
use std::{
	io::{self, Write},
	path::PathBuf,
};
use tungstenite::{connect, Message};
use uuid::Uuid;

use crate::{
	config::Config,
	constants::PROTOCOL_VERSION,
	daemon,
	ext::PathExt,
	project::{self, Project},
	wsync_info, wsync_warn,
};

/// Watch the project daemon's live event feed over WebSocket
#[derive(Parser)]
pub struct Watch {
	/// Project path
	#[arg()]
	project: Option<PathBuf>,

	/// Daemon port (defaults to the runtime record, then the project/config
	/// port)
	#[arg(short = 'P', long)]
	port: Option<u16>,

	/// Print one-line summaries instead of full frames
	#[arg(long)]
	compact: bool,

	/// Print frames as NDJSON with no status banner
	#[arg(long)]
	raw: bool,
}

impl Watch {
	pub fn main(self) -> Result<()> {
		let port = match self.port {
			Some(port) => port,
			None => self.discover_port()?,
		};

		let url = format!("ws://localhost:{port}/ws");
		let (mut socket, _) = connect(&url).with_context(|| format!("Failed to connect to {url}"))?;

		let hello = serde_json::json!({
			"type": "hello",
			"clientId": format!("cli-watch-{}", Uuid::new_v4()),
			"role": "watch",
			"protocol": PROTOCOL_VERSION,
			"name": "wsync watch",
		});

		socket.send(Message::Text(hello.to_string()))?;

		if !self.raw {
			wsync_info!("Watching daemon on port {} (Ctrl+C to stop)", port.to_string().bold());
		}

		loop {
			let message = match socket.read() {
				Ok(message) => message,
				Err(tungstenite::Error::ConnectionClosed) => break,
				Err(err) => bail!("Connection lost: {err}"),
			};

			let text = match message {
				Message::Text(text) => text,
				Message::Close(_) => break,
				// Protocol pings are answered by tungstenite automatically
				_ => continue,
			};

			let Ok(frame) = serde_json::from_str::<Value>(&text) else {
				continue;
			};

			match frame.get("type").and_then(Value::as_str) {
				// Answer heartbeats and keep them out of the feed
				Some("ping") => {
					socket.send(Message::Text(r#"{"type":"pong"}"#.into())).ok();
				}
				Some("pong") => {}
				Some("shutdown") => {
					self.print_frame(&frame, &text);

					if !self.raw {
						wsync_warn!(
							"Daemon closed the connection: {}",
							frame.get("reason").and_then(Value::as_str).unwrap_or("unknown reason")
						);
					}

					break;
				}
				_ => self.print_frame(&frame, &text),
			}
		}

		Ok(())
	}

	/// Finds the daemon port: runtime record first (the authoritative
	/// registry), then the project file, then the config default
	fn discover_port(&self) -> Result<u16> {
		let project_path = project::resolve(self.project.clone().unwrap_or_default())?;

		if project_path.exists() {
			if let Ok(canonical) = daemon::canonical_project(&project_path) {
				let state_dir = daemon::state_dir(None)?;
				let paths = daemon::runtime_paths(&state_dir, &canonical);

				if let Ok(Some(record)) = daemon::read_record(&paths.record) {
					return Ok(record.port);
				}
			}

			Config::load_workspace(project_path.get_parent());

			if let Ok(project) = Project::load(&project_path) {
				if let Some(port) = project.port {
					return Ok(port);
				}
			}
		}

		Ok(Config::new().port)
	}

	fn print_frame(&self, frame: &Value, text: &str) {
		if self.compact {
			println!("{}", summarize(frame));
		} else {
			// NDJSON: one frame per line, exactly as received
			println!("{text}");
		}

		// This is a live feed usually consumed through a pipe; Rust block
		// buffers stdout there, so every line must flush immediately
		io::stdout().flush().ok();
	}
}

/// One-line human summary of a protocol frame
fn summarize(frame: &Value) -> String {
	let kind = frame.get("type").and_then(Value::as_str).unwrap_or("?");

	match kind {
		"hello" => format!(
			"hello  {} v{} (gameId {})",
			frame.get("name").and_then(Value::as_str).unwrap_or("?"),
			frame.get("version").and_then(Value::as_str).unwrap_or("?"),
			frame.get("gameId").map(|id| id.to_string()).unwrap_or("none".into()),
		),
		"event" => match frame.get("topic").and_then(Value::as_str) {
			Some("sync-activity") => {
				let count = |key: &str| frame.get(key).and_then(Value::as_u64).unwrap_or(0);
				let names = frame
					.get("names")
					.and_then(Value::as_array)
					.map(|names| names.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", "))
					.unwrap_or_default();

				format!(
					"sync   {} +{} ~{} -{}  {}",
					frame.get("direction").and_then(Value::as_str).unwrap_or("?"),
					count("additions"),
					count("updates"),
					count("removals"),
					names
				)
			}
			Some("plugin-status") => format!(
				"plugin {} ({})",
				if frame.get("connected").and_then(Value::as_bool).unwrap_or(false) {
					"connected"
				} else {
					"disconnected"
				},
				frame.get("name").and_then(Value::as_str).unwrap_or("?"),
			),
			Some("daemon") => format!("daemon {}", frame.get("note").and_then(Value::as_str).unwrap_or("")),
			topic => format!("event  {}", topic.unwrap_or("?")),
		},
		"shutdown" => format!(
			"close  {} ({})",
			frame.get("reason").and_then(Value::as_str).unwrap_or("?"),
			frame.get("code").and_then(Value::as_str).unwrap_or("?"),
		),
		other => other.to_owned(),
	}
}
