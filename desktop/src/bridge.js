// bridge.js — the only module that talks to Tauri.
//
// Everything privileged goes through a named host command (Design 8.1). Views
// never see `invoke`, never see a path they did not get from the picker, and
// never construct an IPC message. If a view needs a new capability, it gets a
// new method here backed by a new Rust command — not a general-purpose hatch.
//
// The app also has to run in a plain browser so layout work does not require a
// build of the shell. Outside Tauri every call rejects with `code: "no_host"`,
// and the store falls back to memory-only state.

const core = globalThis.__TAURI__?.core;
const events = globalThis.__TAURI__?.event;

/** True when running inside the packaged/dev Tauri window. */
export const IS_TAURI = typeof core?.invoke === "function";

/** Typed failure crossing the IPC boundary. `code` mirrors Rust's `HostError`. */
export class HostError extends Error {
  constructor(code, message) {
    super(message || code);
    this.name = "HostError";
    this.code = code;
  }

  /** Commands whose shape is settled but whose behavior is a later wave. */
  get isNotImplemented() {
    return this.code === "not_implemented";
  }

  /** The user dismissed a native picker or prompt — not an error to report. */
  get isCancelled() {
    return this.code === "cancelled";
  }

  /** No Tauri host at all (browser preview). */
  get isHostless() {
    return this.code === "no_host";
  }

  /**
   * An artifact was found but is not the one its build manifest describes.
   * Its own state rather than a generic failure: retrying cannot fix it, so
   * the UI must say "rebuild", never "try again".
   */
  get isIntegrity() {
    return this.code === "integrity";
  }

  /** The capability is real, but nothing is there to serve it right now. */
  get isUnavailable() {
    return this.code === "unavailable";
  }
}

function toHostError(thrown) {
  if (thrown instanceof HostError) return thrown;
  if (thrown && typeof thrown === "object" && typeof thrown.code === "string") {
    return new HostError(thrown.code, thrown.message);
  }
  return new HostError("unknown", typeof thrown === "string" ? thrown : String(thrown?.message ?? thrown));
}

async function call(command, args) {
  if (!IS_TAURI) {
    throw new HostError("no_host", `${command} needs the WSync desktop host`);
  }
  try {
    return await core.invoke(command, args);
  } catch (thrown) {
    throw toHostError(thrown);
  }
}

/**
 * Subscribe to an event the Rust host emits (`daemon:up`, `daemon:down`).
 *
 * Returns an unsubscribe function that is safe to call before the listener has
 * finished registering, and a no-op outside Tauri — a view can wire this up
 * unconditionally.
 */
export function onHostEvent(name, handler) {
  if (typeof events?.listen !== "function") return () => {};
  let unlisten = null;
  let cancelled = false;
  events
    .listen(name, (event) => {
      try {
        handler(event.payload);
      } catch (error) {
        console.error(`host event handler for ${name} failed`, error);
      }
    })
    .then((stop) => {
      if (cancelled) stop();
      else unlisten = stop;
    })
    .catch((error) => console.error(`could not listen for ${name}`, error));

  return () => {
    cancelled = true;
    unlisten?.();
    unlisten = null;
  };
}

