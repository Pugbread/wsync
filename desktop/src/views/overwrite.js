// views/overwrite.js — the divergence choice modal (Design 7.3).
//
// **Not the default surface any more.** Design 7.0 made connect promptless:
// a code-scope project applies Studio → disk and raises a passive disk review
// (views/review.js) instead of asking anything. This modal is what a
// `scope: "full"` project still gets — the full-scope `/choice` path — and it
// stays working for exactly that reason.
//
// An app-level overlay, not a route: it can appear over any view. Two steps.
//
//   Step 1  Choose the source of truth. Two source cards with per-side counts,
//           a differences rail with the grouped summary, and three answers:
//           Keep Studio, Keep Disk, Cancel.
//   Step 2  Stage disk changes one by one (or all at once) before they are
//           pulled into Studio.
//
// The data is the daemon's. Step 1 reads `GET /choice` — aggregate stats only,
// which is all the daemon broadcasts (§7.2) — plus a short preview page for the
// rail. Step 2 pages `GET /choice/details` progressively, and every page is
// verified before a row is drawn from it (app.js `verifyDetailPage`): the ids
// in that list are what a selection is submitted as, so a page that does not
// add up is abandoned rather than staged.
//
// Submitting is `POST /choice/selection` in chunks with a verified receipt per
// chunk, then `POST /choice`. Three outcomes are reported distinctly, because
// they mean different things to a user: applied, *recorded but not applied*
// (Keep Studio — the transfer itself is a later build, and saying "done" would
// promise files that never moved), and resolved-elsewhere.
//
// A `differs` row can be opened into an inline two-pane diff, fetched lazily
// from `GET /choice/source` and rendered with the shared differ (views/diff.js).
// Lazy is the whole design: §7.2 keeps the divergence payload to paths and
// classifications, so the text only travels for the one row a user actually
// asked about. Expanding is a *read* — it never stages, unstages, or reorders
// anything, and one row is open at a time so the list stays a list.
//
// `options.fixture` keeps the daemon-free path behind the dev flag: the whole
// staging interaction runs on the set at the bottom of this file, and nothing
// is ever submitted.

import { el, icon, plural } from "./dom.js";
import { diffPanes } from "./diff.js";
import {
  filterEntries,
  MAX_LIVE_ROWS,
  moreRowsNote,
  normalizeEntry as normalizeStagingEntry,
  revealInList,
  searchBox,
  sortEntries,
  sortOptionsFor,
  sortSelect as makeSortSelect,
  stagingPane,
  syncSortSelect,
} from "./staging.js";

const KINDS = {
  "only-on-disk": { mark: "+", markClass: "mark-add", label: "Only on disk", verb: "Create in Studio" },
  differs: { mark: "~", markClass: "mark-differs", label: "Differs", verb: "Replace Studio version with disk" },
  "missing-on-disk": { mark: "−", markClass: "mark-missing", label: "Missing on disk", verb: "Remove from Studio" },
};

/** Design 7.3: the rail shows the top 6 and then "+N more". */
const RAIL_PREVIEW = 6;

/** One page of `GET /choice/details`. The contract's ceiling is 1024. */
const PAGE_SIZE = 512;

/**
 * @param {object} api the shared view api from app.js
 * @param {{fixture?: boolean, divergence?: object, onClosed?: () => void}} options
 * @returns {{close: () => void, getChoiceId: () => string|null,
 *   supersede: (summary: object) => void,
 *   resolvedElsewhere: (choiceId: string|null, choice: string|null) => void}|null}
 *   a handle, or null when there is nothing to open.
 */
