//! The project broker (Design 8.4): Studio-triggered project creation.
//!
//! A loopback HTTP/1.1 listener on the first free port of `7968–7971`, running
//! **only while a Projects folder is authorized in Settings**. It answers two
//! routes and nothing else:
//!
//! * `GET  /hello`          → `{broker, projectInit, version}` — how the
//!   plugin's port scan tells a broker from a daemon (Design 3.2).
//! * `POST /projects/init`  → `{ok, slug, status}` — the "Create Project" click.
//!   The hello also carries `games`, the GameIds of registered projects, so
//!   the plugin can offer "Serve Project" instead of a create for a game the
//!   app already has.
//!
//! It is written directly over `tokio::net` in the style of `loopback.rs`, for
//! the same reasons that module gives: the requirement is two routes and a
//! 16 KiB JSON body on a socket that never leaves the machine, and a general
//! HTTP stack would bring a TLS story, a connection pool and a feature-flag
//! surface that none of this can use.
//!
//! ## What the broker refuses to be told
//!
//! Design 11 is explicit: Studio-triggered creation "never accepts a filesystem
//! path from Studio". So the request carries Roblox metadata only, the slug is
//! **derived here**, and a request that so much as mentions a path-shaped field
//! is refused rather than sanitized — a client that thinks it can choose the
//! directory should learn that it cannot, not have its choice silently dropped.
//!
//! Everything the broker writes lands as **exactly one direct child** of the
//! authorized folder:
//!
//! * the candidate name is a single component of `[a-z0-9-]`, generated here;
//! * a name already taken by a **symlink** is refused outright, not suffixed
//!   around — following it would write outside the authorized folder;
//! * the directory is created with `create_dir` (fails if anything is there)
//!   and then re-canonicalized and checked to still be under the root, so a
//!   race that swapped it for a link is caught before a byte is written;
//! * every file inside is written with `create_new` (`templates.rs`).
//!
//! ## Idempotency
//!
//! Two separate rules, because they answer two different accidents:
//!
//! * **`requestId` replay** — the same click retried (a dropped response, a
//!   plugin retry) returns the original answer verbatim, creating nothing.
//! * **`gameId` dedupe** — a *different* click for a place that already has a
//!   project answers `status: "existing"` for that project rather than minting
//!   a sibling. This is the rule that survives an app restart, since the ledger
//!   behind the first one is per-run memory and the registry is on disk.
//!
//! Either way the project is (re-)served afterwards, because the plugin's next
//! move is to re-scan `7978–7990` for a daemon claiming its GameId.
//!
//! ## Naming (Design 7.0)
//!
//! The plugin's `placeName` is authoritative: it resolves the real published
//! place name through MarketplaceService (edit-mode-correct), so `Place3` — the
//! internal file name Studio used to send — no longer reaches this side. That
//! name drives the slug, the display name and the project file's `name`. Only
//! when the request carries no usable `placeName` (an unpublished place on a
//! very old plugin) does the broker ask the public games API what the
//! experience is called — `GET /v1/games?universeIds=<gameId>`. The chain is:
//! **the `placeName` the plugin sent → games-API name → `place-<gameId>`**.
//!
//! The lookup is the one place this host talks to the internet. It is bounded
//! (3 s, capped body), cached per GameId for the broker's lifetime *including
//! its failures*, taken **before** the create lock so an offline machine pays
//! the timeout once and never while holding anything another request wants —
//! and skipped entirely when the plugin already named the place.

use std::{
    collections::{HashMap, VecDeque},
    fs,
    net::Ipv4Addr,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use tauri::{AppHandle, Emitter as _, Manager as _};
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::{TcpListener, TcpStream},
    sync::{oneshot, Mutex, Semaphore},
    task::JoinHandle,
    time,
};

use crate::{
    error::{HostError, HostResult},
    storage,
    templates::{self, ProjectFacts},
};

// -------------------------------------------------------------- constants --

/// Design 3.2: the broker's own range, disjoint from the daemons' `7978–7990`
/// and from both parents' ranges.
pub(crate) const BROKER_PORTS: [u16; 4] = [7968, 7969, 7970, 7971];

/// Request line plus headers. A plugin's POST is a few hundred bytes.
const MAX_HEAD: usize = 8 * 1024;

/// The pinned body cap. `/projects/init` carries names and ids; anything larger
/// is not the contract's request.
const MAX_BODY: usize = 16 * 1024;

/// How much of an over-cap body is read off the socket before the 413 goes
/// out. Enough to answer a mistake politely, not enough to be a sink.
const DRAIN_LIMIT: usize = 1024 * 1024;

/// Total budget for one connection, read to write. Loopback and tiny.
const CONNECTION_TIMEOUT: Duration = Duration::from_secs(10);

/// Concurrent requests actually being worked on. The listener still accepts;
/// this bounds the work, and each unit of work is bounded by `MAX_BODY`.
const MAX_IN_FLIGHT: usize = 16;

/// How long `stop` waits for the accept loop to drop the listener before it
/// aborts the task. Bounded so clearing a folder can never hang the UI.
const STOP_TIMEOUT: Duration = Duration::from_secs(2);

/// Replayable `requestId`s kept per run. A plugin retries within seconds, so
/// this is generous; the `gameId` rule is what covers the long tail.
const LEDGER_LIMIT: usize = 256;

/// Slug length before the collision suffix. Long enough to stay readable, short
/// enough to keep the whole path inside every platform's limits.
const MAX_SLUG_LENGTH: usize = 48;

/// `slug`, `slug-2` … `slug-64`. A folder with 64 same-named projects in it is
/// not a collision any more, it is a mistake worth reporting.
const MAX_SLUG_ATTEMPTS: u32 = 64;

/// Project display name, in the project file and the registry.
const MAX_NAME_LENGTH: usize = 64;

// ------------------------------------------------------------- games API 7.0 --

/// Design 7.0's naming source: the public games endpoint, which answers
/// `{data:[{id,name,…}]}` for a universe (`GameId`) id.
const GAMES_API_BASE: &str = "https://games.roblox.com";

/// The one supported override of that base, and the only reason it exists: a
/// test points it at a local stub so the naming rules can be checked without
/// reaching the internet. Pinned here, beside the default it replaces.
const GAMES_API_BASE_ENV: &str = "WSYNC_GAMES_API_BASE";

/// Design 7.0's "short timeout". The name is a nicety; creating the project is
/// not, and it must not wait on a slow or blackholed endpoint.
const GAMES_API_TIMEOUT: Duration = Duration::from_secs(3);

/// One universe's record is a few hundred bytes. Read at most this much before
/// giving up on whatever is at the other end.
const GAMES_API_MAX_BODY: usize = 64 * 1024;

/// Longest experience name accepted from the network. Both consumers bound
/// themselves anyway (48 for the slug, 64 for the display name); this stops an
/// absurd answer at the door.
const MAX_REMOTE_NAME_LENGTH: usize = 200;

const REQUEST_ID_MIN: usize = 8;
const REQUEST_ID_MAX: usize = 128;

/// Names Windows cannot host as a directory, whatever the filesystem says.
const RESERVED_NAMES: &[&str] = &[
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
    "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

/// Fields that would mean "put it here". None of them is ever honoured, and
/// none is silently ignored either (Design 11).
const FORBIDDEN_FIELDS: &[&str] = &[
    "path",
    "projectpath",
    "project_path",
    "dir",
    "directory",
    "folder",
    "root",
    "projectsroot",
    "target",
    "destination",
    "location",
    "slug",
    "file",
    "filename",
];

// -------------------------------------------------------- serialized types --

/// What Settings shows and `broker_status` returns.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BrokerStatus {
    pub(crate) running: bool,
    pub(crate) port: Option<u16>,
    pub(crate) root: Option<String>,
    /// One sentence for the status line: why it is up, or why it is not.
    pub(crate) detail: String,
}

impl BrokerStatus {
    fn off(detail: impl Into<String>) -> Self {
        Self {
            running: false,
            port: None,
            root: None,
            detail: detail.into(),
        }
    }

    /// A folder *is* authorized but the listener is not up — all four ports
    /// busy, most likely. Settings shows the reason rather than a bare "off",
    /// because the user has already done their half of it.
    pub(crate) fn stopped_because(reason: impl std::fmt::Display) -> Self {
        Self::off(format!("Off — {reason}."))
    }
}

/// `project-init`, emitted once per project the broker actually merges.
///
/// `requestId`, `slug` and `projectId` are the three pinned fields; `status`
/// and the whole `project` record ride along so the Projects view can merge a
/// new card without re-reading `state.json`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProjectInitEvent {
    pub(crate) request_id: String,
    pub(crate) slug: String,
    pub(crate) project_id: String,
    /// `created` · `existing`.
    pub(crate) status: &'static str,
    pub(crate) game_id: u64,
    /// The Design 8.3 registry record, exactly as it was written.
    pub(crate) project: Value,
}

// ------------------------------------------------------------------ events --

/// Where the broker's transitions go, and how it reaches the daemon registry.
///
/// A trait for the same reason `DaemonEvents` is one: the interesting paths —
/// binding, creating, merging, refusing — are worth testing against a real
/// socket and a real filesystem, and a trait means those tests need no Tauri
/// runtime at all.
pub(crate) trait BrokerEvents: Send + Sync + 'static {
    fn up(&self, status: &BrokerStatus);
    fn down(&self, status: &BrokerStatus);
    fn project_init(&self, event: &ProjectInitEvent);
    /// Auto-serve (Design 8.4). Fire-and-forget: the broker's answer must not
    /// wait on a daemon spawn, and the plugin re-scans for the port anyway.
    fn serve(&self, project_id: &str, project_path: &str);
}

impl BrokerEvents for AppHandle {
    fn up(&self, status: &BrokerStatus) {
        let _ = self.emit("broker:up", status);
    }

    fn down(&self, status: &BrokerStatus) {
        let _ = self.emit("broker:down", status);
    }

    fn project_init(&self, event: &ProjectInitEvent) {
        let _ = self.emit("project-init", event);
    }

    fn serve(&self, project_id: &str, project_path: &str) {
        // The same call `daemon_start` makes — the spawn contract, the owner
        // token and the kill-on-exit handle all stay in `daemon.rs`.
        let handle = self.clone();
        let project_id = project_id.to_string();
        let project_path = project_path.to_string();
        tauri::async_runtime::spawn(async move {
            let Some(state) = handle.try_state::<crate::AppState>() else {
                return;
            };
            let data_dir = state.paths.data_dir.clone();
            if let Err(error) = state
                .daemons
                .start(&project_id, &project_path, &data_dir, None)
                .await
            {
                // `daemon:up` is how a success reaches the UI; a failure has no
                // session to report, so it goes out as the same `daemon:down`
                // shape the registry uses for every other lifecycle loss.
                let _ = handle.emit(
                    "daemon:down",
                    crate::daemon::DownEvent {
                        project_id,
                        reason: "serve-failed".to_string(),
                        detail: error.message,
                        boot_id: None,
                    },
                );
            }
        });
    }
}

// ------------------------------------------------------------------ broker --

struct Running {
    port: u16,
    root: PathBuf,
    shutdown: Option<oneshot::Sender<()>>,
    task: JoinHandle<()>,
}

#[derive(Debug, Clone)]
struct LedgerEntry {
    request_id: String,
    /// The root the answer was about. A replay after the folder changed is not
    /// a replay — the slug it names lives somewhere else now.
    root: PathBuf,
    outcome: InitOutcome,
}

#[derive(Debug, Clone)]
struct InitOutcome {
    slug: String,
    status: &'static str,
    project_id: String,
    project_path: String,
}

