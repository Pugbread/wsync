// scripts/lib/policy.mjs — shared plumbing for the WSync policy checks.
//
// Node >= 18, zero dependencies. Everything here is deliberately small: arg
// parsing, a repo-root anchor, a file walker, and a reporter that collects
// every problem before exiting so a failing check tells you all of it at once
// instead of one item per CI run.

import { readdir, stat } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

/** Absolute path to the repository root (this file lives at scripts/lib/). */
export const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..");

/** Repo-relative, forward-slashed path — stable in output across platforms. */
export function rel(absolute) {
	return path.relative(repoRoot, absolute).split(path.sep).join("/");
}

/**
 * Minimal option parser.
 *
 * `spec` maps a long flag name to `{ type: "boolean" | "string", valueName,
 * description, repeatable }`. `--name=value` and `--name value` both work.
 * Unknown flags are an error rather than being silently ignored, so a typo in
 * a CI invocation fails loudly instead of quietly disabling a check.
 */
export function parseArgs(argv, spec) {
	const values = Object.create(null);
	const positionals = [];
	const errors = [];

	for (const [name, def] of Object.entries(spec)) {
		values[name] = def.repeatable ? [] : def.type === "boolean" ? false : undefined;
	}

	for (let i = 0; i < argv.length; i++) {
		const arg = argv[i];
		if (arg === "--") {
			positionals.push(...argv.slice(i + 1));
			break;
		}
		if (!arg.startsWith("--")) {
			positionals.push(arg);
			continue;
		}

		const eq = arg.indexOf("=");
		const name = (eq === -1 ? arg.slice(2) : arg.slice(2, eq)).trim();
		const inlineValue = eq === -1 ? undefined : arg.slice(eq + 1);
		const def = spec[name];

		if (!def) {
			errors.push(`unknown option --${name}`);
			continue;
		}
		if (def.type === "boolean") {
			if (inlineValue !== undefined) errors.push(`--${name} does not take a value`);
			values[name] = true;
			continue;
		}

		let value = inlineValue;
		if (value === undefined) {
			value = argv[++i];
			if (value === undefined) {
				errors.push(`--${name} requires a value`);
				continue;
			}
		}
		if (def.repeatable) values[name].push(value);
		else values[name] = value;
	}

	return { values, positionals, errors };
}

/** Renders `--help` text from the same spec `parseArgs` consumes. */
export function renderHelp({ name, summary, usage, spec, sections = [] }) {
	const lines = [`${name} — ${summary}`, "", "Usage", `  ${usage}`, "", "Options"];
	const rows = Object.entries(spec).map(([flag, def]) => [
		`  --${flag}${def.type === "string" ? ` <${def.valueName ?? "value"}>` : ""}`,
		def.description,
	]);
	const width = Math.max(...rows.map(([left]) => left.length));
	for (const [left, right] of rows) lines.push(`${left.padEnd(width + 2)}${right}`);
	for (const section of sections) {
		lines.push("", section.title, ...section.body.map((line) => `  ${line}`));
	}
	return lines.join("\n");
}

/** Shared `--help` entry every check accepts. */
export const helpFlag = {
	type: "boolean",
	description: "Show this message and exit.",
};

/**
 * Collects problems, notes and warnings, then prints one verdict.
 *
 * Failures are fatal (exit 1). Warnings are printed but never change the exit
 * code — a check that "sort of" fails is a check nobody trusts.
 */
export class Reporter {
	constructor(name) {
		this.name = name;
		this.failures = [];
		this.warnings = [];
		this.notes = [];
	}

	fail(where, message) {
		this.failures.push(where ? `${where}: ${message}` : message);
	}

	warn(message) {
		this.warnings.push(message);
	}

	note(message) {
		this.notes.push(message);
	}

	/** Prints everything collected and exits 1 if any failure was recorded. */
	finish(summary) {
		for (const note of this.notes) console.log(note);

		if (this.warnings.length) {
			console.error("");
			console.error(`WARNING (${this.warnings.length}):`);
			for (const warning of this.warnings) console.error(`  ! ${warning}`);
		}

		if (this.failures.length) {
			console.error("");
			console.error(
				`${this.name}: FAILED with ${this.failures.length} problem${this.failures.length === 1 ? "" : "s"}`,
			);
			for (const failure of this.failures) console.error(`  - ${failure}`);
			process.exit(1);
		}

		console.log("");
		console.log(`${this.name}: OK${summary ? ` — ${summary}` : ""}`);
		process.exit(0);
	}
}

const SKIP_DIRS = new Set([".git", "node_modules", "target", "Packages"]);

/**
 * Recursively lists files under `dir`.
 *
 * Discovery is by walk + predicate, never by a hardcoded file list: the engine,
 * plugin and desktop trees are all under concurrent development, so a check
 * that enumerates known files goes stale the moment someone adds one.
 * Symlinked directories are not followed (no cycles, no escaping the tree).
 */
export async function walk(dir, { filter = () => true, skipDirs = SKIP_DIRS } = {}) {
	const found = [];
	let entries;
	try {
		entries = await readdir(dir, { withFileTypes: true });
	} catch (err) {
		if (err.code === "ENOENT") return found;
		throw err;
	}

	for (const entry of entries.sort((a, b) => (a.name < b.name ? -1 : 1))) {
		const full = path.join(dir, entry.name);
		if (entry.isDirectory()) {
			if (skipDirs.has(entry.name)) continue;
			found.push(...(await walk(full, { filter, skipDirs })));
		} else if (entry.isFile() && filter(full)) {
			found.push(full);
		}
	}
	return found;
}

/** True when `p` exists and is a regular file. */
export async function isFile(p) {
	try {
		return (await stat(p)).isFile();
	} catch {
		return false;
	}
}

/** `1 thing` / `2 things` without the `(s)` noise. */
export function plural(count, singular, pluralForm = `${singular}s`) {
	return `${count} ${count === 1 ? singular : pluralForm}`;
}