export function openOverwriteModal(api, options = {}) {
  const modalRoot = document.getElementById("modal-root");
  if (modalRoot.firstChild) return null;

  const fixture = options.fixture === true;
  const seed = options.divergence ?? null;
  const projectId = fixture ? null : (seed?.projectId ?? null);
  if (!fixture && !projectId) return null;

  const preset = fixture ? mockDivergence() : null;

  // ------------------------------------------------------------- state ---

  /** `loading` · `choose` · `stage` · `working` · `done` · `error`. */
  let phase = fixture ? "choose" : "loading";
  let choiceId = fixture ? preset.choiceId : (seed?.choiceId ?? null);
  let stats = fixture ? preset.summary : blankStats(seed);
  /** Entries loaded so far, in the daemon's order. */
  let entries = fixture ? preset.entries.map(normalizeEntry) : [];
  /** Where the next page starts, or null once the set is complete. */
  let cursor = fixture ? null : 0;
  let totalCount = fixture ? preset.entries.length : (seed?.total ?? null);
  let listError = null;
  let loading = false;
  let progress = null;
  let outcome = null;
  let fatal = null;
  /** True from the first byte of our own submission until it answers. */
  let submitting = false;
  let closed = false;

  let query = "";
  let sort = fixture ? "recent" : "set";
  let visibleRows = MAX_LIVE_ROWS;
  const staged = new Set();

  /** The one row whose diff is open, or null. Design 7.3: one at a time. */
  let expandedId = null;
  /**
   * Row id → `{status, …}`, for the modal's lifetime only.
   *
   * The divergence set is frozen (§7.2), so a row's two sides cannot change
   * under us while this modal is open — which is exactly what makes caching
   * them safe, and what makes a 404 mean "the set was replaced" rather than
   * "try again". Failures are cached too, so a collapse/expand does not
   * re-hammer a daemon that is refusing; the error state offers a retry that
   * drops the entry explicitly.
   */
  const sources = new Map();
  /** Set when a render should hand focus back to a row's diff toggle. */
  let focusDiffToggle = null;

  /**
   * The last-edited ledger for this project, re-read per render rather than
   * baked into the rows: the feed keeps arriving while the modal is open, and a
   * stamp that landed a second ago should still sort.
   */
  function editedStamps() {
    if (fixture) return preset.stamps;
    return api.lastEditedStamps?.(projectId) ?? new Map();
  }

  function editedAt(entry, stamps) {
    if (Number.isFinite(entry.editedAt)) return entry.editedAt;
    return entry.path === null ? null : (stamps.get(entry.path) ?? null);
  }

  // --------------------------------------------------------------- DOM ---

  const overlay = el("div", { class: "overlay", role: "presentation" });
  const modal = el("div", {
    class: "modal",
    role: "dialog",
    "aria-modal": "true",
    "aria-labelledby": "overwrite-title",
    tabindex: "-1",
  });
  overlay.append(modal);

  // Escape and a scrim click *dismiss* — they do not answer. Design 7.3 makes
  // Cancel a real decision the daemon is told about, and a stray Escape must
  // not be able to submit one. Dismissing leaves the choice pending, which is
  // why the banner stays up.
  const popEscape = api.pushEscape(() => dismiss());
  overlay.addEventListener("mousedown", (event) => {
    if (event.target === overlay) dismiss();
  });

  modalRoot.append(overlay);
  modal.focus({ preventScroll: true });
  if (fixture) api.emitBus("divergence", { ...preset.summary, fixture: true });

  function close() {
    if (closed) return;
    closed = true;
    popEscape();
    overlay.remove();
    options.onClosed?.();
  }

  function dismiss() {
    // Never in the middle of a submission: the chunk sequence would be left
    // half-sent with no way to report what happened.
    if (submitting) return;
    close();
    if (phase !== "done") {
      api.setStatus("The divergence review was dismissed — the decision is still pending.", "warn");
    }
  }

  // ------------------------------------------------------------ loading ---

  /** Step 1: the authoritative stats, plus a preview page for the rail. */
  async function loadChoice() {
    phase = "loading";
    render();
    try {
      const pending = await api.fetchPendingChoice(projectId);
      if (closed) return;
      if (!pending.pending) {
        closeAsResolvedElsewhere();
        return;
      }
      choiceId = pending.choiceId;
      stats = pending.stats;
      totalCount = pending.stats.total;
      phase = "choose";
      render();

      // The rail's "top 6": one small verified page, not the whole set.
      if (totalCount > 0) {
        const page = await api.fetchDivergenceDetails(projectId, {
          choiceId,
          cursor: 0,
          limit: Math.max(RAIL_PREVIEW, 8),
          expectedTotal: totalCount,
        });
        if (closed || phase !== "choose") return;
        entries = page.items.map(normalizeEntry);
        cursor = page.nextCursor;
        totalCount = page.totalCount;
        render();
      }
    } catch (error) {
      if (closed) return;
      // A failed preview must not blank a working step 1: only a failure that
      // left us with no stats at all is fatal.
      if (phase === "choose") {
        listError = messageOf(error);
        render();
        return;
      }
      fatal = messageOf(error);
      phase = "error";
      render();
    }
  }

  /**
   * Page the rest of the set in, rendering as it arrives (Design 7.3: a 25 000
   * entry divergence must page, and the user must not wait for the last page to
   * start staging). Sequential by design — a cursor is only known once the
   * previous page has answered.
   */
  async function loadRest() {
    if (fixture || loading || cursor === null) return;
    loading = true;
    listError = null;
    renderStage();

    while (cursor !== null && !closed) {
      let page;
      try {
        page = await api.fetchDivergenceDetails(projectId, {
          choiceId,
          cursor,
          limit: PAGE_SIZE,
          expectedTotal: totalCount,
        });
      } catch (error) {
        // Stop where it broke and keep what is already staged: the cursor still
        // points at the failed page, so `Retry list` resumes rather than
        // restarts.
        listError = messageOf(error);
        loading = false;
        renderStage();
        return;
      }
      if (closed) return;
      entries = entries.concat(page.items.map(normalizeEntry));
      cursor = page.nextCursor;
      totalCount = page.totalCount;
      renderStage();
      // Yield so a 49-page set stays a responsive list rather than a freeze.
      await new Promise((resolve) => setTimeout(resolve, 0));
    }

    loading = false;
    renderStage();
  }

  // ---------------------------------------------------------- decisions ---

  function fixtureOnly(what) {
    api.toast(what, {
      kind: "warn",
      body: "This is the layout fixture — nothing was sent to a daemon.",
    });
  }

  /**
   * `POST /choice` with `studio`: the daemon runs the fenced push
   * (structure → diskFence → sources → diskRevalidate) with backups.
   */
  async function keepStudio() {
    if (fixture) return fixtureOnly("Keep Studio");
    await submit("Recording your decision…", () => api.submitChoice(projectId, "studio", { choiceId }));
  }

  function keepDisk() {
    if (totalCount === 0) {
      // Design 7.3: no differences means no staging step — pull and be done.
      void moveAll();
      return;
    }
    phase = "stage";
    visibleRows = MAX_LIVE_ROWS;
    render();
    void loadRest();
  }

  /** `Move all disk changes`: the full pull, which skips the review entirely. */
  async function moveAll() {
    if (fixture) return fixtureOnly("Move all disk changes");
    await submit("Pulling every disk change into Studio…", () =>
      api.submitChoice(projectId, "disk", { choiceId, mode: "all" }),
    );
  }

  /**
   * The selective pull: chunked ids with verified receipts, then the decision.
   *
   * A selection covering the whole set upgrades to `mode:"all"` (Design 7.3) —
   * same result, one round trip, and no id list to get wrong.
   */
  async function moveStaged() {
    if (fixture) return fixtureOnly(`Move ${staged.size} to Studio`);
    const ids = [...staged].sort((left, right) => left - right);
    if (ids.length === 0) return;
    if (totalCount !== null && ids.length === totalCount) {
      await moveAll();
      return;
    }

    await submit(`Submitting ${plural(ids.length, "change")}…`, async () => {
      const receipt = await api.submitChoiceSelection(projectId, {
        choiceId,
        ids,
        onProgress: ({ sent, total, chunkIndex, chunkCount }) => {
          progress = {
            label:
              chunkCount > 1
                ? `Uploading chunk ${chunkIndex + 1} of ${chunkCount} — ${sent.toLocaleString()} of ${total.toLocaleString()} ids`
                : `Uploaded ${sent.toLocaleString()} of ${total.toLocaleString()} ids`,
            value: sent,
            max: total,
          };
          renderWorking();
        },
      });
      if (receipt.status === "resolved-elsewhere") return receipt;

      progress = { label: `Applying ${plural(ids.length, "change")} to Studio…`, value: null, max: null };
      renderWorking();
      return api.submitChoice(projectId, "disk", { choiceId });
    });
  }

  /** The explicit Cancel button — a decision, told to the daemon. */
  async function cancelChoice() {
    if (fixture) {
      close();
      api.emitBus("divergence", null);
      api.setStatus("Divergence left unresolved — nothing was changed.", "warn");
      return;
    }
    await submit("Cancelling…", () => api.submitChoice(projectId, "cancel", { choiceId }));
  }

  /** One submission envelope: the working phase, the outcome, the errors. */
  async function submit(label, run) {
    if (submitting) return;
    submitting = true;
    phase = "working";
    progress = { label, value: null, max: null };
    render();

    try {
      const result = await run();
      if (closed) return;
      applyOutcome(result);
    } catch (error) {
      if (closed) return;
      phase = "error";
      fatal = messageOf(error);
      render();
    } finally {
      submitting = false;
    }
  }

  function applyOutcome(result) {
    switch (result?.status) {
      case "resolved-elsewhere":
        closeAsResolvedElsewhere();
        return;
      case "pending-application":
        // The honest state. Design 7.4-A's Studio → disk transfer is a later
        // build; reporting this as success would claim files moved.
        outcome = {
          tone: "warn",
          title: "Decision recorded — Studio → disk transfer lands in a later build",
          body: "The daemon has your answer and will not ask again for this set. Nothing on disk has been overwritten yet, and the project stays unsynced until the transfer ships.",
        };
        phase = "done";
        api.emitBus("divergence", null);
        api.setStatus("Keep Studio recorded — the transfer itself lands in a later build.", "warn");
        render();
        return;
      case "cancelled":
        close();
        api.emitBus("divergence", null);
        api.setStatus("Divergence left unresolved — the link stays up but unsynced.", "warn");
        return;
      case "applied": {
        const moved = staged.size > 0 && staged.size !== totalCount ? staged.size : totalCount;
        close();
        api.emitBus("divergence", null);
        api.setStatus(`${plural(moved ?? 0, "disk change")} are being pulled into Studio.`, "ok");
        api.toast("Keep Disk", {
          kind: "ok",
          body: "The daemon is applying the pull inside one Studio undo.",
        });
        return;
      }
      default:
        phase = "error";
        fatal = "The daemon accepted the decision but did not say what it did with it.";
        render();
    }
  }

  function closeAsResolvedElsewhere() {
    close();
    api.emitBus("divergence", null);
    api.setStatus("Initial sync resolved elsewhere.", "warn");
    api.toast("Initial sync resolved elsewhere", {
      kind: "warn",
      body: "Another client — the CLI, another window, or the Studio prompt — answered this choice first.",
    });
  }

  // --------------------------------------------------------- rendering ---

  function render() {
    modal.replaceChildren(...frame());
  }

  function frame() {
    if (phase === "loading") return loadingStep();
    if (phase === "working") return workingStep();
    if (phase === "done") return doneStep();
    if (phase === "error") return errorStep();
    if (phase === "stage") return stepTwo();
    return stepOne();
  }

  function head(title, subtitle) {
    return el(
      "div",
      { class: "modal-head" },
      el(
        "div",
        {},
        el("h2", { class: "modal-title", id: "overwrite-title", text: title }),
        el("p", { class: "modal-sub", text: subtitle }),
      ),
    );
  }

  function loadingStep() {
    return [
      head("Reading the pending decision…", "WSync is asking the daemon what differs before it shows you anything."),
      el("div", { class: "modal-body" }, el("div", { class: "stage-empty", text: "Loading…" })),
      el(
        "div",
        { class: "modal-foot" },
        el("div", { class: "modal-foot-spacer" }),
        el("button", { class: "btn", type: "button", on: { click: dismiss } }, "Close"),
      ),
    ];
  }

  function workingStep() {
    const determinate = Number.isFinite(progress?.value) && Number.isFinite(progress?.max) && progress.max > 0;
    return [
      head("Applying your decision", "Nothing else can be answered until this finishes."),
      el(
        "div",
        { class: "modal-body" },
        el(
          "div",
          { class: "work-panel" },
          el("div", { class: "work-label", text: progress?.label ?? "Working…" }),
          el(
            "div",
            { class: "progress", role: "progressbar", ...(determinate ? { "aria-valuenow": String(progress.value), "aria-valuemin": "0", "aria-valuemax": String(progress.max) } : {}) },
            el("div", {
              class: `progress-bar${determinate ? "" : " progress-bar-indeterminate"}`,
              style: determinate ? `width:${Math.round((progress.value / progress.max) * 100)}%` : "",
            }),
          ),
          el("p", {
            class: "field-hint",
            text: "Every chunk is acknowledged by the daemon and checked here before the next one is sent.",
          }),
        ),
      ),
      el("div", { class: "modal-foot" }, el("div", { class: "modal-foot-spacer" })),
    ];
  }

  function doneStep() {
    return [
      head(outcome.title, ""),
      el(
        "div",
        { class: "modal-body" },
        el(
          "div",
          { class: `notice notice-${outcome.tone === "ok" ? "accent" : outcome.tone}` },
          icon(outcome.tone === "ok" ? "check" : "alert", 14),
          el("span", { text: outcome.body }),
        ),
      ),
      el(
        "div",
        { class: "modal-foot" },
        el("div", { class: "modal-foot-spacer" }),
        el("button", { class: "btn btn-primary", type: "button", on: { click: close } }, "Close"),
      ),
    ];
  }

  function errorStep() {
    return [
      head("The decision was not applied", "Nothing has been changed on either side."),
      el(
        "div",
        { class: "modal-body" },
        el("div", { class: "notice notice-danger" }, icon("alert", 14), el("span", { text: fatal ?? "" })),
      ),
      el(
        "div",
        { class: "modal-foot" },
        el("div", { class: "modal-foot-spacer" }),
        el(
          "button",
          {
            class: "btn",
            type: "button",
            on: {
              click: () => {
                fatal = null;
                if (fixture) {
                  phase = "choose";
                  render();
                } else {
                  void loadChoice();
                }
              },
            },
          },
          "Try again",
        ),
        el("button", { class: "btn btn-primary", type: "button", on: { click: close } }, "Close"),
      ),
    ];
  }

  // Each step returns the modal's head/body/foot as a flat list, so there is
  // exactly one `.modal` box on screen and the body scrolls, not the frame.
  function stepOne() {
    const preview = entries.slice(0, RAIL_PREVIEW);
    const groups = [
      ["only-on-disk", stats.onlyOnDisk],
      ["differs", stats.differs],
      ["missing-on-disk", stats.missingOnDisk],
    ];

    return [
      head(
        "Studio and disk are different",
        "Nothing changes until you confirm. Choose which side is the source of truth.",
      ),
      el(
        "div",
        { class: "modal-body" },
        el(
          "div",
          { class: "source-cards" },
          sourceCard("studio", "Studio", "current place", stats.studioCount, "instances in the open place"),
          sourceCard("disk", "Disk", projectName(), stats.diskCount, "files in the local project"),
        ),
        el(
          "div",
          { class: "diff-rail" },
          el(
            "div",
            { class: "diff-rail-head" },
            ...groups.map(([kind, count]) =>
              el(
                "div",
                { class: "diff-rail-stat" },
                el("span", { class: "diff-rail-stat-value", text: count.toLocaleString() }),
                el("span", { class: "diff-rail-stat-label", text: KINDS[kind].label }),
              ),
            ),
          ),
          el("div", { class: "diff-rail-list" }, ...preview.map((entry) => railRow(entry))),
          listError
            ? el("div", { class: "diff-rail-more", text: `The preview could not be read: ${listError}` })
            : preview.length === 0
              ? el("div", { class: "diff-rail-more", text: "Loading the first paths…" })
              : stats.total > preview.length
                ? el("div", {
                    class: "diff-rail-more",
                    text: `+${(stats.total - preview.length).toLocaleString()} more`,
                  })
                : null,
        ),
      ),
      el(
        "div",
        { class: "modal-foot" },
        el("span", { class: "modal-foot-note", text: `${plural(stats.total, "path")} differ` }),
        el("div", { class: "modal-foot-spacer" }),
        el("button", { class: "btn", type: "button", on: { click: cancelChoice } }, "Cancel"),
        el("button", { class: "btn", type: "button", on: { click: keepStudio } }, icon("studio", 13), "Keep Studio"),
        el(
          "button",
          { class: "btn btn-primary", type: "button", on: { click: keepDisk } },
          icon("disk", 13),
          "Keep Disk",
        ),
      ),
    ];
  }

  function sourceCard(side, title, subtitle, count, note) {
    return el(
      "div",
      { class: "source-card" },
      el(
        "div",
        { class: "source-card-head" },
        el("span", { class: "source-icon" }, icon(side === "studio" ? "studio" : "disk", 15)),
        el(
          "div",
          {},
          el("div", { class: "source-card-title", text: title }),
          el("div", { class: "source-card-sub", text: subtitle }),
        ),
      ),
      el("div", { class: "source-card-count", text: count.toLocaleString() }),
      el("div", { class: "source-card-count-note", text: note }),
    );
  }

  function railRow(entry) {
    const kind = KINDS[entry.kind];
    return el(
      "div",
      { class: "diff-row" },
      el("span", { class: `mark ${kind.markClass}`, text: kind.mark, "aria-hidden": "true" }),
      el("span", { class: "diff-row-path", text: entry.label, title: entry.title }),
      el("span", { class: "diff-row-note", text: kind.label }),
    );
  }

  // ------------------------------------------------------------ step two ---

  const panes = el("div", { class: "stage-panes" });
  const listNote = el("div", { class: "stage-note" });
  const footCount = el("span", { class: "modal-foot-note" });
  const moveButton = el(
    "button",
    { class: "btn btn-primary", type: "button", on: { click: () => void moveStaged() } },
    "Move 0 to Studio",
  );

  // Built once and kept, not rebuilt per render: the search box has to survive
  // a restage without losing what was typed into it, and the sort list has to
  // be able to grow a "Recently edited" option the moment the ledger gets one.
  const { input: searchInput, box: searchWrap } = searchBox("Search disk changes", (value) => {
    query = value;
    visibleRows = MAX_LIVE_ROWS;
    renderStage();
  });

  const sortSelect = makeSortSelect("Sort disk changes", (value) => {
    sort = value;
    renderStage();
  });

  function stepTwo() {
    searchInput.value = query;
    renderStage();

    return [
      head(
        "Stage disk changes",
        "Move the changes you want pulled into Studio. Anything left on the left stays as it is.",
      ),
      el(
        "div",
        { class: "modal-body" },
        el(
          "div",
          { class: "stage-toolbar" },
          searchWrap,
          sortSelect,
          el("div", { class: "toolbar-spacer" }),
          el(
            "button",
            { class: "btn btn-sm", type: "button", on: { click: () => void moveAll() } },
            "Move all disk changes",
          ),
        ),
        panes,
        listNote,
      ),
      el(
        "div",
        { class: "modal-foot" },
        footCount,
        el("div", { class: "modal-foot-spacer" }),
        el("button", { class: "btn", type: "button", on: { click: cancelChoice } }, "Cancel"),
        el(
          "button",
          {
            class: "btn",
            type: "button",
            on: {
              click: () => {
                phase = "choose";
                render();
              },
            },
          },
          "Back",
        ),
        moveButton,
      ),
    ];
  }

  /** Redraw only the parts of step 2 that change as pages and picks arrive. */
  function renderStage() {
    if (phase !== "stage") return;
    const stamps = editedStamps();
    sort = syncSortSelect(sortSelect, sort, sortOptionsFor({ stamps, entries }));
    const remaining = sortEntries(
      filterEntries(entries.filter((entry) => !staged.has(entry.id)), query),
      { sort, kinds: KINDS, stamps, editedAt },
    );
    const picked = filterEntries(entries.filter((entry) => staged.has(entry.id)), query);

    const shared = {
      kinds: KINDS,
      stamps,
      editedAt,
      visibleRows,
      expandedId,
      diffPanelFor: diffPanel,
      // Design 7.3 puts the inline diff on `differs` rows: the other two kinds
      // have only one side, and a one-sided "diff" is just the file.
      diffable: (entry) => entry.kind === "differs",
      onToggleDiff: toggleDiff,
    };

    panes.replaceChildren(
      stagingPane({
        ...shared,
        title: "Disk changes",
        count: remaining.length,
        tools: [
          el(
            "button",
            {
              class: "btn btn-sm",
              type: "button",
              disabled: remaining.length === 0,
              on: {
                click: () => {
                  for (const entry of remaining) staged.add(entry.id);
                  renderStage();
                },
              },
            },
            loading || cursor !== null ? "Stage all loaded" : "Stage all",
          ),
        ],
        rows: remaining,
        emptyText: query ? `Nothing matches "${query}".` : "Everything is staged.",
        onRow: (entry) => {
          staged.add(entry.id);
          renderStage();
        },
        action: "arrowRight",
      }),
      stagingPane({
        ...shared,
        title: "Staged for Studio",
        count: picked.length,
        tools: [
          el(
            "button",
            {
              class: "btn btn-sm",
              type: "button",
              disabled: picked.length === 0,
              on: {
                click: () => {
                  staged.clear();
                  renderStage();
                },
              },
            },
            "Unstage all",
          ),
        ],
        rows: picked,
        emptyText: "Pick the disk changes you want pulled into Studio.",
        onRow: (entry) => {
          staged.delete(entry.id);
          renderStage();
        },
        action: "arrowLeft",
        showVerb: true,
      }),
    );

    renderListNote(remaining.length);

    const known = totalCount ?? entries.length;
    footCount.textContent = `${staged.size.toLocaleString()} of ${known.toLocaleString()} staged`;
    moveButton.textContent =
      staged.size === known && known > 0
        ? "Move all disk changes"
        : `Move ${staged.size.toLocaleString()} to Studio`;
    moveButton.disabled = staged.size === 0;

    // The panes are rebuilt wholesale, so a toggle that was just pressed would
    // otherwise lose focus mid-interaction — keyboard users included. The
    // panel is then scrolled just far enough to be visible: expanding a row
    // near the bottom of a scrolling list must not open something off-screen.
    if (focusDiffToggle !== null) {
      const toggle = panes.querySelector(`[data-diff-for="${focusDiffToggle}"]`);
      focusDiffToggle = null;
      toggle?.focus({ preventScroll: true });
      if (expandedId !== null) revealInList(panes.querySelector(`#stage-diff-${expandedId}`));
    }
  }

  function renderListNote(shown) {
    const known = totalCount ?? entries.length;
    const children = [];

    if (listError) {
      children.push(
        el(
          "div",
          { class: "notice notice-danger" },
          icon("alert", 14),
          el(
            "div",
            { class: "notice-body" },
            el("span", { text: `The list stopped loading at ${entries.length.toLocaleString()} of ${known.toLocaleString()}: ${listError}` }),
            el("span", {
              class: "notice-meta",
              text: "Staging what has arrived is still safe — a selection only ever submits the ids you picked.",
            }),
          ),
          el(
            "button",
            {
              class: "btn btn-sm",
              type: "button",
              style: "margin-left:auto",
              on: { click: () => void loadRest() },
            },
            "Retry list",
          ),
        ),
      );
    } else if (loading || cursor !== null) {
      children.push(
        el("p", {
          class: "field-hint",
          text: `Loading the divergence set — ${entries.length.toLocaleString()} of ${known.toLocaleString()} paths so far.`,
        }),
      );
    }

    children.push(
      moreRowsNote(shown, visibleRows, () => {
        visibleRows += MAX_LIVE_ROWS;
        renderStage();
      }),
    );

    listNote.replaceChildren(...children.filter(Boolean));
  }

  // ------------------------------------------------------- inline row diff ---

  /**
   * Open, or close, one row's diff.
   *
   * Toggling is deliberately the *only* thing this does to the modal's state:
   * `staged` is not touched, the sort is not touched, and the fetch below can
   * never stage anything. Expanding a row to look at it must not be a way to
   * accidentally pull it.
   */
  function toggleDiff(entry) {
    if (expandedId === entry.id) {
      expandedId = null;
      focusDiffToggle = entry.id;
      renderStage();
      return;
    }
    expandedId = entry.id;
    focusDiffToggle = entry.id;
    if (!sources.has(entry.id)) void loadSource(entry);
    renderStage();
  }

  async function loadSource(entry) {
    if (fixture) {
      sources.set(entry.id, fixtureSource(entry));
      renderStage();
      return;
    }

    sources.set(entry.id, { status: "loading" });
    try {
      const result = await api.fetchChoiceSource(projectId, {
        choiceId,
        id: entry.id,
        state: entry.kind,
      });
      if (closed) return;
      if (result.status === "stale") {
        // Design 7.2's stale-set rule reaching us through a row: every id on
        // screen — staged or not — now addresses a set the daemon threw away,
        // so this is the same supersede path a `choice-needed` takes.
        sources.delete(entry.id);
        supersedeLocally();
        return;
      }
      sources.set(entry.id, result);
    } catch (error) {
      if (closed) return;
      sources.set(entry.id, { status: "error", message: messageOf(error) });
    }
    renderStage();
  }

  function diffPanel(entry, panelId) {
    const state = sources.get(entry.id) ?? { status: "loading" };
    return el(
      "div",
      { class: "stage-diff", id: panelId, role: "region", "aria-label": `Diff for ${entry.label}` },
      ...diffBody(entry, state),
    );
  }

  function diffBody(entry, state) {
    switch (state.status) {
      case "loading":
        return [el("div", { class: "stage-diff-note", text: "Reading both sides…" })];

      case "not-script":
        return [
          el("div", {
            class: "stage-diff-note",
            text: "Not a script — property differences are decided by staging, not diffed.",
          }),
        ];

      case "no-plugin":
        return [
          el("div", {
            class: "stage-diff-note stage-diff-note-warn",
            text: "Studio plugin not connected — diff unavailable.",
          }),
          retryRow(entry, "The Studio side is read live from the place; reconnect Studio and try again."),
        ];

      case "error":
        return [
          el("div", { class: "stage-diff-note stage-diff-note-warn", text: state.message }),
          retryRow(entry, "Nothing was staged or changed by the attempt."),
        ];

      case "ok":
        return [
          diffPanes(
            {
              title: "On disk",
              source: state.disk.source,
              present: state.disk.present,
              truncated: state.disk.truncated,
              note: "Not on disk.",
            },
            {
              title: "In Studio",
              source: state.studio.source,
              present: state.studio.present,
              truncated: state.studio.truncated,
              note: "Not in Studio.",
            },
          ),
          state.disk.truncated || state.studio.truncated
            ? el("div", {
                class: "stage-diff-note",
                text: "The daemon sends at most 256 KiB per side; the rest of this file is not shown, so the diff below it is unknown.",
              })
            : null,
        ];

      default:
        return [el("div", { class: "stage-diff-note", text: "No diff is available for this row." })];
    }
  }

  function retryRow(entry, why) {
    return el(
      "div",
      { class: "stage-diff-actions" },
      el("span", { class: "stage-diff-why", text: why }),
      el(
        "button",
        {
          class: "btn btn-sm",
          type: "button",
          on: {
            click: () => {
              sources.delete(entry.id);
              void loadSource(entry);
              renderStage();
            },
          },
        },
        "Try again",
      ),
    );
  }

  /** The stale-set close, reached either from an event or from a row's 404. */
  function supersedeLocally() {
    if (submitting || phase === "done") return;
    close();
    api.setStatus("The divergence set was recomputed — reopen the review to see the new one.", "warn");
    api.toast("The divergence set was replaced", {
      kind: "warn",
      body: "Studio changed while this list was open, so the daemon restarted the comparison. Nothing was staged or pulled.",
    });
  }

  function renderWorking() {
    if (phase === "working") render();
  }

  function projectName() {
    return api.getProject(projectId)?.name ?? "local project";
  }

  // ------------------------------------------------------------- handle ---

  render();
  if (!fixture) void loadChoice();

  return {
    close,
    getChoiceId: () => choiceId,

    /**
     * A `choice-needed` for a different set landed: this one was restarted
     * (Design 7.2's stale-set rule), so every id on screen now addresses a set
     * the daemon has thrown away.
     */
    supersede(summary) {
      if (fixture || submitting || phase === "done") return;
      // Still finding out which set this is: the announcement may well be the
      // one being loaded, and closing on it would be self-inflicted.
      if (!choiceId) return;
      if (!summary?.choiceId || summary.choiceId === choiceId) return;
      supersedeLocally();
    },

    /**
     * A `choice-made` landed. If it is the echo of our own submission the
     * outcome path is already handling it; otherwise somebody else answered
     * and this modal is asking a question that no longer exists.
     */
    resolvedElsewhere(madeChoiceId) {
      if (fixture || submitting || phase === "done") return;
      if (madeChoiceId && choiceId && madeChoiceId !== choiceId) return;
      closeAsResolvedElsewhere();
    },
  };
}

