use anyhow::{bail, Result};
use clap::Parser;
use colored::Colorize;
use log::info;
use std::{path::PathBuf, process};

use crate::{
	config::Config,
	core::Core,
	ext::PathExt,
	program::{Program, ProgramName},
	project::{self, Project},
	sessions, wsync_info,
};

/// Generate JSON sourcemap of the project
#[derive(Parser)]
pub struct Sourcemap {
	/// Project path
	#[arg()]
	project: Option<PathBuf>,

	/// Project path (named alternative to the positional project)
	#[arg(long = "project", value_name = "PATH", conflicts_with = "project")]
	project_flag: Option<PathBuf>,

	/// Session identifier
	#[arg()]
	session: Option<String>,

	/// Output path
	#[arg(short, long)]
	output: Option<PathBuf>,

	/// Regenerate sourcemap every time files change
	#[arg(short, long)]
	watch: bool,

	/// Whether non-script files should be included
	#[arg(short, long)]
	non_scripts: bool,

	/// Run WSync asynchronously
	#[arg(short = 'A', long = "async")]
	run_async: bool,

	/// Spawn the WSync child process (internal)
	#[arg(long, hide = true)]
	wsync_spawn: bool,
}

impl Sourcemap {
	pub fn main(mut self) -> Result<()> {
		let project_path = project::resolve(
			self.project_flag
				.clone()
				.or_else(|| self.project.clone())
				.unwrap_or_default(),
		)?;

		Config::load_workspace(project_path.get_parent());
		let config = Config::new();

		if self.watch && !self.wsync_spawn && (self.run_async || config.run_async) {
			return self.spawn();
		}

		if !project_path.exists() {
			bail!(
				"No project files found in {}",
				project_path.get_parent().to_string().bold()
			);
		}

		if let Some(path) = self.output.as_ref() {
			if config.smart_paths && (path.is_dir() || path.extension().is_none()) {
				let output = if path.get_name().to_lowercase() == "sourcemap" {
					path.with_extension("json")
				} else {
					path.join("sourcemap.json")
				};

				self.output = Some(output);
			}
		}

		let mut project = Project::load(&project_path)?;

		// Scope governs sync, never packaging (see the same note in build.rs):
		// tooling introspection must see the whole projection
		project.scope = Some(String::from("full"));

		let core = Core::new(project, self.watch)?;

		core.sourcemap(self.output.clone(), self.non_scripts)?;

		if let Some(output) = &self.output {
			wsync_info!(
				"Generated sourcemap of project: {} at: {}",
				project_path.to_string().bold(),
				output.resolve()?.to_string().bold()
			);
		}

		if self.watch {
			sessions::add(self.session, None, None, process::id(), config.run_async)?;

			if self.output.is_some() {
				wsync_info!("Watching for changes..");
			}

			let queue = core.queue();
			queue.subscribe_internal().unwrap();

			loop {
				let _message = queue.get_change(0).unwrap();

				info!("Regenerating sourcemap..");
				core.sourcemap(self.output.clone(), self.non_scripts)?;
			}
		}

		Ok(())
	}

	fn spawn(self) -> Result<()> {
		Program::new(ProgramName::Wsync).args(self.spawn_args()).spawn()?;
		Ok(())
	}

	fn spawn_args(self) -> Vec<String> {
		let mut args = vec![String::from("sourcemap")];

		if let Some(project) = self.project_flag.or(self.project) {
			args.push(project.to_string())
		}

		if let Some(session) = self.session {
			args.push(session);
		}

		if let Some(output) = self.output {
			args.push("--output".into());
			args.push(output.to_string())
		}

		if self.watch {
			args.push("--watch".into())
		}

		if self.non_scripts {
			args.push("--non-scripts".into())
		}

		args
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn named_project_survives_child_arguments() {
		let command = Sourcemap::try_parse_from(["sourcemap", "--project", "a project/default.project.json"]).unwrap();
		let child = Sourcemap::try_parse_from(command.spawn_args()).unwrap();
		assert_eq!(child.project, Some(PathBuf::from("a project/default.project.json")));
	}

	#[test]
	fn legacy_project_and_session_remain_supported() {
		let command = Sourcemap::try_parse_from(["sourcemap", "place.json", "my-session"]).unwrap();
		let child = Sourcemap::try_parse_from(command.spawn_args()).unwrap();
		assert_eq!(child.project, Some(PathBuf::from("place.json")));
		assert_eq!(child.session.as_deref(), Some("my-session"));
		assert!(Sourcemap::try_parse_from(["sourcemap", "place.json", "--project", "other.json"]).is_err());
	}
}
