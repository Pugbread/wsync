// views/staging.js — the two-pane staging list, shared by both review surfaces.
//
// Design 7.3 built it for the divergence modal ("Stage disk changes"); Design
// 7.0's disk review is the same interaction against a different set — a list of
// disk items on the left, a queue of things to push into Studio on the right,
// with search, sorts and bounded rendering. So it lives here and both callers
// drive it, rather than existing twice and drifting apart.
//
// Everything exported is a *renderer*: it takes the rows and the current view
// state and hands back DOM. It owns no state, fetches nothing, and decides
// nothing — which side of the panes a row is on, what a click does and what the
// verbs are called all stay with the caller, because those are the parts the
// two surfaces genuinely disagree about.
//
// A `kinds` map is the seam. It maps a contract `state` string to the chip and
// the words shown for it:
//
//   { mark, markClass, label, verb }
//
// The choice flow passes `only-on-disk` / `differs` / `missing-on-disk`; the
// disk review passes `disk-only` / `differs` with its own verbs.

import { el, icon, plural, relativeTime } from "./dom.js";

/** Design 7.3: at most 300 rows live; beyond that the list pages. */
export const MAX_LIVE_ROWS = 300;

/**
 * One row shape, whatever the surface.
 *
 * Both contracts allow one of the two paths to be absent — a Studio-only
 * divergence row has no predicted file path, a disk-only review row has no
 * instance in Studio yet — so the label falls back to whichever exists and the
 * row says why rather than rendering "null".
 *
 * @param {object} entry the verified item from the daemon
 * @param {Record<string, {mark: string, markClass: string, label: string, verb: string}>} kinds
 * @param {string} fallbackKind the kind to use when `state` is not in `kinds`
 */
export function normalizeEntry(entry, kinds, fallbackKind) {
  const path = typeof entry.path === "string" && entry.path !== "" ? entry.path : null;
  const instancePath =
    typeof entry.instancePath === "string" && entry.instancePath !== "" ? entry.instancePath : null;
  const label = path ?? instancePath ?? String(entry.id);
  return {
    id: entry.id,
    kind: kinds[entry.state] ? entry.state : fallbackKind,
    label,
    // Kept alongside `label` because the last-edited ledger is keyed by file
    // path: a row with no path can never have a stamp, and must not
    // accidentally match one under its instance path.
    path,
    instancePath,
    title: path && instancePath ? `${path}\n${instancePath}` : label,
    /** True when the daemon could not name a file for this row. */
    pathless: path === null,
    editedAt: entry.editedAt ?? null,
    search: `${label} ${instancePath ?? ""}`.toLowerCase(),
  };
}

export function filterEntries(list, query) {
  return query ? list.filter((entry) => entry.search.includes(query)) : list;
}

/**
 * Design 7.3's sorts. "Recently edited" puts what the ledger has seen at the
 * top, newest first; a row the ledger has never seen sorts **last** rather than
 * as "edited at the epoch", and ties break by path so the order is stable
 * between renders (the same set must not shuffle when a stamp lands).
 */
export function sortEntries(list, { sort, kinds, stamps, editedAt }) {
  if (sort === "set") return list;
  const order = Object.keys(kinds);
  return [...list].sort((left, right) => {
    if (sort === "az") return left.label.localeCompare(right.label);
    if (sort === "kind") {
      const delta = order.indexOf(left.kind) - order.indexOf(right.kind);
      return delta !== 0 ? delta : left.label.localeCompare(right.label);
    }
    const leftAt = editedAt(left, stamps);
    const rightAt = editedAt(right, stamps);
    if (leftAt === rightAt) return left.label.localeCompare(right.label);
    if (leftAt === null) return 1;
    if (rightAt === null) return -1;
    return rightAt - leftAt;
  });
}

/**
 * The sort list, plus "Recently edited" whenever the ledger has anything to
 * sort this project by.
 *
 * The option is data-driven rather than always-on because it is a claim: an
 * empty ledger offering "Recently edited" would silently mean "set order with
 * extra steps", and the user would have no way to tell.
 */
