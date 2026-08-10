# WSync

WSync is a filesystem ↔ Roblox Studio sync tool: Argon's full-DataModel sync engine combined with Ro-Sync's product layer (desktop app, project management, 62-command CLI surface, divergence resolution).

This repository currently contains the engine through **Phase 2** (see `Design.md` §15): the Argon core rehosted on axum + tokio (Phase 1), plus the protocol v1 WebSocket server and the authenticated daemon lifecycle (Phase 2). A stock Argon Studio plugin can still connect over the msgpack fallback transport.

## Usage

```sh
wsync init my-place -T place        # scaffold a project
wsync serve my-place                # serve it in the foreground (default port 7978)
wsync daemon start --project my-place --raw   # machine-managed daemon (idempotent)
wsync daemon status --project my-place --raw
wsync watch my-place --compact      # live event feed over WebSocket
wsync daemon stop --project my-place
wsync build my-place -o out.rbxl
wsync config --list                 # all settings with defaults and docs
```

Commands: `init`, `serve`, `daemon <start|status|stop|restart|logs>`, `watch`, `build`, `sourcemap`, `stop`, `studio`, `debug`, `exec`, `update`, `install`, `plugin`, `config`, `doc`.

Global config lives in `~/.wsync/config.toml`; per-workspace overrides in `wsync.toml` next to the project. Project files use the Rojo-compatible `*.project.json` format unchanged. `wsync install` (explicit, prompted) copies the binary to `~/.wsync/bin` and adds it to PATH — nothing installs itself on first run.

## Protocol v1 (`wsync/1`)

One daemon per project, loopback only. Two transports share one core sync bus, and a WS plugin and a long-poll plugin occupy the same single-plugin slot (one Studio connection owns the live bridge at a time):

- **WebSocket (primary)** — `GET /ws`, JSON text frames. Every frame is a flat object with a `type` tag and payload fields inline (`{"type":"sync","additions":[…],"updates":[…],"removals":[…]}`), never a nested wrapper. Clients hello with `{"type":"hello","clientId","role":"plugin"|"agent"|"watch"|"app","protocol":1,"name"}`; the daemon answers `{"type":"hello","name","version","gameId","placeIds","rootRefs":[…]}` or a typed `shutdown` (`{reason, code, retryable}`). Heartbeat: server `ping` every ~2 s, silence past ~8 s disconnects. Server→client: `sync`, `details`, `execute`, `push-result`, `request`, `event`, `shutdown`; client→server: `push`, `response`, `event-sub`, `pong`. `watch`/`app` clients receive sanitized `event` frames only (`sync-activity` counts and names, `plugin-status`, `daemon`), filtered via `event-sub {topics}`, behind bounded queues — a slow reader is disconnected, never allowed to block the plugin bridge.
- **msgpack long-poll (fallback)** — the Argon-compatible HTTP surface (`/details`, `/subscribe`, `/read`, `/write`, msgpack `POST /snapshot`, …), always served. `GET /details` reports the Argon-compat version string only when `compat_argon` is enabled; `GET /hello` and the WS hello always report the real version and `protocol: 1`.
- **Refs** are 32-char lowercase hex strings on every JSON surface (all-zeros = root). Malformed refs are rejected with a clear error and the containing frame is never partially applied.
- **HTTP** — `GET /hello` (discovery identity), `GET /snapshot?ref=<hex>` (JSON subtree export), `POST /request` (one-shot remote op routed to the plugin over `request`/`response`, with `TIMEOUT`/`PLUGIN_ERROR`-style error codes), `POST /stop` (authenticated on managed daemons: exact `bootId` + `token`; empty body works on unmanaged daemons), `POST /manager-heartbeat` and `/manager-close` (owner-token lifecycle for the desktop manager; managed daemons self-terminate after 5 minutes without a heartbeat, with a 30-second suspect grace window for laptop sleep).

### Daemon lifecycle

`wsync daemon start` makes the invoking process the daemon (foreground, machine-manageable): with `--raw` it prints exactly one JSON line (`{"ok":true,"port","pid","bootId","project","canonicalProject"}`) once serving, and reports `"alreadyRunning":true` idempotently when a matching daemon already runs. Runtime records live in the platform data dir (`WSync/daemons/<sha256(canonicalProject)>.json`, 0600, atomic; override with `--data-dir` or `WSYNC_STATE_DIR`) alongside an OS-exclusive `.start.lock` and a per-daemon lifecycle log (`wsync daemon logs`). Stopping verifies the live boot identity over `/hello` before anything is killed — a stale PID record is never authority to kill — and prefers graceful `/stop` with an OS-kill fallback.

## Attribution

WSync's engine is forked from [Argon](https://github.com/argon-rbx/argon) (© Dervex and contributors, Apache-2.0) — see `NOTICE`. The product layer is a clean-slate remake of Ro-Sync by Ro-Sync's own author. Licensed under Apache-2.0 (`LICENSE.md`).
