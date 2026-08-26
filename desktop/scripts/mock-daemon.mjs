#!/usr/bin/env node
// mock-daemon.mjs — a stand-in for `wsync daemon start`, for developing and
// testing the desktop against the wire contract before the engine exists.
//
// Zero dependencies, Node >= 18. It implements the parts of Design §3.3 and
// §5.2–5.3 the desktop actually touches:
//
//   spawn contract   argv parsing, one JSON ready line on stdout, then it
//                    stays alive as the daemon itself
//   HTTP             GET  /hello           identity for discovery
//                    GET  /resolve         parked conflicts (script + property)
//                    POST /resolve         answer one, 404 on an unknown id
//                    GET  /review          pending disk review (stats only)
//                    GET  /review/details  cursor-paged, increasing ids
//                    POST /review/push     all, or ≤2048 ids; {pushed,remaining}
//                    POST /review/dismiss  drop the review
//                    GET  /choice          pending divergence (stats only)
//                    GET  /choice/details  cursor-paged, dense sequential ids
//                    GET  /choice/source   one row's two sides, ≤256 KiB each
//                    POST /choice          studio · disk · cancel
//                    POST /choice/selection  chunked ids with receipts
//                    POST /manager-heartbeat, /manager-close, /stop
//   WebSocket        /ws — a hand-rolled RFC 6455 text-frame endpoint: accept
//                    key handshake, unmasked server frames, masked client
//                    frames decoded, server hello, periodic ping, and a
//                    scripted `event` stream so the Activity feed can be
//                    exercised end to end, plus the `conflict`, `disk-review`,
//                    `choice-needed` and `choice-made` topics.
//
// Two connect surfaces, because Design 7.0 left both live:
//
//   default        **code scope**. Connecting applies Studio → disk and raises
//                  a passive `disk-review` over what disk still holds. The
//                  `/review*` routes answer; `/choice` says nothing is pending.
//   --full-scope   the pre-7.0 blocking flow, which a `scope: "full"` project
//                  still gets: `choice-needed` and the `/choice*` routes.
//
// `--divergence n` sizes whichever set the scope selects, so both paths are
// driven by the same knob.
//
// It is a *mock*, not a reference implementation: no compression, no
// extensions, no binary frames, no long-poll fallback, and its "sync" is a
// canned script. Where it does answer, it answers in the shape the real daemon
// promises — including the auth checks, so a desktop that forgets the owner
// token fails here rather than in production.
//
// Usage mirrors the real spawn contract:
//
//   node mock-daemon.mjs daemon start --project /path [--port 7978] \
//     --managed-by desktop --owner-token-env WSYNC_OWNER_TOKEN \
//     --data-dir /path --raw
//
// Mock-only switches, for exercising the desktop's unhappy paths:
//
//   --fail "<message>"   print {"ok":false,…} and exit 1
//   --already-running    print alreadyRunning:true and exit 0 immediately
//   --no-resolve         answer 404 on /resolve (an engine predating conflicts)
//   --silent-start       never print a ready line (exercise the start timeout)
//   --event-interval ms  gap between scripted event frames (default 2500)
//   --ping-interval ms   gap between server pings (default 5000)
//   --conflicts [n]      park n conflicts (default 6), cycling every archetype
//   --divergence [n]     hold a pending set over n paths (default 1500) — a
//                        disk review, or a choice under --full-scope
//   --full-scope         the pre-7.0 blocking choice flow instead of the review
//   --bad-receipt [n]    corrupt the receipt for selection chunk n (default 1)
//   --bad-remaining      answer a push with a `remaining` that does not add up
//   --resolved-elsewhere answer 409 {error:"resolved"} to every choice write
//   --no-plugin          no Studio is connected: 503 on /choice/source and on
//                        POST /review/push (a push needs a plugin to receive
//                        it; dismissing does not)

import http from "node:http";
import crypto from "node:crypto";
import path from "node:path";
import process from "node:process";

// ------------------------------------------------------------------ argv ---

const WS_GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
const DEFAULT_PORT = 7978;
const PORT_SCAN_END = 7990;
const NAME = "wsync-mock";
const VERSION = "0.1.0-mock";
const PROTOCOL = 1;
/** Obviously-a-mock stand-ins for the engine's compiled-in git identity. */
const BUILD_COMMIT = "0000000mock0";
const BUILD_DIRTY = false;

function parseArgv(argv) {
  const options = {
    command: [],
    project: null,
    port: null,
    managedBy: null,
    ownerTokenEnv: null,
    dataDir: null,
    raw: false,
    fail: null,
    alreadyRunning: false,
    noResolve: false,
    silentStart: false,
    eventInterval: 2500,
    pingInterval: 5000,
    managerTimeout: 300_000,
    conflicts: 0,
    divergence: 0,
    fullScope: false,
    badReceipt: null,
    badRemaining: false,
    resolvedElsewhere: false,
    noPlugin: false,
  };

  for (let index = 0; index < argv.length; index += 1) {
    const argument = argv[index];
    const next = () => argv[++index];
    /** For switches whose count is optional: `--divergence` or `--divergence 40`. */
    const optionalCount = (fallback) => {
      const peek = argv[index + 1];
      if (peek !== undefined && /^\d+$/u.test(peek)) {
        index += 1;
        return Number(peek);
      }
      return fallback;
    };
    switch (argument) {
      case "--project": options.project = next(); break;
      case "--port": options.port = Number(next()); break;
      case "--managed-by": options.managedBy = next(); break;
      case "--owner-token-env": options.ownerTokenEnv = next(); break;
      case "--data-dir": options.dataDir = next(); break;
      case "--raw": options.raw = true; break;
      case "--fail": options.fail = next(); break;
      case "--already-running": options.alreadyRunning = true; break;
      case "--no-resolve": options.noResolve = true; break;
      case "--silent-start": options.silentStart = true; break;
      case "--event-interval": options.eventInterval = Number(next()); break;
      case "--ping-interval": options.pingInterval = Number(next()); break;
      case "--manager-timeout": options.managerTimeout = Number(next()); break;
      case "--conflicts": options.conflicts = optionalCount(6); break;
      case "--divergence": options.divergence = optionalCount(1500); break;
      case "--full-scope": options.fullScope = true; break;
      case "--bad-receipt": options.badReceipt = optionalCount(1); break;
      case "--bad-remaining": options.badRemaining = true; break;
      case "--resolved-elsewhere": options.resolvedElsewhere = true; break;
      case "--no-plugin": options.noPlugin = true; break;
      default:
        if (argument.startsWith("--")) throw new Error(`unknown flag ${argument}`);
        options.command.push(argument);
    }
  }
  return options;
}

// ----------------------------------------------------------- ws framing ---

const OPCODE = { continuation: 0x0, text: 0x1, binary: 0x2, close: 0x8, ping: 0x9, pong: 0xa };

/** Server → client: never masked, FIN always set (no fragmentation here). */
export function encodeFrame(opcode, payload = "") {
  const data = Buffer.isBuffer(payload) ? payload : Buffer.from(String(payload), "utf8");
  let header;
  if (data.length < 126) {
    header = Buffer.alloc(2);
    header[1] = data.length;
  } else if (data.length < 65_536) {
    header = Buffer.alloc(4);
    header[1] = 126;
    header.writeUInt16BE(data.length, 2);
  } else {
    header = Buffer.alloc(10);
    header[1] = 127;
    header.writeBigUInt64BE(BigInt(data.length), 2);
  }
  header[0] = 0x80 | opcode;
  return Buffer.concat([header, data]);
}

/**
 * A streaming frame decoder. Returns `push(chunk)`; every complete frame goes
 * to `onFrame({opcode, payload})`, and a protocol violation goes to
 * `onError(code, reason)` with an RFC 6455 close code.
 *
 * `requireMask` is the direction switch: a *server* reading client frames must
 * insist on a mask (§5.1), a *client* reading server frames must insist there
 * is none. The same decoder therefore serves both sides, and the test's client
 * checks this implementation rather than trusting it.
 */
