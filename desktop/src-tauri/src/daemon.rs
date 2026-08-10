//! Sidecar daemon lifecycle (Design 3.2–3.3).
//!
//! One `wsync daemon start` child per served project, spawned **long-lived**:
//! the child *is* the daemon, not a launcher. The host keeps the handle so the
//! daemon dies with the app, drives `/manager-heartbeat` while the project is
//! served, and is the only place the per-project owner token ever exists. The
//! token is generated here, handed to the child through `WSYNC_OWNER_TOKEN`,
//! and never crosses the IPC boundary or reaches `state.json` (Design 11).
//!
//! Ownership rules the rest of the host relies on:
//!
//! * A child that answers `alreadyRunning: true` exits immediately and the
//!   daemon it found is **not ours**. The session is recorded so the UI can
//!   talk to it, but no handle is held and it is never killed — another
//!   desktop, a terminal, or launchd may own that process.
//! * The registry lock is never held across an await that can block on a child.
//!   `stop` removes its entry first and then runs the stop sequence, so the
//!   supervisor task that reaps the child can always take the lock.

use std::{
    collections::{HashMap, VecDeque},
    ffi::OsString,
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    process::Stdio,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tauri::{AppHandle, Emitter as _};
use tokio::{
    io::{AsyncBufReadExt as _, AsyncReadExt as _, BufReader},
    process::{Child, Command},
    sync::{oneshot, watch, Mutex},
    task::JoinHandle,
    time,
};

use crate::{
    error::{HostError, HostResult},
    loopback,
};

// -------------------------------------------------------------- constants --

/// Environment variable naming the daemon binary during development. Checked
/// before the bundled sidecar so a dev build can point at a freshly built
/// engine — or at `scripts/wsync-mock` — without repackaging.
pub(crate) const DAEMON_PATH_ENV: &str = "WSYNC_DAEMON_PATH";

/// The variable the spawn contract reserves for the owner capability token.
/// Named by `--owner-token-env` so the value itself never appears in argv.
pub(crate) const OWNER_TOKEN_ENV: &str = "WSYNC_OWNER_TOKEN";

/// How long the child gets to print its one JSON ready line.
const READY_TIMEOUT: Duration = Duration::from_secs(20);

/// Design 3.3: the daemon's manager watchdog runs on a 5-minute timeout with a
/// 30-second suspect grace, so a 60 s beat survives several missed ticks.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(60);

/// Consecutive heartbeat failures tolerated before the session is called down.
const HEARTBEAT_FAILURES_BEFORE_DOWN: u32 = 3;

/// Every loopback call the host makes is small and local; slower than this is a
/// hung daemon, not a slow one.
const HTTP_TIMEOUT: Duration = Duration::from_secs(4);

/// `/hello` is the reachability probe, so it gets a tighter budget than the
/// calls that ask the daemon to do work.
const HELLO_TIMEOUT: Duration = Duration::from_secs(2);

/// How long a daemon gets to exit on its own after an accepted `/stop`.
const STOP_GRACE: Duration = Duration::from_secs(3);

/// Total budget for the app-exit sweep, however many projects are served.
const SHUTDOWN_BUDGET: Duration = Duration::from_secs(6);

/// Lines of the child's stderr kept for diagnostics. Bounded so a looping
/// daemon cannot grow the host's memory.
const STDERR_TAIL_LINES: usize = 24;

/// The daemon routes the webview's data layer may reach through the host
/// (Design 5.2). Not a proxy: an exact-path allowlist, GET only, and the target
/// is always the loopback daemon of a project this host is tracking.
///
/// `/choice/source` is the row-level read behind the staging list's inline diff
/// (Design 7.3): one divergence row's two sides, ≤256 KiB each, fetched only
/// for the row a user opened. It is a read of a frozen set — it cannot stage,
/// resolve or change anything — which is why it belongs on this side of the
/// line and not on the write allowlist.
///
/// `/review` and `/review/details` are Design 7.0's passive disk review: the
/// same paged read against the set of disk items left over after the connect
/// applied Studio → disk. Reads, and nothing more — pushing is a write.
const READABLE_ROUTES: &[&str] = &[
    "/hello",
    "/resolve",
    "/choice",
    "/choice/details",
    "/choice/source",
    "/review",
    "/review/details",
];

/// The routes the data layer may *write*, and the cap on each one's body.
///
/// Same exact-path model as the read allowlist — adding a capability means
/// adding a row here, never widening a pattern — plus a per-route byte cap, so
/// a looping view cannot hand the engine an unbounded document. The caps are
/// the contract's own bounds with envelope headroom, not round numbers picked
/// for comfort:
///
/// * `/resolve` — `{id, path?, keep, choice}`: four short strings.
/// * `/choice` — `{choiceId, choice, mode?}`: three short strings.
/// * `/choice/selection` — `{choiceId, submissionId, chunkIndex, finalChunk,
///   restart, ids:[…]}`. Design 7.3 bounds one chunk at ≤2048 ids / ≤64 KiB;
///   the rest is the envelope around them.
/// * `/review/push` — `{reviewId, ids:[…≤2048]}` or `{reviewId, mode:"all"}`:
///   Design 7.0's push of chosen disk items into Studio. Same id ceiling as a
///   selection chunk, so the same cap.
/// * `/review/dismiss` — `{reviewId}`: one short string.
const WRITABLE_ROUTES: &[(&str, usize)] = &[
    ("/resolve", 4 * 1024),
    ("/choice", 4 * 1024),
    ("/choice/selection", 80 * 1024),
    ("/review/push", 80 * 1024),
    ("/review/dismiss", 4 * 1024),
];

// --------------------------------------------------------- serialized types --

/// One served project's daemon as the frontend sees it (Design 8.3).
///
/// Deliberately does **not** carry the owner token: `state.json` is a plain
/// file and the webview is the least trusted part of the app.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DaemonSession {
    pub(crate) project_id: String,
    pub(crate) port: u16,
    pub(crate) base: String,
    pub(crate) pid: u32,
    pub(crate) boot_id: String,
    pub(crate) ok: bool,
    /// True when a matching daemon already existed. The app talks to it but
    /// does not own it: no handle, no kill, and stopping it is a request.
    pub(crate) already_running: bool,
    /// False exactly when `already_running` is true — i.e. we hold no handle.
    pub(crate) managed: bool,
    pub(crate) project: String,
    pub(crate) canonical_project: String,
    pub(crate) started_at: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DaemonStatus {
    pub(crate) project_id: String,
    /// Always true from wave 2 on: the command does real work. It stays in the
    /// payload because the frontend still branches on it for older hosts.
    pub(crate) supported: bool,
    /// `serving` · `unreachable` · `foreign` · `stopped`.
    pub(crate) state: &'static str,
    pub(crate) detail: String,
    pub(crate) running: bool,
    pub(crate) port: Option<u16>,
    pub(crate) boot_id: Option<String>,
    /// `None` is the honest answer from `/hello`, which does not report the
    /// Studio link — plugin presence arrives on the WS `plugin-status` topic.
    /// A daemon that does report it is passed straight through.
    pub(crate) plugin_connected: Option<bool>,
    pub(crate) session: Option<DaemonSession>,
    /// The raw `/hello` document when the probe answered, so the Projects view
    /// can show gameId/placeIds without a second round trip.
    pub(crate) hello: Option<Value>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DaemonResponse {
    pub(crate) status: u16,
    pub(crate) ok: bool,
    /// Parsed JSON when the daemon sent JSON, otherwise `None` and `text` holds
    /// the body. A 404 is a response, not an error: the caller decides.
    pub(crate) body: Option<Value>,
    pub(crate) text: Option<String>,
}

// ------------------------------------------------------------- ready line --

/// The one line the spawn contract promises on stdout.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReadyLine {
    ok: bool,
    port: Option<u16>,
    pid: Option<u32>,
    boot_id: Option<String>,
    project: Option<String>,
    canonical_project: Option<String>,
    #[serde(default)]
    already_running: bool,
    error: Option<Value>,
}

/// A parsed, validated success line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Ready {
    pub(crate) port: u16,
    pub(crate) pid: u32,
    pub(crate) boot_id: String,
    pub(crate) project: String,
    pub(crate) canonical_project: String,
    pub(crate) already_running: bool,
}

