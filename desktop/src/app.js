// app.js — shell lifecycle: route registry, persisted store, and the single
// `api` object every view receives (Design 8.2).
//
// Views are plain modules exporting `mount(root, api) -> unmount`. They own
// their own DOM inside `#view`, subscribe through `api.onBus`, and clean up in
// the returned function. There is no framework, no build step, and no global
// mutable view state — the store below is the only shared truth.

import { host, HostError, IS_TAURI, onHostEvent } from "./bridge.js";
import { createDaemonLink, LINK_STATE } from "./ws.js";
import {
  applyTheme,
  normalizeTheme,
  scheduleThemeReveal,
  THEME_OPTIONS,
  watchSystemTheme,
} from "./views/theme.js";
import { mountProjects } from "./views/projects.js";
import { mountActive } from "./views/active.js";
import { mountBacklog, openBacklogModal } from "./views/backlog.js";
import { mountDocs } from "./views/docs.js";
import { mountSettings } from "./views/settings.js";
import { createLastEditedStore } from "./views/last-edited.js";

// ------------------------------------------------------------------ DOM ---

const $root = document.getElementById("root");
const $view = document.getElementById("view");
const $statusLeft = document.getElementById("status-left");
const $statusRight = document.getElementById("status-right");
const $topbarTitle = document.getElementById("topbar-title");
const $topbarDetail = document.getElementById("topbar-detail");
const $daemonDot = document.getElementById("daemon-dot");
const $conflictsBadge = document.getElementById("conflicts-badge");
const $projectsCount = document.getElementById("projects-count");
const $updateButton = document.getElementById("update-button");
const $toasts = document.getElementById("toasts");
const $tabs = [...document.querySelectorAll(".rail-item")];

// ---------------------------------------------------------------- routes ---

const ROUTES = {
  projects: { label: "Projects", mount: mountProjects },
  active: { label: "Activity", mount: mountActive },
  backlog: { label: "Backlog", mount: mountBacklog },
  docs: { label: "Docs", mount: mountDocs },
  settings: { label: "Settings", mount: mountSettings },
};

const DEFAULT_ROUTE = "projects";

// ----------------------------------------------------------------- store ---

// Mirrors the persisted shape in Design 8.3 and src-tauri/src/storage.rs.
// Anything not in this object is not persisted — deliberately.
const DEFAULT_STATE = Object.freeze({
  projects: [],
  projectsRoot: null,
  activeProjectId: null,
  servedProjectIds: [],
  daemonSessions: {},
  appearanceTheme: "system",
  lastView: DEFAULT_ROUTE,
  /** Design 7.3's last-edited ledger; owned by views/last-edited.js. */
  lastEdited: {},
});

const app = {
  state: { ...DEFAULT_STATE },
  info: null,
  currentRoute: null,
  unmountCurrent: null,
  /** Not persisted: why a serve attempt failed, per project id. */
  daemonFailures: new Map(),
  /** Not persisted: last known backlog count, or null when unchecked. */
  conflictCount: null,
  /**
   * Not persisted: `{projectId, total}` for the disk content that lost to
   * Studio, or null. The banner has to survive navigating away, and unlike the
   * old review this is never a pending decision — just a count of what is
   * recoverable until it expires.
   */
  backlog: null,
  /** Not persisted: the open backlog window's handle, or null. */
  backlogWindow: null,
  persistence: "memory",
  /** Not persisted: the live WS link's last reported status (see ws.js). */
  link: { state: LINK_STATE.IDLE, detail: "No project is being served.", projectId: null },
  /** Not persisted: last plugin-status event per project. */
  plugin: new Map(),
  /** Not persisted: projects whose daemon answered 404 on /resolve. */
  conflictsUnsupported: new Set(),
  /**
   * Not persisted: the project broker's last reported state (Design 8.4). The
   * host owns it — it listens exactly while a Projects folder is authorized —
   * so this is a mirror for the UI, never the source of truth.
   */
  broker: { running: false, port: null, root: null, detail: "Off — authorize a folder." },
};

const listeners = new Map();
let pendingPatch = null;
let flushTimer = null;

function on(event, handler) {
  if (!listeners.has(event)) listeners.set(event, new Set());
  listeners.get(event).add(handler);
  return () => listeners.get(event)?.delete(handler);
}

function emit(event, detail) {
  // The pending divergence choice is remembered centrally so a late-mounting
  // view can ask for it instead of having to have been listening.
  if (event === "backlog") app.backlog = detail ?? null;
  for (const handler of listeners.get(event) ?? []) {
    try {
      handler(detail);
    } catch (error) {
      console.error(`bus handler for ${event} failed`, error);
    }
  }
}

function getState() {
  return app.state;
}

/**
 * Merge a patch into the store, notify subscribers, and schedule a write.
 * Writes coalesce: a burst of `setState` calls produces one atomic file
 * replacement rather than one per call.
 */
