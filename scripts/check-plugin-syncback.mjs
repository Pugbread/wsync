#!/usr/bin/env node
// check-plugin-syncback.mjs — the plugin's syncback core, exercised outside Studio.
//
// The Studio plugin's live syncback (Watcher → ReadProcessor → Tree) decides
// what reaches disk when instances move, and a wrong decision there deletes
// files. Nothing about it needs Studio: the modules are plain Luau over a
// handful of Instance methods and three services. Each scenario under
// plugin/tests/*.luau runs the *real* modules over a fake DataModel
// (plugin/tests/harness/fakes.luau) with the standalone `luau` binary — the
// same approach the require-free Protocol/Policy modules use, extended to the
// impure ones by loading each source through `loadstring` with its own
// `script` and `require` (plugin/tests/harness/loader.luau).
//
// What is real: Watcher, the Processor with its Read and Write halves, Tree,
// Changes, Snapshot, Meta, both Error modules, Util, Transport/Hex, SyncScope,
// the equals/generateRef helpers (and Log, loadable as `RealLog`). What is
// stubbed: Log, Config, Dom (property defaults for the code-scope classes),
// the Signal package, and the Roblox globals. A scenario fails by calling
// `error`; its output is reported verbatim.
//
// Runner resolution, in order:
//   1. --luau <path>
//   2. $LUAU
//   3. `luau` on PATH
//   4. otherwise: print a loud SKIP and exit 0 (--require-luau makes it fail)
//
// Node >= 18, zero dependencies. Exit 1 when any scenario fails.

import { spawnSync } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import { Reporter, helpFlag, parseArgs, plural, renderHelp, rel, repoRoot, walk } from "./lib/policy.mjs";

const NAME = "check-plugin-syncback";
const TESTS_DIR = "plugin/tests";
const HARNESS_DIR = "plugin/tests/harness";
const SOURCE_DIR = "plugin/src";

// The plugin modules the loader can serve, by the name the scenarios and the
// loader's module refs use. Add a module here when a scenario needs it.
const MODULES = {
	Tree: "Core/Tree.luau",
	Changes: "Core/Changes.luau",
	Meta: "Core/Meta.luau",
	Snapshot: "Core/Snapshot.luau",
	Error: "Core/Error.luau",
	TransportError: "Transport/Error.luau",
	Hex: "Transport/Hex.luau",
	Util: "Util.luau",
	RealLog: "Log.luau",
	equals: "Helpers/equals.luau",
	generateRef: "Helpers/generateRef.luau",
	SyncScope: "SyncScope.luau",
	Processor: "Core/Processor/init.luau",
	Read: "Core/Processor/Read.luau",
	Write: "Core/Processor/Write.luau",
	Watcher: "Watcher.luau",
	DivergenceProtocol: "Core/Divergence/Protocol.luau",
	DraftRequire: "Remote/DraftRequire.luau",
};

const SPEC = {
	luau: {
		type: "string",
		valueName: "path",
		description: "Path to the luau binary. Highest-priority source; must be runnable.",
	},
	"require-luau": {
		type: "boolean",
		description: "Turn the 'no luau found' skip into a failure. Belt and braces for CI.",
	},
	keep: {
		type: "boolean",
		description: "Keep the generated per-scenario files and print where they are.",
	},
	help: helpFlag,
};

