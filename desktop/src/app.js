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
import { mountConflicts } from "./views/conflicts.js";
import { mountDocs } from "./views/docs.js";
import { mountSettings } from "./views/settings.js";
import { openOverwriteModal } from "./views/overwrite.js";
import { openReviewModal } from "./views/review.js";
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
  conflicts: { label: "Conflicts", mount: mountConflicts },
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
  /** Not persisted: last known conflict count, or null when unchecked. */
  conflictCount: null,
  /**
   * Not persisted: the divergence choice waiting for an answer, or null.
   * It lives here rather than in the Conflicts view so the banner survives
   * navigating away and back — a pending choice is app state, not view state.
   */
  pendingDivergence: null,
  /**
   * Not persisted: the open divergence modal's handle, or null. The app holds
   * it so a `choice-made` from anywhere — the CLI, another window, the plugin's
   * fallback prompt — can close a modal that is now answering a dead question.
   */
  overwrite: null,
  /**
   * Not persisted: Design 7.0's pending disk review, or null. Same reasoning as
   * `pendingDivergence` — the banner has to survive navigating away — but a
   * very different thing: the sync already happened, and this is the optional
   * list of disk items that did not survive it.
   */
  pendingReview: null,
  /** Not persisted: the open disk-review modal's handle, or null. */
  review: null,
  /**
   * Not persisted: reviewIds whose modal has already auto-opened once. The
   * initial-sync popup opens itself exactly once per review — a reconnect that
   * re-broadcasts the same set must not fight a user who closed it, while a
   * *new* reviewId (a superseding connect) opens fresh.
   */
  reviewAutoShown: new Set(),
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
  if (event === "divergence") app.pendingDivergence = detail ?? null;
  if (event === "review") app.pendingReview = detail ?? null;
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
    void pollConflicts();
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
    void pollConflicts();
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
      void pollConflicts();
      break;
    }
    case "choice-needed": {
      const summary = divergenceSummary(frame, projectId);
      // A daemon re-announces the pending choice to every client that
      // connects, so a flapping link would otherwise toast on every reconnect.
      // The set is identified by its choiceId; the same one is not news.
      const known = app.pendingDivergence?.choiceId === summary.choiceId;
      emit("divergence", summary);
      // A modal open on a superseded set is worse than no modal: the staging
      // ids it holds address a divergence set the daemon has already replaced.
      app.overwrite?.supersede(summary);
      if (!known) {
        toast("Studio and disk are different", {
          kind: "warn",
          body: "Nothing changes until you choose a side. Open Conflicts to review.",
        });
      }
      break;
    }
    case "disk-review": {
      // The initial-sync review. Nothing about this blocks the sync: the
      // daemon has already applied Studio → disk and live sync is running.
      // But it *is* a decision the user should see, so the picker opens
      // itself — once per reviewId — as a dismissible overlay; closing it
      // leaves the banner to reopen from.
      const summary = reviewSummary(frame, projectId);
      const known = app.pendingReview?.reviewId === summary.reviewId;
      emit("review", summary);
      // A new connect replaces the pending review, so a modal holding the old
      // one is holding ids the daemon has dropped. Supersede closes it, which
      // is what lets the auto-open below show the new set.
      app.review?.supersede(summary);
      const opened = maybeAutoOpenReview(summary);
      if (!known && summary.total > 0 && !opened) {
        toast("Disk changes await review", {
          kind: "",
          body: `Studio is synced to disk. ${summary.total.toLocaleString()} disk ${summary.total === 1 ? "item" : "items"} can be pushed back if you want them.`,
        });
      }
      break;
    }
    case "choice-made":
      emit("divergence", null);
      // Ours or someone else's: the modal decides, because only it knows
      // whether it is mid-submission (see `resolvedElsewhere` in overwrite.js).
      app.overwrite?.resolvedElsewhere(
        typeof frame.choiceId === "string" ? frame.choiceId : null,
        typeof frame.choice === "string" ? frame.choice : null,
      );
      break;
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

/** Design 7.0's `disk-review` event: aggregate counts, paged details. */
function reviewSummary(frame, projectId) {
  const count = (value) => (Number.isFinite(value) ? Number(value) : 0);
  return {
    projectId,
    reviewId: typeof frame.reviewId === "string" ? frame.reviewId : null,
    total: count(frame.total),
    diskOnly: count(frame.diskOnly),
    differs: count(frame.differs),
  };
}

/** Aggregate counts only — the detail list is paged from the daemon (§7.3). */
function divergenceSummary(frame, projectId) {
  const count = (value) => (Number.isFinite(value) ? Number(value) : 0);
  return {
    projectId,
    choiceId: typeof frame.choiceId === "string" ? frame.choiceId : null,
    total: count(frame.total),
    studioCount: count(frame.studioCount),
    diskCount: count(frame.diskCount),
    onlyOnDisk: count(frame.onlyOnDisk),
    differs: count(frame.differs),
    missingOnDisk: count(frame.missingOnDisk),
    fixture: false,
  };
}

// ------------------------------------------------------------- daemon HTTP --

/**
 * Read an allowlisted daemon route (Design 5.2) through the host. Resolves for
 * any status: `{status, ok, body, text}`.
 */
async function daemonFetch(projectId, route) {
  return host.daemonRequest(projectId, route);
}

/** Write an allowlisted daemon route. Same "any status resolves" contract. */
async function daemonPost(projectId, route, body) {
  return host.daemonPost(projectId, route, body);
}

// ------------------------------------------------------ divergence (7.3) ----
//
// The client half of the choice contract. Everything here treats the daemon as
// something to be *checked*, not trusted: Design 7.3 pages the divergence set
// with dense sequential ids and answers every selection chunk with a receipt,
// and both exist so a client can prove it received the set it acted on. A page
// or a receipt that does not add up aborts loudly — a silently wrong selection
// would overwrite the wrong instances in someone's place file.

/** Contract: ≤1024 per page. Half that keeps one page's payload small. */
const CHOICE_PAGE_LIMIT = 512;
/** Design 7.3: ≤2048 ids and ≤64 KiB per selection chunk. */
const SELECTION_CHUNK_IDS = 2048;
const SELECTION_CHUNK_BYTES = 64 * 1024;
/** Room for `{choiceId, submissionId, chunkIndex, finalChunk, restart}`. */
const SELECTION_ENVELOPE_BYTES = 512;

const DIVERGENCE_STATES = new Set(["only-on-disk", "differs", "missing-on-disk"]);

