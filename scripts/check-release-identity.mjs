#!/usr/bin/env node
// check-release-identity.mjs — everything in a release came from one commit.
//
// Design 8.5 and 10.5 both state the same rule from different ends: "release CI
// must verify desktop host, daemon sidecar, plugin artifact, protocol number and
// generated docs all come from one commit". This check is that assertion, run
// against built artifacts rather than against the tree they were built from.
//
// WHAT CAN ACTUALLY BE ASKED OF A BUILT BINARY, OFFLINE
//
// The release-identity job has no daemon and no Studio, so the binary can only
// be interrogated with commands that answer on their own. What is available was
// established by running them, not assumed:
//
//   `wsync version --raw`   NOT usable. It connects first (`Client::connect`)
//                           and exits 1 with "No WSync daemon answers on port
//                           7978" — its version report is about the *daemon and
//                           plugin it is talking to*, which is exactly what a
//                           release job does not have.
//   `wsync --version`       usable. clap prints `wsync <semver>`: the crate
//                           version, and the only version number the binary
//                           will state without help.
//   `wsync commands`        usable, and the strong one. `src/cli/registry.rs`
//                           prints the embedded `docs/client-commands.generated.json`
//                           *verbatim* (`include_str!`, printed with `print!`),
//                           so its stdout is a byte-for-byte copy of the docs
//                           bundle the binary was compiled with. Comparing that
//                           to the checked-in bundle is a real identity link
//                           between the binary and the docs — not a version
//                           string that anybody can retype.
//   `wsync commands --compact`
//                           usable. Parses the same embedded bundle, so it
//                           catches a binary that carries bytes it cannot read.
//
// The build commit is *not* retrievable offline. `build.rs` stamps
// `WSYNC_BUILD_COMMIT`, but the only surface that reports it is `GET /hello`,
// which needs a running daemon. So the commit is supplied by the caller (CI
// passes `github.sha`; a checkout falls back to `git rev-parse`), compared
// against what the plugin build recorded, and then *corroborated* against the
// binary: `env!("WSYNC_BUILD_COMMIT")` is a string literal in a live code path,
// so the 12-character short commit is present in the binary's data when the
// binary was built from it. A miss is a failure; a hit is corroboration rather
// than proof, and the check says so.
//
// The commit "unknown" is what build.rs and plugin/scripts/build.mjs both write
// outside a repository (or in one with no commits). It matches only itself —
// two artifacts that each shrugged are not two artifacts from one commit, but
// they are also not a mismatch — and the binary probe is skipped for it, since
// the word appears in any binary a hundred times over.
//
// WHAT THIS CHECK ASSERTS
//
//   1. the engine binary runs and reports `wsync <version>`;
//   2. its embedded docs bundle is byte-identical to docs/client-commands.generated.json;
//   3. the generated docs are present and have not drifted from docs/commands/
//      (scripts/check-docs-drift.mjs, run as-is — one implementation of that rule);
//   4. one version number across the engine binary, root Cargo.toml,
//      plugin/WSync.build.json's pluginVersion and desktop tauri.conf.json;
//   5. one protocol number across the plugin manifest, src/constants.rs,
//      plugin/src/Remote/init.luau and the desktop host's `wsync/N`;
//   6. the plugin artifact matches the sha256 its manifest publishes;
//   7. one commit across the plugin manifest, the caller-supplied commit and the
//      binary's own stamp;
//   8. nothing was built dirty (unless --allow-dirty).
//
// Node >= 18, zero dependencies. Exits 1 on any mismatch, writes nothing.

import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { readFile, stat } from "node:fs/promises";
import path from "node:path";
import { Reporter, helpFlag, isFile, parseArgs, renderHelp, repoRoot } from "./lib/policy.mjs";

const NAME = "check-release-identity";

const DEFAULTS = {
	pluginManifest: "plugin/WSync.build.json",
	pluginArtifact: "plugin/WSync.rbxm",
	docsBundle: "docs/client-commands.generated.json",
	docsMarkdown: "docs/client-commands.md",
	tauriConf: "desktop/src-tauri/tauri.conf.json",
	cargoToml: "Cargo.toml",
	engineConstants: "src/constants.rs",
	pluginProtocolSource: "plugin/src/Remote/init.luau",
	desktopHost: "desktop/src-tauri/src/lib.rs",
	driftCheck: "scripts/check-docs-drift.mjs",
};

