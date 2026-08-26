// views/backlog.js — the disk content that lost to Studio.
//
// Sync never asks a question. A connect applies Studio over the project and a
// mid-session clash resolves the same way, because being interrupted by a
// decision you did not plan to make is worse than the loss it guards against.
// This window is what makes that safe: every time disk content would be
// overwritten or dropped, its bytes are kept, and here they are.
//
// So nothing in this window is a decision that has to be made. It opens when
// you ask for it, never over your work, and closing it changes nothing — the
// entries sit where they are until you want one or until they expire.
//
// The interaction is Design 7.3's staging pane, reused wholesale: the left
// pane is what lost, the right is what you want back. Only the vocabulary
// differs, because putting an entry back is not "keeping" a version — it is
// writing the file again and letting it sync, which is exactly what would have
// happened had you saved it a moment later.
//
// Wire contract (`api.*`, implemented in app.js):
//   GET  /backlog          → {total, ttlSeconds, entries:[…]}
//   POST /backlog/restore  → {id}
//   POST /backlog/drop     → {id} | {all: true}
//
// A restore **needs Studio**: with no plugin connected the daemon answers 503,
// which is a wait rather than a dead button — the window parks and replays
// itself when the plugin connects. That is not pedantry: a file restored while
// Studio is away would be sent straight back here by the next connect, which
// is Studio-first like every other one.

import { el, icon, plural } from "./dom.js";
import {
  filterEntries,
  MAX_LIVE_ROWS,
  moreRowsNote,
  searchBox,
  sortEntries,
  sortOptionsFor,
  sortSelect as makeSortSelect,
  stagingPane,
  syncSortSelect,
} from "./staging.js";

/**
 * Why a row is here. `initial-sync` lost to the connect-time apply;
 * `conflict` lost to a clash while sync was running.
 */
const KINDS = {
  "initial-sync": {
    mark: "+",
    markClass: "mark-add",
    label: "Replaced on connect",
    verb: "Put back",
  },
  conflict: {
    mark: "~",
    markClass: "mark-differs",
    label: "Lost a live clash",
    verb: "Put back",
  },
};

const TITLE = "Backlog — disk content Studio replaced";
const SUBTITLE =
  "Studio always wins, so these never reached the place. Move anything you want back — " +
  "it returns to its file and syncs up. Entries are deleted a day after they land here.";

/** Turns `secondsRemaining` into the one number worth reading. */
function expiryLabel(seconds) {
  if (!Number.isFinite(seconds) || seconds <= 0) return "expiring";
  if (seconds >= 3600) return `${Math.floor(seconds / 3600)}h left`;
  if (seconds >= 60) return `${Math.floor(seconds / 60)}m left`;
  return `${Math.floor(seconds)}s left`;
}

function normalizeEntry(entry) {
  const path = typeof entry.path === "string" && entry.path !== "" ? entry.path : String(entry.id);
  const kind = KINDS[entry.reason] ? entry.reason : "initial-sync";
  const expiry = expiryLabel(Number(entry.secondsRemaining));

  return {
    id: entry.id,
    kind,
    label: path,
    path,
    instancePath: null,
    title: `${path}\n${KINDS[kind].label} · ${expiry}`,
    pathless: false,
    editedAt: null,
    expiry,
    bytes: Number(entry.bytes) || 0,
    search: `${path} ${kind}`.toLowerCase(),
  };
}

/**
 * @param {object} api the shared view api from app.js
 * @param {{projectId?: string, onClosed?: () => void}} options
 * @returns {{close: () => void}|null}
 */
