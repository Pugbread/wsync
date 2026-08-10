#!/usr/bin/env node
// build.mjs — builds the WSync Studio plugin into a loadable WSync.rbxm.
//
// The plugin tree *is* a Rojo project (plugin/default.project.json), so the
// build is short: resolve a pinned toolchain, install the wally packages the
// project tree references, check that every $path in the project file exists,
// run `rojo build`, and record what came out in plugin/WSync.build.json.
//
// The manifest is the point of this script. `wsync plugin install` and the
// engine's build.rs both pick up plugin/WSync.rbxm silently, so "which plugin
// is in Studio" is otherwise unanswerable: the .rbxm is a binary blob with no
// version inside it that a human can read. WSync.build.json pins the artifact's
// sha256 next to the plugin version, the protocol number it speaks, and the
// commit it was built from — and `--check` rebuilds and diffs that sha, so a
// manifest committed before an unrelated source change fails CI instead of
// quietly describing a plugin nobody has anymore.
//
// Deliberately standalone: Node >= 18, zero npm dependencies, and no import
// from the repo-root scripts/lib. The plugin directory carries its own
// aftman.toml, wally.toml and licence and is built by people who are not
// building the engine; a build that breaks because a file two directories up
// moved is a build that breaks for the wrong reason.
//
//   node plugin/scripts/build.mjs            build the artifact and manifest
//   node plugin/scripts/build.mjs --check    rebuild to a temp file, diff the sha
//
// Exit 1 on any failure; the artifact and the manifest are only written after
// the build succeeds.

import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { mkdtemp, readFile, rm, stat, writeFile } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const NAME = "build";
const pluginDir = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const repoRoot = path.resolve(pluginDir, "..");

const PROJECT_FILE = "default.project.json";
const ARTIFACT = "WSync.rbxm";
const MANIFEST = "WSync.build.json";
const SCHEMA_VERSION = 1;

// The protocol number the built plugin speaks. Three places have to agree on
// it — the engine (src/constants.rs PROTOCOL_VERSION), the plugin
// (plugin/src/Remote/init.luau Remote.PROTOCOL) and this manifest — and a
// mismatch is a handshake failure at runtime, so the plugin source is
// cross-checked below rather than trusted to have stayed at 1.
const PROTOCOL_VERSION = 1;
const PROTOCOL_SOURCE = "src/Remote/init.luau";
const PROTOCOL_PATTERN = /^\s*Remote\.PROTOCOL\s*=\s*(\d+)\s*$/m;

// Versions plugin/aftman.toml pins. A different version still builds — a
// contributor is not blocked by a point release — but it is called out,
// because `--check` compares bytes and two rojo versions need not agree on
// them.
const PINS = { rojo: "7.5.1", wally: "0.3.2" };

// Probed in order after $PATH. Rokit and aftman put trampolines on $PATH when
// their shell setup ran; these are for the shells where it did not.
const TOOL_DIRS = [
	path.join(os.homedir(), ".rokit", "bin"),
	path.join(os.homedir(), ".aftman", "bin"),
	path.join(os.homedir(), ".cargo", "bin"),
];

// ---------------------------------------------------------------------------
// Reporting and process plumbing
// ---------------------------------------------------------------------------

const warnings = [];
const failures = [];

function step(message) {
	console.log(`  ${message}`);
}

function warn(message) {
	warnings.push(message);
}

/** Records a problem and keeps going, so one run reports every problem. */
function fail(message) {
	failures.push(message);
}

/** Prints everything collected, then exits 0 (clean) or 1 (any failure). */
function finish(summary) {
	if (warnings.length) {
		console.error("");
		console.error(`WARNING (${warnings.length}):`);
		for (const warning of warnings) console.error(`  ! ${warning}`);
	}

	if (failures.length) {
		console.error("");
		console.error(`${NAME}: FAILED with ${failures.length} problem${failures.length === 1 ? "" : "s"}`);
		for (const failure of failures) console.error(`  - ${failure}`);
		process.exit(1);
	}

	console.log("");
	console.log(`${NAME}: OK${summary ? ` — ${summary}` : ""}`);
	process.exit(0);
}