/** Timeout for every `<bin> …` invocation. A hung binary is a failed check. */
const BINARY_TIMEOUT_MS = 30_000;

/** `git rev-parse --short=12` is what both build stampers record. */
const COMMIT_LENGTH = 12;

/** What both stampers write when there is no commit to record. */
const UNKNOWN_COMMIT = "unknown";

const SPEC = {
	bin: {
		type: "string",
		valueName: "path",
		description: "The built engine binary to interrogate (required).",
	},
	"plugin-manifest": {
		type: "string",
		valueName: "path",
		description: `Plugin build manifest (default ${DEFAULTS.pluginManifest}).`,
	},
	"plugin-artifact": {
		type: "string",
		valueName: "path",
		description: `Plugin artifact (default: WSync.rbxm beside the manifest).`,
	},
	docs: {
		type: "string",
		valueName: "path",
		description: `Generated docs bundle (default ${DEFAULTS.docsBundle}).`,
	},
	"tauri-conf": {
		type: "string",
		valueName: "path",
		description: `Desktop Tauri config (default ${DEFAULTS.tauriConf}).`,
	},
	commit: {
		type: "string",
		valueName: "sha",
		description: "The commit being released (default: $GITHUB_SHA, else git HEAD).",
	},
	"expect-version": {
		type: "string",
		valueName: "semver",
		description: "Require this version too — pass the tag (default: $GITHUB_REF_NAME on a tag).",
	},
	"allow-dirty": {
		type: "boolean",
		description: "Permit artifacts stamped as built from a dirty tree.",
	},
	"skip-drift": {
		type: "boolean",
		description: "Skip the docs-drift check (only when another job just ran it).",
	},
	help: helpFlag,
};