pub(crate) struct Broker {
    events: Arc<dyn BrokerEvents>,
    state_file: PathBuf,
    /// Shared with `AppState` so a broker merge and a webview `state_set`
    /// cannot interleave two read-modify-writes on `state.json`.
    state_lock: Arc<Mutex<()>>,
    running: Mutex<Option<Running>>,
    /// Why the broker is not listening, for the Settings status line. "No
    /// folder is authorized" and "the folder is authorized but all four ports
    /// are busy" are very different things to be told.
    ///
    /// A `std` lock on purpose: it holds one short string and is never held
    /// across an await, so the async mutex would buy nothing.
    off_detail: std::sync::Mutex<String>,
    /// Serializes slug resolution → creation → merge. Two clicks in two open
    /// places must not both decide the name `my-game` is free.
    create_lock: Mutex<()>,
    ledger: Mutex<VecDeque<LedgerEntry>>,
    /// Design 7.0's experience names, one answer per GameId for the broker's
    /// lifetime — *including* the misses, so a machine with no network pays the
    /// 3 s timeout once rather than on every click.
    game_names: Mutex<HashMap<u64, Option<String>>>,
    /// Where those names are asked for. Snapshotted at construction from
    /// `WSYNC_GAMES_API_BASE`, so one process cannot have two answers, and
    /// overridable in tests without touching a process-wide environment.
    games_base: std::sync::Mutex<String>,
}

impl Broker {
    pub(crate) fn new(
        events: Arc<dyn BrokerEvents>,
        state_file: PathBuf,
        state_lock: Arc<Mutex<()>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            events,
            state_file,
            state_lock,
            running: Mutex::new(None),
            off_detail: std::sync::Mutex::new(no_root_detail().to_string()),
            create_lock: Mutex::new(()),
            ledger: Mutex::new(VecDeque::new()),
            game_names: Mutex::new(HashMap::new()),
            games_base: std::sync::Mutex::new(games_api_base()),
        })
    }

    // ---------------------------------------------------------- lifecycle --

    /// Start listening for `root`, or move an already-running broker to it.
    pub(crate) async fn start(self: &Arc<Self>, root: &Path) -> HostResult<BrokerStatus> {
        self.start_on(root, &BROKER_PORTS).await
    }

    async fn start_on(self: &Arc<Self>, root: &Path, ports: &[u16]) -> HostResult<BrokerStatus> {
        let root = authorized_root(root).inspect_err(|error| {
            self.remember_off(format!("Off — {}.", error.message));
        })?;

        let mut guard = self.running.lock().await;
        if let Some(active) = guard.as_ref() {
            if active.root == root {
                return Ok(status_of(Some(active), ""));
            }
            // The authorized folder moved. Tear the old listener down first, so
            // the port is free before we ask for one.
            shutdown(guard.take()).await;
        }

        let (listener, port) = bind_first_free(ports).await.inspect_err(|error| {
            self.remember_off(format!("Off — {}.", error.message));
        })?;
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let task = tokio::spawn(accept_loop(
            Arc::clone(self),
            listener,
            root.clone(),
            shutdown_rx,
        ));
        *guard = Some(Running {
            port,
            root,
            shutdown: Some(shutdown_tx),
            task,
        });

        let status = status_of(guard.as_ref(), "");
        drop(guard);
        self.remember_off(no_root_detail().to_string());
        self.events.up(&status);
        Ok(status)
    }

    fn remember_off(&self, detail: String) {
        if let Ok(mut current) = self.off_detail.lock() {
            *current = detail;
        }
    }

    fn off_detail(&self) -> String {
        self.off_detail
            .lock()
            .map(|detail| detail.clone())
            .unwrap_or_else(|_| no_root_detail().to_string())
    }

    /// Stop listening. Idempotent, and bounded: a broker that will not go away
    /// is aborted rather than allowed to hold the UI.
    pub(crate) async fn stop(&self, reason: &str) -> BrokerStatus {
        let was_running = {
            let mut guard = self.running.lock().await;
            guard.take()
        };
        let running = was_running.is_some();
        shutdown(was_running).await;

        // The reason replaces whatever the last "off" reason was, whether or
        // not a listener was there to stop: clearing the folder must not leave
        // Settings still explaining a bind failure from before.
        self.remember_off(format!("Off — {reason}."));
        let status = BrokerStatus::off(self.off_detail());
        // ...but only a real transition is an event.
        if running {
            self.events.down(&status);
        }
        status
    }

    pub(crate) async fn status(&self) -> BrokerStatus {
        let running = self.running.lock().await;
        status_of(running.as_ref(), &self.off_detail())
    }

    // ------------------------------------------------------------ requests --

    /// One connection: read a request, answer it, hang up.
    async fn serve_connection(&self, mut stream: TcpStream, root: &Path) {
        stream.set_nodelay(true).ok();
        let reply = match read_request(&mut stream).await {
            Ok(request) => self.route(request, root).await,
            Err(reply) => reply,
        };
        let _ = stream.write_all(&reply.encode()).await;
        let _ = stream.flush().await;
        let _ = stream.shutdown().await;
    }

    async fn route(&self, request: RawRequest, root: &Path) -> HttpReply {
        // A native client — Studio's HttpService, the CLI, curl — carries no
        // `Origin`. A browser page cannot avoid sending one, so refusing every
        // request that has one closes the cross-site path to a loopback port
        // without needing a token the plugin has no way to hold (Design 11).
        if request.origin.is_some() {
            return HttpReply::error(
                403,
                "the broker does not accept browser-origin requests; it is a loopback surface for Studio",
            );
        }

        match (request.method.as_str(), request.path.as_str()) {
            ("GET", "/hello") => {
                let games = registered_game_ids(&self.read_registry().await);
                HttpReply::json(200, hello_document(&games))
            }
            ("POST", "/projects/init") => self.init(request, root).await,
            ("POST", "/hello") => HttpReply::error(405, "/hello is a GET"),
            ("GET", "/projects/init") => HttpReply::error(405, "/projects/init is a POST"),
            (_, path) => HttpReply::error(
                404,
                format!("{path} is not a broker route (try GET /hello, POST /projects/init)"),
            ),
        }
    }

    // -------------------------------------------------------------- create --

    // ---------------------------------------------------------- the name --

    /// Point the name lookup somewhere else. Tests only: they run against a
    /// local stub, and the alternative — a process-wide environment variable
    /// mutated from several threads — would make them race each other.
    #[cfg(test)]
    fn set_games_base(&self, base: impl Into<String>) {
        if let Ok(mut current) = self.games_base.lock() {
            *current = base.into();
        }
    }

    /// What the *experience* is called per the games API, cached per GameId.
    /// The fallback link of the naming chain — only consulted when the plugin
    /// sent no usable `placeName`.
    ///
    /// Deliberately outside every lock the create path takes. The lookup is the
    /// slowest thing in `/projects/init` and the least important — a second
    /// window creating a different project must not queue behind it, and each
    /// connection is its own task, so the await here holds nothing at all.
    async fn resolve_game_name(&self, game_id: u64) -> Option<String> {
        if let Some(known) = self.game_names.lock().await.get(&game_id) {
            return known.clone();
        }

        let base = self
            .games_base
            .lock()
            .map(|base| base.clone())
            .unwrap_or_else(|_| GAMES_API_BASE.to_string());
        // Two clicks for the same game can race to the same answer. Harmless:
        // the request is idempotent and the second write stores the same name.
        let resolved = fetch_game_name(&base, game_id).await;

        self.game_names.lock().await.insert(game_id, resolved.clone());
        resolved
    }

    async fn init(&self, request: RawRequest, root: &Path) -> HttpReply {
        let parsed = match parse_init(&request.body) {
            Ok(parsed) => parsed,
            Err(reply) => return reply,
        };

        // The plugin's `placeName` is authoritative (it is the published name,
        // resolved through MarketplaceService), so the games API is only asked
        // when the request did not carry one. When it *is* asked, it is asked
        // before anything is locked or written, so both the slug and the
        // display name come from the same answer — and a replayed request
        // re-reads it from the cache.
        let resolved = if parsed.has_place_name() {
            None
        } else {
            self.resolve_game_name(parsed.game_id).await
        };

        match self.create_or_adopt(&parsed, resolved.as_deref(), root).await {
            Ok(outcome) => HttpReply::json(
                200,
                json!({ "ok": true, "slug": outcome.slug, "status": outcome.status }),
            ),
            Err(error) => HttpReply::error(error.status, error.message),
        }
    }

    /// `resolved` is the games API's answer for this GameId — the fallback link
    /// of the naming chain, only ever fetched (and so only ever `Some`) when
    /// the request carried no usable `placeName`.
    async fn create_or_adopt(
        &self,
        request: &InitRequest,
        resolved: Option<&str>,
        root: &Path,
    ) -> Result<InitOutcome, InitError> {
        // Everything from here to the registry write is one critical section:
        // the collision scan is only meaningful if nothing else can create a
        // sibling between the scan and the `create_dir`.
        let _create = self.create_lock.lock().await;

        // 1. The same click, retried. Answer exactly what it was answered
        //    before — and re-serve, because the reason it retried may be that
        //    the daemon never came up.
        if let Some(outcome) = self.replay(&request.request_id, root).await {
            self.events.serve(&outcome.project_id, &outcome.project_path);
            return Ok(outcome);
        }

        let document = self.read_registry().await;

        // 2. A different click for a place WSync already has a project for.
        //    This is the rule that outlives a restart.
        if let Some(outcome) = existing_project(&document, request.game_id) {
            self.remember(&request.request_id, root, &outcome).await;
            self.events.serve(&outcome.project_id, &outcome.project_path);
            return Ok(outcome);
        }

        // 3. A new project. The slug is ours; the request never named one.
        let base = derive_slug(name_source(request, resolved), request.game_id);
        let (slug, directory) = choose_directory(root, &base)?;

        fs::create_dir(&directory).map_err(|error| {
            InitError::new(
                if error.kind() == std::io::ErrorKind::AlreadyExists {
                    409
                } else {
                    500
                },
                format!("could not create the project folder: {error}"),
            )
        })?;

        // The one-direct-child rule, verified after the fact as well as before:
        // if anything about that path resolves outside the authorized folder,
        // undo it before a single file is written.
        if let Err(error) = verify_inside(root, &directory) {
            let _ = fs::remove_dir(&directory);
            return Err(error);
        }

        let facts = ProjectFacts {
            name: display_name(request, resolved, &slug),
            game_id: request.game_id,
            place_ids: request.place_ids(),
            group_id: request.group_id(),
        };
        if let Err(error) = templates::write_scaffold(&directory, &facts) {
            // A half-written project is worse than none.
            let _ = fs::remove_dir_all(&directory);
            return Err(InitError::new(500, error.message));
        }

        let project_id = new_project_id();
        let project_path = directory.to_string_lossy().into_owned();
        let record = registry_record(&project_id, &facts, &project_path);

        if let Err(error) = self.merge_project(&record).await {
            let _ = fs::remove_dir_all(&directory);
            return Err(InitError::new(500, error.message));
        }

        let outcome = InitOutcome {
            slug: slug.clone(),
            status: "created",
            project_id: project_id.clone(),
            project_path: project_path.clone(),
        };
        self.remember(&request.request_id, root, &outcome).await;

        self.events.project_init(&ProjectInitEvent {
            request_id: request.request_id.clone(),
            slug,
            project_id: project_id.clone(),
            status: "created",
            game_id: request.game_id,
            project: record,
        });
        self.events.serve(&project_id, &project_path);

        Ok(outcome)
    }

    // ------------------------------------------------------------ registry --

    async fn read_registry(&self) -> Map<String, Value> {
        let _guard = self.state_lock.lock().await;
        // A corrupt state file must not stop Studio from creating a project:
        // `merge_document` rebuilds from defaults on the next write, and the
        // bad bytes are still reported to every reader that asks for them.
        storage::read_document(&self.state_file).unwrap_or_else(|_| storage::default_document())
    }

    /// Append the new project to `projects[]` and replace `state.json` atomically.
    ///
    /// The document is re-read under the lock rather than trusting the copy the
    /// caller already has: the webview may have written between the two.
    async fn merge_project(&self, record: &Value) -> HostResult<()> {
        let _guard = self.state_lock.lock().await;
        let document = storage::read_document(&self.state_file)
            .unwrap_or_else(|_| storage::default_document());
        let mut projects = document
            .get("projects")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        projects.push(record.clone());

        let mut patch = Map::new();
        patch.insert("projects".into(), Value::Array(projects));
        storage::merge_document(&self.state_file, patch).map(|_| ())
    }

    // -------------------------------------------------------------- ledger --

    async fn replay(&self, request_id: &str, root: &Path) -> Option<InitOutcome> {
        self.ledger
            .lock()
            .await
            .iter()
            .find(|entry| entry.request_id == request_id && entry.root == root)
            .map(|entry| entry.outcome.clone())
    }

    async fn remember(&self, request_id: &str, root: &Path, outcome: &InitOutcome) {
        let mut ledger = self.ledger.lock().await;
        if ledger.len() >= LEDGER_LIMIT {
            ledger.pop_front();
        }
        ledger.push_back(LedgerEntry {
            request_id: request_id.to_string(),
            root: root.to_path_buf(),
            outcome: outcome.clone(),
        });
    }
}

