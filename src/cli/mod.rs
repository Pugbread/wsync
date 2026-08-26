use anyhow::Result;
use clap::{ColorChoice, Parser, Subcommand};
use clap_verbosity_flag::Verbosity;
use env_logger::fmt::WriteStyle;
use log::LevelFilter;
use std::env;

use crate::util;

mod auth;
mod build;
mod client;
mod cloud;
mod config;
mod daemon;
mod debug;
mod doc;
mod exec;
mod init;
mod install;
mod lint;
mod live;
mod monetization;
mod plan;
mod plugin;
mod registry;
mod registry_bundle;
mod repair;
mod upload;
// The server's plugin-connect sites call the auto-refresh hook here
pub(crate) mod refresh;
mod serve;
mod sourcemap;
mod stop;
mod studio;
mod update;
mod watch;
mod workflow;

macro_rules! about {
	() => {
		concat!("WSync ", env!("CARGO_PKG_VERSION"))
	};
}

macro_rules! long_about {
	() => {
		concat!(
			"WSync ",
			env!("CARGO_PKG_VERSION"),
			"\n",
			env!("CARGO_PKG_DESCRIPTION"),
			"\n",
			"Made with <3 by ",
			env!("CARGO_PKG_AUTHORS")
		)
	};
}

#[derive(Parser)]
#[clap(about = about!(), long_about = long_about!(), version)]
pub struct Cli {
	#[command(subcommand)]
	command: Commands,

	#[command(flatten)]
	verbose: Verbosity,

	/// Automatically answer to any prompts
	#[arg(short, long, global = true)]
	yes: bool,

	/// Print full backtrace on panic
	#[arg(short = 'B', long, global = true)]
	backtrace: bool,

	#[arg(long, hide = true, global = true)]
	profile: bool,

	/// Output coloring: auto, always, never
	#[arg(
		long,
		short = 'C',
		global = true,
		value_name = "WHEN",
		default_value = "auto",
		hide_default_value = true,
		hide_possible_values = true
	)]
	pub color: ColorChoice,
}

impl Cli {
	pub fn new() -> Cli {
		Cli::parse()
	}

	pub fn profile(&self) -> bool {
		self.profile
	}

	pub fn yes(&self) -> bool {
		if env::var("RUST_YES").is_ok() {
			return util::env_yes();
		}

		self.yes
	}

	pub fn backtrace(&self) -> bool {
		if env::var("RUST_BACKTRACE").is_ok() {
			return util::env_backtrace();
		}

		self.backtrace
	}

	pub fn verbosity(&self) -> LevelFilter {
		if env::var("RUST_VERBOSE").is_ok() {
			return util::env_verbosity();
		}

		self.verbose.log_level_filter()
	}

	pub fn log_style(&self) -> WriteStyle {
		if env::var("RUST_LOG_STYLE").is_ok() {
			return util::env_log_style();
		}

		match self.color {
			ColorChoice::Always => WriteStyle::Always,
			ColorChoice::Never => WriteStyle::Never,
			_ => WriteStyle::Auto,
		}
	}

