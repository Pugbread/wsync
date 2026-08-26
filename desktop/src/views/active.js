// views/active.js — the live activity feed (Design 8.2).
//
// Frames arrive from the daemon WebSocket (app.js → bus `activity`) and are
// rendered through a sanitizing allowlist: a card shows category, tone, title,
// intent, a bounded fact list and a duration, plus the raw frame behind a
// collapsible. Nothing else from the frame reaches the DOM, and every value is
// coerced to text and length-capped — a chatty or hostile engine cannot grow a
// card without bound.
//
// Writes are batched on requestAnimationFrame: a burst of sync ops repaints
// once, not once per op.

import { el, icon } from "./dom.js";

/** Design 8.2: 200-card ring buffer. */
const FEED_CAPACITY = 200;

/** Facts per card, and characters per fact. Cards stay readable and bounded. */
const MAX_FACTS = 6;
const MAX_FACT_CHARS = 160;
const MAX_TITLE_CHARS = 200;

/** The tones a frame may ask for. Anything else renders neutral. */
const TONES = {
  ok: "feed-tone-ok",
  warn: "feed-tone-warn",
  err: "feed-tone-err",
  danger: "feed-tone-err",
  info: "feed-tone-info",
  accent: "feed-tone-accent",
};

/** The categories a frame may claim; unknown ones fall back to its topic. */
const CATEGORIES = new Set(["sync", "syncback", "conflict", "plugin", "project", "config", "engine"]);

/** What the link's state reads as in the pill (Design 8.2). */
const PILL = {
  idle: { text: "Waiting for daemon", class: "chip chip-warn" },
  connecting: { text: "Connecting", class: "chip chip-accent" },
  live: { text: "Live updates", class: "chip chip-ok" },
  reconnecting: { text: "Reconnecting", class: "chip chip-warn" },
  stopped: { text: "Updates paused", class: "chip chip-danger" },
};

