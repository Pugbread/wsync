#!/usr/bin/env node
// check-heritage.mjs — WSync's shipped surface must not say "Ro-Sync".
//
// Design 1.4 ("remake stance") and 10.5: WSync is a clean-slate remake whose
// product surface is its own. Two classes of string give the game away and both
// are bugs when they reach a user:
//
//   * the parent tool's name (rosync / ro-sync / "ro sync") in docs, plugin,
//     desktop or engine sources — a user-visible identity leak;
//   * the retired default port 7878 — WSync serves 7978 (Design 3.2), and a
//     stale 7878 in code or docs is a live wrong-port bug, not just cosmetics.
//
// Attribution is *required*, not forbidden: NOTICE and the README credit both
// parents, and a source comment that explains why WSync diverges from the
// reference implementation is worth more than the string costs. Those live in
// scripts/data/heritage-allow.json, pinned to a file, a token and the exact
// text of the line — a blanket file exemption would let a real leak in through
// an allowlisted file.
//
// Design.md is excluded outright: it is the design document, its whole job is
// to talk about both parents, and it never ships.
//
// Node >= 18, zero dependencies. Exit 1 on any un-allowlisted hit.

import { readFile, stat } from "node:fs/promises";
import path from "node:path";
import { Reporter, helpFlag, parseArgs, plural, renderHelp, rel, repoRoot, walk } from "./lib/policy.mjs";

const NAME = "check-heritage";
const ALLOWLIST = "scripts/data/heritage-allow.json";

// Directory trees scanned in full, plus individual files. Everything is
// discovered by walking — no file list to go stale as the other trees grow.
const SCAN_DIRS = ["docs", "src", "plugin/src", "desktop/src"];
const SCAN_FILES = ["README.md", "NOTICE", "CHANGELOG.md"];

// Never scanned. Design.md is the design document; scripts/ holds this check
// and the registry generator, both of which must spell the forbidden tokens out.
const EXCLUDED = ["Design.md", "scripts"];

const TOKENS = [
	{ id: "rosync", pattern: /rosync/i, why: "legacy tool name (use wsync)" },
	{ id: "ro-sync", pattern: /ro-sync/i, why: "legacy tool name (use WSync)" },
	{ id: "ro sync", pattern: /\bro sync\b/i, why: "legacy tool name (use WSync)" },
	{ id: "7878", pattern: /\b7878\b/, why: "retired default port (WSync serves 7978)" },
];

// Anything larger is a data blob, not prose or code; scanning it produces
// unreadable output and no useful signal.
const MAX_BYTES = 4 * 1024 * 1024;

const SPEC = {
	root: {
		type: "string",
		valueName: "path",
		repeatable: true,
		description: "Scan an extra file or directory (repeatable). Adds to the defaults.",
	},
	list: {
		type: "boolean",
		description: "Also print the allowlisted hits, with the entry that permits each.",
	},
	help: helpFlag,
};

function help() {
	return renderHelp({
		name: NAME,
		summary: "fails when the shipped surface leaks the parent tool's name or its retired port",
		usage: "node scripts/check-heritage.mjs [--root <path>]... [--list]",
		spec: SPEC,
		sections: [
			{
				title: "Forbidden tokens",
				body: TOKENS.map((t) => `${t.id.padEnd(9)} ${t.why}`),
			},
			{
				title: "Scanned",
				body: [
					...SCAN_DIRS.map((d) => `${d}/ (every file)`),
					...SCAN_FILES,
					"",
					`Excluded: ${EXCLUDED.join(", ")}`,
				],
			},
			{
				title: "Allowlisting",
				body: [
					`Legitimate attribution goes in ${ALLOWLIST}. Each entry pins`,
					"a path, a token id and the exact text that makes the line legitimate:",
					"",
					'  { "path": "NOTICE", "token": "ro-sync",',
					'    "contains": "clean-slate remake of Ro-Sync",',
					'    "max": 1, "reason": "attribution required by ..." }',
					"",
					"An entry that stops matching is reported as a warning, so the allowlist",
					"cannot quietly rot into a blanket exemption.",
				],
			},
		],
	});
}