export function createFrameReader(onFrame, onError = () => {}, { requireMask = true } = {}) {
  let buffer = Buffer.alloc(0);
  let fragmentOpcode = null;
  let fragments = [];

  return function push(chunk) {
    buffer = buffer.length === 0 ? chunk : Buffer.concat([buffer, chunk]);

    for (;;) {
      if (buffer.length < 2) return;
      const first = buffer[0];
      const second = buffer[1];
      const fin = (first & 0x80) !== 0;
      const opcode = first & 0x0f;
      const masked = (second & 0x80) !== 0;
      let length = second & 0x7f;
      let offset = 2;

      if (length === 126) {
        if (buffer.length < offset + 2) return;
        length = buffer.readUInt16BE(offset);
        offset += 2;
      } else if (length === 127) {
        if (buffer.length < offset + 8) return;
        const wide = buffer.readBigUInt64BE(offset);
        if (wide > BigInt(Number.MAX_SAFE_INTEGER)) return onError(1009, "frame too large");
        length = Number(wide);
        offset += 8;
      }

      // RFC 6455 §5.1: every client frame must be masked, and no server frame
      // may be. A server that accepts unmasked client frames is a
      // proxy-poisoning hazard, so the mock refuses them.
      if (requireMask && !masked) return onError(1002, "client frames must be masked");
      if (!requireMask && masked) return onError(1002, "server frames must not be masked");

      let mask = null;
      if (masked) {
        if (buffer.length < offset + 4) return;
        mask = buffer.subarray(offset, offset + 4);
        offset += 4;
      }

      if (buffer.length < offset + length) return;
      const payload = Buffer.from(buffer.subarray(offset, offset + length));
      if (mask) {
        for (let index = 0; index < payload.length; index += 1) {
          payload[index] ^= mask[index % 4];
        }
      }
      buffer = buffer.subarray(offset + length);

      if (opcode === OPCODE.continuation) {
        if (fragmentOpcode === null) return onError(1002, "continuation without a start");
        fragments.push(payload);
        if (fin) {
          const joined = Buffer.concat(fragments);
          const started = fragmentOpcode;
          fragmentOpcode = null;
          fragments = [];
          onFrame({ opcode: started, payload: joined });
        }
        continue;
      }

      if (!fin && (opcode === OPCODE.text || opcode === OPCODE.binary)) {
        fragmentOpcode = opcode;
        fragments = [payload];
        continue;
      }

      onFrame({ opcode, payload });
    }
  };
}

export function acceptKey(clientKey) {
  return crypto.createHash("sha1").update(clientKey + WS_GUID).digest("base64");
}

// --------------------------------------------------------- scripted feed ---

/**
 * The event stream the Activity feed renders. Flat frames with a `type` tag and
 * inline fields (Design §5.3), on the `sync-activity` and `plugin-status`
 * topics. Cycled forever so a running app always has something arriving.
 *
 * Every `sync-activity` frame carries the pinned trio: `direction`, `counts`,
 * and `names` — at most `MAX_ACTIVITY_NAMES` path names. `names` is what feeds
 * the desktop's last-edited store (Design §7.3), which is why it is a list of
 * *paths* and not a prose summary: the store keys on the same path the
 * divergence set projects each entry to, or the two never line up.
 */
const MAX_ACTIVITY_NAMES = 10;

const EVENT_SCRIPT = [
  {
    topic: "plugin-status",
    connected: true,
    place: "Baseplate",
    placeId: 1818,
    clientName: "WSync Studio plugin",
  },
  {
    topic: "sync-activity",
    category: "sync",
    tone: "ok",
    title: "Updated ReplicatedStorage.Shared.Signal",
    intent: "disk → studio",
    direction: "disk-to-studio",
    counts: { added: 0, updated: 1, removed: 0 },
    names: ["src/shared/Signal.luau"],
    facts: { path: "src/shared/Signal.luau", kind: "update", bytes: 2481 },
    durationMs: 12,
  },
  {
    topic: "sync-activity",
    category: "sync",
    tone: "ok",
    title: "Created ServerScriptService.Services.Economy",
    intent: "disk → studio",
    direction: "disk-to-studio",
    counts: { added: 1, updated: 0, removed: 0 },
    names: ["src/server/Services/Economy.server.luau"],
    facts: { path: "src/server/Services/Economy.server.luau", kind: "add" },
    durationMs: 31,
  },
  {
    topic: "sync-activity",
    category: "syncback",
    tone: "info",
    title: "Wrote src/client/Controllers/Camera.client.luau",
    intent: "studio → disk",
    direction: "studio-to-disk",
    counts: { added: 0, updated: 1, removed: 0 },
    names: ["src/client/Controllers/Camera.client.luau"],
    facts: { path: "src/client/Controllers/Camera.client.luau", kind: "update" },
    durationMs: 8,
  },
  // Filled in at send time from the *live* divergence set, so the desktop's
  // last-edited store ends up holding stamps for paths that are actually in the
  // list the modal sorts. Skipped when no set is pending.
  { topic: "sync-activity", dynamic: "divergence" },
  {
    topic: "sync-activity",
    category: "conflict",
    tone: "warn",
    title: "Parked a conflict on src/shared/Types.luau",
    intent: "both sides changed",
    direction: "both",
    counts: { added: 0, updated: 0, removed: 0, conflicted: 1 },
    names: ["src/shared/Types.luau"],
    facts: { path: "src/shared/Types.luau", kind: "conflict" },
  },
  // Filled in from a live parked conflict at send time; skipped when none are
  // left, so the feed never announces something `/resolve` will not list.
  { topic: "conflict" },
  {
    topic: "sync-activity",
    category: "sync",
    tone: "ok",
    title: "Removed StarterGui.Hud.Legacy",
    intent: "disk → studio",
    direction: "disk-to-studio",
    counts: { added: 0, updated: 0, removed: 1 },
    names: ["src/client/UI/Legacy.client.luau"],
    facts: { path: "src/client/UI/Legacy.client.luau", kind: "remove" },
    durationMs: 5,
  },
];

// ------------------------------------------------- scripted conflict set ---

/**
 * The five shapes a parked conflict comes in (Design 6.3, 8.2). Between them
 * they cover both `kind`s and all three classifications, so the Conflicts view
 * can be driven through every branch it has: text diff, property-table diff,
 * a truncated source, and the two one-sided deletions.
 */
