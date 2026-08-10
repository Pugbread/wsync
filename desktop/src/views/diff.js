// views/diff.js — the shared differ (Design 7.3, 7.5, 8.2).
//
// Design calls for a *vendored* diff library rather than a CDN one, which
// Ro-Sync got wrong. Vendoring a package into a build-step-free frontend means
// carrying someone else's bundle in the repository forever, so this is the
// other half of that instruction: a line differ small enough to read in one
// sitting, with no dependency and no network at all.
//
// Two shapes, because the conflict engine has two kinds of conflict (§8.2):
//
//   alignedLines()  script conflicts — a two-pane text diff, rows aligned so
//                   the same line number sits opposite its counterpart.
//   propertyRows()  property conflicts — name | fs value | studio value, with
//                   Roblox tagged values rendered compactly.
//
// Both are pure data; `diffPanes()` and `propertyTable()` render them. The
// divergence modal and the Conflicts view share all four, which is what keeps
// "differs" looking the same wherever the user meets it.

import { el } from "./dom.js";

/** Above this the aligner stops trying: see `alignedLines`. */
const MAX_ALIGN_CELLS = 1_500_000;

/** Rows past this are not rendered; the pane says how many were dropped. */
export const MAX_DIFF_ROWS = 600;

function splitLines(source) {
  if (typeof source !== "string" || source === "") return [];
  return source.replace(/\r\n?/gu, "\n").split("\n");
}

/**
 * A longest-common-subsequence line diff, aligned into two-pane rows.
 *
 * Returns `{rows, aligned, changed}`, where each row is
 * `{left, right, leftNo, rightNo, kind}` and `kind` is `same` · `change` ·
 * `del` · `add`. A `null` side is a gap opposite an insertion or a deletion.
 *
 * The common prefix and suffix are trimmed first, which is what makes this
 * cheap on the realistic case — two versions of the same file differing in a
 * few lines. Only the middle goes through the O(n·m) table, and if that middle
 * is still too big (`MAX_ALIGN_CELLS`) the aligner gives up honestly:
 * `aligned: false`, and the differing region is reported as one deleted block
 * against one added block rather than as a wrong alignment nobody can check.
 */
export function alignedLines(leftSource, rightSource) {
  const left = splitLines(leftSource);
  const right = splitLines(rightSource);

  let head = 0;
  while (head < left.length && head < right.length && left[head] === right[head]) head += 1;

  let tail = 0;
  while (
    tail < left.length - head &&
    tail < right.length - head &&
    left[left.length - 1 - tail] === right[right.length - 1 - tail]
  ) {
    tail += 1;
  }

  const leftMiddle = left.slice(head, left.length - tail);
  const rightMiddle = right.slice(head, right.length - tail);

  const rows = [];
  let leftNo = 1;
  let rightNo = 1;
  const same = (line) => {
    rows.push({ kind: "same", left: line, right: line, leftNo: leftNo++, rightNo: rightNo++ });
  };

  for (let index = 0; index < head; index += 1) same(left[index]);

  let aligned = true;
  if (leftMiddle.length * rightMiddle.length > MAX_ALIGN_CELLS) {
    aligned = false;
    pairBlocks(rows, leftMiddle, rightMiddle, () => leftNo++, () => rightNo++);
  } else {
    let pendingLeft = [];
    let pendingRight = [];
    const flush = () => {
      if (pendingLeft.length === 0 && pendingRight.length === 0) return;
      pairBlocks(rows, pendingLeft, pendingRight, () => leftNo++, () => rightNo++);
      pendingLeft = [];
      pendingRight = [];
    };

    for (const op of lcsOps(leftMiddle, rightMiddle)) {
      if (op.kind === "same") {
        flush();
        same(op.line);
      } else if (op.kind === "del") {
        pendingLeft.push(op.line);
      } else {
        pendingRight.push(op.line);
      }
    }
    flush();
  }

  for (let index = left.length - tail; index < left.length; index += 1) same(left[index]);

  return { rows, aligned, changed: rows.some((row) => row.kind !== "same") };
}

/**
 * Pair a run of removed lines against a run of added ones.
 *
 * Overlapping lines become a single `change` row — the shape a two-pane diff
 * wants, where an edited line sits opposite the line it replaced — and the
 * remainder trails as one-sided rows.
 */
function pairBlocks(rows, removed, added, nextLeftNo, nextRightNo) {
  const paired = Math.min(removed.length, added.length);
  for (let index = 0; index < paired; index += 1) {
    rows.push({
      kind: "change",
      left: removed[index],
      right: added[index],
      leftNo: nextLeftNo(),
      rightNo: nextRightNo(),
    });
  }
  for (let index = paired; index < removed.length; index += 1) {
    rows.push({ kind: "del", left: removed[index], right: null, leftNo: nextLeftNo(), rightNo: null });
  }
  for (let index = paired; index < added.length; index += 1) {
    rows.push({ kind: "add", left: null, right: added[index], leftNo: null, rightNo: nextRightNo() });
  }
}