/// The listener's whole life. Ends when `stop` fires the channel — which is
/// also when the `TcpListener` drops and the port becomes free again.
async fn accept_loop(
    broker: Arc<Broker>,
    listener: TcpListener,
    root: PathBuf,
    mut shutdown: oneshot::Receiver<()>,
) {
    let limiter = Arc::new(Semaphore::new(MAX_IN_FLIGHT));
    loop {
        let accepted = tokio::select! {
            _ = &mut shutdown => break,
            accepted = listener.accept() => accepted,
        };

        let (stream, peer) = match accepted {
            Ok(accepted) => accepted,
            Err(_error) => {
                // Never spin on a failing accept; a descriptor limit would
                // otherwise burn a core until it cleared.
                time::sleep(Duration::from_millis(50)).await;
                continue;
            }
        };
        // The socket is bound to 127.0.0.1, so this can only fail if the OS
        // lied. Checking anyway costs nothing and states the invariant.
        if !peer.ip().is_loopback() {
            continue;
        }

        let broker = Arc::clone(&broker);
        let root = root.clone();
        let limiter = Arc::clone(&limiter);
        tokio::spawn(async move {
            let Ok(_permit) = limiter.acquire_owned().await else {
                return;
            };
            let _ = time::timeout(
                CONNECTION_TIMEOUT,
                broker.serve_connection(stream, &root),
            )
            .await;
        });
    }
}

async fn shutdown(running: Option<Running>) {
    let Some(mut running) = running else { return };
    if let Some(signal) = running.shutdown.take() {
        let _ = signal.send(());
    }
    // Wait for the loop to drop the listener, so the port is genuinely free by
    // the time this returns — then abort rather than hang.
    if time::timeout(STOP_TIMEOUT, &mut running.task).await.is_err() {
        running.task.abort();
    }
}

async fn bind_first_free(ports: &[u16]) -> HostResult<(TcpListener, u16)> {
    let mut last = None;
    for port in ports {
        match TcpListener::bind((Ipv4Addr::LOCALHOST, *port)).await {
            Ok(listener) => return Ok((listener, *port)),
            Err(error) => last = Some(error),
        }
    }
    Err(HostError::unavailable(format!(
        "no free broker port in {}{}",
        describe_ports(ports),
        last.map(|error| format!(" ({error})")).unwrap_or_default()
    )))
}

fn describe_ports(ports: &[u16]) -> String {
    match (ports.first(), ports.last()) {
        (Some(first), Some(last)) if first != last => format!("{first}–{last}"),
        (Some(only), _) => only.to_string(),
        _ => "none".to_string(),
    }
}

fn status_of(running: Option<&Running>, off_detail: &str) -> BrokerStatus {
    match running {
        Some(active) => BrokerStatus {
            running: true,
            port: Some(active.port),
            root: Some(active.root.to_string_lossy().into_owned()),
            detail: format!("Ready on port {}.", active.port),
        },
        None => BrokerStatus::off(if off_detail.is_empty() {
            no_root_detail()
        } else {
            off_detail
        }),
    }
}

fn no_root_detail() -> &'static str {
    "Off — authorize a folder."
}

/// The `/hello` document, exactly as pinned, plus the additive `games` list.
/// `broker: true` is what lets the plugin tell this apart from a daemon on a
/// neighbouring port; `games` names the GameIds of registered projects so the
/// plugin can phrase the no-daemon flow as "Serve Project" instead of a
/// create offer — `/projects/init` for one of these adopts and serves the
/// existing project (the existing-gameId rule) rather than creating anything.
fn hello_document(games: &[u64]) -> Value {
    json!({
        "broker": true,
        "projectInit": true,
        "version": env!("CARGO_PKG_VERSION"),
        "games": games,
    })
}

/// The GameIds the registry currently holds a usable project for, in registry
/// order. The same eligibility rule as `existing_project`: a record whose
/// directory is gone (or whose gameId is 0 — unpublished places share that
/// non-identity) is not advertised, because the init it invites would not
/// adopt it.
fn registered_game_ids(document: &Map<String, Value>) -> Vec<u64> {
    let mut games: Vec<u64> = Vec::new();
    let Some(projects) = document.get("projects").and_then(Value::as_array) else {
        return games;
    };
    for project in projects {
        let Some(game_id) = project.get("gameId").and_then(Value::as_u64) else {
            continue;
        };
        if game_id == 0 || games.contains(&game_id) {
            continue;
        }
        let has_directory = project
            .get("path")
            .and_then(Value::as_str)
            .map(|path| Path::new(path).is_dir())
            .unwrap_or(false);
        if has_directory {
            games.push(game_id);
        }
    }
    games
}

/// The authorized folder, resolved to a physical path.
///
/// Canonicalizing here is what makes every later check meaningful: the root is
/// symlink-free by construction, so "is the target still under the root" is a
/// prefix comparison rather than a walk.
fn authorized_root(root: &Path) -> HostResult<PathBuf> {
    let canonical = fs::canonicalize(root).map_err(|error| {
        HostError::io(format!(
            "the authorized projects folder {} could not be resolved: {error}",
            root.display()
        ))
    })?;
    if !canonical.is_dir() {
        return Err(HostError::invalid_argument(format!(
            "{} is not a folder",
            canonical.display()
        )));
    }
    Ok(canonical)
}

// ------------------------------------------------------------- the request --

/// What Studio sends. Unknown fields are tolerated so the plugin can grow —
/// which is what happened to `gameName`, which the broker no longer reads.
///
/// `placeName` is the published place name, resolved by the plugin through
/// MarketplaceService (the one API that answers correctly in edit mode), so it
/// is authoritative here — the `Place3`-shaped file names that once made it
/// untrustworthy no longer arrive. It heads the naming chain; the games API is
/// the fallback for a request without one.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct InitRequest {
    request_id: String,
    game_id: u64,
    #[serde(default)]
    place_id: Option<u64>,
    #[serde(default)]
    place_name: Option<String>,
    #[serde(default)]
    creator_type: Option<String>,
    #[serde(default)]
    creator_id: Option<u64>,
}

impl InitRequest {
    fn place_ids(&self) -> Vec<u64> {
        self.place_id.filter(|id| *id != 0).into_iter().collect()
    }

    /// True when the plugin named the place — the head of the naming chain,
    /// with the same "usable" bar `name_source` applies. When this holds, the
    /// games API is never consulted.
    fn has_place_name(&self) -> bool {
        self.place_name
            .as_deref()
            .is_some_and(|name| !name.trim().is_empty())
    }

    /// Design 4.3's `groupId`: creator context, and only when Studio said the
    /// creator *is* a group.
    fn group_id(&self) -> Option<u64> {
        let creator_type = self.creator_type.as_deref()?.trim();
        if !creator_type.eq_ignore_ascii_case("group") {
            return None;
        }
        self.creator_id.filter(|id| *id != 0)
    }
}

#[derive(Debug)]
struct InitError {
    status: u16,
    message: String,
}

impl InitError {
    fn new(status: u16, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
}

/// Strict parse: a JSON object, no path-shaped fields, typed and bounded values.
fn parse_init(body: &[u8]) -> Result<InitRequest, HttpReply> {
    if body.is_empty() {
        return Err(HttpReply::error(400, "the request had no body"));
    }

    let raw: Value = serde_json::from_slice(body)
        .map_err(|error| HttpReply::error(400, format!("the body is not JSON: {error}")))?;
    let Value::Object(fields) = &raw else {
        return Err(HttpReply::error(400, "the body must be a JSON object"));
    };

    // Design 11: a path never enters this way. Refused, not ignored — a client
    // that believes it chose the directory must find out that it did not.
    if let Some(offender) = fields
        .keys()
        .find(|key| FORBIDDEN_FIELDS.contains(&key.to_ascii_lowercase().as_str()))
    {
        return Err(HttpReply::error(
            400,
            format!(
                "{offender:?} is not accepted: WSync derives the folder itself and never takes a path from Studio"
            ),
        ));
    }

    let request: InitRequest = serde_json::from_value(raw).map_err(|error| {
        HttpReply::error(400, format!("the body is not a project-init request: {error}"))
    })?;

    let id = request.request_id.trim();
    if id.len() < REQUEST_ID_MIN
        || id.len() > REQUEST_ID_MAX
        || !id.chars().all(|c| c.is_ascii_hexdigit())
    {
        return Err(HttpReply::error(
            400,
            format!("requestId must be {REQUEST_ID_MIN}–{REQUEST_ID_MAX} hex characters"),
        ));
    }
    if request.game_id == 0 {
        return Err(HttpReply::error(400, "gameId is required"));
    }

    Ok(InitRequest {
        request_id: id.to_ascii_lowercase(),
        ..request
    })
}

// -------------------------------------------------------------- the name --

/// The naming chain, minus its last link: the `placeName` the plugin sent —
/// authoritative, because the plugin resolves the published name through
/// MarketplaceService — then the games API's experience name. When both are
/// missing the callers supply the tail — `place-<gameId>` for the slug, the
/// slug for the display name.
fn name_source<'a>(request: &'a InitRequest, resolved: Option<&'a str>) -> Option<&'a str> {
    [request.place_name.as_deref(), resolved]
        .into_iter()
        .flatten()
        .find(|name| !name.trim().is_empty())
}

/// `GET <base>/v1/games?universeIds=<gameId>` → the experience's name.
///
/// Every failure is the same answer — `None` — because they all mean the same
/// thing to the caller: name the project from what Studio said instead. No
/// retry, no error surfaced to the plugin: a project whose folder is
/// `place-123` is a working project, and a creation that failed because a name
/// lookup did is not.
async fn fetch_game_name(base: &str, game_id: u64) -> Option<String> {
    let url = format!(
        "{}/v1/games?universeIds={game_id}",
        base.trim_end_matches('/')
    );
    install_crypto_provider();

    // Built per call rather than shared: this runs once per GameId for the life
    // of the process, so a pooled client would hold idle connections to a host
    // nothing else here talks to.
    let client = reqwest::Client::builder()
        .timeout(GAMES_API_TIMEOUT)
        .user_agent(concat!("WSync-Desktop/", env!("CARGO_PKG_VERSION")))
        .build()
        .ok()?;

    let mut response = client
        .get(url)
        .header("accept", "application/json")
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }

