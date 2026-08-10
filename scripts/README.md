# scripts/

Build tooling and policy checks. Node >= 18, zero npm dependencies, no
`package.json` — every script is `node scripts/<name>.mjs` and nothing else.

Shared plumbing (argument parsing, repo-root anchoring, the file walker, the
error reporter) lives in `lib/policy.mjs`. Checked-in data the checks read lives
in `data/`.

Every check follows the same contract: `--help` explains itself, all problems
are collected and printed together rather than one per run, and a failure exits
1 with nothing modified on disk. None of them write to the repository.

## `build-command-docs.mjs`

Generates `docs/client-commands.md` and `docs/client-commands.generated.json`
from the command registry under `docs/commands/`. Not a check — the build step
the checks below defend. Owned separately; run it, do not edit it.

```sh
node scripts/build-command-docs.mjs
```

## `check-docs-drift.mjs`

Enforces that the two generated command-doc files are byte-identical to what the
registry currently produces. Design 10.5 makes `docs/commands/*.json` the single
source of truth, and the generated bundle is embedded verbatim in the binary and
rendered by the desktop Docs view — so a hand-edit of a generated file, or a
registry edit that was never regenerated, ships a command reference that
disagrees with itself. Because the generator resolves its own paths and always
writes in place, this check copies the registry and the generator into a
temporary workspace, runs the generator *there*, and compares; the repository is
never written to, and a failure names the file, the line and both versions of
it. A registry that fails the generator's own validation is reported with the
generator's output verbatim.

```sh
node scripts/check-docs-drift.mjs          # fix a failure with: node scripts/build-command-docs.mjs
node scripts/check-docs-drift.mjs --keep   # keep the temp workspace to inspect it
```

## `check-command-validation.mjs`

Reconciles three views of the CLI surface: the registry (`docs/commands/`, the
*designed* surface — 71 commands, deliberately ahead of the code), the manifest
(`data/implemented-commands.json`, the *built* surface), and optionally the real
binary. In classify mode — static, no execution, what CI runs today — it fails
when the manifest names a command the registry does not document, or when the
manifest is malformed, unsorted or duplicated; it *reports* every registry
command not yet implemented, which is expected while Design 15 phases 2–6 land
and is never a failure. It also warns when the manifest disagrees with `enum
Commands` in `src/cli/mod.rs`, so a newly landed command is noticed even without
a build. With `--bin` it additionally runs `<bin> --help`, parses the
`Commands:` block, and fails if the binary advertises anything the manifest or
registry does not have, or omits anything the manifest claims. Help output it
cannot parse is a failure, not a shrug — a cross-check that quietly stops
checking is worse than no cross-check.

**The manifest is hand-maintained: update `data/implemented-commands.json` in
the same change that lands a command.**

```sh
node scripts/check-command-validation.mjs
node scripts/check-command-validation.mjs --list-missing
node scripts/check-command-validation.mjs --bin target/release/wsync
node scripts/check-command-validation.mjs --json
```

## `check-heritage.mjs`

Fails when WSync's shipped surface names the parent tool (`rosync`, `ro-sync`,
`ro sync`) or uses the retired default port `7878` — the first is a user-visible
identity leak in a project that is a clean-slate remake (Design 1.4), the second
is a live wrong-port bug, since WSync serves 7978 (Design 3.2). It walks
`docs/`, `src/`, `plugin/src/`, `desktop/src/` in full plus `README.md`,
`NOTICE` and `CHANGELOG.md`; `Design.md` and `scripts/` are excluded outright
(the design document's job is to discuss both parents, and these scripts have to
spell the forbidden tokens out). Attribution is required rather than forbidden,
so legitimate lines are allowlisted in `data/heritage-allow.json` — but an entry
pins a path, a token *and* the exact text of the line, with an occurrence cap, so
an allowlisted file is not a blanket exemption. Entries that stop matching are
reported as warnings, which keeps the allowlist from rotting into one.

```sh
node scripts/check-heritage.mjs
node scripts/check-heritage.mjs --list              # show the allowlisted lines and why
node scripts/check-heritage.mjs --root some/dir     # scan an extra tree
```

## `check-luau-bytecode.mjs`

Compiles every `plugin/src/**/*.luau` at `-O0`, `-O1` and `-O2`. Studio loads
the plugin as bytecode built at `-O2`, and an optimisation level can reject code
the parser accepted (constant folding, upvalue analysis and inlining all see
things parsing does not), so checking one level checks the wrong thing. This is a
compile check, not a lint and not a type-check — it answers only "can Studio
load this". Files are discovered by walking the tree, never from a list, and
each level is compiled in one batched invocation; when a level fails, the files
are recompiled individually so the report names every bad file with the
compiler's own diagnostics.

The compiler is resolved in order: `--luau-compile <path>`, `$LUAU_COMPILE`,
`luau-compile` on `PATH`, a pinned local reference build, and otherwise a loud
SKIP with exit 0 so a contributor without the toolchain is not blocked. The
first two are explicit: if they name something that cannot be run, that is a
failure, never a skip. CI downloads and checksums its own `luau-compile`, passes
`--luau-compile`, and adds `--require-compiler` so the skip branch cannot be
reached silently.

```sh
node scripts/check-luau-bytecode.mjs
node scripts/check-luau-bytecode.mjs --luau-compile /path/to/luau-compile --require-compiler
```

