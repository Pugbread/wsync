# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), that adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

History prior to the fork lives in
[argon-roblox's changelog](https://github.com/argon-rbx/argon-roblox/blob/main/CHANGELOG.md)
(forked at 2.0.23, Apache-2.0).

## [0.1.0] - Unreleased

### Fixed

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
