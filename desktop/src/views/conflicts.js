// views/conflicts.js — parked conflicts and the pending-divergence banner
// (Design 8.2, 7.5).
//
// Two things funnel into this view: item-level conflicts the engine parked
// while live, and a set-level divergence choice waiting for an answer. The
// second appears as a banner opening the overwrite modal; the first as cards.
//
// A card is class-aware, because the two kinds of conflict are not the same
// question. A **script** conflict is two texts, so it gets the two-pane line
// diff (views/diff.js — vendored in the sense Design asks for: no CDN, no
// dependency). A **property** conflict is two tagged-value maps, so it gets a
// property table instead; running a text differ over serialized properties
// would produce a diff of the serializer, not of the change.
//
// Every answer here is `POST /resolve` through the host, one conflict at a
// time. The bulk actions are the same call in a loop with progress, and they
// stop at the first failure rather than carrying on through a daemon that has
// started refusing.

import { el, icon, plural, relativeTime } from "./dom.js";
import { diffPanes, propertyTable } from "./diff.js";
import { reviewBanner } from "./review.js";

/** Design 6.3's vocabulary, in the words a user can act on. */
const CLASSIFICATIONS = {
  "both-edited": {
    label: "Both sides changed",
    tone: "warn",
    local: "Overwrite the Studio instance with the file on disk",
    studio: "Overwrite the file on disk with the Studio instance",
  },
  "fs-deleted-studio-edited": {
    label: "Deleted on disk · edited in Studio",
    tone: "danger",
    local: "Keep the deletion — remove the instance from Studio",
    studio: "Keep the Studio instance — write the file back to disk",
  },
  "studio-deleted-fs-edited": {
    label: "Deleted in Studio · edited on disk",
    tone: "danger",
    local: "Keep the file — recreate the instance in Studio",
    studio: "Keep the deletion — remove the file from disk",
  },
};

const UNKNOWN_CLASSIFICATION = {
  label: "Conflict",
  tone: "warn",
  local: "Keep the disk side",
  studio: "Keep the Studio side",
};

