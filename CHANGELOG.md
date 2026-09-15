# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), that adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- Generated agent instructions now use a concise contract with on-demand
  reference docs, conditional discovery, current backlog semantics, carried
  task authorization, command effects, and explicit context/command budgets.
- Command discovery defaults to a compact offline index; `--full` explicitly
  requests the complete registry. Query output defaults to 200 matches.
- Ordinary CLI commands no longer run installation, update checks, or stats
  persistence as hidden startup/exit work. Use explicit install/update commands.

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

- Offline value schemas (`wsync commands types [TypeName]`) and canonical
  Vector3/Color3 examples in real command help. Typed values are validated
  before delivery with actionable field/type errors.
- Eval source-file/stdin input, safe source-file exports, consistent named
  project flags, negative vector support, and documented flag aliases.
- Per-boot operation receipts distinguish not-started, completed, partial,
  and unknown outcomes, including late replies. Project/daemon boot guards
  and pinned playtest job IDs prevent stale-target writes.
- Regression tests parse every documented CLI example and exercise value
  contracts, source input/export, job identity, nil returns, and timeout recovery.

- `AvoidSync` Studio attribute, now implemented: a boolean attribute set to
  `true` on an instance of any class keeps that instance and its subtree in
  Studio — left out of live syncback, the connect-time diff and the
  Studio → disk pull. Setting it on a synced instance removes its files;
  clearing it syncs the subtree back. The daemon does not read it: a sidecar
  carrying `AvoidSync: true` syncs to Studio once and the instance is
  Studio-only from the moment it lands, the file left behind as a stale copy
  the next connect moves to the backlog. Shares the exclusion plumbing with
  `--!ignoresync`, which now also takes effect the moment a file carrying it
  arrives from disk rather than at the script's next Studio edit
- `--!ignoresync` script directive: a script carrying it on its own line within
  its first 10 lines stays in Studio, together with everything under it — the
  plugin leaves it out of live syncback, the connect-time diff and the
  Studio → disk pull, so it is never written to disk and its bytes never count
  against the per-root transfer bound. Adding the directive to a synced script
  removes its file; deleting the directive syncs the script back. Meant for
  scripts that hold bulk data in string literals. The generated agent docs
  describe it next to `AvoidSync`
- Phase 1 engine fork: Argon 2.0.29 core forked as WSync (`argon` → `wsync`, `~/.argon` → `~/.wsync`, `argon.toml` → `wsync.toml`, default port 7978)
- HTTP server rewritten on axum + tokio (long-poll endpoints preserved wire-for-wire)
- `GET /hello` JSON identity endpoint for daemon discovery
- Config keys: `syncback_model_json`, `conflict_engine`, `compat_argon`, `port_scan_max`, `auto_open_app_modal`
- `share_stats` now defaults to `false` and the telemetry upload path is removed entirely (local counters only)

### Fixed

- Batch and playtest source failures now exit nonzero with truthful results;
  fire-and-forget exec reports queue acknowledgement rather than execution.
- Localhost clients bypass system proxy discovery, avoiding the macOS proxy
  initialization path that failed in restricted hosts.
- Relative lint scopes follow an explicitly selected project; absolute paths
  are preserved. Playtest return slots preserve nils and multiple values.

- `wsync stop` resolves its arguments as session ID, then `host:port` address, then bare port — previously only registry IDs matched despite the help text promising address support
- `wsync stop` exits nonzero when nothing was stopped (no matching session, or a managed daemon refused the unauthenticated stop), so scripts can distinguish success from a no-op
- Stopping a session no longer drops registry entries of sessions it never touched: only entries that were actually stopped (or whose process is verified dead) are removed, `wsync stop --all` keeps entries of daemons that refused, and the registry file is now replaced atomically so a concurrent rewrite by a stopping session can no longer be misread as corrupted and wiped
- Session lookups by host/port now require both provided fields to match (previously either one matched, letting shutdown cleanup remove the wrong session's entry)

For engine history prior to the fork, see [Argon's changelog](https://github.com/argon-rbx/argon/blob/main/CHANGELOG.md) up to 2.0.29.