export function mountActive(root, api) {
  let paused = false;
  let missedWhilePaused = 0;
  let received = 0;
  let lastSyncAt = null;

  /** Rendered cards, newest first. */
  const cards = [];
  /** Frames waiting for the next animation frame. */
  let queue = [];
  let rafHandle = null;

  const statePill = el("span", { class: "chip chip-warn", text: "Waiting for daemon" });

  const stats = {
    actions: statValue("0"),
    lastSync: statValue("—", true),
    plugin: statValue("Not connected", true),
    project: statValue("None", true),
  };

  const pauseButton = el(
    "button",
    { class: "btn btn-sm", type: "button", on: { click: togglePause } },
    "Pause",
  );

  const clearButton = el(
    "button",
    { class: "btn btn-sm", type: "button", on: { click: clearFeed } },
    "Clear",
  );

  // Backoff can be as long as 30 s. When the daemon is back before the timer
  // is, waiting it out is pointless — so offer the retry, but only while it
  // would actually do something.
  const retryButton = el(
    "button",
    {
      class: "btn btn-sm",
      type: "button",
      hidden: true,
      on: {
        click: () => {
          api.retryLink();
          api.setStatus("Reconnecting to the daemon…", "warn");
        },
      },
    },
    icon("refresh", 13),
    "Retry now",
  );

  const feed = el("div", { class: "feed" });

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
          el("h1", { class: "view-title" }, "Activity", " ", statePill),
          el("p", {
            class: "view-sub",
            text: "Every file and instance change the daemon applies, newest first. Cards are sanitized by an allowlist before they reach this view.",
          }),
        ),
        el("div", { class: "view-head-actions" }, retryButton, pauseButton, clearButton),
      ),
      el(
        "div",
        { class: "stat-grid" },
        stat("Actions", stats.actions, `Ring buffer holds ${FEED_CAPACITY}`),
        stat("Last sync", stats.lastSync),
        stat("Plugin", stats.plugin),
        stat("Project", stats.project),
      ),
      feed,
    ),
  );

  function statValue(text, small = false) {
    return el("div", { class: `stat-value${small ? " stat-value-sm" : ""}`, text });
  }

  function stat(label, valueNode, note) {
    return el(
      "div",
      { class: "stat" },
      el("div", { class: "stat-label", text: label }),
      valueNode,
      note ? el("div", { class: "stat-note", text: note }) : null,
    );
  }

  // --------------------------------------------------------------- state ---

  function togglePause() {
    paused = !paused;
    pauseButton.textContent = paused ? "Resume" : "Pause";
    if (paused) {
      missedWhilePaused = 0;
      api.setStatus("Activity feed paused — the daemon keeps working.");
    } else {
      api.setStatus(
        missedWhilePaused > 0
          ? `Activity feed running. ${missedWhilePaused} event${missedWhilePaused === 1 ? "" : "s"} passed while paused.`
          : "Activity feed running.",
      );
      missedWhilePaused = 0;
    }
    renderPill();
  }

  function renderPill() {
    const link = api.getLinkState();
    const stalled =
      link?.state === api.linkStates.RECONNECTING || link?.state === api.linkStates.STOPPED;
    retryButton.hidden = !stalled;

    if (paused) {
      statePill.className = "chip";
      statePill.textContent = "Updates paused";
      statePill.title = "Resume to start showing new events again.";
      return;
    }
    const shape = PILL[link?.state] ?? PILL.idle;
    statePill.className = shape.class;
    statePill.textContent = shape.text;
    statePill.title = link?.detail ?? "";
  }

  function clearFeed() {
    cards.length = 0;
    queue = [];
    received = 0;
    stats.actions.textContent = "0";
    renderFeed();
  }

  // ------------------------------------------------------------- ingest ---

  /**
   * One `event` frame from the daemon. Cheap on purpose: parsing and DOM work
   * happen once per animation frame, however many frames arrived in between.
   */
  function pushEvent(frame) {
    if (paused) {
      missedWhilePaused += 1;
      return;
    }
    queue.push(frame);
    // A hidden window gets no animation frames; without this cap the queue
    // would grow for as long as the app sits in the background.
    if (queue.length > FEED_CAPACITY) queue = queue.slice(-FEED_CAPACITY);
    if (rafHandle === null) rafHandle = requestAnimationFrame(flush);
  }

  function flush() {
    rafHandle = null;
    if (queue.length === 0) return;
    const batch = queue;
    queue = [];

    for (const frame of batch) {
      received += 1;
      const card = formatEvent(frame);
      if (card.category === "sync" || card.category === "syncback") lastSyncAt = card.at;
      cards.unshift(card);
    }
    if (cards.length > FEED_CAPACITY) cards.length = FEED_CAPACITY;

    stats.actions.textContent = String(received);
    stats.lastSync.textContent = lastSyncAt ? clockTime(lastSyncAt) : "—";
    renderFeed();
  }

  // ----------------------------------------------------------- rendering ---

  function renderFeed() {
    if (cards.length === 0) {
      const link = api.getLinkState();
      feed.replaceChildren(
        link?.state === "live"
          ? emptyPanel(
              "Connected — nothing has happened yet",
              "The daemon is live. Edit a file, or change something in Studio, and it lands here.",
            )
          : emptyPanel(
              "No activity yet",
              "Serve a project to open the daemon's event feed. Connect Studio and every change either side makes shows up here.",
            ),
      );
      return;
    }
    feed.replaceChildren(...cards.map(renderCard));
  }

  function emptyPanel(title, body) {
    return el(
      "div",
      { class: "empty" },
      el("div", { class: "empty-icon" }, icon("activity", 17)),
      el("div", { class: "empty-title", text: title }),
      el("p", { class: "empty-body", text: body ?? "" }),
    );
  }

  function renderCard(card) {
    return el(
      "div",
      { class: "feed-card" },
      el("span", { class: `feed-tone ${card.toneClass}` }, icon(card.glyph, 13)),
      el(
        "div",
        { class: "feed-main" },
        el("div", { class: "feed-title", text: card.title }),
        card.facts.length || card.intent
          ? el(
              "div",
              { class: "feed-facts" },
              card.intent ? el("span", { class: "feed-intent", text: card.intent }) : null,
              ...card.facts.map((fact) =>
                el("span", { class: "feed-fact" }, el("b", { text: `${fact.key} ` }), fact.value),
              ),
            )
          : null,
        rawDetails(card.raw),
      ),
      el(
        "span",
        { class: "feed-time" },
        card.duration ? el("span", { class: "feed-duration", text: card.duration }) : null,
        el("span", { text: clockTime(card.at) }),
      ),
    );
  }

  function rawDetails(raw) {
    const details = el("details", { class: "feed-raw" });
    details.append(
      el("summary", { text: "Raw frame" }),
      el("pre", { class: "feed-raw-body", text: raw }),
    );
    return details;
  }

  // ------------------------------------------------------------ context ---

  function refreshContext() {
    const project = api.getProject(api.getState().activeProjectId);
    stats.project.textContent = project ? project.name : "None";
    renderPlugin(api.getPluginStatus());
  }

  function renderPlugin(status) {
    if (!status) {
      stats.plugin.textContent = "Not connected";
      return;
    }
    stats.plugin.textContent = status.connected
      ? (status.place ? `Connected · ${status.place}` : "Connected")
      : "Not connected";
  }

  // ---------------------------------------------------------- lifecycle ---

  const unsubscribers = [
    api.onBus("state", refreshContext),
    api.onBus("activity", pushEvent),
    api.onBus("plugin", renderPlugin),
    api.onBus("link", () => {
      renderPill();
      if (cards.length === 0) renderFeed();
    }),
  ];

  refreshContext();
  renderPill();
  renderFeed();

  const link = api.getLinkState();
  api.setStatus(
    link?.state === "live" ? "Activity feed is live." : (link?.detail ?? "Waiting for a daemon."),
    link?.state === "live" ? "ok" : "warn",
  );

  return () => {
    if (rafHandle !== null) cancelAnimationFrame(rafHandle);
    for (const unsubscribe of unsubscribers) unsubscribe();
  };
}

