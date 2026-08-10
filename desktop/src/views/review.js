// views/review.js — the initial-sync review (Design 7.0, revised).
//
// The surface that replaced the blocking choice modal on code-scope projects.
// By the time this opens, **the sync already happened**: connect hydrated,
// diffed, and applied Studio → disk under a fence, and live two-way sync is
// running. What is left is a pile of disk items that did not survive that
// apply — files that exist only on disk, and the preserved originals of files
// Studio overwrote — and the only question is whether the user wants any of
// them kept, i.e. pushed back into Studio.
//
// It **opens itself** when the review arrives (app.js auto-opens it once per
// reviewId), but it blocks nothing:
//
// * Escape / the scrim / Close only put the window away — the review stays
//   pending and the banner remains to reopen it. They are never a Skip,
//   because Skip is a decision: the daemon drops the preserved copies;
// * **Skip** is the explicit "keep Studio's versions everywhere" button, and
//   the only thing that dismisses the review;
// * there is no "Keep Studio" side to stage, because Studio already won. The
//   header says so rather than offering a decision that has already been made.
//
// The staging interaction is Design 7.3's, reused wholesale from
// views/staging.js: the same two panes, the same search and sorts, the same
// bounded rendering. Only the vocabulary and the endpoints differ — the left
// pane is the disk files, the right is the keep-list, and the verbs say what
// keeping one actually does.
//
// Wire contract (`api.*`, implemented in app.js):
//   GET  /review                → {pending, reviewId, stats}
//   GET  /review/details        → one verified page of the pending set
//   POST /review/push           → {reviewId, ids} | {reviewId, mode:"all"}
//   POST /review/dismiss        → {reviewId}
//
// A push is **repeatable**: the daemon answers `{pushed, remaining}` and the
// ids stay valid, so a 3 000-item push is chunked and the answers are checked
// as they come back rather than trusted. A push also **needs Studio**: with no
// plugin connected the daemon answers 503, which is a wait — the modal parks
// in a retry state that fires on its own when the plugin connects — never a
// dead button.
//
// There is deliberately no inline diff here. The review contract has no
// per-row source route — `/choice/source` reads a frozen divergence set that a
// code-scope connect never creates — and a "View diff" button that could only
// fail is worse than none.

import { el, icon, plural } from "./dom.js";
import {
  filterEntries,
  MAX_LIVE_ROWS,
  moreRowsNote,
  normalizeEntry as normalizeStagingEntry,
  searchBox,
  sortEntries,
  sortOptionsFor,
  sortSelect as makeSortSelect,
  stagingPane,
  syncSortSelect,
} from "./staging.js";

/**
 * The review's two states, and what pushing one does.
 *
 * `disk-only` is a file with no instance in the place; `differs` is the disk
 * copy that was preserved when Studio's version landed on top of it.
 */
const KINDS = {
  "disk-only": {
    mark: "+",
    markClass: "mark-add",
    label: "Only on disk",
    verb: "Keep — create in Studio",
  },
  differs: {
    mark: "~",
    markClass: "mark-differs",
    label: "Differs from Studio",
    verb: "Keep — replace the Studio version",
  },
};

/** The window's one title, whatever phase it is in. */
const TITLE = "Initial Sync — Choose files to keep";
/** The user's whole decision, in two sentences. */
const SUBTITLE =
  "Studio is already synced to disk. Choose any disk files to keep — they'll be pushed to Studio. " +
  "Or skip and keep Studio's versions everywhere.";

/** One page of `GET /review/details`. The contract's ceiling is 1024. */
const PAGE_SIZE = 512;

/**
 * @param {object} api the shared view api from app.js
 * @param {{review?: object, onClosed?: () => void}} options
 * @returns {{close: () => void, getReviewId: () => string|null,
 *   supersede: (summary: object) => void, resolvedElsewhere: () => void}|null}
 */
