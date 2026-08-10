// views/dom.js — the small element helper every view builds its DOM with.
//
// Deliberately tiny and deliberately not a framework: no diffing, no
// templates, no reactivity. Views re-render the region that changed, which at
// this data size is both the simplest and the fastest thing that works.
//
// It also means no view ever assigns `innerHTML`, so untrusted text (paths,
// project names, daemon messages) can only ever land as text nodes.

/**
 * @param {string} tag
 * @param {object} [props] `class`, `text`, `title`, `data`, `on` (event map),
 *   anything else becomes an attribute. Nullish attribute values are skipped.
 * @param {...(Node|string|null|undefined|Array)} children
 */
export function el(tag, props = {}, ...children) {
  const node = document.createElement(tag);
  for (const [key, value] of Object.entries(props ?? {})) {
    if (value === null || value === undefined || value === false) continue;
    if (key === "class") node.className = value;
    else if (key === "text") node.textContent = value;
    else if (key === "data") Object.assign(node.dataset, value);
    else if (key === "on") {
      for (const [event, handler] of Object.entries(value)) node.addEventListener(event, handler);
    } else if (key === "value") node.value = value;
    else if (key === "checked" || key === "disabled" || key === "hidden") node[key] = Boolean(value);
    else node.setAttribute(key, value === true ? "" : String(value));
  }
  append(node, children);
  return node;
}

export function append(parent, children) {
  for (const child of children.flat(Infinity)) {
    if (child === null || child === undefined || child === false) continue;
    parent.append(child instanceof Node ? child : document.createTextNode(String(child)));
  }
  return parent;
}

const ICON_PATHS = {
  search: ["M7.2 2.4a4.8 4.8 0 1 0 0 9.6 4.8 4.8 0 0 0 0-9.6z", "M10.7 10.7 14 14"],
  plus: ["M8 3.2v9.6M3.2 8h9.6"],
  folder: ["M1.9 4.2a1 1 0 0 1 1-1h3.3l1.5 1.5h6.4a1 1 0 0 1 1 1v6.1a1 1 0 0 1-1 1H2.9a1 1 0 0 1-1-1v-7.6z"],
  trash: ["M2.9 4.4h10.2", "M6.2 4.4V3.1a.9.9 0 0 1 .9-.9h1.8a.9.9 0 0 1 .9.9v1.3", "M4.2 4.4l.6 8.2a1 1 0 0 0 1 .9h4.4a1 1 0 0 0 1-.9l.6-8.2"],
  alert: ["M8 2 1.9 13h12.2L8 2z", "M8 6.4v3M8 11.2v.5"],
  book: ["M4.25 2.4h5.4l2.1 2.1v9.1h-7.5a1 1 0 0 1-1-1V3.4a1 1 0 0 1 1-1z", "M9.65 2.4v2.1h2.1"],
  refresh: ["M13.2 8a5.2 5.2 0 1 1-1.6-3.7", "M13.4 2.6v2.9h-2.9"],
  check: ["M3.4 8.4l3 3 6.2-6.9"],
  activity: ["M1.6 8.5h2.8l1.5-4.2 3 8.4 1.6-4.2h4"],
  arrowRight: ["M3.2 8h9.6", "M9.2 4.4 12.8 8l-3.6 3.6"],
  arrowLeft: ["M12.8 8H3.2", "M6.8 4.4 3.2 8l3.6 3.6"],
  studio: ["M3.6 4.1 8 2.2l4.4 1.9v5.8L8 13.8 3.6 9.9V4.1z", "M6.4 6.6h3.2v3.2H6.4z"],
  disk: ["M2.4 4.5c0-1.05 2.5-1.9 5.6-1.9s5.6.85 5.6 1.9-2.5 1.9-5.6 1.9-5.6-.85-5.6-1.9z", "M2.4 4.5v7c0 1.05 2.5 1.9 5.6 1.9s5.6-.85 5.6-1.9v-7"],
  settings: ["M8 5.85A2.15 2.15 0 1 0 8 10.15 2.15 2.15 0 0 0 8 5.85z", "M8 1.3v1.75M8 12.95v1.75M14.7 8h-1.75M3.05 8H1.3M12.74 3.26l-1.24 1.24M4.5 11.5l-1.24 1.24M12.74 12.74 11.5 11.5M4.5 4.5 3.26 3.26"],
};

/** Inline stroke icon. `size` in px; inherits `currentColor`. */
export function icon(name, size = 14) {
  const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  svg.setAttribute("viewBox", "0 0 16 16");
  svg.setAttribute("width", String(size));
  svg.setAttribute("height", String(size));
  svg.setAttribute("fill", "none");
  svg.setAttribute("stroke", "currentColor");
  svg.setAttribute("stroke-width", "1.4");
  svg.setAttribute("stroke-linecap", "round");
  svg.setAttribute("stroke-linejoin", "round");
  svg.setAttribute("aria-hidden", "true");
  for (const definition of ICON_PATHS[name] ?? []) {
    const path = document.createElementNS("http://www.w3.org/2000/svg", "path");
    path.setAttribute("d", definition);
    svg.append(path);
  }
  return svg;
}

/** Up to two initials for a project thumb. */
export function initials(name) {
  const words = String(name ?? "")
    .split(/[\s._/-]+/u)
    .filter(Boolean);
  if (words.length === 0) return "?";
  if (words.length === 1) return words[0].slice(0, 2).toUpperCase();
  return (words[0][0] + words[1][0]).toUpperCase();
}

export function plural(count, singular, pluralForm = `${singular}s`) {
  return `${count} ${count === 1 ? singular : pluralForm}`;
}

/**
 * Compact relative time for feed, registry and last-edited timestamps.
 *
 * Takes an ISO string (what the daemon's records carry) or epoch milliseconds
 * (what the last-edited ledger stores), because both exist and neither is worth
 * converting at every call site.
 */
export function relativeTime(iso) {
  const then = typeof iso === "number" ? iso : Date.parse(iso);
  if (!Number.isFinite(then)) return "—";
  const seconds = Math.round((Date.now() - then) / 1000);
  if (seconds < 60) return "just now";
  if (seconds < 3600) return `${Math.floor(seconds / 60)}m ago`;
  if (seconds < 86400) return `${Math.floor(seconds / 3600)}h ago`;
  if (seconds < 2592000) return `${Math.floor(seconds / 86400)}d ago`;
  return new Date(then).toLocaleDateString();
}

// A `pendingPanel("…", "…", "wave 2")` helper lived here while the Activity and
// Conflicts views had nothing but stub states to show. Both now render real
// data, or an honest reason there is none, so the helper had no callers left.