// ------------------------------------------------------------- helpers ----

function blankStats(seed) {
  const count = (value) => (Number.isFinite(value) ? Number(value) : 0);
  return {
    total: count(seed?.total),
    studioCount: count(seed?.studioCount),
    diskCount: count(seed?.diskCount),
    onlyOnDisk: count(seed?.onlyOnDisk),
    differs: count(seed?.differs),
    missingOnDisk: count(seed?.missingOnDisk),
  };
}

/**
 * One row shape for both sources: the shared normalizer with this modal's
 * vocabulary. `path` is nullable in the divergence contract — a Studio-only
 * instance whose file path the middleware cannot predict has none — and an
 * unknown state is read as `differs`, the only kind with two sides.
 */
function normalizeEntry(entry) {
  return normalizeStagingEntry(entry, KINDS, "differs");
}

function messageOf(error) {
  return error?.message ?? String(error);
}

// ------------------------------------------------------------------ fixture --

/**
 * A representative divergence set: all three change kinds, nested paths, and a
 * spread of edit times so every sort produces a visibly different order.
 * Used only behind the dev flag (`index.html?dev=overwrite`, or Settings →
 * Developer → Divergence modal): it never reaches a daemon, and every control
 * that would submit says so instead.
 */
export function mockDivergence() {
  const minute = 60_000;
  const now = Date.now();
  const rows = [
    ["src/shared/Signal.luau", "only-on-disk", 2],
    ["src/server/Services/Economy.server.luau", "differs", 6],
    ["src/client/Controllers/Camera.client.luau", "differs", 11],
    ["src/shared/Config/Balance.json", "only-on-disk", 18],
    ["src/server/Services/Matchmaking.server.luau", "missing-on-disk", 24],
    ["src/shared/Types.luau", "differs", 31],
    ["src/client/UI/Hud.client.luau", "only-on-disk", 44],
    ["src/client/UI/Inventory.client.luau", "differs", 58],
    ["src/shared/Util/Table.luau", "only-on-disk", 73],
    ["src/server/Data/Profiles.server.luau", "missing-on-disk", 96],
    ["src/shared/Assets/Sounds.model.json", "only-on-disk", 130],
    ["src/client/Effects/Trail.model.json", "differs", 168],
    ["src/server/Services/Analytics.server.luau", "missing-on-disk", 205],
    ["src/shared/Constants.luau", "differs", 260],
  ];

  const entries = rows.map(([path, state, ago], index) => ({
    id: index,
    path,
    instancePath: `Workspace.${path.replace(/\.[^.]+$/u, "").split("/").join(".")}`,
    state,
    class: path.endsWith(".json") ? "Configuration" : "ModuleScript",
    editedAt: now - ago * minute,
  }));

  return {
    choiceId: "fixture-choice",
    entries,
    /** What the last-edited ledger would hold, so the sort has real input. */
    stamps: new Map(entries.map((entry) => [entry.path, entry.editedAt])),
    summary: {
      total: entries.length,
      studioCount: 8412,
      diskCount: 8398,
      onlyOnDisk: entries.filter((entry) => entry.state === "only-on-disk").length,
      differs: entries.filter((entry) => entry.state === "differs").length,
      missingOnDisk: entries.filter((entry) => entry.state === "missing-on-disk").length,
    },
  };
}

