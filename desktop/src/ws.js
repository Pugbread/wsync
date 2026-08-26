// ws.js — the app's WebSocket link to one project daemon (Design 5.3).
//
// One link object for the whole app, retargeted as the served project changes:
// `setTarget({projectId, base})` connects, `setTarget(null)` disconnects. The
// link owns reconnection, so no view has to think about it.
//
// The contract it speaks, in full:
//
//   → {"type":"hello", clientId:<u32>, role:"app", protocol:1, name:"WSync Desktop"}
//   ← {"type":"hello", name, version, gameId, placeIds, rootRefs}
//   ← {"type":"ping"}                 → {"type":"pong"}
//   ← {"type":"event", topic, …}      delivered to onFrame
//   ← {"type":"shutdown", reason, code, retryable}
//   → {"type":"event-sub", topics}    optional, sent after the server hello
//
// Frames are flat JSON objects with a `type` tag and their payload inline; the
// link never unwraps a nested envelope, because there is not one.
//
// `retryable: false` is honoured strictly: a daemon that says "do not come
// back" is not retried, and the link parks in `stopped` until something
// external (a `daemon:up`, a re-serve) gives it a new target.

/** What the Activity view's state pill renders. */
export const LINK_STATE = Object.freeze({
  /** No project is served — nothing to connect to. */
  IDLE: "idle",
  /** A socket is opening, or open and waiting for the server hello. */
  CONNECTING: "connecting",
  /** Server hello received; frames are flowing. */
  LIVE: "live",
  /** The socket dropped and a retry is scheduled. */
  RECONNECTING: "reconnecting",
  /** The daemon sent a non-retryable shutdown. Only a new target revives it. */
  STOPPED: "stopped",
});

const PROTOCOL = 1;
const FIRST_RETRY_MS = 1000;
const MAX_RETRY_MS = 30_000;

/**
 * With no frame at all for this long the socket is treated as dead even if the
 * OS still thinks it is open — a laptop that slept through the daemon's exit
 * otherwise sits on a socket that will never speak again. Generous next to the
 * daemon's ~2 s ping so a busy engine is never dropped mid-batch.
 */
const IDLE_TIMEOUT_MS = 30_000;

/** Design 5.3: the app/watch feeds this client asks for. */
export const APP_TOPICS = Object.freeze([
  "sync-activity",
  "plugin-status",
  // Disk content that lost to Studio. Sync never asks a question, so this is
  // an announcement rather than a prompt: the app updates a count with it and
  // opens nothing.
  "backlog",
  "config-changed",
  "project-init",
]);

let nextClientSeed = null;

/** A random u32, per the hello contract. */
function newClientId() {
  const crypto = globalThis.crypto;
  if (crypto?.getRandomValues) {
    nextClientSeed ??= new Uint32Array(1);
    crypto.getRandomValues(nextClientSeed);
    return nextClientSeed[0];
  }
  return Math.floor(Math.random() * 2 ** 32);
}

/**
 * @param {object} options
 * @param {(frame: object, context: {projectId: string}) => void} options.onFrame
 *   every server frame, already JSON-parsed.
 * @param {(status: {state: string, projectId: string|null, detail: string,
 *   attempt: number, retryInMs: number|null}) => void} options.onState
 * @param {string} [options.name] the client name sent in the hello.
 * @param {string[]|null} [options.topics] `event-sub` topics, or null for all.
 * @param {(url: string) => WebSocket} [options.open] injection seam for tests.
 */