function emptyChoiceStats() {
  return { total: 0, studioCount: 0, diskCount: 0, onlyOnDisk: 0, differs: 0, missingOnDisk: 0 };
}

function countOf(value) {
  return Number.isFinite(value) ? Number(value) : 0;
}

function normalizeChoiceStats(stats) {
  const source = stats && typeof stats === "object" ? stats : {};
  return {
    total: countOf(source.total),
    studioCount: countOf(source.studioCount),
    diskCount: countOf(source.diskCount),
    onlyOnDisk: countOf(source.onlyOnDisk),
    differs: countOf(source.differs),
    missingOnDisk: countOf(source.missingOnDisk),
  };
}

function requireChoiceId(choiceId) {
  if (typeof choiceId !== "string" || choiceId.trim() === "") {
    throw new HostError("invalid_argument", "no choiceId was supplied");
  }
  return choiceId;
}

/** The daemon answered, and its answer was not usable. Never a silent retry. */
function protocolError(message) {
  return new HostError("protocol", message);
}

function daemonRefusal(route, response) {
  const detail =
    (typeof response.body?.error === "string" && response.body.error) ||
    (typeof response.body?.message === "string" && response.body.message) ||
    response.text ||
    "";
  const error = new HostError(
    "daemon",
    `${route} answered ${response.status}${detail ? `: ${detail}` : "."}`,
  );
  // The numeric status rides along for callers that can do better than a
  // message — a 503 on a review push means "no Studio plugin yet", which is
  // a wait-and-retry, not a failure.
  error.status = response.status;
  return error;
}

/**
 * `GET /choice` — is a divergence decision waiting, and how big is it?
 *
 * Aggregate stats only, by design (§7.2): the path list is never broadcast, it
 * is paged on demand by `fetchDivergenceDetails`.
 */
async function fetchPendingChoice(projectId) {
  const response = await daemonFetch(projectId, "/choice");
  if (response.status === 404) {
    throw new HostError("unavailable", "This engine has no /choice route.");
  }
  if (!response.ok) throw daemonRefusal("GET /choice", response);

  const body = response.body ?? {};
  if (body.pending !== true) {
    return { projectId, pending: false, choiceId: null, stats: emptyChoiceStats() };
  }
  const choiceId = typeof body.choiceId === "string" ? body.choiceId.trim() : "";
  if (!choiceId) {
    throw protocolError("GET /choice reported a pending decision with no choiceId.");
  }
  return { projectId, pending: true, choiceId, stats: normalizeChoiceStats(body.stats) };
}

/**
 * `GET /choice/details` — one verified page of the frozen divergence set.
 *
 * One page per call, so the caller controls pacing and can render progressively
 * (Design 7.3: 25 000 entries must page, never arrive at once). The page is
 * checked before it is returned; see `verifyDetailPage` for what "checked"
 * means and why every rule is there.
 *
 * @param {string} projectId
 * @param {{choiceId: string, cursor?: number, limit?: number,
 *   expectedTotal?: number|null}} options
 */
async function fetchDivergenceDetails(projectId, options = {}) {
  const choiceId = requireChoiceId(options.choiceId);
  const cursor = Number.isInteger(options.cursor) ? options.cursor : 0;
  const limit = Math.min(Math.max(1, options.limit ?? CHOICE_PAGE_LIMIT), 1024);
  if (cursor < 0) throw new HostError("invalid_argument", "cursor cannot be negative");

  const route =
    `/choice/details?choiceId=${encodeURIComponent(choiceId)}` +
    `&cursor=${cursor}&limit=${limit}`;
  const response = await daemonFetch(projectId, route);

  if (response.status === 404 || response.status === 409) {
    // The set is frozen and immutable: it can only vanish by being resolved or
    // restarted, and either way anything already staged is stale.
    throw new HostError(
      "daemon",
      "The divergence set is no longer available — it was resolved or restarted.",
    );
  }
  if (!response.ok) throw daemonRefusal("GET /choice/details", response);

  return verifyDetailPage(response.body ?? {}, {
    choiceId,
    cursor,
    limit,
    expectedTotal: Number.isInteger(options.expectedTotal) ? options.expectedTotal : null,
  });
}

/**
 * Design 7.3's page-integrity contract, enforced client-side.
 *
 * The daemon promises dense sequential ids over an immutable set. That is a
 * checkable promise, and checking it is the point: ids are what a selection is
 * submitted as, so a page with a gap, a repeat, or a shifting `totalCount`
 * would submit ids that mean something different on the daemon's side than they
 * did in the list the user picked from.
 *
 * Refused, each with its own reason:
 * - a different `choiceId` (an answer about another set entirely)
 * - ids that are not exactly `cursor … cursor + n - 1` (gap, repeat, reorder)
 * - a `totalCount` that moved between pages, or is not a count
 * - more items than the requested limit, or than the set claims to hold
 * - a `nextCursor` that does not advance by exactly this page's length —
 *   including one that does not advance at all, which would page forever
 * - a last page that stops short of `totalCount`
 */
