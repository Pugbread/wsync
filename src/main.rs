use env_logger::WriteStyle;
use log::{debug, error, info};
use puffin_http::Server;
use std::{
	env,
	io::{self, IsTerminal},
	mem::ManuallyDrop,
	process::ExitCode,
};

use wsync::{cli::Cli, config::Config, crash_handler, logger, wsync_error};

const PROFILER_ADDRESS: &str = "localhost:8888";

fn main() -> ExitCode {
	crash_handler::hook();

	// Clap handles help, version and usage errors before configuration or
	// installation has any opportunity to touch disk or the network.
	let cli = Cli::new();
	let config_kind = Config::load();

	let yes = cli.yes();
	let backtrace = cli.backtrace();
	let verbosity = cli.verbosity();
	let log_style = cli.log_style();

	if log_style == WriteStyle::Auto && io::stdin().is_terminal() {
		env::set_var("RUST_LOG_STYLE", "always");
	} else {
		env::set_var(
			"RUST_LOG_STYLE",
			match log_style {
				WriteStyle::Always => "always",
				_ => "never",
			},
		)
	}

	env::set_var("RUST_VERBOSE", verbosity.as_str());
	env::set_var("RUST_YES", if yes { "1" } else { "0" });
	env::set_var("RUST_BACKTRACE", if backtrace { "1" } else { "0" });

	logger::init(verbosity, log_style);

	match config_kind {
		Ok(kind) => info!("{kind:?} config loaded"),
		Err(err) => error!("Failed to load config file: {err}"),
	}

	// Maintenance belongs to explicit install/update commands. Starting it
	// for every command made offline help fail in restricted environments and
	// allowed an optional updater panic to invalidate a successful operation.

	if cfg!(debug_assertions) && cli.profile() {
		match Server::new(PROFILER_ADDRESS) {
			Ok(server) => {
				let _ = ManuallyDrop::new(server);

				info!("Profiler started at {PROFILER_ADDRESS}");
			}
			Err(err) => {
				error!("Failed to start profiler: {err}");
			}
		}

		puffin::set_scopes_on(true);
	}

	let exit_code = match cli.main() {
		Ok(()) => {
			debug!("Successfully executed command!");
			ExitCode::SUCCESS
		}
		Err(err) => {
			wsync_error!("{}", err);
			ExitCode::FAILURE
		}
	};

	exit_code
}