function setState(patch) {
  const changed = {};
  for (const [key, value] of Object.entries(patch)) {
    if (!(key in DEFAULT_STATE)) {
      console.warn(`ignoring unpersisted state key ${key}`);
      continue;
    }
    if (Object.is(app.state[key], value)) continue;
    app.state[key] = value;
    changed[key] = value;
  }
  if (Object.keys(changed).length === 0) return;

  pendingPatch = { ...(pendingPatch ?? {}), ...changed };
  if (flushTimer === null) flushTimer = setTimeout(flushState, 150);
  emit("state", changed);
  refreshShellChrome();
}

async function flushState() {
  flushTimer = null;
  const patch = pendingPatch;
  pendingPatch = null;
  if (!patch || app.persistence !== "host") return;
  try {
    await host.stateSet(patch);
  } catch (error) {
    app.persistence = "memory";
    setStatus("Changes are not being saved to disk.", "err");
    toast("Could not save app state", {
      kind: "err",
      body: error instanceof HostError ? error.message : String(error),
    });
  }
}

function flushStateNow() {
  if (flushTimer !== null) {
    clearTimeout(flushTimer);
    flushTimer = null;
  }
  void flushState();
}

// -------------------------------------------------------- last-edited 7.3 ---
//
// Fed here rather than in a view, because the ledger has to keep filling while
// the user is anywhere in the app — a divergence modal that opened on Tuesday
// sorts by what the feed saw on Monday.
const lastEdited = createLastEditedStore({ getState, setState });

// ----------------------------------------------------------------- theme ---

function getAppearanceTheme() {
  return normalizeTheme(app.state.appearanceTheme);
}

function setAppearanceTheme(value) {
  const preference = normalizeTheme(value);
  setState({ appearanceTheme: preference });
  const resolved = applyTheme(document.documentElement, preference);
  emit("theme", resolved);
  return resolved;
}

// ------------------------------------------------------- status / toasts ---

function setStatus(message, kind) {
  $statusLeft.textContent = message || "Ready";
  $statusLeft.dataset.kind = kind || "";
}

function toast(title, { kind = "", body = "", timeout = 4800 } = {}) {
  const node = document.createElement("div");
  node.className = `toast${kind ? ` toast-${kind}` : ""}`;

  const copy = document.createElement("div");
  const heading = document.createElement("div");
  heading.className = "toast-title";
  heading.textContent = title;
  copy.append(heading);
  if (body) {
    const detail = document.createElement("div");
    detail.className = "toast-body";
    detail.textContent = body;
    copy.append(detail);
  }
  node.append(copy);
  $toasts.append(node);

  const dismiss = () => {
    node.classList.add("is-leaving");
    setTimeout(() => node.remove(), 180);
  };
  node.addEventListener("click", dismiss);
  if (timeout > 0) setTimeout(dismiss, timeout);
  return dismiss;
}

// ------------------------------------------------------------ escape key ---

// Overlays push a handler; Escape always resolves the topmost one first, so a
// modal opened over a dialog closes in the order the user sees.
const escapeStack = [];

function pushEscape(handler) {
  escapeStack.push(handler);
  return () => {
    const index = escapeStack.lastIndexOf(handler);
    if (index >= 0) escapeStack.splice(index, 1);
  };
}

document.addEventListener("keydown", (event) => {
  if (event.key !== "Escape") return;
  const handler = escapeStack.at(-1);
  if (handler) {
    event.preventDefault();
    handler();
    return;
  }
  // Nothing is open: give focus back to the view instead of leaving the user
  // stuck in a search field.
  const active = document.activeElement;
  if (active && active !== document.body && typeof active.blur === "function") {
    active.blur();
    $view.focus({ preventScroll: true });
  }
});

// ------------------------------------------------------- project registry ---

/** Design 8.3: `p_<base36-time><rand4>`. */
function newProjectId() {
  const time = Date.now().toString(36);
  const random = Math.floor(Math.random() * 36 ** 4)
    .toString(36)
    .padStart(4, "0");
  return `p_${time}${random}`;
}

function projects() {
  return Array.isArray(app.state.projects) ? app.state.projects : [];
}

function getProject(projectId) {
  return projects().find((project) => project.id === projectId) ?? null;
}

function addProject({ name, path }) {
  const existing = projects().find((project) => project.path === path);
  if (existing) return { project: existing, added: false };

  const project = {
    id: newProjectId(),
    name: name || "Untitled project",
    path,
    addedAt: new Date().toISOString(),
    gameId: null,
    groupId: null,
    placeIds: [],
    wallyEnabled: false,
    wallyFolder: null,
    wallyFile: null,
    settings: {},
  };
  setState({ projects: [...projects(), project], activeProjectId: project.id });
  return { project, added: true };
}