function verifyDetailPage(body, expected) {
  const fail = (why) => {
    throw protocolError(
      `GET /choice/details returned a page WSync cannot trust (${why}). ` +
        "Nothing was staged; the list was abandoned rather than acted on.",
    );
  };

  if (!body || typeof body !== "object") fail("the response was not a JSON object");
  if (typeof body.choiceId !== "string" || body.choiceId !== expected.choiceId) {
    fail(`it is about choiceId ${JSON.stringify(body.choiceId)}, not ${expected.choiceId}`);
  }
  if (!Array.isArray(body.items)) fail("items was not an array");
  if (body.items.length > expected.limit) {
    fail(`it holds ${body.items.length} items for a limit of ${expected.limit}`);
  }
  if (!Number.isInteger(body.totalCount) || body.totalCount < 0) {
    fail(`totalCount was ${JSON.stringify(body.totalCount)}`);
  }
  if (expected.expectedTotal !== null && body.totalCount !== expected.expectedTotal) {
    fail(`totalCount moved from ${expected.expectedTotal} to ${body.totalCount} mid-page`);
  }

  const items = body.items.map((item, index) => {
    const at = expected.cursor + index;
    if (!item || typeof item !== "object") fail(`entry ${at} was not an object`);
    if (item.id !== at) {
      // Dense and sequential from the cursor: this single check rules out gaps,
      // repeats and reordering in one go.
      fail(`entry ${index} carries id ${JSON.stringify(item.id)} where ${at} was due`);
    }
    if (!DIVERGENCE_STATES.has(item.state)) {
      fail(`entry ${at} has state ${JSON.stringify(item.state)}`);
    }
    if (typeof item.instancePath !== "string" || item.instancePath === "") {
      fail(`entry ${at} has no instancePath`);
    }
    if (item.path !== null && typeof item.path !== "string") {
      fail(`entry ${at} has a path that is neither a string nor null`);
    }
    return {
      id: item.id,
      path: item.path ?? null,
      instancePath: item.instancePath,
      state: item.state,
      class: typeof item.class === "string" ? item.class : null,
    };
  });

  if (expected.cursor + items.length > body.totalCount) {
    fail(`it runs past totalCount ${body.totalCount}`);
  }

  const hasNext = body.nextCursor !== undefined && body.nextCursor !== null;
  if (hasNext) {
    if (!Number.isInteger(body.nextCursor)) fail(`nextCursor was ${JSON.stringify(body.nextCursor)}`);
    if (body.nextCursor !== expected.cursor + items.length) {
      fail(
        `nextCursor ${body.nextCursor} does not follow ${items.length} items from ${expected.cursor}`,
      );
    }
    if (body.nextCursor >= body.totalCount) {
      fail(`nextCursor ${body.nextCursor} is past the end of a ${body.totalCount}-entry set`);
    }
    if (items.length === 0) fail("it is empty but claims there is more");
  } else if (expected.cursor + items.length !== body.totalCount) {
    fail(
      `it ends at ${expected.cursor + items.length} of ${body.totalCount} without a nextCursor`,
    );
  }

  return {
    choiceId: body.choiceId,
    cursor: expected.cursor,
    items,
    nextCursor: hasNext ? body.nextCursor : null,
    totalCount: body.totalCount,
  };
}

/**
 * Contract: each side of `/choice/source` is at most 256 KiB. A UTF-8 payload
 * of that size cannot exceed this many JS string units, so anything longer is
 * a contract violation rather than a big file.
 */
const SOURCE_MAX_CHARS = 256 * 1024;

/**
 * `GET /choice/source` — one divergence row's two sides, for the inline diff.
 *
 * Lazily fetched per row and never as part of the set: Design 7.2 keeps the
 * divergence payload to classifications and paths, and shipping every script's
 * text with it would turn a 25 000-entry divergence into a gigabyte.
 *
 * The four documented answers are *outcomes*, not failures, because each one is
 * something the modal has to say in a row rather than a reason to blow up:
 *
 * - `ok` — both sides, either possibly `truncated`.
 * - `not-script` (400) — a property row. Staging decides these, not a differ.
 * - `no-plugin` (503) — Studio is not connected, so the Studio side does not
 *   exist to be read.
 * - `stale` (404) — the row or the whole set is gone. The caller takes the
 *   supersede path; every id on screen now addresses a set that was replaced.
 *
 * Anything else throws, including a 200 whose body does not describe the row
 * that was asked for.
 */
async function fetchChoiceSource(projectId, options = {}) {
  const choiceId = requireChoiceId(options.choiceId);
  const id = options.id;
  if (!Number.isInteger(id) || id < 0) {
    throw new HostError("invalid_argument", "a source request needs a row id");
  }

  const route = `/choice/source?choiceId=${encodeURIComponent(choiceId)}&id=${id}`;
  const response = await daemonFetch(projectId, route);

  if (response.status === 404) return { status: "stale", id };
  if (response.status === 400) {
    return {
      status: "not-script",
      id,
      message:
        (typeof response.body?.error === "string" && response.body.error) ||
        "This row is not a script.",
    };
  }
  if (response.status === 503) {
    return {
      status: "no-plugin",
      id,
      message:
        (typeof response.body?.error === "string" && response.body.error) ||
        "No Studio plugin is connected.",
    };
  }
  if (!response.ok) throw daemonRefusal("GET /choice/source", response);

  return verifySourcePair(response.body ?? {}, { id, state: options.state ?? null });
}

/**
 * The row-source contract, checked before a line of it is rendered.
 *
 * The set is frozen and immutable (§7.2), so every one of these is a real
 * violation rather than a version skew: a body about a different row, a state
 * that moved under a frozen set, a side that is neither present nor absent, or
 * a source longer than the contract's own ceiling.
 */
function verifySourcePair(body, expected) {
  const fail = (why) => {
    throw protocolError(
      `GET /choice/source returned a row WSync cannot trust (${why}). Nothing was rendered.`,
    );
  };

  if (!body || typeof body !== "object") fail("the response was not a JSON object");
  if (body.id !== expected.id) {
    fail(`it describes row ${JSON.stringify(body.id)}, not ${expected.id}`);
  }
  if (!DIVERGENCE_STATES.has(body.state)) fail(`state was ${JSON.stringify(body.state)}`);
  if (expected.state !== null && body.state !== expected.state) {
    fail(`state moved from ${expected.state} to ${body.state} under a frozen set`);
  }

  const side = (name) => {
    const raw = body[name];
    if (!raw || typeof raw !== "object") fail(`${name} was not an object`);
    if (typeof raw.present !== "boolean") fail(`${name}.present was ${JSON.stringify(raw.present)}`);
    if (raw.source !== undefined && raw.source !== null && typeof raw.source !== "string") {
      fail(`${name}.source was neither a string nor absent`);
    }
    if (typeof raw.source === "string" && raw.source.length > SOURCE_MAX_CHARS) {
      fail(`${name}.source is longer than the 256 KiB the contract allows`);
    }
    return {
      present: raw.present,
      source: typeof raw.source === "string" ? raw.source : null,
      truncated: raw.truncated === true,
    };
  };

  return {
    status: "ok",
    id: body.id,
    path: typeof body.path === "string" && body.path !== "" ? body.path : null,
    instancePath: typeof body.instancePath === "string" ? body.instancePath : "",
    state: body.state,
    disk: side("disk"),
    studio: side("studio"),
  };
}

const CHOICE_ANSWERS = new Set(["studio", "disk", "cancel"]);

/**
 * `POST /choice` — the decision itself.
 *
 * Resolves to a typed outcome rather than a bare boolean, because the three
 * that matter read very differently to a user:
 *
 * - `applied` — the daemon did the work (Keep Disk pulls now).
 * - `pending-application` — the decision is recorded and the transfer is not
 *   built yet (Keep Studio). The caller must say exactly that; reporting it as
 *   success would promise files that never moved.
 * - `resolved-elsewhere` — a 409: the CLI or another window answered first.
 * - `cancelled` — the abort was recorded; the link stays up but unsynced.
 */