/** Aborts immediately. For problems that make every later step meaningless. */
function abort(message, ...details) {
	fail(message);
	for (const detail of details) fail(detail);
	finish();
}

/** Repo-relative, forward-slashed path — stable output across platforms. */
function rel(absolute) {
	return path.relative(repoRoot, absolute).split(path.sep).join("/");
}

/** Replaces $HOME with ~ so resolution output is readable and not personal. */
function tilde(absolute) {
	const home = os.homedir();
	return absolute.startsWith(home) ? `~${absolute.slice(home.length)}` : absolute;
}

/** Runs a command with its output attached to ours. Returns the exit status. */
function run(command, args, { cwd = pluginDir } = {}) {
	const result = spawnSync(command, args, { cwd, stdio: "inherit", shell: false });
	if (result.error) {
		if (result.error.code === "ENOENT") return { ok: false, status: 127, reason: `${command} not found` };
		return { ok: false, status: 1, reason: result.error.message };
	}
	return { ok: result.status === 0, status: result.status ?? 1, reason: `exit ${result.status}` };
}

/** Runs a command quietly. Returns trimmed stdout, or null if it failed. */
function capture(command, args, { cwd = pluginDir } = {}) {
	const result = spawnSync(command, args, { cwd, encoding: "utf8", shell: false });
	if (result.error || result.status !== 0) return null;
	return `${result.stdout ?? ""}`.trim();
}

async function exists(target) {
	try {
		await stat(target);
		return true;
	} catch {
		return false;
	}
}

// ---------------------------------------------------------------------------
// Toolchain resolution
// ---------------------------------------------------------------------------

/**
 * Asks a candidate binary for its version, from inside the plugin directory.
 *
 * The cwd matters: `rojo` on $PATH is usually an aftman/rokit trampoline that
 * resolves the real binary from the nearest manifest, so a trampoline for a
 * tool plugin/aftman.toml does not list exits non-zero here. That is the
 * point — "on $PATH" is not the same as "runnable for this project", and only
 * the second one builds anything.
 */
function probe(command) {
	const result = spawnSync(command, ["--version"], { cwd: pluginDir, encoding: "utf8", shell: false });
	if (result.error || result.status !== 0) return null;
	const banner = `${result.stdout ?? ""}${result.stderr ?? ""}`.trim().split("\n")[0] ?? "";
	return { version: banner.match(/(\d+\.\d+\.\d+(?:[-+][\w.]+)?)/)?.[1] ?? null, banner };
}

/**
 * Finds a runnable `rojo` / `wally`, in the order a contributor would expect:
 * an explicit override, $PATH, the toolchain managers' bin directories, then —
 * only if none of those worked — installing from the pinned manifest.
 */