function updateProject(projectId, patch) {
  const next = projects().map((project) =>
    project.id === projectId ? { ...project, ...patch } : project,
  );
  setState({ projects: next });
  return getProject(projectId);
}

function removeProject(projectId) {
  // Forgetting a project must not leave its daemon running: the app would have
  // no way left to reach or stop it.
  if (isProjectServed(projectId) && IS_TAURI) {
    void host.daemonStop(projectId).catch(() => {});
  }
  const next = projects().filter((project) => project.id !== projectId);
  const sessions = { ...app.state.daemonSessions };
  delete sessions[projectId];
  app.daemonFailures.delete(projectId);
  app.conflictsUnsupported.delete(projectId);
  app.plugin.delete(projectId);
  // A forgotten project's edit history is not something to keep paying for.
  lastEdited.forget(projectId);
  setState({
    projects: next,
    daemonSessions: sessions,
    servedProjectIds: servedProjectIds().filter((id) => id !== projectId),
    activeProjectId: app.state.activeProjectId === projectId ? (next[0]?.id ?? null) : app.state.activeProjectId,
  });
  refreshLink();
}

function setActiveProject(projectId) {
  setState({ activeProjectId: projectId });
  // The socket follows the active project when that project is served.
  refreshLink();
}

// ------------------------------------------------ projects folder + broker --
//
// Design 8.4. The host is authoritative on both halves: it persists the root
// and it owns the listener, so everything here is "ask, then mirror". The
// store's `projectsRoot` is kept in step so a view can read it synchronously.

function getBrokerStatus() {
  return app.broker;
}

function applyBrokerAnswer(answer) {
  if (answer?.broker) app.broker = answer.broker;
  setState({ projectsRoot: answer?.root ?? null });
  emit("broker", app.broker);
  return answer;
}

/** Ask the host where the broker stands. Never throws — Settings polls it. */
async function refreshBrokerStatus() {
  if (!IS_TAURI) return app.broker;
  try {
    app.broker = await host.brokerStatus();
    emit("broker", app.broker);
  } catch {
    // A host that cannot answer is not a reason to overwrite what we last knew.
  }
  return app.broker;
}

/** Authorize a folder through the native picker; the host starts the broker. */
async function setProjectsRoot() {
  return applyBrokerAnswer(await host.projectsRootSet());
}

/** Withdraw authorization; the host stops the broker and frees the port. */
async function clearProjectsRoot() {
  return applyBrokerAnswer(await host.projectsRootClear());
}

function servedProjectIds() {
  return Array.isArray(app.state.servedProjectIds) ? app.state.servedProjectIds : [];
}

function isProjectServed(projectId) {
  return servedProjectIds().includes(projectId);
}

// ------------------------------------------------------- daemon sessions ----
//
// The host owns the daemon children and their owner tokens; this half owns the
// session record (Design 8.3) and the intent (`servedProjectIds`). The token is
// deliberately absent from everything below — the webview never sees it, and it
// is never written to `state.json`.

function getDaemonFailure(projectId) {
  return app.daemonFailures.get(projectId) ?? null;
}

/** `http://127.0.0.1:<port>` for a served project, or null. */
function getDaemonBase(projectId) {
  return app.state.daemonSessions?.[projectId]?.base ?? null;
}

function getDaemonSession(projectId) {
  return app.state.daemonSessions?.[projectId] ?? null;
}

function writeSession(projectId, session) {
  setState({ daemonSessions: { ...app.state.daemonSessions, [projectId]: session } });
}

function dropSession(projectId, { keepServed = false } = {}) {
  const sessions = { ...app.state.daemonSessions };
  delete sessions[projectId];
  setState({
    daemonSessions: sessions,
    ...(keepServed ? {} : { servedProjectIds: servedProjectIds().filter((id) => id !== projectId) }),
  });
}

function recordFailure(projectId, error) {
  const hostError = error instanceof HostError ? error : new HostError("unknown", String(error));
  const failure = {
    code: hostError.code,
    message: hostError.message,
    /** True when nothing is wrong with the project — the capability is absent. */
    pending: hostError.isNotImplemented || hostError.isHostless,
    at: new Date().toISOString(),
  };
  app.daemonFailures.set(projectId, failure);
  // Design 8.3 keeps the last outcome in the session record, so a failed serve
  // is still visible after a restart rather than quietly forgotten.
  writeSession(projectId, {
    projectId,
    ok: false,
    error: { code: failure.code, message: failure.message },
    at: failure.at,
  });
  return hostError;
}

/**
 * Read the host's view of a project's daemon: its registry record reconciled
 * against a live `/hello`. Never throws — this is polled.
 */
async function daemonStatus(projectId) {
  try {
    return await host.daemonStatus(projectId);
  } catch (error) {
    return {
      projectId,
      supported: false,
      state: "unavailable",
      detail: error instanceof HostError ? error.message : String(error),
      running: false,
      port: null,
      bootId: null,
      pluginConnected: null,
      session: null,
      hello: null,
    };
  }
}