async function submitChoice(projectId, choice, options = {}) {
  if (!CHOICE_ANSWERS.has(choice)) {
    throw new HostError("invalid_argument", `${JSON.stringify(choice)} is not a divergence answer`);
  }
  const choiceId = requireChoiceId(options.choiceId);
  const payload = { choiceId, choice };
  // Design 7.3: a selection covering the whole set upgrades to `mode:"all"`.
  if (options.mode === "all") payload.mode = "all";

  const response = await daemonPost(projectId, "/choice", payload);
  if (response.status === 409 && response.body?.error === "resolved") {
    return { status: "resolved-elsewhere", choiceId };
  }
  if (!response.ok) throw daemonRefusal("POST /choice", response);

  const body = response.body ?? {};
  if (body.ok !== true) throw daemonRefusal("POST /choice", response);
  if (choice === "cancel") return { status: "cancelled", choiceId };
  if (body.applied === true) return { status: "applied", choiceId };
  if (body.pendingApplication === true) return { status: "pending-application", choiceId };
  throw protocolError(
    "POST /choice accepted the decision but reported neither applied nor pendingApplication.",
  );
}

/** A submission id is only ever compared to itself; uniqueness is all it needs. */
function newSubmissionId() {
  const uuid = globalThis.crypto?.randomUUID;
  if (typeof uuid === "function") return globalThis.crypto.randomUUID();
  return `s_${Date.now().toString(36)}_${Math.floor(Math.random() * 2 ** 32).toString(36)}`;
}

/** Split ids into chunks inside both of Design 7.3's bounds at once. */
function chunkSelection(ids) {
  const chunks = [];
  let current = [];
  let bytes = SELECTION_ENVELOPE_BYTES;
  for (const id of ids) {
    const cost = String(id).length + 1;
    if (current.length >= SELECTION_CHUNK_IDS || bytes + cost > SELECTION_CHUNK_BYTES) {
      chunks.push(current);
      current = [];
      bytes = SELECTION_ENVELOPE_BYTES;
    }
    current.push(id);
    bytes += cost;
  }
  if (current.length > 0 || chunks.length === 0) chunks.push(current);
  return chunks;
}

/**
 * Every field of a chunk receipt, checked against what was actually sent.
 *
 * Design 7.3: "any receipt mismatch aborts with an explicit abort op". There is
 * no separate abort route in the contract — the abort *is* refusing to send the
 * next chunk, which leaves the partial submission uncommitted; the daemon
 * discards it when a later submission opens with `restart: true`.
 */
function verifyReceipt(body, expected) {
  const problems = [];
  if (!body || typeof body !== "object") {
    problems.push("the receipt was not a JSON object");
  } else {
    if (body.ok !== true) problems.push(`ok was ${JSON.stringify(body.ok)}`);
    if (body.acceptedChunk !== expected.chunkIndex) {
      problems.push(`acceptedChunk was ${JSON.stringify(body.acceptedChunk)}, sent ${expected.chunkIndex}`);
    }
    if (body.selectedCount !== expected.selectedCount) {
      problems.push(
        `selectedCount was ${JSON.stringify(body.selectedCount)}, ${expected.selectedCount} ids have been sent`,
      );
    }
    if (expected.finalChunk) {
      if (body.committed !== true) problems.push("the final chunk was not committed");
      // The contract does not pin `nextChunk` past the end, so only a value
      // that is positively wrong is treated as a mismatch.
      const next = body.nextChunk;
      if (next !== undefined && next !== null && next !== expected.chunkIndex + 1) {
        problems.push(`nextChunk was ${JSON.stringify(next)} on the final chunk`);
      }
    } else {
      if (body.committed === true) problems.push("committed arrived before the final chunk");
      if (body.nextChunk !== expected.chunkIndex + 1) {
        problems.push(`nextChunk was ${JSON.stringify(body.nextChunk)}, expected ${expected.chunkIndex + 1}`);
      }
    }
  }
  if (problems.length === 0) return;
  throw protocolError(
    `The daemon's receipt for chunk ${expected.chunkIndex} did not match what was sent ` +
      `(${problems.join("; ")}). The submission was abandoned — nothing has been applied.`,
  );
}

/**
 * `POST /choice/selection` — the staged ids, chunked, every receipt verified.
 *
 * Uploads only. The decision that acts on the committed selection is a separate
 * `POST /choice {choice:"disk"}`, per the contract, so the caller can report the
 * two phases separately.
 *
 * @param {string} projectId
 * @param {{choiceId: string, ids: number[],
 *   onProgress?: (progress: {sent: number, total: number, chunkIndex: number,
 *   chunkCount: number}) => void}} options
 */
async function submitChoiceSelection(projectId, options = {}) {
  const choiceId = requireChoiceId(options.choiceId);
  const ids = Array.isArray(options.ids) ? options.ids : null;
  if (!ids || ids.length === 0) {
    throw new HostError("invalid_argument", "a selection needs at least one id");
  }
  if (!ids.every((id) => Number.isInteger(id) && id >= 0)) {
    throw new HostError("invalid_argument", "selection ids must be non-negative integers");
  }
  const onProgress = typeof options.onProgress === "function" ? options.onProgress : () => {};

  const submissionId = newSubmissionId();
  const chunks = chunkSelection(ids);
  let sent = 0;

  for (let chunkIndex = 0; chunkIndex < chunks.length; chunkIndex += 1) {
    const chunk = chunks[chunkIndex];
    const finalChunk = chunkIndex === chunks.length - 1;
    const payload = { choiceId, submissionId, chunkIndex, finalChunk, ids: chunk };
    // Chunk 0 always restarts: a previous abandoned submission must not be
    // able to leave ids behind that this one would silently inherit.
    if (chunkIndex === 0) payload.restart = true;

    const response = await daemonPost(projectId, "/choice/selection", payload);
    if (response.status === 409 && response.body?.error === "resolved") {
      return { status: "resolved-elsewhere", choiceId, submissionId };
    }
    if (!response.ok) throw daemonRefusal("POST /choice/selection", response);

    sent += chunk.length;
    verifyReceipt(response.body, { chunkIndex, finalChunk, selectedCount: sent });
    onProgress({ sent, total: ids.length, chunkIndex, chunkCount: chunks.length });
  }

  return { status: "committed", choiceId, submissionId, selectedCount: sent };
}