const CONFLICT_ARCHETYPES = [
  {
    kind: "script",
    classification: "both-edited",
    path: "src/shared/Types.luau",
    instancePath: "ReplicatedStorage.Shared.Types",
    class: "ModuleScript",
    fs: {
      present: true,
      hash: "f1a0",
      source: [
        "export type Vector = { x: number, y: number }",
        "",
        "export type Player = {",
        "\tid: number,",
        "\tname: string,",
        "\tposition: Vector,",
        "}",
        "",
        "return {}",
      ].join("\n"),
    },
    studio: {
      present: true,
      hash: "b7c2",
      source: [
        "export type Vector = { x: number, y: number, z: number }",
        "",
        "export type Player = {",
        "\tid: number,",
        "\tname: string,",
        "\tteam: string?,",
        "\tposition: Vector,",
        "}",
        "",
        "return {}",
      ].join("\n"),
    },
  },
  {
    kind: "properties",
    classification: "both-edited",
    path: "src/client/UI/Hud.model.json",
    instancePath: "StarterGui.Hud.Root",
    class: "Frame",
    fs: {
      present: true,
      hash: "2c9d",
      fsProps: {
        Size: { UDim2: [[0, 320], [0, 180]] },
        BackgroundColor3: { Color3: [0.1, 0.1, 0.12] },
        Visible: { Bool: true },
        Name: { String: "Root" },
      },
    },
    studio: {
      present: true,
      hash: "5e10",
      studioProps: {
        Size: { UDim2: [[0, 360], [0, 180]] },
        BackgroundColor3: { Color3: [0.1, 0.1, 0.12] },
        Visible: { Bool: false },
        BackgroundTransparency: { Float32: 0.25 },
      },
    },
  },
  {
    kind: "script",
    classification: "fs-deleted-studio-edited",
    path: "src/server/Services/Matchmaking.server.luau",
    instancePath: "ServerScriptService.Services.Matchmaking",
    class: "Script",
    fs: { present: false, hash: null },
    studio: {
      present: true,
      hash: "9930",
      source: ["local Matchmaking = {}", "", "function Matchmaking.queue(player)", "\treturn true", "end", "", "return Matchmaking"].join("\n"),
    },
  },
  {
    kind: "script",
    classification: "both-edited",
    path: "src/client/Controllers/Camera.client.luau",
    instancePath: "StarterPlayer.StarterPlayerScripts.Camera",
    class: "LocalScript",
    truncated: true,
    fs: {
      present: true,
      hash: "aa41",
      truncated: true,
      source: ["-- 512 KiB of camera maths; the daemon sends the first 256 KiB", "local Camera = {}", "Camera.fov = 70", "return Camera"].join("\n"),
    },
    studio: {
      present: true,
      hash: "aa77",
      truncated: true,
      source: ["-- 512 KiB of camera maths; the daemon sends the first 256 KiB", "local Camera = {}", "Camera.fov = 82", "Camera.shake = true", "return Camera"].join("\n"),
    },
  },
  {
    kind: "properties",
    classification: "studio-deleted-fs-edited",
    path: "src/shared/Config/Balance.json",
    instancePath: "ReplicatedStorage.Shared.Config.Balance",
    class: "Configuration",
    fs: {
      present: true,
      hash: "31bd",
      fsProps: {
        Attributes: { String: "{\"maxPlayers\":24}" },
        Name: { String: "Balance" },
      },
    },
    studio: { present: false, hash: null },
  },
];

function buildConflicts(count) {
  const now = Date.now();
  return Array.from({ length: count }, (_, index) => {
    const archetype = CONFLICT_ARCHETYPES[index % CONFLICT_ARCHETYPES.length];
    const round = Math.floor(index / CONFLICT_ARCHETYPES.length);
    const suffix = round === 0 ? "" : `.${round}`;
    return {
      ...archetype,
      id: `cf_${index + 1}`,
      path: archetype.path ? archetype.path.replace(/(\.\w+)$/u, `${suffix}$1`) : null,
      instancePath: `${archetype.instancePath}${suffix}`,
      detectedAt: new Date(now - index * 47_000).toISOString(),
    };
  });
}

// --------------------------------------------- scripted divergence set 7.2 --

const DIVERGENCE_ROOTS = [
  ["src/shared", "ReplicatedStorage.Shared", "ModuleScript", ".luau"],
  ["src/server/Services", "ServerScriptService.Services", "Script", ".server.luau"],
  ["src/client/Controllers", "StarterPlayer.StarterPlayerScripts", "LocalScript", ".client.luau"],
  ["src/client/UI", "StarterGui", "Frame", ".model.json"],
  ["src/shared/Config", "ReplicatedStorage.Shared.Config", "Configuration", ".json"],
];

const DIVERGENCE_STATES = ["only-on-disk", "differs", "differs", "missing-on-disk"];

/**
 * A frozen divergence set with **dense sequential ids** — the property the
 * desktop verifies every page against (Design 7.3). Big enough by default to
 * force several pages, and every fourth Studio-only entry gets a null `path`
 * so the client's "no predicted file path" branch is exercised too.
 */
function buildDivergence(count) {
  const entries = Array.from({ length: count }, (_, id) => {
    const [folder, instanceRoot, className, extension] = DIVERGENCE_ROOTS[id % DIVERGENCE_ROOTS.length];
    const state = DIVERGENCE_STATES[id % DIVERGENCE_STATES.length];
    const name = `Module${String(id).padStart(4, "0")}`;
    const unpredictable = state === "missing-on-disk" && id % 4 === 3;
    return {
      id,
      path: unpredictable ? null : `${folder}/${name}${extension}`,
      instancePath: `${instanceRoot}.${name}`,
      state,
      class: className,
    };
  });

  const tally = (state) => entries.filter((entry) => entry.state === state).length;
  return {
    choiceId: `ch_${crypto.randomUUID().slice(0, 8)}`,
    entries,
    stats: {
      total: entries.length,
      studioCount: 8412,
      diskCount: 8398,
      onlyOnDisk: tally("only-on-disk"),
      differs: tally("differs"),
      missingOnDisk: tally("missing-on-disk"),
    },
  };
}

/** Design 7.3 bounds a selection chunk at 2048 ids. */
const MAX_SELECTION_IDS = 2048;
/** Contract: `GET /choice/details` pages at most 1024 entries. */
const MAX_DETAIL_LIMIT = 1024;

// ------------------------------------------------ scripted disk review 7.0 --

/** The two states a review entry can be in. Studio-only rows do not exist. */
const REVIEW_STATES = ["disk-only", "differs", "differs", "disk-only"];

/**
 * A pending disk review: what disk still holds after the connect applied
 * Studio → disk.
 *
 * Ids are dense here, but the contract only promises they *increase* — pushing
 * a subset removes entries and the rest keep their ids, which is exactly the
 * property the desktop's page verifier has to tolerate and the choice set's
 * "dense from the cursor" rule cannot. Every fourth `disk-only` row has a null
 * `instancePath`, because a file Studio has never seen has no instance yet.
 */
function buildReview(count) {
  const entries = Array.from({ length: count }, (_, id) => {
    const [folder, instanceRoot, className, extension] =
      DIVERGENCE_ROOTS[id % DIVERGENCE_ROOTS.length];
    const state = REVIEW_STATES[id % REVIEW_STATES.length];
    const name = `Module${String(id).padStart(4, "0")}`;
    const unseen = state === "disk-only" && id % 4 === 3;
    return {
      id,
      // Always present: a review entry is a file that exists on disk.
      path: `${folder}/${name}${extension}`,
      instancePath: unseen ? null : `${instanceRoot}.${name}`,
      state,
      class: className,
    };
  });

  return {
    reviewId: `rv_${crypto.randomUUID().slice(0, 8)}`,
    entries,
    /** Ids already pushed or dismissed, so a repeat push is a no-op. */
    pushed: new Set(),
  };
}

/** The stats block `GET /review` and the `disk-review` event both carry. */
function reviewStats(review) {
  const pending = review.entries.filter((entry) => !review.pushed.has(entry.id));
  return {
    total: pending.length,
    diskOnly: pending.filter((entry) => entry.state === "disk-only").length,
    differs: pending.filter((entry) => entry.state === "differs").length,
  };
}

/** Design 7.0 bounds one push at 2048 ids, like a selection chunk. */
const MAX_PUSH_IDS = 2048;

// ------------------------------------------------- scripted row sources 7.3 --

/** Contract: each side of `/choice/source` is at most 256 KiB. */
const SOURCE_LIMIT = 256 * 1024;

/**
 * Which classes are script-backed, and therefore diffable.
 *
 * The contract answers 400 for everything else: a `Frame`'s divergence is a
 * property difference, and running a text differ over serialized properties
 * would diff the serializer rather than the change (the same reason the
 * Conflicts view has two card shapes).
 */
const SCRIPT_CLASSES = new Set(["ModuleScript", "Script", "LocalScript"]);

/**
 * Every 25th row is a file bigger than the transfer ceiling, so the truncated
 * pair is reachable from the first page of a small divergence set (`--divergence
 * 40` puts one at id 1) rather than only in a set nobody generates by hand.
 */
function isOversizedRow(id) {
  return id % 25 === 1;
}

