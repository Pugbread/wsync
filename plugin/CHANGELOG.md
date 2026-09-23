# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), that adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

History prior to the fork lives in
[argon-roblox's changelog](https://github.com/argon-rbx/argon-roblox/blob/main/CHANGELOG.md)
(forked at 2.0.23, Apache-2.0).

## [0.1.0] - Unreleased

### Removed

- Studio waypoints and Studio saves: the `save` and `waypoint` remote ops,
  the `set_batch` op's `waypoint` argument, and every automatic
  `ChangeHistoryService:SetWaypoint` call (after each applied sync batch,
  each live write, `transmit` sources and the Executor) together with the
  `Helpers/setWaypoint` module. Studio auto-saves, and `undo` / `redo` drive
  its own history. `transaction_begin` / `transaction_finish` and clipboard
  paste keep their ChangeHistory recordings, so a daemon-driven bulk pull
  and a paste are still one Studio undo each

### Agent interface corrections

- Validate live tagged values against the generated canonical schema, with
  exact field examples and offline discovery hints for datatype mismatches.
- Preserve explicit return counts and nil slots across Roblox BindableEvent
  and BindableFunction boundaries; encode each return with a shared budget.
- Pin runtime requests and multipart reads to their playtest generation;
  enforce named-context readiness instead of accepting a different context.

### Fixed

- A malformed sync or details frame now triggers the resync it was meant to:
  its error was logged with `Log.error`, which throws, so the resync call
  after it never ran

- The connect upload reports what the daemon's three-way comparison needs:
  the SHA-256 of each Studio-side script's LF-normalized editor text on
  `update`/`remove` entries, a Studio-only root's synced descendants (bounded,
  flagged when truncated), and the place id. An empty diff uploads one empty
  chunk so the daemon records that the sides agree, and a `clean` receipt is
  accepted as committed
- After a connect that rewrote disk (`rehydrate` in the receipt), the plugin
  re-reads the daemon tree and re-pairs its instances before live sync
  starts, so scripts written to disk during the connect are no longer
  orphaned. Re-pairing an instance drops its previous id mapping instead of
  leaving a stale id that a later removal could hit