// -------------------------------------------------------- disk review (7.0) --
//
// The client half of Design 7.0's passive review. The engine has *already*
// applied Studio → disk by the time any of this runs, so nothing here is a
// decision about the sync — it is a list of disk items the apply left over, and
// an optional push of some of them back into Studio.
//
// Everything is checked the same way the divergence set is, and for the same
// reason: the ids in a page are what a push is submitted as. The one rule that
// differs is density. A divergence set is frozen, so its ids run `cursor …
// cursor + n - 1`; a review **shrinks** as items are pushed and keeps the ids
// of what is left, so the check is "strictly increasing across the whole set"
// rather than "dense from the cursor". Both rule out gaps, repeats and
// reordering; only the second survives a partial push.

/** Contract: ≤2048 ids per push call. */
const PUSH_CHUNK_IDS = 2048;
/** The same 64 KiB envelope the selection chunks use. */
const PUSH_CHUNK_BYTES = 64 * 1024;

const REVIEW_STATES = new Set(["disk-only", "differs"]);

function emptyReviewStats() {
  return { total: 0, diskOnly: 0, differs: 0 };
}

function normalizeReviewStats(stats) {
  const source = stats && typeof stats === "object" ? stats : {};
  return {
    total: countOf(source.total),
    diskOnly: countOf(source.diskOnly),
    differs: countOf(source.differs),
  };
}

function requireReviewId(reviewId) {
  if (typeof reviewId !== "string" || reviewId.trim() === "") {
    throw new HostError("invalid_argument", "no reviewId was supplied");
  }
  return reviewId;
}

/** `GET /review` — is a disk review pending, and how big is it? */
async function fetchPendingReview(projectId) {
  const response = await daemonFetch(projectId, "/review");
  if (response.status === 404) {
    throw new HostError("unavailable", "This engine has no /review route.");
  }
  if (!response.ok) throw daemonRefusal("GET /review", response);

  const body = response.body ?? {};
  if (body.pending !== true) {
    return { projectId, pending: false, reviewId: null, stats: emptyReviewStats() };
  }
  const reviewId = typeof body.reviewId === "string" ? body.reviewId.trim() : "";
  if (!reviewId) {
    throw protocolError("GET /review reported a pending review with no reviewId.");
  }
  return { projectId, pending: true, reviewId, stats: normalizeReviewStats(body.stats) };
}

/**
 * `GET /review/details` — one verified page of the pending review.
 *
 * @param {string} projectId
 * @param {{reviewId: string, cursor?: number, limit?: number,
 *   expectedTotal?: number|null, after?: number|null}} options
 */
async function fetchReviewDetails(projectId, options = {}) {
  const reviewId = requireReviewId(options.reviewId);
  const cursor = Number.isInteger(options.cursor) ? options.cursor : 0;
  const limit = Math.min(Math.max(1, options.limit ?? CHOICE_PAGE_LIMIT), 1024);
  if (cursor < 0) throw new HostError("invalid_argument", "cursor cannot be negative");

  const route =
    `/review/details?reviewId=${encodeURIComponent(reviewId)}` + `&cursor=${cursor}&limit=${limit}`;
  const response = await daemonFetch(projectId, route);

  if (response.status === 404 || response.status === 409) {
    throw new HostError(
      "daemon",
      "That disk review is no longer available — it was pushed, dismissed, or replaced by a new connect.",
    );
  }
  if (!response.ok) throw daemonRefusal("GET /review/details", response);

  return verifyReviewPage(response.body ?? {}, {
    reviewId,
    cursor,
    limit,
    expectedTotal: Number.isInteger(options.expectedTotal) ? options.expectedTotal : null,
  });
}

/**
 * The review page contract, checked before a row is drawn from it.
 *
 * Refused, each with its own reason:
 * - a different `reviewId` (a page about another set entirely)
 * - a non-integer, negative, or out-of-order id — the ids are what a push
 *   names, and a repeat would push one item twice while missing another
 * - a `totalCount` that moved between pages, or is not a count
 * - more items than the requested limit, or than the set claims to hold
 * - a `nextCursor` that does not advance by exactly this page's length
 * - a last page that stops short of `totalCount`
 */
function verifyReviewPage(body, expected) {
  const fail = (why) => {
    throw protocolError(
      `GET /review/details returned a page WSync cannot trust (${why}). ` +
        "Nothing was picked; the list was abandoned rather than acted on.",
    );
  };

  if (!body || typeof body !== "object") fail("the response was not a JSON object");
  if (typeof body.reviewId !== "string" || body.reviewId !== expected.reviewId) {
    fail(`it is about reviewId ${JSON.stringify(body.reviewId)}, not ${expected.reviewId}`);
  }
  if (!Array.isArray(body.items)) fail("items was not an array");
  if (body.items.length > expected.limit) {
    fail(`it holds ${body.items.length} items for a limit of ${expected.limit}`);
  }
  if (!Number.isInteger(body.totalCount) || body.totalCount < 0) {
    fail(`totalCount was ${JSON.stringify(body.totalCount)}`);
  }
  if (expected.expectedTotal !== null && body.totalCount !== expected.expectedTotal) {
    fail(`totalCount moved from ${expected.expectedTotal} to ${body.totalCount} mid-page`);
  }

  let previousId = -1;
  const items = body.items.map((item, index) => {
    const at = expected.cursor + index;
    if (!item || typeof item !== "object") fail(`entry ${at} was not an object`);
    if (!Number.isInteger(item.id) || item.id < 0) {
      fail(`entry ${at} carries id ${JSON.stringify(item.id)}`);
    }
    if (item.id <= previousId) {
      fail(`entry ${at} carries id ${item.id} after ${previousId}; ids must increase`);
    }
    previousId = item.id;
    if (!REVIEW_STATES.has(item.state)) fail(`entry ${at} has state ${JSON.stringify(item.state)}`);
    // A review entry is a *disk* item, so it always has a file path; the
    // instance path is what may be missing, for a file Studio has never seen.
    if (typeof item.path !== "string" || item.path === "") fail(`entry ${at} has no path`);
    if (item.instancePath !== null && typeof item.instancePath !== "string") {
      fail(`entry ${at} has an instancePath that is neither a string nor null`);
    }
    return {
      id: item.id,
      path: item.path,
      instancePath: item.instancePath ?? null,
      state: item.state,
      class: typeof item.class === "string" ? item.class : null,
    };
  });

  if (expected.cursor + items.length > body.totalCount) {
    fail(`it runs past totalCount ${body.totalCount}`);
  }

  const hasNext = body.nextCursor !== undefined && body.nextCursor !== null;
  if (hasNext) {
    if (!Number.isInteger(body.nextCursor)) fail(`nextCursor was ${JSON.stringify(body.nextCursor)}`);
    if (body.nextCursor !== expected.cursor + items.length) {
      fail(`nextCursor ${body.nextCursor} does not follow ${items.length} items from ${expected.cursor}`);
    }
    if (body.nextCursor >= body.totalCount) {
      fail(`nextCursor ${body.nextCursor} is past the end of a ${body.totalCount}-entry review`);
    }
    if (items.length === 0) fail("it is empty but claims there is more");
  } else if (expected.cursor + items.length !== body.totalCount) {
    fail(`it ends at ${expected.cursor + items.length} of ${body.totalCount} without a nextCursor`);
  }

  return {
    reviewId: body.reviewId,
    cursor: expected.cursor,
    items,
    nextCursor: hasNext ? body.nextCursor : null,
    totalCount: body.totalCount,
  };
}

