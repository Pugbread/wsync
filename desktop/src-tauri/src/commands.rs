//! The complete privileged surface the WSync webview can reach (Design 8.1).
//!
//! Nothing else is exposed: no shell, no filesystem plugin, no HTTP plugin. If
//! a view needs a capability, it gets a named command here with a validated,
//! typed signature — not a general-purpose escape hatch.
//!
//! Real as of wave 5: app metadata, persisted state, the native folder picker,
//! the whole daemon lifecycle, both halves of the data layer —
//! `daemon_request` (allowlisted GET) and `daemon_post` (allowlisted POST) —
//! the secret store (OS keychain with a `0600` file fallback), and the Studio
//! plugin install with its build-manifest verification. One typed stub is
//! left, returning `not_implemented` rather than a plausible lie:
//! `secret_export_env`'s daemon handoff.

use std::path::PathBuf;

use serde::Serialize;
use serde_json::{Map, Value};
use tauri::{AppHandle, Manager as _, State};
use tauri_plugin_dialog::DialogExt as _;

use crate::{
    broker::BrokerStatus,
    daemon::{DaemonResponse, DaemonSession, DaemonStatus},
    error::{HostError, HostResult},
    plugin_install,
    secrets::{self, SecretStatus},
    storage, AppState,
};

// ---------------------------------------------------------------- app info --

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AppInfo {
    name: &'static str,
    version: &'static str,
    protocol: &'static str,
    target: &'static str,
    platform: &'static str,
    /// False when no ed25519 public key was compiled in. The frontend hides all
    /// update affordances on false — fail closed, per Design 8.5.
    updater_configured: bool,
    data_dir: String,
    state_file: String,
    secrets_file: String,
    daemons_dir: String,
}

#[tauri::command]
pub(crate) fn app_info(state: State<'_, AppState>) -> AppInfo {
    let paths = &state.paths;
    AppInfo {
        name: "WSync",
        version: env!("CARGO_PKG_VERSION"),
        protocol: crate::PROTOCOL,
        target: env!("WSYNC_TARGET_TRIPLE"),
        platform: platform_name(),
        updater_configured: crate::embedded_updater_public_key().is_some(),
        data_dir: paths.data_dir.display().to_string(),
        state_file: paths.state_file.display().to_string(),
        secrets_file: paths.secrets_file.display().to_string(),
        daemons_dir: paths.daemons_dir.display().to_string(),
    }
}

const fn platform_name() -> &'static str {
    if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else {
        "unknown"
    }
}

// ------------------------------------------------------------ app state 8.3 --

/// Read the persisted registry. Omit `key` for the whole document — that is
/// what the frontend does once at boot; per-key reads exist for cheap polls.
#[tauri::command]
pub(crate) async fn state_get(
    state: State<'_, AppState>,
    key: Option<String>,
) -> HostResult<Value> {
    let _guard = state.state_lock.lock().await;
    storage::read_value(&state.paths.state_file, key.as_deref())
}

/// Merge a patch of allowlisted keys and replace `state.json` atomically.
/// Returns the document as it now stands, so the caller never has to guess.
#[tauri::command]
pub(crate) async fn state_set(
    state: State<'_, AppState>,
    patch: Map<String, Value>,
) -> HostResult<Value> {
    let _guard = state.state_lock.lock().await;
    storage::merge_document(&state.paths.state_file, patch)
}

// ------------------------------------------------------------ folder picker --

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PickedFolder {
    path: String,
    name: String,
}

/// The only way a filesystem path enters the app on Desktop (Design 11): the
/// user picks it in a native dialog. No view may supply a path directly.
#[tauri::command]
pub(crate) async fn pick_project_folder(app: AppHandle) -> HostResult<PickedFolder> {
    let path = pick_folder(&app, "Choose a WSync project folder").await?;
    let name = folder_name(&path);
    Ok(PickedFolder {
        path: path.display().to_string(),
        name,
    })
}