function resolveTool(name, { allowInstall = true } = {}) {
	const envName = `WSYNC_${name.toUpperCase()}`;
	const candidates = [];

	if (process.env[envName]) candidates.push({ command: process.env[envName], source: `$${envName}` });
	candidates.push({ command: name, source: "PATH" });
	for (const dir of TOOL_DIRS) candidates.push({ command: path.join(dir, name), source: tilde(dir) });

	const firstUsable = () => {
		for (const candidate of candidates) {
			const probed = probe(candidate.command);
			if (probed) return { ...candidate, ...probed };
		}
		return null;
	};

	let found = firstUsable();
	if (found || !allowInstall) return found;

	// Nothing runnable. plugin/aftman.toml names the versions this project is
	// built with, so installing from it is both the fix and the pin.
	const manager = ["rokit", "aftman"].find((candidate) => probe(candidate));
	if (manager) {
		step(`no runnable ${name}; installing plugin/aftman.toml tools with ${manager}`);
		let installed = run(manager, ["install", "--no-trust-check"]);
		// rokit and aftman have both carried --no-trust-check, but not in every
		// release; a rejected flag should not cost us the install.
		if (!installed.ok) installed = run(manager, ["install"]);
		if (!installed.ok) warn(`${manager} install failed (${installed.reason})`);
		found = firstUsable();
		if (found) return found;
	}

	// Last resort, and only for rojo: wally has no crates.io release to fall
	// back to, and an absent wally is survivable (see installPackages).
	if (name === "rojo") {
		step(`installing rojo ${PINS.rojo} from crates.io — this compiles from source, expect several minutes`);
		const started = Date.now();
		const built = run("cargo", ["install", `rojo@${PINS.rojo}`, "--locked"]);
		const minutes = ((Date.now() - started) / 60_000).toFixed(1);
		if (!built.ok) {
			warn(`cargo install rojo@${PINS.rojo} failed after ${minutes} min (${built.reason})`);
			return null;
		}
		step(`cargo install finished in ${minutes} min`);
		return firstUsable();
	}

	return null;
}

/** Resolves rojo, checking the version is one that can build this project. */
function resolveRojo() {
	const rojo = resolveTool("rojo");
	if (!rojo) {
		abort(
			"no runnable rojo found",
			`install the pinned toolchain: (cd ${rel(pluginDir)} && aftman install) — or rokit install`,
			`or build one: cargo install rojo@${PINS.rojo} --locked`,
		);
	}

	const major = Number(rojo.version?.split(".")[0]);
	if (Number.isFinite(major) && major < 7) {
		abort(
			`rojo ${rojo.version} is too old to build this project (rojo 7 project format)`,
			`plugin/aftman.toml pins rojo ${PINS.rojo}`,
		);
	}
	if (rojo.version && rojo.version !== PINS.rojo) {
		warn(`rojo ${rojo.version} is not the pinned ${PINS.rojo} — --check compares bytes, so builds may not match CI`);
	}

	step(`rojo ${rojo.version ?? "?"} (${rojo.source})`);
	return rojo;
}

/**
 * Resolves the wsync engine binary — the builder of record for this plugin.
 * The tree needs wsync's own middleware: `src/Lib/Dom/database.msgpack` and
 * the `manifest` node mapped to `wally.toml`. Plain rojo has no mapping for
 * either and silently drops both, which ships a plugin that cannot load
 * (`database is not a valid member` at Lib/Dom line 1). wsync-first is also
 * the dogfooding path: WSync builds WSync.
 */
function resolveWsync() {
	const candidates = [];
	if (process.env.WSYNC_BUILD_BIN) {
		candidates.push({ command: process.env.WSYNC_BUILD_BIN, source: "$WSYNC_BUILD_BIN" });
	}
	const repoRoot = path.dirname(pluginDir);
	for (const profile of ["release", "debug"]) {
		candidates.push({ command: path.join(repoRoot, "target", profile, "wsync"), source: `target/${profile}` });
	}
	candidates.push({ command: "wsync", source: "$PATH" });

	for (const candidate of candidates) {
		const version = capture(candidate.command, ["--version"]);
		if (version === null) {
			if (candidate.source === "$WSYNC_BUILD_BIN") {
				abort(`$WSYNC_BUILD_BIN (${candidate.command}) is not runnable`);
			}
			continue;
		}
		const parsed = version.split(/\s+/).pop();
		step(`wsync ${parsed || "?"} (${candidate.source})`);
		return { kind: "wsync", command: candidate.command, version: parsed, source: candidate.source };
	}
	return null;
}

/**
 * wsync when available, otherwise a hard stop: the rojo path is kept only for
 * historical reference above and is unfit for this tree (see resolveWsync).
 */