function help() {
	return renderHelp({
		name: NAME,
		summary: `runs every ${TESTS_DIR}/*.luau scenario over the real plugin modules with the luau binary`,
		usage: "node scripts/check-plugin-syncback.mjs [--luau <path>] [--require-luau] [--keep]",
		spec: SPEC,
		sections: [
			{
				title: "Runner resolution",
				body: [
					"1. --luau <path>",
					"2. $LUAU",
					"3. `luau` on PATH",
					"4. otherwise: SKIP with a warning, exit 0",
					"",
					"Sources 1 and 2 are explicit: if they name something that cannot be run,",
					"that is a failure, never a skip. CI downloads the pinned luau release,",
					"which ships `luau` next to `luau-compile`, and passes --luau.",
				],
			},
			{
				title: "Writing a scenario",
				body: [
					`Drop a *.luau file in ${TESTS_DIR}/. It receives \`Fakes\`, \`ref\` and`,
					"`require` (see harness/loader.luau); `require(ref(\"Read\"))` loads the real",
					"module. Report a failure with `error(...)`; print freely, output is shown.",
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

function resolveRunner(explicit) {
	for (const [value, source] of [
		[explicit, "--luau"],
		[process.env.LUAU, "$LUAU"],
	]) {
		if (!value) continue;
		const resolved = path.resolve(value);
		if (isRunnable(resolved)) return { runner: resolved, source };
		return {
			error: `${source} points at "${value}", which cannot be executed. Explicitly-selected runners are never skipped.`,
		};
	}

	if (isRunnable("luau")) return { runner: "luau", source: "PATH" };
	return { skip: true };
}

const FENCE_OPEN = "[=====[\n";
const FENCE_CLOSE = "]=====]";

/** A Luau long string carrying `text` verbatim. */
function fence(text, where) {
	if (text.includes(FENCE_CLOSE)) {
		throw new Error(`${where} contains the long-string fence ${FENCE_CLOSE}`);
	}
	return `${FENCE_OPEN}${text}${text.endsWith("\n") ? "" : "\n"}${FENCE_CLOSE}`;
}

/** One self-contained luau file: sources, fakes, the scenario, the loader. */
function assemble(scenarioPath) {
	const read = (p) => readFileSync(p, "utf8");
	const lines = ["local SOURCES = {"];
	for (const [name, source] of Object.entries(MODULES)) {
		lines.push(`\t${name} = ${fence(read(path.join(repoRoot, SOURCE_DIR, source)), source)},`);
	}
	lines.push("}");
	lines.push(`local FAKES_SOURCE = ${fence(read(path.join(repoRoot, HARNESS_DIR, "fakes.luau")), "fakes.luau")}`);
	lines.push(`local SCENARIO_SOURCE = ${fence(read(scenarioPath), rel(scenarioPath))}`);
	lines.push(read(path.join(repoRoot, HARNESS_DIR, "loader.luau")));
	return lines.join("\n");
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

	const testsDir = path.resolve(repoRoot, TESTS_DIR);
	const scenarios = (await walk(testsDir, { filter: (f) => f.endsWith(".luau") })).filter(
		(f) => path.dirname(f) === testsDir,
	);
	if (!scenarios.length) {
		report.fail(rel(testsDir), "no scenario *.luau files found");
		report.finish();
	}

	const resolution = resolveRunner(values.luau);
	if (resolution.error) {
		report.fail("runner", resolution.error);
		report.finish();
	}
	if (resolution.skip) {
		const message = `no luau binary found (checked --luau, $LUAU and PATH). ${plural(scenarios.length, "scenario")} went UNCHECKED.`;
		if (values["require-luau"]) {
			report.fail("runner", `${message} --require-luau was set, so this is a failure.`);
			report.finish();
		}
		console.warn("");
		console.warn(`${NAME}: SKIP — ${message}`);
		console.warn("  Install luau (https://github.com/luau-lang/luau/releases) or pass --luau <path>.");
		console.warn("  CI pins its own build and never takes this branch.");
		process.exit(0);
	}

	const { runner, source } = resolution;
	report.note(`runner   : ${runner} (via ${source})`);
	report.note(`scenarios: ${plural(scenarios.length, "file")} under ${rel(testsDir)}/`);

	const workDir = mkdtempSync(path.join(os.tmpdir(), "wsync-plugin-syncback-"));

	for (const scenario of scenarios) {
		const generated = path.join(workDir, path.basename(scenario));
		writeFileSync(generated, assemble(scenario));

		const run = spawnSync(runner, [generated], { encoding: "utf8", stdio: "pipe", maxBuffer: 32 * 1024 * 1024 });
		const output = `${run.stdout ?? ""}${run.stderr ?? ""}`;

		if (run.error) {
			report.fail(rel(scenario), `could not run luau: ${run.error.message}`);
		} else if (run.status !== 0) {
			report.fail(rel(scenario), `scenario failed (exit ${run.status})\n${indent(output)}`);
		} else {
			report.note(`  ok  ${rel(scenario)}`);
			for (const line of output.trimEnd().split("\n")) report.note(`      ${line}`);
		}
	}

	if (values.keep) {
		report.note(`generated files kept under ${workDir}`);
	} else {
		rmSync(workDir, { recursive: true, force: true });
	}

	report.finish(`${plural(scenarios.length, "scenario")} passed over the real plugin modules`);
}

main().catch((err) => {
	console.error(`${NAME}: ${err.stack || err.message || err}`);
	process.exit(1);
});
