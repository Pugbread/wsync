// views/last-edited.js — the per-project "when did this path last change" store
// (Design 7.3: "sort … Recently edited · A→Z · Change type; last-edited store
// fed by sanitized activity events").
//
// The divergence set arrives from the daemon in the daemon's own order and
// carries no timestamps: §7.2 freezes *what* differs, never *when* it was
// touched. "Recently edited" is therefore not something the daemon can answer —
// it is something the app has to have been watching for. That is what this is:
// a small, bounded ledger fed by the `sync-activity` event stream the app is
// already subscribed to, so by the time a divergence appears there is a local
// history to sort it by.
//
// Three properties matter, and each one is a rule below:
//
//   Sanitized   the only thing taken from a frame is `names` — path strings,
//               allowlisted the same way the Activity feed allowlists a card
//               (§8.2). No object from the wire is retained, ever.
//   Bounded     ≤400 paths per project, ≤8 projects, LRU on both axes. A
//               daemon that emits a million distinct paths grows this by zero
//               bytes past the cap, which matters because it is *persisted*.
//   Cheap       events arrive at sync speed; `state.json` must not. Ingest is
//               in-memory and immediate (a read right after an event sees it);
//               the write is debounced and coalesced through the app store.
//
// Persisted under its own state key (`lastEdited`), so it is neither mixed into
// the project registry nor lost on restart.

/** Design 7.3's bound, chosen so the whole ledger stays a few tens of KiB. */
export const MAX_PATHS_PER_PROJECT = 400;

/** How many projects keep a ledger at all. LRU by last ingest. */
export const MAX_PROJECTS = 8;

/** The pinned contract: `sync-activity` carries ≤10 path names per event. */
export const MAX_NAMES_PER_EVENT = 10;

/** Longer than any real project-relative path; anything past it is not one. */
const MAX_PATH_CHARS = 300;

/** Writes are coalesced into at most one `setState` per this window. */
const COMMIT_DEBOUNCE_MS = 1000;

/** Nothing before this is a plausible edit time; treated as "unknown, use now". */
const EPOCH_FLOOR = Date.UTC(2020, 0, 1);

/**
 * A path name, or null.
 *
 * Deliberately strict rather than clever: a string, trimmed, non-empty, within
 * a sane length, and free of control characters (a `\n` in a "path" is either a
 * broken engine or an attempt to forge a second entry). Nothing is rewritten —
 * a path that needs rewriting to be acceptable is not one this store keeps.
 */
export function sanitizePath(value) {
  if (typeof value !== "string") return null;
  const trimmed = value.trim();
  if (trimmed === "" || trimmed.length > MAX_PATH_CHARS) return null;
  // A control character in a "path" is either a broken engine or an attempt
  // to forge a second entry, so it disqualifies the whole name.
  if (/[\u0000-\u001f\u007f]/u.test(trimmed)) return null;
  return trimmed;
}

/**
 * The persisted document, read defensively.
 *
 * Anything that is not the shape this module writes is dropped rather than
 * repaired: the ledger is a cache of observations, so a corrupt entry costs a
 * sort order, never data.
 */
function readDocument(raw) {
  const projects = new Map();
  if (!raw || typeof raw !== "object" || Array.isArray(raw)) return projects;

  for (const [projectId, record] of Object.entries(raw)) {
    if (typeof projectId !== "string" || projectId === "") continue;
    if (!record || typeof record !== "object" || Array.isArray(record)) continue;
    const paths = new Map();
    const source = record.paths;
    if (source && typeof source === "object" && !Array.isArray(source)) {
      for (const [path, at] of Object.entries(source)) {
        const name = sanitizePath(path);
        if (name === null || !Number.isFinite(at) || at < EPOCH_FLOOR) continue;
        paths.set(name, Number(at));
      }
    }
    if (paths.size === 0) continue;
    const seenAt = Number.isFinite(record.seenAt)
      ? Number(record.seenAt)
      : Math.max(...paths.values());
    projects.set(projectId, { seenAt, paths: prunePaths(paths) });
  }
  return pruneProjects(projects);
}

/** Newest-first, capped. The dropped ones are the oldest observations. */
function prunePaths(paths) {
  if (paths.size <= MAX_PATHS_PER_PROJECT) return paths;
  const kept = [...paths.entries()]
    .sort((left, right) => right[1] - left[1])
    .slice(0, MAX_PATHS_PER_PROJECT);
  return new Map(kept);
}

/** LRU across projects: the least recently *fed* ledger is the one that goes. */
function pruneProjects(projects) {
  if (projects.size <= MAX_PROJECTS) return projects;
  const kept = [...projects.entries()]
    .sort((left, right) => right[1].seenAt - left[1].seenAt)
    .slice(0, MAX_PROJECTS);
  return new Map(kept);
}

