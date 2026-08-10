#!/usr/bin/env node
// check-command-validation.mjs — the command registry vs. the real binary.
//
// Design 10.5 asks for a check that "cross-examines the registry against the
// real binary — a static *classify* mode (every documented command and flag
// must parse) and a *live* mode". This is the classify half, plus the first
// slice of the binary cross-check.
//
// Two facts have to stay reconciled:
//
//   docs/commands/*.json          the DESIGNED surface (71 commands, Design 10.2)
//   scripts/data/implemented-commands.json   the BUILT surface (Design 15 phase state)
//
// The registry is intentionally ahead of the binary while phases land, so a
// registry entry with no implementation is *reported*, never fatal. The failures
// are the ones that mean something is actually wrong:
//
//   * a manifest command with no registry entry  -> shipping an undocumented command
//   * (--bin) an advertised command missing from the manifest or the registry
//   * (--bin) a manifest command the binary does not advertise
//
// Node >= 18, zero dependencies. Exit 1 on any failure above.

import { spawnSync } from "node:child_process";
import { readFile } from "node:fs/promises";
import path from "node:path";
import {
	Reporter,
	helpFlag,
	isFile,
	parseArgs,
	plural,
	renderHelp,
	repoRoot,
} from "./lib/policy.mjs";

const NAME = "check-command-validation";
const BUNDLE = "docs/client-commands.generated.json";
const MANIFEST = "scripts/data/implemented-commands.json";
const CLI_SOURCE = "src/cli/mod.rs";

const SPEC = {
	bin: {
		type: "string",
		valueName: "path",
		description:
			"Also run `<path> --help` and cross-check the advertised subcommands (fails on drift).",
	},
	"list-missing": {
		type: "boolean",
		description: "Print every registry command that is not implemented yet (default: first 12).",
	},
	json: {
		type: "boolean",
		description: "Emit the reconciliation as JSON on stdout instead of a human report.",
	},
	help: helpFlag,
};

function help() {
	return renderHelp({
		name: NAME,
		summary: "reconciles the command registry, the implemented-command manifest, and the binary",
		usage: "node scripts/check-command-validation.mjs [--bin <path>] [--list-missing] [--json]",
		spec: SPEC,
		sections: [
			{
				title: "Modes",
				body: [
					"classify (default) — static only, no execution. Runs anywhere, including",
					"  a checkout with no compiled binary. This is what CI runs today.",
					"",
					"--bin <path>     — additionally executes `<path> --help` and reconciles the",
					"  advertised top-level subcommands against both the manifest and the",
					"  registry. Wire this up in CI once the rust job artifacts the binary.",
				],
			},
			{
				title: "Fails when",
				body: [
					`a ${MANIFEST} entry has no docs/commands/<name>.json`,
					`${MANIFEST} is malformed, unsorted, or has duplicates`,
					"--bin: the binary advertises a command absent from the manifest or the registry",
					"--bin: the manifest lists a command the binary does not advertise",
				],
			},
			{
				title: "Reports without failing",
				body: [
					"registry commands with no implementation yet (expected while phases land)",
					`a ${MANIFEST} that disagrees with \`enum Commands\` in ${CLI_SOURCE}`,
				],
			},
		],
	});
}

async function readJson(relativePath, report) {
	const full = path.join(repoRoot, relativePath);
	if (!(await isFile(full))) {
		report.fail(relativePath, "file is missing");
		return null;
	}
	try {
		return JSON.parse(await readFile(full, "utf8"));
	} catch (err) {
		report.fail(relativePath, `invalid JSON — ${err.message}`);
		return null;
	}
}

/** Validates the manifest shape and returns `{ commands, aliases }` or null. */
function readManifest(raw, report) {
	if (!raw || typeof raw !== "object" || Array.isArray(raw)) {
		report.fail(MANIFEST, "must contain a JSON object");
		return null;
	}
	if (!Array.isArray(raw.commands)) {
		report.fail(MANIFEST, 'must have a "commands" array');
		return null;
	}

	const commands = [];
	const seen = new Set();
	for (const [i, entry] of raw.commands.entries()) {
		if (typeof entry !== "string" || !entry.trim()) {
			report.fail(MANIFEST, `commands[${i}] must be a non-empty string`);
			continue;
		}
		if (seen.has(entry)) report.fail(MANIFEST, `duplicate entry "${entry}"`);
		seen.add(entry);
		commands.push(entry);
	}

	const sorted = [...commands].sort();
	if (commands.join("\0") !== sorted.join("\0")) {
		report.fail(MANIFEST, `"commands" must be sorted — expected order: ${sorted.join(", ")}`);
	}

	const aliases = new Map();
	if (raw.aliases !== undefined) {
		if (typeof raw.aliases !== "object" || raw.aliases === null || Array.isArray(raw.aliases)) {
			report.fail(MANIFEST, '"aliases" must be an object mapping alias -> canonical name');
		} else {
			for (const [alias, canonical] of Object.entries(raw.aliases)) {
				if (typeof canonical !== "string" || !seen.has(canonical)) {
					report.fail(
						MANIFEST,
						`alias "${alias}" points at "${canonical}", which is not a manifest command`,
					);
					continue;
				}
				aliases.set(alias, canonical);
			}
		}
	}

	return { commands, aliases };
}