async function serveProject(projectId) {
  const project = getProject(projectId);
  if (!project) return false;
  setState({ activeProjectId: projectId });
  setStatus(`Starting ${project.name}…`, "warn");

  try {
    const session = await host.daemonStart(projectId, project.path, project.settings?.port ?? null);
    app.daemonFailures.delete(projectId);
    app.conflictsUnsupported.delete(projectId);
    setState({
      servedProjectIds: [...new Set([...servedProjectIds(), projectId])],
      daemonSessions: { ...app.state.daemonSessions, [projectId]: session },
    });
    setStatus(
      session.alreadyRunning
        ? `${project.name} joined a daemon already running on port ${session.port}.`
        : `${project.name} is serving on port ${session.port}.`,
      "ok",
    );
    if (session.alreadyRunning) {
      toast("Joined a running daemon", {
        kind: "warn",
        body: `Port ${session.port} was already serving this project. WSync is driving it, but another process owns it — stopping is a request, not a kill.`,
      });
    }
    emit("daemon", { projectId, ok: true, session });
    refreshLink();
    void pollBacklog();
    return true;
  } catch (error) {
    const hostError = recordFailure(projectId, error);
    setStatus(
      hostError.isHostless ? "Serving needs the WSync desktop host." : `Could not serve ${project.name}.`,
      hostError.isHostless ? "warn" : "err",
    );
    emit("daemon", { projectId, ok: false, error: hostError });
    return false;
  }
}

async function stopProject(projectId) {
  const project = getProject(projectId);
  if (!project) return true;
  try {
    const outcome = await host.daemonStop(projectId);
    app.daemonFailures.delete(projectId);
    app.conflictsUnsupported.delete(projectId);
    dropSession(projectId);
    setStatus(
      outcome === "requested"
        ? `${project.name} was asked to stop; that daemon is managed elsewhere.`
        : `${project.name} stopped.`,
      "",
    );
    emit("daemon", { projectId, ok: false, outcome });
    refreshLink();
    void pollBacklog();
    return true;
  } catch (error) {
    const hostError = recordFailure(projectId, error);
    // The intent is gone either way: a stop that failed still means the user
    // does not want this project served, and leaving it in `servedProjectIds`
    // would keep the shell claiming a daemon that may not exist.
    setState({ servedProjectIds: servedProjectIds().filter((id) => id !== projectId) });
    emit("daemon", { projectId, ok: false, error: hostError });
    refreshLink();
    return false;
  }
}

/**
 * Reconcile persisted sessions with reality at boot.
 *
 * The host kills its daemons on exit, so a session in `state.json` usually
 * describes a process that is gone. Rather than trust it — or silently restart
 * things the user did not ask for on this run — every remembered session is
 * checked and dropped if the host is not tracking it.
 */
async function reconcileSessions() {
  if (!IS_TAURI) return;
  const remembered = servedProjectIds().filter((id) => getProject(id));
  if (remembered.length === 0) {
    if (servedProjectIds().length > 0) setState({ servedProjectIds: [] });
    return;
  }

  const alive = [];
  const sessions = { ...app.state.daemonSessions };
  for (const projectId of remembered) {
    const status = await daemonStatus(projectId);
    if (status.running && status.session) {
      alive.push(projectId);
      sessions[projectId] = status.session;
    } else {
      delete sessions[projectId];
    }
  }

  const dropped = remembered.length - alive.length;
  setState({ servedProjectIds: alive, daemonSessions: sessions });
  if (dropped > 0) {
    setStatus(
      `${dropped} ${dropped === 1 ? "project is" : "projects are"} no longer served — daemons stop with the app.`,
      "warn",
    );
  }
}

// ----------------------------------------------------- daemon WS link -------

/**
 * Which served project the socket follows: the active one when it is served,
 * otherwise the first served project. One link, retargeted — the app never
 * holds several sockets open (Design 3.2: one Studio connection per daemon).
 */
function linkProjectId() {
  const served = servedProjectIds();
  if (served.length === 0) return null;
  const active = app.state.activeProjectId;
  return served.includes(active) ? active : served[0];
}

function refreshLink() {
  const projectId = linkProjectId();
  const base = projectId ? getDaemonBase(projectId) : null;
  link.setTarget(projectId && base ? { projectId, base } : null);
}

const link = createDaemonLink({
  onState(status) {
    app.link = status;
    emit("link", status);
    refreshShellChrome();
  },
  onFrame(frame, context) {
    if (frame.type === "event") return onDaemonEvent(frame, context.projectId);
    if (frame.type === "shutdown") {
      const retryable = frame.retryable !== false;
      toast(retryable ? "Daemon disconnected" : "Daemon stopped", {
        kind: retryable ? "warn" : "err",
        body: typeof frame.reason === "string" ? frame.reason : "The daemon closed the connection.",
      });
      return;
    }
    // `sync`, `details`, `execute`, `push-result` and `request` are the Studio
    // plugin's traffic. The app subscribes to the event feed only, so anything
    // else arriving here is informational.
  },
});