/**
 * The classic LCS table, backtracked into an op list.
 *
 * `Uint32Array` rather than nested arrays: one flat allocation, and the guard
 * in `alignedLines` has already bounded it. Lines are interned to integers
 * first so the inner loop compares numbers, not strings.
 */
function lcsOps(left, right) {
  const dictionary = new Map();
  const intern = (line) => {
    let id = dictionary.get(line);
    if (id === undefined) {
      id = dictionary.size;
      dictionary.set(line, id);
    }
    return id;
  };
  const a = left.map(intern);
  const b = right.map(intern);
  const width = b.length + 1;
  const table = new Uint32Array((a.length + 1) * width);

  for (let i = a.length - 1; i >= 0; i -= 1) {
    for (let j = b.length - 1; j >= 0; j -= 1) {
      table[i * width + j] =
        a[i] === b[j]
          ? table[(i + 1) * width + j + 1] + 1
          : Math.max(table[(i + 1) * width + j], table[i * width + j + 1]);
    }
  }

  const ops = [];
  let i = 0;
  let j = 0;
  while (i < a.length && j < b.length) {
    if (a[i] === b[j]) {
      ops.push({ kind: "same", line: left[i] });
      i += 1;
      j += 1;
    } else if (table[(i + 1) * width + j] >= table[i * width + j + 1]) {
      ops.push({ kind: "del", line: left[i] });
      i += 1;
    } else {
      ops.push({ kind: "add", line: right[j] });
      j += 1;
    }
  }
  while (i < a.length) ops.push({ kind: "del", line: left[i++] });
  while (j < b.length) ops.push({ kind: "add", line: right[j++] });
  return ops;
}

// -------------------------------------------------------------- rendering --

const SIDE_KINDS = {
  left: { change: "diff-del", del: "diff-del", add: "diff-gap", same: "" },
  right: { change: "diff-add", del: "diff-gap", add: "diff-add", same: "" },
};

/**
 * A two-pane text diff.
 *
 * @param {{title: string, source: string|null, present?: boolean,
 *   truncated?: boolean, note?: string}} left
 * @param {object} right same shape
 */
export function diffPanes(left, right) {
  const both = left.present !== false && right.present !== false;
  const diff = both ? alignedLines(left.source ?? "", right.source ?? "") : null;

  return el(
    "div",
    { class: "diff-panes" },
    textPane("left", left, diff),
    textPane("right", right, diff),
    diff && !diff.aligned
      ? el("p", {
          class: "diff-note",
          text: "These files are too large to align line by line; the changed region is shown as a block.",
        })
      : null,
    diff && diff.rows.length > MAX_DIFF_ROWS
      ? el("p", {
          class: "diff-note",
          text: `Showing the first ${MAX_DIFF_ROWS} of ${diff.rows.length} lines.`,
        })
      : null,
  );
}

function textPane(side, pane, diff) {
  const head = el(
    "div",
    { class: "diff-pane-head" },
    el("span", { text: pane.title }),
    pane.truncated
      ? el("span", {
          class: "chip chip-warn",
          text: "truncated",
          title: "The daemon sends at most 256 KiB of a script's source; the rest is not shown.",
        })
      : null,
  );

  // A side that is not there is not an empty file: "deleted" and "empty" mean
  // opposite things when deciding which side to keep.
  if (pane.present === false) {
    return el(
      "div",
      { class: "diff-pane" },
      head,
      el("div", {
        class: "diff-pane-body diff-pane-body-empty",
        text: pane.note ?? "Deleted on this side.",
      }),
    );
  }
  if (!diff) {
    const source = typeof pane.source === "string" ? pane.source : "";
    return el(
      "div",
      { class: "diff-pane" },
      head,
      el("div", {
        class: `diff-pane-body${source ? "" : " diff-pane-body-empty"}`,
        text: source || "The daemon did not send this side's contents.",
      }),
    );
  }

  const body = el("div", { class: "diff-pane-body" });
  for (const row of diff.rows.slice(0, MAX_DIFF_ROWS)) {
    const text = side === "left" ? row.left : row.right;
    const number = side === "left" ? row.leftNo : row.rightNo;
    const className = SIDE_KINDS[side][row.kind];
    body.append(
      el(
        "span",
        { class: `diff-line${className ? ` ${className}` : ""}` },
        el("span", { class: "diff-lineno", text: number === null ? "" : String(number) }),
        el("span", { class: "diff-linetext", text: text === null ? "" : text || " " }),
      ),
    );
  }
  return el("div", { class: "diff-pane" }, head, body);
}

// ------------------------------------------------------- property diffing --

/**
 * Union the two tagged-value maps into comparable rows.
 *
 * Returns `{name, fs, studio, state}` per property, changed ones first so the
 * decision is at the top of the table. `state` is `changed` · `fs-only` ·
 * `studio-only` · `same`; values are already formatted for display.
 */
