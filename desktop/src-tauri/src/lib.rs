//! WSync Desktop host (Design 8).
//!
//! The Rust side owns everything the webview must not: the state file, the
//! native folder picker, the sidecar daemons and their owner tokens, and — from
//! a later wave — the OS keychain, the project broker, and the signed updater.
//! The webview's entire privilege list is `capabilities/main.json` plus the
//! commands registered below.

mod broker;
mod commands;
mod daemon;
mod error;
mod loopback;
mod plugin_install;
mod secrets;
mod storage;
mod templates;

use std::sync::Arc;

use broker::Broker;
use daemon::DaemonRegistry;
use storage::AppPaths;
use tauri::Manager as _;

/// The wire protocol this build speaks (Design 5). Surfaced in `app_info` so a
/// plugin/daemon mismatch is visible in the app, not just in logs.
pub(crate) const PROTOCOL: &str = "wsync/1";

pub(crate) struct AppState {
    pub(crate) paths: AppPaths,
    /// Serializes read-modify-write cycles on `state.json`. The file write
    /// itself is atomic; this keeps two concurrent patches from losing one.
    ///
    /// Shared with the broker (`Arc`), which merges Studio-created projects
    /// into the same document from outside the webview's flow (Design 8.4).
    pub(crate) state_lock: Arc<tokio::sync::Mutex<()>>,
    /// Serializes the secret store the same way, across both its backends: a
    /// keychain write and a file write racing would be a coin flip over which
    /// key the daemon eventually gets.
    pub(crate) secrets_lock: tokio::sync::Mutex<()>,
    /// Serializes plugin installs. Two clicks racing would have two temp files
    /// renaming over the same `WSync.rbxm` while Studio watches the directory.
    pub(crate) plugin_lock: tokio::sync::Mutex<()>,
    /// Live daemon children, their owner tokens, and their heartbeat tasks.
    /// The only place a token exists in this process.
    pub(crate) daemons: DaemonRegistry,
    /// The Studio-facing project broker. Listens only while a Projects folder
    /// is authorized (Design 8.4).
    pub(crate) broker: Arc<Broker>,
}

/// The ed25519 public key compiled in at build time, if any.
///
/// Fail closed (Design 8.5): a build without `WSYNC_UPDATER_PUBLIC_KEY` never
/// registers the updater plugin at all, and `app_info.updaterConfigured` is
/// false, so the frontend shows no update affordance. There is no code path
/// that installs an unverified artifact.
pub(crate) fn embedded_updater_public_key() -> Option<&'static str> {
    option_env!("WSYNC_UPDATER_PUBLIC_KEY")
        .map(str::trim)
        .filter(|key| !key.is_empty())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let mut builder = tauri::Builder::default().plugin(tauri_plugin_dialog::init());

    if let Some(public_key) = embedded_updater_public_key() {
        builder = builder.plugin(
            tauri_plugin_updater::Builder::new()
                .pubkey(public_key)
                .build(),
        );
    }

    let app = builder
        .setup(|app| {
            let paths = AppPaths::initialize(app.handle()).map_err(std::io::Error::other)?;
            let state_lock = Arc::new(tokio::sync::Mutex::new(()));
            // The handle is both event sinks: every lifecycle transition
            // reaches the webview as `daemon:up`/`daemon:down`, and every
            // broker transition as `broker:up`/`broker:down`/`project-init`.
            let broker = Broker::new(
                Arc::new(app.handle().clone()),
                paths.state_file.clone(),
                Arc::clone(&state_lock),
            );
            app.manage(AppState {
                paths,
                state_lock,
                secrets_lock: tokio::sync::Mutex::new(()),
                plugin_lock: tokio::sync::Mutex::new(()),
                daemons: DaemonRegistry::new(Arc::new(app.handle().clone())),
                broker,
            });

            // Design 8.4: the broker runs exactly while a Projects folder is
            // authorized — including the folder authorized in an earlier run.
            // Off the setup path, because binding must not delay the window.
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                commands::restore_broker(&handle).await;
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::app_info,
            commands::state_get,
            commands::state_set,
            commands::pick_project_folder,
            commands::daemon_start,
            commands::daemon_stop,
            commands::daemon_status,
            commands::daemon_request,
            commands::daemon_post,
            commands::plugin_install,
            commands::plugin_status,
            commands::plugins_dir_reveal,
            commands::secret_set,
            commands::secret_get,
            commands::secret_clear,
            commands::secret_export_env,
            commands::projects_root_get,
            commands::projects_root_set,
            commands::projects_root_clear,
            commands::projects_root_reveal,
            commands::broker_status,
        ])
        .build(tauri::generate_context!())
        .expect("error while starting WSync Desktop");

    // Design 3.3: the desktop keeps its daemon handles for kill-on-exit. This
    // is the one place that guarantee is made good, so it runs on every exit
    // path the window manager can take — and it is bounded, because a hung
    // daemon must not stop the app from quitting.
    app.run(|handle, event| {
        if matches!(event, tauri::RunEvent::Exit) {
            if let Some(state) = handle.try_state::<AppState>() {
                tauri::async_runtime::block_on(async {
                    // The broker first: it is instant, and a listener that
                    // outlived the window would hold a port against the next run.
                    state.broker.stop("WSync is closing").await;
                    state.daemons.shutdown_all().await;
                });
            }
        }
    });
}