export function openReviewModal(api, options = {}) {
  const modalRoot = document.getElementById("modal-root");
  if (modalRoot.firstChild) return null;

  const seed = options.review ?? null;
  const projectId = seed?.projectId ?? null;
  if (!projectId) return null;

  // ------------------------------------------------------------- state ---

  /** `loading` · `review` · `working` · `waiting-plugin` · `done` · `error`. */
  let phase = "loading";
  /** The 503-parked submission: `{label, run}`, replayable as-is. */
  let retry = null;
  let reviewId = seed?.reviewId ?? null;
  let stats = blankStats(seed);
  /** Entries loaded so far, in the daemon's order. */
  let entries = [];
  /** Where the next page starts, or null once the set is complete. */
  let cursor = 0;
  let totalCount = seed?.total ?? null;
  let listError = null;
  let loading = false;
  let progress = null;
  let outcome = null;
  let fatal = null;
  /** True from the first byte of our own push until it answers. */
  let submitting = false;
  let closed = false;

  let query = "";
  let sort = "set";
  let visibleRows = MAX_LIVE_ROWS;
  const staged = new Set();

  /**
   * The last-edited ledger for this project, re-read per render rather than
   * baked into the rows: the feed keeps arriving while the modal is open, and a
   * stamp that landed a second ago should still sort.
   */
  function editedStamps() {
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
    "aria-labelledby": "review-title",
    tabindex: "-1",
  });
  overlay.append(modal);

  // Escape and a scrim click close the window and nothing else. Unlike the
  // choice modal there is no decision to leave pending: the review stays
  // exactly as it was, and the banner is still there to reopen it.
  const popEscape = api.pushEscape(() => dismissWindow());
  overlay.addEventListener("mousedown", (event) => {
    if (event.target === overlay) dismissWindow();
  });

  modalRoot.append(overlay);
  modal.focus({ preventScroll: true });

  // A push parked on 503 replays itself the moment the plugin connects — the
  // whole point of the wait state is that connecting Studio is the fix.
  const unsubscribePlugin =
    api.onBus?.("plugin", (status) => {
      if (closed || phase !== "waiting-plugin" || !retry) return;
      if (status?.projectId !== projectId || status.connected !== true) return;
      void submit(retry.label, retry.run);
    }) ?? null;

  function close() {
    if (closed) return;
    closed = true;
    popEscape();
    unsubscribePlugin?.();
    overlay.remove();
    options.onClosed?.();
  }

  /** Closing the window. Never in the middle of a push. */
  function dismissWindow() {
    if (submitting) return;
    close();
  }

  // ------------------------------------------------------------ loading ---

  async function loadReview() {
    phase = "loading";
    render();
    try {
      const pending = await api.fetchPendingReview(projectId);
      if (closed) return;
      if (!pending.pending) {
        closeAsGone();
        return;
      }
      reviewId = pending.reviewId;
      stats = pending.stats;
      totalCount = pending.stats.total;
      entries = [];
      cursor = 0;
      staged.clear();
      phase = "review";
      render();
      await loadRest();
    } catch (error) {
      if (closed) return;
      fatal = messageOf(error);
      phase = "error";
      render();
    }
  }

  /**
   * Page the set in, rendering as it arrives — the same discipline the
   * divergence list uses, and for the same reason: the user must be able to
   * start picking before the last page lands.
   */
  async function loadRest() {
    if (loading || cursor === null) return;
    loading = true;
    listError = null;
    renderStage();

    while (cursor !== null && !closed) {
      let page;
      try {
        page = await api.fetchReviewDetails(projectId, {
          reviewId,
          cursor,
          limit: PAGE_SIZE,
          expectedTotal: totalCount,
        });
      } catch (error) {
        // Stop where it broke and keep what is already picked: the cursor still
        // points at the failed page, so `Retry list` resumes rather than
        // restarts.
        listError = messageOf(error);
        loading = false;
        renderStage();
        return;
      }
      if (closed) return;
      entries = entries.concat(page.items.map((item) => normalizeStagingEntry(item, KINDS, "differs")));
      cursor = page.nextCursor;
      totalCount = page.totalCount;
      renderStage();
      // Yield so a 49-page set stays a responsive list rather than a freeze.
      await new Promise((resolve) => setTimeout(resolve, 0));
    }

    loading = false;
    renderStage();
  }

  // ------------------------------------------------------------ actions ---

  /** What this window believes is still pending — the push receipt is checked
   * against it, so it is passed rather than left to the daemon to assert. */
  function knownRemaining() {
    return Number.isInteger(totalCount) ? totalCount : entries.length;
  }

  /** `POST /review/push {mode:"all"}` — keep everything still pending. */
  async function pushAll() {
    await submit("Keeping every disk file — pushing them into Studio…", async () => {
      const result = await api.pushReview(projectId, {
        reviewId,
        mode: "all",
        knownRemaining: knownRemaining(),
      });
      return { ...result, requested: totalCount ?? entries.length };
    });
  }

  /**
   * The selective push: the picked ids, chunked, each answer checked.
   *
   * A selection covering everything still pending upgrades to `mode:"all"` —
   * same result, one round trip, and no id list to get wrong.
   */
  async function pushStaged() {
    const ids = [...staged].sort((left, right) => left - right);
    if (ids.length === 0) return;
    if (totalCount !== null && ids.length === totalCount) {
      await pushAll();
      return;
    }

    await submit(`Keeping ${plural(ids.length, "file")} — pushing into Studio…`, async () => {
      const result = await api.pushReview(projectId, {
        reviewId,
        ids,
        knownRemaining: knownRemaining(),
        onProgress: ({ pushed, total, chunkIndex, chunkCount }) => {
          progress = {
            label:
              chunkCount > 1
                ? `Chunk ${chunkIndex + 1} of ${chunkCount} — ${pushed.toLocaleString()} of ${total.toLocaleString()} pushed`
                : `Pushed ${pushed.toLocaleString()} of ${total.toLocaleString()}`,
            value: pushed,
            max: total,
          };
          renderWorking();
        },
      });
      return { ...result, requested: ids.length };
    });
  }

  /**
   * `POST /review/dismiss` — Skip: keep Studio's versions everywhere. The one
   * explicit way this review is dismissed; the daemon drops the preserved disk
   * copies, so it is a button and never a side effect of closing the window.
   */
  async function skipReview() {
    await submit("Skipping — keeping Studio's versions everywhere…", async () => {
      const result = await api.dismissReview(projectId, { reviewId });
      return { ...result, dismissed: true };
    });
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
      retry = null;
      applyOutcome(result);
    } catch (error) {
      if (closed) return;
      if (error?.status === 503) {
        // No Studio plugin to receive the push. Retryable by definition —
        // nothing about the review changed — so the submission parks and
        // replays: on the Retry button, or by itself when the plugin connects.
        retry = { label, run };
        phase = "waiting-plugin";
        render();
        return;
      }
      phase = "error";
      fatal = messageOf(error);
      render();
    } finally {
      submitting = false;
    }
  }

  function applyOutcome(result) {
    if (result?.status === "gone") {
      closeAsGone();
      return;
    }

    if (result?.dismissed === true) {
      close();
      api.emitBus("review", null);
      api.setStatus("Skipped — Studio's versions are kept everywhere.", "");
      return;
    }

    const pushed = Number.isFinite(result?.pushed) ? result.pushed : (result?.requested ?? 0);
    const remaining = Number.isFinite(result?.remaining) ? result.remaining : 0;

    if (remaining > 0) {
      // A partial push leaves a live review behind, so the modal stays open on
      // the smaller list rather than claiming to be finished.
      api.emitBus("review", {
        projectId,
        reviewId,
        total: remaining,
        diskOnly: null,
        differs: null,
      });
      api.setStatus(
        `${plural(pushed, "disk file")} kept (pushed to Studio) — ${remaining.toLocaleString()} still to choose from.`,
        "ok",
      );
      void loadReview();
      return;
    }

    outcome = {
      tone: "ok",
      title: `${plural(pushed, "disk file")} kept — pushed to Studio`,
      body: "The daemon applied them inside one Studio undo, and wrote the disk versions back. Nothing is left to choose.",
    };
    phase = "done";
    api.emitBus("review", null);
    api.setStatus(`${plural(pushed, "disk file")} kept — pushed to Studio.`, "ok");
    render();
  }

  /** The review is no longer there: pushed out, dismissed, or replaced. */
  function closeAsGone() {
    close();
    api.emitBus("review", null);
    api.setStatus("That review is no longer pending.", "warn");
    api.toast("This review was replaced", {
      kind: "warn",
      body: "It was answered elsewhere, or a new connect rebuilt it. Nothing was pushed by this window.",
    });
  }

  // --------------------------------------------------------- rendering ---

  function render() {
    modal.replaceChildren(...frame());
  }

  function frame() {
    if (phase === "loading") return loadingStep();
    if (phase === "working") return workingStep();
    if (phase === "waiting-plugin") return waitingStep();
    if (phase === "done") return doneStep();
    if (phase === "error") return errorStep();
    return reviewStep();
  }

  function head(title, subtitle) {
    return el(
      "div",
      { class: "modal-head" },
      el(
        "div",
        {},
        el("h2", { class: "modal-title", id: "review-title", text: title }),
        el("p", { class: "modal-sub", text: subtitle }),
      ),
    );
  }

  function loadingStep() {
    return [
      head(TITLE, SUBTITLE),
      el("div", { class: "modal-body" }, el("div", { class: "stage-empty", text: "Loading the disk files…" })),
      el(
        "div",
        { class: "modal-foot" },
        el("div", { class: "modal-foot-spacer" }),
        el("button", { class: "btn", type: "button", on: { click: dismissWindow } }, "Close"),
      ),
    ];
  }

  /**
   * The 503 park: a push with no Studio plugin connected. Everything is kept —
   * the selection, the parked submission — and the retry is both a button and
   * automatic on the next `plugin-status: connected`.
   */
  function waitingStep() {
    return [
      head(TITLE, SUBTITLE),
      el(
        "div",
        { class: "modal-body" },
        el(
          "div",
          { class: "notice notice-warn" },
          icon("alert", 14),
          el(
            "div",
            { class: "notice-body" },
            el("span", {}, el("strong", { text: "Waiting for the Studio plugin to connect. " }),
              "Pushing files into Studio needs the place open with the WSync plugin running."),
            el("span", {
              class: "notice-meta",
              text: "Nothing was lost — your selection is kept, and the push retries by itself when the plugin connects.",
            }),
          ),
        ),
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
                phase = "review";
                render();
              },
            },
          },
          "Back to the list",
        ),
        el(
          "button",
          {
            class: "btn btn-primary",
            type: "button",
            on: { click: () => retry && void submit(retry.label, retry.run) },
          },
          "Retry now",
        ),
      ),
    ];
  }

  function workingStep() {
    const determinate =
      Number.isFinite(progress?.value) && Number.isFinite(progress?.max) && progress.max > 0;
    return [
      head(TITLE, "The daemon applies the kept files inside one Studio undo."),
      el(
        "div",
        { class: "modal-body" },
        el(
          "div",
          { class: "work-panel" },
          el("div", { class: "work-label", text: progress?.label ?? "Working…" }),
          el(
            "div",
            {
              class: "progress",
              role: "progressbar",
              ...(determinate
                ? {
                    "aria-valuenow": String(progress.value),
                    "aria-valuemin": "0",
                    "aria-valuemax": String(progress.max),
                  }
                : {}),
            },
            el("div", {
              class: `progress-bar${determinate ? "" : " progress-bar-indeterminate"}`,
              style: determinate ? `width:${Math.round((progress.value / progress.max) * 100)}%` : "",
            }),
          ),
          el("p", {
            class: "field-hint",
            text: "Every chunk is answered with what it pushed and what is left, and both are checked here before the next one is sent.",
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
      head("Nothing was pushed", "Studio and disk are as they were; the review is still pending."),
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
                void loadReview();
              },
            },
          },
          "Try again",
        ),
        el("button", { class: "btn btn-primary", type: "button", on: { click: close } }, "Close"),
      ),
    ];
  }

  // ------------------------------------------------------------ the list ---

  const panes = el("div", { class: "stage-panes" });
  const listNote = el("div", { class: "stage-note" });
  const footCount = el("span", { class: "modal-foot-note" });
  const pushButton = el(
    "button",
    { class: "btn btn-primary", type: "button", on: { click: () => void pushStaged() } },
    "Keep selected",
  );

  const { input: searchInput, box: searchWrap } = searchBox("Search disk files", (value) => {
    query = value;
    visibleRows = MAX_LIVE_ROWS;
    renderStage();
  });

  const sortSelect = makeSortSelect("Sort disk files", (value) => {
    sort = value;
    renderStage();
  });

  function reviewStep() {
    searchInput.value = query;
    renderStage();

    return [
      head(TITLE, SUBTITLE),
      el(
        "div",
        { class: "modal-body" },
        el("div", { class: "stage-toolbar" }, searchWrap, sortSelect, el("div", { class: "toolbar-spacer" })),
        panes,
        listNote,
      ),
      el(
        "div",
        { class: "modal-foot" },
        footCount,
        el("div", { class: "modal-foot-spacer" }),
        el(
          "button",
          {
            class: "btn",
            type: "button",
            title: "Keep Studio's versions everywhere — nothing from this list is pushed",
            on: { click: () => void skipReview() },
          },
          "Skip",
        ),
        el(
          "button",
          {
            class: "btn",
            type: "button",
            title: "Keep every disk file in this list — push them all to Studio",
            on: { click: () => void pushAll() },
          },
          "Keep all",
        ),
        pushButton,
      ),
    ];
  }

  /** Redraw only the parts of the list that change as pages and picks arrive. */
  function renderStage() {
    if (phase !== "review") return;
    const stamps = editedStamps();
    sort = syncSortSelect(sortSelect, sort, sortOptionsFor({ stamps, entries }));
    const remaining = sortEntries(
      filterEntries(entries.filter((entry) => !staged.has(entry.id)), query),
      { sort, kinds: KINDS, stamps, editedAt },
    );
    const picked = filterEntries(entries.filter((entry) => staged.has(entry.id)), query);

    const shared = { kinds: KINDS, stamps, editedAt, visibleRows };

    panes.replaceChildren(
      stagingPane({
        ...shared,
        title: "Disk files",
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
            loading || cursor !== null ? "Pick all loaded" : "Pick all",
          ),
        ],
        rows: remaining,
        emptyText: query ? `Nothing matches "${query}".` : "Everything is picked to keep.",
        onRow: (entry) => {
          staged.add(entry.id);
          renderStage();
        },
        action: "arrowRight",
      }),
      stagingPane({
        ...shared,
        title: "Keep — push to Studio",
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
            "Clear",
          ),
        ],
        rows: picked,
        emptyText: "Move over the disk files you want to keep. Everything left over stays as Studio has it.",
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
    footCount.textContent = `${staged.size.toLocaleString()} of ${known.toLocaleString()} picked to keep`;
    pushButton.textContent =
      staged.size === known && known > 0
        ? `Keep all ${known.toLocaleString()}`
        : `Keep selected (${staged.size.toLocaleString()})`;
    pushButton.disabled = staged.size === 0;
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
            el("span", {
              text: `The list stopped loading at ${entries.length.toLocaleString()} of ${known.toLocaleString()}: ${listError}`,
            }),
            el("span", {
              class: "notice-meta",
              text: "Pushing what has arrived is still safe — a push only ever names the ids you picked.",
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
          text: `Loading the review — ${entries.length.toLocaleString()} of ${known.toLocaleString()} items so far.`,
        }),
      );
    } else if (entries.length === 0) {
      children.push(
        el("p", { class: "field-hint", text: "Nothing is waiting for review." }),
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

  function renderWorking() {
    if (phase === "working") render();
  }

  // ------------------------------------------------------------- handle ---

  render();
  void loadReview();

  return {
    close,
    getReviewId: () => reviewId,

    /**
     * A `disk-review` for a different set landed — a new connect replaced this
     * one, so every id on screen addresses a review the daemon has dropped.
     */
    supersede(summary) {
      if (submitting || phase === "done") return;
      if (!reviewId) return;
      if (!summary?.reviewId || summary.reviewId === reviewId) return;
      close();
      api.setStatus("Studio reconnected — the disk review was rebuilt.", "warn");
      api.toast("This review was replaced", {
        kind: "warn",
        body: "A new connect recomputed it. Nothing was pushed; the current list opens by itself.",
      });
    },

    /** The review was answered somewhere else (the CLI, another window). */
    resolvedElsewhere() {
      if (submitting || phase === "done") return;
      closeAsGone();
    },
  };
}

// -------------------------------------------------------------- banner ----

/**
 * The small banner that remains while the initial-sync review is pending —
 * the reopen affordance for a popup the user backgrounded with Escape or the
 * scrim. Returns null when there is nothing to review, so a caller can
 * `replaceChildren(reviewBanner(...))` unconditionally.
 *
 * "Choose files" reopens the picker; "Skip" is the same explicit dismiss the
 * modal's Skip performs — keep Studio's versions everywhere.
 *
 * @param {object} api
 * @param {object|null} review the summary from the `review` bus event
 * @param {{style?: string}} [options]
 */
export function reviewBanner(api, review, options = {}) {
  const total = Number.isFinite(review?.total) ? review.total : 0;
  if (!review || total <= 0) return null;

  const skip = el(
    "button",
    {
      class: "btn btn-sm",
      type: "button",
      style: "margin-left:auto",
      title: "Keep Studio's versions everywhere",
      on: {
        click: () => {
          skip.disabled = true;
          void Promise.resolve(api.dismissPendingReview()).finally(() => {
            skip.disabled = false;
          });
        },
      },
    },
    "Skip",
  );

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
        text: `Initial sync: ${total.toLocaleString()} disk ${total === 1 ? "file" : "files"} to choose from. `,
      }),
      "Studio is synced to disk — keep any of these, or skip and keep Studio's versions.",
    ),
    skip,
    el(
      "button",
      {
        class: "btn btn-sm btn-primary",
        type: "button",
        on: { click: () => api.openReviewModal({ review }) },
      },
      "Choose files",
    ),
  );
}

// ------------------------------------------------------------- helpers ----

function blankStats(seed) {
  const count = (value) => (Number.isFinite(value) ? Number(value) : 0);
  return {
    total: count(seed?.total),
    diskOnly: count(seed?.diskOnly),
    differs: count(seed?.differs),
  };
}

function messageOf(error) {
  return error?.message ?? String(error);
}