export function openBacklogModal(api, options = {}) {
  const modalRoot = document.getElementById("modal-root");
  if (modalRoot.firstChild) return null;

  const projectId = options.projectId ?? null;
  if (!projectId) return null;

  /** `loading` · `ready` · `working` · `waiting-plugin` · `error`. */
  let phase = "loading";
  /** The 503-parked submission, replayable as-is. */
  let retry = null;
  let entries = [];
  let staged = [];
  let listError = null;
  let notice = null;
  let closed = false;
  let query = "";
  let sort = "kind";

  const overlay = el("div", { class: "modal-overlay" });
  const modal = el("div", {
    class: "modal modal-wide",
    tabindex: "-1",
    role: "dialog",
    "aria-modal": "true",
    "aria-label": TITLE,
  });

  overlay.append(modal);

  const popEscape = api.pushEscape(() => close());
  overlay.addEventListener("mousedown", (event) => {
    if (event.target === overlay) close();
  });

  modalRoot.append(overlay);
  modal.focus({ preventScroll: true });

  // A restore parked on 503 replays itself the moment the plugin connects —
  // the whole point of the wait state is that connecting Studio is the fix.
  const unsubscribePlugin =
    api.onBus?.("plugin", (status) => {
      if (closed || phase !== "waiting-plugin" || !retry) return;
      if (status?.projectId !== projectId || status.connected !== true) return;
      void submit(retry);
    }) ?? null;

  function close() {
    if (closed) return;
    closed = true;
    popEscape?.();
    unsubscribePlugin?.();
    overlay.remove();
    options.onClosed?.();
  }

  async function load() {
    phase = "loading";
    listError = null;
    render();

    try {
      const payload = await api.fetchBacklog(projectId);
      const list = Array.isArray(payload?.entries) ? payload.entries : [];

      entries = list.map(normalizeEntry);
      // A staged row that expired between loads is simply gone
      staged = staged.filter((row) => entries.some((entry) => entry.id === row.id));
      phase = "ready";
    } catch (error) {
      listError = String(error?.message ?? error);
      phase = "error";
    }

    render();
  }

  /** Runs one batch of restores, honouring the 503 wait. */
  async function submit(rows) {
    phase = "working";
    notice = null;
    render();

    const done = [];

    for (const row of rows) {
      try {
        await api.restoreBacklogEntry(projectId, { id: row.id });
        done.push(row.id);
      } catch (error) {
        const message = String(error?.message ?? error);

        // 503 is "Studio is not here yet", which is a wait, not a failure
        if (/no studio plugin/i.test(message) || /503/.test(message)) {
          retry = rows.filter((candidate) => !done.includes(candidate.id));
          phase = "waiting-plugin";
          render();
          return;
        }

        notice = `Could not put ${row.label} back: ${message}`;
        break;
      }
    }

    retry = null;

    if (done.length > 0) {
      notice = notice ?? `Put ${plural(done.length, "file", "files")} back.`;
      api.emitBus?.("backlog", null);
    }

    staged = staged.filter((row) => !done.includes(row.id));
    await load();
  }

  async function dropAll() {
    phase = "working";
    render();

    try {
      await api.dropBacklogEntries(projectId, { all: true });
      staged = [];
      notice = "Backlog emptied.";
      api.emitBus?.("backlog", null);
    } catch (error) {
      notice = `Could not empty the backlog: ${String(error?.message ?? error)}`;
    }

    await load();
  }

  function stage(entry) {
    if (staged.some((row) => row.id === entry.id)) return;
    staged = staged.concat(entry);
    render();
  }

  function unstage(entry) {
    staged = staged.filter((row) => row.id !== entry.id);
    render();
  }

  function render() {
    modal.replaceChildren();

    modal.append(
      el("div", { class: "modal-head" }, [
        el("h2", { class: "modal-title" }, TITLE),
        el("p", { class: "modal-subtitle" }, SUBTITLE),
      ]),
    );

    if (phase === "loading") {
      modal.append(el("div", { class: "modal-body" }, el("p", { class: "muted" }, "Reading the backlog…")));
      return;
    }

    if (phase === "error") {
      modal.append(
        el("div", { class: "modal-body" }, [
          el("p", { class: "error" }, listError ?? "The backlog could not be read."),
          el("button", { class: "btn", onclick: () => void load() }, "Try again"),
        ]),
      );
      modal.append(footer());
      return;
    }

    if (entries.length === 0) {
      modal.append(
        el("div", { class: "modal-body" }, [
          el("p", { class: "muted" }, "Nothing has lost to Studio. When something does, it will wait here for a day."),
        ]),
      );
      modal.append(footer());
      return;
    }

    const available = entries.filter((entry) => !staged.some((row) => row.id === entry.id));
    const filtered = sortEntries(filterEntries(available, query), {
      sort,
      kinds: KINDS,
      stamps: new Map(),
      editedAt: () => null,
    });

    const tools = el("div", { class: "stage-tools" }, [
      searchBox("Search paths…", (value) => {
        query = value;
        render();
      }),
      makeSortSelect(sort, sortOptionsFor({ stamps: false, entries }), (value) => {
        sort = value;
        render();
      }),
    ]);

    const left = stagingPane({
      title: "Replaced by Studio",
      count: filtered.length,
      tools,
      rows: filtered,
      emptyText: query ? "No paths match that search." : "Everything here is staged.",
      onRow: stage,
      action: { label: "Move all", onClick: () => filtered.forEach(stage), disabled: filtered.length === 0 },
      showVerb: true,
      kinds: KINDS,
      stamps: new Map(),
      editedAt: () => null,
      visibleRows: MAX_LIVE_ROWS,
    });

    const right = stagingPane({
      title: "Put back — write to disk and sync",
      count: staged.length,
      rows: staged,
      emptyText: "Move over anything you want back. Everything left here expires in a day.",
      onRow: unstage,
      action: { label: "Clear", onClick: () => { staged = []; render(); }, disabled: staged.length === 0 },
      kinds: KINDS,
      stamps: new Map(),
      editedAt: () => null,
      visibleRows: MAX_LIVE_ROWS,
    });

    modal.append(el("div", { class: "stage-panes" }, [left, right]));

    const note = moreRowsNote(filtered.length, MAX_LIVE_ROWS);
    if (note) modal.append(note);

    modal.append(footer());
  }

  function footer() {
    const children = [];

    if (phase === "waiting-plugin") {
      children.push(
        el("span", { class: "modal-note" }, [
          icon("clock"),
          " Waiting for Studio — this finishes on its own when the plugin connects.",
        ]),
      );
    } else if (notice) {
      children.push(el("span", { class: "modal-note" }, notice));
    } else {
      children.push(
        el("span", { class: "modal-note" }, `${plural(entries.length, "entry", "entries")} waiting`),
      );
    }

    const busy = phase === "working" || phase === "waiting-plugin";

    children.push(
      el("div", { class: "modal-actions" }, [
        el(
          "button",
          { class: "btn", onclick: () => void dropAll(), disabled: busy || entries.length === 0 },
          "Empty backlog",
        ),
        el("button", { class: "btn", onclick: close, disabled: false }, "Close"),
        el(
          "button",
          {
            class: "btn btn-primary",
            onclick: () => void submit(staged.slice()),
            disabled: busy || staged.length === 0,
          },
          staged.length > 0 ? `Put back (${staged.length})` : "Put back",
        ),
      ]),
    );

    return el("div", { class: "modal-foot" }, children);
  }

  render();
  void load();

  return { close };
}