export const host = {
  /** Version, platform, data-dir paths, and whether the updater has a key. */
  appInfo: () => call("app_info"),

  /** Whole persisted document, or one Design 8.3 key. */
  stateGet: (key) => call("state_get", key === undefined ? {} : { key }),

  /** Merge allowlisted keys and replace `state.json` atomically. */
  stateSet: (patch) => call("state_set", { patch }),

  /** Native folder picker — the only way a path enters the app (Design 11). */
  pickProjectFolder: () => call("pick_project_folder"),

  // --- Projects folder + broker (Design 8.4).
  //
  // Authorizing a folder is a host operation, not a preference: the broker
  // listens exactly while one is authorized, so the same call that persists the
  // root starts the listener, and clearing it stops one. None of these takes a
  // path — the picker is the only source, in Rust.
  projectsRootGet: () => call("projects_root_get"),
  projectsRootSet: () => call("projects_root_set"),
  projectsRootClear: () => call("projects_root_clear"),
  /** Reveal the authorized folder in the OS file manager. No arguments by design. */
  projectsRootReveal: () => call("projects_root_reveal"),
  /** `{running, port, root, detail}` — what the Settings status line reads. */
  brokerStatus: () => call("broker_status"),

  // --- Daemon lifecycle (Design 3.3). The host owns the child handles and the
  // per-project owner token; neither ever crosses this boundary.
  daemonStart: (projectId, projectPath, port) =>
    call("daemon_start", { projectId, projectPath, port: port ?? null }),
  daemonStop: (projectId) => call("daemon_stop", { projectId }),
  daemonStatus: (projectId) => call("daemon_status", { projectId }),

  /**
   * Read one allowlisted daemon route through the host (Design 5.2).
   *
   * The webview cannot call the daemon over HTTP itself: a browser-origin
   * request needs the owner capability token, and the token deliberately never
   * leaves Rust. The host is a native loopback client, so it skips that gate.
   * Resolves for any status — a 404 is data (an engine without the conflict
   * routes), not a failure.
   */
  daemonRequest: (projectId, route) => call("daemon_request", { projectId, route }),

  /**
   * Write one allowlisted daemon route through the host (Design 5.2).
   *
   * The write half of the same narrow door: `POST /resolve`, `POST /choice`,
   * `POST /choice/selection` and nothing else, each with a body cap enforced in
   * Rust. Resolves for any status for the same reason as `daemonRequest` — the
   * divergence flow has to read a 409 to learn the choice was resolved
   * elsewhere, and a 404 to learn a conflict id is already gone.
   */
  daemonPost: (projectId, route, body) => call("daemon_post", { projectId, route, body }),

  // --- Studio plugin (Design 8.2).
  //
  // The host resolves the artifact itself — app resources, then
  // `WSYNC_PLUGIN_ARTIFACT`, then a dev tree — verifies it against the sha256
  // in the `WSync.build.json` beside it, and copies it into Roblox's plugins
  // folder. A dev-tree artifact older than the plugin sources is rebuilt by
  // the host first, so Install always means the checkout's current code. No
  // path crosses this boundary in either direction except the one the host
  // reports back.
  //
  // `{pick: true}` is the second click after a failed resolve: it opens the
  // native file dialog. Never the first click's behavior — an unrequested
  // native dialog is a worse answer than an error naming the build command.
  pluginInstall: (options) => call("plugin_install", { pick: options?.pick ?? null }),

  /**
   * `{installed, path, size, modifiedAt, sha256, pluginVersion,
   * protocolVersion, builtAt, buildCommit, buildDirty, verified, stale,
   * daemonProtocol, protocolMatch, warning, note}`.
   *
   * `projectId` is optional and names a served project: with one, the host
   * compares the installed plugin's protocol against that daemon's `/hello`.
   * Resolves rather than throwing — an absent plugin is a state, not an error.
   */
  pluginStatus: (projectId) => call("plugin_status", { projectId: projectId ?? null }),

  /** Reveal the Roblox plugins folder. No arguments, by design. */
  pluginsDirReveal: () => call("plugins_dir_reveal"),

  // --- Secrets (Design 8.2, 11). The OS keychain, with a 0600 file fallback.
  //
  // `value` is the only secret that ever crosses this boundary, and it only
  // travels *inward*. Every answer is a status — `{name, present, masked,
  // store, note}` — where `masked` is a tail like "…abcd" and nothing more; the
  // key itself stays in Rust, which is what makes the store worth having.
  secretSet: (name, value) => call("secret_set", { name, value }),
  secretGet: (name) => call("secret_get", { name }),
  secretClear: (name) => call("secret_clear", { name }),

  /**
   * Reserved: hand the stored key to a spawned daemon by environment variable.
   * Answers `not_implemented` until the CLI credential handoff lands — the
   * webview must never become the courier for a key it cannot read.
   */
  secretExportEnv: (name) => call("secret_export_env", { name }),
};
