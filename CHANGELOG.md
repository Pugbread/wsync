# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), that adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- Script class and RunContext are now fully encoded in the file suffix — one
  suffix, one meaning, no mode flag: `.server.luau` is a `Script` with
  `RunContext = Legacy`, `.client.luau` a `Script` with `RunContext = Client`,
  `.local.luau` a `LocalScript`, `.runserver.luau` a `Script` with
  `RunContext = Server`, and plain `.luau` a `ModuleScript`
  (`RunContext = Plugin` falls back to `.server.luau` with the value kept in
  the data sidecar). The `legacyScripts` / `emitLegacyScripts` project field is
  parsed but ignored. **Breaking:** existing `.client.luau` files change
  meaning from `LocalScript` to a RunContext-Client `Script` — rename them to
  `.local.luau` (or delete and re-create the project from Studio)
- Code scope now reads and writes the instance-data sidecars
  (`*.meta.json` / `*.data.json`) of in-scope instances, so attributes, tags
  and suffix-inexpressible properties (e.g. `RunContext = Plugin`) round-trip
  instead of re-flagging the connect-time disk review on every reconnect

### Added

- Phase 1 engine fork: Argon 2.0.29 core forked as WSync (`argon` → `wsync`, `~/.argon` → `~/.wsync`, `argon.toml` → `wsync.toml`, default port 7978)
- HTTP server rewritten on axum + tokio (long-poll endpoints preserved wire-for-wire)
- `GET /hello` JSON identity endpoint for daemon discovery
- Config keys: `syncback_model_json`, `conflict_engine`, `compat_argon`, `port_scan_max`, `auto_open_app_modal`
- `share_stats` now defaults to `false` and the telemetry upload path is removed entirely (local counters only)

### Fixed

- `wsync stop` resolves its arguments as session ID, then `host:port` address, then bare port — previously only registry IDs matched despite the help text promising address support
- `wsync stop` exits nonzero when nothing was stopped (no matching session, or a managed daemon refused the unauthenticated stop), so scripts can distinguish success from a no-op
- Stopping a session no longer drops registry entries of sessions it never touched: only entries that were actually stopped (or whose process is verified dead) are removed, `wsync stop --all` keeps entries of daemons that refused, and the registry file is now replaced atomically so a concurrent rewrite by a stopping session can no longer be misread as corrupted and wiped
- Session lookups by host/port now require both provided fields to match (previously either one matched, letting shutdown cleanup remove the wrong session's entry)

For engine history prior to the fork, see [Argon's changelog](https://github.com/argon-rbx/argon/blob/main/CHANGELOG.md) up to 2.0.29.
