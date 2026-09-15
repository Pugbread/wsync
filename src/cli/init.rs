use anyhow::{Context, Result};
use clap::{ArgAction, Parser};
use colored::Colorize;
use std::path::PathBuf;

use crate::{
	config::Config,
	ext::PathExt,
	logger, project, stats,
	workspace::{self, WorkspaceConfig, WorkspaceLicense},
	wsync_error, wsync_info,
};

/// Initialize a new WSync project
#[derive(Parser)]
pub struct Init {
	/// Project path
	#[arg()]
	project: Option<PathBuf>,

	/// Project path (named alternative to the positional project)
	#[arg(long = "project", value_name = "PATH", conflicts_with = "project")]
	project_flag: Option<PathBuf>,

	/// Workspace template
	#[arg(short = 'T', long)]
	template: Option<String>,

	/// Workspace license
	#[arg(short, long)]
	license: Option<String>,

	/// Configure Git
	#[arg(
		short,
        long,
        default_missing_value("true"),
		hide_possible_values = true,
        num_args(0..=1),
    	action = ArgAction::Set,
    )]
	git: Option<bool>,

	/// Setup Wally
	#[arg(
		short,
        long,
        default_missing_value("true"),
		hide_possible_values = true,
        num_args(0..=1),
    	action = ArgAction::Set,
    )]
	wally: Option<bool>,

	/// Setup selene
	#[arg(
		short,
        long,
        default_missing_value("true"),
		hide_possible_values = true,
        num_args(0..=1),
    	action = ArgAction::Set,
    )]
	selene: Option<bool>,

	/// Include docs (README, CHANGELOG, etc.)
	#[arg(
		short,
        long,
        default_missing_value("true"),
		hide_possible_values = true,
        num_args(0..=1),
    	action = ArgAction::Set,
    )]
	docs: Option<bool>,

	/// Initialize using roblox-ts
	#[arg(
		short,
        long,
        default_missing_value("true"),
		hide_possible_values = true,
        num_args(0..=1),
    	action = ArgAction::Set,
    )]
	ts: Option<bool>,
}

impl Init {
	pub fn main(self) -> Result<()> {
		// Templates are needed by project creation, not by every CLI command.
		crate::installer::install_templates(false)?;
		let project = self.project_flag.or(self.project).unwrap_or_default();
		let project_path = project::resolve(project.clone())?;

		Config::load_workspace(project_path.get_parent());
		let config = Config::new();

		let template = self.template.unwrap_or(config.template.clone());
		let git = self.git.unwrap_or(config.use_git);
		let wally = self.wally.unwrap_or(config.use_wally);
		let selene = self.selene.unwrap_or(config.use_selene);
		let docs = self.docs.unwrap_or(config.include_docs);
		let ts = self.ts.unwrap_or(config.ts_mode);

		let license = WorkspaceLicense {
			force: self.license.is_some(),
			inner: &self.license.unwrap_or(config.license.clone()),
		};

		let mut workspace_config = WorkspaceConfig {
			project: &project,
			template: &template,
			license,
			git,
			wally,
			selene,
			docs,
			rojo_mode: config.rojo_mode,
			use_lua: config.lua_extension,
		};
		drop(config);

		if ts {
			if let Some(path) = workspace::init_ts(workspace_config)? {
				let path = path.resolve()?.join("default.project.json");
				crate::cli::refresh::refresh_workspace(&path)
					.context("Project initialized, but agent documents could not be generated; run `wsync refresh --project <path>` to finish")?;

				wsync_info!(
					"Successfully initialized roblox-ts project: {}",
					path.to_string().bold()
				);
			}

			return Ok(());
		}

		if project_path.exists() {
			wsync_error!("Project {} already exists!", project_path.to_string().bold());

			if !logger::prompt("Would you like to continue and add potentially missing files?", false) {
				return Ok(());
			}
		}

		workspace_config.project = &project_path;
		workspace::init(workspace_config)?;
		crate::cli::refresh::refresh_workspace(&project_path)
			.context("Project initialized, but agent documents could not be generated; run `wsync refresh --project <path>` to finish")?;

		wsync_info!("Successfully initialized project: {}", project_path.to_string().bold());

		stats::projects_created(1);

		Ok(())
	}
}