/**
 * The fixture's answer for one row's `GET /choice/source`.
 *
 * Cycles the four states a real row can be in — a plain diff, a truncated pair,
 * a property row, and a disconnected plugin — so the dev preview shows every
 * branch of the inline diff without a daemon to arrange them.
 */
function fixtureSource(entry) {
  if (entry.label.endsWith(".json")) {
    return {
      status: "not-script",
      id: entry.id,
      message: "not a script row",
    };
  }
  if (entry.id % 7 === 4) {
    return { status: "no-plugin", id: entry.id, message: "no plugin connected" };
  }

  const name = entry.label.split("/").pop().replace(/\..*$/u, "");
  const disk = [
    "--!strict",
    `-- ${entry.label}`,
    "",
    `local ${name} = {}`,
    "",
    `function ${name}.step(dt: number)`,
    "\tlocal budget = 1 / 60",
    "\treturn math.min(dt, budget)",
    "end",
    "",
    `return ${name}`,
  ].join("\n");
  const studio = [
    "--!strict",
    `-- ${entry.label}`,
    "",
    `local ${name} = {}`,
    "",
    `function ${name}.step(dt: number, scale: number?)`,
    "\tlocal budget = 1 / 30",
    "\treturn math.min(dt * (scale or 1), budget)",
    "end",
    "",
    `return ${name}`,
  ].join("\n");

  const truncated = entry.id % 5 === 2;
  return {
    status: "ok",
    id: entry.id,
    path: entry.path,
    instancePath: entry.label,
    state: entry.kind,
    disk: { present: true, source: disk, truncated },
    studio: { present: true, source: studio, truncated },
  };
}