	pub fn main(self) -> Result<()> {
		match self.command {
			Commands::Init(command) => command.main(),
			Commands::Serve(command) => command.main(),
			Commands::Daemon(command) => command.main(),
			Commands::Watch(command) => command.main(),
			Commands::Build(command) => command.main(),
			Commands::Sourcemap(command) => command.main(),
			Commands::Stop(command) => command.main(),
			Commands::Studio(command) => command.main(),
			Commands::Debug(command) => command.main(),
			Commands::Exec(command) => command.main(),
			Commands::Update(command) => command.main(),
			Commands::Install(command) => command.main(),
			Commands::Plugin(command) => command.main(),
			Commands::Config(command) => command.main(),
			Commands::Doc(command) => command.main(),

			// Command registry
			Commands::Commands(command) => command.main(),
			Commands::Context(command) => command.main(),

			// Live diagnostics
			Commands::Ping(command) => command.main(),
			Commands::Version(command) => command.main(),
			Commands::Status(command) => command.main(),
			Commands::Doctor(command) => command.main(),
			Commands::Capabilities(command) => command.main(),

			// Live inspection
			Commands::Get(command) => command.main(),
			Commands::Ls(command) => command.main(),
			Commands::Tree(command) => command.main(),
			Commands::Props(command) => command.main(),
			Commands::Query(command) => command.main(),
			Commands::Find(command) => command.main(),
			Commands::FindAttr(command) => command.main(),
			Commands::Classinfo(command) => command.main(),
			Commands::Enums(command) => command.main(),
			Commands::Enum(command) => command.main(),
			Commands::Select(command) => command.main(),
			Commands::Logs(command) => command.main(),
			Commands::Tail(command) => command.main(),
			Commands::Source(command) => command.main(),

			// Path tools
			Commands::Path(command) => command.main(),
			Commands::Meta(command) => command.main(),
			Commands::Where(command) => command.main(),

			// Conflict resolution

			// Live writes
			Commands::Set(command) => command.main(),
			Commands::New(command) => command.main(),
			Commands::Rm(command) => command.main(),
			Commands::Mv(command) => command.main(),
			Commands::Attr(command) => command.main(),
			Commands::Tag(command) => command.main(),
			Commands::Call(command) => command.main(),
			Commands::Eval(command) => command.main(),
			Commands::Save(command) => command.main(),
			Commands::Waypoint(command) => command.main(),
			Commands::Undo(command) => command.main(),
			Commands::Redo(command) => command.main(),

			// Studio clipboard & agent runtime
			Commands::Copy(command) => command.main(),
			Commands::Paste(command) => command.main(),
			Commands::Capture(command) => command.main(),
			Commands::Playtest(command) => command.main(),
			Commands::Run(command) => command.main(),

			// Command registry & project docs
			Commands::Plan(command) => command.main(),
			Commands::Refresh(command) => command.main(),

			// Project setup
			Commands::Auth(command) => command.main(),

			// Live inspection & Studio control additions
			Commands::Snapshot(command) => command.main(),
			Commands::Backlog(command) => command.main(),
			Commands::Services(command) => command.main(),
			Commands::Open(command) => command.main(),

			// Live diagnostics & maintenance
			Commands::Lint(command) => command.main(),
			Commands::Repair(command) => command.main(),

			// Open Cloud
			Commands::Upload(command) => command.main(),
			Commands::Monetization(command) => command.main(),

			// Studio control
			Commands::Transmit(command) => command.main(),
		}
	}
}

#[derive(Subcommand)]
pub enum Commands {
	Init(init::Init),
	Serve(serve::Serve),
	Daemon(daemon::Daemon),
	Watch(watch::Watch),
	Build(build::Build),
	Sourcemap(sourcemap::Sourcemap),
	Stop(stop::Stop),
	Studio(studio::Studio),
	Debug(debug::Debug),
	Exec(exec::Exec),
	Update(update::Update),
	Install(install::Install),
	Plugin(plugin::Plugin),
	Config(config::Config),
	Doc(doc::Doc),

	Commands(registry::Commands),
	Context(registry::Context),

	Ping(live::Ping),
	Version(live::Version),
	Status(live::Status),
	Doctor(live::Doctor),
	Capabilities(live::Capabilities),

	Get(live::Get),
	Ls(live::Ls),
	Tree(live::Tree),
	Props(live::Props),
	Query(live::Query),
	Find(live::Find),
	FindAttr(live::FindAttr),
	// Spelled as one word: the registry entry is `classinfo`, not
	// `class-info`, and clap derives the command name from this variant
	Classinfo(live::ClassInfo),
	Enums(live::Enums),
	Enum(live::Enum),
	Select(live::Select),
	Logs(live::Logs),
	Tail(live::Tail),
	Source(live::Source),

	Path(live::Path),
	Meta(live::Meta),
	Where(live::Where),

	Set(live::Set),
	New(live::New),
	Rm(live::Rm),
	Mv(live::Mv),
	Attr(live::Attr),
	Tag(live::Tag),
	Call(live::Call),
	Eval(live::Eval),
	Save(live::Save),
	Waypoint(live::Waypoint),
	Undo(live::Undo),
	Redo(live::Redo),

	Copy(live::Copy),
	Paste(live::Paste),
	Capture(live::Capture),
	Playtest(live::Playtest),
	Run(workflow::Run),

	Plan(plan::Plan),
	Refresh(refresh::Refresh),

	Auth(auth::Auth),

	Snapshot(live::Snapshot),
	Backlog(live::Backlog),
	Services(live::Services),
	Open(live::Open),

	Lint(lint::Lint),
	Repair(repair::Repair),

	Upload(upload::Upload),
	Monetization(monetization::Monetization),

	Transmit(live::Transmit),
}