/** Split ids into chunks inside both of the push contract's bounds at once. */
function chunkPush(ids) {
  const chunks = [];
  let current = [];
  let bytes = SELECTION_ENVELOPE_BYTES;
  for (const id of ids) {
    const cost = String(id).length + 1;
    if (current.length >= PUSH_CHUNK_IDS || bytes + cost > PUSH_CHUNK_BYTES) {
      chunks.push(current);
      current = [];
      bytes = SELECTION_ENVELOPE_BYTES;
    }
    current.push(id);
    bytes += cost;
  }
  if (current.length > 0 || chunks.length === 0) chunks.push(current);
  return chunks;
}

/**
 * `POST /review/push` — push chosen disk items into Studio.
 *
 * Two shapes, both repeatable: `{mode:"all"}` for everything still pending, or
 * an id list that is chunked at 2048. Each answer is `{ok, pushed, remaining}`
 * and each is checked before the next chunk goes out:
 *
 * - `pushed` never exceeds what the chunk asked for;
 * - `remaining` never goes *up*, and never survives a push that claims to have
 *   moved more than was left.
 *
 * An unchecked `remaining` is the field that matters: it is what tells the
 * modal whether the review is finished, and a daemon that under-reports it
 * would leave items stranded with the UI claiming success.
 *
 * `knownRemaining` is the caller's own count of what was pending — the size of
 * the list it is looking at. Passing it is what makes the *first* answer
 * checkable rather than taken on faith: without it there is nothing to compare
 * the first `remaining` against. A review only ever shrinks under a live
 * `reviewId` (a bigger one arrives as a new id), so "remaining is at most what
 * I knew, minus what this call pushed" holds even if another client is pushing
 * at the same time.
 *
 * @param {string} projectId
 * @param {{reviewId: string, mode?: "all", ids?: number[],
 *   knownRemaining?: number|null,
 *   onProgress?: (progress: {pushed: number, total: number, chunkIndex: number,
 *   chunkCount: number, remaining: number}) => void}} options
 */
async function pushReview(projectId, options = {}) {
  const reviewId = requireReviewId(options.reviewId);
  const onProgress = typeof options.onProgress === "function" ? options.onProgress : () => {};
  const known = Number.isInteger(options.knownRemaining) ? options.knownRemaining : null;

  if (options.mode === "all") {
    const answer = await postReview(projectId, "/review/push", { reviewId, mode: "all" });
    if (answer.status === "gone") return answer;
    const receipt = verifyPushReceipt(answer.body, { requested: null, remainingBefore: known });
    return { status: "pushed", reviewId, ...receipt };
  }

  const ids = Array.isArray(options.ids) ? options.ids : null;
  if (!ids || ids.length === 0) {
    throw new HostError("invalid_argument", "a push needs at least one id");
  }
  if (!ids.every((id) => Number.isInteger(id) && id >= 0)) {
    throw new HostError("invalid_argument", "push ids must be non-negative integers");
  }

  const chunks = chunkPush(ids);
  let pushed = 0;
  let remaining = known;

  for (let chunkIndex = 0; chunkIndex < chunks.length; chunkIndex += 1) {
    const chunk = chunks[chunkIndex];
    const answer = await postReview(projectId, "/review/push", { reviewId, ids: chunk });
    if (answer.status === "gone") return answer;

    const receipt = verifyPushReceipt(answer.body, {
      requested: chunk.length,
      remainingBefore: remaining,
    });
    pushed += receipt.pushed;
    remaining = receipt.remaining;
    onProgress({
      pushed,
      total: ids.length,
      chunkIndex,
      chunkCount: chunks.length,
      remaining,
    });
  }

  return { status: "pushed", reviewId, pushed, remaining };
}

/** `POST /review/dismiss` — drop the review without pushing anything. */
async function dismissReview(projectId, options = {}) {
  const reviewId = requireReviewId(options.reviewId);
  const answer = await postReview(projectId, "/review/dismiss", { reviewId });
  if (answer.status === "gone") return answer;
  if (answer.body?.ok !== true) {
    throw protocolError("POST /review/dismiss answered without ok:true.");
  }
  return { status: "dismissed", reviewId };
}

/**
 * The shared half of both writes: a 404 is "that review is gone" — pushed out
 * by someone else, dismissed, or replaced by a new connect — and never a
 * failure to report as one.
 */
async function postReview(projectId, route, payload) {
  const response = await daemonPost(projectId, route, payload);
  if (response.status === 404 || response.status === 409) {
    return { status: "gone", body: null };
  }
  if (!response.ok) throw daemonRefusal(`POST ${route}`, response);
  return { status: "ok", body: response.body ?? {} };
}

