#!/usr/bin/env node
// Keep the offline catalog and the real plugin decoder aligned, then execute
// contract/round-trip tests using constructor stubs under standalone Luau.
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const args = process.argv.slice(2);
const allowed = new Set(['--write', '--require-luau', '--luau']);
for (let i = 0; i < args.length; i++) {
  if (!allowed.has(args[i])) throw new Error(`Unknown argument ${args[i]}`);
  if (args[i] === '--luau' && !args[++i]) throw new Error('--luau needs a path');
}
const catalog = JSON.parse(fs.readFileSync(path.join(root, 'docs/value-types.json'), 'utf8'));
const codecPath = path.join(root, 'plugin/src/Remote/Codec.luau');
let codec = fs.readFileSync(codecPath, 'utf8');
function luau(value) {
  if (Array.isArray(value)) return `{${value.map(luau).join(',')}}`;
  if (value && typeof value === 'object') return `{${Object.entries(value).map(([key, val]) => `[${JSON.stringify(key)}]=${luau(val)}`).join(',')}}`;
  return JSON.stringify(value);
}
const start = '-- BEGIN GENERATED VALUE SCHEMAS (scripts/check-value-codec.mjs --write)';
const end = '-- END GENERATED VALUE SCHEMAS';
const generated = `${start}\nlocal VALUE_SCHEMAS: { [string]: any } = {\n${catalog.types.map(entry => `\t[${JSON.stringify(entry.name)}] = ${luau({ schema:entry.schema, decode:entry.decode, example:JSON.stringify(entry.example), notes:entry.notes })},`).join('\n')}\n}\n${end}`;
if (codec.includes(start)) {
  const current = codec.slice(codec.indexOf(start), codec.indexOf(end) + end.length);
  if (current !== generated) {
    if (!args.includes('--write')) throw new Error('Value schema drift: run node scripts/check-value-codec.mjs --write');
    codec = codec.replace(current, generated);
    fs.writeFileSync(codecPath, codec);
  }
} else {
  if (!args.includes('--write')) throw new Error('Codec is missing its generated value schema block');
  codec = codec.replace('local instanceCodec: InstanceCodec? = nil', `${generated}\n\nlocal instanceCodec: InstanceCodec? = nil`);
  fs.writeFileSync(codecPath, codec);
}
const decoders = codec.slice(codec.indexOf('local TAG_DECODERS:'), codec.indexOf('\nfunction decodeValue'));
const names = [...decoders.matchAll(/^\t(\w+) = function\(tag|^\t(\w+) = function\(_tag/gm)].map(match => match[1] || match[2]).sort();
const documented = catalog.types.map(entry => entry.name).sort();
if (JSON.stringify(names) !== JSON.stringify(documented)) throw new Error(`Codec/catalog tags differ: ${names} vs ${documented}`);
const luauArg = args.indexOf('--luau');
const runner = luauArg >= 0 ? args[luauArg + 1] : process.env.LUAU || 'luau';
const temp = fs.mkdtempSync(path.join(os.tmpdir(), 'wsync-value-codec-'));
try {
  const test = fs.readFileSync(path.join(root, 'plugin/remote-tests/codec.luau'), 'utf8');
  fs.writeFileSync(path.join(temp, 'Codec.luau'), codec);
  fs.writeFileSync(path.join(temp, 'test.luau'), `local catalog = ${luau(catalog)}\n${test}`);
  const result = spawnSync(runner, [path.join(temp, 'test.luau')], { encoding:'utf8', timeout:60000 });
  if (result.error?.code === 'ENOENT' && !args.includes('--require-luau') && luauArg < 0 && !process.env.LUAU) {
    console.warn('SKIP: Luau unavailable. Catalog drift checks passed; codec behavior was NOT tested. Use --require-luau in CI.');
  } else {
    if (result.stdout) process.stdout.write(result.stdout);
    if (result.stderr) process.stderr.write(result.stderr);
    if (result.error) throw result.error;
    if (result.status !== 0) throw new Error(`Codec tests failed (${result.status})`);
  }
} finally { fs.rmSync(temp, { recursive:true, force:true }); }
console.log(`Value contract: ${documented.length} catalog tags match real Codec decoders.`);