/**
 * The strip that says something is waiting, and opens the window.
 *
 * A strip rather than an overlay, and a count rather than a question: the sync
 * already resolved, so this is information the user acts on when they choose
 * to. Returns null when the backlog is empty, so a caller can
 * `replaceChildren(backlogBanner(...))` unconditionally.
 */
export function backlogBanner(api, backlog, options = {}) {
  const total = Number.isFinite(backlog?.total) ? backlog.total : 0;
  if (!backlog || total <= 0) return null;

  return el(
    "div",
    {
      class: "notice notice-accent",
      style: options.style ?? "margin-bottom:14px",
      role: "status",
    },
    icon("disk", 14),
    el(
      "span",
      {},
      el("strong", {
        text: `${total.toLocaleString()} disk ${total === 1 ? "file" : "files"} replaced by Studio. `,
      }),
      "Put any of them back, or leave them — they are deleted a day after they land here.",
    ),
    el(
      "button",
      {
        class: "btn btn-sm btn-primary",
        type: "button",
        style: "margin-left:auto",
        on: { click: () => api.openBacklog({ projectId: backlog.projectId }) },
      },
      "Open backlog",
    ),
  );
}

/**
 * The Backlog tab: what lost, with a one-click way to put a single entry back
 * and a way into the staging window for several at once.
 *
 * Per-row buttons rather than a required drag: putting one file back is the
 * common case, and making it a two-pane exercise would be ceremony. The window
 * is there for the times it really is a pile.
 */