export function mountConflicts(root, api) {
  /** Last payload from the bus: see app.js `pollConflicts`. */
  let report = { items: [], total: 0, partial: false, unsupported: 0, unreachable: 0, served: 0 };
  // A choice may already be pending from before this view mounted.
  let divergence = api.getPendingDivergence();
  // ...and so may a disk review (Design 7.0), which is the code-scope answer to
  // the same set-level question the choice modal asks on a full-scope project.
  let review = api.getPendingReview?.() ?? null;
  let refreshing = false;
  /** Conflict ids with a resolution in flight, so a card cannot be answered twice. */
  const busy = new Set();
  /** Bulk run state: `{keep, done, total, error}` or null. */
  let bulk = null;

  const banner = el("div", { hidden: true });
  const summary = el("div", { class: "stat-grid" });
  const bulkNote = el("div", { hidden: true });
  const list = el("div", {});

  const keepAllLocal = el(
    "button",
    { class: "btn btn-sm", type: "button", on: { click: () => resolveAll("local") } },
    "Keep all local",
  );
  const keepAllStudio = el(
    "button",
    { class: "btn btn-sm", type: "button", on: { click: () => resolveAll("studio") } },
    "Keep all Studio",
  );
  const refresh = el(
    "button",
    { class: "btn btn-sm", type: "button", on: { click: () => refreshCounts() } },
    icon("refresh", 13),
    "Refresh",
  );
  const toolbar = el("div", { class: "toolbar" }, keepAllLocal, keepAllStudio);

  root.append(
    el(
      "div",
      { class: "view-scroll" },
      el(
        "div",
        { class: "view-head" },
        el(
          "div",
          {},
          el("h1", { class: "view-title", text: "Conflicts" }),
          el("p", {
            class: "view-sub",
            text: "Changes that landed on both sides before either could sync. Nothing is resolved until you choose a side.",
          }),
        ),
        el("div", { class: "view-head-actions" }, refresh),
      ),
      banner,
      summary,
      toolbar,
      bulkNote,
      list,
    ),
  );

  // ------------------------------------------------------------- banner ---

  function renderBanner() {
    // Design 7.0's review comes first when both exist: it describes work the
    // daemon has *already* done, so leaving it under a "nothing has changed
    // yet" banner would misstate both.
    const reviewStrip = reviewBanner(api, review);
    if (!divergence) {
      banner.hidden = reviewStrip === null;
      banner.replaceChildren(...(reviewStrip ? [reviewStrip] : []));
      return;
    }
    banner.hidden = false;
    banner.replaceChildren(
      ...(reviewStrip ? [reviewStrip] : []),
      el(
        "div",
        { class: "notice notice-accent", style: "margin-bottom:14px" },
        icon("alert", 14),
        el(
          "span",
          {},
          el("strong", { text: "Studio and disk are different. " }),
          `${divergence.total.toLocaleString()} paths differ. Nothing changes until you confirm.`,
        ),
        el(
          "button",
          {
            class: "btn btn-sm btn-primary",
            type: "button",
            style: "margin-left:auto",
            on: { click: reviewDivergence },
          },
          "Review differences",
        ),
      ),
    );
  }

  /**
   * The modal owns its own loading: step 1 reads `GET /choice` for the stats,
   * step 2 pages `GET /choice/details`. All this hands over is which project
   * and which choice — anything more would be this view guessing at a set it
   * does not hold.
   */
  function reviewDivergence() {
    api.openOverwriteModal(divergence?.fixture === true ? { fixture: true } : { divergence });
  }

  // ------------------------------------------------------------ summary ---

  function renderSummary() {
    const unknown = report.served > 0 && report.unsupported === report.served;
    summary.replaceChildren(
      stat(
        "Parked conflicts",
        unknown ? "—" : String(report.total),
        unknown
          ? "This engine has no /resolve"
          : report.total === 0
            ? "Nothing waiting"
            : "Awaiting a decision",
      ),
      review
        ? stat(
            "Disk review",
            review.total.toLocaleString(),
            "Studio already synced (§7.0)",
          )
        : stat("Pending choice", divergence ? "1" : "0", "Set-level divergence"),
      stat(
        "Served projects",
        String(report.served),
        report.unreachable > 0 ? `${report.unreachable} not answering` : "Polled every 20 s",
      ),
    );

    const idle = bulk === null;
    const nothing = report.items.length === 0;
    keepAllLocal.disabled = !idle || nothing;
    keepAllStudio.disabled = !idle || nothing;
    keepAllLocal.textContent = `Keep all local${nothing ? "" : ` (${report.items.length})`}`;
    keepAllStudio.textContent = `Keep all Studio${nothing ? "" : ` (${report.items.length})`}`;
    toolbar.hidden = report.served === 0;
  }

  function stat(label, value, note) {
    return el(
      "div",
      { class: "stat" },
      el("div", { class: "stat-label", text: label }),
      el("div", { class: "stat-value", text: value }),
      note ? el("div", { class: "stat-note", text: note }) : null,
    );
  }

  function renderBulk() {
    if (!bulk) {
      bulkNote.hidden = true;
      bulkNote.replaceChildren();
      return;
    }
    bulkNote.hidden = false;
    const side = bulk.keep === "local" ? "the disk side" : "the Studio side";
    bulkNote.replaceChildren(
      el(
        "div",
        { class: `notice ${bulk.error ? "notice-danger" : "notice-accent"}`, style: "margin-bottom:12px" },
        icon(bulk.error ? "alert" : "refresh", 14),
        el(
          "div",
          { class: "notice-body" },
          el("span", {
            text: bulk.error
              ? `Stopped after ${bulk.done} of ${bulk.total}. ${bulk.error}`
              : `Keeping ${side} — ${bulk.done} of ${bulk.total} resolved…`,
          }),
          bulk.error
            ? el("span", {
                class: "notice-meta",
                text: "The conflicts already answered stay answered; the rest were left alone.",
              })
            : null,
        ),
        bulk.error
          ? el(
              "button",
              {
                class: "btn btn-sm",
                type: "button",
                style: "margin-left:auto",
                on: {
                  click: () => {
                    bulk = null;
                    renderBulk();
                    renderSummary();
                    // The poll's updates were ignored while the run owned the
                    // list; catch the status line back up now.
                    renderStatus();
                    void api.refreshConflicts();
                  },
                },
              },
              "Dismiss",
            )
          : null,
      ),
    );
  }

  // --------------------------------------------------------------- list ---

  function renderList() {
    if (report.served === 0) {
      list.replaceChildren(
        panel(
          "No project is being served",
          "Conflicts are reported by a running daemon. Serve a project on the Projects view to start checking.",
        ),
      );
      return;
    }
    if (report.served > 0 && report.unsupported === report.served) {
      list.replaceChildren(
        panel(
          "This engine predates the conflict engine",
          "The daemon answered 404 for /resolve, so WSync cannot tell whether anything is parked. The count is shown as unknown rather than zero.",
        ),
      );
      return;
    }
    if (report.items.length === 0 && report.unreachable === report.served) {
      list.replaceChildren(
        panel(
          "No daemon answered",
          "Every served project failed to answer /resolve. The conflict count is unknown until one does.",
        ),
      );
      return;
    }
    if (report.items.length === 0) {
      list.replaceChildren(
        panel(
          "No parked conflicts",
          "The conflict engine parks an item when both sides changed it before either could sync. Nothing is waiting right now.",
        ),
      );
      return;
    }
    // Note the filter: `replaceChildren` stringifies a null argument into the
    // text "null" rather than skipping it, unlike `el`'s child handling.
    list.replaceChildren(
      ...[
        ...report.items.map((conflict) => conflictCard(conflict)),
        report.partial
          ? el("p", {
              class: "field-hint",
              style: "margin-top:10px",
              text: `Showing ${report.items.length} of ${report.total}; the daemon paged the rest.`,
            })
          : null,
      ].filter(Boolean),
    );
  }

  function panel(title, body) {
    return el(
      "div",
      { class: "empty" },
      el("div", { class: "empty-icon" }, icon("alert", 17)),
      el("div", { class: "empty-title", text: title }),
      el("p", { class: "empty-body", text: body }),
    );
  }

  /**
   * One conflict, read defensively: the daemon's record is trusted for what it
   * says and never for what it omits. A missing side renders as "not sent",
   * which is a different statement from "deleted" — and both are different
   * from an empty file.
   */
  function conflictCard(conflict) {
    const id = conflict?.id;
    const classification = CLASSIFICATIONS[conflict?.classification] ?? UNKNOWN_CLASSIFICATION;
    const path = String(conflict?.path ?? conflict?.instancePath ?? id ?? "unknown path");
    const project = api.getProject(conflict?.projectId);
    const working = busy.has(id) || bulk !== null;

    const fs = conflict?.fs ?? {};
    const studio = conflict?.studio ?? {};
    const kind = conflict?.kind === "properties" ? "properties" : conflict?.kind === "script" ? "script" : null;

    return el(
      "div",
      { class: "conflict-card" },
      el(
        "div",
        { class: "conflict-head" },
        el(
          "div",
          { class: "conflict-headings" },
          el("span", { class: "conflict-path", text: path, title: path }),
          el(
            "div",
            { class: "conflict-meta" },
            conflict?.instancePath
              ? el("span", { class: "conflict-instance", text: conflict.instancePath, title: conflict.instancePath })
              : null,
            conflict?.class ? el("span", { class: "chip chip-plain", text: conflict.class }) : null,
            el("span", { class: `chip chip-${classification.tone}`, text: classification.label }),
            conflict?.detectedAt
              ? el("span", { class: "conflict-when", text: relativeTime(conflict.detectedAt) })
              : null,
            project ? el("span", { class: "chip chip-plain", text: project.name }) : null,
          ),
        ),
        el(
          "div",
          { class: "row-actions" },
          el(
            "button",
            {
              class: "btn btn-sm",
              type: "button",
              disabled: working,
              title: classification.local,
              on: { click: () => resolveOne(conflict, "local") },
            },
            busy.has(id) ? "Resolving…" : "Keep local",
          ),
          el(
            "button",
            {
              class: "btn btn-sm",
              type: "button",
              disabled: working,
              title: classification.studio,
              on: { click: () => resolveOne(conflict, "studio") },
            },
            "Keep Studio",
          ),
        ),
      ),
      el(
        "div",
        { class: "conflict-body" },
        kind === "properties"
          ? propertyTable(fs.fsProps, studio.studioProps, {
              fsPresent: fs.present,
              studioPresent: studio.present,
            })
          : diffPanes(
              {
                title: "On disk",
                source: fs.source,
                present: fs.present,
                truncated: fs.truncated === true,
                note: "Deleted on disk.",
              },
              {
                title: "In Studio",
                source: studio.source,
                present: studio.present,
                truncated: studio.truncated === true,
                note: "Deleted in Studio.",
              },
            ),
        kind === null
          ? el("p", {
              class: "field-hint",
              style: "margin-top:8px",
              text: "The daemon did not say whether this is a script or a property conflict; it is shown as text.",
            })
          : null,
      ),
    );
  }

  // ------------------------------------------------------------ actions ---

  /** Drop a resolved card immediately, then let the poll confirm it. */
  function forget(id) {
    report = {
      ...report,
      items: report.items.filter((entry) => entry.id !== id),
      total: Math.max(0, report.total - 1),
    };
  }

  async function resolveOne(conflict, keep) {
    const id = conflict?.id;
    if (busy.has(id) || bulk !== null) return;
    busy.add(id);
    renderList();
    try {
      const result = await api.resolveConflict(conflict.projectId, conflict, keep);
      forget(id);
      api.invalidateConflictCount();
      api.setStatus(
        result.status === "already-resolved"
          ? "That conflict had already been resolved elsewhere."
          : `Kept the ${keep === "local" ? "disk" : "Studio"} side of ${conflict.path ?? conflict.instancePath ?? id}.`,
        "ok",
      );
    } catch (error) {
      api.toast("Could not resolve", { kind: "err", body: error?.message ?? String(error) });
    } finally {
      busy.delete(id);
      renderAll();
      void api.refreshConflicts();
    }
  }

  /**
   * Design 8.2's Keep-all: sequential, with progress, stopping at the first
   * refusal. Sequential rather than parallel on purpose — the daemon applies
   * each resolution to the working tree, and a burst of concurrent writes is
   * exactly the race the conflict engine exists to prevent.
   */
  async function resolveAll(keep) {
    if (bulk !== null) return;
    const queue = [...report.items];
    if (queue.length === 0) return;

    bulk = { keep, done: 0, total: queue.length, error: null };
    renderBulk();
    renderSummary();
    renderList();

    for (const conflict of queue) {
      try {
        await api.resolveConflict(conflict.projectId, conflict, keep);
        forget(conflict.id);
        bulk.done += 1;
        renderBulk();
        renderList();
      } catch (error) {
        bulk.error = error?.message ?? String(error);
        renderBulk();
        renderSummary();
        renderList();
        api.invalidateConflictCount();
        void api.refreshConflicts();
        return;
      }
    }

    api.setStatus(`${plural(bulk.done, "conflict")} resolved.`, "ok");
    bulk = null;
    api.invalidateConflictCount();
    renderBulk();
    renderAll();
    void api.refreshConflicts();
  }

  async function refreshCounts() {
    if (refreshing) return;
    refreshing = true;
    refresh.disabled = true;
    api.setStatus("Checking for conflicts…");
    try {
      await api.refreshConflicts();
    } finally {
      refreshing = false;
      refresh.disabled = false;
    }
  }

  function renderStatus() {
    if (bulk !== null) return;
    if (report.served === 0) {
      api.setStatus("No project is being served.", "warn");
    } else if (report.unsupported === report.served) {
      api.setStatus("This engine has no /resolve — the conflict count is unknown.", "warn");
    } else if (report.total > 0) {
      api.setStatus(`${plural(report.total, "parked conflict")}.`, "warn");
    } else {
      api.setStatus("No parked conflicts.", "ok");
    }
  }

  function renderAll() {
    renderBanner();
    renderSummary();
    renderBulk();
    renderList();
  }

  // ---------------------------------------------------------- lifecycle ---

  const unsubscribers = [
    api.onBus("divergence", (detail) => {
      divergence = detail;
      renderBanner();
      renderSummary();
    }),
    api.onBus("review", (detail) => {
      review = detail;
      renderBanner();
      renderSummary();
    }),
    api.onBus("conflicts", (detail) => {
      // A bulk run owns the list while it is walking it; letting a poll swap
      // the array underneath would re-add cards it has already answered.
      if (bulk !== null) return;
      // Tolerate a bare array from any other producer.
      report = Array.isArray(detail)
        ? { items: detail, total: detail.length, partial: false, unsupported: 0, unreachable: 0, served: 1 }
        : { ...report, ...(detail ?? {}) };
      renderAll();
      renderStatus();
    }),
    api.onBus("daemon", () => void refreshCounts()),
  ];

  renderAll();
  void refreshCounts();

  return () => {
    for (const unsubscribe of unsubscribers) unsubscribe();
  };
}
