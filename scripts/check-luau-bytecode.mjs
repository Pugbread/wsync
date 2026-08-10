#!/usr/bin/env node
// check-luau-bytecode.mjs — every plugin source must compile, at every -O level.
//
// The Studio plugin ships as bytecode built by Roblox's own Luau compiler. A
// file that parses in an editor can still fail to compile, and an optimisation
// level can reject what -O0 accepted (constant folding, upvalue analysis and
// inlining all see code the parser does not). Studio builds at -O2, so testing
// only the default level tests the wrong thing. Each file is compiled at -O0,
// -O1 and -O2 with `luau-compile --null` (compile, discard the output).
//
// This is a *compile* check, not a lint and not a type-check: selene and the
// Luau analyser cover those. It answers one question — can Studio load this.
//
// Compiler resolution, in order:
//   1. --luau-compile <path>
//   2. $LUAU_COMPILE
//   3. `luau-compile` on PATH
//   4. the pinned local reference build (developer convenience on this machine)
//   5. otherwise: print a loud SKIP and exit 0
//
// (5) exists so a contributor without the toolchain is not blocked; CI must not
// rely on it, which is why CI passes --luau-compile explicitly. An *explicit*
// compiler that cannot be executed is a hard failure — a CI download that
// silently half-worked must never look like a pass.
//
// Node >= 18, zero dependencies. Exit 1 when any file fails to compile.

import { spawnSync } from "node:child_process";
import os from "node:os";
import path from "node:path";
import { Reporter, helpFlag, parseArgs, plural, renderHelp, rel, repoRoot, walk } from "./lib/policy.mjs";

const NAME = "check-luau-bytecode";
const DEFAULT_DIR = "plugin/src";
const OPTIMIZATION_LEVELS = ["0", "1", "2"];

// A local reference build, not part of the repo and not part of CI. Present on
// the maintainer's machine; absent everywhere else, where resolution simply
// falls through to the SKIP.
const LOCAL_FALLBACK = path.join(
	os.homedir(),
	".terminal64/widgets/ro-sync/tools/luau/darwin-arm64/luau-compile",
);

const SPEC = {
	"luau-compile": {
		type: "string",
		valueName: "path",
		description: "Path to the luau-compile binary. Highest-priority source; must be runnable.",
	},
	dir: {
		type: "string",
		valueName: "path",
		description: `Directory to scan for *.luau, relative to the repo root (default: ${DEFAULT_DIR}).`,
	},
	"require-compiler": {
		type: "boolean",
		description: "Turn the 'no compiler found' skip into a failure. Belt and braces for CI.",
	},
	help: helpFlag,
};

function help() {
	return renderHelp({
		name: NAME,
		summary: `compiles every ${DEFAULT_DIR}/**/*.luau at -O0, -O1 and -O2`,
		usage: "node scripts/check-luau-bytecode.mjs [--luau-compile <path>] [--dir <path>] [--require-compiler]",
		spec: SPEC,
		sections: [
			{
				title: "Compiler resolution",
				body: [
					"1. --luau-compile <path>",
					"2. $LUAU_COMPILE",
					"3. `luau-compile` on PATH",
					"4. a pinned local reference build (this machine only)",
					"5. otherwise: SKIP with a warning, exit 0",
					"",
					"Sources 1 and 2 are explicit: if they name something that cannot be run,",
					"that is a failure, never a skip. Only the implicit chain reaches the SKIP.",
					"CI pins and downloads its own luau-compile and passes --luau-compile.",
				],
			},
			{
				title: "Reporting",
				body: [
					"Each level is compiled in one batched invocation. If a level fails, the",
					"files are recompiled one at a time so the report names every bad file,",
					"not just the first, with the compiler's own diagnostics.",
				],
			},
		],
	});
}

/** True when `candidate` can actually be executed. */
function isRunnable(candidate) {
	const probe = spawnSync(candidate, ["--help"], { encoding: "utf8", stdio: "pipe" });
	return !probe.error;
}

/**
 * Returns `{ compiler, source }`, `{ error }` for an explicit-but-broken
 * compiler, or `{ skip: true }` when nothing is available.
 */
