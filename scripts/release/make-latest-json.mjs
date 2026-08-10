#!/usr/bin/env node
// make-latest-json.mjs — the Tauri v2 updater manifest for a WSync release.
//
// Design 8.5: the desktop app updates itself from a `latest.json` published on
// the GitHub release, verified against an ed25519 public key compiled into the
// app. This script writes that document from the bundles a release job just
// built, and refuses to write one that the updater would reject.
//
// THE SCHEMA IS NOT GUESSED
//
// It is the "static" shape `tauri-plugin-updater` deserializes
// (`RemoteRelease`, updater.rs):
//
//   {
//     "version":  "0.1.0",              // required; parsed as semver after a
//                                       // leading `v` is stripped
//     "notes":    "…",                  // optional
//     "pub_date": "2026-01-01T00:00:00Z",// optional, RFC 3339 — an unparseable
//                                       // date fails the *whole* manifest
//     "platforms": {                    // required for the static shape
//       "darwin-aarch64": { "url": "https://…", "signature": "…" }
//     }
//   }
//
// Platform keys are `{os}-{arch}` with an optional `-{installer}` suffix; the
// client looks up `{os}-{arch}-{installer}` first and falls back to
// `{os}-{arch}`. The vocabulary is fixed by the plugin's own `updater_os()`,
// `updater_arch()` and `Installer::name()`, so a typo like `macos-arm64` is
// rejected here rather than becoming an update nobody is offered.
//
// THE SIGNATURE IS STRUCTURALLY VERIFIED
//
// `<bundle>.sig` — what `tauri build` writes when signing is on — holds base64
// of a minisign signature file. The client base64-decodes it and hands it to
// `minisign_verify::Signature::decode`, which needs four lines, a second line
// that decodes to exactly 74 bytes, and a fourth that decodes to 64. All of
// that can be checked here without the private key, and anything else would be
// an update the client refuses after downloading it — so it fails the build
// instead.
//
// This script never signs anything and never touches a key.
//
// Node >= 18, zero dependencies.

import { readFile, writeFile } from "node:fs/promises";
import path from "node:path";
import { Reporter, helpFlag, isFile, parseArgs, renderHelp } from "../lib/policy.mjs";

const NAME = "make-latest-json";

/** From `updater_os()` in tauri-plugin-updater. */
const OPERATING_SYSTEMS = ["linux", "darwin", "windows"];
/** From `updater_arch()`. */
const ARCHITECTURES = ["i686", "x86_64", "armv7", "aarch64", "riscv64"];
/** From `Installer::name()` — the optional third segment. */
const INSTALLERS = ["appimage", "deb", "rpm", "app", "msi", "nsis"];

const PLATFORM_PATTERN = new RegExp(
	`^(${OPERATING_SYSTEMS.join("|")})-(${ARCHITECTURES.join("|")})(-(${INSTALLERS.join("|")}))?$`,
);

/** `Signature::decode` in minisign-verify: 74 bytes, then 64. */
const SIGNATURE_BYTES = 74;
const GLOBAL_SIGNATURE_BYTES = 64;

const SPEC = {
	version: {
		type: "string",
		valueName: "semver",
		description: "The version being released. A leading `v` is stripped.",
	},
	platform: {
		type: "string",
		valueName: "key=path",
		repeatable: true,
		description: "An update bundle, e.g. darwin-aarch64=WSync.app.tar.gz. Repeatable.",
	},
	"base-url": {
		type: "string",
		valueName: "url",
		description: "Where the bundles will be downloadable from (the release's download URL).",
	},
	notes: { type: "string", valueName: "text", description: "Release notes shown in the update prompt." },
	"notes-file": { type: "string", valueName: "path", description: "Read the notes from a file instead." },
	"pub-date": { type: "string", valueName: "rfc3339", description: "Publication date (default: now, UTC)." },
	out: { type: "string", valueName: "path", description: "Where to write it (default latest.json, `-` for stdout)." },
	help: helpFlag,
};

