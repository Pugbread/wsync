# WSync Desktop

The Tauri 2 shell for WSync (Design §8). A framework-free ES-module frontend in
this directory, a narrow Rust host in `src-tauri/`.

The shell, the state store, the theme engine, the project registry, the daemon
lifecycle, the live event feed, Design §7.0's disk review (banner + picker) and
the full-scope divergence choice behind it, conflict resolution, the staging
list's inline row diffs, the last-edited store, the Open Cloud API key store,
**and the Studio plugin install** are real. The one
thing that still answers `not_implemented` is the daemon's secret handoff — see
[What works today](#what-works-today).

The daemon itself is built in the workspace root. You do not need it to run the
app: [a mock daemon](#running-against-the-mock-daemon) implements the wire
contract and ships in `scripts/`.

## Layout

```
desktop/
  src/                  the frontend, and the whole of `frontendDist`
    index.html          app shell: rail, topbar, #view, statusbar, overlay roots
    style.css           one hand-written stylesheet; colour lives in tokens only
    app.js              route registry, persisted store, the shared `api` object
    bridge.js           the only module that touches Tauri
    ws.js               the reconnecting WebSocket link to one project daemon
    views/
      theme.js          system | dark | black | light token maps
      dom.js            the small `el()` helper every view builds DOM with
      projects.js       card list + detail pane, serve toggle, error card
      active.js         the live activity feed and its allowlist formatter
      conflicts.js      parked conflicts + the review / divergence banners
      diff.js           the line differ and the property-table diff
      last-edited.js    bounded per-project path → last-change ledger
      docs.js           renders docs/client-commands.generated.json
      settings.js       appearance, projects folder, secrets, plugin, about
      review.js         the disk review — banner + picker (Design §7.0)
      staging.js        the two-pane staging list both reviews render with
      overwrite.js      the two-step divergence modal (Design §7.3, full scope)
  scripts/
    make-icons.mjs      regenerates src-tauri/icons/ from code
    mock-daemon.mjs     a zero-dependency stand-in for `wsync daemon start`
    mock-daemon.test.mjs  drives it over real sockets
    wsync-mock          shim: point WSYNC_DAEMON_PATH here
  src-tauri/
    Cargo.toml  build.rs  tauri.conf.json
    capabilities/main.json    the entire webview privilege list
    binaries/                 the wsync sidecar is staged here at build time
    src/
      main.rs lib.rs          entry point, plugin + command registration
      commands.rs             the whole IPC surface
      daemon.rs               sidecar lifecycle: spawn, registry, heartbeat, stop
      loopback.rs             the small HTTP/1.1 client used to reach 127.0.0.1
      storage.rs error.rs     state.json, and the typed error crossing IPC
```

`src/` + `src-tauri/` is the standard no-bundler Tauri layout, and here it is
load-bearing rather than cosmetic: `tauri::generate_context!` embeds **every
file** under `frontendDist` with no ignore list of any kind. Pointing it at
`desktop/` would compress `src-tauri/target/` — tens of gigabytes — into the
binary on every build. `frontendDist` must contain the frontend and nothing
else.

## Dev loop

Prerequisites:

- Rust stable (built and checked against 1.95; `rust-version = 1.82`)
- The Tauri 2 system dependencies for your platform —
  <https://tauri.app/start/prerequisites/>. macOS needs Xcode command line tools;
  Linux needs `libwebkit2gtk-4.1-dev`, `libgtk-3-dev`, `libayatana-appindicator3-dev`,
  `librsvg2-dev`.
- The Tauri CLI: `cargo install tauri-cli --version "^2"` (or `cargo binstall tauri-cli`)

Then:

```sh
cd desktop/src-tauri
cargo check          # type-check the host
cargo test           # state, the loopback client, and the daemon lifecycle
cargo tauri dev      # run the app
```

`cargo test` covers the ready-line parser, owner-token generation, the
stop-fallback ordering, the route allowlists, the secret store, and the HTTP
framing — plus end-to-end tests that spawn real child processes: the mock daemon
(skipped when Node is absent) and small scripted `sh` daemons for the refusal,
adoption and no-ready-line paths.

A plain `cargo test` **never touches your keychain**: the secret store takes its
backend as an explicit parameter, and the tests pass either "no keychain" or a
stub binary that records what it was handed — which is how the "the value is
never in argv" guarantee is asserted rather than asserted-about. The live macOS
round trip is opt-in, because `security` cannot be pointed at a throwaway
keychain in prompt mode (see the header of `src/secrets.rs`) and so must use the
default one:

```sh
WSYNC_KEYCHAIN_TESTS=1 cargo test          # writes + deletes a `wsyncTestKey` item
```

Without the variable that one test prints why it did nothing instead of silently
passing.

The frontend has no test runner. `node --check` is the parse check, but note it
only treats a file as an ES module when the extension is `.mjs`, so a `.js`
frontend module has to be checked as a copy:

```sh
for f in src/*.js src/views/*.js; do
  cp "$f" "/tmp/$(basename "$f").mjs" && node --check "/tmp/$(basename "$f").mjs" || echo "FAILED $f"
done
node --check scripts/*.mjs
```

There is **no frontend build step and no `node_modules`**. `tauri.conf.json`
points `frontendDist` at `../src`, so `cargo tauri dev` serves `index.html`
as-is and a reload picks up edits to any `.js`/`.css` file.

`withGlobalTauri` is on, which is what lets `bridge.js` reach `invoke` through
`window.__TAURI__.core` without a bundler. `bridge.js` is the only file allowed
to touch it.

### Previewing the UI in a plain browser

Every view degrades cleanly with no host: host calls reject with
`code: "no_host"`, the store runs in memory, and the app says so in the status
bar. Useful for layout work:

```sh
cd desktop/src && python3 -m http.server 8731
# then open http://127.0.0.1:8731/index.html
```

`http://127.0.0.1:8731/index.html?dev=overwrite` opens the divergence modal on
its fixture. The same fixture is reachable from Settings → Developer.

The Docs view fetches `../../docs/client-commands.generated.json`, which only
resolves when something serves the repository root — expected, and it renders a
"not generated yet" state otherwise.

## Running against the mock daemon

`scripts/mock-daemon.mjs` is a stand-in for the engine: zero dependencies,
Node 18+, and it implements the parts of the contract the desktop touches — the
spawn handshake, `/hello`, `/stop`, `/manager-heartbeat`, `/manager-close`, the
conflict routes (`GET`/`POST /resolve`), Design §7.0's disk review (`GET
/review`, `GET /review/details`, `POST /review/push`, `POST /review/dismiss`),
the whole divergence choice lifecycle (`GET /choice`, `GET /choice/details`,
`GET /choice/source`, `POST /choice`, `POST /choice/selection` with receipts),
and a hand-rolled RFC 6455 WebSocket at `/ws` that sends the server hello,
pings, and a scripted `sync-activity` / `plugin-status` / `conflict` /
`disk-review` / `choice-needed` / `choice-made` event stream. Its
`sync-activity` frames carry `direction`, `counts` and `names` — and one of them
names paths from the *live* set, so the desktop's last-edited store fills with
stamps for rows the picker will actually list. It is enough to exercise serve →
connect → live feed → resolve → diff → decide → stop end to end.

It models **both connect surfaces**, because Design §7.0 left both live. By
default a connect is code scope: the daemon has already applied Studio → disk,
so it raises a passive `disk-review` and `/choice` reports nothing pending.
`--full-scope` is the pre-7.0 blocking flow a `scope: "full"` project still
gets. `--divergence n` sizes whichever set the scope selects, and reconnecting
**replaces** a pending review — the same thing the real daemon does, since a
connect is itself a sync.

`scripts/wsync-mock` is the shim that makes it look like the `wsync` binary:

```sh
cd desktop/src-tauri
WSYNC_DAEMON_PATH="$PWD/../scripts/wsync-mock" cargo tauri dev
```

Add a project, flip its serve toggle, and the Activity view fills up. Extra
mock-only switches go through an environment variable, so the host's argv stays
exactly what it sends the real engine:

```sh
# an engine that predates the conflict engine (404 on /resolve)
WSYNC_MOCK_ARGS="--no-resolve" WSYNC_DAEMON_PATH=… cargo tauri dev

# a fast event stream, for watching the rAF batching work
WSYNC_MOCK_ARGS="--event-interval 200" WSYNC_DAEMON_PATH=… cargo tauri dev

# a daemon that refuses to start — renders the project-error card
WSYNC_MOCK_ARGS="--fail 'port 7978 serves another project'" WSYNC_DAEMON_PATH=… cargo tauri dev

# a daemon that was already running — the adopt path, which WSync never kills
WSYNC_MOCK_ARGS="--already-running" WSYNC_DAEMON_PATH=… cargo tauri dev

# six parked conflicts, cycling every archetype the Conflicts view renders:
# script + property, both-edited and each one-sided deletion, one truncated
WSYNC_MOCK_ARGS="--conflicts 6" WSYNC_DAEMON_PATH=… cargo tauri dev

# a disk review over 1500 paths — the default surface, enough to page it
WSYNC_MOCK_ARGS="--divergence 1500" WSYNC_DAEMON_PATH=… cargo tauri dev

# a push answered with a `remaining` that cannot be true: the picker must
# refuse the receipt and report that nothing further was pushed
WSYNC_MOCK_ARGS="--divergence 40 --bad-remaining" WSYNC_DAEMON_PATH=… cargo tauri dev

# the pre-7.0 blocking choice a full-scope project still gets
WSYNC_MOCK_ARGS="--divergence 1500 --full-scope" WSYNC_DAEMON_PATH=… cargo tauri dev

# a receipt that does not match what was sent: the modal must abort, not apply
WSYNC_MOCK_ARGS="--divergence 5000 --full-scope --bad-receipt 1" WSYNC_DAEMON_PATH=… cargo tauri dev

# the choice is answered by someone else between reading it and writing it
WSYNC_MOCK_ARGS="--divergence 1500 --full-scope --resolved-elsewhere" WSYNC_DAEMON_PATH=… cargo tauri dev

# no Studio attached: /choice/source answers 503 and a row diff has to say so
WSYNC_MOCK_ARGS="--divergence 40 --full-scope --no-plugin" WSYNC_DAEMON_PATH=… cargo tauri dev
```

Every 25th row of a generated divergence set is a script larger than the 256 KiB
transfer ceiling, so `--divergence 40 --full-scope` reaches a **truncated** diff
pair at row 1 and a **non-script** row (answering 400) at row 9 — the three
interesting row-diff states are all on the first page. In a review set every
fourth disk-only row has a null `instancePath`, which is the file Studio has
never seen.

Run it standalone, or run its tests:

```sh
node scripts/mock-daemon.mjs daemon start --project /tmp/demo --raw
node scripts/mock-daemon.test.mjs
```

The test file drives the mock over real sockets with its own hand-written
WebSocket client, so a framing mistake on either side fails there rather than in
the app.

`WSYNC_DAEMON_PATH` also points at a real engine build:

```sh
cargo build -p wsync
WSYNC_DAEMON_PATH="$PWD/target/debug/wsync" cargo tauri dev
```

There is deliberately no fallback guess at `../target/debug`: without the
variable and without a bundled sidecar, serving fails with an error that says
what to set. A silently-wrong engine is worse than a refusal.

### Icons

`src-tauri/icons/` is generated, not hand-drawn:

```sh
node scripts/make-icons.mjs
```

Packaging for macOS and Windows additionally needs `icon.icns` and `icon.ico`,
which are **not** generated yet — a release-wave task.

## Sidecar

`bundle.externalBin` declares `binaries/wsync`, so Tauri expects
`src-tauri/binaries/wsync-<target-triple>` (plus `.exe` on Windows) to exist at
build time and copies it next to the app. That binary is the WSync engine built
from the repo root; it is **never committed** — `binaries/` is gitignored apart
from `.gitkeep`.

Stage it before packaging:

```sh
cargo build --release -p wsync
cp target/release/wsync desktop/src-tauri/binaries/wsync-$(rustc -vV | sed -n 's/host: //p')
```

Until the engine exists, `build.rs` stages an **empty placeholder** for debug
builds so the shell still compiles and runs, and prints a `cargo:warning`
saying so. Release builds refuse to substitute a placeholder: shipping an
installer wrapped around a fake sidecar would be worse than a failed build.

`daemon.rs` treats a zero-byte sidecar as no sidecar, so the placeholder is
never spawned; the error tells you to set `WSYNC_DAEMON_PATH` instead.

### The plugin artifact

The Studio plugin ships as a resource beside the binary. `bundle.resources` maps
the **directory** `src-tauri/resources/` onto the root of the app's resource
directory (`WSync.app/Contents/Resources` on macOS, beside `WSync.exe` on
Windows), which is where `plugin_install.rs` looks for `WSync.rbxm` and reads
`WSync.build.json` from.

`build.rs` fills that directory, and what it puts there depends on the profile:

- **Release** stages `plugin/WSync.rbxm` + `plugin/WSync.build.json` and refuses
  the build unless both exist, the artifact is non-empty, and the manifest parses,
  names `WSync.rbxm` and carries a usable sha256. A manifest-less artifact
  installs in a `verified: false` state — reasonable for a hand-built dev tree,
  not a thing to wrap an installer around. Anything *else* found in the directory
  also fails the build: everything in it is bundled, so a stray file there is a
  file inside the shipped app.
- **Debug** stages nothing and clears anything a release build left behind.
  Resources outrank the dev-tree lookup on purpose, so a stale copy would shadow
  the `plugin/WSync.rbxm` you just rebuilt — in a checkout, the checkout wins.

The directory form is deliberate: an *empty* directory is skipped by Tauri's
resource resolver, while a named file that does not exist is a hard
`ResourcePathNotFound`, and `tauri_build::build()` resolves resources on every
`cargo check`. Empty-and-ignored is what keeps a fresh clone building.

So a dev build finds `plugin/WSync.rbxm` by walking up from the working
directory, or takes an explicit override:

```sh
node plugin/scripts/build.mjs                        # produces the pair
WSYNC_PLUGIN_ARTIFACT="$PWD/plugin/WSync.rbxm" cargo tauri dev
```

Note the precedence is the opposite of `WSYNC_DAEMON_PATH`'s: app resources win
over the environment variable. Pointing the daemon at another binary affects
this app's own child process; pointing this at another `.rbxm` writes it into
Roblox Studio, where it outlives the app — so a packaged install prefers the
artifact it shipped with.

## Daemon lifecycle

`src-tauri/src/daemon.rs` owns every daemon this app starts (Design §3.2–3.3).

**Spawn.** One long-lived child per served project — the child *is* the daemon:

```
wsync daemon start --project <path> [--port <p>] --managed-by desktop \
  --owner-token-env WSYNC_OWNER_TOKEN --data-dir <app data dir> --raw
```

The owner token is 32 CSPRNG bytes, hex, generated per serve and handed over
**only** through that environment variable — never argv, never IPC, never
`state.json`. The child's first stdout line is parsed with a 20 s timeout; a
`{"ok":false,…}` line keeps the daemon's own message, and anything else is
reported as a protocol violation with the child's stderr tail attached.

**Adoption.** `alreadyRunning: true` means a matching daemon already existed and
the child exits at once. That session is recorded so the UI can drive it, with
`managed: false` — no handle is held, and stopping it is a request that never
escalates to a kill. Another desktop or a terminal may own that process.

**While served.** A heartbeat task POSTs `/manager-heartbeat` every 60 s; three
consecutive failures raise `daemon:down` with reason `heartbeat-lost`. A
supervisor task reaps the child and raises `daemon:down` if it exits on its own.

**Stop.** `POST /stop {bootId, token}`, a 3 s grace period, and only then the
held handle as a fallback. On app exit the same registry is swept with
`/manager-close` first — every held handle dies with the app.

Lifecycle transitions reach the webview as Tauri events (`daemon:up`,
`daemon:down`), so a daemon that dies on its own updates the UI immediately
rather than at the next poll.

`loopback.rs` is the ~200-line HTTP/1.1 client behind all of this. It exists
instead of a general HTTP stack because every call is plain HTTP to
`127.0.0.1`: no TLS to configure, no connection pool to hand a socket belonging
to a daemon that has since exited on the same scanned port, and — importantly —
no `Origin` header, which is what puts these requests on the daemon's native
loopback path rather than its browser-origin one (Design §5.2).

## The daemon WebSocket

`src/ws.js` holds one socket, retargeted as the served project changes. It
sends the `role: "app"` hello, answers the daemon's `ping` with `pong`,
subscribes to the app event topics, and reconnects with exponential backoff
(1 s → 30 s, jittered) — except after a `shutdown` frame with
`retryable: false`, which parks the link until something gives it a new target.
A socket that goes 30 s without a frame is treated as dead, so a laptop that
slept through a daemon's exit does not sit on a socket that will never speak.

`event` frames land on the app bus and render in the Activity view through a
sanitizing allowlist (category, tone, title, intent, bounded facts, duration,
plus the raw frame behind a collapsible), batched on `requestAnimationFrame`.

## Updater

Signed updates fail closed (Design §8.5). The ed25519 public key is compiled in
from the environment:

```sh
WSYNC_UPDATER_PUBLIC_KEY="$(cat updater-key.pub)" cargo tauri build
```

Without it the updater plugin is **not registered at all**, `app_info` reports
`updaterConfigured: false`, and the frontend hides every update affordance.
There is no code path that installs an unverified artifact.

The endpoint in `tauri.conf.json` is a placeholder
(`https://github.com/OWNER/wsync/releases/latest/download/latest.json`) — point
it at the real repository before the first release, and add the
fingerprint-pinning check described in §8.5.
`scripts/check-release-identity.mjs` warns while that placeholder is still
there, because a shipped build would look for `latest.json` at a repository that
does not exist.

## Release

`.github/workflows/release.yml` packages this app; `desktop/` has no release
script of its own. On a `v*` tag it builds the plugin, builds the engine for
macOS arm64 and Linux x86_64, then on macOS:

1. downloads the engine artifact and installs it as
   `src-tauri/binaries/wsync-aarch64-apple-darwin` (Tauri appends
   `-<target triple>` to each `bundle.externalBin` entry);
2. downloads the plugin pair into `plugin/`, from where `build.rs` stages it
   into `src-tauri/resources/`;
3. installs a pinned `tauri-cli` and runs
   `cargo tauri build --config '{"bundle":{"createUpdaterArtifacts":true}}'`.

`createUpdaterArtifacts` lives in the workflow rather than in `tauri.conf.json`
because it is a property of a release build, not of the app: it defaults to
false, and it is what makes `tauri build` emit the `WSync.app.tar.gz` and
`.sig` the updater consumes.

Signing is both-or-neither. With `TAURI_SIGNING_PRIVATE_KEY` **and**
`WSYNC_UPDATER_PUBLIC_KEY` set as repository secrets, the bundle is signed and
the app carries the matching public key. With neither, the build still happens,
the app reports `updaterConfigured: false`, no `latest.json` is published, and
the release is marked a prerelease that says so. With exactly one of them the
job fails — a private key with no public key signs updates nobody can verify,
and a public key with no private key ships an app waiting for updates nobody
will sign.

Nothing is published until `scripts/check-release-identity.mjs` has agreed that
the engine binary, the plugin pair, the protocol number, the generated docs and
this app's `tauri.conf.json` version all come from the tagged commit (Design
§8.5). Releases are drafts by default.

To package locally, do what the workflow does:

```sh
node plugin/scripts/build.mjs
cargo build --release -p wsync
cp target/release/wsync desktop/src-tauri/binaries/wsync-$(rustc -vV | sed -n 's/host: //p')
cd desktop/src-tauri && cargo tauri build
```

## The privilege surface

The webview has `core:default` plus window drag-start, and nothing else: no
shell, no filesystem, no HTTP plugin (`capabilities/main.json`). Every
privileged operation is a named Rust command in `src/commands.rs`:

| Command | State |
|---|---|
| `app_info` | real |
| `state_get` / `state_set` | real — atomic writes to `<data dir>/WSync/state.json` |
| `pick_project_folder` | real — native dialog, canonicalized, dir-checked |
| `daemon_start` | real — spawns the sidecar, holds the handle, starts the heartbeat |
| `daemon_stop` | real — `/stop` with boot id + token, grace period, kill as fallback |
| `daemon_status` | real — registry record reconciled against a live `/hello` |
| `daemon_request` | real — GET on an exact-path allowlist against a tracked project's daemon |
| `daemon_post` | real — POST on a narrower allowlist, with a per-route body cap |
| `plugin_install` | real — resolves `WSync.rbxm`, verifies its sha256, installs it atomically |
| `plugin_status` | real — what is installed, whether it is this build's, protocol vs. the daemon |
| `plugins_dir_reveal` | real — argument-free reveal of the Roblox plugins folder |
| `secret_set` / `secret_get` / `secret_clear` | real — OS keychain, `0600` file fallback |
| `secret_export_env` | reserved → `not_implemented` |

`daemon_request` is the narrowest thing that lets the webview's data layer read
the engine: **GET only**, and only `/hello`, `/resolve`, `/choice`,
`/choice/details`, `/choice/source`. It exists because the owner token has to
stay in Rust — a browser-origin request would need it, and a native loopback
client does not. It is not a proxy: no arbitrary host, no arbitrary route, no
write methods.

`/choice/source` is the newest row on that list: one divergence row's two sides
(≤256 KiB each), read lazily when a `differs` row is expanded in the staging
list. It reads a frozen set and cannot change anything, which is why it is on
the read side and not the write one.

`daemon_post` is the write half, and it is narrower still:

| Route | Body | Cap |
|---|---|---|
| `/resolve` | `{id, path?, keep, choice}` | 4 KiB |
| `/choice` | `{choiceId, choice, mode?}` | 4 KiB |
| `/choice/selection` | `{choiceId, submissionId, chunkIndex, finalChunk, restart?, ids[]}` | 80 KiB |

Exact path, **no query string**, a JSON object body, and the cap checked before
a socket is opened. `/push`, `/pull/stream`, `/compare`, `/projects/init` and
`/stop` are not reachable from the webview at all. Non-2xx comes back as data
rather than as an error, because the divergence flow has to *read* a 409 to
learn the choice was resolved elsewhere and a 404 to learn a conflict id is
already gone.

Adding a capability means adding a command here, not widening the capability
file.

## What works today

Real:

- adding a project through the native picker; the registry (persisted); search,
  filters, selection, two-click removal
- the appearance engine end to end; the projects-folder authorization; the docs
  view against a generated registry
- **daemon lifecycle** — serve/stop, adoption of an already-running daemon,
  per-project heartbeat, kill-on-app-exit, and `daemon:up`/`daemon:down`
  reaching the UI
- **the live activity feed** — the WebSocket link, reconnection, the sanitizing
  formatter, rAF batching, and the state pill
- **the conflicts view** — `/resolve` polled every 20 s and invalidated by
  `conflict` events (including the honest "this engine has no `/resolve`"
  state), class-aware cards (line diff for script conflicts, property table for
  property conflicts), per-card and bulk `POST /resolve`
- **the divergence choice** — `GET /choice` for the stats, `GET /choice/details`
  paged progressively with every page verified, chunked `POST /choice/selection`
  with a checked receipt per chunk, and `POST /choice` for the decision;
  `choice-needed` / `choice-made` events drive the banner and close a modal
  whose choice was answered elsewhere
- **the staging list's inline row diffs** — a `differs` row expands into a
  two-pane diff fetched lazily from `GET /choice/source`, cached per row for the
  modal's lifetime, one row open at a time, with distinct states for loading, a
  truncated pair, a non-script row (400), a disconnected plugin (503), and a
  set that was replaced underneath (404 → the modal's supersede path). Expanding
  is a read: it never stages, unstages or reorders anything
- **the last-edited store** — `views/last-edited.js`, a per-project ledger of
  path → last-change time fed by the `names` on `sync-activity` events and
  persisted under its own state key. Bounded at 400 paths per project and 8
  projects with LRU eviction on both, and it is what makes the divergence
  modal's "Recently edited" sort real: rows the ledger knows carry an
  `edited 4m ago` label and sort newest first, rows it does not sort last
- **secrets** — the Open Cloud API key in the macOS keychain (via `security`,
  with the value on stdin so it is never in `argv`) and a `0600`
  `secrets.json` fallback written atomically. `secret_get` answers with
  presence, a masked tail (`…abcd`) and which store holds it; the key itself
  never crosses back to the webview
- **the Studio plugin install** — `src-tauri/src/plugin_install.rs` resolves
  `WSync.rbxm` from the app's own resources, then `WSYNC_PLUGIN_ARTIFACT`, then
  a checkout's `plugin/WSync.rbxm`, then a file the user picks; checks it
  against the sha256 in the `WSync.build.json` beside it and **refuses** a
  mismatch outright; and copies it into `~/Documents/Roblox/Plugins`
  (`%LOCALAPPDATA%\Roblox\Plugins` on Windows) by temp+rename, removing an
  older `WSync.lua`/`WSync.luau` on the way. An artifact with no manifest
  installs into a reported "unverified" state rather than silently claiming
  verification. `plugin_status` hashes what is installed to establish its
  identity, and flags a plugin whose protocol disagrees with the running
  daemon's `/hello`

Real, but honest about a daemon-side gap:

- **Keep Studio** records the decision and reports
  "Decision recorded — Studio → disk transfer lands in a later build". The
  daemon answers `{ok:true, applied:false, pendingApplication:true}` because
  Design §7.4-A's fenced push is not built yet, and the modal says exactly that
  rather than claiming files moved.

Stubbed, and visibly so in the UI:

- `secret_export_env`: the command name is reserved so the daemon handoff has
  one settled place to live, and it answers `not_implemented` rather than
  returning the key to the webview so the webview could pass it along