function verifyPushReceipt(body, expected) {
  const problems = [];
  if (!body || typeof body !== "object") {
    problems.push("the receipt was not a JSON object");
  } else {
    if (body.ok !== true) problems.push(`ok was ${JSON.stringify(body.ok)}`);
    if (!Number.isInteger(body.pushed) || body.pushed < 0) {
      problems.push(`pushed was ${JSON.stringify(body.pushed)}`);
    } else if (expected.requested !== null && body.pushed > expected.requested) {
      problems.push(`pushed ${body.pushed} of ${expected.requested} requested ids`);
    }
    if (!Number.isInteger(body.remaining) || body.remaining < 0) {
      problems.push(`remaining was ${JSON.stringify(body.remaining)}`);
    } else if (expected.remainingBefore !== null) {
      if (body.remaining > expected.remainingBefore) {
        problems.push(`remaining rose from ${expected.remainingBefore} to ${body.remaining}`);
      } else if (
        Number.isInteger(body.pushed) &&
        expected.remainingBefore - body.remaining < body.pushed
      ) {
        problems.push(
          `remaining fell from ${expected.remainingBefore} to ${body.remaining} for ${body.pushed} pushed`,
        );
      }
    }
  }
  if (problems.length > 0) {
    throw protocolError(
      `The daemon's answer to a review push did not add up (${problems.join("; ")}). ` +
        "The push was abandoned; whatever it already applied is in Studio and the rest is untouched.",
    );
  }
  return { pushed: body.pushed, remaining: body.remaining };
}

// ------------------------------------------------------- conflicts (6.3) ----

/**
 * `POST /resolve` — answer one parked conflict.
 *
 * `keep` and `choice` carry the same value in the contract; both are sent
 * rather than one guessed at. The conflict id is passed through exactly as the
 * daemon reported it — it is the daemon's handle, not something to reformat.
 *
 * A 404 is reported as `already-resolved`, not as a failure: the id is only
 * unknown because someone (the CLI, another window, the plugin) answered it
 * first, and the card should disappear rather than turn red.
 */
async function resolveConflict(projectId, conflict, keep) {
  if (keep !== "local" && keep !== "studio") {
    throw new HostError("invalid_argument", `${JSON.stringify(keep)} is not a conflict answer`);
  }
  const id = conflict?.id;
  if (typeof id !== "string" && !Number.isInteger(id)) {
    throw new HostError("invalid_argument", "the conflict carried no usable id");
  }

  const payload = { id, keep, choice: keep };
  if (typeof conflict?.path === "string" && conflict.path !== "") payload.path = conflict.path;

  const response = await daemonPost(projectId, "/resolve", payload);
  if (response.status === 404) return { status: "already-resolved", id };
  if (!response.ok) throw daemonRefusal("POST /resolve", response);

  const body = response.body ?? {};
  if (body.ok !== true) throw daemonRefusal("POST /resolve", response);
  if (body.resolved !== undefined && body.resolved !== id) {
    throw protocolError(
      `POST /resolve acknowledged ${JSON.stringify(body.resolved)} for a request about ${JSON.stringify(id)}.`,
    );
  }
  return { status: "resolved", id };
}

// --------------------------------------------------------- conflict polling --

/** Design 8.2: 20 s poll, plus invalidation whenever a `conflict` event lands. */
const CONFLICT_POLL_MS = 20_000;
let conflictTimer = null;
let conflictPollInFlight = false;

async function pollConflicts() {
  const served = servedProjectIds();
  if (!IS_TAURI || served.length === 0) {
    app.conflictsUnsupported.clear();
    conflictBadge.report(0);
    emit("conflicts", { items: [], total: 0, partial: false, unsupported: 0, unreachable: 0, served: 0 });
    await refreshPendingChoice();
    await refreshPendingReview();
    return;
  }
  if (conflictPollInFlight) return;
  conflictPollInFlight = true;

  const items = [];
  let total = 0;
  let partial = false;
  let unsupported = 0;
  let unreachable = 0;

  try {
    for (const projectId of served) {
      let response;
      try {
        response = await daemonFetch(projectId, "/resolve");
      } catch {
        unreachable += 1;
        continue;
      }

      if (response.status === 404) {
        // An engine that predates the conflict engine. Report "unknown", never
        // "zero" — a wrong zero would read as "nothing to resolve".
        app.conflictsUnsupported.add(projectId);
        unsupported += 1;
        continue;
      }
      app.conflictsUnsupported.delete(projectId);
      if (!response.ok) {
        unreachable += 1;
        continue;
      }

      const body = response.body ?? {};
      const list = Array.isArray(body.conflicts) ? body.conflicts : Array.isArray(body) ? body : [];
      for (const conflict of list) items.push({ ...conflict, projectId });
      total += Number.isFinite(body.total) ? Number(body.total) : list.length;
      partial ||= body.partial === true || list.length < total;
    }
  } finally {
    conflictPollInFlight = false;
  }

  const allUnsupported = unsupported === served.length;
  const nothingAnswered = unsupported + unreachable === served.length;
  if (allUnsupported) {
    conflictBadge.report(null, { note: "This engine predates the conflict engine." });
  } else if (nothingAnswered) {
    conflictBadge.report(null, { note: "No daemon answered /resolve." });
  } else {
    conflictBadge.report(total, { partial });
  }

  emit("conflicts", {
    items,
    total,
    partial,
    unsupported,
    unreachable,
    served: served.length,
  });

  // Same cadence, same triggers: a pending divergence is the set-level half of
  // the same question the conflict list answers item by item (Design 7.5).
  await refreshPendingChoice();
  await refreshPendingReview();
}

function startConflictPolling() {
  if (conflictTimer !== null) return;
  conflictTimer = setInterval(() => void pollConflicts(), CONFLICT_POLL_MS);
  void pollConflicts();
}

/**
 * Ask the linked daemon whether a divergence decision is waiting.
 *
 * The `choice-needed` event is the fast path, but it is not the only one: a
 * choice raised before this app connected — or while it was reconnecting —
 * would otherwise never produce a banner. `GET /choice` is the durable truth,
 * so it rides the same 20 s cadence as the conflict poll.
 *
 * One project at a time, deliberately: the socket follows one served project
 * and the modal can only answer one choice, so a second project's banner would
 * be an offer the app cannot honour.
 */
async function refreshPendingChoice() {
  // The dev fixture is not a daemon's answer; never let a poll clear it.
  if (app.pendingDivergence?.fixture === true) return;

  const projectId = linkProjectId();
  if (!IS_TAURI || !projectId) {
    if (app.pendingDivergence) emit("divergence", null);
    return;
  }

  let choice;
  try {
    choice = await fetchPendingChoice(projectId);
  } catch {
    // An engine without `/choice`, or one that is not answering. Leave whatever
    // the banner already says alone rather than claiming the choice is gone.
    return;
  }

  if (!choice.pending) {
    if (app.pendingDivergence) emit("divergence", null);
    return;
  }
  if (app.pendingDivergence?.choiceId === choice.choiceId) return;
  emit("divergence", { projectId, choiceId: choice.choiceId, ...choice.stats, fixture: false });
}