/**
 * Pulls the top-level command names out of `<bin> --help`.
 *
 * clap 4 renders the block as:
 *
 *     Commands:
 *       init       Initialize a new project
 *       serve, s   Start the sync server
 *
 * Entries are the lines at the *first* entry's exact indentation, so wrapped
 * description continuations (which clap indents further) are not mistaken for
 * commands. Comma-separated extras on an entry line are visible aliases.
 *
 * A line at entry indentation that does not parse is returned in `unparsed`
 * rather than skipped. Silently ignoring output this parser does not understand
 * is how a cross-check quietly stops checking anything.
 */
const ENTRY = /^([a-z0-9][a-z0-9._-]*(?:\s*,\s*[a-z0-9][a-z0-9._-]*)*)(?:\s|$)/i;

function parseAdvertisedCommands(helpText) {
	const lines = helpText.split(/\r?\n/);
	const start = lines.findIndex((line) => /^\s*(commands|subcommands):\s*$/i.test(line));
	if (start === -1) return null;

	const commands = [];
	const aliases = new Map();
	const unparsed = [];
	let entryIndent = null;

	for (const line of lines.slice(start + 1)) {
		if (!line.trim()) break;
		const indent = line.length - line.trimStart().length;
		if (indent === 0) break;
		if (entryIndent === null) entryIndent = indent;
		if (indent !== entryIndent) continue; // wrapped description line

		const match = line.trim().match(ENTRY);
		if (!match) {
			unparsed.push(line.trim());
			continue;
		}
		const names = match[1]
			.split(",")
			.map((n) => n.trim())
			.filter(Boolean);
		commands.push(names[0]);
		for (const alias of names.slice(1)) aliases.set(alias, names[0]);
	}

	return { commands, aliases, unparsed };
}

/**
 * Best-effort read of `enum Commands` in src/cli/mod.rs, used only to warn that
 * the manifest looks stale. Report-only and silently skipped when the shape is
 * not what we expect — src/ belongs to the engine work and this check must
 * never fail CI because someone restructured it.
 */