    // Read in chunks with a ceiling rather than `bytes()`: the timeout already
    // bounds the wait, and this bounds the memory whatever the other end sends.
    let mut body = Vec::new();
    while let Ok(Some(chunk)) = response.chunk().await {
        if body.len() + chunk.len() > GAMES_API_MAX_BODY {
            return None;
        }
        body.extend_from_slice(&chunk);
    }

    game_name_from_body(&body, game_id)
}

/// The one shape this understands: `{data:[{id,name,…}]}`, and only the entry
/// whose `id` is the universe that was asked about. An answer about something
/// else is not an answer.
fn game_name_from_body(body: &[u8], game_id: u64) -> Option<String> {
    let document: Value = serde_json::from_slice(body).ok()?;
    let name = document
        .get("data")?
        .as_array()?
        .iter()
        .find(|entry| entry.get("id").and_then(Value::as_u64) == Some(game_id))?
        .get("name")?
        .as_str()?
        .trim();

    if name.is_empty() || name.chars().count() > MAX_REMOTE_NAME_LENGTH {
        return None;
    }
    Some(name.to_string())
}

/// Make sure rustls has a process-wide crypto provider before a TLS client is
/// built.
///
/// `reqwest`'s `rustls-no-provider` feature — the one `tauri-plugin-updater`
/// already unified into this binary — compiles no default in, so
/// `ClientBuilder::build()` fails for want of one unless something installed it
/// first. The updater does exactly this before its own request; doing it here
/// too means a project creation never depends on an update check having run.
/// Idempotent, and a lost race is fine: whoever won installed the same ring
/// provider.
fn install_crypto_provider() {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }
}

/// The games API's base for this process: the environment override when it is
/// set to something usable, the public endpoint otherwise.
fn games_api_base() -> String {
    std::env::var(GAMES_API_BASE_ENV)
        .ok()
        .map(|base| base.trim().to_string())
        .filter(|base| !base.is_empty())
        .unwrap_or_else(|| GAMES_API_BASE.to_string())
}

// ------------------------------------------------------------------- slugs --

/// Design 8.4's derivation, in full: lowercase, `[a-z0-9-]`, runs collapsed,
/// trimmed, bounded, with `place-<gameId>` whenever nothing usable survives.
///
/// Non-ASCII is a separator rather than a transliteration: a Cyrillic or CJK
/// title becomes `place-<gameId>`, which is honest, and never a directory name
/// whose bytes depend on the filesystem's normalization.
pub(crate) fn derive_slug(source: Option<&str>, game_id: u64) -> String {
    let fallback = format!("place-{game_id}");
    let Some(source) = source else { return fallback };

    let mut slug = String::with_capacity(MAX_SLUG_LENGTH);
    let mut pending_separator = false;
    for character in source.chars() {
        let character = character.to_ascii_lowercase();
        if character.is_ascii_alphanumeric() {
            if pending_separator && !slug.is_empty() {
                slug.push('-');
            }
            pending_separator = false;
            if slug.len() >= MAX_SLUG_LENGTH {
                break;
            }
            slug.push(character);
        } else {
            pending_separator = true;
        }
    }

    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() || RESERVED_NAMES.contains(&slug.as_str()) {
        return fallback;
    }
    slug
}

/// The first free name, and the guards that make "one direct child" true.
fn choose_directory(root: &Path, base: &str) -> Result<(String, PathBuf), InitError> {
    debug_assert!(is_single_component(base), "derive_slug produced {base:?}");
    if !is_single_component(base) {
        return Err(InitError::new(500, "the derived folder name was not usable"));
    }

    for attempt in 1..=MAX_SLUG_ATTEMPTS {
        let candidate = if attempt == 1 {
            base.to_string()
        } else {
            format!("{base}-{attempt}")
        };
        let target = root.join(&candidate);

        // `symlink_metadata` is the no-follow probe: a symlink reports itself
        // rather than whatever it points at.
        match fs::symlink_metadata(&target) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok((candidate, target))
            }
            Err(error) => {
                return Err(InitError::new(
                    500,
                    format!("could not inspect {candidate}: {error}"),
                ))
            }
            Ok(metadata) if metadata.file_type().is_symlink() => {
                // Never suffix around this one. A link where the project should
                // be is the exact case Design 11 names, and stepping over it
                // quietly would leave it in place for the next request.
                return Err(InitError::new(
                    409,
                    format!(
                        "{candidate:?} in the projects folder is a symbolic link; \
                         WSync will not create or follow one. Remove or rename it and try again"
                    ),
                ));
            }
            Ok(_) => continue,
        }
    }

    Err(InitError::new(
        409,
        format!("{base:?} and {MAX_SLUG_ATTEMPTS} numbered variants are all taken"),
    ))
}

/// A name that is one path component and nothing else.
fn is_single_component(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_SLUG_LENGTH + 4
        && name != "."
        && name != ".."
        && name
            .chars()
            .all(|character| character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-')
}

/// After creation: the directory must still resolve to a direct child of the
/// authorized root.
fn verify_inside(root: &Path, directory: &Path) -> Result<(), InitError> {
    let resolved = fs::canonicalize(directory).map_err(|error| {
        InitError::new(
            500,
            format!("could not resolve the new project folder: {error}"),
        )
    })?;
    if resolved.parent() != Some(root) {
        return Err(InitError::new(
            409,
            "the new project folder did not resolve to a direct child of the authorized folder",
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------- registry --

/// A project already in the registry for this GameId, if its folder is still
/// there. A record pointing at a folder the user deleted is not a reason to
/// refuse to create a new one.
fn existing_project(document: &Map<String, Value>, game_id: u64) -> Option<InitOutcome> {
    // GameId 0 is every unpublished place at once, not an identity — adopting
    // by it would hand one place's project to a different place.
    if game_id == 0 {
        return None;
    }
    let projects = document.get("projects")?.as_array()?;
    for project in projects {
        if project.get("gameId").and_then(Value::as_u64) != Some(game_id) {
            continue;
        }
        let (Some(id), Some(path)) = (
            project.get("id").and_then(Value::as_str),
            project.get("path").and_then(Value::as_str),
        ) else {
            continue;
        };
        if !Path::new(path).is_dir() {
            continue;
        }
        let slug = Path::new(path)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| id.to_string());
        return Some(InitOutcome {
            slug,
            status: "existing",
            project_id: id.to_string(),
            project_path: path.to_string(),
        });
    }
    None
}

/// Design 8.3's project record, plus the `initializedFromStudio` marker.
fn registry_record(project_id: &str, facts: &ProjectFacts, path: &str) -> Value {
    json!({
        "id": project_id,
        "name": facts.name,
        "path": path,
        "addedAt": rfc3339(SystemTime::now()),
        "gameId": facts.game_id,
        "groupId": facts.group_id,
        "placeIds": facts.place_ids,
        "wallyEnabled": false,
        "wallyFolder": null,
        "wallyFile": null,
        "settings": {},
        "initializedFromStudio": true,
    })
}

/// The project's display name: the naming chain's answer (the plugin's
/// `placeName` first), tidied, or the slug when the chain ran out.
fn display_name(request: &InitRequest, resolved: Option<&str>, slug: &str) -> String {
    let Some(source) = name_source(request, resolved) else {
        return slug.to_string();
    };
    let mut name = String::with_capacity(MAX_NAME_LENGTH);
    let mut pending_space = false;
    for character in source.chars() {
        if character.is_whitespace() || character.is_control() {
            pending_space = !name.is_empty();
            continue;
        }
        if pending_space {
            if name.chars().count() >= MAX_NAME_LENGTH {
                break;
            }
            name.push(' ');
            pending_space = false;
        }
        if name.chars().count() >= MAX_NAME_LENGTH {
            break;
        }
        name.push(character);
    }
    if name.is_empty() {
        slug.to_string()
    } else {
        name
    }
}

/// Design 8.3: `p_<base36-time><rand4>`, generated the same way the frontend
/// does so a broker-created project is indistinguishable from an added one.
fn new_project_id() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis())
        .unwrap_or_default();
    let mut random = [0u8; 4];
    if getrandom::fill(&mut random).is_err() {
        random = (millis as u32).to_le_bytes();
    }
    let suffix = u32::from_le_bytes(random) % 36u32.pow(4);
    format!("p_{}{:0>4}", base36(millis), base36(u128::from(suffix)))
}

fn base36(mut value: u128) -> String {
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    if value == 0 {
        return "0".to_string();
    }
    let mut out = Vec::new();
    while value > 0 {
        out.push(DIGITS[(value % 36) as usize]);
        value /= 36;
    }
    out.reverse();
    String::from_utf8(out).unwrap_or_default()
}

/// `addedAt` in the same shape the frontend writes (`new Date().toISOString()`
/// to the second), without pulling a date crate in for one field.
fn rfc3339(now: SystemTime) -> String {
    let seconds = now
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default();
    let (year, month, day) = civil_from_days((seconds / 86_400) as i64);
    let time = seconds % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        time / 3600,
        (time % 3600) / 60,
        time % 60
    )
}

/// Days since the epoch → civil date (Howard Hinnant's `civil_from_days`).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    let era = if shifted >= 0 { shifted } else { shifted - 146_096 } / 146_097;
    let day_of_era = (shifted - era * 146_097) as u64;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era as i64 + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_position = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_position + 2) / 5 + 1) as u32;
    let month = if month_position < 10 {
        month_position + 3
    } else {
        month_position - 9
    } as u32;
    (year + i64::from(month <= 2), month, day)
}

// -------------------------------------------------------------- HTTP plumbing --

#[derive(Debug)]
struct RawRequest {
    method: String,
    /// Path only; a query string is stripped before routing.
    path: String,
    origin: Option<String>,
    body: Vec<u8>,
}

#[derive(Debug)]
struct HttpReply {
    status: u16,
    body: Vec<u8>,
}

impl HttpReply {
    fn json(status: u16, value: Value) -> Self {
        let body = serde_json::to_vec(&value).unwrap_or_else(|_| b"{}".to_vec());
        Self { status, body }
    }

    fn error(status: u16, message: impl Into<String>) -> Self {
        Self::json(status, json!({ "ok": false, "error": message.into() }))
    }

    fn encode(&self) -> Vec<u8> {
        let head = format!(
            "HTTP/1.1 {status} {reason}\r\n\
             Content-Type: application/json\r\n\
             Content-Length: {length}\r\n\
             Cache-Control: no-store\r\n\
             Connection: close\r\n\r\n",
            status = self.status,
            reason = reason_phrase(self.status),
            length = self.body.len(),
        );
        let mut out = head.into_bytes();
        out.extend_from_slice(&self.body);
        out
    }
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        411 => "Length Required",
        413 => "Payload Too Large",
        431 => "Request Header Fields Too Large",
        505 => "HTTP Version Not Supported",
        _ => "Internal Server Error",
    }
}

