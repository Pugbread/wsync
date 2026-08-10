#!/usr/bin/env node
// Builds the generated command docs from the command registry.
//
//   source of truth : docs/commands/<name>.json + docs/commands/index.json
//   outputs         : docs/client-commands.md
//                     docs/client-commands.generated.json
//
// The JSON bundle is embedded verbatim into the wsync binary and rendered by
// the desktop Docs view, so it must be deterministic: no timestamps, stable
// ordering, stable key order, trailing newline.
//
// Node >= 18, zero dependencies. Any validation failure prints every problem
// found and exits non-zero.

import { readdir, readFile, writeFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

const scriptDir = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.resolve(scriptDir, "..");
const commandsDir = path.join(repoRoot, "docs", "commands");
const indexPath = path.join(commandsDir, "index.json");
const bundlePath = path.join(repoRoot, "docs", "client-commands.generated.json");
const markdownPath = path.join(repoRoot, "docs", "client-commands.md");

// Canonical category order (Design 10.2 + 10.3). A command may not use a
// category outside this list; the generated `categories` array follows this
// order regardless of index order, so the docs never reshuffle on an insert.
const CATEGORIES = [
  "Project setup",
  "Daemon",
  "Command registry",
  "Agent runtime",
  "Live inspection",
  "Path tools",
  "Live diagnostics",
  "Open Cloud",
  "Studio control",
  "Conflict resolution",
  "Maintenance",
  "Project docs",
  "Live writes",
  "Studio clipboard",
  "Build & sourcemap",
  "Tool management",
];

// Emitted key order for every command entry in the bundle.
const KEY_ORDER = [
  "name",
  "title",
  "category",
  "description",
  "subcommands",
  "subcommandPaths",
  "subcommandMetadata",
  "usage",
  "examples",
  "notes",
];

const REQUIRED_STRINGS = ["name", "title", "category", "description", "usage"];

// The registry is a clean-room surface: no heritage tokens, no retired port.
const FORBIDDEN = [
  [/rosync/i, "legacy tool name (use wsync)"],
  [/ro-sync/i, "legacy tool name (use wsync)"],
  [/\bro sync\b/i, "legacy tool name (use WSync)"],
  [/\b7878\b/, "legacy default port (use 7978)"],
];

const errors = [];
const fail = (where, message) => errors.push(`${where}: ${message}`);

const isPlainObject = (v) => typeof v === "object" && v !== null && !Array.isArray(v);
const isFilledString = (v) => typeof v === "string" && v.trim().length > 0;

function checkStringArray(where, field, value, { required }) {
  if (value === undefined) {
    if (required) fail(where, `missing ${field}`);
    return [];
  }
  if (!Array.isArray(value)) {
    fail(where, `${field} must be an array`);
    return [];
  }
  if (required && value.length === 0) fail(where, `${field} must not be empty`);
  value.forEach((item, i) => {
    if (!isFilledString(item)) fail(where, `${field}[${i}] must be a non-empty string`);
  });
  const seen = new Set();
  for (const item of value) {
    if (typeof item !== "string") continue;
    if (seen.has(item)) fail(where, `${field} has duplicate entry "${item}"`);
    seen.add(item);
  }
  return value;
}

function validateCommand(command, slug, fileName) {
  const where = fileName;

  for (const field of REQUIRED_STRINGS) {
    if (!isFilledString(command[field])) fail(where, `missing or empty "${field}"`);
  }

  const unknownKeys = Object.keys(command).filter((k) => !KEY_ORDER.includes(k));
  if (unknownKeys.length) fail(where, `unknown key(s): ${unknownKeys.join(", ")}`);

  if (command.name !== slug) fail(where, `"name" (${command.name}) must match the file name`);
  if (command.title !== `wsync ${slug}`) {
    fail(where, `"title" must be "wsync ${slug}", got "${command.title}"`);
  }
  if (!CATEGORIES.includes(command.category)) {
    fail(where, `unknown category "${command.category}"`);
  }

  if (isFilledString(command.usage)) {
    for (const line of command.usage.split("\n")) {
      const trimmed = line.trim();
      if (!trimmed || trimmed.startsWith("#")) continue;
      if (!trimmed.startsWith("wsync ")) fail(where, `usage line does not start with "wsync ": ${trimmed}`);
    }
  }

  const examples = checkStringArray(where, "examples", command.examples, { required: true });
  for (const example of examples) {
    if (typeof example === "string" && !example.includes("wsync ")) {
      fail(where, `example does not invoke wsync: ${example}`);
    }
  }
  checkStringArray(where, "notes", command.notes, { required: true });

  const subcommands = "subcommands" in command
    ? checkStringArray(where, "subcommands", command.subcommands, { required: true })
    : [];

  let subcommandPaths = [];
  if ("subcommandPaths" in command) {
    if (!subcommands.length) fail(where, "subcommandPaths requires subcommands");
    subcommandPaths = checkStringArray(where, "subcommandPaths", command.subcommandPaths, { required: true });
    for (const p of subcommandPaths) {
      if (typeof p !== "string") continue;
      const head = p.split(" ")[0];
      if (!subcommands.includes(head)) {
        fail(where, `subcommandPaths entry "${p}" starts with an undeclared subcommand "${head}"`);
      }
    }
  }

  if ("subcommandMetadata" in command) {
    const metadata = command.subcommandMetadata;
    if (!isPlainObject(metadata)) {
      fail(where, "subcommandMetadata must be an object");
    } else {
      if (!subcommands.length) fail(where, "subcommandMetadata requires subcommands");
      const known = new Set([...subcommands, ...subcommandPaths]);
      const expected = subcommandPaths.length ? subcommandPaths : subcommands;
      for (const key of Object.keys(metadata)) {
        if (!known.has(key)) fail(where, `subcommandMetadata key "${key}" is not a declared subcommand`);
        const entry = metadata[key];
        if (!isPlainObject(entry)) {
          fail(where, `subcommandMetadata["${key}"] must be an object`);
          continue;
        }
        const entryKeys = Object.keys(entry).filter((k) => k !== "safety" && k !== "requires");
        if (entryKeys.length) {
          fail(where, `subcommandMetadata["${key}"] has unknown field(s): ${entryKeys.join(", ")}`);
        }
        if (!isFilledString(entry.safety)) {
          fail(where, `subcommandMetadata["${key}"] missing "safety"`);
        }
        if (!Array.isArray(entry.requires)) {
          fail(where, `subcommandMetadata["${key}"] missing "requires" array`);
        } else {
          entry.requires.forEach((r, i) => {
            if (!isFilledString(r)) fail(where, `subcommandMetadata["${key}"].requires[${i}] must be a non-empty string`);
          });
        }
      }
      for (const key of expected) {
        if (!(key in metadata)) fail(where, `subcommandMetadata is missing an entry for "${key}"`);
      }
    }
  } else if (subcommands.length) {
    fail(where, "subcommands declared without subcommandMetadata");
  }
}

function checkForbidden(fileName, raw) {
  for (const [pattern, reason] of FORBIDDEN) {
    const match = raw.match(pattern);
    if (match) fail(fileName, `contains "${match[0]}" — ${reason}`);
  }
}

function anchor(heading) {
  return heading
    .toLowerCase()
    .replace(/[^a-z0-9 _-]/g, "")
    .replace(/ /g, "-");
}

function shellBlock(value) {
  const body = Array.isArray(value) ? value.join("\n") : String(value ?? "");
  return "```sh\n" + body.trimEnd() + "\n```";
}

function subcommandLines(command) {
  const metadata = command.subcommandMetadata;
  if (!metadata) return null;
  const keys = command.subcommandPaths?.length ? command.subcommandPaths : command.subcommands;
  return keys.map((key) => {
    const entry = metadata[key];
    const requires = entry.requires.length ? entry.requires.map((r) => `\`${r}\``).join(", ") : "nothing";
    return `- \`${key}\` — safety \`${entry.safety}\`, requires ${requires}`;
  }).join("\n");
}

function commandMarkdown(command) {
  const parts = [`### \`${command.title}\``, command.description, "**Usage**", shellBlock(command.usage)];
  const subs = subcommandLines(command);
  if (subs) parts.push("**Subcommands**", subs);
  if (command.examples.length) parts.push("**Examples**", shellBlock(command.examples));
  if (command.notes.length) parts.push("**Notes**", command.notes.map((n) => `- ${n}`).join("\n"));
  parts.push("---");
  return parts.join("\n\n");
}

function buildMarkdown(commands, categories, byCategory) {
  const contents = categories.map((category) => {
    const names = byCategory.get(category).map((c) => `\`${c.name}\``).join(", ");
    return `- [${category}](#${anchor(category)}) (${byCategory.get(category).length}) — ${names}`;
  });

  const body = categories.flatMap((category) => [
    `## ${category}`,
    "",
    ...byCategory.get(category).map((c) => commandMarkdown(c) + "\n"),
  ]);

  return [
    "<!-- Generated by scripts/build-command-docs.mjs. Edit docs/commands/*.json instead. -->",
    "",
    "# wsync command reference",
    "",
    `${commands.length} commands in ${categories.length} categories. Every entry is generated from one JSON file`,
    "under `docs/commands/`; edit the JSON and re-run `node scripts/build-command-docs.mjs`.",
    "",
    "`wsync commands [name] [--compact]` serves this same registry from the binary, and the desktop",
    "Docs view renders it from `docs/client-commands.generated.json`.",
    "",
    "## Contents",
    "",
    ...contents,
    "",
    ...body,
  ].join("\n").replace(/\n{3,}/g, "\n\n") + "";
}

async function main() {
  let index;
  try {
    index = JSON.parse(await readFile(indexPath, "utf8"));
  } catch (err) {
    console.error(`docs/commands/index.json could not be read: ${err.message}`);
    process.exit(1);
  }

  if (!isPlainObject(index) || !Array.isArray(index.commands)) {
    console.error("docs/commands/index.json must be an object with a `commands` array");
    process.exit(1);
  }

  const slugs = index.commands;
  const seen = new Set();
  for (const slug of slugs) {
    if (!isFilledString(slug)) {
      fail("index.json", `command entries must be non-empty strings, got ${JSON.stringify(slug)}`);
      continue;
    }
    if (seen.has(slug)) fail("index.json", `duplicate entry "${slug}"`);
    seen.add(slug);
  }

  const onDisk = (await readdir(commandsDir))
    .filter((f) => f.endsWith(".json") && f !== "index.json")
    .map((f) => f.slice(0, -".json".length))
    .sort();

  for (const slug of onDisk) {
    if (!seen.has(slug)) fail("index.json", `docs/commands/${slug}.json is not listed in the index`);
  }

  const commands = [];
  for (const slug of slugs) {
    if (!isFilledString(slug)) continue;
    const fileName = `${slug}.json`;
    let raw;
    try {
      raw = await readFile(path.join(commandsDir, fileName), "utf8");
    } catch {
      fail("index.json", `listed command "${slug}" has no docs/commands/${fileName}`);
      continue;
    }
    checkForbidden(fileName, raw);

    let command;
    try {
      command = JSON.parse(raw);
    } catch (err) {
      fail(fileName, `invalid JSON — ${err.message}`);
      continue;
    }
    if (!isPlainObject(command)) {
      fail(fileName, "must contain a JSON object");
      continue;
    }
    validateCommand(command, slug, fileName);
    commands.push(command);
  }

  if (errors.length) {
    console.error(`command registry validation failed (${errors.length} problem${errors.length === 1 ? "" : "s"}):`);
    for (const message of errors) console.error(`  - ${message}`);
    process.exit(1);
  }

  const byCategory = new Map();
  for (const command of commands) {
    if (!byCategory.has(command.category)) byCategory.set(command.category, []);
    byCategory.get(command.category).push(command);
  }
  const categories = CATEGORIES.filter((c) => byCategory.has(c));

  const bundle = {
    schemaVersion: 1,
    source: "docs/commands/*.json",
    generatedAt: null,
    categories,
    commands: commands.map((command) => {
      const ordered = {};
      for (const key of KEY_ORDER) if (key in command) ordered[key] = command[key];
      return ordered;
    }),
  };

  await writeFile(bundlePath, JSON.stringify(bundle, null, 2) + "\n");
  await writeFile(markdownPath, buildMarkdown(commands, categories, byCategory).trimEnd() + "\n");

  console.log(`built ${commands.length} command docs in ${categories.length} categories`);
  for (const category of categories) {
    console.log(`  ${String(byCategory.get(category).length).padStart(2)}  ${category}`);
  }
  console.log(`  -> ${path.relative(repoRoot, markdownPath)}`);
  console.log(`  -> ${path.relative(repoRoot, bundlePath)}`);
}

main().catch((err) => {
  console.error(err.stack || err.message || err);
  process.exit(1);
});