function onDaemonEvent(frame, projectId) {
  // The feed's own allowlist formatter runs in views/active.js; the bus carries
  // the frame as it arrived so any other subscriber sees the same thing.
  emit("activity", { ...frame, projectId });

  // Design 7.3: the last-edited store is fed by sanitized activity events. Only
  // `names` is read, and only the store's own allowlist decides what a name is
  // — nothing else from a `sync-activity` frame is retained anywhere.
  if (frame.topic === "sync-activity") {
    lastEdited.record(projectId, frame.names, frame.at);
  }

  switch (frame.topic) {
    case "plugin-status": {
      const status = {
        projectId,
        connected: frame.connected === true,
        place: typeof frame.place === "string" ? frame.place : null,
        placeId: Number.isFinite(frame.placeId) ? frame.placeId : null,
        clientName: typeof frame.clientName === "string" ? frame.clientName : null,
      };
      app.plugin.set(projectId, status);
      emit("plugin", status);
      break;
    }
    case "conflict": {
      // Design 8.2: the 20 s poll plus event-driven invalidation. The badge
      // goes to "unknown" first so it can never show a stale number while the
      // re-read is in flight.
      conflictBadge.invalidate();
      emit("conflict", {
        projectId,
        id: typeof frame.id === "string" ? frame.id : null,
        path: typeof frame.path === "string" ? frame.path : null,
        instancePath: typeof frame.instancePath === "string" ? frame.instancePath : null,
        classification: typeof frame.classification === "string" ? frame.classification : null,
      });
      void pollBacklog();
      break;
    }
    case "project-init":
      // The daemon's own view of a project-init. The authoritative one is the
      // host's `project-init` event (the broker did the work and holds the
      // record), so this is relayed to the bus and nothing more — a second
      // toast for one creation would just be noise.
      emit("project-init-activity", { projectId, at: frame.at ?? null });
      break;
    default:
      break;
  }
}



// ------------------------------------------------------------- daemon HTTP --

/**
 * Read an allowlisted daemon route (Design 5.2) through the host. Resolves for
 * any status: `{status, ok, body, text}`.
 */
/** A daemon answer that is not ok, turned into the error a view can show. */
function daemonRefusal(what, response) {
  const detail =
    (response?.body && (response.body.error ?? response.body.message)) ??
    `HTTP ${response?.status ?? "?"}`;

  return new HostError(response?.status === 503 ? "unavailable" : "daemon", `${what}: ${detail}`);
}

/**
 * `GET /backlog` — the disk content that lost to Studio.
 *
 * Sync never asks a question, so this is a list of recoverable losers rather
 * than anything pending: reading it changes nothing, and not reading it costs
 * nothing but the entries' one-day life.
 */
async function fetchBacklog(projectId) {
  const response = await daemonFetch(projectId, "/backlog");

  if (response.status === 404) {
    throw new HostError("unavailable", "This engine has no /backlog route.");
  }
  if (!response.ok) throw daemonRefusal("GET /backlog", response);

  const body = response.body ?? {};
  const entries = Array.isArray(body.entries) ? body.entries : [];

  return {
    projectId,
    total: Number(body.total) || entries.length,
    ttlSeconds: Number(body.ttlSeconds) || 0,
    entries,
  };
}

/**
 * `POST /backlog/restore` — put one entry back on disk and push it to Studio.
 *
 * A 503 means Studio is not connected, which callers treat as a wait rather
 * than a failure: restoring without a live channel would leave the file on disk
 * for the next connect to send straight back here.
 */
async function restoreBacklogEntry(projectId, options = {}) {
  const id = typeof options.id === "string" ? options.id.trim() : "";
  if (!id) throw new HostError("invalid_argument", "a backlog id is required");

  const response = await daemonPost(projectId, "/backlog/restore", { id });
  if (!response.ok) throw daemonRefusal("POST /backlog/restore", response);

  return response.body ?? {};
}

/** `POST /backlog/drop` — forget one entry, or all of them, without restoring. */
async function dropBacklogEntries(projectId, options = {}) {
  const payload = options.all === true ? { all: true } : { id: String(options.id ?? "").trim() };
  if (payload.id === "") throw new HostError("invalid_argument", "a backlog id is required");

  const response = await daemonPost(projectId, "/backlog/drop", payload);
  if (!response.ok) throw daemonRefusal("POST /backlog/drop", response);

  return response.body ?? {};
}

async function daemonFetch(projectId, route) {
  return host.daemonRequest(projectId, route);
}

/** Write an allowlisted daemon route. Same "any status resolves" contract. */
async function daemonPost(projectId, route, body) {
  return host.daemonPost(projectId, route, body);
}