/**
 * Ask the linked daemon whether a disk review is waiting (Design 7.0).
 *
 * The `disk-review` event is the fast path; this is the durable one, for a
 * review raised before the app connected — which is the normal case, since the
 * daemon raises it the moment Studio connects and the app may well have been
 * started afterwards.
 *
 * An engine without `/review` (or one that is not answering) leaves whatever
 * the banner already says alone: claiming "no review" on a failed read would
 * hide a real one.
 */
async function refreshPendingReview() {
  const projectId = linkProjectId();
  if (!IS_TAURI || !projectId) {
    if (app.pendingReview) emit("review", null);
    return;
  }

  let review;
  try {
    review = await fetchPendingReview(projectId);
  } catch {
    return;
  }

  if (!review.pending) {
    if (app.pendingReview) emit("review", null);
    // There is no "someone else answered it" event in the contract — a review
    // is answered by a push or a dismiss, and the poll is how this window finds
    // out about another client's. An open modal is then showing a list of ids
    // the daemon has dropped, so it is told rather than left there.
    app.review?.resolvedElsewhere();
    return;
  }
  const summary = { projectId, reviewId: review.reviewId, ...review.stats };
  // The same review with a smaller count is still news: another client (or this
  // one's modal) pushed part of it, and the banner is a number.
  const known = app.pendingReview;
  if (!(known?.reviewId === review.reviewId && known.total === review.stats.total)) {
    emit("review", summary);
  }
  // Checked even when nothing changed: an auto-open blocked earlier (another
  // modal was up) gets its retry on this cadence.
  maybeAutoOpenReview(summary);
}

/**
 * The initial-sync popup (Design 7.0 revised): when a review with anything in
 * it is reported for the served project — by the `disk-review` event or the
 * `GET /review` poll — the picker opens itself as a dismissible overlay.
 *
 * Once per reviewId, ever: Escape backgrounds it to the banner, and a
 * reconnect re-broadcasting the same set must not shove it back at a user who
 * closed it. A *new* reviewId is a new set and opens fresh. Marked as shown
 * only when the modal really opened — if another overlay held the modal root,
 * the next event or poll tries again.
 *
 * @returns {boolean} true when this call opened the modal.
 */
function maybeAutoOpenReview(summary) {
  if (!summary?.reviewId || !(summary.total > 0)) return false;
  if (summary.projectId !== linkProjectId()) return false;
  if (app.reviewAutoShown.has(summary.reviewId)) return false;
  if (app.review) return false;
  const handle = openDiskReviewModal({ review: summary });
  if (!handle) return false;
  app.reviewAutoShown.add(summary.reviewId);
  return true;
}

/**
 * Open the divergence modal, holding on to its handle so daemon events can
 * reach it. One at a time: a second call while one is open returns the open one.
 */
function openDivergenceModal(options = {}) {
  if (app.overwrite) return app.overwrite;
  const handle = openOverwriteModal(buildApi(), {
    ...options,
    onClosed: () => {
      app.overwrite = null;
    },
  });
  // A modal that refused to open (no divergence to show) never registers.
  app.overwrite = handle?.close ? handle : null;
  return handle;
}

/**
 * Open the disk-review modal. Same one-at-a-time rule, and the same reason for
 * holding the handle: a `disk-review` for a *new* set has to be able to close
 * a window holding ids the daemon has dropped.
 *
 * Two callers: the banner's Review button, and `maybeAutoOpenReview` — the
 * initial-sync popup that opens once per reviewId when the review arrives.
 * Either way it is a dismissible overlay over a sync that already happened;
 * nothing waits on it.
 */
function openDiskReviewModal(options = {}) {
  if (app.review) return app.review;
  const seed = options.review ?? app.pendingReview;
  if (!seed) return null;
  const handle = openReviewModal(buildApi(), {
    ...options,
    review: seed,
    onClosed: () => {
      app.review = null;
    },
  });
  app.review = handle?.close ? handle : null;
  return handle;
}

/**
 * The banner's Skip: tell the daemon the review can go, and drop the banner.
 * The same decision as the modal's Skip — keep Studio's versions everywhere.
 *
 * A 404 is success — someone else already answered it — because the user's
 * intent was "stop showing me this", and it has been met either way.
 */
async function dismissPendingReview() {
  const pending = app.pendingReview;
  if (!pending?.projectId || !pending.reviewId) {
    emit("review", null);
    return true;
  }
  try {
    await dismissReview(pending.projectId, { reviewId: pending.reviewId });
    emit("review", null);
    setStatus("Skipped — Studio's versions are kept everywhere.", "");
    return true;
  } catch (error) {
    setStatus("The review could not be skipped.", "err");
    toast("Could not skip the review", {
      kind: "err",
      body: error instanceof HostError ? error.message : String(error),
    });
    return false;
  }
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
      $conflictsBadge.title = note || "Conflict count has not been checked";
      return;
    }
    if (count === 0) {
      $conflictsBadge.hidden = true;
      return;
    }
    $conflictsBadge.hidden = false;
    $conflictsBadge.dataset.state = "some";
    $conflictsBadge.textContent = partial ? `${count}+` : String(count);
    $conflictsBadge.title = `${count}${partial ? " or more" : ""} parked conflicts`;
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
    refreshConflicts: () => pollConflicts(),
    resolveConflict: (projectId, conflict, keep) => resolveConflict(projectId, conflict, keep),
    fetchPendingChoice,
    fetchDivergenceDetails,
    fetchChoiceSource,
    submitChoice,
    submitChoiceSelection,

    // the disk review (Design 7.0)
    fetchPendingReview,
    fetchReviewDetails,
    pushReview,
    dismissReview,

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
    getPendingDivergence: () => app.pendingDivergence,
    openOverwriteModal: (options) => openDivergenceModal(options),
    getPendingReview: () => app.pendingReview,
    openReviewModal: (options) => openDiskReviewModal(options),
    dismissPendingReview: () => dismissPendingReview(),

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
  startConflictPolling();
  // Seed the broker mirror: `broker:up` may already have fired during boot, and
  // a folder that is authorized but cannot bind never fires one at all.
  void refreshBrokerStatus();

  // Dev affordance: `index.html?dev=overwrite` renders the divergence modal
  // from its fixture so the layout can be reviewed without a daemon.
  if (new URLSearchParams(location.search).get("dev") === "overwrite") {
    openDivergenceModal({ fixture: true });
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
    void pollConflicts();
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
