#!/usr/bin/env node
// check-docs-drift.mjs — the generated command docs must match the registry.
//
// Design 10.5: `docs/commands/<name>.json` + `index.json` are the single source
// of truth; `docs/client-commands.md` and `docs/client-commands.generated.json`
// are build products that get embedded verbatim into the binary and rendered by
// the desktop Docs view. If someone edits a generated file by hand, or edits a
// registry entry without re-running the generator, those consumers ship a lie.
//
// The generator resolves its own paths from `import.meta.url` and always writes
// in place, so this check can't ask it for a different output directory. It
// instead builds a throwaway workspace — a copy of `docs/commands/` plus a copy
// of the generator — runs the generator *there*, and byte-compares its two
// outputs against the checked-in ones. The repo tree is never written to.
//
// Node >= 18, zero dependencies. Exit 1 on drift or on a generator failure.

import { spawnSync } from "node:child_process";
import { copyFile, mkdir, mkdtemp, readdir, readFile, rm } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { Reporter, helpFlag, isFile, parseArgs, renderHelp, repoRoot } from "./lib/policy.mjs";

const NAME = "check-docs-drift";
const GENERATOR = "scripts/build-command-docs.mjs";
const REGISTRY_DIR = "docs/commands";
const GENERATED = ["docs/client-commands.generated.json", "docs/client-commands.md"];

const SPEC = {
	keep: {
		type: "boolean",
		description: "Keep the temporary workspace and print its path (for debugging drift).",
	},
	help: helpFlag,
};

function help() {
	return renderHelp({
		name: NAME,
		summary:
			"fails when the checked-in generated command docs differ from what the registry produces",
		usage: "node scripts/check-docs-drift.mjs [--keep]",
		spec: SPEC,
		sections: [
			{
				title: "What it enforces",
				body: [
					`${GENERATED[0]} and ${GENERATED[1]} are byte-identical to a`,
					`fresh run of ${GENERATOR} over ${REGISTRY_DIR}/.`,
					"",
					"The generator runs in a temporary copy of the tree, so this check never",
					"modifies the repository. Fix a failure by running the generator for real",
					"and committing its output:",
					`  node ${GENERATOR}`,
				],
			},
		],
	});
}

/** Recursive copy — `fs.cp` is still flagged experimental on Node 18. */
async function copyTree(from, to) {
	await mkdir(to, { recursive: true });
	for (const entry of await readdir(from, { withFileTypes: true })) {
		const src = path.join(from, entry.name);
		const dest = path.join(to, entry.name);
		if (entry.isDirectory()) await copyTree(src, dest);
		else if (entry.isFile()) await copyFile(src, dest);
	}
}

const indent = (text) =>
	String(text ?? "")
		.trimEnd()
		.split("\n")
		.map((line) => `      ${line}`)
		.join("\n");

/**
 * Describes the first difference between two buffers in a way that points at
 * the actual edit: line number, and both versions of that line.
 */
function describeDifference(expected, actual) {
	if (expected.equals(actual)) return null;

	let offset = 0;
	const limit = Math.min(expected.length, actual.length);
	while (offset < limit && expected[offset] === actual[offset]) offset++;

	const line = expected.subarray(0, offset).toString("utf8").split("\n").length;
	const pick = (buffer) => buffer.toString("utf8").split("\n")[line - 1] ?? "<end of file>";
	const clip = (s) => (s.length > 160 ? `${s.slice(0, 157)}...` : s);

	return (
		`first difference at line ${line} (byte ${offset}); ` +
		`checked-in ${actual.length} B vs regenerated ${expected.length} B` +
		`\n      checked in  : ${clip(pick(actual))}` +
		`\n      regenerated : ${clip(pick(expected))}`
	);
}

/**
 * Runs the generator in a temp workspace. Returns
 * `{ outputs: Map<file, Buffer> }` or `{ error: string }` — never throws for a
 * generator-level failure, so the caller can report it alongside anything else.
 */
async function regenerate(keepWorkspace) {
	const workspace = await mkdtemp(path.join(os.tmpdir(), "wsync-docs-drift-"));
	try {
		// The generator anchors itself at <script dir>/.., so the workspace has
		// to mirror the repo layout: scripts/ next to docs/.
		await mkdir(path.join(workspace, "scripts"), { recursive: true });
		await copyFile(
			path.join(repoRoot, GENERATOR),
			path.join(workspace, "scripts", path.basename(GENERATOR)),
		);
		await copyTree(path.join(repoRoot, REGISTRY_DIR), path.join(workspace, REGISTRY_DIR));

		const run = spawnSync(process.execPath, [path.join(workspace, GENERATOR)], {
			cwd: workspace,
			encoding: "utf8",
			stdio: "pipe",
		});

		if (run.error) {
			return { error: `could not run the generator: ${run.error.message}` };
		}
		if (run.status !== 0) {
			return {
				error:
					`the registry does not build (generator exit ${run.status}). Output:\n` +
					indent(run.stderr || run.stdout || "<no output>"),
			};
		}

		const outputs = new Map();
		const missing = [];
		for (const file of GENERATED) {
			const produced = path.join(workspace, file);
			if (await isFile(produced)) outputs.set(file, await readFile(produced));
			else missing.push(file);
		}
		if (missing.length) {
			return { error: `the generator did not produce ${missing.join(", ")}` };
		}
		return { outputs };
	} finally {
		if (keepWorkspace) console.log(`kept temporary workspace: ${workspace}`);
		else await rm(workspace, { recursive: true, force: true });
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
	if (report.failures.length) report.finish();

	if (!(await isFile(path.join(repoRoot, GENERATOR)))) {
		report.fail(GENERATOR, "generator is missing — cannot verify the generated docs");
		report.finish();
	}

	const { outputs, error } = await regenerate(values.keep);
	if (error) {
		report.fail(GENERATOR, error);
		report.finish();
	}

	for (const file of GENERATED) {
		const checkedInPath = path.join(repoRoot, file);
		if (!(await isFile(checkedInPath))) {
			report.fail(file, "generated file is missing from the repo — run the generator and commit it");
			continue;
		}
		const actual = await readFile(checkedInPath);
		const difference = describeDifference(outputs.get(file), actual);
		if (difference) report.fail(file, `drifted from ${REGISTRY_DIR}/ — ${difference}`);
		else report.note(`  ok  ${file} (${actual.length} B)`);
	}

	if (report.failures.length) {
		report.fail(
			"how to fix",
			`run \`node ${GENERATOR}\` and commit the result (never hand-edit the generated files)`,
		);
	}

	const entries = (await readdir(path.join(repoRoot, REGISTRY_DIR))).filter(
		(f) => f.endsWith(".json") && f !== "index.json",
	).length;
	report.note(`checked ${GENERATED.length} generated files against ${entries} registry entries`);
	report.finish(`generated docs match ${REGISTRY_DIR}/`);
}

main().catch((err) => {
	console.error(`${NAME}: ${err.stack || err.message || err}`);
	process.exit(1);
});