// --------------------------------------------------------- conflict polling --

/**
 * Keep the backlog badge honest on a timer.
 *
 * The `backlog` event is the fast path, but not the only one: content that
 * lost while this app was closed would otherwise never show a count. Reading
 * the list is cheap and changes nothing, so it rides the same cadence the
 * conflict poll used to.
 */
async function pollBacklog() {
  await refreshBacklog();

  const total = app.backlog?.total ?? 0;

  conflictBadge.report(total);
}

function startBacklogPolling() {
  if (conflictTimer !== null) return;
  conflictTimer = setInterval(() => void pollBacklog(), CONFLICT_POLL_MS);
  void pollBacklog();
}

/**
 * Poll `GET /backlog` and keep the banner's count honest.
 *
 * Nothing here opens itself. Sync never asks a question, so the backlog is
 * something you go and look at when you want it — a window that appeared over
 * your work to tell you a file lost would be the interruption this whole model
 * exists to remove.
 *
 * An engine without `/backlog` (or one not answering) leaves whatever the
 * banner already says alone: claiming "nothing waiting" on a failed read would
 * hide entries that are really there.
 */
async function refreshBacklog() {
  const projectId = linkProjectId();
  if (!IS_TAURI || !projectId) {
    if (app.backlog) emit("backlog", null);
    return;
  }

  let backlog;
  try {
    backlog = await fetchBacklog(projectId);
  } catch {
    return;
  }

  const summary = backlog.total > 0 ? { projectId, total: backlog.total } : null;
  const known = app.backlog;

  if (known?.total !== summary?.total || known?.projectId !== summary?.projectId) {
    emit("backlog", summary);
  }
}

/**
 * Open the backlog window. One at a time, and only ever because the user asked
 * — there is no auto-open counterpart, by design.
 */
function openBacklog(options = {}) {
  if (app.backlogWindow) return app.backlogWindow;

  const handle = openBacklogModal(buildApi(), {
    ...options,
    projectId: options.projectId ?? linkProjectId(),
    onClosed: () => {
      app.backlogWindow = null;
      void refreshBacklog();
    },
  });

  app.backlogWindow = handle?.close ? handle : null;
  return handle;
}

// --------------------------------------------------------- shell chrome ----

function refreshShellChrome() {
  const total = projects().length;
  $projectsCount.hidden = total === 0;
  $projectsCount.textContent = String(total);

  const served = servedProjectIds().length;
  const failed = [...app.daemonFailures.values()].filter((failure) => !failure.pending).length;
  let kind = "idle";
  let label = "idle";
  if (served > 0) {
    kind = failed > 0 ? "err" : "ok";
    label = `${served} ${served === 1 ? "project" : "projects"} serving`;
    // The socket is what actually carries updates, so a served project whose
    // link is down must not read as a healthy green dot.
    if (app.link.state === LINK_STATE.RECONNECTING) {
      kind = "warn";
      label += " · reconnecting";
    } else if (app.link.state === LINK_STATE.STOPPED) {
      kind = "err";
      label += " · updates stopped";
    } else if (app.link.state === LINK_STATE.CONNECTING) {
      label += " · connecting";
    }
  } else if (total > 0) {
    label = "no project served";
  } else {
    label = "no projects";
  }
  $daemonDot.className = `dot dot-${kind}`;
  $daemonDot.title = `Daemon: ${label}`;
  $statusRight.textContent = `daemon: ${label}`;
  $root.dataset.connection = kind;

  refreshTopbar();
}

function refreshTopbar() {
  const route = ROUTES[app.currentRoute] ?? ROUTES[DEFAULT_ROUTE];
  $topbarTitle.textContent = route.label;
  const active = getProject(app.state.activeProjectId);
  $topbarDetail.textContent = active ? active.path : "";
  $topbarDetail.title = active ? active.path : "";
}

// Design 8.2: 20 s poll + event invalidation, `N+` while partial. `null` is a
// real answer — "not known" — and is never rendered as zero.
const conflictBadge = {
  report(count, { partial = false, note = "" } = {}) {
    app.conflictCount = count;
    if (count === null || count === undefined) {
      $conflictsBadge.hidden = false;
      $conflictsBadge.dataset.state = "unknown";
      $conflictsBadge.textContent = "—";
      $conflictsBadge.title = note || "Backlog has not been checked";
      return;
    }
    if (count === 0) {
      $conflictsBadge.hidden = true;
      return;
    }
    $conflictsBadge.hidden = false;
    $conflictsBadge.dataset.state = "some";
    $conflictsBadge.textContent = partial ? `${count}+` : String(count);
    $conflictsBadge.title = `${count}${partial ? " or more" : ""} files waiting in the backlog`;
  },
  invalidate() {
    app.conflictCount = null;
    $conflictsBadge.hidden = true;
  },
};

// ------------------------------------------------------------ navigation ---