/**
 * Real-looking Luau for one side of one row.
 *
 * The two sides differ in a handful of lines — a changed signature, a changed
 * constant, an added guard — which is what a genuine "differs" row looks like
 * and what makes the aligner's common-prefix/suffix trimming do real work.
 */
function luauSource(entry, side) {
  const name = entry.instancePath.split(".").pop();
  const studio = side === "studio";
  const lines = [
    "--!strict",
    `-- ${entry.path ?? entry.instancePath}`,
    `-- ${studio ? "as it stands in the open place" : "as it stands on disk"}`,
    "",
    "local RunService = game:GetService(\"RunService\")",
    "",
    `local ${name} = {}`,
    `${name}.__index = ${name}`,
    "",
    `export type ${name} = typeof(setmetatable({} :: {`,
    "\tenabled: boolean,",
    "\tbudget: number,",
    ...(studio ? ["\tscale: number,"] : []),
    `}, ${name}))`,
    "",
    `function ${name}.new(): ${name}`,
    `\treturn setmetatable({`,
    "\t\tenabled = true,",
    `\t\tbudget = ${studio ? "1 / 30" : "1 / 60"},`,
    ...(studio ? ["\t\tscale = 1,"] : []),
    `\t}, ${name})`,
    "end",
    "",
    `function ${name}.step(self: ${name}, dt: number)`,
    "\tif not self.enabled then",
    "\t\treturn 0",
    "\tend",
    ...(studio
      ? ["\tif not RunService:IsRunning() then", "\t\treturn 0", "\tend"]
      : []),
    `\treturn math.min(dt${studio ? " * self.scale" : ""}, self.budget)`,
    "end",
    "",
    `return ${name}`,
  ];
  return lines.join("\n");
}

/**
 * An oversized file: the same module padded past the ceiling with generated
 * data, so the truncation is real rather than a flag on a short string.
 */
function oversizedSource(entry, side) {
  const head = luauSource(entry, side);
  const filler = [];
  let size = head.length;
  let index = 0;
  while (size < SOURCE_LIMIT + 8 * 1024) {
    const line = `\t[${index}] = Vector3.new(${index % 97}, ${(index * 7) % 89}, ${(index * 13) % 83}),`;
    filler.push(line);
    size += line.length + 1;
    index += 1;
  }
  return `${head}\n\n-- generated lookup table\nlocal LOOKUP = {\n${filler.join("\n")}\n}\n`;
}

/**
 * One side of one row, already clipped to the contract's ceiling.
 *
 * `truncated` is the daemon telling the client the diff below the cut is
 * unknown — not a hint that the file is large. The desktop renders it as a
 * caveat on the diff for exactly that reason.
 */
function sourceSide(entry, side, present) {
  if (!present) return { present: false };
  const full = isOversizedRow(entry.id)
    ? oversizedSource(entry, side)
    : luauSource(entry, side);
  if (full.length > SOURCE_LIMIT) {
    return { present: true, source: full.slice(0, SOURCE_LIMIT), truncated: true };
  }
  return { present: true, source: full, truncated: false };
}

/** The `GET /choice/source` body for one row of the frozen set. */
function buildRowSource(entry) {
  return {
    id: entry.id,
    path: entry.path,
    instancePath: entry.instancePath,
    state: entry.state,
    // A row that is only on disk has no Studio side to read, and vice versa.
    // Absent is a state, not a missing field.
    disk: sourceSide(entry, "disk", entry.state !== "missing-on-disk"),
    studio: sourceSide(entry, "studio", entry.state !== "only-on-disk"),
  };
}

// ---------------------------------------------------------------- daemon ---