export function mountBacklog(root, api) {
  let entries = [];
  let error = null;
  let busy = new Set();
  let loading = true;

  const list = el("div", {});

  root.replaceChildren(
    el("div", { class: "view-head" }, [
      el("h1", {}, "Backlog"),
      el(
        "p",
        { class: "muted" },
        "Sync is Studio-first and never asks: when Studio's version wins, the disk copy is kept here " +
          "for a day so you can put it back if it mattered.",
      ),
    ]),
    list,
  );

  async function load() {
    loading = true;
    render();

    try {
      const payload = await api.fetchBacklog(api.getServedProjectId?.());
      entries = (Array.isArray(payload?.entries) ? payload.entries : []).map(normalizeEntry);
      error = null;
    } catch (problem) {
      error = String(problem?.message ?? problem);
    }

    loading = false;
    render();
  }

  async function act(entry, action) {
    busy.add(entry.id);
    render();

    try {
      if (action === "restore") {
        await api.restoreBacklogEntry(api.getServedProjectId?.(), { id: entry.id });
      } else {
        await api.dropBacklogEntries(api.getServedProjectId?.(), { id: entry.id });
      }
      api.emitBus?.("backlog", null);
    } catch (problem) {
      error = String(problem?.message ?? problem);
    }

    busy.delete(entry.id);
    await load();
  }

  function render() {
    if (loading) {
      list.replaceChildren(el("p", { class: "muted" }, "Reading the backlog…"));
      return;
    }

    if (error) {
      list.replaceChildren(
        el("p", { class: "error" }, error),
        el("button", { class: "btn", on: { click: () => void load() } }, "Try again"),
      );
      return;
    }

    if (entries.length === 0) {
      list.replaceChildren(
        el(
          "p",
          { class: "muted" },
          "Nothing has lost to Studio. When something does, it waits here for a day.",
        ),
      );
      return;
    }

    list.replaceChildren(
      el("div", { class: "view-actions" }, [
        el(
          "button",
          { class: "btn btn-primary", on: { click: () => api.openBacklog?.({}) } },
          `Put several back (${entries.length})`,
        ),
      ]),
      ...entries.map((entry) =>
        el("div", { class: "card" }, [
          el("div", { class: "card-main" }, [
            el("strong", { text: entry.label }),
            el("span", { class: "muted" }, ` ${KINDS[entry.kind].label} · ${entry.expiry}`),
          ]),
          el("div", { class: "card-actions" }, [
            el(
              "button",
              {
                class: "btn btn-sm btn-primary",
                disabled: busy.has(entry.id),
                on: { click: () => void act(entry, "restore") },
              },
              "Put back",
            ),
            el(
              "button",
              {
                class: "btn btn-sm",
                disabled: busy.has(entry.id),
                on: { click: () => void act(entry, "drop") },
              },
              "Drop",
            ),
          ]),
        ]),
      ),
    );
  }

  const off = api.onBus?.("backlog", () => void load());

  void load();

  return () => off?.();
}