function buildApi() {
  return {
    // store
    getState,
    setState,
    onBus: on,
    emitBus: emit,

    // appearance
    getAppearanceTheme,
    setAppearanceTheme,
    themeOptions: THEME_OPTIONS,

    // registry
    projects,
    getProject,
    addProject,
    updateProject,
    removeProject,
    setActiveProject,
    isProjectServed,
    servedProjectIds,

    // projects folder + broker (Design 8.4)
    getBrokerStatus,
    refreshBrokerStatus,
    setProjectsRoot,
    clearProjectsRoot,
    revealProjectsRoot: () => host.projectsRootReveal(),

    // daemon lifecycle + transport
    serveProject,
    stopProject,
    daemonStatus,
    getDaemonBase,
    getDaemonSession,
    getDaemonFailure,
    daemonFetch,

    // the live WS link (ws.js)
    getLinkState: () => app.link,
    retryLink: () => link.retryNow(),
    linkStates: LINK_STATE,
    getPluginStatus: (projectId) => app.plugin.get(projectId ?? linkProjectId()) ?? null,

    // conflicts + divergence data layer
    refreshBacklog: () => pollBacklog(),
    // the backlog
    fetchBacklog,
    restoreBacklogEntry,
    dropBacklogEntries,

    // the last-edited ledger (Design 7.3), read-only from a view's side
    lastEditedStamps: (projectId) => lastEdited.stampsFor(projectId),
    hasLastEdited: (projectId) => lastEdited.has(projectId),

    // chrome
    setStatus,
    toast,
    pushEscape,
    navigate,
    reportConflictCount: conflictBadge.report,
    invalidateConflictCount: conflictBadge.invalidate,
    getBacklog: () => app.backlog,
    getServedProjectId: () => linkProjectId(),
    openBacklog: (options) => openBacklog(options),

    // host
    host,
    appInfo: () => app.info,
    isTauri: IS_TAURI,
    persistence: () => app.persistence,
  };
}

function navigate(route) {
  const next = ROUTES[route] ? route : DEFAULT_ROUTE;
  if (app.currentRoute === next) return;

  if (typeof app.unmountCurrent === "function") {
    try {
      app.unmountCurrent();
    } catch (error) {
      console.error("view unmount failed", error);
    }
  }
  app.unmountCurrent = null;
  app.currentRoute = next;
  $view.replaceChildren();

  for (const tab of $tabs) {
    const selected = tab.dataset.route === next;
    tab.setAttribute("aria-selected", selected ? "true" : "false");
    tab.tabIndex = selected ? 0 : -1;
    if (selected) $view.setAttribute("aria-labelledby", tab.id);
  }
  $root.dataset.view = next;
  refreshTopbar();

  try {
    app.unmountCurrent = ROUTES[next].mount($view, buildApi()) ?? null;
  } catch (error) {
    console.error(`mounting ${next} failed`, error);
    setStatus(`The ${ROUTES[next].label} view failed to load.`, "err");
  }
  setState({ lastView: next });
}

for (const [index, tab] of $tabs.entries()) {
  tab.addEventListener("click", () => navigate(tab.dataset.route));
  tab.addEventListener("keydown", (event) => {
    const keys = { ArrowDown: index + 1, ArrowUp: index - 1, Home: 0, End: $tabs.length - 1 };
    if (!(event.key in keys)) return;
    event.preventDefault();
    const target = $tabs[(keys[event.key] + $tabs.length) % $tabs.length];
    target.focus();
    navigate(target.dataset.route);
  });
}

// ------------------------------------------------------------------ boot ---

async function boot() {
  // Reveal no later than 800 ms even if the host never answers (Design 8.2).
  scheduleThemeReveal(document.documentElement);
  setStatus("Loading…");

  try {
    app.info = await host.appInfo();
    $root.dataset.platform = app.info.platform;
  } catch (error) {
    app.info = null;
    $root.dataset.platform = "browser";
    if (!(error instanceof HostError && error.isHostless)) console.error(error);
  }

  if (IS_TAURI) {
    try {
      const stored = await host.stateGet();
      app.state = { ...DEFAULT_STATE, ...(stored ?? {}) };
      app.persistence = "host";
      // The ledger was built from defaults at module load; this is the first
      // moment the saved one exists to be read.
      lastEdited.rehydrate();
      setStatus("Ready");
    } catch (error) {
      app.state = { ...DEFAULT_STATE };
      app.persistence = "memory";
      const message = error instanceof HostError ? error.message : String(error);
      setStatus("Running on defaults — saved state could not be read.", "warn");
      toast("Could not read saved state", { kind: "warn", body: message });
    }
  } else {
    setStatus("Preview mode — no desktop host, nothing is saved.", "warn");
  }

  applyTheme(document.documentElement, getAppearanceTheme());
  watchSystemTheme(() => {
    if (getAppearanceTheme() === "system") {
      emit("theme", applyTheme(document.documentElement, "system"));
    }
  });

  // Design 8.5: no compiled-in key => no update affordance at all.
  $updateButton.hidden = !app.info?.updaterConfigured;

  refreshShellChrome();
  conflictBadge.invalidate();
  navigate(app.state.lastView ?? DEFAULT_ROUTE);

  installHostEventListeners();
  await reconcileSessions();
  refreshLink();
  startBacklogPolling();
  // Seed the broker mirror: `broker:up` may already have fired during boot, and
  // a folder that is authorized but cannot bind never fires one at all.
  void refreshBrokerStatus();

  // Dev affordance: `index.html?dev=backlog` opens the backlog window so the
  // layout can be reviewed without waiting for something to lose to Studio.
  if (new URLSearchParams(location.search).get("dev") === "backlog") {
    openBacklog({});
  }
}