## `check-release-identity.mjs`

Design 8.5 and 10.5 state the same rule from two directions: everything in a
release comes from one commit. This check is that assertion, run against *built*
artifacts rather than the tree they came from — the release workflow's `identity`
job runs it after every other job has produced something, and before anything is
published.

The interesting part is what a built binary can be asked with no daemon and no
Studio, which was settled by running the commands rather than assuming:

| command | offline? | what it gives |
| --- | --- | --- |
| `wsync version --raw` | **no** | connects first; exits 1 with "No WSync daemon answers on port 7978". Its subject is the daemon and plugin it talks to. |
| `wsync --version` | yes | `wsync <semver>` — the crate version. |
| `wsync commands` | yes | `docs/client-commands.generated.json` **verbatim** (`include_str!`, printed with `print!`), so it is compared byte for byte. |
| `wsync commands --compact` | yes | parses the same embedded bundle — catches a binary carrying bytes it cannot read. |

The build commit is **not** retrievable offline: `build.rs` stamps
`WSYNC_BUILD_COMMIT`, but only `GET /hello` reports it. So the commit is supplied
(`--commit`, `$GITHUB_SHA`, or `git rev-parse`), compared against what the plugin
build recorded, and then corroborated against the binary — `env!()` puts that
short commit in a live code path, so a binary built from it contains the string.
A miss fails; a hit is corroboration, and the output says so. The commit
`unknown` (what both stampers write outside a repository) matches only itself,
and skips the probe: the word appears in any binary a hundred times over.

Everything asserted: the binary runs and reports a version; its embedded docs
bundle is the checked-in one; the generated docs exist and have not drifted
(`check-docs-drift.mjs`, run as-is); one version across the binary, root
`Cargo.toml`, `pluginVersion` and `tauri.conf.json`; one protocol number across
`WSync.build.json`, `src/constants.rs`, `plugin/src/Remote/init.luau` and the
desktop host's `wsync/N`; the artifact matches its manifest's sha256; one commit
everywhere; and nothing built dirty unless `--allow-dirty`.

```sh
node scripts/check-release-identity.mjs --bin target/release/wsync
node scripts/check-release-identity.mjs --bin target/debug/wsync --allow-dirty   # local tree
node scripts/check-release-identity.mjs --bin dist/wsync \
  --plugin-manifest dist/WSync.build.json --commit "$GITHUB_SHA"
```

## `release/`

### `release/make-latest-json.mjs`

Writes the Tauri v2 updater manifest (`latest.json`, Design 8.5) from the
bundles a release job just built. Not a check — a build step — but it refuses to
write a manifest the updater would reject, which is most of what it does.

The schema is the "static" shape `tauri-plugin-updater` deserializes: `version`
(semver, leading `v` stripped), optional `notes`, optional RFC 3339 `pub_date` —
an unparseable one fails the *whole* manifest client-side — and `platforms`
keyed `{os}-{arch}[-{installer}]`. That vocabulary is fixed by the plugin's own
`updater_os()` / `updater_arch()` / `Installer::name()`, so `macos-arm64` is
rejected here instead of becoming an update nobody is offered.

Each `<bundle>.sig` is validated the way the client will read it: base64 of a
four-line minisign signature file whose second line decodes to 74 bytes and
whose fourth decodes to 64. A missing `.sig` is a failure, never a silently
unsigned entry — an unsigned build must not be published through the updater.
This script never signs anything and never sees a key.

```sh
node scripts/release/make-latest-json.mjs \
  --version v0.1.0 \
  --base-url https://github.com/<owner>/wsync/releases/download/v0.1.0 \
  --platform darwin-aarch64=bundle/macos/WSync.app.tar.gz \
  --notes-file notes.md --out latest.json
```

## `data/`

| file | what it is |
| --- | --- |
| `implemented-commands.json` | Top-level commands actually implemented in the binary. Hand-maintained; update it when a command lands. |
| `heritage-allow.json` | Lines permitted to name the parent tool, each with the reason it is legitimate. |

## CI

`.github/workflows/ci.yml` runs these in the `docs` job (drift, command
validation in classify mode, heritage) and the `plugin` job (bytecode, against a
pinned `luau-compile` release verified by sha256). To re-pin the compiler, pick
a tag from <https://github.com/luau-lang/luau/releases> and read the asset digest
straight from the API rather than computing it locally:

```sh
gh api repos/luau-lang/luau/releases/tags/<tag> --jq '.assets[] | "\(.name) \(.digest)"'
```

then update `LUAU_VERSION` and `LUAU_UBUNTU_SHA256` in the workflow's `env:`
block.

`.github/workflows/release.yml` runs on `v*` tags (and on demand, which does
everything except publish). `plugin` → `engine` → `desktop` → `identity` →
`publish`: the engine embeds the plugin the release ships (`--features plugin`),
the desktop app packages that engine as its sidecar and the same plugin pair as
app resources, `identity` runs `check-release-identity.mjs` against every
downloaded artifact, and only then does `publish` draft a GitHub release with
`latest.json` attached. Its downloaded tools are pinned the same way — aftman,
`cargo-binstall` and `tauri-cli` by version, the first two by sha256 as well, and
the `tauri-cli` pin is re-asserted after installation so it holds whichever
install path ran. Without a signing key the desktop build still happens, is
labelled a prerelease, and publishes no `latest.json`.