export function createMockDaemon(options = {}) {
  const {
    project = process.cwd(),
    ownerToken = null,
    managedBy = null,
    noResolve = false,
    eventInterval = 2500,
    pingInterval = 5000,
    managerTimeout = 0,
    conflicts: conflictCount = 0,
    divergence: divergenceCount = 0,
    fullScope = false,
    badReceipt = null,
    badRemaining = false,
    resolvedElsewhere = false,
    noPlugin = false,
    onShutdown = () => {},
  } = options;

  const bootId = crypto.randomUUID();
  const canonicalProject = path.resolve(project);
  const clients = new Set();
  const timers = new Set();
  let lastHeartbeat = Date.now();
  let closing = false;

  /**
   * Whether Studio's plugin is connected right now. Seeded by `--no-plugin`
   * and mutable through `setPluginConnected`, so a harness can model the
   * plugin arriving mid-session — the exact situation behind a 503'd push.
   */
  let pluginAbsent = noPlugin;

  /** Parked conflicts, by id, so `POST /resolve` can actually remove one. */
  const conflicts = new Map(buildConflicts(conflictCount).map((entry) => [entry.id, entry]));
  // Two sample backlog entries, so the app's list and its expiry countdown
  // have something to render without a real clash having happened.
  const backlog = new Map(
    [
      { id: "bk_1", path: "src/ReplicatedStorage/Hello.luau", reason: "initial-sync", bytes: 214 },
      { id: "bk_2", path: "src/ServerScriptService/Main.server.luau", reason: "conflict", bytes: 1042 },
    ].map((entry) => [
      entry.id,
      { ...entry, capturedAt: Math.floor(Date.now() / 1000), secondsRemaining: 24 * 60 * 60 },
    ]),
  );

  /**
   * The frozen divergence set, or null. Only a full-scope project has one:
   * Design 7.0 made the code-scope connect promptless, so there is no choice to
   * freeze — the daemon has already applied Studio → disk.
   */
  let choice = fullScope && divergenceCount > 0 ? buildDivergence(divergenceCount) : null;
  /** How the pending choice was answered, once it has been. */
  let decision = null;
  /** In-flight `POST /choice/selection` state: one submission at a time. */
  let selection = null;

  /** Design 7.0's pending disk review, or null. The code-scope surface. */
  let review = !fullScope && divergenceCount > 0 ? buildReview(divergenceCount) : null;

  const identity = {
    name: NAME,
    version: VERSION,
    // The real engine compiles these in from git (`WSYNC_BUILD_COMMIT` /
    // `WSYNC_BUILD_DIRTY`). The mock carries fixed stand-ins so Settings →
    // About has a build identity to render and copy without an engine build.
    commit: BUILD_COMMIT,
    dirty: BUILD_DIRTY,
    protocol: PROTOCOL,
    project,
    canonicalProject,
    gameId: 4242424242,
    placeIds: [1818, 1819],
    bootId,
    pid: process.pid,
    port: 0, // filled in once the socket is bound
    managedBy,
    projectInit: false,
  };

  const server = http.createServer((request, response) => {
    const url = new URL(request.url ?? "/", "http://127.0.0.1");
    const route = url.pathname;

    if (request.method === "GET" && route === "/hello") {
      return sendJson(response, 200, identity);
    }

    // The backlog: disk content that lost to Studio. Sync never asks a
    // question, so these are recoverable losers rather than pending work.
    if (request.method === "GET" && route === "/backlog") {
      const entries = [...backlog.values()];
      return sendJson(response, 200, {
        total: entries.length,
        ttlSeconds: 24 * 60 * 60,
        entries,
      });
    }

    if (request.method === "POST" && route === "/backlog/restore") {
      return readJson(request, (body) => {
        const id = typeof body?.id === "string" ? body.id : "";
        const entry = backlog.get(id);
        if (!entry) {
          return sendJson(response, 404, { ok: false, error: "no such backlog entry" });
        }
        backlog.delete(id);
        return sendJson(response, 200, { ok: true, path: entry.path, restoredTo: entry.path });
      });
    }

    if (request.method === "POST" && route === "/backlog/drop") {
      return readJson(request, (body) => {
        if (body?.all === true) {
          const dropped = backlog.size;
          backlog.clear();
          return sendJson(response, 200, { ok: true, dropped });
        }
        const id = typeof body?.id === "string" ? body.id : "";
        if (!backlog.delete(id)) {
          return sendJson(response, 404, { ok: false, error: "no such backlog entry" });
        }
        return sendJson(response, 200, { ok: true, dropped: 1 });
      });
    }

    if (request.method === "GET" && route === "/resolve") {
      // An engine that predates the conflict engine answers 404 here; the app
      // has to degrade rather than show a wrong count.
      if (noResolve) return sendJson(response, 404, { error: "unknown route" });
      const parked = [...conflicts.values()];
      return sendJson(response, 200, { conflicts: parked, total: parked.length, partial: false });
    }

    if (request.method === "POST" && route === "/resolve") {
      if (noResolve) return sendJson(response, 404, { error: "unknown route" });
      return readJson(request, (body) => {
        const keep = body?.keep;
        if (keep !== "local" && keep !== "studio") {
          return sendJson(response, 400, { ok: false, error: "keep must be local or studio" });
        }
        // The contract carries the same value twice; a client that sends two
        // different ones has a bug the mock should surface, not paper over.
        if (body?.choice !== undefined && body.choice !== keep) {
          return sendJson(response, 400, { ok: false, error: "choice must match keep" });
        }
        if (!conflicts.has(body?.id)) {
          return sendJson(response, 404, { ok: false, error: "unknown conflict id" });
        }
        conflicts.delete(body.id);
        return sendJson(response, 200, { ok: true, resolved: body.id });
      });
    }

    // --- Design 7.0's passive disk review ---------------------------------
    //
    // Nothing here blocks a sync: by the time these answer, the daemon has
    // already applied Studio → disk and live sync is running. `/review` is the
    // durable half of the `disk-review` event, and both carry counts only.

    if (request.method === "GET" && route === "/review") {
      if (!pendingReview()) return sendJson(response, 200, { pending: false });
      return sendJson(response, 200, {
        pending: true,
        reviewId: review.reviewId,
        stats: reviewStats(review),
      });
    }

    if (request.method === "GET" && route === "/review/details") {
      if (!pendingReview()) {
        return sendJson(response, 404, { error: "no disk review is pending" });
      }
      if (url.searchParams.get("reviewId") !== review.reviewId) {
        return sendJson(response, 409, { error: "stale reviewId" });
      }
      const pending = pendingEntries();
      const cursor = Number(url.searchParams.get("cursor") ?? 0);
      const requested = Number(url.searchParams.get("limit") ?? 256);
      if (!Number.isInteger(cursor) || cursor < 0 || cursor > pending.length) {
        return sendJson(response, 400, { error: "bad cursor" });
      }
      if (!Number.isInteger(requested) || requested < 1 || requested > MAX_DETAIL_LIMIT) {
        return sendJson(response, 400, { error: `limit must be 1…${MAX_DETAIL_LIMIT}` });
      }
      // The cursor indexes what is *still pending*, so a push shrinks the list
      // under it — and the ids that survive keep the values the client already
      // holds, which is what makes a second push of the same list valid.
      const items = pending.slice(cursor, cursor + requested);
      const next = cursor + items.length;
      return sendJson(response, 200, {
        reviewId: review.reviewId,
        items,
        totalCount: pending.length,
        ...(next < pending.length ? { nextCursor: next } : {}),
      });
    }

    if (request.method === "POST" && route === "/review/push") {
      return readJson(request, (body) => {
        if (!pendingReview() || body?.reviewId !== review.reviewId) {
          // 404, not 409: a review that is gone has no ids to argue about, and
          // the client's answer is the same either way — reload or close.
          return sendJson(response, 404, { ok: false, error: "unknown reviewId" });
        }
        if (pluginAbsent) {
          // The contract's one retryable refusal: a push lands in the open
          // place, so it needs a plugin to receive it. Answered before any
          // mutation — a 503'd push moved nothing.
          return sendJson(response, 503, { ok: false, error: "no Studio plugin is connected" });
        }

        let ids;
        if (body.mode === "all") {
          if (body.ids !== undefined) {
            return sendJson(response, 400, { ok: false, error: "mode:all takes no ids" });
          }
          ids = pendingEntries().map((entry) => entry.id);
        } else if (Array.isArray(body.ids)) {
          if (body.ids.length === 0 || body.ids.length > MAX_PUSH_IDS) {
            return sendJson(response, 400, {
              ok: false,
              error: `ids must be an array of 1…${MAX_PUSH_IDS}`,
            });
          }
          if (!body.ids.every((id) => Number.isInteger(id) && id >= 0 && id < review.entries.length)) {
            return sendJson(response, 400, { ok: false, error: "an id is outside the review" });
          }
          ids = body.ids;
        } else {
          return sendJson(response, 400, { ok: false, error: "a push needs ids or mode:all" });
        }

        // Repeatable by construction: an id that has already been pushed is
        // simply not pushed again, and does not count.
        let pushed = 0;
        for (const id of ids) {
          if (review.pushed.has(id)) continue;
          review.pushed.add(id);
          pushed += 1;
        }
        const remaining = reviewStats(review).total;
        broadcast({
          topic: "sync-activity",
          category: "sync",
          tone: "ok",
          title: `Pushed ${pushed} disk items to Studio`,
          intent: "disk → studio",
          direction: "disk-to-studio",
          counts: { added: 0, updated: pushed, removed: 0 },
          names: [],
          facts: { paths: pushed, kind: "review-push" },
        });
        if (remaining === 0) review = null;

        return sendJson(response, 200, {
          ok: true,
          pushed,
          // The failure switch: a `remaining` that does not follow from what was
          // pushed. A client that trusts it reports a finished review while
          // items are still waiting.
          remaining: badRemaining ? remaining + ids.length + 1 : remaining,
        });
      });
    }

    if (request.method === "POST" && route === "/review/dismiss") {
      return readJson(request, (body) => {
        if (!pendingReview() || body?.reviewId !== review.reviewId) {
          return sendJson(response, 404, { ok: false, error: "unknown reviewId" });
        }
        review = null;
        return sendJson(response, 200, { ok: true, dismissed: true });
      });
    }

    if (request.method === "GET" && route === "/choice") {
      if (!pendingChoice()) return sendJson(response, 200, { pending: false });
      return sendJson(response, 200, {
        pending: true,
        choiceId: choice.choiceId,
        stats: choice.stats,
      });
    }

    if (request.method === "GET" && route === "/choice/details") {
      if (!pendingChoice()) {
        return sendJson(response, 404, { error: "no divergence set is pending" });
      }
      if (url.searchParams.get("choiceId") !== choice.choiceId) {
        return sendJson(response, 409, { error: "stale choiceId" });
      }
      const cursor = Number(url.searchParams.get("cursor") ?? 0);
      const requested = Number(url.searchParams.get("limit") ?? 256);
      if (!Number.isInteger(cursor) || cursor < 0 || cursor > choice.entries.length) {
        return sendJson(response, 400, { error: "bad cursor" });
      }
      if (!Number.isInteger(requested) || requested < 1 || requested > MAX_DETAIL_LIMIT) {
        return sendJson(response, 400, { error: `limit must be 1…${MAX_DETAIL_LIMIT}` });
      }
      const items = choice.entries.slice(cursor, cursor + requested);
      const next = cursor + items.length;
      return sendJson(response, 200, {
        choiceId: choice.choiceId,
        items,
        totalCount: choice.entries.length,
        ...(next < choice.entries.length ? { nextCursor: next } : {}),
      });
    }

    // One row's two sides, for the staging list's inline diff (Design 7.3).
    //
    // The order of the refusals is deliberate and worth stating, because the
    // contract pins three of them without ranking them: whether the row exists
    // (404) and whether it is a script (400) are both decidable from the frozen
    // set alone, so they are answered first. 503 means "the row is diffable and
    // I would fetch it, but Studio is not here" — an answer about the plugin,
    // not about the row.
    if (request.method === "GET" && route === "/choice/source") {
      if (!pendingChoice()) {
        return sendJson(response, 404, { error: "no divergence set is pending" });
      }
      if (url.searchParams.get("choiceId") !== choice.choiceId) {
        // 404 rather than the details route's 409: a stale set has no rows, so
        // the row being asked for does not exist. Either way the client takes
        // the supersede path.
        return sendJson(response, 404, { error: "stale choiceId" });
      }
      const raw = url.searchParams.get("id");
      const id = Number(raw);
      if (raw === null || !Number.isInteger(id) || id < 0 || id >= choice.entries.length) {
        return sendJson(response, 404, { error: "unknown row" });
      }
      const entry = choice.entries[id];
      if (!SCRIPT_CLASSES.has(entry.class)) {
        return sendJson(response, 400, {
          error: "not a script row; property differences are decided by staging, not diffed",
        });
      }
      if (pluginAbsent) {
        return sendJson(response, 503, { error: "no Studio plugin is connected" });
      }
      return sendJson(response, 200, buildRowSource(entry));
    }

    if (request.method === "POST" && route === "/choice") {
      return readJson(request, (body) => {
        const stale = choiceWriteRefusal(body?.choiceId);
        if (stale) return sendJson(response, 409, stale);
        if (!["studio", "disk", "cancel"].includes(body?.choice)) {
          return sendJson(response, 400, { ok: false, error: "choice must be studio, disk or cancel" });
        }
        if (body.mode !== undefined && body.mode !== "all") {
          return sendJson(response, 400, { ok: false, error: "the only mode is all" });
        }
        // Keep Disk is either the whole set (`mode:"all"`) or a selection that
        // has already been committed. Anything else would be the daemon
        // guessing at what to pull, which it must never do.
        if (body.choice === "disk" && body.mode !== "all" && !selection?.committed) {
          return sendJson(response, 409, { ok: false, error: "no committed selection" });
        }

        decision = body.choice;
        broadcast({ topic: "choice-made", choiceId: choice.choiceId, choice: decision });

        if (body.choice === "studio") {
          // Design 7.4-A is a later build: the decision is recorded, the
          // Studio → disk transfer has not run. Saying so is the whole point.
          return sendJson(response, 200, { ok: true, applied: false, pendingApplication: true });
        }
        if (body.choice === "cancel") {
          return sendJson(response, 200, { ok: true, applied: false, cancelled: true });
        }
        return sendJson(response, 200, { ok: true, applied: true });
      });
    }

    if (request.method === "POST" && route === "/choice/selection") {
      return readJson(request, (body) => {
        const stale = choiceWriteRefusal(body?.choiceId);
        if (stale) return sendJson(response, 409, stale);

        const { submissionId, chunkIndex, finalChunk, ids } = body ?? {};
        if (typeof submissionId !== "string" || submissionId === "") {
          return sendJson(response, 400, { ok: false, error: "submissionId is required" });
        }
        if (!Number.isInteger(chunkIndex) || chunkIndex < 0) {
          return sendJson(response, 400, { ok: false, error: "chunkIndex must be an index" });
        }
        if (typeof finalChunk !== "boolean") {
          return sendJson(response, 400, { ok: false, error: "finalChunk must be a boolean" });
        }
        if (!Array.isArray(ids) || ids.length > MAX_SELECTION_IDS) {
          return sendJson(response, 400, { ok: false, error: `ids must be an array of ≤${MAX_SELECTION_IDS}` });
        }
        if (!ids.every((id) => Number.isInteger(id) && id >= 0 && id < choice.entries.length)) {
          return sendJson(response, 400, { ok: false, error: "an id is outside the divergence set" });
        }

        if (chunkIndex === 0) {
          // `restart` on chunk 0 is what makes an abandoned submission
          // harmless: it discards whatever the last one left behind.
          if (body.restart !== true) {
            return sendJson(response, 400, { ok: false, error: "chunk 0 must carry restart:true" });
          }
          selection = { submissionId, nextChunk: 0, ids: [], committed: false };
        }
        if (!selection || selection.submissionId !== submissionId) {
          return sendJson(response, 409, { ok: false, error: "unknown submissionId" });
        }
        if (selection.committed) {
          return sendJson(response, 409, { ok: false, error: "this submission is already committed" });
        }
        if (selection.nextChunk !== chunkIndex) {
          return sendJson(response, 409, {
            ok: false,
            error: `expected chunk ${selection.nextChunk}, got ${chunkIndex}`,
          });
        }

        selection.ids.push(...ids);
        selection.nextChunk = chunkIndex + 1;
        selection.committed = finalChunk;

        const receipt = {
          ok: true,
          acceptedChunk: chunkIndex,
          nextChunk: chunkIndex + 1,
          selectedCount: selection.ids.length,
          committed: finalChunk,
        };
        // The failure switch: a receipt that looks fine but does not match what
        // was sent. A client that trusts it applies the wrong selection.
        if (badReceipt !== null && badReceipt === chunkIndex) {
          receipt.selectedCount += 1;
        }
        return sendJson(response, 200, receipt);
      });
    }

    if (request.method === "POST" && route === "/manager-heartbeat") {
      return readJson(request, (body) => {
        if (!authorized(body)) return sendJson(response, 401, { error: "bad token" });
        lastHeartbeat = Date.now();
        response.writeHead(204).end();
      });
    }

    if (request.method === "POST" && route === "/manager-close") {
      return readJson(request, (body) => {
        if (!authorized(body)) return sendJson(response, 401, { error: "bad token" });
        response.writeHead(204).end();
        shutdown("manager-close");
      });
    }

    if (request.method === "POST" && route === "/stop") {
      return readJson(request, (body) => {
        if (!authorized(body)) return sendJson(response, 401, { error: "bad token" });
        if (body?.bootId !== bootId) {
          return sendJson(response, 409, { error: "boot id mismatch", bootId });
        }
        sendJson(response, 200, { ok: true, stopping: true });
        shutdown("stop");
      });
    }

    sendJson(response, 404, { error: "unknown route", route });
  });

  server.on("upgrade", (request, socket) => {
    const url = new URL(request.url ?? "/", "http://127.0.0.1");
    const key = request.headers["sec-websocket-key"];
    if (url.pathname !== "/ws" || !key) {
      socket.end("HTTP/1.1 400 Bad Request\r\n\r\n");
      return;
    }

    socket.write(
      [
        "HTTP/1.1 101 Switching Protocols",
        "Upgrade: websocket",
        "Connection: Upgrade",
        `Sec-WebSocket-Accept: ${acceptKey(String(key))}`,
        "\r\n",
      ].join("\r\n"),
    );
    socket.setNoDelay(true);
    attachClient(socket);
  });

  function attachClient(socket) {
    const client = { socket, topics: null, greeted: false, timers: new Set() };
    clients.add(client);

    const send = (frame) => {
      if (!socket.destroyed) socket.write(frame);
    };
    const sendJsonFrame = (value) => send(encodeFrame(OPCODE.text, JSON.stringify(value)));
    client.sendJson = sendJsonFrame;

    const close = (code = 1000, reason = "") => {
      const payload = Buffer.alloc(2 + Buffer.byteLength(reason));
      payload.writeUInt16BE(code, 0);
      payload.write(reason, 2);
      send(encodeFrame(OPCODE.close, payload));
      socket.end();
    };

    const read = createFrameReader(
      (frame) => {
        if (frame.opcode === OPCODE.close) return close(1000, "bye");
        if (frame.opcode === OPCODE.ping) return send(encodeFrame(OPCODE.pong, frame.payload));
        if (frame.opcode === OPCODE.pong) return;
        if (frame.opcode !== OPCODE.text) return close(1003, "text frames only");

        let message;
        try {
          message = JSON.parse(frame.payload.toString("utf8"));
        } catch {
          return close(1007, "not JSON");
        }

        if (message?.type === "hello") {
          if (message.protocol !== PROTOCOL) {
            sendJsonFrame({
              type: "shutdown",
              reason: `protocol ${message.protocol} is not supported`,
              code: "protocol_mismatch",
              retryable: false,
            });
            return close(1002, "protocol mismatch");
          }
          client.greeted = true;
          sendJsonFrame({
            type: "hello",
            name: NAME,
            version: VERSION,
            gameId: identity.gameId,
            placeIds: identity.placeIds,
            rootRefs: ["a".repeat(32)],
          });
          startFeed(client, sendJsonFrame);
          return;
        }

        if (message?.type === "event-sub") {
          client.topics = Array.isArray(message.topics) ? message.topics : null;
          return;
        }

        if (message?.type === "pong") return;

        // Anything else is a client → server frame the mock does not model
        // (push, response). Acknowledge nothing rather than pretend.
      },
      (code, reason) => close(code, reason),
    );

    socket.on("data", (chunk) => {
      try {
        read(chunk);
      } catch (error) {
        close(1011, String(error?.message ?? error));
      }
    });
    const drop = () => {
      for (const timer of client.timers) clearInterval(timer);
      clients.delete(client);
    };
    socket.on("close", drop);
    socket.on("error", drop);
  }

  function startFeed(client, sendJsonFrame) {
    // Design 7.0: a connect *is* a sync. The daemon hydrates, diffs, applies
    // Studio → disk, and raises a fresh review over what disk still holds —
    // which is why reconnecting replaces the pending one rather than resuming
    // it: the ids in the old set describe an apply that has since happened
    // again. The desktop has to survive that (its open modal supersedes), so
    // the mock does it for real.
    if (review !== null) {
      review = buildReview(review.entries.length);
      const announce = setTimeout(() => {
        if (!pendingReview()) return;
        if (client.topics && !client.topics.includes("disk-review")) return;
        sendJsonFrame({
          type: "event",
          at: Date.now(),
          topic: "disk-review",
          reviewId: review.reviewId,
          ...reviewStats(review),
        });
      }, 350);
      announce.unref?.();
    }

    // A choice raised before this client connected still has to reach it. The
    // real daemon broadcasts `choice-needed` when it freezes the set; a client
    // that arrives afterwards learns from `GET /choice`. The mock sends it on
    // connect so both paths in the app can be driven from here.
    if (pendingChoice()) {
      const announce = setTimeout(() => {
        if (!pendingChoice()) return;
        if (client.topics && !client.topics.includes("choice-needed")) return;
        sendJsonFrame({
          type: "event",
          at: Date.now(),
          topic: "choice-needed",
          choiceId: choice.choiceId,
          ...choice.stats,
        });
      }, 350);
      announce.unref?.();
    }

    if (pingInterval > 0) {
      // The contract's application-level ping, not the RFC 6455 control frame:
      // the app answers with a `{"type":"pong"}` text frame.
      const ping = setInterval(() => sendJsonFrame({ type: "ping", at: Date.now() }), pingInterval);
      client.timers.add(ping);
      timers.add(ping);
    }
    if (eventInterval > 0) {
      let index = 0;
      const feed = setInterval(() => {
        const scripted = EVENT_SCRIPT[index % EVENT_SCRIPT.length];
        index += 1;
        // Design 8.2: a `conflict` event invalidates the badge, so the frame
        // has to name a conflict `/resolve` will actually list. Once they are
        // all answered there is nothing to announce, and the slot is skipped.
        // The same reasoning applies to the divergence-fed activity slot: it
        // names paths from the live set or it names nothing.
        const entry =
          scripted.topic === "conflict"
            ? liveConflictEvent()
            : scripted.dynamic === "divergence"
              ? liveDivergenceActivity(index)
              : scripted.topic === "plugin-status" && pluginAbsent
                // The plugin state has to be consistent everywhere it shows: a
                // feed announcing a connected plugin while the plugin-needing
                // routes answer 503 would be the mock lying rather than
                // modelling anything.
                ? { ...scripted, connected: false, place: null, placeId: null }
                : scripted;
        if (!entry) return;
        if (client.topics && !client.topics.includes(entry.topic)) return;
        sendJsonFrame({ type: "event", at: Date.now(), seq: index, ...entry });
      }, eventInterval);
      client.timers.add(feed);
      timers.add(feed);
    }
  }

  function authorized(body) {
    if (!ownerToken) return true;
    return typeof body?.token === "string" && body.token === ownerToken;
  }

  /** A divergence set exists and nobody has answered it yet. */
  function pendingChoice() {
    return choice !== null && decision === null;
  }

  /** A disk review exists and still has something in it (Design 7.0). */
  function pendingReview() {
    return review !== null && review.entries.length > review.pushed.size;
  }

  /** What is left of the review, in set order. */
  function pendingEntries() {
    return review === null ? [] : review.entries.filter((entry) => !review.pushed.has(entry.id));
  }

  /**
   * Why a write against the pending choice must be refused, or null.
   *
   * Both cases answer the same 409 the contract pins, because they are the same
   * thing from the client's side: the decision it is holding is no longer the
   * one the daemon is waiting for.
   */
  function choiceWriteRefusal(choiceId) {
    if (resolvedElsewhere) return { ok: false, error: "resolved" };
    if (!pendingChoice()) return { ok: false, error: "resolved" };
    if (choiceId !== choice.choiceId) return { ok: false, error: "resolved" };
    return null;
  }

  /**
   * A `sync-activity` frame naming real paths from the pending divergence set.
   *
   * This is what lets the desktop's last-edited store be exercised against the
   * list it will later sort: the stamps it collects key on the same paths
   * `/choice/details` reports, so "Recently edited" has something to say about
   * rows the user can actually see. Walks the set so successive events stamp
   * different paths rather than the same one forever.
   */
  function liveDivergenceActivity(seed) {
    // Whichever set is live: the review under code scope, the frozen choice
    // under `--full-scope`. Both lists are keyed by the same file paths the
    // ledger stamps, which is what gives "Recently edited" something true to
    // sort by in either surface.
    const source = pendingReview() ? pendingEntries() : pendingChoice() ? choice.entries : [];
    const withPaths = source.filter((entry) => entry.path !== null);
    if (withPaths.length === 0) return null;

    const take = Math.min(MAX_ACTIVITY_NAMES, withPaths.length);
    const start = (seed * take) % withPaths.length;
    const names = Array.from(
      { length: take },
      (_, offset) => withPaths[(start + offset) % withPaths.length].path,
    );

    return {
      topic: "sync-activity",
      category: "sync",
      tone: "ok",
      title: `Synced ${names.length} paths`,
      intent: "disk → studio",
      direction: "disk-to-studio",
      counts: { added: 0, updated: names.length, removed: 0 },
      names,
      facts: { paths: names.length, kind: "update" },
      durationMs: 9 + (seed % 20),
    };
  }

  /** The `conflict` frame for a currently parked conflict, or null. */
  function liveConflictEvent() {
    const parked = conflicts.values().next().value;
    if (!parked) return null;
    return {
      topic: "conflict",
      id: parked.id,
      path: parked.path,
      instancePath: parked.instancePath,
      classification: parked.classification,
    };
  }

  /** Send an event to every client subscribed to its topic. */
  function broadcast(frame) {
    for (const client of clients) {
      if (!client.greeted || typeof client.sendJson !== "function") continue;
      if (client.topics && !client.topics.includes(frame.topic)) continue;
      client.sendJson({ type: "event", at: Date.now(), ...frame });
    }
  }

  function shutdown(reason) {
    if (closing) return;
    closing = true;
    for (const timer of timers) clearInterval(timer);
    for (const client of clients) {
      try {
        client.socket.write(
          encodeFrame(
            OPCODE.text,
            JSON.stringify({ type: "shutdown", reason, code: reason, retryable: false }),
          ),
        );
        client.socket.end();
      } catch {
        // The socket is already gone; nothing to tell.
      }
    }
    clients.clear();
    server.close();
    // Let the /stop response and the close frames flush, then drop whatever
    // keep-alive connections are still holding the server open.
    const settle = setTimeout(() => {
      server.closeAllConnections?.();
      onShutdown(reason);
    }, 150);
    settle.unref?.();
  }

  if (managerTimeout > 0 && managedBy) {
    // Design §3.3: the daemon dies if its manager stops beating. Generous here
    // so a debugger-paused desktop does not lose its daemon.
    const watchdog = setInterval(() => {
      if (Date.now() - lastHeartbeat > managerTimeout) shutdown("manager-timeout");
    }, Math.max(1000, Math.floor(managerTimeout / 5)));
    watchdog.unref?.();
    timers.add(watchdog);
  }

  return {
    server,
    identity,
    bootId,
    shutdown,
    get clientCount() {
      return clients.size;
    },
    async listen(preferredPort) {
      const port = await listenOnFirstFreePort(server, preferredPort);
      identity.port = port;
      return port;
    },

    /**
     * Flip whether the Studio plugin is "connected", and announce it the way
     * the real daemon does — a `plugin-status` event. Connecting is what turns
     * a 503'd `/review/push` (and `/choice/source`) back into a working route,
     * so a harness can drive the desktop's wait-then-retry path end to end.
     */
    setPluginConnected(connected) {
      pluginAbsent = !connected;
      broadcast(
        connected
          ? {
              topic: "plugin-status",
              connected: true,
              place: "Baseplate",
              placeId: 1818,
              clientName: "WSync Studio plugin",
            }
          : { topic: "plugin-status", connected: false, place: null, placeId: null, clientName: null },
      );
    },

    /**
     * Sever every live socket without a shutdown frame — a network drop, not a
     * daemon stop. A reconnecting client re-runs the connect flow, which under
     * a pending review rebuilds it with a fresh reviewId (the supersede path).
     */
    dropClients() {
      for (const client of [...clients]) {
        try {
          client.socket.destroy();
        } catch {
          // Already gone.
        }
      }
    },
  };
}