function help() {
	return renderHelp({
		name: NAME,
		summary: "writes the Tauri v2 updater manifest for a release",
		usage: "node scripts/release/make-latest-json.mjs --version <v> --base-url <url> --platform <key>=<path>",
		spec: SPEC,
		sections: [
			{
				title: "What it refuses to write",
				body: [
					"A manifest with no platforms, an unknown platform key, a bundle that is",
					"missing or empty, or a `<bundle>.sig` that is not the base64 minisign",
					"signature the updater will demand. Every one of those is an update the",
					"client would reject after downloading it.",
					"",
					"Signing happens in `tauri build`; this script only reads the .sig files.",
				],
			},
			{
				title: "Example",
				body: [
					"node scripts/release/make-latest-json.mjs \\",
					"  --version v0.1.0 \\",
					"  --base-url https://github.com/Pugbread/wsync/releases/download/v0.1.0 \\",
					"  --platform darwin-aarch64=bundle/macos/WSync.app.tar.gz \\",
					"  --notes-file CHANGELOG-excerpt.md --out latest.json",
				],
			},
		],
	});
}

/** Semver, as strict as `semver::Version::from_str` after `v` is stripped. */
function parseVersion(raw) {
	const value = String(raw).trim().replace(/^v/, "");
	const ok = /^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$/.test(value);
	return ok ? value : null;
}

/** RFC 3339, which is what the client parses `pub_date` with. */
function isRfc3339(value) {
	if (!/^\d{4}-\d{2}-\d{2}[Tt]\d{2}:\d{2}:\d{2}(\.\d+)?([Zz]|[+-]\d{2}:\d{2})$/.test(value)) return false;
	return !Number.isNaN(Date.parse(value));
}

function decodeBase64(text) {
	if (!/^[A-Za-z0-9+/]+={0,2}$/.test(text)) return null;
	const bytes = Buffer.from(text, "base64");
	// Buffer.from is lenient; re-encoding catches input it silently truncated.
	return bytes.toString("base64").replace(/=+$/, "") === text.replace(/=+$/, "") ? bytes : null;
}

/**
 * Checks a `.sig` the way the client will: base64 of a four-line minisign
 * signature file. Returns `{ signature }` or `{ error }`.
 */
function inspectSignature(raw) {
	const trimmed = raw.trim();
	if (!trimmed) return { error: "is empty" };

	const outer = decodeBase64(trimmed.replace(/\s+/g, ""));
	if (!outer) {
		return {
			error:
				"is not base64 — `tauri build` writes base64 of a minisign signature file, " +
				"and the client base64-decodes this string before parsing it",
		};
	}

	const lines = outer.toString("utf8").split(/\r?\n/);
	if (lines.length < 4) {
		return { error: `decodes to ${lines.length} line(s); minisign_verify::Signature::decode needs 4` };
	}

	const body = decodeBase64(lines[1].trim());
	if (!body || body.length !== SIGNATURE_BYTES) {
		return {
			error: `line 2 decodes to ${body ? `${body.length} bytes` : "nothing"}, expected ${SIGNATURE_BYTES}`,
		};
	}

	const global = decodeBase64(lines[3].trim());
	if (!global || global.length !== GLOBAL_SIGNATURE_BYTES) {
		return {
			error: `line 4 decodes to ${global ? `${global.length} bytes` : "nothing"}, expected ${GLOBAL_SIGNATURE_BYTES}`,
		};
	}

	return { signature: trimmed };
}