function resolveBuilder() {
	const wsync = resolveWsync();
	if (wsync) return wsync;

	abort(
		"no runnable wsync binary found — it is the builder of record for this plugin",
		"the tree needs wsync middleware (src/Lib/Dom/database.msgpack, the wally.toml manifest node); rojo drops both silently",
		"build one at the repo root (`cargo build`) or point $WSYNC_BUILD_BIN at a wsync binary",
	);
}

// ---------------------------------------------------------------------------
// wally packages
// ---------------------------------------------------------------------------

/**
 * Minimal TOML reader for the two things this script needs out of wally.toml:
 * the package version and the dependency aliases. Full TOML is not worth a
 * dependency here — wally manifests are flat tables of quoted strings.
 */
function readTomlTables(text) {
	const tables = new Map();
	let current = null;

	for (const raw of text.split("\n")) {
		const line = raw.replace(/(^|\s)#.*$/, "").trim();
		if (!line) continue;

		const header = line.match(/^\[([^\]]+)\]$/);
		if (header) {
			current = header[1].trim();
			if (!tables.has(current)) tables.set(current, new Map());
			continue;
		}

		const entry = line.match(/^([A-Za-z0-9_.-]+)\s*=\s*(.+)$/);
		if (entry && current) tables.get(current).set(entry[1], entry[2].trim().replace(/^["']|["']$/g, ""));
	}

	return tables;
}

/** wally realm -> the directory `wally install` writes link modules into. */
const PACKAGE_DIRS = new Map([
	["dependencies", "Packages"],
	["dev-dependencies", "DevPackages"],
	["server-dependencies", "ServerPackages"],
]);

/**
 * Every dependency alias in wally.toml, with the link module wally is expected
 * to have written for it. This is what makes the offline path safe: an install
 * that could not reach the registry is only tolerated when the packages it
 * would have produced are already on disk.
 */
function expectedPackageLinks(tables) {
	const links = [];
	for (const [table, dir] of PACKAGE_DIRS) {
		for (const alias of tables.get(table)?.keys() ?? []) {
			links.push({ alias, dir, candidates: [`${alias}.lua`, `${alias}.luau`].map((f) => path.join(pluginDir, dir, f)) });
		}
	}
	return links;
}

async function missingPackageLinks(links) {
	const missing = [];
	for (const link of links) {
		const present = await Promise.all(link.candidates.map(exists));
		if (!present.some(Boolean)) missing.push(`${link.dir}/${link.alias}`);
	}
	return missing;
}

/**
 * Runs `wally install`, tolerating both an existing Packages directory (wally
 * overwrites it) and an unreachable registry.
 *
 * The offline case is deliberate. wally resolves from a network index, and the
 * plugin is often built on a machine that cannot reach it — but the packages
 * are already installed, and refusing to build then would mean no artifact for
 * no reason. An install that fails with a complete Packages tree is a warning;
 * one that fails with packages missing is fatal.
 */
async function installPackages(links, { skipWally }) {
	if (skipWally) {
		step("skipping wally install (WSYNC_SKIP_WALLY=1)");
	} else {
		const wally = resolveTool("wally");
		if (wally) {
			if (wally.version && wally.version !== PINS.wally) {
				warn(`wally ${wally.version} is not the pinned ${PINS.wally}`);
			}
			step(`wally ${wally.version ?? "?"} (${wally.source})`);
			const installed = run(wally.command, ["install"]);
			if (!installed.ok) warn(`wally install failed (${installed.reason}) — falling back to the packages on disk`);
		} else {
			warn("no runnable wally found — falling back to the packages on disk");
		}
	}

	const missing = await missingPackageLinks(links);
	if (missing.length) {
		abort(
			`wally packages are missing: ${missing.join(", ")}`,
			"the project tree references them, so the plugin cannot be built without them",
			`run \`wally install\` in ${rel(pluginDir)} with the registry reachable, or copy a populated Packages/ in`,
		);
	}

	// wally.lock is the only record of what was actually resolved, and it is
	// git-ignored, so print it: a --check that fails after a dependency moved
	// is otherwise a mystery.
	const lockPath = path.join(pluginDir, "wally.lock");
	if (await exists(lockPath)) {
		const lock = await readFile(lockPath, "utf8");
		const resolved = [...lock.matchAll(/name\s*=\s*"([^"]+)"\s*\nversion\s*=\s*"([^"]+)"/g)]
			.map(([, name, version]) => `${name}@${version}`)
			.filter((entry) => !entry.startsWith("wsync/plugin@"));
		if (resolved.length) step(`packages: ${resolved.join(", ")}`);
	}
}

// ---------------------------------------------------------------------------
// Project verification
// ---------------------------------------------------------------------------

/** Collects every `$path` in a Rojo project tree, in document order. */
function collectProjectPaths(node, trail, found) {
	if (!node || typeof node !== "object") return found;

	if (typeof node.$path === "string") found.push({ at: trail.join("."), value: node.$path });
	else if (node.$path && typeof node.$path === "object" && typeof node.$path.optional === "string") {
		found.push({ at: trail.join("."), value: node.$path.optional, optional: true });
	}

	for (const [key, value] of Object.entries(node)) {
		if (key.startsWith("$")) continue;
		collectProjectPaths(value, [...trail, key], found);
	}
	return found;
}

/**
 * Checks that the project file parses and that every path it names exists.
 *
 * rojo reports a missing path itself, but as one error at a time and in terms
 * of the file it could not open. Doing it here names the project key as well,
 * which is what tells you whether the fix belongs to the source tree or to the
 * build (a missing Packages/ is this script's problem; a missing src/ is not).
 */
async function verifyProject() {
	const projectPath = path.join(pluginDir, PROJECT_FILE);
	let project;
	try {
		project = JSON.parse(await readFile(projectPath, "utf8"));
	} catch (err) {
		abort(`${rel(projectPath)} could not be read as JSON — ${err.message}`);
	}

	const missing = [];
	for (const entry of collectProjectPaths(project.tree, [project.name ?? "tree"], [])) {
		if (entry.optional) continue;
		if (!(await exists(path.join(pluginDir, entry.value)))) missing.push(`${entry.at} -> ${entry.value}`);
	}

	if (missing.length) {
		abort(
			`${rel(projectPath)} references paths that do not exist: ${missing.join(", ")}`,
			"either the project file or the source tree is wrong; this script will not guess which",
		);
	}

	return project;
}

// ---------------------------------------------------------------------------
// Build identity
// ---------------------------------------------------------------------------

/**
 * The commit and dirtiness the artifact was built from, matching what the
 * engine's build.rs stamps into the binary (short-12, "unknown" outside git,
 * `git status --porcelain` for dirtiness) so the two identities can be
 * compared without knowing which produced which.
 */
function buildIdentity() {
	const commit = capture("git", ["rev-parse", "--short=12", "HEAD"], { cwd: repoRoot });
	const status = capture("git", ["status", "--porcelain"], { cwd: repoRoot });
	return {
		buildCommit: commit && commit.length > 0 ? commit : "unknown",
		buildDirty: status === null ? false : status.length > 0,
	};
}

/** The version in wally.toml [package] — the plugin's own version number. */
function pluginVersion(tables) {
	const version = tables.get("package")?.get("version");
	if (!version) abort("plugin/wally.toml has no [package] version");
	return version;
}

/**
 * The protocol number, cross-checked against the plugin source.
 *
 * A manifest that claims a protocol the plugin does not speak is worse than no
 * manifest: `wsync plugin status` compares that number against the engine's.
 * A source file that no longer matches the pattern is only a warning — the
 * plugin tree is under active development and a rename must not break the
 * build — but a number that disagrees is a hard failure.
 */
async function protocolVersion() {
	const sourcePath = path.join(pluginDir, PROTOCOL_SOURCE);
	if (!(await exists(sourcePath))) {
		warn(`${rel(sourcePath)} is gone — protocol ${PROTOCOL_VERSION} in the manifest is now unverified`);
		return PROTOCOL_VERSION;
	}

	const declared = (await readFile(sourcePath, "utf8")).match(PROTOCOL_PATTERN)?.[1];
	if (declared === undefined) {
		warn(`no \`Remote.PROTOCOL = <n>\` in ${rel(sourcePath)} — protocol ${PROTOCOL_VERSION} is now unverified`);
		return PROTOCOL_VERSION;
	}
	if (Number(declared) !== PROTOCOL_VERSION) {
		abort(
			`protocol mismatch: ${rel(sourcePath)} declares ${declared}, this script writes ${PROTOCOL_VERSION}`,
			"bump PROTOCOL_VERSION in plugin/scripts/build.mjs and src/constants.rs together, or the handshake breaks",
		);
	}

	return PROTOCOL_VERSION;
}

// ---------------------------------------------------------------------------
// Build
// ---------------------------------------------------------------------------

async function sha256(file) {
	return createHash("sha256").update(await readFile(file)).digest("hex");
}

/** `<builder> build` into `outputPath`. Fatal on failure — there is nothing after it. */
function buildArtifact(builder, outputPath) {
	const built = run(builder.command, ["build", PROJECT_FILE, "--output", outputPath]);
	if (!built.ok) abort(`${builder.kind ?? "rojo"} build failed (${built.reason})`);
}

async function writeManifest(manifestPath, manifest) {
	await writeFile(manifestPath, `${JSON.stringify(manifest, null, 2)}\n`, "utf8");
}

async function readManifest(manifestPath) {
	try {
		return JSON.parse(await readFile(manifestPath, "utf8"));
	} catch (err) {
		if (err.code === "ENOENT") return null;
		abort(`${rel(manifestPath)} could not be read as JSON — ${err.message}`);
	}
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

function help() {
	return [
		`${NAME} — builds plugin/${ARTIFACT} and the manifest that describes it`,
		"",
		"Usage",
		`  node plugin/scripts/build.mjs [--check] [--skip-wally]`,
		"",
		"Options",
		"  --check       Rebuild to a temporary file and diff its sha256 against",
		`                plugin/${MANIFEST}. Leaves the artifact and the manifest`,
		"                untouched. Exit 1 when the manifest is missing or stale.",
		"  --skip-wally  Skip `wally install` and use the packages already on disk.",
		"                Same as WSYNC_SKIP_WALLY=1.",
		"  --help        Print this message.",
		"",
		"Environment",
		"  WSYNC_SKIP_WALLY=1   as --skip-wally",
		"  WSYNC_ROJO=<path>    use this rojo instead of resolving one",
		"  WSYNC_WALLY=<path>   use this wally instead of resolving one",
		"",
		"Toolchain resolution, in order: $WSYNC_ROJO / $WSYNC_WALLY, $PATH,",
		`${TOOL_DIRS.map(tilde).join(", ")}, then \`aftman install\` (or rokit)`,
		`from plugin/aftman.toml, then \`cargo install rojo@${PINS.rojo} --locked\`.`,
	].join("\n");
}

function parseCli(argv) {
	const cli = { check: false, skipWally: false, help: false };
	for (const arg of argv) {
		if (arg === "--check") cli.check = true;
		else if (arg === "--skip-wally") cli.skipWally = true;
		else if (arg === "--help" || arg === "-h") cli.help = true;
		else {
			console.error(`${NAME}: unknown option ${arg}`);
			console.error(help());
			process.exit(1);
		}
	}
	return cli;
}

const cli = parseCli(process.argv.slice(2));

if (cli.help) {
	console.log(help());
	process.exit(0);
}

const manifestPath = path.join(pluginDir, MANIFEST);
const artifactPath = path.join(pluginDir, ARTIFACT);

console.log(`${NAME}: ${cli.check ? "checking" : "building"} ${rel(artifactPath)}`);

// --check reads the manifest before touching anything, so a missing one fails
// on its own terms instead of after a needless build.
const baseline = cli.check ? await readManifest(manifestPath) : null;
if (cli.check && !baseline) {
	abort(
		`${rel(manifestPath)} does not exist — nothing to check against`,
		"run `node plugin/scripts/build.mjs` and commit the manifest it writes",
	);
}

const wallyTables = readTomlTables(await readFile(path.join(pluginDir, "wally.toml"), "utf8"));
const version = pluginVersion(wallyTables);
const protocol = await protocolVersion();

await installPackages(expectedPackageLinks(wallyTables), {
	skipWally: cli.skipWally || process.env.WSYNC_SKIP_WALLY === "1",
});
await verifyProject();

const builder = resolveBuilder();

if (cli.check) {
	// A temp output keeps the working tree exactly as it was: --check answers
	// "is the committed manifest still true", and must not make it true.
	const scratch = await mkdtemp(path.join(os.tmpdir(), "wsync-plugin-"));
	const candidate = path.join(scratch, ARTIFACT);
	let rebuilt;
	try {
		buildArtifact(builder, candidate);
		rebuilt = await sha256(candidate);
	} finally {
		await rm(scratch, { recursive: true, force: true });
	}

	step(`rebuilt sha256 ${rebuilt}`);
	step(`manifest sha256 ${baseline.sha256}`);

	if (baseline.schemaVersion !== SCHEMA_VERSION) {
		fail(`${rel(manifestPath)} is schemaVersion ${baseline.schemaVersion}, this script writes ${SCHEMA_VERSION}`);
	}
	if (baseline.artifact !== ARTIFACT) fail(`${rel(manifestPath)} names artifact "${baseline.artifact}", expected "${ARTIFACT}"`);
	if (baseline.pluginVersion !== version) {
		fail(`${rel(manifestPath)} says pluginVersion ${baseline.pluginVersion}, wally.toml says ${version}`);
	}
	if (baseline.protocolVersion !== protocol) {
		fail(`${rel(manifestPath)} says protocolVersion ${baseline.protocolVersion}, the plugin speaks ${protocol}`);
	}
	if (baseline.sha256 !== rebuilt) {
		fail(
			`${rel(manifestPath)} is stale: it records ${baseline.sha256}, a rebuild produces ${rebuilt} — ` +
				"run `node plugin/scripts/build.mjs` and commit the manifest",
		);
	}

	// The artifact itself is git-ignored, so this is about a local tree only:
	// a stale .rbxm is what `wsync plugin install` would push into Studio.
	if (await exists(artifactPath)) {
		const onDisk = await sha256(artifactPath);
		if (onDisk !== rebuilt) warn(`${rel(artifactPath)} on disk (${onDisk.slice(0, 12)}) is not this build — rebuild before installing`);
	}

	finish(`manifest matches a fresh build (${rebuilt.slice(0, 12)}, plugin ${version}, protocol ${protocol})`);
}

buildArtifact(builder, artifactPath);

const digest = await sha256(artifactPath);
const bytes = (await stat(artifactPath)).size;
const identity = buildIdentity();

await writeManifest(manifestPath, {
	schemaVersion: SCHEMA_VERSION,
	artifact: ARTIFACT,
	sha256: digest,
	pluginVersion: version,
	protocolVersion: protocol,
	// When this artifact was produced. Identity is the sha256; this exists so
	// a human can tell two 0.1.x builds apart at a glance. --check ignores it.
	builtAt: new Date().toISOString(),
	...identity,
});

step(`${rel(artifactPath)} — ${bytes.toLocaleString("en-US")} bytes, sha256 ${digest}`);
step(`${rel(manifestPath)} — plugin ${version}, protocol ${protocol}, commit ${identity.buildCommit}${identity.buildDirty ? " (dirty)" : ""}`);

finish(`built ${ARTIFACT} (${(bytes / 1024).toFixed(0)} KiB)`);