- A sync change for an instance ID the plugin did not know could disconnect
  the plugin (about one ID in 16): the warning printed the raw 16-byte ID,
  and a `%` byte in it made `Error.new`'s `gsub` raise "invalid use of '%'
  in replacement string" from inside the sync loop. Error and log text is now
  filled in one pass with a function replacement (`Util.interpolate`, also
  used by the transport's errors), so any argument text is safe; IDs always
  print as the 32-char lowercase hex the daemon logs; and log formatting
  never throws (`Util.safeStringify`). Removing an ID the plugin does not
  know is now a debug line (it is already gone from Studio), while updates
  for unknown IDs stay warnings, rate-limited with a per-batch summary
  (`plugin/tests/error-formatting.luau`)

- One bad change no longer costs the rest of its sync batch or the
  connection. Each addition, update and removal is applied on its own; a
  failure is logged with the change's hex ID (and instance path, when known)
  and skipped, and Core's sync loop logs an unexpected error in handling one
  message and reads on — only a deliberate session end (a declined change
  prompt, a project mismatch) still disconnects
  (`plugin/tests/sync-batch-isolation.luau`)

- A class change from disk replaces the instance in place instead of leaving
  a duplicate: adding `init.luau` to a Folder's directory, removing it, and
  renaming a script to another suffix (`init.server.luau` ↔ `init.luau`,
  `X.server.luau` ↔ `X.luau`) now swap the instance for one of the new class
  under the same ID, with every child moved across and the old instance
  destroyed. The swap no longer echoes back to disk: the tree follows the
  replacement before anything moves, and the children's move out of the old
  instance is announced to the read processor, which used to push it as
  their deletion (plus a fresh addition of the replacement). The daemon
  expresses these renames as one class-change update that keeps the ID,
  rather than a removal and an unrelated addition
  (`plugin/tests/class-change.luau`, `tests/class_change.rs`)

- Moving a folder or script with children to another place in Studio (or
  deleting one and undoing) no longer lands on disk without its children. A
  reparent reaches syncback as a removal followed by an addition of the same
  objects, and the removal only untracked the moved instance itself, so the
  addition skipped every still-tracked descendant as an echo — while the
  removal had already deleted their files. Removals now untrack the whole
  subtree (`Tree.removeSubtree`, used by the read processor and by
  daemon-applied removals alike), and the Watcher disconnects and rewires a
  moved subtree cleanly instead of leaking a second set of connections. The
  event → changes mapping moved from Core into `ReadProcessor.onEvent` so
  `plugin/tests/syncback-subtree.luau` exercises exactly what Core runs
  (`node scripts/check-plugin-syncback.mjs`)

- Connecting to a game the WSync app already has a project for no longer
  offers the two-step Create Project confirm when its daemon is down: the
  broker hello's `games` list marks the game as known, the card reads
  "'<place>' is already in your WSync Projects · daemon not running", and one
  "Serve Project" click adopts and serves the existing project

- The Not Connected card and the Create Project confirm prompt now show the
  published place name (via MarketplaceService) instead of Studio's internal
  edit-mode name ("Place7"); project folders created from the plugin are named
  after the real place
- Skipped the doomed MarketplaceService lookup for unpublished places
  (PlaceId 0)

### Added

- `sync_barrier` op (`wsync wait-synced`): queued behind the sync frames the
  transport already holds, like the transaction bracket, so Core's sync loop
  applies every earlier frame before it runs (a connect-time decision wait
  stashes it with those frames). It then waits out the script source writes
  still landing through `UpdateSourceAsync`, now counted by
  `Lib/Dom/SourceWrites`, and answers `{applied, settled,
  sourceWritesAwaited, pendingSourceWrites}`
- `verify_sources` / `verify_tree` ops (`wsync verify`, `Handlers/Sync`):
  per daemon ref, whether the instance is in Studio and whether its editor
  text (CRLF folded to LF) hashes to the disk text, plus editor drafts that
  differ from the committed `.Source`; and a paged walk of the synced roots
  listing Studio's scripts and same-named siblings under the bulk pull's
  scope and exclusion rules. Both are read-only and bounded (`capabilities`
  reports the limits)
- Playtest runs: the final `result`/`aborted` record carries every client
  failure reported during the run as `clientFailures` — failed client
  results with their tracebacks, and PlayClient error logs with repeats
  merged (bounded; `clientFailuresOmitted` counts the rest) — whatever the
  outcome, so a run that times out while the server waits still says which
  client broke and where. It also carries the `exitCode` and final
  `jobStatus` the docs already promised
- `eval` takes `drafts: true` (`wsync eval --drafts`): the chunk's `require`
  compiles every ModuleScript it reaches — directly or through other modules
  — from its editor text (`ScriptEditorService:GetEditorSource`; the
  committed `.Source` when there is no draft) and runs it with the real
  `script`, so a module WSync just wrote no longer requires as empty. Nothing
  is committed and no instance is created; `require` keeps its
  once-per-module, exactly-one-value and recursion rules. String requires,
  asset ids and unreadable modules use Studio's own `require`. The response
  adds `drafts: {modules, fromEditor?}` (`Remote/DraftRequire.luau`, tested
  under the standalone `luau` binary in `plugin/tests/eval-drafts.luau`)
- `AvoidSync` attribute (`SyncScope`): an instance of any class whose boolean
  `AvoidSync` attribute is `true` is kept in Studio with its whole subtree, on
  the same plumbing as `--!ignoresync` — never paired at connect, never
  snapshotted by live syncback, never walked by `read_subtree`. An Attributes
  change that sets it on a tracked instance pushes a removal; one that clears
  it pushes the subtree back as an addition. Daemon changes that arrive
  carrying either marker are untracked on arrival (Studio-only from the moment
  they land, nothing pushed back), and a live hydrate never pairs through, or
  stands a copy beside, a kept-in-Studio instance. The connect stage line
  counts both markers. Covered by `plugin/tests/exclusion-markers.luau`
- `--!ignoresync` script directive (`SyncScope`): a script carrying it on its
  own line within its first 10 lines is kept in Studio with its whole subtree —
  never paired with a file at connect, never snapshotted by live syncback,
  never walked by `read_subtree` (so its bytes never count against the
  transfer bounds). A Source edit that adds the directive to a tracked script
  pushes a removal (the daemon drops the disk copy); one that deletes it from
  a kept-out script pushes the script back as an addition. The connect stage
  line reports how many scripts the directive kept in Studio
- Forked from argon-roblox 2.0.23 and rebranded to WSync
- Transport-agnostic client layer (`src/Transport/`) with two implementations:
  the inherited HTTP long-poll path (msgpack) and a new `wsync/1` WebSocket
  path (`HttpService:CreateWebStreamClient`, JSON text frames, hello handshake,
  ping/pong heartbeat, typed shutdowns, exponential-backoff reconnect)
- Automatic transport selection via `GET /hello` protocol probe
- Daemon discovery (`src/Discovery.luau`): localhost port scan `7978–7990` with
  GameId/PlaceIds matching, surfaced on the Not Connected page
- Settings: `AutoDiscover` and `PreferAppModal` (the latter is display-only
  until the divergence coordinator ships)
- Capture engine (`src/Remote/Capture/`): `capture_prepare`/`capture_read`/
  `capture_close` render transparent `ui`, isolated or in-place `model`, and
  `viewport` captures as tightly-packed RGBA8 via CaptureService screenshots
  with EditableImage readback and dual-backdrop alpha recovery — protected
  Sky/Atmosphere evacuation, camera/UI restore, ≤2 sessions, 120 s sliding TTL
- Studio clipboard (`src/Remote/Handlers/Clipboard.luau`): `clipboard_copy`/
  `clipboard_read` serialize native instance trees (SerializationService,
  editor-source refresh) into a routed payload envelope;
  `clipboard_paste_begin/chunk/commit` assemble, SHA-verify, and paste it as
  one undoable change with original-parent routing (`to` override)
- Create Project flow: when no daemon matches but the desktop broker answers
  on `7968–7971`, the Not Connected page offers an explicit Create Project
  button — `POST /projects/init` with place metadata, then a bounded,
  cancellable wait for the new daemon
- `capabilities` now reports `limits.capture` and `limits.clipboard`
- Playtest op family (`src/Playtest/`, `src/Remote/Handlers/Playtest.luau`;
  playtest.json): `playtest_start/status/contexts/wait/stop` drive Studio
  Play/Run/Multiplayer jobs; runtime plugin copies inside the playtest
  DataModels boot a PluginConnectionService agent (never localhost) that
  authenticates every frame with the job's private generation token and
  appears as `server` / `client:N`; `playtest_exec/logs/ui/input/capture`
  reach into live contexts (temporary Script/LocalScript execution with the
  shared value codec, log ring slices, resolved UI records, validated virtual
  input, RGBA captures adopted into the shared capture session registry); and
  `playtest_run_start/run_poll/run_cancel` own the playscript run lifecycle —
  injected `playtest.*` API, ~20/s+burst emit rate limiting with accurate
  `dropped` records, 64 KiB payload and 1 MiB result-envelope caps, ~2 s
  heartbeats with a job-status re-check before `aborted`, first completion
  wins, and the 0/2/3/4/5 exit mapping
- `capabilities` now reports `limits.playtest`, and the boot gate
  (`init.server.luau`) routes playtest DataModel clones to the runtime agent
  instead of silently no-opping
- Transmit op family (`src/Remote/TransmitRules.luau`,
  `src/Remote/Handlers/Transmit.luau`; transmit.json): `transmit_prepare`
  reads pixels *out* of image-bearing instances — EditableImage directly,
  ImageLabel/ImageButton/Decal/Texture/MeshPart through their `Content`
  property (object-backed Content is read in place, otherwise
  `AssetService:CreateEditableImageAsync`) — via `paths`, or via `source`
  Luau run at the `eval` trust level (shared `Write.runSource`, bracketed and
  audited like `eval`). Every image is adopted as an ordinary capture session,
  so the existing `capture_read`/`capture_close` stream and drop it; a
  per-item failure (unresolved path, no image, oversize, decode failure) is
  reported in a parallel `failures` array instead of failing the batch
- `capabilities` now reports `limits.transmit`; the capture session registry
  gained per-origin allowances, so a 16-image transmit batch and the pinned
  2-session capture rule no longer compete for the same slots

- Default port `8000` → `7978`
- `TwoWaySync` default on, `OnlyCodeMode` default off (full-DataModel two-way
  sync is the WSync default)