function resolveCompiler(explicit) {
	for (const [value, source] of [
		[explicit, "--luau-compile"],
		[process.env.LUAU_COMPILE, "$LUAU_COMPILE"],
	]) {
		if (!value) continue;
		const resolved = path.resolve(value);
		if (isRunnable(resolved)) return { compiler: resolved, source };
		return {
			error:
				`${source} points at "${value}", which cannot be executed. ` +
				"Explicitly-selected compilers are never skipped.",
		};
	}

	if (isRunnable("luau-compile")) return { compiler: "luau-compile", source: "PATH" };
	if (isRunnable(LOCAL_FALLBACK)) return { compiler: LOCAL_FALLBACK, source: "local reference build" };
	return { skip: true };
}

/** Compiles `files` at one optimisation level. Returns the spawn result. */
function compile(compiler, level, files) {
	return spawnSync(compiler, ["--null", `-O${level}`, ...files], {
		encoding: "utf8",
		stdio: "pipe",
		maxBuffer: 32 * 1024 * 1024,
	});
}

const indent = (text) =>
	String(text ?? "")
		.trimEnd()
		.split("\n")
		.map((line) => `      ${line}`)
		.join("\n");

async function main() {
	const { values, positionals, errors } = parseArgs(process.argv.slice(2), SPEC);
	if (values.help) {
		console.log(help());
		return;
	}

	const report = new Reporter(NAME);
	for (const error of errors) report.fail("arguments", error);
	for (const extra of positionals) report.fail("arguments", `unexpected argument "${extra}"`);
	if (report.failures.length) report.finish();

	const scanDir = path.resolve(repoRoot, values.dir ?? DEFAULT_DIR);
	const files = await walk(scanDir, { filter: (f) => f.endsWith(".luau") });
	if (!files.length) {
		report.fail(rel(scanDir), "no *.luau files found — wrong --dir, or the plugin tree is missing");
		report.finish();
	}

	const resolution = resolveCompiler(values["luau-compile"]);
	if (resolution.error) {
		report.fail("compiler", resolution.error);
		report.finish();
	}
	if (resolution.skip) {
		const message =
			"no luau-compile found (checked --luau-compile, $LUAU_COMPILE, PATH, and the local " +
			`reference build). ${plural(files.length, "plugin file")} went UNCHECKED.`;
		if (values["require-compiler"]) {
			report.fail("compiler", `${message} --require-compiler was set, so this is a failure.`);
			report.finish();
		}
		console.warn("");
		console.warn(`${NAME}: SKIP — ${message}`);
		console.warn("  Install luau-compile (https://github.com/luau-lang/luau/releases) or pass");
		console.warn("  --luau-compile <path>. CI pins its own build and never takes this branch.");
		process.exit(0);
	}

	const { compiler, source } = resolution;
	report.note(`compiler: ${compiler} (via ${source})`);
	report.note(`sources : ${plural(files.length, "file")} under ${rel(scanDir)}/`);

	for (const level of OPTIMIZATION_LEVELS) {
		const batch = compile(compiler, level, files);
		if (batch.error) {
			report.fail(`-O${level}`, `could not run the compiler: ${batch.error.message}`);
			break;
		}
		if (batch.status === 0) {
			report.note(`  ok  -O${level}  ${plural(files.length, "file")}`);
			continue;
		}

		// Batched runs report every failing file, but re-running one file at a
		// time is what makes the report unambiguous about which files are fine.
		const failed = [];
		for (const file of files) {
			const single = compile(compiler, level, [file]);
			if (single.status !== 0) {
				failed.push(rel(file));
				report.fail(
					`${rel(file)} -O${level}`,
					`failed to compile\n${indent(single.stderr || single.stdout || "<no diagnostics>")}`,
				);
			}
		}
		if (!failed.length) {
			// Batched run failed but no single file does: an aggregate-only problem
			// (argument list, compiler crash). Surface it verbatim rather than
			// pretending the level passed.
			report.fail(
				`-O${level}`,
				`batched compile exited ${batch.status} but every file compiles alone\n${indent(batch.stderr || batch.stdout)}`,
			);
		}
	}

	report.finish(
		`${plural(files.length, "file")} compile at -O${OPTIMIZATION_LEVELS.join(", -O")}`,
	);
}

main().catch((err) => {
	console.error(`${NAME}: ${err.stack || err.message || err}`);
	process.exit(1);
});
