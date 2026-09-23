#!/usr/bin/env node
// Run the actual exec and playscript wrapper sources through a Bindable-like
// table copy (which drops mixed-key metadata), then the production envelope.
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const args = process.argv.slice(2);
let runner = process.env.LUAU || 'luau';
let required = false;
for (let i = 0; i < args.length; i++) {
  if (args[i] === '--require-luau') required = true;
  else if (args[i] === '--luau' && args[i + 1]) { runner = args[++i]; required = true; }
  else throw new Error(`Unknown/incomplete argument: ${args[i]}`);
}
const runtime = fs.readFileSync(path.join(root, 'plugin/src/Playtest/Runtime.luau'), 'utf8');
function extract(start, end) {
  const from = runtime.indexOf(start);
  const to = runtime.indexOf(end, from);
  if (from < 0 || to < 0) throw new Error(`Missing production wrapper: ${start}`);
  return runtime.slice(from, to);
}
const wrappers = extract('local EXEC_WRAPPER_HEAD =', '--- Execute one source') + '\n' + extract('local PLAYSCRIPT_WRAPPER_HEAD =', '--- Install a playscript runner');
const cap = extract('local function capEnvelope(', 'local function cleanupResults(');
if (!runtime.includes('packedResult = ResultEnvelope.restore(packed, transportedCount)') || !runtime.includes('ResultEnvelope.restore(arguments[1], arguments[2])')) throw new Error('Runtime must restore explicit counts on both transport paths');
const test = fs.readFileSync(path.join(root, 'plugin/remote-tests/playtest-result.luau'), 'utf8');
const temp = fs.mkdtempSync(path.join(os.tmpdir(), 'wsync-playtest-result-'));
try {
  fs.copyFileSync(path.join(root, 'plugin/src/Playtest/ResultEnvelope.luau'), path.join(temp, 'ResultEnvelope.luau'));
  // The run state machine is require-free too: its terminal record is where
  // client failures must survive a timeout
  fs.copyFileSync(path.join(root, 'plugin/src/Playtest/RunState.luau'), path.join(temp, 'RunState.luau'));
  fs.copyFileSync(path.join(root, 'plugin/src/Remote/Codec.luau'), path.join(temp, 'Codec.luau'));
  fs.writeFileSync(path.join(temp, 'test.luau'), 'local HttpService = {}\nlocal LIMITS = {resultBytes=1024*1024}\n' + wrappers + '\n' + cap + '\n' + test);
  const result = spawnSync(runner, [path.join(temp, 'test.luau')], { encoding:'utf8', timeout:60000 });
  if (result.error?.code === 'ENOENT' && !required) console.warn('SKIP: Luau unavailable. Playtest result behavior was NOT tested.');
  else {
    if (result.stdout) process.stdout.write(result.stdout);
    if (result.stderr) process.stderr.write(result.stderr);
    if (result.error) throw result.error;
    if (result.status !== 0) throw new Error(`Playtest result tests failed (${result.status})`);
  }
} finally { fs.rmSync(temp, {recursive:true,force:true}); }