/** Reads and validates the allowlist. Returns an array of entry objects. */
async function readAllowlist(report) {
	const full = path.join(repoRoot, ALLOWLIST);
	let raw;
	try {
		raw = JSON.parse(await readFile(full, "utf8"));
	} catch (err) {
		if (err.code === "ENOENT") {
			report.warn(`${ALLOWLIST} is missing — every hit will be treated as a violation`);
			return [];
		}
		report.fail(ALLOWLIST, `invalid JSON — ${err.message}`);
		return [];
	}

	if (!raw || typeof raw !== "object" || !Array.isArray(raw.allow)) {
		report.fail(ALLOWLIST, 'must be a JSON object with an "allow" array');
		return [];
	}

	const tokenIds = new Set(TOKENS.map((t) => t.id));
	const entries = [];
	raw.allow.forEach((entry, i) => {
		const where = `${ALLOWLIST}#allow[${i}]`;
		if (!entry || typeof entry !== "object") {
			report.fail(where, "must be an object");
			return;
		}
		for (const field of ["path", "token", "contains", "reason"]) {
			if (typeof entry[field] !== "string" || !entry[field].trim()) {
				report.fail(where, `missing or empty "${field}"`);
				return;
			}
		}
		if (!tokenIds.has(entry.token)) {
			report.fail(where, `unknown token "${entry.token}" (known: ${[...tokenIds].join(", ")})`);
			return;
		}
		const max = entry.max ?? 1;
		if (!Number.isInteger(max) || max < 1) {
			report.fail(where, '"max" must be a positive integer');
			return;
		}
		entries.push({ ...entry, max, index: i, hits: 0 });
	});
	return entries;
}

/** Collects every file to scan, as absolute paths, deduped and sorted. */
async function collectFiles(extraRoots) {
	const excluded = new Set(EXCLUDED.map((e) => path.join(repoRoot, e)));
	const isExcluded = (absolute) =>
		[...excluded].some((e) => absolute === e || absolute.startsWith(`${e}${path.sep}`));

	const roots = [
		...SCAN_DIRS.map((d) => path.join(repoRoot, d)),
		...SCAN_FILES.map((f) => path.join(repoRoot, f)),
		...extraRoots.map((r) => path.resolve(repoRoot, r)),
	];

	const files = new Set();
	for (const root of roots) {
		let info;
		try {
			info = await stat(root);
		} catch {
			continue; // a tree that does not exist yet is not an error
		}
		if (info.isDirectory()) {
			for (const file of await walk(root)) if (!isExcluded(file)) files.add(file);
		} else if (info.isFile() && !isExcluded(root)) {
			files.add(root);
		}
	}
	return [...files].sort();
}

const clip = (s, n = 140) => (s.length > n ? `${s.slice(0, n - 3)}...` : s);

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

	const allowlist = await readAllowlist(report);
	if (report.failures.length) report.finish();

	const files = await collectFiles(values.root);
	let scanned = 0;
	let skipped = 0;
	let allowed = 0;
	const allowedLines = [];

	for (const file of files) {
		const relative = rel(file);
		const info = await stat(file);
		if (info.size > MAX_BYTES) {
			skipped++;
			report.warn(`${relative} skipped: ${(info.size / 1024 / 1024).toFixed(1)} MB exceeds the scan limit`);
			continue;
		}

		// Binary files (the msgpack property database, images) decode to
		// replacement characters; ASCII tokens still match, which is what we want.
		const text = await readFile(file, "utf8");
		scanned++;
		const lines = text.split(/\r?\n/);

		for (const [i, line] of lines.entries()) {
			for (const token of TOKENS) {
				const match = line.match(token.pattern);
				if (!match) continue;

				const entry = allowlist.find(
					(candidate) =>
						candidate.path === relative &&
						candidate.token === token.id &&
						line.includes(candidate.contains),
				);

				if (entry) {
					entry.hits++;
					allowed++;
					if (entry.hits > entry.max) {
						report.fail(
							`${relative}:${i + 1}`,
							`"${match[0]}" — allowlist entry #${entry.index} permits at most ` +
								`${plural(entry.max, "line")}, found ${entry.hits}. Widen "max" deliberately ` +
								"or remove the extra occurrence.",
						);
					} else if (values.list) {
						allowedLines.push(`  allow  ${relative}:${i + 1}  "${match[0]}"  (#${entry.index}: ${entry.reason})`);
					}
					continue;
				}

				report.fail(
					`${relative}:${i + 1}`,
					`contains "${match[0]}" — ${token.why}\n      ${clip(line.trim())}`,
				);
			}
		}
	}

	for (const entry of allowlist) {
		if (entry.hits === 0) {
			report.warn(
				`stale allowlist entry #${entry.index} (${entry.path} / ${entry.token} / ` +
					`"${clip(entry.contains, 60)}") matched nothing — delete it or restore the attribution`,
			);
		}
	}

	if (values.list && allowedLines.length) report.note(allowedLines.join("\n"));
	report.note(
		`scanned ${plural(scanned, "file")} across ${SCAN_DIRS.join(", ")} and ${SCAN_FILES.join(", ")}` +
			(skipped ? `; skipped ${plural(skipped, "oversized file")}` : ""),
	);
	report.note(`${plural(allowed, "allowlisted attribution line")}, ${plural(TOKENS.length, "token")} enforced`);
	report.finish("no heritage leaks in the shipped surface");
}

main().catch((err) => {
	console.error(`${NAME}: ${err.stack || err.message || err}`);
	process.exit(1);
});