function help() {
	return renderHelp({
		name: NAME,
		summary: "fails when a release's binary, plugin, protocol number and docs disagree",
		usage: "node scripts/check-release-identity.mjs --bin <path> [options]",
		spec: SPEC,
		sections: [
			{
				title: "What it enforces",
				body: [
					"One version, one protocol number, one commit and one docs bundle across",
					"the engine binary, plugin/WSync.build.json, the generated docs and the",
					"desktop app's tauri.conf.json (Design 8.5, 10.5).",
					"",
					"The binary is asked only what it can answer without a daemon:",
					"`--version`, and `commands`, whose stdout is the embedded docs bundle",
					"verbatim and is compared byte for byte. The build commit is not",
					"retrievable offline (only `GET /hello` reports it), so it is supplied",
					"and then corroborated against the binary's own stamped string.",
				],
			},
			{
				title: "Examples",
				body: [
					"node scripts/check-release-identity.mjs --bin target/release/wsync",
					"node scripts/check-release-identity.mjs --bin dist/wsync --commit $GITHUB_SHA",
					"node scripts/check-release-identity.mjs --bin target/debug/wsync --allow-dirty",
				],
			},
		],
	});
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

/** Resolves a caller path against the cwd, a default against the repo root. */
function resolvePath(given, fallback) {
	return given ? path.resolve(given) : path.join(repoRoot, fallback);
}

/**
 * Repo-relative when the path is inside the repository, absolute when it is
 * not — a release job's artifacts live in the runner's temp directory, and
 * `../../../../..` in front of them helps nobody.
 */
function relative(absolute) {
	const inside = path.relative(repoRoot, absolute).split(path.sep).join("/");
	return inside && !inside.startsWith("..") ? inside : absolute;
}

const indent = (text) =>
	String(text ?? "")
		.trimEnd()
		.split("\n")
		.map((line) => `      ${line}`)
		.join("\n");

async function readJson(file) {
	const raw = await readFile(file, "utf8");
	return JSON.parse(raw);
}

async function sha256(file) {
	return createHash("sha256").update(await readFile(file)).digest("hex");
}

/**
 * Both stampers record `git rev-parse --short=12`, while CI knows the full
 * 40-character sha. Comparison happens on the short form, and only for things
 * that look like hex — "unknown" is passed through untouched so it can match
 * only itself.
 */
function shortCommit(value) {
	const trimmed = String(value ?? "").trim();
	if (!trimmed) return null;
	if (/^[0-9a-f]{7,40}$/i.test(trimmed)) return trimmed.slice(0, COMMIT_LENGTH).toLowerCase();
	return trimmed;
}

function gitCommit() {
	const run = spawnSync("git", ["rev-parse", `--short=${COMMIT_LENGTH}`, "HEAD"], {
		cwd: repoRoot,
		encoding: "utf8",
		stdio: ["ignore", "pipe", "ignore"],
	});
	if (run.status !== 0) return null;
	const value = (run.stdout || "").trim();
	return value.length ? value : null;
}

function gitDirty() {
	const run = spawnSync("git", ["status", "--porcelain"], {
		cwd: repoRoot,
		encoding: "utf8",
		stdio: ["ignore", "pipe", "ignore"],
	});
	if (run.status !== 0) return null;
	return (run.stdout || "").trim().length > 0;
}

/** Runs the binary. Returns `{ ok, stdout, stderr, reason }` — never throws. */
function runBinary(bin, args) {
	const run = spawnSync(bin, args, {
		encoding: "buffer",
		timeout: BINARY_TIMEOUT_MS,
		maxBuffer: 64 * 1024 * 1024,
		stdio: ["ignore", "pipe", "pipe"],
	});
	if (run.error) {
		return { ok: false, reason: run.error.message };
	}
	const stderr = run.stderr?.toString("utf8") ?? "";
	if (run.signal) {
		return { ok: false, reason: `killed by ${run.signal} (timeout ${BINARY_TIMEOUT_MS} ms)`, stderr };
	}
	if (run.status !== 0) {
		return { ok: false, reason: `exit ${run.status}`, stdout: run.stdout, stderr };
	}
	return { ok: true, stdout: run.stdout ?? Buffer.alloc(0), stderr };
}

/** First difference between two buffers, reported by line like the drift check. */
function describeDifference(expected, actual) {
	if (expected.equals(actual)) return null;
	let offset = 0;
	const limit = Math.min(expected.length, actual.length);
	while (offset < limit && expected[offset] === actual[offset]) offset++;
	const line = expected.subarray(0, offset).toString("utf8").split("\n").length;
	const pick = (buffer) => buffer.toString("utf8").split("\n")[line - 1] ?? "<end of output>";
	const clip = (s) => (s.length > 140 ? `${s.slice(0, 137)}...` : s);
	return (
		`first difference at line ${line} (byte ${offset}); ` +
		`binary emitted ${expected.length} B vs ${actual.length} B on disk` +
		`\n      on disk : ${clip(pick(actual))}` +
		`\n      binary  : ${clip(pick(expected))}`
	);
}

/** `version = "x.y.z"` from the [package] table of the root Cargo.toml. */
function cargoVersion(text) {
	const packageTable = text.split(/^\s*\[/m).find((section) => section.startsWith("package]"));
	return packageTable?.match(/^\s*version\s*=\s*"([^"]+)"/m)?.[1] ?? null;
}

// ---------------------------------------------------------------------------
// The checks
// ---------------------------------------------------------------------------

/**
 * Records a value under a label, and fails once the same fact is claimed with
 * two different values. Every "these must agree" assertion in here is this.
 */
class Agreement {
	constructor(report, subject) {
		this.report = report;
		this.subject = subject;
		this.claims = [];
	}

	claim(source, value) {
		if (value === null || value === undefined) return;
		this.claims.push({ source, value: String(value) });
	}

	/** Returns the agreed value, or null when there is disagreement or nothing. */
	settle() {
		if (this.claims.length === 0) {
			this.report.fail(this.subject, "nothing in this release states it — cannot be checked");
			return null;
		}
		const distinct = [...new Set(this.claims.map((claim) => claim.value))];
		if (distinct.length === 1) {
			this.report.note(
				`  ok  ${this.subject}: ${distinct[0]} (${this.claims.length} source${this.claims.length === 1 ? "" : "s"} agree)`,
			);
			return distinct[0];
		}
		this.report.fail(
			this.subject,
			`${distinct.length} different values in one release —\n` +
				this.claims.map((claim) => `      ${claim.source} = ${claim.value}`).join("\n"),
		);
		return null;
	}
}

async function main() {
	const { values, positionals, errors } = parseArgs(process.argv.slice(2), SPEC);
	if (values.help) {
		console.log(help());
		return;
	}

	const report = new Reporter(NAME);
	for (const error of errors) report.fail("arguments", error);
	for (const extra of positionals) report.fail("arguments", `unexpected argument "${extra}"`);

	const binPath = values.bin ? path.resolve(values.bin) : null;
	if (!binPath) {
		report.fail("arguments", "--bin is required — this check is about a *built* release");
	}
	if (report.failures.length) report.finish();

	const manifestPath = resolvePath(values["plugin-manifest"], DEFAULTS.pluginManifest);
	const artifactPath = values["plugin-artifact"]
		? path.resolve(values["plugin-artifact"])
		: values["plugin-manifest"]
			? path.join(path.dirname(manifestPath), "WSync.rbxm")
			: path.join(repoRoot, DEFAULTS.pluginArtifact);
	const docsPath = resolvePath(values.docs, DEFAULTS.docsBundle);
	const tauriConfPath = resolvePath(values["tauri-conf"], DEFAULTS.tauriConf);

	const version = new Agreement(report, "version");
	const protocol = new Agreement(report, "protocol number");
	const commit = new Agreement(report, "build commit");

	// --- the engine binary ---------------------------------------------------

	let binaryBytes = null;
	if (!(await isFile(binPath))) {
		report.fail(relative(binPath), "no such file — build the engine before checking a release");
	} else {
		const versionRun = runBinary(binPath, ["--version"]);
		if (!versionRun.ok) {
			report.fail(
				relative(binPath),
				`\`--version\` did not run (${versionRun.reason})${versionRun.stderr ? `\n${indent(versionRun.stderr)}` : ""}`,
			);
		} else {
			const printed = versionRun.stdout.toString("utf8").trim();
			const parsed = printed.match(/^wsync\s+(\S+)$/);
			if (!parsed) {
				report.fail(
					relative(binPath),
					`\`--version\` printed ${JSON.stringify(printed)}, expected \`wsync <version>\` — ` +
						"this check reads that line, so an unparseable one is a failure, not a shrug",
				);
			} else {
				version.claim(`${relative(binPath)} --version`, parsed[1]);
			}
		}

		// The embedded docs bundle, verbatim. This is the binary <-> docs link.
		const bundleRun = runBinary(binPath, ["commands"]);
		if (!bundleRun.ok) {
			report.fail(
				relative(binPath),
				`\`commands\` did not run offline (${bundleRun.reason}) — it must never need a daemon` +
					(bundleRun.stderr ? `\n${indent(bundleRun.stderr)}` : ""),
			);
		} else if (!(await isFile(docsPath))) {
			report.fail(relative(docsPath), "generated docs bundle is missing — nothing to compare the binary to");
		} else {
			const onDisk = await readFile(docsPath);
			const emitted = bundleRun.stdout;
			// registry.rs prints the embedded string and adds a newline only when
			// the file does not end with one, so a bundle without a trailing
			// newline still compares equal.
			const expected = onDisk.at(-1) === 0x0a ? onDisk : Buffer.concat([onDisk, Buffer.from("\n")]);
			const difference = describeDifference(emitted, expected);
			if (difference) {
				report.fail(
					"embedded docs",
					`\`${relative(binPath)} commands\` is not ${relative(docsPath)} — the binary was ` +
						`built from a different registry than this checkout carries; ${difference}`,
				);
			} else {
				report.note(`  ok  embedded docs bundle == ${relative(docsPath)} (${expected.length} B, byte-identical)`);
			}
		}

		// A binary that emits bytes it cannot itself parse is a broken embed.
		const compactRun = runBinary(binPath, ["commands", "--compact"]);
		if (!compactRun.ok) {
			report.fail(
				relative(binPath),
				`\`commands --compact\` failed (${compactRun.reason}) — the embedded bundle does not parse` +
					(compactRun.stderr ? `\n${indent(compactRun.stderr)}` : ""),
			);
		} else {
			try {
				const compact = JSON.parse(compactRun.stdout.toString("utf8"));
				const bundle = (await isFile(docsPath)) ? await readJson(docsPath) : null;
				const documented = Array.isArray(bundle?.commands) ? bundle.commands.length : null;
				if (documented !== null && compact.total !== documented) {
					report.fail(
						"embedded docs",
						`\`commands --compact\` counts ${compact.total} commands, ${relative(docsPath)} has ${documented}`,
					);
				}
			} catch (err) {
				report.fail(relative(binPath), `\`commands --compact\` printed unparseable JSON — ${err.message}`);
			}
		}

		try {
			binaryBytes = await readFile(binPath);
		} catch (err) {
			report.fail(relative(binPath), `could not be read for the commit stamp probe — ${err.message}`);
		}
	}

	// --- the plugin pair -----------------------------------------------------

	let manifest = null;
	if (!(await isFile(manifestPath))) {
		report.fail(
			relative(manifestPath),
			"plugin build manifest is missing — a release without it ships an unverifiable plugin",
		);
	} else {
		try {
			manifest = await readJson(manifestPath);
		} catch (err) {
			report.fail(relative(manifestPath), `is not valid JSON — ${err.message}`);
		}
	}

	if (manifest) {
		const where = relative(manifestPath);
		if (manifest.schemaVersion !== 1) {
			report.fail(where, `schemaVersion is ${JSON.stringify(manifest.schemaVersion)}, this check reads 1`);
		}
		if (manifest.artifact !== "WSync.rbxm") {
			report.fail(where, `names artifact ${JSON.stringify(manifest.artifact)}, expected "WSync.rbxm"`);
		}
		if (typeof manifest.sha256 !== "string" || !/^[0-9a-f]{64}$/.test(manifest.sha256)) {
			report.fail(where, `sha256 is not 64 lowercase hex characters (${JSON.stringify(manifest.sha256)})`);
		}
		version.claim(`${where} pluginVersion`, manifest.pluginVersion);
		protocol.claim(`${where} protocolVersion`, manifest.protocolVersion);
		commit.claim(`${where} buildCommit`, shortCommit(manifest.buildCommit));

		if (manifest.buildDirty === true && !values["allow-dirty"]) {
			report.fail(
				where,
				"buildDirty is true — the plugin was built from a tree with uncommitted changes. " +
					"Pass --allow-dirty only for a build nobody will install.",
			);
		} else if (manifest.buildDirty === true) {
			report.warn(`${where}: buildDirty is true (allowed by --allow-dirty)`);
		}
	}

	if (!(await isFile(artifactPath))) {
		report.fail(
			relative(artifactPath),
			`plugin artifact is missing — build it with \`node plugin/scripts/build.mjs\``,
		);
	} else if (manifest && /^[0-9a-f]{64}$/.test(String(manifest.sha256))) {
		const size = (await stat(artifactPath)).size;
		if (size === 0) {
			report.fail(relative(artifactPath), "is zero bytes — a placeholder must never reach a release");
		}
		const digest = await sha256(artifactPath);
		if (digest !== manifest.sha256) {
			report.fail(
				relative(artifactPath),
				`sha256 ${digest} does not match ${relative(manifestPath)} (${manifest.sha256}) — ` +
					"the artifact and its manifest are from different builds",
			);
		} else {
			report.note(`  ok  ${relative(artifactPath)} matches its manifest sha256 (${size.toLocaleString("en-US")} B)`);
		}
	}

	// --- protocol number, from every tree that states it ---------------------

	const constantsPath = path.join(repoRoot, DEFAULTS.engineConstants);
	if (await isFile(constantsPath)) {
		const declared = (await readFile(constantsPath, "utf8")).match(
			/pub const PROTOCOL_VERSION\s*:\s*u8\s*=\s*(\d+)\s*;/,
		)?.[1];
		if (declared === undefined) {
			report.warn(`${DEFAULTS.engineConstants}: no \`PROTOCOL_VERSION: u8 = <n>\` — the engine's number went unchecked`);
		} else {
			protocol.claim(`${DEFAULTS.engineConstants} PROTOCOL_VERSION`, Number(declared));
		}
	}

	const pluginProtocolPath = path.join(repoRoot, DEFAULTS.pluginProtocolSource);
	if (await isFile(pluginProtocolPath)) {
		const declared = (await readFile(pluginProtocolPath, "utf8")).match(/^\s*Remote\.PROTOCOL\s*=\s*(\d+)\s*$/m)?.[1];
		if (declared === undefined) {
			report.warn(`${DEFAULTS.pluginProtocolSource}: no \`Remote.PROTOCOL = <n>\` — the plugin's number went unchecked`);
		} else {
			protocol.claim(`${DEFAULTS.pluginProtocolSource} Remote.PROTOCOL`, Number(declared));
		}
	}

	const desktopHostPath = path.join(repoRoot, DEFAULTS.desktopHost);
	if (await isFile(desktopHostPath)) {
		const declared = (await readFile(desktopHostPath, "utf8")).match(/PROTOCOL\s*:\s*&str\s*=\s*"wsync\/(\d+)"/)?.[1];
		if (declared === undefined) {
			report.warn(`${DEFAULTS.desktopHost}: no \`PROTOCOL: &str = "wsync/<n>"\` — the host's number went unchecked`);
		} else {
			protocol.claim(`${DEFAULTS.desktopHost} PROTOCOL`, Number(declared));
		}
	}

	// --- versions, from every tree that states one ---------------------------

	const cargoPath = path.join(repoRoot, DEFAULTS.cargoToml);
	if (await isFile(cargoPath)) {
		const declared = cargoVersion(await readFile(cargoPath, "utf8"));
		if (declared === null) report.warn(`${DEFAULTS.cargoToml}: no [package] version — the crate version went unchecked`);
		else version.claim(`${DEFAULTS.cargoToml} [package] version`, declared);
	}

	if (!(await isFile(tauriConfPath))) {
		report.fail(relative(tauriConfPath), "desktop Tauri config is missing — the app's version cannot be checked");
	} else {
		try {
			const config = await readJson(tauriConfPath);
			version.claim(`${relative(tauriConfPath)} version`, config.version);

			// The release workflow stages the sidecar as `binaries/wsync-<triple>`
			// because this list says `binaries/wsync`; if that ever changes, the
			// staging step is silently wrong, so it is checked here.
			const externalBin = config.bundle?.externalBin;
			if (!Array.isArray(externalBin) || !externalBin.includes("binaries/wsync")) {
				report.fail(
					relative(tauriConfPath),
					`bundle.externalBin no longer contains "binaries/wsync" — the release workflow stages ` +
						"the sidecar under that name and would ship a bundle without an engine",
				);
			}

			// Not a failure: a release built before the repository is public is a
			// legitimate thing to produce, it just cannot self-update.
			const endpoints = config.plugins?.updater?.endpoints ?? [];
			if (endpoints.some((endpoint) => String(endpoint).includes("OWNER"))) {
				report.warn(
					`${relative(tauriConfPath)}: updater endpoint is still the OWNER placeholder — ` +
						"a shipped build will look for latest.json at a repository that does not exist",
				);
			}
		} catch (err) {
			report.fail(relative(tauriConfPath), `is not valid JSON — ${err.message}`);
		}
	}

	// --- the commit ----------------------------------------------------------

	const supplied = shortCommit(values.commit ?? process.env.GITHUB_SHA ?? gitCommit() ?? UNKNOWN_COMMIT);
	const suppliedSource = values.commit
		? "--commit"
		: process.env.GITHUB_SHA
			? "$GITHUB_SHA"
			: gitCommit()
				? "git HEAD"
				: "no commit available";
	commit.claim(suppliedSource, supplied);

	const dirtyTree = gitDirty();
	if (dirtyTree === true && !values["allow-dirty"]) {
		report.fail(
			"working tree",
			"`git status --porcelain` is not empty — a release is cut from a committed tree. " +
				"Pass --allow-dirty for a build nobody will install.",
		);
	} else if (dirtyTree === true) {
		report.warn("working tree: uncommitted changes (allowed by --allow-dirty)");
	}

	// --- drift ---------------------------------------------------------------

	for (const generated of [DEFAULTS.docsBundle, DEFAULTS.docsMarkdown]) {
		if (!(await isFile(path.join(repoRoot, generated)))) {
			report.fail(generated, "generated docs file is missing — run `node scripts/build-command-docs.mjs`");
		}
	}

	if (values["skip-drift"]) {
		report.warn("docs drift was skipped (--skip-drift) — another job must have run check-docs-drift.mjs");
	} else {
		const drift = spawnSync(process.execPath, [path.join(repoRoot, DEFAULTS.driftCheck)], {
			cwd: repoRoot,
			encoding: "utf8",
			stdio: "pipe",
		});
		if (drift.error) {
			report.fail(DEFAULTS.driftCheck, `could not be run — ${drift.error.message}`);
		} else if (drift.status !== 0) {
			report.fail(
				DEFAULTS.driftCheck,
				`the generated docs do not match docs/commands/ (exit ${drift.status}):\n` +
					indent(drift.stdout ? `${drift.stdout}\n${drift.stderr}` : drift.stderr),
			);
		} else {
			report.note("  ok  generated docs match the registry (check-docs-drift)");
		}
	}

	// --- settle the three agreements ----------------------------------------

	version.settle();
	protocol.settle();
	const agreedCommit = commit.settle();

	const expected = values["expect-version"] ?? tagVersion();
	if (expected) {
		const wanted = expected.replace(/^v/, "");
		const claimed = [...new Set(version.claims.map((claim) => claim.value))];
		if (claimed.length && !claimed.includes(wanted)) {
			report.fail(
				"version",
				`the release is tagged ${expected} but nothing in it says ${wanted} (found ${claimed.join(", ")})`,
			);
		}
	}

	// The stamp probe, last, because it only means anything once the commit the
	// artifacts agree on is known.
	if (binaryBytes && agreedCommit) {
		if (agreedCommit === UNKNOWN_COMMIT) {
			report.note(
				`  --  binary commit stamp not probed: the recorded commit is "${UNKNOWN_COMMIT}", ` +
					"a word that appears throughout any binary and would prove nothing",
			);
		} else if (!/^[0-9a-f]{7,40}$/.test(agreedCommit)) {
			report.warn(`build commit "${agreedCommit}" is not a hex sha — the binary stamp probe was skipped`);
		} else if (binaryBytes.includes(Buffer.from(agreedCommit, "utf8"))) {
			report.note(
				`  ok  ${relative(binPath)} carries the string "${agreedCommit}" (build.rs stamps ` +
					"WSYNC_BUILD_COMMIT into a live code path; corroboration, not proof)",
			);
		} else {
			report.fail(
				relative(binPath),
				`does not contain the commit "${agreedCommit}" that the rest of the release records. ` +
					"build.rs stamps WSYNC_BUILD_COMMIT with `git rev-parse --short=12` into a string " +
					"literal `GET /hello` reads, so a binary built from this commit contains it — " +
					"this one was built from a different commit, or outside the repository.",
			);
		}
	}

	report.finish(
		agreedCommit
			? `binary, plugin, protocol and docs all state commit ${agreedCommit}`
			: "release artifacts agree",
	);
}

/** `refs/tags/v1.2.3` → `v1.2.3`, on a tag build only. */
function tagVersion() {
	if (process.env.GITHUB_REF_TYPE === "tag" && process.env.GITHUB_REF_NAME) return process.env.GITHUB_REF_NAME;
	const ref = process.env.GITHUB_REF ?? "";
	return ref.startsWith("refs/tags/") ? ref.slice("refs/tags/".length) : null;
}

main().catch((err) => {
	console.error(`${NAME}: ${err.stack || err.message || err}`);
	process.exit(1);
});