/// Open the native picker and hand back a canonical directory.
///
/// Canonicalizing here is load-bearing rather than tidy: a symlinked pick would
/// otherwise let a later write resolve somewhere the user never authorized, and
/// the broker's "one direct child of the root" rule is a prefix comparison that
/// only means anything against a physical path (Design 11).
async fn pick_folder(app: &AppHandle, title: &str) -> HostResult<PathBuf> {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    app.dialog()
        .file()
        .set_title(title)
        .pick_folder(move |picked| {
            let _ = sender.send(picked);
        });

    let picked = receiver
        .await
        .map_err(|_| HostError::io("the folder picker closed without answering"))?
        .ok_or_else(|| HostError::cancelled("no folder was chosen"))?;

    let path = picked
        .into_path()
        .map_err(|error| HostError::invalid_argument(format!("unusable folder: {error}")))?;

    let path: PathBuf = std::fs::canonicalize(&path).map_err(|error| {
        HostError::io(format!("could not resolve {}: {error}", path.display()))
    })?;
    if !path.is_dir() {
        return Err(HostError::invalid_argument(format!(
            "{} is not a folder",
            path.display()
        )));
    }
    Ok(path)
}

fn folder_name(path: &std::path::Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

// ------------------------------------------------- projects folder + broker --
//
// Design 8.4: the broker listens **only while a Projects folder is authorized**.
// Authorization is therefore not a preference the webview writes into state —
// it is a host operation with a lifecycle attached, which is why these four
// commands exist instead of a `projectsRoot` key the settings view could set.
//
// The root is persisted here as well as returned, so a folder authorized in one
// run is still authorized in the next even if the webview never asks.

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProjectsRoot {
    /// The authorized folder, or `None` when Studio cannot create projects.
    root: Option<String>,
    name: Option<String>,
    broker: BrokerStatus,
}

async fn projects_root_answer(state: &AppState, root: Option<String>) -> ProjectsRoot {
    ProjectsRoot {
        name: root.as_deref().map(|root| folder_name(std::path::Path::new(root))),
        root,
        broker: state.broker.status().await,
    }
}

/// What is authorized right now, and whether the broker is up on it.
#[tauri::command]
pub(crate) async fn projects_root_get(state: State<'_, AppState>) -> HostResult<ProjectsRoot> {
    let root = read_projects_root(&state).await;
    Ok(projects_root_answer(&state, root).await)
}

/// Authorize a folder through the native picker and start the broker on it.
///
/// Takes no path argument on purpose (Design 11): the webview cannot name the
/// folder Studio will write into, any more than it can name a project's path.
///
/// A picker that succeeds but a broker that cannot bind is **not** an error
/// here: the folder is genuinely authorized, and the honest report is an
/// authorized root with a broker that says why it is off.
#[tauri::command]
pub(crate) async fn projects_root_set(
    app: AppHandle,
    state: State<'_, AppState>,
) -> HostResult<ProjectsRoot> {
    let path = pick_folder(&app, "Choose the folder Studio may create projects in").await?;
    let root = path.display().to_string();

    write_projects_root(&state, Some(root.clone())).await?;
    if let Err(error) = state.broker.start(&path).await {
        return Ok(ProjectsRoot {
            name: Some(folder_name(&path)),
            root: Some(root),
            broker: BrokerStatus::stopped_because(error.message),
        });
    }
    Ok(projects_root_answer(&state, Some(root)).await)
}

/// Withdraw authorization: the broker stops and the port is released.
#[tauri::command]
pub(crate) async fn projects_root_clear(state: State<'_, AppState>) -> HostResult<ProjectsRoot> {
    write_projects_root(&state, None).await?;
    // The reason becomes the Settings status line, so it says what to do next
    // rather than what just happened.
    state.broker.stop("authorize a folder").await;
    Ok(projects_root_answer(&state, None).await)
}

/// Reveal the authorized folder in the OS file manager.
///
/// Deliberately argument-free: it can only ever open the folder the user
/// authorized through the picker, so there is no path for a view to supply and
/// nothing to validate beyond "is one authorized".
#[tauri::command]
pub(crate) async fn projects_root_reveal(state: State<'_, AppState>) -> HostResult<()> {
    let root = read_projects_root(&state)
        .await
        .ok_or_else(|| HostError::unavailable("no projects folder is authorized"))?;
    let path = PathBuf::from(&root);
    if !path.is_dir() {
        return Err(HostError::io(format!("{root} is no longer a folder")));
    }
    reveal(&path)
}

/// Hand a directory to the OS file manager.
///
/// Shared by the two argument-free reveals, and deliberately not generalized
/// beyond them: both callers name a directory the host itself resolved, so
/// there is no webview-supplied path anywhere near this.
fn reveal(path: &std::path::Path) -> HostResult<()> {
    let (program, args): (&str, Vec<&std::ffi::OsStr>) = if cfg!(target_os = "macos") {
        ("open", vec![path.as_os_str()])
    } else if cfg!(target_os = "windows") {
        ("explorer", vec![path.as_os_str()])
    } else {
        ("xdg-open", vec![path.as_os_str()])
    };
    // No shell: the path is an argument, not a string to be interpreted.
    std::process::Command::new(program)
        .args(args)
        .spawn()
        .map_err(|error| {
            HostError::io(format!("could not open {}: {error}", path.display()))
        })?;
    Ok(())
}

/// `{running, port?, root?}` — polled by Settings and seeded at boot.
#[tauri::command]
pub(crate) async fn broker_status(state: State<'_, AppState>) -> HostResult<BrokerStatus> {
    Ok(state.broker.status().await)
}

/// Bring the broker up for a folder authorized in an earlier run (boot path).
///
/// Quiet by design: a folder that has since been deleted or moved is a state to
/// report in Settings, not a reason to fail the launch. The `broker:up` event
/// is what tells the frontend it worked.
pub(crate) async fn restore_broker(app: &AppHandle) {
    let Some(state) = app.try_state::<AppState>() else {
        return;
    };
    let Some(root) = read_projects_root(&state).await else {
        return;
    };
    let _ = state.broker.start(std::path::Path::new(&root)).await;
}

async fn read_projects_root(state: &AppState) -> Option<String> {
    let _guard = state.state_lock.lock().await;
    storage::read_value(&state.paths.state_file, Some("projectsRoot"))
        .ok()?
        .as_str()
        .map(str::to_string)
        .filter(|root| !root.trim().is_empty())
}

async fn write_projects_root(state: &AppState, root: Option<String>) -> HostResult<()> {
    let _guard = state.state_lock.lock().await;
    let mut patch = Map::new();
    patch.insert(
        "projectsRoot".into(),
        root.map(Value::String).unwrap_or(Value::Null),
    );
    storage::merge_document(&state.paths.state_file, patch).map(|_| ())
}

// --------------------------------------------------------- daemon lifecycle --

/// Spawn (or adopt) the project's daemon and start serving it.
///
/// The child is the daemon (Design 3.3): the handle stays in the registry for
/// kill-on-exit, the owner token never leaves the host, and the returned
/// session is exactly what the frontend persists under `daemonSessions`.
///
/// An `alreadyRunning` answer means a matching daemon existed; the session is
/// still returned so the app can drive it, with `managed: false` to say the app
/// holds no handle.
#[tauri::command]
pub(crate) async fn daemon_start(
    state: State<'_, AppState>,
    project_id: String,
    project_path: String,
    port: Option<u16>,
) -> HostResult<DaemonSession> {
    require_non_empty("projectId", &project_id)?;
    require_non_empty("projectPath", &project_path)?;
    if port == Some(0) {
        return Err(HostError::invalid_argument("port 0 is not a daemon port"));
    }

    state
        .daemons
        .start(&project_id, &project_path, &state.paths.data_dir, port)
        .await
}

/// Authenticated stop: `/stop` with this session's exact boot id and owner
/// token (Design 11), a short grace period, and only then the held handle as a
/// fallback. A daemon we merely adopted is asked, never killed.
#[tauri::command]
pub(crate) async fn daemon_stop(
    state: State<'_, AppState>,
    project_id: String,
) -> HostResult<String> {
    require_non_empty("projectId", &project_id)?;
    let report = state.daemons.stop(&project_id).await?;
    Ok(report.outcome.as_str().to_string())
}

/// Deliberately succeeds rather than erroring: the Projects view polls this,
/// and a poll that throws every tick would be noise rather than information.
/// The answer is the registry's record reconciled against a live `/hello`.
#[tauri::command]
pub(crate) async fn daemon_status(
    state: State<'_, AppState>,
    project_id: String,
) -> HostResult<DaemonStatus> {
    require_non_empty("projectId", &project_id)?;
    Ok(state.daemons.status(&project_id).await)
}

/// Read one allowlisted route on a served project's daemon (Design 5.2).
///
/// Not a proxy and not an escape hatch: GET only, an exact-path allowlist
/// (`/hello`, `/resolve`, the `/choice*` reads and Design 7.0's `/review*`
/// reads), and the target is always the loopback daemon of a project this host
/// is tracking. It exists so the owner token can stay in the host while the
/// webview's data layer still reaches the engine.
///
/// Non-2xx is returned, not thrown: the conflicts poller needs to see a 404 to
/// recognize an engine that predates the conflict routes.
#[tauri::command]
pub(crate) async fn daemon_request(
    state: State<'_, AppState>,
    project_id: String,
    route: String,
) -> HostResult<DaemonResponse> {
    require_non_empty("projectId", &project_id)?;
    require_non_empty("route", &route)?;
    state.daemons.request(&project_id, &route).await
}

/// Write one allowlisted route on a served project's daemon (Design 5.2).
///
/// Only the routes that carry a *decision*: `POST /resolve` (keep local or keep
/// Studio for one parked conflict), `POST /choice` (the divergence answer),
/// `POST /choice/selection` (the chunked selective-pull id list), and Design
/// 7.0's `POST /review/push` / `POST /review/dismiss` (the passive disk review's
/// two answers). Nothing else — `/push`, `/pull/stream`, `/compare`,
/// `/projects/init` and `/stop` are deliberately unreachable from the webview,
/// and the exact-path model means adding one is a change here rather than a
/// looser pattern.
///
/// The body is a JSON object with a per-route size cap (`daemon.rs`), checked
/// before a socket is opened. Non-2xx comes back as data, not as an error: 409
/// is how the divergence flow learns the choice was resolved elsewhere, and 404
/// is how a conflict card learns its id is already gone.
#[tauri::command]
pub(crate) async fn daemon_post(
    state: State<'_, AppState>,
    project_id: String,
    route: String,
    body: Value,
) -> HostResult<DaemonResponse> {
    require_non_empty("projectId", &project_id)?;
    require_non_empty("route", &route)?;
    state.daemons.post(&project_id, &route, body).await
}

// ----------------------------------------------------------- studio plugin --
//
// Design 8.2, Appendix A `plugin`: install `WSync.rbxm` into the OS Roblox
// plugins directory, remove stale copies, and report whether its protocol
// matches the running daemon's.
//
// The verification rules and the resolution order live in `plugin_install.rs`;
// what belongs here is the boundary — which arguments the webview may send
// (one boolean, and a project id it already holds), and the fact that the
// picker only opens on an explicit second click rather than as a surprise
// consolation prize when automatic resolution fails.

/// Install the Studio plugin.
///
/// `pick` opens the native file dialog instead of resolving automatically. It
/// is what the "Choose WSync.rbxm…" affordance calls after an install failed
/// to find one — never the first thing a click does, because a native dialog
/// nobody asked for is worse than an error that says how to build the artifact.
///
/// A dev-tree artifact that is older than the plugin sources is rebuilt before
/// it is installed (`plugin_install::rebuild_dev_tree`), so the button always
/// installs what the checkout currently says — never a forgotten build.
#[tauri::command]
pub(crate) async fn plugin_install(
    app: AppHandle,
    state: State<'_, AppState>,
    pick: Option<bool>,
) -> HostResult<plugin_install::InstallReport> {
    let _guard = state.plugin_lock.lock().await;

    // Ask where it would go before asking what to put there: a platform with no
    // Studio has nowhere to install to, and finding that out *after* opening a
    // file picker would be a rude way to learn it.
    let directory = plugin_install::plugins_dir()?;

    let artifact = if pick.unwrap_or(false) {
        plugin_install::Artifact {
            path: pick_plugin_artifact(&app).await?,
            origin: plugin_install::Origin::Picked,
        }
    } else {
        plugin_install::resolve(&plugin_sources(&app))?
    };

    // Hashing, copying and — for a stale checkout — the rebuild are blocking
    // work; none of it belongs on the async runtime's threads.
    blocking(move || {
        let mut rebuilt: Option<String> = None;

        if artifact.origin == plugin_install::Origin::DevTree {
            if let (Some(stale), Some(plugin_dir)) = (
                plugin_install::dev_tree_stale(&artifact.path),
                artifact.path.parent(),
            ) {
                plugin_install::rebuild_dev_tree(plugin_dir)?;
                rebuilt = Some(
                    stale
                        .source
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_else(|| stale.source.display().to_string()),
                );
            }
        }

        let mut report = plugin_install::install(&artifact, &directory)?;

        if let Some(changed) = rebuilt {
            let rebuilt_note =
                format!("Rebuilt from the checkout first — {changed} had changed since the last build.");
            report.note = Some(match report.note.take() {
                Some(existing) => format!("{rebuilt_note} {existing}"),
                None => rebuilt_note,
            });
        }

        Ok(report)
    })
    .await
}

/// What is installed, whether it is this build's artifact, and — when
/// `project_id` names a served project — whether the daemon agrees about the
/// protocol.
///
/// Succeeds rather than throwing, for the reason `daemon_status` gives: this is
/// what a settings panel renders, and a status line that throws every time the
/// plugin is absent is noise rather than information.
#[tauri::command]
pub(crate) async fn plugin_status(
    app: AppHandle,
    state: State<'_, AppState>,
    project_id: Option<String>,
) -> HostResult<plugin_install::PluginStatus> {
    // Reuse the one path that already reconciles a session against a live
    // `/hello` — there is no second probe here, and no second opinion.
    let daemon_protocol = match project_id.filter(|id| !id.trim().is_empty()) {
        Some(project_id) => state
            .daemons
            .status(&project_id)
            .await
            .hello
            .as_ref()
            .and_then(plugin_install::hello_protocol),
        None => None,
    };

    let sources = plugin_sources(&app);
    let directory = plugin_install::plugins_dir().ok();
    blocking(move || {
        Ok(plugin_install::status(
            directory.as_deref(),
            &sources,
            daemon_protocol,
        ))
    })
    .await
}

/// Reveal the Roblox plugins folder in the OS file manager.
///
/// Argument-free for the same reason `projects_root_reveal` is: there is
/// exactly one folder it can ever open, and it is not one the webview names.
/// Creating it when it is missing is deliberate — it is where the next install
/// writes anyway, and a button that fails on a fresh machine is a worse answer
/// than an empty folder.
#[tauri::command]
pub(crate) async fn plugins_dir_reveal() -> HostResult<()> {
    let directory = plugin_install::plugins_dir()?;
    std::fs::create_dir_all(&directory).map_err(|error| {
        HostError::io(format!("could not create {}: {error}", directory.display()))
    })?;
    reveal(&directory)
}

fn plugin_sources(app: &AppHandle) -> plugin_install::Sources {
    // Two directories, because "packaged beside the binary" is one place inside
    // a `.app` and another next to `WSync.exe`. Duplicates are harmless: the
    // first one holding the artifact wins.
    let mut resource_dirs = Vec::new();
    if let Ok(directory) = app.path().resource_dir() {
        resource_dirs.push(directory);
    }
    if let Some(directory) = std::env::current_exe()
        .ok()
        .and_then(|executable| executable.parent().map(std::path::Path::to_path_buf))
    {
        resource_dirs.push(directory);
    }

    plugin_install::Sources {
        resource_dirs,
        env_override: std::env::var_os(plugin_install::ARTIFACT_PATH_ENV),
        cwd: std::env::current_dir().ok(),
    }
}

/// The native picker, constrained to one file of one extension.
async fn pick_plugin_artifact(app: &AppHandle) -> HostResult<PathBuf> {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    app.dialog()
        .file()
        .set_title("Choose WSync.rbxm")
        .add_filter("Roblox model", &["rbxm"])
        .pick_file(move |picked| {
            let _ = sender.send(picked);
        });

    let picked = receiver
        .await
        .map_err(|_| HostError::io("the file picker closed without answering"))?
        .ok_or_else(|| HostError::cancelled("no plugin file was chosen"))?;

    picked
        .into_path()
        .map_err(|error| HostError::invalid_argument(format!("unusable file: {error}")))
}

// ------------------------------------------------------------------ secrets --
//
// Design 8.2: "Open Cloud API key → OS keychain with 0600 fallback … never
// argv". The store itself is `secrets.rs`; these three commands are the door,
// and the door is deliberately narrow — a name from a fixed list in, a status
// with a masked tail out. The value crosses **into** the host and never back.
//
// Every one of them holds the secrets lock: two Settings clicks racing on the
// file store would otherwise read-modify-write over each other.

/// Store the Open Cloud API key. Returns where it landed and its masked tail.
///
/// `value` is the one secret this boundary carries, and it only travels inward.
/// It is never logged, never echoed in an error, and never returned.
#[tauri::command]
pub(crate) async fn secret_set(
    state: State<'_, AppState>,
    name: String,
    value: String,
) -> HostResult<SecretStatus> {
    secrets::validate_name(&name)?;
    let _guard = state.secrets_lock.lock().await;
    let path = state.paths.secrets_file.clone();
    // `security` is a child process and the file store is blocking I/O; neither
    // belongs on the async runtime's threads.
    blocking(move || {
        secrets::set(
            &path,
            &name,
            &value,
        )
    })
    .await
}

/// Presence, the masked tail (`"…abcd"`), and which store holds it.
///
/// Never the value: the webview shows that a key is set and lets the user
/// replace or clear it, which needs no more than this.
#[tauri::command]
pub(crate) async fn secret_get(
    state: State<'_, AppState>,
    name: String,
) -> HostResult<SecretStatus> {
    secrets::validate_name(&name)?;
    let _guard = state.secrets_lock.lock().await;
    let path = state.paths.secrets_file.clone();
    blocking(move || secrets::get(&path, &name)).await
}

/// Clear the key from **both** stores, so "Clear" leaves nothing behind.
#[tauri::command]
pub(crate) async fn secret_clear(
    state: State<'_, AppState>,
    name: String,
) -> HostResult<SecretStatus> {
    secrets::validate_name(&name)?;
    let _guard = state.secrets_lock.lock().await;
    let path = state.paths.secrets_file.clone();
    blocking(move || secrets::clear(&path, &name)).await
}

/// Reserved: hand the stored key to a spawned daemon by environment variable.
///
/// The name exists now so the handoff has one settled place to live (Design
/// 8.2: "synced into the CLI credential store via env-var handoff, never
/// argv"), but the daemon side of that contract is a later wave. Returning
/// `not_implemented` is the honest answer; returning the key to the webview so
/// it could pass it along would defeat the entire store above it.
#[tauri::command]
pub(crate) async fn secret_export_env(name: String) -> HostResult<()> {
    secrets::validate_name(&name)?;
    Err(HostError::not_implemented(
        "Handing a secret to the daemon",
        "a later wave: the CLI credential handoff",
    ))
}

/// Run blocking work off the async runtime, keeping the typed error.
///
/// Used by the two commands that do real filesystem or child-process work: the
/// secret store's keychain calls, and the plugin install's hash-and-copy.
async fn blocking<T, F>(work: F) -> HostResult<T>
where
    F: FnOnce() -> HostResult<T> + Send + 'static,
    T: Send + 'static,
{
    tauri::async_runtime::spawn_blocking(work)
        .await
        .map_err(|error| HostError::io(format!("a host task did not finish: {error}")))?
}

// ---------------------------------------------------------------- shared --

fn require_non_empty(field: &str, value: &str) -> HostResult<()> {
    if value.trim().is_empty() {
        return Err(HostError::invalid_argument(format!("{field} was empty")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stubs_validate_before_reporting_not_implemented() {
        // Argument validation runs first: a caller with a bad request learns
        // that, not that the feature is missing.
        assert_eq!(
            secrets::validate_name("nope").unwrap_err().code,
            crate::error::code::INVALID_ARGUMENT
        );
        assert!(secrets::validate_name("openCloudApiKey").is_ok());
        assert_eq!(
            require_non_empty("projectId", "  ").unwrap_err().code,
            crate::error::code::INVALID_ARGUMENT
        );
    }
}