/// Read one request: head to `\r\n\r\n`, then exactly `Content-Length` bytes.
///
/// Every failure is a `HttpReply` rather than an error, so a malformed request
/// still gets a well-formed answer instead of a dropped connection.
async fn read_request(stream: &mut TcpStream) -> Result<RawRequest, HttpReply> {
    let mut buffer = Vec::with_capacity(1024);
    let mut chunk = [0u8; 2048];

    let head_end = loop {
        if let Some(at) = find(&buffer, b"\r\n\r\n") {
            break at;
        }
        if buffer.len() > MAX_HEAD {
            return Err(HttpReply::error(431, "the request headers were too large"));
        }
        let read = stream
            .read(&mut chunk)
            .await
            .map_err(|error| HttpReply::error(400, format!("could not read the request: {error}")))?;
        if read == 0 {
            return Err(HttpReply::error(
                400,
                "the connection closed before the request was complete",
            ));
        }
        buffer.extend_from_slice(&chunk[..read]);
    };

    let head = String::from_utf8_lossy(&buffer[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let request_line = lines
        .next()
        .ok_or_else(|| HttpReply::error(400, "the request had no request line"))?;
    let mut parts = request_line.split_whitespace();
    let method = parts
        .next()
        .ok_or_else(|| HttpReply::error(400, "the request had no method"))?
        .to_ascii_uppercase();
    let target = parts
        .next()
        .ok_or_else(|| HttpReply::error(400, "the request had no target"))?;
    if let Some(version) = parts.next() {
        if !version.starts_with("HTTP/1.") {
            return Err(HttpReply::error(505, "the broker speaks HTTP/1.1"));
        }
    }
    let path = target.split(['?', '#']).next().unwrap_or("/").to_string();

    let mut content_length: Option<usize> = None;
    let mut chunked = false;
    let mut expects_continue = false;
    let mut origin = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match name.trim().to_ascii_lowercase().as_str() {
            "content-length" => content_length = value.parse::<usize>().ok(),
            "transfer-encoding" => chunked |= value.to_ascii_lowercase().contains("chunked"),
            "expect" => expects_continue |= value.eq_ignore_ascii_case("100-continue"),
            "origin" => origin = Some(value.to_string()),
            _ => {}
        }
    }

    let mut body = buffer.split_off(head_end + 4);
    if method != "GET" && method != "HEAD" {
        if chunked {
            return Err(HttpReply::error(
                411,
                "the broker needs a Content-Length; chunked bodies are not accepted",
            ));
        }
        let Some(length) = content_length else {
            return Err(HttpReply::error(411, "the request had no Content-Length"));
        };
        if length > MAX_BODY {
            // Read the oversized body off the socket (bounded) before answering.
            // Closing on a peer that is still writing costs it the response: the
            // RST that follows unread data can discard the 413 it needs to see.
            drain(stream, length.min(DRAIN_LIMIT).saturating_sub(body.len())).await;
            return Err(HttpReply::error(
                413,
                format!("bodies are capped at {MAX_BODY} bytes; this one declared {length}"),
            ));
        }
        if expects_continue && body.len() < length {
            let _ = stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n").await;
        }
        while body.len() < length {
            let read = stream.read(&mut chunk).await.map_err(|error| {
                HttpReply::error(400, format!("could not read the body: {error}"))
            })?;
            if read == 0 {
                return Err(HttpReply::error(400, "the body ended early"));
            }
            body.extend_from_slice(&chunk[..read]);
            if body.len() > MAX_BODY {
                return Err(HttpReply::error(
                    413,
                    format!("bodies are capped at {MAX_BODY} bytes"),
                ));
            }
        }
        body.truncate(length);
    }

    Ok(RawRequest {
        method,
        path,
        origin,
        body,
    })
}

/// Read and discard at most `remaining` bytes, so an over-cap request still
/// gets its refusal instead of a connection reset. Bounded twice: by this
/// count and by the connection's overall timeout.
async fn drain(stream: &mut TcpStream, mut remaining: usize) {
    let mut sink = [0u8; 4096];
    while remaining > 0 {
        match stream.read(&mut sink).await {
            Ok(0) | Err(_) => return,
            Ok(read) => remaining = remaining.saturating_sub(read),
        }
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

// --------------------------------------------------------------------- tests --

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    // --- pure rules ---------------------------------------------------------

    #[test]
    fn the_port_range_is_the_one_design_3_2_pins() {
        assert_eq!(BROKER_PORTS, [7968, 7969, 7970, 7971]);
    }

    #[test]
    fn slugs_are_derived_the_way_design_8_4_says() {
        let slug = |name: &str| derive_slug(Some(name), 42);
        assert_eq!(slug("My Game"), "my-game");
        assert_eq!(slug("  Super   Cool  Place!! "), "super-cool-place");
        assert_eq!(slug("Tycoon 2: Electric Boogaloo"), "tycoon-2-electric-boogaloo");
        assert_eq!(slug("__weird__name__"), "weird-name");
        assert_eq!(slug("ALLCAPS"), "allcaps");
        assert_eq!(slug("a"), "a");

        // Nothing usable survives → the documented fallback.
        assert_eq!(slug("   "), "place-42");
        assert_eq!(slug("!!!"), "place-42");
        assert_eq!(slug("Мой проект"), "place-42");
        assert_eq!(derive_slug(None, 42), "place-42");
        assert_eq!(derive_slug(Some(""), 7), "place-7");

        // A name that would be a directory Windows cannot host.
        assert_eq!(slug("CON"), "place-42");
        assert_eq!(slug("com1"), "place-42");

        // Bounded, and never left with a trailing separator after the cut.
        let long = slug(&"long name ".repeat(40));
        assert!(long.len() <= MAX_SLUG_LENGTH, "{long}");
        assert!(!long.ends_with('-'), "{long}");

        // Whatever comes out is one path component.
        for name in ["../../etc", "a/b/c", "C:\\Users", "..", ".", "a\0b"] {
            let derived = slug(name);
            assert!(is_single_component(&derived), "{name:?} → {derived:?}");
            assert!(!derived.contains(['/', '\\']), "{derived}");
        }
    }

    #[test]
    fn project_ids_match_the_frontends_shape() {
        let id = new_project_id();
        assert!(id.starts_with("p_"), "{id}");
        assert!(
            id[2..].chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()),
            "{id}"
        );
        let mut seen = std::collections::HashSet::new();
        for _ in 0..64 {
            assert!(seen.insert(new_project_id()));
        }
    }

    #[test]
    fn timestamps_are_iso_8601_utc() {
        assert_eq!(
            rfc3339(UNIX_EPOCH + Duration::from_secs(0)),
            "1970-01-01T00:00:00Z"
        );
        assert_eq!(
            rfc3339(UNIX_EPOCH + Duration::from_secs(1_770_000_000)),
            "2026-02-02T02:40:00Z"
        );
        // A leap day, because that is where a hand-rolled calendar goes wrong.
        assert_eq!(
            rfc3339(UNIX_EPOCH + Duration::from_secs(1_709_164_800)),
            "2024-02-29T00:00:00Z"
        );
    }

    // --- the harness --------------------------------------------------------

    #[derive(Default)]
    struct Recorder {
        ups: StdMutex<Vec<BrokerStatus>>,
        downs: StdMutex<Vec<BrokerStatus>>,
        inits: StdMutex<Vec<ProjectInitEvent>>,
        serves: StdMutex<Vec<(String, String)>>,
    }

    impl BrokerEvents for Recorder {
        fn up(&self, status: &BrokerStatus) {
            self.ups.lock().unwrap().push(status.clone());
        }
        fn down(&self, status: &BrokerStatus) {
            self.downs.lock().unwrap().push(status.clone());
        }
        fn project_init(&self, event: &ProjectInitEvent) {
            self.inits.lock().unwrap().push(event.clone());
        }
        fn serve(&self, project_id: &str, project_path: &str) {
            self.serves
                .lock()
                .unwrap()
                .push((project_id.to_string(), project_path.to_string()));
        }
    }

    impl Recorder {
        fn inits(&self) -> Vec<ProjectInitEvent> {
            self.inits.lock().unwrap().clone()
        }
        fn serves(&self) -> Vec<(String, String)> {
            self.serves.lock().unwrap().clone()
        }
    }

    /// A stand-in for `games.roblox.com`, so the naming rules are tested
    /// without the internet — and so every *other* test can be pointed at
    /// something that refuses instantly instead of reaching it by accident.
    struct GamesStub {
        port: u16,
        hits: Arc<StdMutex<Vec<String>>>,
        task: JoinHandle<()>,
    }

    impl GamesStub {
        fn base(&self) -> String {
            format!("http://127.0.0.1:{}", self.port)
        }

        fn hits(&self) -> Vec<String> {
            self.hits.lock().unwrap().clone()
        }
    }

    impl Drop for GamesStub {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    /// `responder` sees the request target (`/v1/games?universeIds=…`) and
    /// answers `(status, body)`.
    async fn games_stub<R>(responder: R) -> GamesStub
    where
        R: Fn(&str) -> (u16, String) + Send + Sync + 'static,
    {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let hits = Arc::new(StdMutex::new(Vec::new()));
        let recorded = Arc::clone(&hits);

        let task = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let mut head = Vec::new();
                let mut chunk = [0u8; 1024];
                while find(&head, b"\r\n\r\n").is_none() {
                    match stream.read(&mut chunk).await {
                        Ok(0) | Err(_) => break,
                        Ok(read) => head.extend_from_slice(&chunk[..read]),
                    }
                }
                let text = String::from_utf8_lossy(&head).into_owned();
                let target = text
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .unwrap_or("")
                    .to_string();
                recorded.lock().unwrap().push(target.clone());

                let (status, body) = responder(&target);
                let response = format!(
                    "HTTP/1.1 {status} {}\r\ncontent-type: application/json\r\n\
                     content-length: {}\r\nconnection: close\r\n\r\n{body}",
                    reason_phrase(status),
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
            }
        });

        GamesStub { port, hits, task }
    }

    /// A base nothing can be listening on, so a test that does not care about
    /// the name lookup fails it immediately rather than reaching the internet.
    const NO_GAMES_API: &str = "http://127.0.0.1:1";

    struct Harness {
        _directory: tempfile::TempDir,
        /// The canonical temp root; `root` is the authorized folder inside it.
        base: PathBuf,
        root: PathBuf,
        state_file: PathBuf,
        events: Arc<Recorder>,
        broker: Arc<Broker>,
    }

    impl Harness {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            // Canonical from the start, so a recorded project path can be
            // compared to `root.join(slug)` on a platform where the temp
            // directory is itself a link (macOS: /var → /private/var).
            let base = fs::canonicalize(directory.path()).unwrap();
            let root = base.join("Projects");
            fs::create_dir(&root).unwrap();
            let state_file = base.join("state.json");
            let events = Arc::new(Recorder::default());
            let broker = Broker::new(
                Arc::clone(&events) as Arc<dyn BrokerEvents>,
                state_file.clone(),
                Arc::new(Mutex::new(())),
            );
            // Nothing in this file may touch the network. Tests about the name
            // lookup opt in by pointing this at their own stub.
            broker.set_games_base(NO_GAMES_API);
            Self {
                _directory: directory,
                base,
                root,
                state_file,
                events,
                broker,
            }
        }

        /// Start on one OS-assigned free port, so tests never fight each other
        /// — or a WSync that happens to be running on this machine — for the
        /// real 7968–7971.
        async fn start(&self) -> u16 {
            let ports = [free_port()];
            self.broker
                .start_on(&self.root, &ports)
                .await
                .expect("the broker should bind")
                .port
                .unwrap()
        }

        fn projects(&self) -> Vec<Value> {
            storage::read_document(&self.state_file)
                .unwrap()
                .get("projects")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
        }
    }

    /// A port nothing is listening on. The bind-and-drop race is the same one
    /// `loopback.rs`'s tests take, and is not worth a lock file.
    fn free_port() -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        port
    }

    async fn post(port: u16, body: Value) -> crate::loopback::Response {
        crate::loopback::post_json(port, "/projects/init", &body, Duration::from_secs(5))
            .await
            .expect("the broker should answer")
    }

    /// `name` arrives as the *place* name — which the plugin resolves through
    /// MarketplaceService to the real published name, making it the
    /// authoritative head of the naming chain.
    fn init_body(request_id: &str, game_id: u64, name: &str) -> Value {
        json!({
            "requestId": request_id,
            "gameId": game_id,
            "placeName": name,
            "placeId": game_id + 1,
        })
    }

    /// A request from a plugin that could not name the place at all — the only
    /// shape that reaches the games-API fallback.
    fn init_body_unnamed(request_id: &str, game_id: u64) -> Value {
        json!({
            "requestId": request_id,
            "gameId": game_id,
            "placeId": game_id + 1,
        })
    }

    /// The games API's answer for one universe.
    fn games_answer(game_id: u64, name: &str) -> String {
        json!({ "data": [{ "id": game_id, "name": name, "rootPlaceId": game_id + 1 }] }).to_string()
    }

    // --- routes -------------------------------------------------------------

    #[tokio::test]
    async fn hello_says_what_the_plugin_scans_for() {
        let harness = Harness::new();
        let port = harness.start().await;

        let response = crate::loopback::get(port, "/hello", Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(response.status, 200);
        let body = response.json().unwrap();
        assert_eq!(body["broker"], true);
        assert_eq!(body["projectInit"], true);
        assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(body["games"], json!([]), "an empty registry advertises no games");

        // Neighbouring shapes are refused rather than half-answered.
        assert_eq!(
            crate::loopback::get(port, "/projects/init", Duration::from_secs(5))
                .await
                .unwrap()
                .status,
            405
        );
        assert_eq!(
            crate::loopback::get(port, "/anything-else", Duration::from_secs(5))
                .await
                .unwrap()
                .status,
            404
        );
    }

    #[tokio::test]
    async fn a_create_writes_one_child_merges_the_registry_and_auto_serves() {
        let harness = Harness::new();
        let port = harness.start().await;

        let response = post(port, init_body("a1b2c3d4", 123456, "My Game")).await;
        assert_eq!(response.status, 200);
        let body = response.json().unwrap();
        assert_eq!(body["ok"], true);
        assert_eq!(body["slug"], "my-game");
        assert_eq!(body["status"], "created");

        // Exactly one direct child, with the scaffold inside it.
        let children: Vec<_> = fs::read_dir(&harness.root)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(children, vec!["my-game".to_string()]);
        let project_dir = harness.root.join("my-game");
        for service in templates::SERVICES {
            assert!(project_dir.join("src").join(service).is_dir(), "missing {service}");
        }
        let document: Value =
            serde_json::from_str(&fs::read_to_string(project_dir.join("default.project.json")).unwrap())
                .unwrap();
        assert_eq!(document["name"], "My Game");
        assert_eq!(document["gameId"], 123456);
        assert_eq!(document["placeIds"], json!([123457]));

        // Merged into the registry, marked, and pointing at the folder.
        let projects = harness.projects();
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0]["initializedFromStudio"], true);
        assert_eq!(projects[0]["gameId"], 123456);
        assert_eq!(projects[0]["path"], project_dir.to_string_lossy().as_ref());
        assert!(projects[0]["id"].as_str().unwrap().starts_with("p_"));

        // One event, carrying the record the view merges.
        let inits = harness.events.inits();
        assert_eq!(inits.len(), 1);
        assert_eq!(inits[0].request_id, "a1b2c3d4");
        assert_eq!(inits[0].slug, "my-game");
        assert_eq!(inits[0].status, "created");
        assert_eq!(inits[0].project_id, projects[0]["id"].as_str().unwrap());

        // ...and it was handed to the daemon registry to serve.
        assert_eq!(
            harness.events.serves(),
            vec![(
                projects[0]["id"].as_str().unwrap().to_string(),
                project_dir.to_string_lossy().into_owned()
            )]
        );
    }

    #[tokio::test]
    async fn a_replayed_request_id_creates_nothing_and_answers_the_same() {
        let harness = Harness::new();
        let port = harness.start().await;

        let first = post(port, init_body("deadbeef", 1, "Replay Me")).await;
        let second = post(port, init_body("deadbeef", 1, "Replay Me")).await;
        // Even a replay that disagrees about the name gets the first answer.
        let third = post(port, init_body("DEADBEEF", 1, "Something Else")).await;

        for response in [&first, &second, &third] {
            let body = response.json().unwrap();
            assert_eq!(body["slug"], "replay-me");
            assert_eq!(body["status"], "created", "a replay must not create again");
        }
        assert_eq!(fs::read_dir(&harness.root).unwrap().count(), 1);
        assert_eq!(harness.projects().len(), 1);
        // One event for one project, however many times it was asked for.
        assert_eq!(harness.events.inits().len(), 1);
        // ...but every attempt re-serves, because a retry may be a retry of a
        // daemon that never came up.
        assert_eq!(harness.events.serves().len(), 3);
    }

    #[tokio::test]
    async fn a_second_click_for_the_same_game_adopts_instead_of_minting_a_sibling() {
        let harness = Harness::new();
        let port = harness.start().await;

        let created = post(port, init_body("11111111", 99, "Same Game")).await;
        assert_eq!(created.json().unwrap()["status"], "created");

        // Different requestId, same GameId, even a different name.
        let adopted = post(port, init_body("22222222", 99, "Renamed In Studio")).await;
        let body = adopted.json().unwrap();
        assert_eq!(body["ok"], true);
        assert_eq!(body["status"], "existing");
        assert_eq!(body["slug"], "same-game");

        assert_eq!(fs::read_dir(&harness.root).unwrap().count(), 1);
        assert_eq!(harness.projects().len(), 1);
        // No second `project-init`: nothing new was merged.
        assert_eq!(harness.events.inits().len(), 1);
        // It is still served, which is the whole point of answering at all.
        assert_eq!(harness.events.serves().len(), 2);
    }

    #[tokio::test]
    async fn a_registry_entry_whose_folder_is_gone_does_not_block_a_new_project() {
        let harness = Harness::new();
        let port = harness.start().await;

        post(port, init_body("33333333", 55, "Gone Game")).await;
        fs::remove_dir_all(harness.root.join("gone-game")).unwrap();

        let response = post(port, init_body("44444444", 55, "Gone Game")).await;
        assert_eq!(response.json().unwrap()["status"], "created");
        assert!(harness.root.join("gone-game").is_dir());
    }

    #[tokio::test]
    async fn colliding_names_are_suffixed_in_order() {
        let harness = Harness::new();
        let port = harness.start().await;

        // A folder the user already made by hand, and one WSync makes.
        fs::create_dir(harness.root.join("shared-name")).unwrap();
        let second = post(port, init_body("aaaaaaaa", 1, "Shared Name")).await;
        let third = post(port, init_body("bbbbbbbb", 2, "Shared Name")).await;
        let fourth = post(port, init_body("cccccccc", 3, "Shared Name")).await;

        assert_eq!(second.json().unwrap()["slug"], "shared-name-2");
        assert_eq!(third.json().unwrap()["slug"], "shared-name-3");
        assert_eq!(fourth.json().unwrap()["slug"], "shared-name-4");
        assert!(harness.root.join("shared-name-2/src/ReplicatedStorage").is_dir());
        // The pre-existing folder is untouched.
        assert_eq!(fs::read_dir(harness.root.join("shared-name")).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_symlink_where_the_project_would_go_is_refused_not_followed() {
        let harness = Harness::new();
        let port = harness.start().await;

        // The classic shape: a link inside the authorized folder pointing out
        // of it. Writing through it would put a project anywhere the attacker
        // liked, so the name is refused outright rather than suffixed around.
        let elsewhere = harness.base.join("elsewhere");
        fs::create_dir(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, harness.root.join("escape-me")).unwrap();

        let response = post(port, init_body("55555555", 8, "Escape Me")).await;
        assert_eq!(response.status, 409);
        let body = response.json().unwrap();
        assert_eq!(body["ok"], false);
        assert!(
            body["error"].as_str().unwrap().contains("symbolic link"),
            "{body}"
        );

        // Nothing was written through the link, and no sibling was invented.
        assert_eq!(fs::read_dir(&elsewhere).unwrap().count(), 0);
        assert!(!harness.root.join("escape-me-2").exists());
        assert!(harness.projects().is_empty());
        assert!(harness.events.inits().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn the_direct_child_check_rejects_anything_that_resolves_elsewhere() {
        let directory = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(directory.path()).unwrap();
        let root = base.join("root");
        let outside = base.join("outside");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&outside).unwrap();

        let child = root.join("child");
        fs::create_dir(&child).unwrap();
        fs::create_dir(child.join("deep")).unwrap();
        assert!(verify_inside(&root, &child).is_ok());

        // A grandchild is not a direct child — "exactly one" means one level.
        assert!(verify_inside(&root, &child.join("deep")).is_err());

        // A link inside the root that resolves out of it is the case Design 11
        // names: the name looks like a child, the bytes would land elsewhere.
        std::os::unix::fs::symlink(&outside, root.join("linked")).unwrap();
        assert!(verify_inside(&root, &root.join("linked")).is_err());

        // ...and a path that walks back out with `..` is caught the same way.
        assert!(verify_inside(&root, &root.join("child/../..")).is_err());
    }

    #[tokio::test]
    async fn a_body_over_the_cap_is_refused_before_it_is_parsed() {
        let harness = Harness::new();
        let port = harness.start().await;

        let response = post(
            port,
            json!({
                "requestId": "66666666",
                "gameId": 4,
                "placeName": "x".repeat(MAX_BODY),
            }),
        )
        .await;
        assert_eq!(response.status, 413);
        assert_eq!(response.json().unwrap()["ok"], false);
        assert!(harness.projects().is_empty());

        // The listener is still healthy afterwards.
        assert_eq!(
            crate::loopback::get(port, "/hello", Duration::from_secs(5))
                .await
                .unwrap()
                .status,
            200
        );
    }

    #[tokio::test]
    async fn malformed_and_path_carrying_requests_are_refused() {
        let harness = Harness::new();
        let port = harness.start().await;

        let cases: Vec<(Value, &str)> = vec![
            (json!({"gameId": 1}), "requestId"),
            (json!({"requestId": "abcdef01"}), "gameId"),
            (json!({"requestId": "not-hex-at-all", "gameId": 1}), "hex"),
            (json!({"requestId": "abc", "gameId": 1}), "hex"),
            (json!({"requestId": "abcdef01", "gameId": 0}), "gameId"),
            (json!({"requestId": "abcdef01", "gameId": "one"}), "request"),
            // Design 11: no path ever enters this way.
            (
                json!({"requestId": "abcdef01", "gameId": 1, "path": "/tmp/anywhere"}),
                "never takes a path",
            ),
            (
                json!({"requestId": "abcdef01", "gameId": 1, "projectPath": "/tmp/x"}),
                "never takes a path",
            ),
            (
                json!({"requestId": "abcdef01", "gameId": 1, "slug": "../escape"}),
                "never takes a path",
            ),
        ];

        for (body, expected) in cases {
            let response = post(port, body.clone()).await;
            assert_eq!(response.status, 400, "{body} should be refused");
            let answered = response.json().unwrap();
            assert_eq!(answered["ok"], false);
            assert!(
                answered["error"].as_str().unwrap().contains(expected),
                "{body} → {answered}"
            );
        }

        assert!(harness.projects().is_empty());
        assert!(fs::read_dir(&harness.root).unwrap().next().is_none());
    }

    #[tokio::test]
    async fn unknown_fields_are_tolerated_so_the_plugin_can_grow() {
        let harness = Harness::new();
        let port = harness.start().await;

        let response = post(
            port,
            json!({
                "requestId": "77777777",
                "gameId": 12,
                // Design 7.0 dropped `gameName` from the naming chain; a client
                // still sending one is tolerated exactly like any other field
                // the broker does not read.
                "gameName": "Ignored Now",
                "placeName": "Start Place",
                "creatorType": "Group",
                "creatorId": 4242,
                "somethingTheFutureAdds": true,
            }),
        )
        .await;
        assert_eq!(response.status, 200);
        assert_eq!(response.json().unwrap()["slug"], "start-place");

        // `creatorType: Group` is Design 4.3's `groupId`, in both places.
        let projects = harness.projects();
        assert_eq!(projects[0]["groupId"], 4242);
        let document: Value = serde_json::from_str(
            &fs::read_to_string(harness.root.join("start-place/default.project.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(document["groupId"], 4242);
    }

    #[tokio::test]
    async fn the_naming_chain_is_place_name_then_games_api_then_place_id() {
        // The primary chain with no games API reachable: the plugin's
        // `placeName` names the project, and with none at all the id does.
        let harness = Harness::new();
        let port = harness.start().await;

        let response = post(
            port,
            json!({"requestId": "88888888", "gameId": 31, "placeName": "Baseplate Draft"}),
        )
        .await;
        assert_eq!(response.json().unwrap()["slug"], "baseplate-draft");

        let response = post(port, json!({"requestId": "99999999", "gameId": 32})).await;
        assert_eq!(response.json().unwrap()["slug"], "place-32");
        assert_eq!(harness.projects()[1]["name"], "place-32");
    }

    #[tokio::test]
    async fn hello_advertises_registered_games_so_the_plugin_offers_serve_not_create() {
        let harness = Harness::new();
        let port = harness.start().await;

        let created = post(
            port,
            json!({"requestId": "aa11aa11", "gameId": 424242, "placeName": "Known Game"}),
        )
        .await;
        assert_eq!(created.status, 200);

        let body = crate::loopback::get(port, "/hello", Duration::from_secs(5))
            .await
            .unwrap()
            .json()
            .unwrap();
        assert_eq!(body["games"], json!([424242]));

        // A registry record whose directory is gone is not advertised: the
        // init it invites would not adopt it (`existing_project` skips it).
        fs::remove_dir_all(harness.root.join("known-game")).unwrap();
        let body = crate::loopback::get(port, "/hello", Duration::from_secs(5))
            .await
            .unwrap()
            .json()
            .unwrap();
        assert_eq!(body["games"], json!([]));
    }

    // --- naming: placeName is authoritative, the games API is the fallback ---

    #[tokio::test]
    async fn the_place_name_the_plugin_sent_wins_and_skips_the_games_api() {
        // The plugin resolves the published name through MarketplaceService, so
        // its `placeName` is the real one — it must drive the slug, the display
        // name and the project file, and the games API must not even be asked.
        let stub = games_stub(|_| (200, games_answer(90210, "A Different Answer"))).await;

        let harness = Harness::new();
        harness.broker.set_games_base(stub.base());
        let port = harness.start().await;

        let response = post(
            port,
            json!({"requestId": "5a5a5a5a", "gameId": 90210, "placeName": "Switch and Shoot", "placeId": 7}),
        )
        .await;
        assert_eq!(response.status, 200);
        assert_eq!(response.json().unwrap()["slug"], "switch-and-shoot");
        assert!(stub.hits().is_empty(), "{:?}", stub.hits());

        // The display name and the project file agree with the slug: one
        // answer, three places.
        let projects = harness.projects();
        assert_eq!(projects[0]["name"], "Switch and Shoot");
        let document: Value = serde_json::from_str(
            &fs::read_to_string(harness.root.join("switch-and-shoot/default.project.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(document["name"], "Switch and Shoot");
        assert_eq!(document["gameId"], 90210);
        assert_eq!(document["scope"], "code");
        assert_eq!(document["tree"]["ServerScriptService"]["$path"], "src/ServerScriptService");
    }

    #[tokio::test]
    async fn a_blank_place_name_is_no_place_name() {
        // Whitespace is the same as absent: the fallback lookup still runs.
        let stub = games_stub(|target| {
            assert!(target.starts_with("/v1/games?universeIds="), "{target}");
            (200, games_answer(616, "Named By The Api"))
        })
        .await;

        let harness = Harness::new();
        harness.broker.set_games_base(stub.base());
        let port = harness.start().await;

        let response = post(
            port,
            json!({"requestId": "6b6b6b6b", "gameId": 616, "placeName": "   "}),
        )
        .await;
        assert_eq!(response.json().unwrap()["slug"], "named-by-the-api");
        assert_eq!(stub.hits().len(), 1, "{:?}", stub.hits());
    }

    #[tokio::test]
    async fn the_games_api_names_a_project_the_plugin_could_not() {
        // No `placeName` in the request — an unpublished place, or a very old
        // plugin — so the games API supplies the name.
        let stub = games_stub(|target| {
            assert!(target.starts_with("/v1/games?universeIds="), "{target}");
            (200, games_answer(90211, "From The Api"))
        })
        .await;

        let harness = Harness::new();
        harness.broker.set_games_base(stub.base());
        let port = harness.start().await;

        let response = post(port, init_body_unnamed("7c7c7c7c", 90211)).await;
        assert_eq!(response.status, 200);
        assert_eq!(response.json().unwrap()["slug"], "from-the-api");
        assert_eq!(stub.hits(), vec!["/v1/games?universeIds=90211".to_string()]);
        assert_eq!(harness.projects()[0]["name"], "From The Api");
    }

    #[tokio::test]
    async fn one_lookup_per_game_id_for_the_brokers_lifetime() {
        let stub = games_stub(|_| (200, games_answer(4242, "Cached Once"))).await;
        let harness = Harness::new();
        harness.broker.set_games_base(stub.base());
        let port = harness.start().await;

        // Three unnamed requests for the same game: a create, its replay, and a
        // fresh click that adopts. One lookup.
        assert_eq!(post(port, init_body_unnamed("aaaa0001", 4242)).await.status, 200);
        assert_eq!(post(port, init_body_unnamed("aaaa0001", 4242)).await.status, 200);
        assert_eq!(post(port, init_body_unnamed("aaaa0002", 4242)).await.status, 200);
        assert_eq!(stub.hits().len(), 1, "{:?}", stub.hits());

        // A different game is a different question.
        assert_eq!(post(port, init_body_unnamed("aaaa0003", 4243)).await.status, 200);
        assert_eq!(stub.hits().len(), 2, "{:?}", stub.hits());
    }

    #[tokio::test]
    async fn an_unusable_games_answer_falls_back_instead_of_failing_the_create() {
        // Every one of these is "no name", not "no project": a creation that
        // failed because a name lookup did would be a worse outcome than a
        // folder called after the place id. All unnamed — a request that
        // carries a `placeName` never reaches the lookup at all.
        let cases: Vec<(u64, u16, String, String)> = vec![
            (51, 500, "{}".to_string(), "place-51".to_string()),
            (52, 200, "not json at all".to_string(), "place-52".to_string()),
            (53, 200, json!({ "data": [] }).to_string(), "place-53".to_string()),
            // An answer about a different universe is not an answer.
            (54, 200, games_answer(999, "Someone Elses Game"), "place-54".to_string()),
            (55, 200, json!({ "data": [{ "id": 55 }] }).to_string(), "place-55".to_string()),
            (56, 200, games_answer(56, "   "), "place-56".to_string()),
        ];

        for (game_id, status, body, expected) in cases {
            let answer = body.clone();
            let stub = games_stub(move |_| (status, answer.clone())).await;
            let harness = Harness::new();
            harness.broker.set_games_base(stub.base());
            let port = harness.start().await;

            let response = post(port, init_body_unnamed(&format!("bbbb{game_id:04}"), game_id)).await;
            assert_eq!(response.status, 200, "gameId {game_id} → {body}");
            assert_eq!(response.json().unwrap()["slug"], expected, "gameId {game_id}");
        }
    }

    #[tokio::test]
    async fn an_unreachable_games_api_does_not_hold_up_the_create() {
        let harness = Harness::new();
        let port = harness.start().await;

        let started = std::time::Instant::now();
        let response = post(port, init_body_unnamed("cccc0001", 77)).await;
        assert_eq!(response.json().unwrap()["slug"], "place-77");
        // The default base refuses instantly; what is being asserted is that
        // the timeout is a ceiling and not a floor.
        assert!(started.elapsed() < GAMES_API_TIMEOUT, "{:?}", started.elapsed());
    }

    #[test]
    fn a_games_answer_is_only_read_for_the_universe_that_was_asked_about() {
        let body = json!({
            "data": [
                { "id": 11, "name": "Wrong One" },
                { "id": 12, "name": "  Right One  " },
            ]
        })
        .to_string();
        assert_eq!(
            game_name_from_body(body.as_bytes(), 12).as_deref(),
            Some("Right One"),
        );
        assert_eq!(game_name_from_body(body.as_bytes(), 13), None);

        // Bounded: an absurd name is refused rather than carried into a path.
        let huge = json!({ "data": [{ "id": 1, "name": "n".repeat(MAX_REMOTE_NAME_LENGTH + 1) }] })
            .to_string();
        assert_eq!(game_name_from_body(huge.as_bytes(), 1), None);
        assert!(game_name_from_body(b"", 1).is_none());
    }

    #[test]
    fn a_tls_client_can_be_built_before_anything_else_has_run() {
        // The regression `loopback.rs`'s header describes: with reqwest's
        // `rustls-no-provider` unified in, `build()` fails unless a provider is
        // installed first. If a feature change ever removes ring from the tree,
        // this fails here rather than the first time a user clicks Create.
        install_crypto_provider();
        assert!(rustls::crypto::CryptoProvider::get_default().is_some());
        assert!(reqwest::Client::builder()
            .timeout(GAMES_API_TIMEOUT)
            .build()
            .is_ok());
    }

    #[test]
    fn the_games_base_is_the_public_endpoint_unless_the_override_says_otherwise() {
        // The override is the seam the tests above use; this is the one place
        // that proves the environment variable is what opens it.
        static ENV_LOCK: StdMutex<()> = StdMutex::new(());
        let _guard = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());

        let restore = std::env::var(GAMES_API_BASE_ENV).ok();
        std::env::remove_var(GAMES_API_BASE_ENV);
        assert_eq!(games_api_base(), GAMES_API_BASE);
        assert!(GAMES_API_BASE.starts_with("https://"), "{GAMES_API_BASE}");

        std::env::set_var(GAMES_API_BASE_ENV, "  http://127.0.0.1:9 ");
        assert_eq!(games_api_base(), "http://127.0.0.1:9");

        // An empty override is not an override.
        std::env::set_var(GAMES_API_BASE_ENV, "   ");
        assert_eq!(games_api_base(), GAMES_API_BASE);

        match restore {
            Some(value) => std::env::set_var(GAMES_API_BASE_ENV, value),
            None => std::env::remove_var(GAMES_API_BASE_ENV),
        }
    }

    #[tokio::test]
    async fn a_browser_origin_never_reaches_a_route() {
        let harness = Harness::new();
        let port = harness.start().await;

        // Hand-rolled, because the point is a header the loopback client will
        // never send: a page on some site POSTing at the loopback port.
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let body = init_body("abcdabcd", 5, "From A Website").to_string();
        let request = format!(
            "POST /projects/init HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: https://evil.example\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut answer = String::new();
        stream.read_to_string(&mut answer).await.unwrap();

        assert!(answer.starts_with("HTTP/1.1 403"), "{answer}");
        assert!(harness.projects().is_empty());
        assert!(fs::read_dir(&harness.root).unwrap().next().is_none());
    }

    // --- lifecycle ----------------------------------------------------------

    #[tokio::test]
    async fn nothing_listens_until_a_folder_is_authorized() {
        let harness = Harness::new();

        let status = harness.broker.status().await;
        assert!(!status.running);
        assert_eq!(status.port, None);
        assert_eq!(status.root, None);
        assert!(status.detail.contains("authorize"), "{}", status.detail);

        // And a stop on a broker that never started is a no-op, not an error —
        // and not an event either, because nothing transitioned.
        let stopped = harness.broker.stop("authorize a folder").await;
        assert!(!stopped.running);
        assert!(harness.events.downs.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_unauthorized_folder_is_refused_rather_than_created() {
        let harness = Harness::new();
        let missing = harness.base.join("not-there");
        let error = harness.broker.start(&missing).await.unwrap_err();
        assert_eq!(error.code, crate::error::code::IO);
        assert!(!missing.exists(), "start must never create the root");
        assert!(!harness.broker.status().await.running);
    }

    #[tokio::test]
    async fn the_port_falls_through_the_range() {
        let harness = Harness::new();
        // Occupy the first port of a private three-port range, exactly as a
        // second WSync (or Ro-Sync-era leftover) would occupy 7968.
        let occupied = free_port();
        let _squatter = std::net::TcpListener::bind(("127.0.0.1", occupied)).unwrap();
        let ports = [occupied, free_port(), free_port()];

        let status = harness.broker.start_on(&harness.root, &ports).await.unwrap();
        assert!(status.running);
        assert_eq!(status.port, Some(ports[1]), "it must skip the busy port");
        assert!(status.detail.contains(&ports[1].to_string()));
    }

    #[tokio::test]
    async fn every_port_busy_is_reported_not_guessed_around() {
        let harness = Harness::new();
        let first = free_port();
        let second = free_port();
        let _a = std::net::TcpListener::bind(("127.0.0.1", first)).unwrap();
        let _b = std::net::TcpListener::bind(("127.0.0.1", second)).unwrap();

        let error = harness
            .broker
            .start_on(&harness.root, &[first, second])
            .await
            .unwrap_err();
        assert_eq!(error.code, crate::error::code::UNAVAILABLE);
        assert!(error.message.contains("no free broker port"), "{error}");

        // Authorized but not listening is its own state: Settings must be able
        // to say *why*, not fall back to "authorize a folder" when one is.
        let status = harness.broker.status().await;
        assert!(!status.running);
        assert!(status.detail.contains("no free broker port"), "{}", status.detail);
    }

    #[tokio::test]
    async fn clearing_the_folder_stops_the_listener_and_frees_the_port() {
        let harness = Harness::new();
        let port = harness.start().await;
        assert!(harness.broker.status().await.running);

        let status = harness.broker.stop("authorize a folder").await;
        assert!(!status.running);
        assert_eq!(status.port, None);
        assert_eq!(status.detail, "Off — authorize a folder.");
        // ...and the same answer for anyone who asks later, not just the caller.
        let polled = harness.broker.status().await;
        assert!(!polled.running);
        assert_eq!(polled.detail, status.detail);
        assert_eq!(harness.events.downs.lock().unwrap().len(), 1);

        // The socket is genuinely gone: nothing answers, and the port rebinds.
        assert!(crate::loopback::get(port, "/hello", Duration::from_millis(500))
            .await
            .is_err());
        TcpListener::bind((Ipv4Addr::LOCALHOST, port))
            .await
            .expect("stop must release the port");
    }

    #[tokio::test]
    async fn re_authorizing_moves_the_broker_to_the_new_folder() {
        let harness = Harness::new();
        let first_port = harness.start().await;

        let second_root = harness.base.join("Elsewhere");
        fs::create_dir(&second_root).unwrap();
        let ports = [free_port()];
        let status = harness.broker.start_on(&second_root, &ports).await.unwrap();

        assert_eq!(status.port, Some(ports[0]));
        assert_eq!(
            status.root.as_deref(),
            Some(fs::canonicalize(&second_root).unwrap().to_string_lossy().as_ref())
        );
        // The old listener is gone, not leaked.
        TcpListener::bind((Ipv4Addr::LOCALHOST, first_port))
            .await
            .expect("the previous listener must have been torn down");

        // Starting again on the same root is idempotent: same port, no rebind.
        let again = harness.broker.start_on(&second_root, &[free_port()]).await.unwrap();
        assert_eq!(again.port, Some(ports[0]));
    }

    // --- scratch harness ----------------------------------------------------

    /// A real broker on the real `7968–7971`, driven from outside over HTTP.
    ///
    /// Not part of the suite (`#[ignore]`): it binds the production ports and
    /// stays up, which is exactly what makes it useful for standing in for
    /// Studio with `curl`. Everything it does is the shipping code path — the
    /// same `Broker`, the same `state.json` merge, and the same
    /// `DaemonRegistry::start` the `daemon_start` command uses.
    ///
    ///   WSYNC_HARNESS_ROOT    the authorized projects folder   (required)
    ///   WSYNC_HARNESS_STATE   state.json to merge into         (required)
    ///   WSYNC_HARNESS_EVENTS  JSONL file of emitted events     (required)
    ///   WSYNC_HARNESS_DATA    daemon --data-dir                (optional)
    ///   WSYNC_DAEMON_PATH     engine binary; without it, auto-serve is recorded
    ///                         but no daemon is spawned
    ///
    /// It runs until `$WSYNC_HARNESS_EVENTS.stop` appears, or 120 s.
    #[tokio::test]
    #[ignore = "scratch harness: `cargo test -- --ignored harness_broker`, then drive it over HTTP"]
    async fn harness_broker() {
        use std::io::Write as _;

        let variable = |name: &str| std::env::var(name).unwrap_or_default();
        let root = PathBuf::from(variable("WSYNC_HARNESS_ROOT"));
        let state_file = PathBuf::from(variable("WSYNC_HARNESS_STATE"));
        let events_file = PathBuf::from(variable("WSYNC_HARNESS_EVENTS"));
        assert!(root.is_dir(), "WSYNC_HARNESS_ROOT must be a folder");
        assert!(!events_file.as_os_str().is_empty(), "WSYNC_HARNESS_EVENTS is required");

        struct Silent;
        impl crate::daemon::DaemonEvents for Silent {
            fn up(&self, _session: &crate::daemon::DaemonSession) {}
            fn down(&self, _event: &crate::daemon::DownEvent) {}
        }

        struct Sink {
            log: PathBuf,
            data_dir: PathBuf,
            daemons: Arc<crate::daemon::DaemonRegistry>,
        }

        impl Sink {
            fn write(&self, event: &str, payload: Value) {
                let line = json!({ "event": event, "payload": payload }).to_string();
                if let Ok(mut file) = fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&self.log)
                {
                    let _ = writeln!(file, "{line}");
                }
                println!("[harness] {line}");
            }
        }

        impl BrokerEvents for Sink {
            fn up(&self, status: &BrokerStatus) {
                self.write("broker:up", serde_json::to_value(status).unwrap_or_default());
            }
            fn down(&self, status: &BrokerStatus) {
                self.write("broker:down", serde_json::to_value(status).unwrap_or_default());
            }
            fn project_init(&self, event: &ProjectInitEvent) {
                self.write("project-init", serde_json::to_value(event).unwrap_or_default());
            }
            fn serve(&self, project_id: &str, project_path: &str) {
                self.write(
                    "serve-requested",
                    json!({ "projectId": project_id, "projectPath": project_path }),
                );
                if std::env::var(crate::daemon::DAEMON_PATH_ENV).is_err() {
                    return;
                }
                let daemons = Arc::clone(&self.daemons);
                let data_dir = self.data_dir.clone();
                let log = self.log.clone();
                let project_id = project_id.to_string();
                let project_path = project_path.to_string();
                tokio::spawn(async move {
                    let result = daemons
                        .start(&project_id, &project_path, &data_dir, None)
                        .await;
                    let payload = match result {
                        Ok(session) => json!({
                            "projectId": session.project_id,
                            "port": session.port,
                            "pid": session.pid,
                            "bootId": session.boot_id,
                        }),
                        Err(error) => json!({ "projectId": project_id, "error": error.message }),
                    };
                    let line = json!({ "event": "daemon:up", "payload": payload }).to_string();
                    if let Ok(mut file) =
                        fs::OpenOptions::new().create(true).append(true).open(&log)
                    {
                        let _ = writeln!(file, "{line}");
                    }
                    println!("[harness] {line}");
                });
            }
        }

        let data_dir = if variable("WSYNC_HARNESS_DATA").is_empty() {
            root.join(".harness-data")
        } else {
            PathBuf::from(variable("WSYNC_HARNESS_DATA"))
        };
        fs::create_dir_all(&data_dir).unwrap();

        let sink = Arc::new(Sink {
            log: events_file.clone(),
            data_dir,
            daemons: Arc::new(crate::daemon::DaemonRegistry::new(Arc::new(Silent))),
        });
        let daemons = Arc::clone(&sink.daemons);
        let broker = Broker::new(
            sink as Arc<dyn BrokerEvents>,
            state_file.clone(),
            Arc::new(Mutex::new(())),
        );

        // What `projects_root_set` does before it starts the broker: the
        // authorization is persisted, so it survives into the next run.
        let mut patch = Map::new();
        patch.insert(
            "projectsRoot".into(),
            Value::String(root.to_string_lossy().into_owned()),
        );
        storage::merge_document(&state_file, patch).expect("persisting projectsRoot");

        let status = broker.start(&root).await.expect("the broker should bind");
        println!("[harness] listening on 127.0.0.1:{}", status.port.unwrap());

        let stop_file = PathBuf::from(format!("{}.stop", events_file.display()));
        let deadline = std::time::Instant::now() + Duration::from_secs(120);
        while !stop_file.exists() && std::time::Instant::now() < deadline {
            time::sleep(Duration::from_millis(250)).await;
        }

        // ...and what `projects_root_clear` does: withdraw the authorization,
        // then stop the listener.
        let mut patch = Map::new();
        patch.insert("projectsRoot".into(), Value::Null);
        let _ = storage::merge_document(&state_file, patch);
        broker.stop("authorize a folder").await;
        daemons.shutdown_all().await;
        println!("[harness] stopped");
    }

    // --- the scaffold against the real engine -------------------------------

    /// A scaffolded project has to be something `wsync` will actually serve.
    /// Only the real binary can prove that, so this runs when — and only when —
    /// `WSYNC_DAEMON_PATH` points at one.
    #[tokio::test]
    async fn the_real_engine_accepts_a_scaffolded_project() {
        let Ok(binary) = std::env::var(crate::daemon::DAEMON_PATH_ENV) else {
            eprintln!(
                "skipping: set {} to a `wsync` binary to check the scaffold against the engine",
                crate::daemon::DAEMON_PATH_ENV
            );
            return;
        };
        if !Path::new(&binary).is_file() {
            eprintln!("skipping: {binary} is not a file");
            return;
        }

        let harness = Harness::new();
        let port = harness.start().await;
        let response = post(port, init_body("f0f0f0f0", 987654, "Engine Check")).await;
        assert_eq!(response.status, 200);
        let project = harness.projects().remove(0);
        let project_path = project["path"].as_str().unwrap().to_string();

        struct Silent;
        impl crate::daemon::DaemonEvents for Silent {
            fn up(&self, _session: &crate::daemon::DaemonSession) {}
            fn down(&self, _event: &crate::daemon::DownEvent) {}
        }

        let data_dir = harness.base.join("data");
        fs::create_dir_all(&data_dir).unwrap();
        let registry = crate::daemon::DaemonRegistry::new(Arc::new(Silent));
        let session = registry
            .start(
                project["id"].as_str().unwrap(),
                &project_path,
                &data_dir,
                Some(free_port()),
            )
            .await
            .expect("the engine should accept the scaffolded project");

        // The ready line *is* the proof: the engine parsed
        // `default.project.json`, resolved every `$path`, and started serving.
        assert!(session.ok);
        assert!(session.canonical_project.contains("default.project.json"));
        registry.shutdown_all().await;
    }
}