export function createDaemonLink({
  onFrame = () => {},
  onState = () => {},
  name = "WSync Desktop",
  topics = APP_TOPICS,
  open = (url) => new WebSocket(url),
} = {}) {
  let target = null; // {projectId, base}
  let socket = null;
  let attempt = 0;
  let retryTimer = null;
  let idleTimer = null;
  let state = LINK_STATE.IDLE;
  let detail = "No project is being served.";
  let destroyed = false;
  let serverHello = null;

  function report(nextState, nextDetail, retryInMs = null) {
    state = nextState;
    detail = nextDetail;
    onState({
      state,
      detail,
      projectId: target?.projectId ?? null,
      attempt,
      retryInMs,
      serverHello,
    });
  }

  function clearTimers() {
    if (retryTimer !== null) {
      clearTimeout(retryTimer);
      retryTimer = null;
    }
    if (idleTimer !== null) {
      clearTimeout(idleTimer);
      idleTimer = null;
    }
  }

  function dropSocket() {
    if (!socket) return;
    // Detach first: a close we caused must not schedule a reconnect.
    socket.onopen = null;
    socket.onmessage = null;
    socket.onerror = null;
    socket.onclose = null;
    try {
      socket.close();
    } catch {
      // Already closing or never opened; nothing to do.
    }
    socket = null;
  }

  function touchIdleTimer() {
    if (idleTimer !== null) clearTimeout(idleTimer);
    idleTimer = setTimeout(() => {
      if (!socket) return;
      dropSocket();
      scheduleReconnect("The daemon stopped sending frames.");
    }, IDLE_TIMEOUT_MS);
  }

  function backoffMs() {
    const base = Math.min(MAX_RETRY_MS, FIRST_RETRY_MS * 2 ** Math.max(0, attempt - 1));
    // Jitter keeps a restarted daemon from being hit by every client at once.
    return Math.round(base * (0.8 + Math.random() * 0.4));
  }

  function scheduleReconnect(why) {
    if (destroyed || !target) return;
    attempt += 1;
    const wait = backoffMs();
    report(LINK_STATE.RECONNECTING, why, wait);
    retryTimer = setTimeout(() => {
      retryTimer = null;
      connect();
    }, wait);
  }

  function connect() {
    if (destroyed || !target) return;
    clearTimers();
    dropSocket();

    const url = `${target.base.replace(/^http/u, "ws")}/ws`;
    report(
      attempt === 0 ? LINK_STATE.CONNECTING : LINK_STATE.RECONNECTING,
      attempt === 0 ? "Connecting to the daemon…" : `Reconnecting (attempt ${attempt + 1})…`,
    );

    let opened;
    try {
      opened = open(url);
    } catch (error) {
      scheduleReconnect(`Could not open ${url}: ${error?.message ?? error}`);
      return;
    }
    socket = opened;
    const mine = socket;

    socket.onopen = () => {
      if (mine !== socket) return;
      report(LINK_STATE.CONNECTING, "Connected — waiting for the daemon's hello…");
      send({ type: "hello", clientId: newClientId(), role: "app", protocol: PROTOCOL, name });
      touchIdleTimer();
    };

    socket.onmessage = (message) => {
      if (mine !== socket) return;
      touchIdleTimer();

      let frame;
      try {
        frame = JSON.parse(typeof message.data === "string" ? message.data : "");
      } catch {
        // A frame we cannot read is not a reason to tear down a working link.
        console.warn("daemon sent an unreadable frame");
        return;
      }
      if (!frame || typeof frame !== "object") return;

      if (frame.type === "ping") {
        send({ type: "pong" });
        return;
      }

      if (frame.type === "hello") {
        serverHello = frame;
        attempt = 0;
        report(LINK_STATE.LIVE, `Live updates from ${frame.name ?? "the daemon"}.`);
        if (Array.isArray(topics) && topics.length) send({ type: "event-sub", topics });
      }

      if (frame.type === "shutdown") {
        const retryable = frame.retryable !== false;
        dropSocket();
        clearTimers();
        if (retryable) {
          scheduleReconnect(shutdownText(frame));
        } else {
          attempt = 0;
          report(LINK_STATE.STOPPED, shutdownText(frame));
        }
        // Still hand the frame on: the app reports the reason to the user.
      }

      onFrame(frame, { projectId: target?.projectId ?? null });
    };

    socket.onerror = () => {
      // `onclose` always follows and carries the usable detail; reacting here
      // too would double every reconnect.
    };

    socket.onclose = (event) => {
      if (mine !== socket) return;
      socket = null;
      serverHello = null;
      const why = event?.reason
        ? `The daemon closed the connection: ${event.reason}`
        : "The daemon connection closed.";
      scheduleReconnect(why);
    };
  }

  function send(value) {
    if (!socket || socket.readyState !== 1) return false;
    try {
      socket.send(JSON.stringify(value));
      return true;
    } catch {
      return false;
    }
  }

  return {
    /**
     * Point the link at a project's daemon, or at nothing. Re-targeting the
     * same base is a no-op, so a state broadcast does not churn the socket.
     */
    setTarget(next) {
      if (destroyed) return;
      // Including null === null: selecting a project while nothing is served
      // must not tear down and re-announce an idle link on every click.
      const same =
        (!next && !target) ||
        Boolean(next && target && next.base === target.base && next.projectId === target.projectId);
      if (same) return;

      clearTimers();
      dropSocket();
      serverHello = null;
      attempt = 0;
      target = next ? { projectId: next.projectId, base: next.base } : null;

      if (!target) {
        report(LINK_STATE.IDLE, "No project is being served.");
        return;
      }
      connect();
    },

    /** Retry now, whatever the backoff said — used when the host says a daemon came up. */
    retryNow() {
      if (destroyed || !target) return;
      clearTimers();
      attempt = 0;
      connect();
    },

    getState() {
      return { state, detail, projectId: target?.projectId ?? null, attempt, serverHello };
    },

    send,

    destroy() {
      destroyed = true;
      clearTimers();
      dropSocket();
      target = null;
      state = LINK_STATE.IDLE;
    },
  };
}

function shutdownText(frame) {
  const reason = typeof frame.reason === "string" ? frame.reason : "the daemon shut down";
  const code = typeof frame.code === "string" ? ` (${frame.code})` : "";
  return `${reason}${code}`;
}