function joinUrl(base, name) {
	return `${base.replace(/\/+$/, "")}/${encodeURI(name)}`;
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

	const version = values.version ? parseVersion(values.version) : null;
	if (!values.version) report.fail("arguments", "--version is required");
	else if (!version) report.fail("--version", `${JSON.stringify(values.version)} is not a semver the client can parse`);

	const baseUrl = values["base-url"];
	if (!baseUrl) report.fail("arguments", "--base-url is required — the manifest publishes absolute URLs");
	else if (!/^https?:\/\/\S+$/.test(baseUrl)) report.fail("--base-url", `${JSON.stringify(baseUrl)} is not an http(s) URL`);

	if (values.notes && values["notes-file"]) {
		report.fail("arguments", "--notes and --notes-file are mutually exclusive");
	}

	const pubDate = values["pub-date"] ?? new Date().toISOString().replace(/\.\d+Z$/, "Z");
	if (!isRfc3339(pubDate)) {
		report.fail("--pub-date", `${JSON.stringify(pubDate)} is not RFC 3339 — the client fails the whole manifest on that`);
	}

	if (values.platform.length === 0) {
		report.fail("arguments", "at least one --platform <key>=<path> is required");
	}

	const platforms = {};
	for (const entry of values.platform) {
		const split = entry.indexOf("=");
		if (split <= 0) {
			report.fail("--platform", `${JSON.stringify(entry)} is not <key>=<path>`);
			continue;
		}
		const key = entry.slice(0, split).trim();
		const bundlePath = path.resolve(entry.slice(split + 1).trim());

		if (!PLATFORM_PATTERN.test(key)) {
			report.fail(
				"--platform",
				`"${key}" is not a target the updater looks up. Keys are {os}-{arch}[-{installer}] with ` +
					`os in ${OPERATING_SYSTEMS.join("|")}, arch in ${ARCHITECTURES.join("|")}, ` +
					`installer in ${INSTALLERS.join("|")}`,
			);
			continue;
		}
		if (key in platforms) {
			report.fail("--platform", `"${key}" was given twice`);
			continue;
		}

		if (!(await isFile(bundlePath))) {
			report.fail(key, `${bundlePath} does not exist — the manifest would advertise a download nobody can make`);
			continue;
		}
		const bundle = await readFile(bundlePath);
		if (bundle.length === 0) {
			report.fail(key, `${bundlePath} is empty`);
			continue;
		}

		const signaturePath = `${bundlePath}.sig`;
		if (!(await isFile(signaturePath))) {
			report.fail(
				key,
				`${signaturePath} is missing — this bundle was built unsigned, and an unsigned build must ` +
					"not be published through the updater (Design 8.5 is fail-closed)",
			);
			continue;
		}

		const inspected = inspectSignature(await readFile(signaturePath, "utf8"));
		if (inspected.error) {
			report.fail(key, `${path.basename(signaturePath)} ${inspected.error}`);
			continue;
		}

		platforms[key] = {
			signature: inspected.signature,
			url: joinUrl(baseUrl ?? "", path.basename(bundlePath)),
		};
		report.note(
			`  ok  ${key} → ${path.basename(bundlePath)} (${bundle.length.toLocaleString("en-US")} B, signed)`,
		);
	}

	let notes = values.notes ?? null;
	if (values["notes-file"]) {
		const notesPath = path.resolve(values["notes-file"]);
		if (!(await isFile(notesPath))) report.fail("--notes-file", `${notesPath} does not exist`);
		else notes = (await readFile(notesPath, "utf8")).trim();
	}

	if (report.failures.length) report.finish();

	// Key order is the client's read order, and sorted platforms keep the diff
	// between two releases' manifests readable.
	const manifest = {
		version,
		notes: notes && notes.length ? notes : `WSync ${version}`,
		pub_date: pubDate,
		platforms: Object.fromEntries(Object.keys(platforms).sort().map((key) => [key, platforms[key]])),
	};

	const rendered = `${JSON.stringify(manifest, null, 2)}\n`;
	const destination = values.out ?? "latest.json";

	if (destination === "-") {
		process.stdout.write(rendered);
	} else {
		await writeFile(path.resolve(destination), rendered, "utf8");
		report.note(`wrote ${path.resolve(destination)} (${rendered.length} B)`);
	}

	report.finish(`${version} for ${Object.keys(manifest.platforms).join(", ")}`);
}

main().catch((err) => {
	console.error(`${NAME}: ${err.stack || err.message || err}`);
	process.exit(1);
});