async function readCliSourceCommands() {
	const full = path.join(repoRoot, CLI_SOURCE);
	if (!(await isFile(full))) return null;
	const source = await readFile(full, "utf8");
	const block = source.match(/pub enum Commands\s*\{([\s\S]*?)\n\}/);
	if (!block) return null;

	const names = [];
	for (const line of block[1].split("\n")) {
		const match = line.trim().match(/^([A-Z][A-Za-z0-9]*)\s*\(/);
		if (match) names.push(match[1]);
	}
	if (!names.length) return null;
	// `FindAttr` -> `find-attr`; `Init` -> `init`.
	return names
		.map((name) =>
			name
				.replace(/([a-z0-9])([A-Z])/g, "$1-$2")
				.toLowerCase(),
		)
		.sort();
}

const difference = (a, b) => a.filter((item) => !b.has(item));

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

	const bundle = await readJson(BUNDLE, report);
	const manifestRaw = await readJson(MANIFEST, report);
	if (report.failures.length) report.finish();

	if (!bundle || !Array.isArray(bundle.commands)) {
		report.fail(BUNDLE, 'must be the generated bundle with a "commands" array');
		report.finish();
	}
	const registry = bundle.commands.map((c) => c?.name).filter((n) => typeof n === "string");
	const registrySet = new Set(registry);
	if (registry.length !== bundle.commands.length) {
		report.fail(BUNDLE, "some registry entries have no name — regenerate the docs bundle");
	}

	const manifest = readManifest(manifestRaw, report);
	if (!manifest || report.failures.length) report.finish();
	const manifestSet = new Set(manifest.commands);

	// --- classify: manifest ⊆ registry -------------------------------------
	const undocumented = difference(manifest.commands, registrySet);
	for (const name of undocumented) {
		report.fail(
			MANIFEST,
			`"${name}" is implemented but has no registry entry — add docs/commands/${name}.json ` +
				`and list it in docs/commands/index.json`,
		);
	}

	// --- classify: registry \ manifest (reported, never fatal) -------------
	const unimplemented = difference(registry, manifestSet);

	// --- classify: manifest vs. the CLI source (warning only) --------------
	const sourceCommands = await readCliSourceCommands();
	if (sourceCommands) {
		const sourceSet = new Set(sourceCommands);
		const addedInSource = difference(sourceCommands, manifestSet);
		const goneFromSource = difference(manifest.commands, sourceSet);
		if (addedInSource.length) {
			report.warn(
				`${CLI_SOURCE} declares ${plural(addedInSource.length, "command")} missing from ` +
					`${MANIFEST}: ${addedInSource.join(", ")} — update the manifest`,
			);
		}
		if (goneFromSource.length) {
			report.warn(
				`${MANIFEST} lists ${plural(goneFromSource.length, "command")} not declared in ` +
					`${CLI_SOURCE}: ${goneFromSource.join(", ")} — update the manifest`,
			);
		}
	}

	// --- --bin: reconcile against what the binary advertises ---------------
	let advertised = null;
	if (values.bin !== undefined) {
		const binary = path.resolve(values.bin);
		if (!(await isFile(binary))) {
			report.fail("--bin", `no such file: ${binary}`);
		} else {
			const run = spawnSync(binary, ["--help"], { encoding: "utf8", stdio: "pipe" });
			if (run.error) {
				report.fail("--bin", `could not run ${binary} --help: ${run.error.message}`);
			} else if (run.status !== 0) {
				report.fail("--bin", `${binary} --help exited ${run.status}: ${(run.stderr || "").trim()}`);
			} else {
				const parsed = parseAdvertisedCommands(`${run.stdout}\n${run.stderr}`);
				if (!parsed || !parsed.commands.length) {
					report.fail(
						"--bin",
						`could not find a "Commands:" section in ${binary} --help — the parser needs ` +
							"updating, or the binary advertises no subcommands",
					);
				} else {
					advertised = parsed;
					for (const line of parsed.unparsed) {
						report.fail(
							"--bin",
							`could not parse this line of the "Commands:" block: "${line}" — the parser ` +
								"needs updating; refusing to report a pass on output it does not understand",
						);
					}
					// clap always adds `help`; it is not part of the surface.
					const names = parsed.commands.filter((n) => n !== "help");
					const advertisedSet = new Set(names);

					for (const name of difference(names, registrySet)) {
						report.fail(
							"--bin",
							`the binary advertises "${name}" but the registry has no docs/commands/${name}.json`,
						);
					}
					for (const name of difference(names, manifestSet)) {
						report.fail(
							"--bin",
							`the binary advertises "${name}" but ${MANIFEST} does not list it — add it`,
						);
					}
					for (const name of difference(manifest.commands, advertisedSet)) {
						report.fail(
							"--bin",
							`${MANIFEST} lists "${name}" but the binary does not advertise it — ` +
								"remove it from the manifest or wire the command up",
						);
					}
					for (const [alias, canonical] of parsed.aliases) {
						if (manifest.aliases.get(alias) !== canonical) {
							report.warn(
								`the binary advertises alias "${alias}" -> "${canonical}"; ` +
									`record it in the "aliases" map of ${MANIFEST}`,
							);
						}
					}
				}
			}
		}
	}

	// --- output -------------------------------------------------------------
	if (values.json) {
		console.log(
			JSON.stringify(
				{
					mode: values.bin === undefined ? "classify" : "classify+bin",
					registry: registry.length,
					implemented: manifest.commands.length,
					unimplemented,
					undocumented,
					advertised: advertised ? advertised.commands.filter((n) => n !== "help") : null,
					ok: report.failures.length === 0,
					failures: report.failures,
					warnings: report.warnings,
				},
				null,
				2,
			),
		);
		process.exit(report.failures.length ? 1 : 0);
	} else {
		const shown = values["list-missing"] ? unimplemented : unimplemented.slice(0, 12);
		report.note(`registry     : ${plural(registry.length, "command")} (${BUNDLE})`);
		report.note(`implemented  : ${plural(manifest.commands.length, "command")} (${MANIFEST})`);
		report.note(`  ${manifest.commands.join(", ")}`);
		if (advertised) {
			const names = advertised.commands.filter((n) => n !== "help");
			report.note(`advertised   : ${plural(names.length, "command")} (${values.bin} --help)`);
		}
		report.note(
			`not built yet: ${plural(unimplemented.length, "registry command")} ` +
				`(reported, not a failure — Design 15 phases 2-6)`,
		);
		if (shown.length) {
			report.note(`  ${shown.join(", ")}${shown.length < unimplemented.length ? ", ... (--list-missing for all)" : ""}`);
		}
	}

	report.finish(
		values.bin === undefined
			? `${plural(manifest.commands.length, "implemented command")} all documented`
			: `binary, manifest and registry agree on ${plural(manifest.commands.length, "command")}`,
	);
}

main().catch((err) => {
	console.error(`${NAME}: ${err.stack || err.message || err}`);
	process.exit(1);
});