export function sortOptionsFor({ stamps, entries }) {
  const options = [
    { id: "set", label: "Set order" },
    { id: "az", label: "A→Z" },
    { id: "kind", label: "Change type" },
  ];
  if (stamps.size > 0 || entries.some((entry) => Number.isFinite(entry.editedAt))) {
    options.unshift({ id: "recent", label: "Recently edited" });
  }
  return options;
}

/**
 * Fill a `<select>` with the sorts currently on offer and return the one that
 * survived. A sort the list can no longer offer must not stay selected, or the
 * select would show a blank and the rows would be in an order with no name.
 */
export function syncSortSelect(select, sort, options) {
  const effective = options.some((option) => option.id === sort) ? sort : "set";
  select.replaceChildren(
    ...options.map((option) =>
      el("option", { value: option.id, selected: effective === option.id ? "" : null, text: option.label }),
    ),
  );
  select.value = effective;
  return effective;
}

/** The search box both surfaces put above the panes. Built once by the caller. */
export function searchBox(label, onQuery) {
  const input = el("input", {
    class: "input",
    type: "search",
    placeholder: "Search paths…",
    "aria-label": label,
    on: { input: (event) => onQuery(event.target.value.trim().toLowerCase()) },
  });
  const box = el("div", { class: "search" }, el("span", { class: "search-icon" }, icon("search", 13)), input);
  return { input, box };
}

export function sortSelect(label, onSort) {
  return el("select", {
    class: "select",
    style: "width:auto",
    "aria-label": label,
    on: { change: (event) => onSort(event.target.value) },
  });
}

/**
 * One pane of the two.
 *
 * @param {object} config
 * @param {string} config.title
 * @param {number} config.count total rows behind this pane (before row capping)
 * @param {Node[]} config.tools buttons for the pane header
 * @param {object[]} config.rows normalized entries, already filtered and sorted
 * @param {string} config.emptyText
 * @param {(entry: object) => void} config.onRow what clicking a row does
 * @param {"arrowRight"|"arrowLeft"} config.action
 * @param {boolean} [config.showVerb] label each row with its concrete verb
 * @param {number} config.visibleRows
 * @param {number|null} config.expandedId the row whose diff is open, if any
 * @param {(entry: object) => Node|null} [config.diffPanelFor] omit entirely on
 *   a surface with no per-row diff contract — no endpoint, no toggle.
 * @param {(entry: object) => boolean} [config.diffable] which rows may open one
 * @param {(entry: object) => void} [config.onToggleDiff]
 */
