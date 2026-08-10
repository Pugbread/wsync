# WSync Studio Plugin

The WSync Roblox Studio plugin syncs the full DataModel — instances, properties,
attributes, and tags — two ways between Studio and your local file system,
talking to a `wsync` daemon over WebSocket (protocol `wsync/1`), with an HTTP
long-poll fallback for Studio builds without `CreateWebStreamClient`.

Part of the [WSync](../Design.md) project. The plugin discovers a matching
daemon automatically (localhost port scan `7978–7990`, GameId match), or
connects to an explicit host and port.

## Attribution

Forked from [argon-roblox](https://github.com/argon-rbx/argon-roblox)
(© Dervex and contributors), licensed under the
[Apache License 2.0](LICENSE.md). The Fusion UI, sync core (Dom, Processor,
Tree, Watcher), and Config system originate there; WSync replaces the
transport layer, adds daemon discovery, and rebrands the surface.

Toolbar/state icon assets are still Argon's placeholder art
(TODO(phase-3-polish): replace with WSync artwork).

## Build

Prerequisites: Node 18 or newer, and a toolchain manager — [aftman] or [rokit].
`aftman.toml` pins the versions: rojo 7.5.1 packages the artifact, wally 0.3.2
installs the packages the project tree references.

```sh
scripts/build          # from plugin/ — a wrapper around the line below
node scripts/build.mjs # the build itself; --help lists everything
```

The build resolves the toolchain, runs `wally install`, checks that every
`$path` in `default.project.json` exists, runs `rojo build`, and writes two
files:

| File | Committed? | What it is |
| --- | --- | --- |
| `WSync.rbxm` | no, git-ignored | the plugin Studio loads |
| `WSync.build.json` | yes | the artifact's sha256, plugin version, protocol number and build commit |

`rojo` and `wally` are looked for in this order: `$WSYNC_ROJO` / `$WSYNC_WALLY`,
`$PATH`, `~/.rokit/bin`, `~/.aftman/bin`, `~/.cargo/bin`, then `aftman install`
(or `rokit install`) from `aftman.toml` — and, only for rojo and only if all of
that failed, `cargo install rojo@7.5.1 --locked`, which compiles from source and
takes minutes. Being on `$PATH` is not enough on its own: aftman and rokit put
trampolines there, and a trampoline for a tool `aftman.toml` does not list
cannot run, so each candidate is asked for its version before it is used.

### Installing it into Studio

From the repository root:

```sh
node plugin/scripts/build.mjs && wsync plugin install
```

`wsync plugin install` takes `plugin/WSync.rbxm` from the current workspace
first, then a `WSync.rbxm` sitting next to the `wsync` binary, and finally the
copy compiled into the binary — the engine's `build.rs` embeds
`plugin/WSync.rbxm` when the engine is built with `--features plugin`, and warns
loudly when there is nothing to embed. Restart Studio afterwards;
`wsync plugin status` compares what is installed against the source by SHA-256.

### `--check`

```sh
node plugin/scripts/build.mjs --check
```

Rebuilds to a temporary file and diffs its sha256 against `WSync.build.json`,
leaving the artifact and the manifest alone. It fails when the manifest is
missing or describes a different build — that is, when the plugin sources
changed and nobody rebuilt. CI runs it *before* its own build, against the
manifest exactly as committed; run afterwards it would only compare a build
with itself.

The fix for a failure is always the same: `node plugin/scripts/build.mjs`, then
commit `WSync.build.json`. Note that `wally.lock` is git-ignored, so CI
re-resolves the version ranges in `wally.toml` on every run — a dependency
publishing a patch release changes the artifact and fails `--check` until the
manifest is rebuilt. The resolved package versions are printed above the diff.

### Offline

`WSYNC_SKIP_WALLY=1` (or `--skip-wally`) skips `wally install` and builds
against the `Packages/` already on disk. A `wally install` that cannot reach the
registry is not fatal either — the build falls back to the installed packages
with a warning, and only stops if a package the project actually needs is
missing.

## Editor tooling

```sh
scripts/install   # wally install + sourcemap + package types for luau-lsp
```

[aftman]: https://github.com/LPGhatguy/aftman
[rokit]: https://github.com/rojo-rbx/rokit