// ------------------------------------------------------------- http utils ---

function sendJson(response, status, body) {
  const payload = Buffer.from(JSON.stringify(body), "utf8");
  response.writeHead(status, {
    "content-type": "application/json; charset=utf-8",
    "content-length": String(payload.length),
  });
  response.end(payload);
}

function readJson(request, then) {
  const chunks = [];
  let size = 0;
  request.on("data", (chunk) => {
    size += chunk.length;
    // Every body this daemon takes is a small JSON object; anything larger is
    // not a client it should be talking to.
    if (size > 64 * 1024) return request.destroy();
    chunks.push(chunk);
  });
  request.on("end", () => {
    try {
      then(chunks.length ? JSON.parse(Buffer.concat(chunks).toString("utf8")) : {});
    } catch {
      then(null);
    }
  });
}

/** Design §3.2: default 7978, scan 7978–7990. */
function listenOnFirstFreePort(server, preferred) {
  const candidates =
    preferred && Number.isFinite(preferred)
      ? [preferred]
      : Array.from({ length: PORT_SCAN_END - DEFAULT_PORT + 1 }, (_, index) => DEFAULT_PORT + index);

  return new Promise((resolve, reject) => {
    let index = 0;
    const attempt = () => {
      if (index >= candidates.length) {
        reject(new Error(`no free port in ${candidates[0]}–${candidates.at(-1)}`));
        return;
      }
      const port = candidates[index++];
      const onError = (error) => {
        server.removeListener("listening", onListening);
        if (error.code === "EADDRINUSE" || error.code === "EACCES") return attempt();
        reject(error);
      };
      const onListening = () => {
        server.removeListener("error", onError);
        resolve(port);
      };
      server.once("error", onError);
      server.once("listening", onListening);
      server.listen(port, "127.0.0.1");
    };
    attempt();
  });
}