export function stagingPane(config) {
  const {
    title,
    count,
    tools,
    rows,
    emptyText,
    onRow,
    action,
    showVerb = false,
    kinds,
    stamps,
    editedAt,
    visibleRows,
    expandedId = null,
    diffPanelFor = null,
    diffable = () => false,
    onToggleDiff = () => {},
  } = config;

  const visible = rows.slice(0, visibleRows);
  const drawn = visible.flatMap((entry) => {
    const kind = kinds[entry.kind];
    const edited = editedAt(entry, stamps);
    const open = expandedId === entry.id;
    const panelId = `stage-diff-${entry.id}`;

    // The move button and the diff toggle are siblings, not nested: a button
    // inside a button is invalid, and — more to the point — the two are
    // genuinely different actions. Clicking the row stages; the toggle only
    // ever reads.
    const move = el(
      "button",
      {
        class: "stage-row-move",
        type: "button",
        title: showVerb ? `Remove from the queue — ${entry.label}` : `${kind.verb} — ${entry.label}`,
        on: { click: () => onRow(entry) },
      },
      el("span", { class: `mark ${kind.markClass}`, text: kind.mark, "aria-hidden": "true" }),
      el(
        "span",
        { class: "stage-row-main" },
        el("span", { class: "stage-row-path", text: entry.label, title: entry.title }),
        rowMeta(entry, kind, edited, showVerb),
      ),
      el("span", { class: "stage-row-action" }, icon(action, 13)),
    );

    const row = el(
      "div",
      { class: `stage-row${open ? " stage-row-open" : ""}` },
      move,
      diffPanelFor && diffable(entry)
        ? el(
            "button",
            {
              class: "stage-row-diff",
              type: "button",
              "aria-expanded": open ? "true" : "false",
              "aria-controls": panelId,
              title: open ? "Hide the diff" : "View the diff for this path",
              data: { diffFor: String(entry.id) },
              on: { click: () => onToggleDiff(entry) },
            },
            open ? "Hide diff" : "View diff",
          )
        : null,
    );

    return open && diffPanelFor ? [row, diffPanelFor(entry, panelId)] : [row];
  });

  return el(
    "div",
    { class: "stage-pane" },
    el(
      "div",
      { class: "stage-pane-head" },
      el("span", { class: "stage-pane-title", text: title }),
      el("span", { class: "stage-pane-count", text: plural(count, "path") }),
    ),
    el("div", { class: "stage-pane-tools" }, ...tools),
    el(
      "div",
      {
        // A list holding an open diff gets more height to hold it in: a
        // two-pane diff inside a 336px scroller is a keyhole, and the modal
        // body scrolls anyway. Keyed off the rows actually drawn — a row
        // filtered or paged out of view has no panel to make room for.
        class: `stage-list${visible.some((entry) => entry.id === expandedId) ? " stage-list-expanded" : ""}`,
      },
      drawn.length ? drawn : el("div", { class: "stage-empty", text: emptyText }),
    ),
  );
}

/** The small second line under a row's path: verb, edit stamp, caveats. */
function rowMeta(entry, kind, edited, showVerb) {
  const parts = [];
  if (showVerb) parts.push(el("span", { class: "stage-row-verb", text: kind.verb }));
  if (edited !== null) {
    parts.push(
      el("span", {
        class: "stage-row-edited",
        text: `edited ${relativeTime(edited)}`,
        title: new Date(edited).toLocaleString(),
      }),
    );
  }
  if (entry.pathless) {
    parts.push(el("span", { class: "stage-row-verb", text: "no predicted file path" }));
  }
  return parts.length ? el("span", { class: "stage-row-meta" }, ...parts) : null;
}

/**
 * Scroll an expanded panel into view **within its own list**.
 *
 * Not `scrollIntoView`: that walks every scrollable ancestor, and the modal
 * body is one of them — opening a diff would scroll the toolbar and the pane
 * headers off the screen. This moves one container, by the smallest amount that
 * works, and keeps the row's own line visible in preference to the last line of
 * its diff.
 */
export function revealInList(panel) {
  const list = panel?.closest(".stage-list");
  const row = panel?.previousElementSibling;
  if (!list || !row) return;
  const top = list.getBoundingClientRect().top;
  const rowOffset = row.getBoundingClientRect().top - top;
  const panelBottom = panel.getBoundingClientRect().bottom - top;

  if (panelBottom > list.clientHeight) {
    list.scrollTop += Math.min(rowOffset, panelBottom - list.clientHeight);
  } else if (rowOffset < 0) {
    list.scrollTop += rowOffset;
  }
}

/**
 * The "showing N of M rows" footer with its own paging button, shared because
 * both lists are bounded the same way.
 */
export function moreRowsNote(shown, visibleRows, onMore) {
  if (shown <= visibleRows) return null;
  return el(
    "div",
    { class: "stage-more" },
    el("span", {
      class: "field-hint",
      text: `Showing ${visibleRows.toLocaleString()} of ${shown.toLocaleString()} rows.`,
    }),
    el(
      "button",
      { class: "btn btn-sm", type: "button", on: { click: onMore } },
      `Show ${Math.min(MAX_LIVE_ROWS, shown - visibleRows).toLocaleString()} more`,
    ),
  );
}
