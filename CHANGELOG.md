# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), that adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- **A connect is three-way now.** The daemon keeps a sync base — for every
  instance path, the class and the SHA-256 of the normalized script source
  both sides last agreed on (`.wsync-backups/sync-base.json`, recorded at
  every committed connect and, while connected, from a lagged tree snapshot
  committed only once the connection has held) — and the plugin reports the
  same key for the Studio side of every difference, plus its place id. Only
  disk moved since the base → disk wins and is pushed to Studio in one
  bracketed replay (edits, files created on disk, and disk deletions of Studio
  subtrees nobody touched); only Studio moved → Studio wins, with no backlog
  entry; both moved, a difference outside the key, an ambiguous path, or no
  base (first connect, another place) → Studio wins and the disk side goes to
  the backlog, exactly as before. A daemon crash no longer turns into a mass
  overwrite of disk work on the next connect
- After a connect that rewrote disk, the plugin re-pairs its instances with
  the daemon tree before live sync starts; an empty diff now uploads too, so
  the daemon knows the sides agree and records the base

- One path spelling everywhere: every command that addresses one instance
  takes its path positionally or as `--path` — `get`, `ls`, `tree`, `props`,
  `source`, `meta`, `path`, `set`, `new`, `rm`, `attr`, `tag`, `call`,
  `capture ui|model` and `plan set|new|rm` (`open` gains a repeatable
  `--path`). Both forms together are accepted only when they name the same
  path; two different paths are a usage error before anything is sent
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

- `wsync doctor` works offline: without a daemon it still loads the project
  and scans every mapped directory for disk hazards — ambiguous init files,
  flat/folder pairs, empty shadowed folders, interrupted reshapes, duplicate
  sibling names and oversized unignored files — each with the path WSync
  uses and a suggested fix (`--raw`: `offline`, `hazards`). A missing daemon
  is a warning now, so offline the exit code reflects the disk checks; with a
  daemon the scan is appended to the report. The scanner is a library module
  (`core::hazards`) other commands can call

- Crash-loop protection: a daemon that dies while applying a connect (or
  within 30 s of it) leaves a marker, and the next daemon starts in safe mode —
  a connect compares and decides but applies nothing, writes what it would
  have done to `.wsync-backups/safe-mode-report.json`, and refuses with that
  explanation. `/hello` reports `safeMode`; `wsync daemon resume` ends it
- `wsync backlog diff <id>` (an entry against the file at its path now),
  `wsync backlog pin <id>` / `unpin <id>` (a pinned entry never expires), and
  the `backlog_ttl_hours` setting (24). Capturing bytes identical to an entry
  already held for the same path refreshes that entry instead of adding a
  second copy; `backlog list` shows a path's older versions under its newest

- `wsync wait-synced [--timeout <secs>] [--raw]`: a barrier for batch edits.
  It returns once Studio has applied every disk change the daemon had seen
  when it was called — the watcher's debouncer is observed flushing past that
  moment, the sync processor is fenced, the plugin connection is proven to
  hold every resulting frame, and the plugin answers a queue-ordered
  `sync_barrier` only after applying them (script source writes included).
  Reports how many changes were confirmed and how long it took; fails with
  the stage that stalled on timeout, without a plugin, or without a daemon,
  and lists disk edits that lost to Studio (moved to the backlog) while it
  waited. `wsync status` gains the pending-outbound count (queued, and sent
  but not yet confirmed by a barrier) in human and `--raw` output