// ------------------------------------------------------------------ main ---

async function main(argv) {
  let options;
  try {
    options = parseArgv(argv);
  } catch (error) {
    process.stdout.write(`${JSON.stringify({ ok: false, error: String(error.message) })}\n`);
    process.exit(2);
  }

  if (options.command[0] !== "daemon" || options.command[1] !== "start") {
    process.stdout.write(
      `${JSON.stringify({ ok: false, error: "usage: mock-daemon.mjs daemon start --project <path>" })}\n`,
    );
    process.exit(2);
  }
  if (!options.project) {
    process.stdout.write(`${JSON.stringify({ ok: false, error: "--project is required" })}\n`);
    process.exit(2);
  }

  if (options.fail) {
    process.stdout.write(
      `${JSON.stringify({ ok: false, error: { code: "mock_failure", message: options.fail } })}\n`,
    );
    process.stderr.write(`mock daemon refused to start: ${options.fail}\n`);
    process.exit(1);
  }

  if (options.alreadyRunning) {
    // The contract's adopt path: report the daemon that already exists and
    // exit at once. The desktop must not keep a handle on this child.
    process.stdout.write(
      `${JSON.stringify({
        ok: true,
        alreadyRunning: true,
        port: options.port ?? DEFAULT_PORT,
        pid: process.pid + 1,
        bootId: crypto.randomUUID(),
        project: options.project,
        canonicalProject: path.resolve(options.project),
      })}\n`,
    );
    process.exit(0);
  }

  const ownerToken = options.ownerTokenEnv ? (process.env[options.ownerTokenEnv] ?? null) : null;
  if (options.ownerTokenEnv && !ownerToken) {
    process.stdout.write(
      `${JSON.stringify({
        ok: false,
        error: {
          code: "missing_owner_token",
          message: `${options.ownerTokenEnv} was named by --owner-token-env but is not set`,
        },
      })}\n`,
    );
    process.exit(1);
  }

  const daemon = createMockDaemon({
    project: options.project,
    ownerToken,
    managedBy: options.managedBy,
    noResolve: options.noResolve,
    eventInterval: options.eventInterval,
    pingInterval: options.pingInterval,
    managerTimeout: options.managerTimeout,
    conflicts: options.conflicts,
    divergence: options.divergence,
    fullScope: options.fullScope,
    badReceipt: options.badReceipt,
    badRemaining: options.badRemaining,
    resolvedElsewhere: options.resolvedElsewhere,
    noPlugin: options.noPlugin,
    onShutdown: () => process.exit(0),
  });

  let port;
  try {
    port = await daemon.listen(options.port);
  } catch (error) {
    process.stdout.write(
      `${JSON.stringify({ ok: false, error: { code: "port_unavailable", message: String(error.message) } })}\n`,
    );
    process.exit(1);
  }

  process.stderr.write(`mock daemon serving ${options.project} on 127.0.0.1:${port}\n`);
  if (!options.silentStart) {
    process.stdout.write(
      `${JSON.stringify({
        ok: true,
        port,
        pid: process.pid,
        bootId: daemon.bootId,
        project: options.project,
        canonicalProject: path.resolve(options.project),
      })}\n`,
    );
  }

  for (const signal of ["SIGINT", "SIGTERM"]) {
    process.on(signal, () => daemon.shutdown(signal.toLowerCase()));
  }
}

// Only run as a program; importing the module (the test does) must not listen.
const invokedDirectly =
  process.argv[1] && import.meta.url === `file://${path.resolve(process.argv[1])}`;
if (invokedDirectly) {
  main(process.argv.slice(2)).catch((error) => {
    process.stdout.write(`${JSON.stringify({ ok: false, error: String(error?.message ?? error) })}\n`);
    process.exit(1);
  });
}

export {
  OPCODE,
  parseArgv,
  EVENT_SCRIPT,
  MAX_ACTIVITY_NAMES,
  MAX_PUSH_IDS,
  REVIEW_STATES,
  SCRIPT_CLASSES,
  SOURCE_LIMIT,
  isOversizedRow,
};