function toDocument(projects) {
  const out = {};
  for (const [projectId, record] of projects) {
    const paths = {};
    for (const [path, at] of record.paths) paths[path] = at;
    out[projectId] = { seenAt: record.seenAt, paths };
  }
  return out;
}

/**
 * @param {object} options
 * @param {() => object} options.getState the app store's reader.
 * @param {(patch: object) => void} options.setState the app store's writer.
 * @param {() => number} [options.now] injectable clock, for tests.
 * @param {(run: () => void, ms: number) => any} [options.schedule]
 * @param {(handle: any) => void} [options.cancel]
 */
export function createLastEditedStore({
  getState,
  setState,
  now = () => Date.now(),
  schedule = (run, ms) => setTimeout(run, ms),
  cancel = (handle) => clearTimeout(handle),
} = {}) {
  /** The truth, in memory. The persisted copy trails it by ≤1 s. */
  let projects = readDocument(getState()?.lastEdited);
  let commitTimer = null;
  let dirty = false;

  function commit() {
    if (commitTimer !== null) {
      cancel(commitTimer);
      commitTimer = null;
    }
    if (!dirty) return;
    dirty = false;
    setState({ lastEdited: toDocument(projects) });
  }

  function scheduleCommit() {
    dirty = true;
    if (commitTimer !== null) return;
    commitTimer = schedule(() => {
      commitTimer = null;
      commit();
    }, COMMIT_DEBOUNCE_MS);
  }

  /**
   * Ingest one `sync-activity` frame's `names` for a project.
   *
   * Returns how many stamps were actually written, so a caller (or a test) can
   * tell "the event carried nothing usable" from "the event was ignored".
   *
   * `at` is the frame's own timestamp when it is plausible: an engine that
   * clock-skews into the future would otherwise pin its paths to the top of
   * "Recently edited" forever, so anything past now is clamped to now.
   */
  function record(projectId, names, at) {
    if (typeof projectId !== "string" || projectId === "") return 0;
    if (!Array.isArray(names) || names.length === 0) return 0;

    const clock = now();
    const stamp =
      Number.isFinite(at) && at >= EPOCH_FLOOR && at <= clock ? Number(at) : clock;

    let ledger = projects.get(projectId);
    if (!ledger) {
      ledger = { seenAt: clock, paths: new Map() };
      projects.set(projectId, ledger);
    }

    let written = 0;
    for (const raw of names.slice(0, MAX_NAMES_PER_EVENT)) {
      const path = sanitizePath(raw);
      if (path === null) continue;
      const previous = ledger.paths.get(path);
      // Re-inserting moves the key to the end of the Map's order, which is what
      // keeps the newest observation newest even when the stamp is unchanged.
      ledger.paths.delete(path);
      ledger.paths.set(path, previous !== undefined && previous > stamp ? previous : stamp);
      written += 1;
    }
    if (written === 0) {
      // Nothing usable: do not let an empty event count as project activity,
      // or a chatty broken engine would evict a good ledger through the LRU.
      if (ledger.paths.size === 0) projects.delete(projectId);
      return 0;
    }

    ledger.seenAt = clock;
    ledger.paths = prunePaths(ledger.paths);
    projects = pruneProjects(projects);
    scheduleCommit();
    return written;
  }

  /**
   * The project's stamps as a `Map` of path → epoch ms.
   *
   * A copy: callers sort and read it during a render, and handing out the live
   * map would let a view mutate the ledger by accident.
   */
  function stampsFor(projectId) {
    const ledger = projects.get(projectId);
    return ledger ? new Map(ledger.paths) : new Map();
  }

  return {
    record,
    stampsFor,
    /** Whether "Recently edited" has anything to sort by for this project. */
    has: (projectId) => (projects.get(projectId)?.paths.size ?? 0) > 0,
    editedAt: (projectId, path) => projects.get(projectId)?.paths.get(path) ?? null,
    /** Write the pending patch now — used on `pagehide`, and by tests. */
    flush: commit,

    /**
     * Re-read the persisted ledger.
     *
     * The store is constructed at module load, before boot has asked the host
     * for `state.json`, so without this the first session after a restart would
     * sort by an empty history and quietly overwrite the saved one.
     */
    rehydrate() {
      if (dirty) commit();
      projects = readDocument(getState()?.lastEdited);
    },
    /** Forget a project's ledger; called when the project is removed. */
    forget(projectId) {
      if (!projects.delete(projectId)) return;
      scheduleCommit();
    },
    /** Diagnostics only. */
    size: () => projects.size,
  };
}