export function propertyRows(fsProps, studioProps) {
  const fs = fsProps && typeof fsProps === "object" ? fsProps : {};
  const studio = studioProps && typeof studioProps === "object" ? studioProps : {};
  const names = [...new Set([...Object.keys(fs), ...Object.keys(studio)])].sort((a, b) =>
    a.localeCompare(b),
  );

  const rows = names.map((name) => {
    const inFs = Object.hasOwn(fs, name);
    const inStudio = Object.hasOwn(studio, name);
    const left = inFs ? formatTaggedValue(fs[name]) : null;
    const right = inStudio ? formatTaggedValue(studio[name]) : null;
    let state = "same";
    if (inFs && !inStudio) state = "fs-only";
    else if (!inFs && inStudio) state = "studio-only";
    else if (left !== right) state = "changed";
    return { name, fs: left, studio: right, state };
  });

  const rank = { changed: 0, "fs-only": 1, "studio-only": 1, same: 2 };
  return rows.sort((a, b) => rank[a.state] - rank[b.state] || a.name.localeCompare(b.name));
}

/** Long enough to be useful in a table cell, short enough to stay one. */
const MAX_VALUE_CHARS = 96;

/**
 * Render one Roblox tagged value compactly.
 *
 * The wire form is a single-key map naming the type — `{"UDim2": [[0,320],
 * [0,180]]}`, `{"Bool": true}` — so the type is worth keeping (a `0` that is a
 * `Float32` is not the same fact as a `0` that is an `Enum`), while the payload
 * collapses to something that fits in a cell. Anything that is not in that
 * shape is rendered as compact JSON rather than guessed at.
 */
export function formatTaggedValue(value) {
  if (value === null || value === undefined) return "—";
  if (typeof value !== "object" || Array.isArray(value)) return clip(scalar(value));

  const keys = Object.keys(value);
  if (keys.length !== 1) return clip(scalar(value));

  const [type] = keys;
  const inner = value[type];
  if (typeof inner === "string") return clip(`${type} ${JSON.stringify(inner)}`);
  if (typeof inner === "boolean" || typeof inner === "number") return clip(`${type} ${scalar(inner)}`);
  return clip(`${type}(${scalar(inner)})`);
}

function scalar(value) {
  if (value === null || value === undefined) return "—";
  if (typeof value === "number") return formatNumber(value);
  if (typeof value === "boolean") return String(value);
  if (typeof value === "string") return JSON.stringify(value);
  if (Array.isArray(value)) {
    // Nested groups keep their shape — a UDim2's two UDims read as two pairs.
    return value
      .map((entry) => (Array.isArray(entry) ? `(${scalar(entry)})` : scalar(entry)))
      .join(", ");
  }
  return Object.entries(value)
    .map(([key, entry]) => `${key}: ${scalar(entry)}`)
    .join(", ");
}

function formatNumber(value) {
  if (!Number.isFinite(value)) return String(value);
  if (Number.isInteger(value)) return String(value);
  return String(Math.round(value * 1e4) / 1e4);
}

function clip(text) {
  return text.length > MAX_VALUE_CHARS ? `${text.slice(0, MAX_VALUE_CHARS - 1)}…` : text;
}

/**
 * The property-table diff: name | on disk | in Studio.
 *
 * @param {object|null} fsProps
 * @param {object|null} studioProps
 * @param {{fsPresent?: boolean, studioPresent?: boolean}} [presence]
 */
export function propertyTable(fsProps, studioProps, presence = {}) {
  const rows = propertyRows(fsProps, studioProps);
  if (rows.length === 0) {
    return el("div", {
      class: "diff-pane-body diff-pane-body-empty",
      text: "The daemon did not send any properties for this conflict.",
    });
  }

  const absent = (present) => (present === false ? "deleted" : "—");
  const table = el(
    "table",
    { class: "prop-table" },
    el(
      "thead",
      {},
      el(
        "tr",
        {},
        el("th", { text: "Property" }),
        el("th", { text: "On disk" }),
        el("th", { text: "In Studio" }),
      ),
    ),
    el(
      "tbody",
      {},
      ...rows.map((row) =>
        el(
          "tr",
          { class: `prop-row prop-${row.state}` },
          el("td", { class: "prop-name", text: row.name }),
          el("td", {
            class: `prop-value${row.state === "changed" || row.state === "fs-only" ? " prop-value-fs" : ""}`,
            text: row.fs ?? absent(presence.fsPresent),
          }),
          el("td", {
            class: `prop-value${row.state === "changed" || row.state === "studio-only" ? " prop-value-studio" : ""}`,
            text: row.studio ?? absent(presence.studioPresent),
          }),
        ),
      ),
    ),
  );

  return el("div", { class: "prop-table-wrap" }, table);
}