/// Parse the daemon's ready line.
///
/// Failures are typed rather than prose-matched: `{"ok":false,…}` is the
/// daemon's own refusal and keeps its message; anything else is a protocol
/// violation and says so.
pub(crate) fn parse_ready_line(line: &str) -> HostResult<Ready> {
    let line = line.trim();
    if line.is_empty() {
        return Err(HostError::protocol("the daemon printed an empty ready line"));
    }

    let parsed: ReadyLine = serde_json::from_str(line).map_err(|error| {
        HostError::protocol(format!(
            "the daemon's first stdout line was not the expected JSON ({error}): {}",
            truncate(line, 200)
        ))
    })?;

    if !parsed.ok {
        return Err(HostError::daemon(
            parsed
                .error
                .as_ref()
                .map(render_error)
                .unwrap_or_else(|| "the daemon refused to start and gave no reason".to_string()),
        ));
    }

    let port = parsed
        .port
        .filter(|port| *port != 0)
        .ok_or_else(|| HostError::protocol("the daemon's ready line carried no port"))?;
    let pid = parsed
        .pid
        .ok_or_else(|| HostError::protocol("the daemon's ready line carried no pid"))?;
    let boot_id = parsed
        .boot_id
        .filter(|id| !id.trim().is_empty())
        .ok_or_else(|| HostError::protocol("the daemon's ready line carried no bootId"))?;

    Ok(Ready {
        port,
        pid,
        boot_id,
        project: parsed.project.unwrap_or_default(),
        canonical_project: parsed.canonical_project.unwrap_or_default(),
        already_running: parsed.already_running,
    })
}

/// `error` may be a string or a `{code, message}` object; render both readably.
fn render_error(error: &Value) -> String {
    match error {
        Value::String(message) => message.clone(),
        Value::Object(map) => {
            let message = map
                .get("message")
                .and_then(Value::as_str)
                .or_else(|| map.get("error").and_then(Value::as_str));
            let code = map.get("code").and_then(Value::as_str);
            match (code, message) {
                (Some(code), Some(message)) => format!("{message} ({code})"),
                (None, Some(message)) => message.to_string(),
                (Some(code), None) => code.to_string(),
                (None, None) => error.to_string(),
            }
        }
        other => other.to_string(),
    }
}

fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let head: String = text.chars().take(limit).collect();
    format!("{head}…")
}

// -------------------------------------------------------------- owner token --

/// 32 CSPRNG bytes, lowercase hex (Design 11). One per project serve — never
/// reused across projects or across restarts of the same project.
pub(crate) fn generate_owner_token() -> HostResult<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes)
        .map_err(|error| HostError::io(format!("could not read secure randomness: {error}")))?;
    let mut token = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(token, "{byte:02x}");
    }
    Ok(token)
}

// ----------------------------------------------------------- binary lookup --

/// Where the `wsync` engine binary lives for this build.
///
/// `WSYNC_DAEMON_PATH` wins so a dev build can point at a fresh `cargo build`
/// or at the mock; otherwise the bundled sidecar sits next to the app
/// executable, where Tauri's `externalBin` copies it with the target-triple
/// suffix stripped. There is deliberately **no** guess at `../target/debug`: a
/// silently-wrong binary is worse than an error that says what to set.
pub(crate) fn resolve_binary() -> HostResult<PathBuf> {
    resolve_binary_from(std::env::var_os(DAEMON_PATH_ENV), bundled_sidecar())
}

fn resolve_binary_from(
    override_path: Option<OsString>,
    bundled: Option<PathBuf>,
) -> HostResult<PathBuf> {
    if let Some(raw) = override_path {
        let path = PathBuf::from(&raw);
        if !path.is_file() {
            return Err(HostError::invalid_argument(format!(
                "{DAEMON_PATH_ENV} points at {}, which is not a file",
                path.display()
            )));
        }
        return Ok(path);
    }

    if let Some(bundled) = bundled {
        // `build.rs` stages an empty placeholder in debug builds so the shell
        // compiles before the engine exists. Spawning that would fail in a
        // confusing way, so treat zero bytes as "no sidecar".
        let usable = std::fs::metadata(&bundled)
            .map(|meta| meta.is_file() && meta.len() > 0)
            .unwrap_or(false);
        if usable {
            return Ok(bundled);
        }
    }

    Err(HostError::unavailable(format!(
        "no WSync engine binary is available.\n\
         Release builds carry it as a bundled sidecar. For a dev build, point \
         {DAEMON_PATH_ENV} at one:\n  \
         cargo build -p wsync && export {DAEMON_PATH_ENV}=\"$PWD/target/debug/wsync\"\n\
         or, to run against the mock daemon:\n  \
         export {DAEMON_PATH_ENV}=\"$PWD/desktop/scripts/wsync-mock\""
    )))
}

fn bundled_sidecar() -> Option<PathBuf> {
    let executable = std::env::current_exe().ok()?;
    let directory = executable.parent()?;
    let name = if cfg!(target_os = "windows") {
        "wsync.exe"
    } else {
        "wsync"
    };
    Some(directory.join(name))
}

// -------------------------------------------------------- stop orchestration --

type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;
type Killer = Box<dyn FnOnce() -> BoxFuture<()> + Send>;
type Waiter = Box<dyn FnOnce() -> BoxFuture<bool> + Send>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StopOutcome {
    /// `/stop` was accepted and the process went away inside the grace window.
    Graceful,
    /// `/stop` was accepted but the process outlived the grace window.
    KilledAfterTimeout,
    /// `/stop` failed; the handle was killed as the last resort.
    KilledAfterError,
    /// `/stop` was accepted for a daemon we do not own — nothing to fall back to.
    Requested,
    /// `/stop` failed and we hold no handle. The daemon is still out there.
    Failed,
}

impl StopOutcome {
    /// The stable name the frontend branches on. Written out rather than
    /// derived from `Debug`, so renaming a variant cannot silently change the
    /// wire value the UI reads.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Graceful => "graceful",
            Self::KilledAfterTimeout => "killed-after-timeout",
            Self::KilledAfterError => "killed-after-error",
            Self::Requested => "requested",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct StopReport {
    pub(crate) outcome: StopOutcome,
    pub(crate) stop_error: Option<String>,
}

/// The stop sequence, factored out so its ordering is testable without a
/// process: request `/stop`, wait briefly, and only then fall back to the kill.
///
/// `kill` is `None` for a daemon we do not own, which turns the fallback off
/// entirely — killing a process another manager owns is not ours to do.
pub(crate) async fn stop_with_fallback<S, W, K>(
    stop: S,
    wait_exit: W,
    kill: Option<K>,
) -> StopReport
where
    S: FnOnce() -> BoxFuture<Result<(), String>>,
    W: FnOnce() -> BoxFuture<bool>,
    K: FnOnce() -> BoxFuture<()>,
{
    let stop_error = stop().await.err();

    let outcome = match (stop_error.is_none(), kill) {
        (true, Some(kill)) => {
            if wait_exit().await {
                StopOutcome::Graceful
            } else {
                kill().await;
                StopOutcome::KilledAfterTimeout
            }
        }
        (true, None) => StopOutcome::Requested,
        // A daemon that would not take `/stop` will not take a grace period
        // either; the kill is the whole remaining plan.
        (false, Some(kill)) => {
            kill().await;
            StopOutcome::KilledAfterError
        }
        (false, None) => StopOutcome::Failed,
    };

    StopReport {
        outcome,
        stop_error,
    }
}

// ------------------------------------------------------------------ registry --

/// The live child of one served project. Split from the registry entry so the
/// supervisor task owns the `Child` outright and nothing else has to lock it:
/// the kill channel and the exit watch are the whole interface.
struct ChildControl {
    kill: Option<oneshot::Sender<()>>,
    exited: watch::Receiver<bool>,
}

struct Entry {
    session: DaemonSession,
    owner_token: String,
    child: Option<ChildControl>,
    stderr_tail: Arc<Mutex<VecDeque<String>>>,
    /// Set before a deliberate teardown so the supervisor stays quiet and the
    /// requester owns the single `daemon:down` the frontend sees.
    expected_exit: Arc<AtomicBool>,
    tasks: Vec<JoinHandle<()>>,
}

impl Entry {
    fn abort_tasks(&mut self) {
        for task in self.tasks.drain(..) {
            task.abort();
        }
    }
}

pub(crate) struct DaemonRegistry {
    entries: Mutex<HashMap<String, Entry>>,
    /// Serializes starts so two clicks on the same toggle cannot race two
    /// children into existence. Starts are rare and fast, so a global gate is
    /// simpler than per-project bookkeeping and cannot deadlock.
    start_gate: Mutex<()>,
    events: Arc<dyn DaemonEvents>,
    /// Set only by tests, which point the registry at a scripted child instead
    /// of the real engine. `None` resolves the binary at start time.
    binary: Option<PathBuf>,
}

impl DaemonRegistry {
    pub(crate) fn new(events: Arc<dyn DaemonEvents>) -> Self {
        Self::with_binary(events, None)
    }