- `wsync verify [--no-wait] [--raw]`: hashes every synced script on disk
  (the engine's own VFS read and line-ending normalization) against Studio's
  editor text in bounded chunks and lists content mismatches with both sizes,
  scripts missing in Studio or on disk, disk changes the daemon never picked
  up, same-named siblings in Studio and in the daemon's projection, disk
  layout hazards (the `doctor` scanner: double-init folders, flat+folder
  pairs), backlogged paths rewritten on disk that never reached Studio, and
  (as information) editor drafts that differ from the committed Source.
  Exits 0 only when clean
- A missing daemon is now loud: every command that addresses a project
  (`--project`, a positional project, or the project in the working
  directory) prints a short banner on stderr when no live daemon serves it —
  offline commands such as `lint`, `build`, `sourcemap` and `doctor`
  included. It says what the runtime record and the daemon journal know
  (crashed, with the exit status and last stderr lines a manager recorded;
  heartbeat lost; stopped; not answering; never started), when, and the
  command that starts it. stdout is never touched, so `--raw` JSON stays
  parseable; the check reads the records and probes a port only when
  something claims a daemon serves it. Lifecycle, install/config/auth and
  legacy session commands are skipped, and `WSYNC_NO_DAEMON_BANNER=1` turns
  it off for scripts that never run a daemon
- Desktop: a daemon that crashes or stops answering heartbeats raises an OS
  notification and a persistent in-app banner with a Restart action, and its
  exit status and last stderr lines are appended to the project's daemon log
  (`Manager: …` notes), so a stack-overflow abort leaves evidence for
  `wsync daemon logs` and the banner above
- `playtest run` puts client failures in its final result: every failed
  client result (with its traceback) and every PlayClient error log reported
  during the run is repeated on the terminal record as `clientFailures`
  (`clientFailuresOmitted` past the caps) — including when the server times
  out while the main script waits, and when the CLI writes the timeout itself
  because no terminal record arrived. Human output prints a failed client's
  traceback as it arrives and summarises all client failures after the final
  line; plugin timeouts are labelled `timeout` instead of `failure`. The
  workflow `playtest` step's timeout and lost-stream errors carry the same
  `clientFailures` alongside the records they saw
- `wsync eval --drafts`: `require` runs each ModuleScript's editor text (the
  draft WSync wrote) instead of its committed `.Source`, transitively and
  without committing anything, and the result reports which modules ran from
  editor text. The `eval` and `source` docs explain the drafts caveat
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

### Removed

- Studio waypoints and Studio saves. `wsync save` and `wsync waypoint` (and
  their plugin ops) are gone, as is `wsync set --waypoint` and the
  `set_batch` op's `waypoint` argument — Studio auto-saves, and its own
  change history is what `wsync undo` / `wsync redo` drive. The plugin no
  longer sets a `WSync: …` waypoint after every sync batch or live write; a
  daemon-driven bulk pull and a clipboard paste are still one Studio undo
  each (ChangeHistory recordings)

### Fixed

- A disk edit made while WSync was writing a Studio change to disk (or
  applying a connect), or within 300 ms after, was silently dropped as the
  write's echo. Events in that window are now held and delivered once it
  closes; only events for paths the processor itself wrote are dropped, so
  echo suppression is unchanged and an agent's edit to another file is not
  lost. `wait-synced` waits for held events before it passes
- Removing one instance's path mapping from the daemon tree unmapped every
  instance sharing that path, so a later event there resolved from the
  parent's level

- Two init files in one folder (`init.client.luau` beside `init.luau`, as
  when an agent renames one into the other) recursed until the stack
  overflowed — killing the daemon with nothing in its log — and crashed
  `lint`, `sourcemap` and `build` the same way. The first sync rule's init
  file now wins (the order that resolves a lone init file), the others are
  ignored, and a warning names the folder and every ignored file, once per
  occurrence rather than on every event
- Every recursive walk of the snapshot and write paths (directories, project
  nodes, model files, subtrees sent from Studio) is bounded by a level count
  and a stack budget: a cyclic layout (a project `$path` naming an ancestor)
  or a pathologically deep model fails with an error naming the path instead
  of overflowing the stack. A daemon panic is now written to the daemon log
  (`wsync daemon logs`) with its message, location and the path being
  processed, and the log opens before the first snapshot
- A file ↔ folder reshape (a script gaining its first child, or losing its
  last) runs as one crash-safe transaction through a `.X.wsync-reshape`
  staging directory, so a crash can no longer leave `X.luau` beside
  `X/init.luau` or beside an empty `X/`; the next daemon start finishes or
  rolls back an interrupted one. Leftovers of older builds resolve
  deterministically: a folder with an init file wins over the same-named flat
  file, and an empty folder beside a flat file is ignored instead of
  projecting a duplicate `X` Folder (and no longer blocks the next
  promotion). A folder that holds other files keeps its folder form instead
  of being deleted with them

- A file written on disk at a path the backlog had just taken never reached
  Studio, and deleting it sent Studio a removal for an id it never received:
  content a connect moved to the backlog now leaves the daemon tree too
- A connect could write Studio's copy of a script and then move that very
  copy to the backlog as "disk-only" (when the connect failed to pair the
  path — e.g. a stale daemon-tree entry left by an earlier backlog capture):
  a disk-only path Studio's projection also claims now keeps Studio's copy,
  and the disk original is backlogged only if its bytes differ
- Studio-only scripts a connect wrote to disk were orphaned — the daemon
  tracked them under ids the plugin never learned, so later edits went
  nowhere in either direction; the plugin now re-pairs after such a connect
- A disk-only directory or flat script with a sidecar now leaves the disk
  whole when it goes to the backlog, instead of leaving an empty folder or an
  orphaned `.meta.json` behind to project as a duplicate sibling

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