/**
 * Design 8.4: a project the broker created, merged into the registry live.
 *
 * The host has already written it to `state.json` and started its daemon, so
 * this is a *merge*, not a create: the record arrives whole, it is keyed by id,
 * and re-applying it (a replayed request, a second window) is a no-op rather
 * than a duplicate card. Nothing is re-read from disk — that is what makes the
 * Projects view update without a reload.
 */
function onProjectInit(event) {
  const record = event?.project;
  if (!record?.id) return;

  const known = projects().some((project) => project.id === record.id);
  setState({
    projects: known
      ? projects().map((project) => (project.id === record.id ? { ...project, ...record } : project))
      : [...projects(), record],
  });
  // The user is in Studio, not here; when they look back, the project they just
  // created should be the one selected.
  setActiveProject(record.id);
  emit("project-init", event);

  if (!known) {
    toast("Studio created a project", {
      kind: "ok",
      body:
        event.status === "existing"
          ? `${record.name} already existed — WSync is serving it.`
          : `${record.name} was created in your projects folder and is starting now.`,
    });
    setStatus(`${record.name} was created from Studio.`, "ok");
  }
}

/**
 * The host emits a lifecycle transition for every daemon it owns, including the
 * ones nobody asked about: a crash, a heartbeat that stopped answering, a
 * process killed from outside. Without these the UI would keep drawing a
 * session that no longer exists until the next poll.
 */
function installHostEventListeners() {
  // Design 8.4: the broker's own transitions. `broker:up` also fires at boot
  // for a folder authorized in an earlier run, which is the only way the
  // frontend learns about that one without polling.
  onHostEvent("broker:up", (status) => {
    if (!status) return;
    app.broker = status;
    if (status.root) setState({ projectsRoot: status.root });
    emit("broker", status);
  });

  onHostEvent("broker:down", (status) => {
    app.broker = status ?? { running: false, port: null, root: null, detail: "Off." };
    emit("broker", app.broker);
  });

  onHostEvent("project-init", onProjectInit);

  onHostEvent("daemon:up", (session) => {
    if (!session?.projectId) return;
    app.daemonFailures.delete(session.projectId);
    setState({
      servedProjectIds: [...new Set([...servedProjectIds(), session.projectId])],
      daemonSessions: { ...app.state.daemonSessions, [session.projectId]: session },
    });
    emit("daemon", { projectId: session.projectId, ok: true, session });
    refreshLink();
    link.retryNow();
  });

  onHostEvent("daemon:down", (event) => {
    const projectId = event?.projectId;
    if (!projectId) return;
    // A `stopped` reason is our own teardown; the command already updated the
    // store and said so. Everything else is news.
    if (event.reason !== "stopped") {
      const project = getProject(projectId);
      app.daemonFailures.set(projectId, {
        code: event.reason ?? "exited",
        message: event.detail ?? "The daemon is no longer running.",
        pending: false,
        at: new Date().toISOString(),
      });
      writeSession(projectId, {
        projectId,
        ok: false,
        error: { code: event.reason ?? "exited", message: event.detail ?? "" },
        at: new Date().toISOString(),
      });
      setState({ servedProjectIds: servedProjectIds().filter((id) => id !== projectId) });
      setStatus(`${project?.name ?? projectId} stopped serving.`, "err");
      toast("Daemon stopped", { kind: "err", body: event.detail ?? event.reason ?? "" });
      emit("daemon", { projectId, ok: false, reason: event.reason });
    }
    refreshLink();
    void pollBacklog();
  });
}

/** The ledger's debounced write has to land before the state file is flushed. */
function persistNow() {
  lastEdited.flush();
  flushStateNow();
}

window.addEventListener("pagehide", persistNow);
document.addEventListener("visibilitychange", () => {
  if (document.visibilityState === "hidden") persistNow();
});

boot().catch((error) => {
  console.error("boot failed", error);
  setStatus("WSync failed to start.", "err");
  document.documentElement.classList.remove("theme-pending");
});