    fn with_binary(events: Arc<dyn DaemonEvents>, binary: Option<PathBuf>) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            start_gate: Mutex::new(()),
            events,
            binary,
        }
    }

    // --------------------------------------------------------------- start --

    pub(crate) async fn start(
        &self,
        project_id: &str,
        project_path: &str,
        data_dir: &Path,
        port: Option<u16>,
    ) -> HostResult<DaemonSession> {
        let _gate = self.start_gate.lock().await;

        // Already serving and still answering? Hand back the session we have
        // rather than spawning a second child for the same project.
        if let Some(existing) = self.session_of(project_id).await {
            if hello(existing.port).await.is_ok() {
                return Ok(existing);
            }
            // Recorded but gone. Drop it before starting a replacement.
            self.forget(project_id).await;
        }

        let binary = match &self.binary {
            Some(path) => path.clone(),
            None => resolve_binary()?,
        };
        let token = generate_owner_token()?;

        let mut command = Command::new(&binary);
        command.arg("daemon").arg("start");
        command.arg("--project").arg(project_path);
        if let Some(port) = port {
            command.arg("--port").arg(port.to_string());
        }
        command.arg("--managed-by").arg("desktop");
        command.arg("--owner-token-env").arg(OWNER_TOKEN_ENV);
        command.arg("--data-dir").arg(data_dir);
        command.arg("--raw");
        command.env(OWNER_TOKEN_ENV, &token);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Belt and braces beside the explicit kill paths: a dropped handle
            // must never leave an orphan daemon holding a port.
            .kill_on_drop(true);

        let mut child = command
            .spawn()
            .map_err(|error| HostError::io(format!("could not run {}: {error}", binary.display())))?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| HostError::io("the daemon child had no stdout"))?;
        let stderr = child.stderr.take();

        let mut lines = BufReader::new(stdout).lines();
        let ready = match time::timeout(READY_TIMEOUT, lines.next_line()).await {
            Err(_elapsed) => {
                let _ = child.start_kill();
                let detail = read_stderr(stderr).await;
                return Err(HostError::timeout(format!(
                    "the daemon did not report itself within {}s{}",
                    READY_TIMEOUT.as_secs(),
                    suffix(&detail)
                )));
            }
            Ok(Err(error)) => {
                let _ = child.start_kill();
                return Err(HostError::io(format!(
                    "could not read the daemon's ready line: {error}"
                )));
            }
            Ok(Ok(None)) => {
                // Clean EOF with no line: the child died. Its exit status and
                // stderr are the only diagnostic there is, so surface both.
                let detail = read_stderr(stderr).await;
                let code = child
                    .wait()
                    .await
                    .ok()
                    .and_then(|status| status.code())
                    .map(|code| format!(" (exit code {code})"))
                    .unwrap_or_default();
                return Err(HostError::daemon(format!(
                    "the daemon exited without reporting itself{code}{}",
                    suffix(&detail)
                )));
            }
            Ok(Ok(Some(line))) => match parse_ready_line(&line) {
                Ok(ready) => ready,
                Err(mut error) => {
                    let _ = child.start_kill();
                    let detail = read_stderr(stderr).await;
                    error.message = format!("{}{}", error.message, suffix(&detail));
                    return Err(error);
                }
            },
        };

        let session = DaemonSession {
            project_id: project_id.to_string(),
            port: ready.port,
            base: format!("http://127.0.0.1:{}", ready.port),
            pid: ready.pid,
            boot_id: ready.boot_id.clone(),
            ok: true,
            already_running: ready.already_running,
            managed: !ready.already_running,
            project: if ready.project.is_empty() {
                project_path.to_string()
            } else {
                ready.project.clone()
            },
            canonical_project: ready.canonical_project.clone(),
            started_at: epoch_millis(),
        };

        let stderr_tail: Arc<Mutex<VecDeque<String>>> = Arc::new(Mutex::new(VecDeque::new()));
        let expected_exit = Arc::new(AtomicBool::new(false));
        let mut tasks = Vec::new();

        // Keep both pipes drained: a child blocked writing to a full pipe is a
        // hang with no error message.
        tasks.push(tokio::spawn(async move {
            while let Ok(Some(_line)) = lines.next_line().await {}
        }));
        if let Some(stderr) = stderr {
            let tail = Arc::clone(&stderr_tail);
            tasks.push(tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let mut tail = tail.lock().await;
                    if tail.len() == STDERR_TAIL_LINES {
                        tail.pop_front();
                    }
                    tail.push_back(line);
                }
            }));
        }

        let child_control = if ready.already_running {
            // Per the contract this child has already exited and the daemon it
            // found belongs to somebody else. Reap it and hold nothing.
            tasks.push(tokio::spawn(async move {
                let _ = child.wait().await;
            }));
            None
        } else {
            let (kill_tx, kill_rx) = oneshot::channel::<()>();
            let (exited_tx, exited_rx) = watch::channel(false);
            tasks.push(spawn_supervisor(
                Arc::clone(&self.events),
                project_id.to_string(),
                ready.boot_id.clone(),
                child,
                kill_rx,
                exited_tx,
                Arc::clone(&stderr_tail),
                Arc::clone(&expected_exit),
            ));
            Some(ChildControl {
                kill: Some(kill_tx),
                exited: exited_rx,
            })
        };

        tasks.push(self.spawn_heartbeat(project_id.to_string(), session.port, token.clone()));

        self.entries.lock().await.insert(
            project_id.to_string(),
            Entry {
                session: session.clone(),
                owner_token: token,
                child: child_control,
                stderr_tail,
                expected_exit,
                tasks,
            },
        );

        self.events.up(&session);
        Ok(session)
    }

    /// Design 3.3: one beat per served project while it is served.
    fn spawn_heartbeat(&self, project_id: String, port: u16, token: String) -> JoinHandle<()> {
        let events = Arc::clone(&self.events);
        tokio::spawn(async move {
            let body = serde_json::json!({ "token": token });
            let mut failures = 0u32;
            let mut reported_down = false;
            loop {
                time::sleep(HEARTBEAT_INTERVAL).await;
                match post_json(port, "/manager-heartbeat", &body).await {
                    Ok(_) => {
                        failures = 0;
                        reported_down = false;
                    }
                    Err(error) => {
                        failures += 1;
                        if failures >= HEARTBEAT_FAILURES_BEFORE_DOWN && !reported_down {
                            reported_down = true;
                            emit_down(
                                &events,
                                &project_id,
                                "heartbeat-lost",
                                &format!("The daemon stopped answering heartbeats: {error}"),
                                None,
                            );
                        }
                    }
                }
            }
        })
    }

    // ---------------------------------------------------------------- stop --

    pub(crate) async fn stop(&self, project_id: &str) -> HostResult<StopReport> {
        // Take the entry out *first*: the stop sequence awaits the child's
        // exit, and the supervisor that observes that exit needs this lock.
        let Some(mut entry) = self.entries.lock().await.remove(project_id) else {
            return Err(HostError::unavailable(format!(
                "no daemon is being tracked for {project_id}"
            )));
        };
        entry.expected_exit.store(true, Ordering::SeqCst);

        let port = entry.session.port;
        let token = entry.owner_token.clone();
        let boot_id = entry.session.boot_id.clone();

        let stop = move || -> BoxFuture<Result<(), String>> {
            Box::pin(async move {
                post_json(
                    port,
                    "/stop",
                    &serde_json::json!({ "bootId": boot_id, "token": token }),
                )
                .await
                .map(|_| ())
            })
        };

        let (wait_exit, kill): (Waiter, Option<Killer>) = match entry.child.take() {
            Some(ChildControl { kill, exited }) => {
                let mut waiting = exited.clone();
                let waiter: Waiter = Box::new(move || {
                    Box::pin(async move {
                        if *waiting.borrow_and_update() {
                            return true;
                        }
                        time::timeout(STOP_GRACE, waiting.changed()).await.is_ok()
                    })
                });
                let mut killing = exited;
                let killer: Killer = Box::new(move || {
                    Box::pin(async move {
                        if let Some(kill) = kill {
                            let _ = kill.send(());
                        }
                        // Give the supervisor a moment to reap before the task
                        // is aborted, so no zombie outlives the app.
                        let _ = time::timeout(STOP_GRACE, killing.changed()).await;
                    })
                });
                (waiter, Some(killer))
            }
            None => (Box::new(|| Box::pin(async { true })), None),
        };

        let report = stop_with_fallback(stop, wait_exit, kill).await;
        entry.abort_tasks();

        let detail = match (report.outcome, &report.stop_error) {
            (StopOutcome::Graceful, _) => "The daemon shut down.".to_string(),
            (StopOutcome::Requested, _) => {
                "Stop was requested; that daemon is managed elsewhere.".to_string()
            }
            (StopOutcome::KilledAfterTimeout, _) => format!(
                "The daemon did not exit within {}s and was terminated.",
                STOP_GRACE.as_secs()
            ),
            (StopOutcome::KilledAfterError, Some(error)) => {
                format!("Stop failed ({error}); the daemon was terminated.")
            }
            (StopOutcome::KilledAfterError, None) => {
                "Stop failed; the daemon was terminated.".to_string()
            }
            (StopOutcome::Failed, Some(error)) => format!("Stop failed: {error}"),
            (StopOutcome::Failed, None) => "Stop failed.".to_string(),
        };
        emit_down(
            &self.events,
            project_id,
            "stopped",
            &detail,
            Some(&entry.session.boot_id),
        );

        if report.outcome == StopOutcome::Failed {
            return Err(HostError::daemon(detail));
        }
        Ok(report)
    }

    /// Best-effort teardown on app exit (Design 3.3: handles are kept so the
    /// daemons die with the app). `/manager-close` first — the manager going
    /// away is exactly what that route means — then the kill.
    pub(crate) async fn shutdown_all(&self) {
        let entries: Vec<(String, Entry)> = {
            let mut guard = self.entries.lock().await;
            guard.drain().collect()
        };
        if entries.is_empty() {
            return;
        }

        let sweep = async move {
            for (_project_id, mut entry) in entries {
                entry.expected_exit.store(true, Ordering::SeqCst);
                let _ = post_json(
                    entry.session.port,
                    "/manager-close",
                    &serde_json::json!({ "token": entry.owner_token }),
                )
                .await;
                if let Some(control) = entry.child.take() {
                    if let Some(kill) = control.kill {
                        let _ = kill.send(());
                    }
                    let mut exited = control.exited;
                    if !*exited.borrow_and_update() {
                        let _ = time::timeout(STOP_GRACE, exited.changed()).await;
                    }
                }
                entry.abort_tasks();
            }
        };
        let _ = time::timeout(SHUTDOWN_BUDGET, sweep).await;
    }

    // -------------------------------------------------------------- status --

    pub(crate) async fn status(&self, project_id: &str) -> DaemonStatus {
        let Some(session) = self.session_of(project_id).await else {
            return DaemonStatus {
                project_id: project_id.to_string(),
                supported: true,
                state: "stopped",
                detail: "No daemon is running for this project.".to_string(),
                running: false,
                port: None,
                boot_id: None,
                plugin_connected: None,
                session: None,
                hello: None,
            };
        };

        match hello(session.port).await {
            Ok(hello) => {
                let reported = hello.get("bootId").and_then(Value::as_str);
                let plugin_connected = hello.get("pluginConnected").and_then(Value::as_bool);
                if reported.is_none_or(|id| id == session.boot_id) {
                    DaemonStatus {
                        project_id: project_id.to_string(),
                        supported: true,
                        state: "serving",
                        detail: format!("Serving on 127.0.0.1:{}.", session.port),
                        running: true,
                        port: Some(session.port),
                        boot_id: Some(session.boot_id.clone()),
                        plugin_connected,
                        session: Some(session),
                        hello: Some(hello),
                    }
                } else {
                    // Something else took the port. Say so rather than drawing
                    // a green dot for a daemon that is not ours.
                    DaemonStatus {
                        project_id: project_id.to_string(),
                        supported: true,
                        state: "foreign",
                        detail: format!(
                            "Another daemon answers on 127.0.0.1:{} (boot id {}).",
                            session.port,
                            reported.unwrap_or("unknown")
                        ),
                        running: false,
                        port: Some(session.port),
                        boot_id: Some(session.boot_id.clone()),
                        plugin_connected: None,
                        session: Some(session),
                        hello: Some(hello),
                    }
                }
            }
            Err(error) => {
                let tail = self.stderr_tail(project_id).await;
                DaemonStatus {
                    project_id: project_id.to_string(),
                    supported: true,
                    state: "unreachable",
                    detail: format!(
                        "The daemon on 127.0.0.1:{} is not answering: {error}{}",
                        session.port,
                        suffix(&tail)
                    ),
                    running: false,
                    port: Some(session.port),
                    boot_id: Some(session.boot_id.clone()),
                    plugin_connected: None,
                    session: Some(session),
                    hello: None,
                }
            }
        }
    }

    // ------------------------------------------------------------ requests --

    /// Authenticated read against a served project's daemon.
    ///
    /// The host makes this call, not the webview, for two reasons: the owner
    /// token stays in the host, and a native loopback client carries no
    /// `Origin`, so it lands on the daemon's un-gated path rather than the
    /// browser-origin one (Design 5.2).
    pub(crate) async fn request(&self, project_id: &str, route: &str) -> HostResult<DaemonResponse> {
        let route = validate_route(route)?;
        let session = self.session_of(project_id).await.ok_or_else(|| {
            HostError::unavailable(format!("no daemon is being tracked for {project_id}"))
        })?;

        let response = loopback::get(session.port, &route, HTTP_TIMEOUT)
            .await
            .map_err(|error| HostError::daemon(format!("{route} failed: {error}")))?;

        Ok(into_daemon_response(response))
    }

    /// Authenticated write against a served project's daemon.
    ///
    /// Everything `request` says about the transport holds here too — the token
    /// stays in the host, and no `Origin` header means the daemon's un-gated
    /// loopback path (Design 5.2). What is different is the blast radius, so
    /// this side is narrower still: an exact-path allowlist with **no query
    /// string**, a JSON object body (never a bare array or scalar), and a
    /// per-route byte cap checked before a socket is opened.
    ///
    /// Like the read side, a non-2xx answer is returned rather than thrown: the
    /// divergence flow has to *see* a 409 to know the choice was resolved
    /// elsewhere, and a 404 to know a conflict id is already gone.
    pub(crate) async fn post(
        &self,
        project_id: &str,
        route: &str,
        body: Value,
    ) -> HostResult<DaemonResponse> {
        let (route, cap) = validate_post_route(route)?;
        if !body.is_object() {
            return Err(HostError::invalid_argument(format!(
                "{route} takes a JSON object body"
            )));
        }

        let encoded = serde_json::to_vec(&body)
            .map_err(|error| HostError::invalid_argument(format!("unserializable body: {error}")))?;
        if encoded.len() > cap {
            return Err(HostError::invalid_argument(format!(
                "{route} bodies are capped at {cap} bytes; this one was {}",
                encoded.len()
            )));
        }

        let session = self.session_of(project_id).await.ok_or_else(|| {
            HostError::unavailable(format!("no daemon is being tracked for {project_id}"))
        })?;

        let response = loopback::post_json(session.port, route, &body, HTTP_TIMEOUT)
            .await
            .map_err(|error| HostError::daemon(format!("{route} failed: {error}")))?;

        Ok(into_daemon_response(response))
    }

    // ------------------------------------------------------------- helpers --

    pub(crate) async fn session_of(&self, project_id: &str) -> Option<DaemonSession> {
        self.entries
            .lock()
            .await
            .get(project_id)
            .map(|entry| entry.session.clone())
    }

    async fn stderr_tail(&self, project_id: &str) -> String {
        let tail = {
            let guard = self.entries.lock().await;
            guard
                .get(project_id)
                .map(|entry| Arc::clone(&entry.stderr_tail))
        };
        match tail {
            Some(tail) => tail
                .lock()
                .await
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join("\n"),
            None => String::new(),
        }
    }

    /// Drop a session we can no longer reach, quietly: the caller is about to
    /// replace it and will emit the transition itself.
    async fn forget(&self, project_id: &str) {
        if let Some(mut entry) = self.entries.lock().await.remove(project_id) {
            entry.expected_exit.store(true, Ordering::SeqCst);
            if let Some(control) = entry.child.take() {
                if let Some(kill) = control.kill {
                    let _ = kill.send(());
                }
            }
            entry.abort_tasks();
        }
    }

}