// --------------------------------------------------------------- formatter --

/**
 * The allowlist. Reads only the fields Design 8.2 names and coerces every one
 * of them to bounded text; the untouched frame is kept as a raw string for the
 * collapsible, never as live objects the renderer might walk.
 */
function formatEvent(frame) {
  const topic = text(frame?.topic, 48) || "event";
  const category = CATEGORIES.has(frame?.category) ? frame.category : categoryFor(topic);
  const tone = TONES[frame?.tone] ?? defaultTone(topic, category);
  const at = Number.isFinite(frame?.at) ? Number(frame.at) : Date.now();

  return {
    at,
    category,
    toneClass: tone,
    glyph: glyphFor(category),
    title: text(frame?.title, MAX_TITLE_CHARS) || describe(frame, topic),
    intent: text(frame?.intent, 64),
    duration: Number.isFinite(frame?.durationMs) ? `${Math.round(frame.durationMs)} ms` : "",
    facts: factList(frame?.facts),
    raw: safeJson(frame),
  };
}

function factList(facts) {
  if (!facts || typeof facts !== "object") return [];
  return Object.entries(facts)
    .slice(0, MAX_FACTS)
    .map(([key, value]) => ({ key: text(key, 32), value: text(value, MAX_FACT_CHARS) }))
    .filter((fact) => fact.key && fact.value);
}

/** A title for frames that carry none — plugin-status, mostly. */
function describe(frame, topic) {
  if (topic === "plugin-status") {
    const where = text(frame?.place, 64);
    return frame?.connected
      ? `Studio plugin connected${where ? ` — ${where}` : ""}`
      : "Studio plugin disconnected";
  }
  if (topic === "backlog") return "Disk content moved to the backlog";
  if (topic === "conflict") return "A conflict was parked";
  if (topic === "project-init") return "Studio asked for a new project";
  if (topic === "config-changed") return "Project configuration changed";
  return topic;
}

function categoryFor(topic) {
  if (topic === "plugin-status") return "plugin";
  if (topic === "backlog") return "backlog";
  if (topic === "project-init") return "project";
  if (topic === "config-changed") return "config";
  return "sync";
}

function defaultTone(topic, category) {
  if (category === "conflict") return TONES.warn;
  if (category === "plugin") return TONES.accent;
  return TONES.info;
}

function glyphFor(category) {
  if (category === "conflict") return "alert";
  if (category === "plugin") return "studio";
  if (category === "syncback") return "arrowLeft";
  if (category === "project" || category === "config") return "folder";
  return "activity";
}

/** Coerce to a single-line, length-capped string. Objects become JSON. */
function text(value, limit) {
  if (value === null || value === undefined || value === "") return "";
  let raw;
  if (typeof value === "string") raw = value;
  else if (typeof value === "number" || typeof value === "boolean") raw = String(value);
  else raw = safeJson(value, 400);
  const flattened = raw.replace(/\s+/gu, " ").trim();
  return flattened.length > limit ? `${flattened.slice(0, limit)}…` : flattened;
}

function safeJson(value, limit = 4000) {
  let raw;
  try {
    raw = JSON.stringify(value, null, 2) ?? String(value);
  } catch {
    raw = "[unserializable frame]";
  }
  return raw.length > limit ? `${raw.slice(0, limit)}\n… truncated` : raw;
}

function clockTime(at) {
  const when = new Date(at);
  if (Number.isNaN(when.getTime())) return "";
  return when.toLocaleTimeString(undefined, { hour12: false });
}