/// Probe a daemon's identity. Short timeout on purpose: this is the reachability
/// check, and a daemon that cannot answer it in two seconds is not serving.
async fn hello(port: u16) -> Result<Value, String> {
    let response = loopback::get(port, "/hello", HELLO_TIMEOUT).await?;
    if !response.is_success() {
        return Err(format!("/hello answered {}", response.status));
    }
    response
        .json()
        .ok_or_else(|| "/hello did not answer with JSON".to_string())
}

/// Reaps the child and reports an *unexpected* exit exactly once. A deliberate
/// teardown sets `expected_exit` and emits its own, richer `daemon:down`.
#[allow(clippy::too_many_arguments)]
fn spawn_supervisor(
    events: Arc<dyn DaemonEvents>,
    project_id: String,
    boot_id: String,
    mut child: Child,
    kill_rx: oneshot::Receiver<()>,
    exited_tx: watch::Sender<bool>,
    stderr_tail: Arc<Mutex<VecDeque<String>>>,
    expected_exit: Arc<AtomicBool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let detail = tokio::select! {
            status = child.wait() => {
                let code = status.ok().and_then(|status| status.code());
                let tail = stderr_tail.lock().await.iter().cloned().collect::<Vec<_>>();
                let head = match code {
                    Some(0) => "The daemon exited.".to_string(),
                    Some(code) => format!("The daemon exited with code {code}."),
                    None => "The daemon was terminated.".to_string(),
                };
                if tail.is_empty() { head } else { format!("{head}\n{}", tail.join("\n")) }
            }
            _ = kill_rx => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                "The daemon was stopped by WSync.".to_string()
            }
        };

        let _ = exited_tx.send(true);
        if !expected_exit.load(Ordering::SeqCst) {
            emit_down(&events, &project_id, "exited", &detail, Some(&boot_id));
        }
    })
}

/// Exact-path allowlist. Query strings are allowed (the divergence detail list
/// is cursor-paged); path traversal, absolute URLs, and unknown routes are not.
fn validate_route(route: &str) -> HostResult<String> {
    let route = shaped_like_a_route(route)?;
    let path = route.split('?').next().unwrap_or_default();
    if !READABLE_ROUTES.contains(&path) {
        return Err(HostError::invalid_argument(format!(
            "{path:?} is not a route the app may read (allowed: {})",
            READABLE_ROUTES.join(", ")
        )));
    }
    Ok(route.to_string())
}

/// The write allowlist: exact path, no query string at all, and the route's
/// body cap returned alongside it so the caller cannot forget to apply one.
fn validate_post_route(route: &str) -> HostResult<(&'static str, usize)> {
    let route = shaped_like_a_route(route)?;
    if route.contains('?') {
        return Err(HostError::invalid_argument(format!(
            "{route:?} carries a query string; the app's write routes take a body, not parameters"
        )));
    }
    WRITABLE_ROUTES
        .iter()
        .find(|(path, _)| *path == route)
        .map(|(path, cap)| (*path, *cap))
        .ok_or_else(|| {
            HostError::invalid_argument(format!(
                "{route:?} is not a route the app may write (allowed: {})",
                WRITABLE_ROUTES
                    .iter()
                    .map(|(path, _)| *path)
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        })
}

/// The checks both allowlists share: a rooted, single-slash, traversal-free
/// path. Absolute URLs fail on the leading-slash rule, so `http://elsewhere/…`
/// can never be reached through either command.
///
/// Traversal is checked on the path only. A `..` inside a query value cannot
/// escape anything — the path is still matched exactly against the allowlist —
/// and rejecting it would fail on a legitimate `choiceId` that happened to
/// contain two dots.
fn shaped_like_a_route(route: &str) -> HostResult<&str> {
    let route = route.trim();
    let path = route.split('?').next().unwrap_or_default();
    if !route.starts_with('/') || route.starts_with("//") || path.contains("..") {
        return Err(HostError::invalid_argument(format!(
            "{route:?} is not a daemon route"
        )));
    }
    Ok(route)
}

/// One mapping from the loopback client's answer to the shape the webview sees.
fn into_daemon_response(response: loopback::Response) -> DaemonResponse {
    let body = response.json();
    DaemonResponse {
        status: response.status,
        ok: response.is_success(),
        text: if body.is_none() {
            Some(truncate(&response.text(), 2000))
        } else {
            None
        },
        body,
    }
}

/// POST a JSON body and require a 2xx. Anything else — including the daemon's
/// own 401 for a wrong owner token — is the error, with its status attached.
async fn post_json(port: u16, route: &str, body: &Value) -> Result<u16, String> {
    let response = loopback::post_json(port, route, body, HTTP_TIMEOUT).await?;
    if response.is_success() {
        Ok(response.status)
    } else {
        Err(format!("{route} answered {}", response.status))
    }
}

async fn read_stderr(stderr: Option<tokio::process::ChildStderr>) -> String {
    let Some(mut stderr) = stderr else {
        return String::new();
    };
    let mut buffer = Vec::new();
    if time::timeout(Duration::from_millis(750), stderr.read_to_end(&mut buffer))
        .await
        .is_err()
    {
        return String::new();
    }
    truncate(String::from_utf8_lossy(&buffer).trim(), 2000)
}

fn suffix(detail: &str) -> String {
    if detail.trim().is_empty() {
        String::new()
    } else {
        format!("\n{}", detail.trim())
    }
}

fn epoch_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or_default()
}

// -------------------------------------------------------------------- events --

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DownEvent {
    pub(crate) project_id: String,
    /// `stopped` · `exited` · `heartbeat-lost`. The frontend branches on this.
    pub(crate) reason: String,
    pub(crate) detail: String,
    pub(crate) boot_id: Option<String>,
}

/// Where lifecycle transitions go.
///
/// The registry is written against this rather than against `AppHandle` for one
/// concrete reason: the spawn, adopt and stop paths are the parts most worth
/// testing against a real child process, and a trait means those tests need no
/// Tauri runtime at all.
pub(crate) trait DaemonEvents: Send + Sync + 'static {
    fn up(&self, session: &DaemonSession);
    fn down(&self, event: &DownEvent);
}

impl DaemonEvents for AppHandle {
    fn up(&self, session: &DaemonSession) {
        let _ = self.emit("daemon:up", session);
    }

    fn down(&self, event: &DownEvent) {
        let _ = self.emit("daemon:down", event);
    }
}

fn emit_down(
    events: &Arc<dyn DaemonEvents>,
    project_id: &str,
    reason: &str,
    detail: &str,
    boot_id: Option<&str>,
) {
    events.down(&DownEvent {
        project_id: project_id.to_string(),
        reason: reason.to_string(),
        detail: detail.to_string(),
        boot_id: boot_id.map(str::to_string),
    });
}

// --------------------------------------------------------------------- tests --

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    // --- ready line ---------------------------------------------------------

    #[test]
    fn ready_line_parses_the_spawn_contract() {
        let ready = parse_ready_line(
            r#"{"ok":true,"port":7978,"pid":4242,"bootId":"b-1","project":"/p","canonicalProject":"/p"}"#,
        )
        .unwrap();
        assert_eq!(
            ready,
            Ready {
                port: 7978,
                pid: 4242,
                boot_id: "b-1".into(),
                project: "/p".into(),
                canonical_project: "/p".into(),
                already_running: false,
            }
        );
    }

    #[test]
    fn ready_line_carries_already_running() {
        let ready = parse_ready_line(
            r#"{"ok":true,"port":7979,"pid":9,"bootId":"b","project":"/p","canonicalProject":"/p","alreadyRunning":true}"#,
        )
        .unwrap();
        assert!(ready.already_running, "alreadyRunning must survive parsing");
    }

    #[test]
    fn ready_line_tolerates_surrounding_whitespace() {
        assert!(parse_ready_line("  {\"ok\":true,\"port\":7978,\"pid\":1,\"bootId\":\"b\"}\r\n").is_ok());
    }

    #[test]
    fn ready_line_surfaces_the_daemons_own_refusal() {
        let error =
            parse_ready_line(r#"{"ok":false,"error":"port 7978 serves another project"}"#).unwrap_err();
        assert_eq!(error.code, crate::error::code::DAEMON);
        assert!(error.message.contains("another project"), "{}", error.message);

        let structured =
            parse_ready_line(r#"{"ok":false,"error":{"code":"port_conflict","message":"busy"}}"#)
                .unwrap_err();
        assert_eq!(structured.message, "busy (port_conflict)");

        let bare = parse_ready_line(r#"{"ok":false}"#).unwrap_err();
        assert_eq!(bare.code, crate::error::code::DAEMON);
    }

    #[test]
    fn ready_line_rejects_non_contract_output() {
        for line in [
            "",
            "   ",
            "Compiling wsync v0.1.0",
            r#"{"ok":true,"pid":1,"bootId":"b"}"#,          // no port
            r#"{"ok":true,"port":0,"pid":1,"bootId":"b"}"#, // port 0
            r#"{"ok":true,"port":7978,"bootId":"b"}"#,      // no pid
            r#"{"ok":true,"port":7978,"pid":1}"#,           // no bootId
            r#"{"ok":true,"port":7978,"pid":1,"bootId":"  "}"#,
        ] {
            let error = parse_ready_line(line).unwrap_err();
            assert_eq!(
                error.code,
                crate::error::code::PROTOCOL,
                "{line:?} should be a protocol error, got {error}"
            );
        }
    }

    // --- owner token --------------------------------------------------------

    #[test]
    fn owner_tokens_are_32_random_bytes_of_lowercase_hex() {
        let token = generate_owner_token().unwrap();
        assert_eq!(token.len(), 64, "32 bytes, hex-encoded");
        assert!(
            token
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "lowercase hex only: {token}"
        );

        // Distinctness is the whole point: one token per project serve.
        let mut seen = std::collections::HashSet::new();
        for _ in 0..64 {
            assert!(seen.insert(generate_owner_token().unwrap()), "token repeated");
        }
    }

    // --- binary lookup ------------------------------------------------------

    #[test]
    fn binary_lookup_prefers_the_override_and_never_guesses() {
        let directory = tempfile::tempdir().unwrap();
        let real = directory.path().join("wsync");
        std::fs::write(&real, b"#!/bin/sh\n").unwrap();
        let bundled = directory.path().join("bundled");
        std::fs::write(&bundled, b"#!/bin/sh\n").unwrap();

        // Override wins over the bundled sidecar.
        assert_eq!(
            resolve_binary_from(Some(real.clone().into()), Some(bundled.clone())).unwrap(),
            real
        );

        // A bad override is reported, not silently ignored in favour of the
        // sidecar — otherwise a typo would run the wrong engine.
        let error =
            resolve_binary_from(Some(directory.path().join("nope").into()), Some(bundled.clone()))
                .unwrap_err();
        assert_eq!(error.code, crate::error::code::INVALID_ARGUMENT);

        // The zero-byte placeholder build.rs stages is not a usable sidecar.
        let placeholder = directory.path().join("placeholder");
        std::fs::write(&placeholder, b"").unwrap();
        let error = resolve_binary_from(None, Some(placeholder)).unwrap_err();
        assert_eq!(error.code, crate::error::code::UNAVAILABLE);
        assert!(
            error.message.contains(DAEMON_PATH_ENV),
            "the error must say what to set: {}",
            error.message
        );

        assert_eq!(resolve_binary_from(None, Some(bundled.clone())).unwrap(), bundled);
    }

    // --- route allowlist ----------------------------------------------------

    #[test]
    fn route_allowlist_is_exact() {
        assert!(validate_route("/resolve").is_ok());
        assert!(validate_route("/choice/details?cursor=abc").is_ok());
        // Traversal is a path concern; a query value that contains dots is not
        // one, and a choiceId is opaque.
        assert!(validate_route("/choice/details?choiceId=ch..1&cursor=0").is_ok());
        for bad in [
            "/stop",
            "/push",
            "resolve",
            "//evil.example/resolve",
            "/choice/../stop",
            "/resolvex",
            "http://127.0.0.1:7978/resolve",
        ] {
            assert!(validate_route(bad).is_err(), "{bad} should be refused");
        }
    }

    #[test]
    fn the_row_source_route_is_readable_and_never_writable() {
        // The staging list's inline diff (Design 7.3): a cursor-free read of one
        // frozen row, query string and all.
        assert!(validate_route("/choice/source?choiceId=ch_1&id=17").is_ok());
        assert!(validate_route("/choice/source").is_ok());
        // Neighbours of the exact path stay refused — the allowlist is not a
        // prefix match, which is the whole reason it is spelled out.
        for bad in [
            "/choice/sources",
            "/choice/source/17",
            "/choice/../choice/source",
        ] {
            assert!(validate_route(bad).is_err(), "{bad} should be refused");
        }
        // Readable is not writable: a row diff is a read, and nothing about
        // opening one may reach a route that changes state.
        assert!(validate_post_route("/choice/source").is_err());
    }

    #[test]
    fn the_review_routes_split_the_same_way_the_choice_ones_do() {
        // Design 7.0's passive review: two reads, two writes, no overlap.
        assert!(validate_route("/review").is_ok());
        assert!(validate_route("/review/details?reviewId=rv_1&cursor=0&limit=512").is_ok());
        assert!(validate_post_route("/review/push").is_ok());
        assert!(validate_post_route("/review/dismiss").is_ok());

        // Pushing is a write, listing is not — neither can be reached the other
        // way round, and neighbours of both are refused.
        assert!(validate_post_route("/review").is_err());
        assert!(validate_post_route("/review/details").is_err());
        assert!(validate_route("/review/push").is_err());
        assert!(validate_route("/review/dismiss").is_err());
        for bad in ["/reviews", "/review/detail", "/review/pushes", "/review/../stop"] {
            assert!(validate_route(bad).is_err(), "{bad} should be refused");
            assert!(validate_post_route(bad).is_err(), "{bad} should be refused");
        }
    }

    #[test]
    fn write_allowlist_is_exact_and_narrower_than_the_read_one() {
        for good in [
            "/resolve",
            "/choice",
            "/choice/selection",
            "/review/push",
            "/review/dismiss",
        ] {
            assert!(validate_post_route(good).is_ok(), "{good} should be allowed");
        }
        for bad in [
            // Readable, but nothing the app may write.
            "/hello",
            "/choice/details",
            "/review",
            "/review/details",
            // Never reachable from the webview at all.
            "/stop",
            "/push",
            "/pull/stream",
            "/compare",
            "/projects/init",
            // Shape violations.
            "choice",
            "//evil.example/choice",
            "/choice/../stop",
            "/choicex",
            "http://127.0.0.1:7978/choice",
            // A write route takes a body, so a query string is not a thing it
            // can mean — and allowing one would widen the exact-path match.
            "/choice?choice=disk",
            "/resolve?keep=studio",
        ] {
            assert!(validate_post_route(bad).is_err(), "{bad} should be refused");
        }
    }

    #[test]
    fn every_write_route_carries_a_cap_that_fits_its_contract() {
        // Design 7.3: ≤2048 ids / ≤64 KiB per selection chunk. The cap has to
        // clear that with room for the envelope, and the two small routes have
        // to stay small — a cap nobody can hit is not a cap.
        let cap = |route: &str| validate_post_route(route).unwrap().1;
        assert!(cap("/choice/selection") >= 64 * 1024);
        assert!(cap("/choice/selection") <= 128 * 1024);
        assert_eq!(cap("/resolve"), 4 * 1024);
        assert_eq!(cap("/choice"), 4 * 1024);
        // Design 7.0's push carries the same ≤2048 ids, so it gets the same
        // room; dismissing carries one id and stays small.
        assert_eq!(cap("/review/push"), cap("/choice/selection"));
        assert_eq!(cap("/review/dismiss"), 4 * 1024);
    }

    #[tokio::test]
    async fn a_write_refuses_a_body_that_is_not_an_object_or_is_over_the_cap() {
        let (_events, registry) = registry_for(PathBuf::from("/nonexistent"));

        // Both checks run before the session lookup, so a bad body is reported
        // as a bad body rather than as "no daemon is tracked".
        let error = registry
            .post("p_none", "/choice", Value::Array(vec![]))
            .await
            .unwrap_err();
        assert_eq!(error.code, crate::error::code::INVALID_ARGUMENT);
        assert!(error.message.contains("JSON object"), "{}", error.message);

        let ids: Vec<Value> = (0..40_000).map(|id| Value::from(id)).collect();
        let error = registry
            .post(
                "p_none",
                "/choice/selection",
                serde_json::json!({ "choiceId": "c", "ids": ids }),
            )
            .await
            .unwrap_err();
        assert_eq!(error.code, crate::error::code::INVALID_ARGUMENT);
        assert!(error.message.contains("capped at"), "{}", error.message);

        // A body inside the cap gets as far as the (absent) session.
        let error = registry
            .post("p_none", "/choice", serde_json::json!({ "choice": "disk" }))
            .await
            .unwrap_err();
        assert_eq!(error.code, crate::error::code::UNAVAILABLE);
    }

    // --- stop ordering ------------------------------------------------------

    /// A fake child: records the order in which the stop sequence calls it.
    #[derive(Clone, Default)]
    struct FakeChild(Arc<StdMutex<Vec<&'static str>>>);

    impl FakeChild {
        fn note(&self, entry: &'static str) {
            self.0.lock().unwrap().push(entry);
        }
        fn calls(&self) -> Vec<&'static str> {
            self.0.lock().unwrap().clone()
        }
    }

    /// The three halves of a stop, each recording that it ran.
    type Sequence = (
        Box<dyn FnOnce() -> BoxFuture<Result<(), String>> + Send>,
        Waiter,
        Killer,
    );

    fn sequence(child: FakeChild, stop_ok: bool, exits_in_grace: bool) -> Sequence {
        let (on_stop, on_wait, on_kill) = (child.clone(), child.clone(), child);
        (
            Box::new(move || -> BoxFuture<Result<(), String>> {
                Box::pin(async move {
                    on_stop.note("stop");
                    if stop_ok {
                        Ok(())
                    } else {
                        Err("connection refused".to_string())
                    }
                })
            }),
            Box::new(move || -> BoxFuture<bool> {
                Box::pin(async move {
                    on_wait.note("wait");
                    exits_in_grace
                })
            }),
            Box::new(move || -> BoxFuture<()> {
                Box::pin(async move {
                    on_kill.note("kill");
                })
            }),
        )
    }

    #[test]
    fn stop_outcomes_have_stable_wire_names() {
        // `daemon_stop` returns these to the frontend, which branches on
        // "requested" to explain that the daemon is managed elsewhere.
        assert_eq!(StopOutcome::Graceful.as_str(), "graceful");
        assert_eq!(StopOutcome::Requested.as_str(), "requested");
        assert_eq!(StopOutcome::KilledAfterTimeout.as_str(), "killed-after-timeout");
        assert_eq!(StopOutcome::KilledAfterError.as_str(), "killed-after-error");
        assert_eq!(StopOutcome::Failed.as_str(), "failed");
    }

    #[tokio::test]
    async fn stop_never_kills_a_daemon_that_shut_itself_down() {
        let child = FakeChild::default();
        let (stop, wait, kill) = sequence(child.clone(), true, true);
        let report = stop_with_fallback(stop, wait, Some(kill)).await;
        assert_eq!(report.outcome, StopOutcome::Graceful);
        assert_eq!(child.calls(), vec!["stop", "wait"]);
    }

    #[tokio::test]
    async fn kill_is_the_last_resort_after_the_grace_window() {
        let child = FakeChild::default();
        let (stop, wait, kill) = sequence(child.clone(), true, false);
        let report = stop_with_fallback(stop, wait, Some(kill)).await;
        assert_eq!(report.outcome, StopOutcome::KilledAfterTimeout);
        // Ordering is the contract: /stop, then wait, and only then the kill.
        assert_eq!(child.calls(), vec!["stop", "wait", "kill"]);
    }

    #[tokio::test]
    async fn a_failed_stop_falls_straight_through_to_the_kill() {
        let child = FakeChild::default();
        let (stop, wait, kill) = sequence(child.clone(), false, true);
        let report = stop_with_fallback(stop, wait, Some(kill)).await;
        assert_eq!(report.outcome, StopOutcome::KilledAfterError);
        assert_eq!(report.stop_error.as_deref(), Some("connection refused"));
        assert_eq!(
            child.calls(),
            vec!["stop", "kill"],
            "no grace period for a daemon that would not take /stop"
        );
    }

    #[tokio::test]
    async fn a_daemon_we_do_not_own_is_asked_never_killed() {
        let child = FakeChild::default();
        let (stop, wait, _kill) = sequence(child.clone(), true, false);
        let report = stop_with_fallback(stop, wait, None::<Killer>).await;
        assert_eq!(report.outcome, StopOutcome::Requested);
        assert_eq!(
            child.calls(),
            vec!["stop"],
            "an alreadyRunning daemon is never killed"
        );

        let child = FakeChild::default();
        let (stop, wait, _kill) = sequence(child.clone(), false, false);
        let report = stop_with_fallback(stop, wait, None::<Killer>).await;
        assert_eq!(report.outcome, StopOutcome::Failed);
        assert_eq!(child.calls(), vec!["stop"]);
    }

    // --- the whole path, against a real child ------------------------------

    /// Collects the lifecycle transitions a run produced, so a test can assert
    /// the frontend would have been told exactly once.
    #[derive(Default)]
    struct Recorder {
        ups: StdMutex<Vec<DaemonSession>>,
        downs: StdMutex<Vec<DownEvent>>,
    }

    impl DaemonEvents for Recorder {
        fn up(&self, session: &DaemonSession) {
            self.ups.lock().unwrap().push(session.clone());
        }
        fn down(&self, event: &DownEvent) {
            self.downs.lock().unwrap().push(event.clone());
        }
    }

    impl Recorder {
        fn up_count(&self) -> usize {
            self.ups.lock().unwrap().len()
        }
        fn down_reasons(&self) -> Vec<String> {
            self.downs
                .lock()
                .unwrap()
                .iter()
                .map(|event| event.reason.clone())
                .collect()
        }
    }

    fn registry_for(binary: PathBuf) -> (Arc<Recorder>, DaemonRegistry) {
        let recorder = Arc::new(Recorder::default());
        let registry = DaemonRegistry::with_binary(Arc::clone(&recorder) as Arc<dyn DaemonEvents>, Some(binary));
        (recorder, registry)
    }

    /// `desktop/scripts/wsync-mock`, when Node is available to run it.
    fn mock_shim() -> Option<PathBuf> {
        if std::process::Command::new("node")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| !status.success())
            .unwrap_or(true)
        {
            eprintln!("skipping: node is not on PATH");
            return None;
        }
        let shim = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("scripts")
            .join("wsync-mock");
        shim.is_file().then_some(shim)
    }

    /// A one-line fake engine, for the paths that need no daemon at all.
    #[cfg(unix)]
    fn scripted_daemon(directory: &Path, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt as _;
        let path = directory.join("fake-wsync");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[tokio::test]
    async fn starts_probes_reads_and_stops_a_real_daemon() {
        let Some(shim) = mock_shim() else { return };
        let directory = tempfile::tempdir().unwrap();
        let project = directory.path().to_string_lossy().into_owned();
        let (events, registry) = registry_for(shim);

        let session = registry
            .start("p_test", &project, directory.path(), None)
            .await
            .expect("the mock daemon should start");
        assert!(session.ok && session.managed && !session.already_running);
        assert!(session.port >= 7978, "port {} is outside the range", session.port);
        assert_eq!(session.base, format!("http://127.0.0.1:{}", session.port));
        assert_eq!(events.up_count(), 1);

        // `/hello` agrees with the ready line, so the status is honest.
        let status = registry.status("p_test").await;
        assert!(status.supported && status.running);
        assert_eq!(status.state, "serving");
        assert_eq!(status.boot_id.as_deref(), Some(session.boot_id.as_str()));
        assert_eq!(
            status.hello.as_ref().and_then(|hello| hello.get("managedBy")),
            Some(&Value::String("desktop".into())),
            "the spawn contract's --managed-by must reach the daemon"
        );

        // The data layer's read works; the allowlist still holds.
        let resolve = registry.request("p_test", "/resolve").await.unwrap();
        assert_eq!(resolve.status, 200);
        assert!(resolve.ok);
        assert!(registry.request("p_test", "/stop").await.is_err());

        // ...and so does its write, including the part that matters most: a
        // refusal comes back as data, so the frontend can tell "unknown id"
        // from "the daemon is unreachable".
        let unknown = registry
            .post(
                "p_test",
                "/resolve",
                serde_json::json!({ "id": "no-such-conflict", "keep": "local", "choice": "local" }),
            )
            .await
            .unwrap();
        assert_eq!(unknown.status, 404);
        assert!(!unknown.ok);
        assert!(registry
            .post("p_test", "/stop", serde_json::json!({}))
            .await
            .is_err());

        // Stop is graceful: the daemon took `/stop` and went away by itself.
        let report = registry.stop("p_test").await.unwrap();
        assert_eq!(report.outcome, StopOutcome::Graceful);
        assert!(registry.session_of("p_test").await.is_none());
        assert_eq!(
            events.down_reasons(),
            vec!["stopped"],
            "exactly one transition, from the stop itself"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_refusing_daemon_keeps_its_own_words_and_its_stderr() {
        let directory = tempfile::tempdir().unwrap();
        let binary = scripted_daemon(
            directory.path(),
            r#"echo '{"ok":false,"error":{"code":"port_conflict","message":"7978 serves another project"}}'
echo 'wsync: refusing to bind' >&2
exit 1"#,
        );
        let (events, registry) = registry_for(binary);

        let error = registry
            .start("p_bad", "/tmp/project", directory.path(), None)
            .await
            .unwrap_err();
        assert_eq!(error.code, crate::error::code::DAEMON);
        assert!(
            error.message.contains("7978 serves another project"),
            "the daemon's own message must survive: {}",
            error.message
        );
        assert_eq!(events.up_count(), 0, "a refusal is not a session");
        assert!(registry.session_of("p_bad").await.is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn an_adopted_daemon_is_never_killed() {
        let directory = tempfile::tempdir().unwrap();
        let binary = scripted_daemon(
            directory.path(),
            r#"echo '{"ok":true,"alreadyRunning":true,"port":7999,"pid":4242,"bootId":"other-boot","project":"/p","canonicalProject":"/p"}'
exit 0"#,
        );
        let (_events, registry) = registry_for(binary);

        let session = registry
            .start("p_adopted", "/p", directory.path(), None)
            .await
            .unwrap();
        assert!(session.already_running);
        assert!(!session.managed, "an adopted daemon is not ours to kill");
        assert_eq!(session.pid, 4242, "the pid is the daemon's, not the launcher's");

        // Nothing is listening on 7999, so `/stop` fails — and because we hold
        // no handle there is nothing to fall back to. It must not become a kill.
        let error = registry.stop("p_adopted").await.unwrap_err();
        assert_eq!(error.code, crate::error::code::DAEMON);
        assert!(registry.session_of("p_adopted").await.is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_daemon_that_never_reports_itself_is_not_a_session() {
        let directory = tempfile::tempdir().unwrap();
        let binary = scripted_daemon(directory.path(), "echo 'Compiling wsync v0.1.0' >&2\nexit 3");
        let (events, registry) = registry_for(binary);

        let error = registry
            .start("p_quiet", "/p", directory.path(), None)
            .await
            .unwrap_err();
        assert_eq!(error.code, crate::error::code::DAEMON);
        assert!(error.message.contains("exit code 3"), "{}", error.message);
        assert!(
            error.message.contains("Compiling wsync"),
            "stderr is the only diagnostic there is: {}",
            error.message
        );
        assert_eq!(events.up_count(), 0);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_owner_token_reaches_the_child_only_through_the_environment() {
        let directory = tempfile::tempdir().unwrap();
        let recorded = directory.path().join("argv.txt");
        let binary = scripted_daemon(
            directory.path(),
            &format!(
                r#"printf '%s\n' "$@" > {recorded}
printf 'env:%s\n' "$WSYNC_OWNER_TOKEN" >> {recorded}
echo '{{"ok":true,"port":7978,"pid":1,"bootId":"b","project":"/p","canonicalProject":"/p"}}'
exec sleep 30"#,
                recorded = recorded.display()
            ),
        );
        let (_events, registry) = registry_for(binary);

        registry
            .start("p_argv", "/p", directory.path(), Some(7981))
            .await
            .unwrap();
        // Let the script finish writing before reading it back.
        time::sleep(Duration::from_millis(250)).await;
        let transcript = std::fs::read_to_string(&recorded).unwrap();
        let argv: Vec<&str> = transcript.lines().collect();

        assert_eq!(
            &argv[..argv.len() - 1],
            [
                "daemon",
                "start",
                "--project",
                "/p",
                "--port",
                "7981",
                "--managed-by",
                "desktop",
                "--owner-token-env",
                OWNER_TOKEN_ENV,
                "--data-dir",
                &directory.path().to_string_lossy(),
                "--raw",
            ],
            "argv must match the spawn contract exactly"
        );

        let token = argv.last().unwrap().strip_prefix("env:").unwrap();
        assert_eq!(token.len(), 64, "the child gets the token in the environment");
        assert!(
            !transcript.contains(&format!("--owner-token {token}")),
            "the token value must never appear in argv"
        );

        registry.shutdown_all().await;
    }

    #[tokio::test]
    async fn shutdown_all_clears_the_registry() {
        let Some(shim) = mock_shim() else { return };
        let directory = tempfile::tempdir().unwrap();
        let project = directory.path().to_string_lossy().into_owned();
        let (_events, registry) = registry_for(shim);

        registry
            .start("p_exit", &project, directory.path(), None)
            .await
            .unwrap();
        registry.shutdown_all().await;
        assert!(
            registry.session_of("p_exit").await.is_none(),
            "app exit must leave no tracked daemon behind"
        );
    }
}
