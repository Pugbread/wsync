#!/usr/bin/env node
// mock-daemon.test.mjs — drives the mock daemon over real sockets.
//
// The point of this file is that the WebSocket half is hand-rolled on both
// sides: the client below speaks RFC 6455 against `node:net` directly, so a
// framing mistake in either implementation fails here rather than in the app.
// Zero dependencies, `node --test`-free (plain assertions) so it runs anywhere
// Node 18 runs.
//
//   node scripts/mock-daemon.test.mjs

import assert from "node:assert/strict";
import crypto from "node:crypto";
import net from "node:net";
import http from "node:http";
import { spawn } from "node:child_process";
import path from "node:path";
import { fileURLToPath } from "node:url";

import {
  acceptKey,
  createFrameReader,
  createMockDaemon,
  encodeFrame,
  isOversizedRow,
  MAX_ACTIVITY_NAMES,
  MAX_PUSH_IDS,
  OPCODE,
  SCRIPT_CLASSES,
  SOURCE_LIMIT,
} from "./mock-daemon.mjs";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const MOCK = path.join(HERE, "mock-daemon.mjs");

let failures = 0;
const only = process.argv[2] ?? null;

async function test(name, body) {
  if (only && !name.includes(only)) return;
  try {
    await body();
    process.stdout.write(`  ok   ${name}\n`);
  } catch (error) {
    failures += 1;
    process.stdout.write(`  FAIL ${name}\n       ${error?.message ?? error}\n`);
    if (error?.stack) {
      process.stdout.write(
        `${error.stack.split("\n").slice(1, 4).map((line) => `       ${line.trim()}`).join("\n")}\n`,
      );
    }
  }
}

// ------------------------------------------------------------- ws client ---

/**
 * A minimal RFC 6455 client: real handshake, masked client frames, unmasked
 * server frames decoded. Deliberately not sharing the mock's reader, so the
 * two implementations check each other.
 */
function connectWebSocket(port, pathname = "/ws") {
  return new Promise((resolve, reject) => {
    const key = crypto.randomBytes(16).toString("base64");
    const socket = net.connect(port, "127.0.0.1");
    const messages = [];
    const waiters = [];
    let handshake = "";
    let upgraded = false;
    let read = null;

    const deliver = (value) => {
      const waiter = waiters.shift();
      if (waiter) waiter.resolve(value);
      else messages.push(value);
    };

    socket.on("error", reject);
    socket.on("connect", () => {
      socket.write(
        [
          `GET ${pathname} HTTP/1.1`,
          "Host: 127.0.0.1",
          "Upgrade: websocket",
          "Connection: Upgrade",
          `Sec-WebSocket-Key: ${key}`,
          "Sec-WebSocket-Version: 13",
          "Origin: http://tauri.localhost",
          "\r\n",
        ].join("\r\n"),
      );
    });

    socket.on("data", (chunk) => {
      if (!upgraded) {
        handshake += chunk.toString("latin1");
        const end = handshake.indexOf("\r\n\r\n");
        if (end === -1) return;
        const head = handshake.slice(0, end);
        const rest = Buffer.from(handshake.slice(end + 4), "latin1");
        upgraded = true;

        const status = head.split("\r\n")[0];
        const accept = /sec-websocket-accept:\s*(\S+)/i.exec(head)?.[1];
        if (!status.includes("101")) return reject(new Error(`handshake failed: ${status}`));
        if (accept !== acceptKey(key)) {
          return reject(new Error(`bad Sec-WebSocket-Accept: ${accept}`));
        }

        read = createFrameReader(
          (frame) => {
            if (frame.opcode === OPCODE.close) return deliver({ closed: true, payload: frame.payload });
            if (frame.opcode === OPCODE.ping) return sendRaw(OPCODE.pong, frame.payload);
            if (frame.opcode !== OPCODE.text) return;
            deliver(JSON.parse(frame.payload.toString("utf8")));
          },
          (code, reason) => reject(new Error(`framing error ${code}: ${reason}`)),
          // A client reads server frames, which must arrive unmasked.
          { requireMask: false },
        );
        resolve(api);
        if (rest.length) read(rest);
        return;
      }
      read?.(chunk);
    });

    socket.on("close", () => deliver({ closed: true, transport: true }));

    /** Client frames are masked, per RFC 6455 §5.3. */
    function sendRaw(opcode, payload) {
      const data = Buffer.isBuffer(payload) ? payload : Buffer.from(String(payload), "utf8");
      const mask = crypto.randomBytes(4);
      const masked = Buffer.from(data);
      for (let index = 0; index < masked.length; index += 1) masked[index] ^= mask[index % 4];

      let header;
      if (masked.length < 126) {
        header = Buffer.alloc(2);
        header[1] = 0x80 | masked.length;
      } else {
        header = Buffer.alloc(4);
        header[1] = 0x80 | 126;
        header.writeUInt16BE(masked.length, 2);
      }
      header[0] = 0x80 | opcode;
      socket.write(Buffer.concat([header, mask, masked]));
    }

    const api = {
      socket,
      send: (value) => sendRaw(OPCODE.text, JSON.stringify(value)),
      sendRaw,
      /** Write an unmasked client frame — a protocol violation, on purpose. */
      sendUnmasked(value) {
        socket.write(encodeFrame(OPCODE.text, JSON.stringify(value)));
      },
      next(timeout = 4000) {
        if (messages.length) return Promise.resolve(messages.shift());
        return new Promise((resolveNext, rejectNext) => {
          const timer = setTimeout(() => {
            const index = waiters.findIndex((waiter) => waiter.timer === timer);
            if (index >= 0) waiters.splice(index, 1);
            rejectNext(new Error("timed out waiting for a frame"));
          }, timeout);
          waiters.push({
            timer,
            resolve: (value) => {
              clearTimeout(timer);
              resolveNext(value);
            },
          });
        });
      },
      async waitFor(predicate, timeout = 6000) {
        const deadline = Date.now() + timeout;
        for (;;) {
          const frame = await this.next(Math.max(50, deadline - Date.now()));
          if (predicate(frame)) return frame;
          if (Date.now() > deadline) throw new Error("gave up waiting for a matching frame");
        }
      },
      close() {
        socket.destroy();
      },
    };
  });
}

// ----------------------------------------------------------- http client ---

function request(port, method, route, body) {
  return new Promise((resolve, reject) => {
    const payload = body === undefined ? null : Buffer.from(JSON.stringify(body), "utf8");
    const call = http.request(
      {
        host: "127.0.0.1",
        port,
        method,
        path: route,
        // No connection pooling: successive daemons in this file land on the
        // same scanned port, and a pooled socket from a shut-down daemon would
        // surface as a spurious ECONNRESET.
        agent: false,
        headers: payload
          ? { "content-type": "application/json", "content-length": payload.length }
          : {},
      },
      (response) => {
        const chunks = [];
        response.on("data", (chunk) => chunks.push(chunk));
        response.on("end", () => {
          const text = Buffer.concat(chunks).toString("utf8");
          let parsed = null;
          try {
            parsed = text ? JSON.parse(text) : null;
          } catch {
            parsed = null;
          }
          resolve({ status: response.statusCode, body: parsed, text });
        });
      },
    );
    call.on("error", reject);
    if (payload) call.write(payload);
    call.end();
  });
}

// ------------------------------------------------------------------ tests ---

async function withDaemon(options, body) {
  const daemon = createMockDaemon({ eventInterval: 60, pingInterval: 80, ...options });
  const port = await daemon.listen(null);
  try {
    await body(daemon, port);
  } finally {
    await new Promise((resolve) => {
      daemon.server.close(resolve);
      daemon.shutdown("test-end");
      setTimeout(resolve, 200);
    });
  }
}

process.stdout.write("mock daemon\n");

await test("GET /hello answers the Design 5.2 identity", () =>
  withDaemon({ project: "/tmp/demo", managedBy: "desktop" }, async (daemon, port) => {
    const { status, body } = await request(port, "GET", "/hello");
    assert.equal(status, 200);
    for (const field of [
      "name",
      "version",
      // Settings → About renders the daemon's build identity from these.
      "commit",
      "dirty",
      "protocol",
      "project",
      "canonicalProject",
      "gameId",
      "placeIds",
      "bootId",
      "pid",
      "port",
      "managedBy",
      "projectInit",
    ]) {
      assert.ok(field in body, `/hello is missing ${field}`);
    }
    assert.equal(body.protocol, 1);
    assert.equal(body.port, port);
    assert.equal(body.bootId, daemon.bootId);
    assert.equal(body.managedBy, "desktop");
  }));

await test("GET /resolve lists conflicts, or 404s on an older engine", async () => {
  await withDaemon({}, async (_daemon, port) => {
    const { status, body } = await request(port, "GET", "/resolve");
    assert.equal(status, 200);
    assert.deepEqual(body, { conflicts: [], total: 0, partial: false });
  });
  await withDaemon({ noResolve: true }, async (_daemon, port) => {
    const { status } = await request(port, "GET", "/resolve");
    assert.equal(status, 404);
  });
});

await test("GET /resolve carries both conflict kinds in the pinned shape", () =>
  withDaemon({ conflicts: 5 }, async (_daemon, port) => {
    const { status, body } = await request(port, "GET", "/resolve");
    assert.equal(status, 200);
    assert.equal(body.conflicts.length, 5);

    for (const conflict of body.conflicts) {
      for (const field of ["id", "path", "instancePath", "class", "kind", "classification", "fs", "studio", "detectedAt"]) {
        assert.ok(field in conflict, `a conflict is missing ${field}`);
      }
      assert.ok(["script", "properties"].includes(conflict.kind));
      assert.ok(
        ["both-edited", "fs-deleted-studio-edited", "studio-deleted-fs-edited"].includes(
          conflict.classification,
        ),
      );
      assert.equal(typeof conflict.fs.present, "boolean");
      assert.equal(typeof conflict.studio.present, "boolean");
    }

    // A script conflict carries text; a property conflict carries tagged maps.
    const script = body.conflicts.find((entry) => entry.kind === "script");
    assert.equal(typeof script.fs.source, "string");
    const properties = body.conflicts.find((entry) => entry.kind === "properties");
    assert.equal(typeof properties.fs.fsProps, "object");
    assert.equal(typeof properties.studio.studioProps, "object");

    // One side absent is a real state, not a missing field.
    const deleted = body.conflicts.find((entry) => entry.classification === "fs-deleted-studio-edited");
    assert.equal(deleted.fs.present, false);
    assert.equal(deleted.studio.present, true);

    // A truncated source says so, so the view can label it.
    assert.ok(body.conflicts.some((entry) => entry.fs.truncated === true));
  }));

await test("POST /resolve answers one conflict and 404s an unknown id", () =>
  withDaemon({ conflicts: 3 }, async (_daemon, port) => {
    const before = (await request(port, "GET", "/resolve")).body.conflicts;
    const target = before[1];

    const ok = await request(port, "POST", "/resolve", {
      id: target.id,
      path: target.path,
      keep: "studio",
      choice: "studio",
    });
    assert.equal(ok.status, 200);
    assert.deepEqual(ok.body, { ok: true, resolved: target.id });

    const after = (await request(port, "GET", "/resolve")).body.conflicts;
    assert.equal(after.length, before.length - 1);
    assert.ok(!after.some((entry) => entry.id === target.id));

    // The same id twice is the "someone else answered it first" path.
    const repeat = await request(port, "POST", "/resolve", { id: target.id, keep: "studio", choice: "studio" });
    assert.equal(repeat.status, 404);

    // `keep` and `choice` carry the same value; disagreeing is a client bug.
    const mismatch = await request(port, "POST", "/resolve", {
      id: before[0].id,
      keep: "local",
      choice: "studio",
    });
    assert.equal(mismatch.status, 400);
    assert.equal((await request(port, "POST", "/resolve", { id: before[0].id, keep: "sideways" })).status, 400);
  }));

// --- Design 7.0: the passive disk review -----------------------------------

/** Everything still pending in the review, as one page. */
async function reviewRows(port, reviewId, limit = 1024) {
  const { body } = await request(
    port,
    "GET",
    `/review/details?reviewId=${reviewId}&cursor=${cursorOf(0)}&limit=${limit}`,
  );
  return body.items;
}

/** Named so the intent of a literal 0 cursor is not lost in a template. */
function cursorOf(value) {
  return value;
}

await test("code scope raises a disk review and no choice at all", () =>
  withDaemon({ divergence: 40 }, async (_daemon, port) => {
    // Design 7.0: connect already applied Studio → disk, so there is nothing
    // to decide — only a list of what disk still holds.
    assert.deepEqual((await request(port, "GET", "/choice")).body, { pending: false });

    const { status, body } = await request(port, "GET", "/review");
    assert.equal(status, 200);
    assert.equal(body.pending, true);
    assert.equal(typeof body.reviewId, "string");
    assert.equal(body.stats.total, 40);
    assert.equal(
      body.stats.diskOnly + body.stats.differs,
      40,
      "the two states must partition the review",
    );
    // Counts only: the path list is paged, never broadcast.
    assert.ok(!("items" in body) && !("paths" in body));
  }));

await test("--full-scope keeps the old choice flow and raises no review", () =>
  withDaemon({ divergence: 40, fullScope: true }, async (_daemon, port) => {
    assert.equal((await request(port, "GET", "/choice")).body.pending, true);
    assert.deepEqual((await request(port, "GET", "/review")).body, { pending: false });
    assert.equal((await request(port, "GET", "/review/details?cursor=0&limit=10")).status, 404);
  }));

await test("GET /review/details pages with increasing ids and a full path", () =>
  withDaemon({ divergence: 1500 }, async (_daemon, port) => {
    const { reviewId } = (await request(port, "GET", "/review")).body;

    let cursor = 0;
    let pages = 0;
    let previous = -1;
    const seen = new Set();
    for (;;) {
      const { status, body } = await request(
        port,
        "GET",
        `/review/details?reviewId=${reviewId}&cursor=${cursor}&limit=512`,
      );
      assert.equal(status, 200);
      assert.equal(body.reviewId, reviewId);
      assert.equal(body.totalCount, 1500);
      pages += 1;

      for (const item of body.items) {
        assert.ok(Number.isInteger(item.id) && item.id > previous, "ids must increase across the set");
        previous = item.id;
        assert.ok(!seen.has(item.id), `id ${item.id} arrived twice`);
        seen.add(item.id);
        assert.ok(["disk-only", "differs"].includes(item.state));
        // A review entry *is* a file on disk, so the path is never null; the
        // instance is what may not exist yet.
        assert.equal(typeof item.path, "string");
        assert.ok(item.instancePath === null || typeof item.instancePath === "string");
      }

      if (body.nextCursor === undefined) {
        assert.equal(cursor + body.items.length, body.totalCount, "the last page must reach the end");
        break;
      }
      assert.equal(body.nextCursor, cursor + body.items.length);
      cursor = body.nextCursor;
    }
    assert.equal(seen.size, 1500);
    assert.equal(pages, 3);

    // A disk-only file Studio has never seen has no instance path.
    assert.ok((await reviewRows(port, reviewId, 8)).some((item) => item.instancePath === null));
  }));

await test("GET /review/details refuses a stale reviewId and an oversized page", () =>
  withDaemon({ divergence: 40 }, async (_daemon, port) => {
    const { reviewId } = (await request(port, "GET", "/review")).body;
    assert.equal((await request(port, "GET", "/review/details?reviewId=nope&cursor=0&limit=10")).status, 409);
    assert.equal(
      (await request(port, "GET", `/review/details?reviewId=${reviewId}&cursor=0&limit=2048`)).status,
      400,
    );
    assert.equal(
      (await request(port, "GET", `/review/details?reviewId=${reviewId}&cursor=-1&limit=10`)).status,
      400,
    );
  }));

await test("POST /review/push moves a subset and reports what is left", () =>
  withDaemon({ divergence: 40 }, async (_daemon, port) => {
    const { reviewId } = (await request(port, "GET", "/review")).body;
    const rows = await reviewRows(port, reviewId);
    const picked = rows.slice(0, 12).map((row) => row.id);

    const first = await request(port, "POST", "/review/push", { reviewId, ids: picked });
    assert.equal(first.status, 200);
    assert.deepEqual(first.body, { ok: true, pushed: 12, remaining: 28 });

    // Repeatable: the same ids again push nothing and change nothing, which is
    // what makes a retried chunk safe.
    const again = await request(port, "POST", "/review/push", { reviewId, ids: picked });
    assert.deepEqual(again.body, { ok: true, pushed: 0, remaining: 28 });

    // What is left keeps its own ids — the set shrank, it was not renumbered.
    const left = await reviewRows(port, reviewId);
    assert.equal(left.length, 28);
    assert.deepEqual(
      left.map((row) => row.id),
      rows.slice(12).map((row) => row.id),
    );
    assert.equal((await request(port, "GET", "/review")).body.stats.total, 28);
  }));

await test("POST /review/push mode:all empties the review", () =>
  withDaemon({ divergence: 40 }, async (_daemon, port) => {
    const { reviewId } = (await request(port, "GET", "/review")).body;
    const rows = await reviewRows(port, reviewId);
    await request(port, "POST", "/review/push", { reviewId, ids: rows.slice(0, 5).map((row) => row.id) });

    const all = await request(port, "POST", "/review/push", { reviewId, mode: "all" });
    assert.deepEqual(all.body, { ok: true, pushed: 35, remaining: 0 });

    // An emptied review is gone: nothing pending, and its id is stale.
    assert.deepEqual((await request(port, "GET", "/review")).body, { pending: false });
    assert.equal((await request(port, "POST", "/review/push", { reviewId, mode: "all" })).status, 404);
    assert.equal((await request(port, "GET", `/review/details?reviewId=${reviewId}`)).status, 404);
  }));

await test("a push is refused when it is not a shape the contract names", () =>
  withDaemon({ divergence: 40 }, async (_daemon, port) => {
    const { reviewId } = (await request(port, "GET", "/review")).body;
    const cases = [
      [{ reviewId }, 400],
      [{ reviewId, ids: [] }, 400],
      [{ reviewId, ids: Array.from({ length: MAX_PUSH_IDS + 1 }, (_, id) => id) }, 400],
      [{ reviewId, ids: [0, 999_999] }, 400],
      [{ reviewId, ids: [0, "3"] }, 400],
      [{ reviewId, mode: "all", ids: [0] }, 400],
      [{ reviewId: "rv_nope", ids: [0] }, 404],
      [{ ids: [0] }, 404],
    ];
    for (const [body, status] of cases) {
      const answer = await request(port, "POST", "/review/push", body);
      assert.equal(answer.status, status, JSON.stringify(body).slice(0, 80));
      assert.equal(answer.body.ok, false);
    }
    // Nothing was pushed by any of them.
    assert.equal((await request(port, "GET", "/review")).body.stats.total, 40);
  }));

await test("POST /review/dismiss drops the review without pushing anything", () =>
  withDaemon({ divergence: 40 }, async (_daemon, port) => {
    const { reviewId } = (await request(port, "GET", "/review")).body;
    assert.equal((await request(port, "POST", "/review/dismiss", { reviewId: "rv_nope" })).status, 404);

    const answer = await request(port, "POST", "/review/dismiss", { reviewId });
    assert.equal(answer.status, 200);
    assert.equal(answer.body.ok, true);
    assert.deepEqual((await request(port, "GET", "/review")).body, { pending: false });
    // Dismissing twice is a 404, not a second success.
    assert.equal((await request(port, "POST", "/review/dismiss", { reviewId })).status, 404);
  }));

await test("--bad-remaining answers a push with a remaining that cannot be true", () =>
  withDaemon({ divergence: 40, badRemaining: true }, async (_daemon, port) => {
    const { reviewId } = (await request(port, "GET", "/review")).body;
    const rows = await reviewRows(port, reviewId);
    const { body } = await request(port, "POST", "/review/push", {
      reviewId,
      ids: rows.slice(0, 4).map((row) => row.id),
    });
    // 36 are genuinely left; the switch reports more than were there before,
    // which is exactly what the desktop's receipt check has to catch.
    assert.equal(body.pushed, 4);
    assert.ok(body.remaining > 40, `remaining ${body.remaining} should be impossible`);
  }));

await test("a push with no plugin answers 503, moves nothing, and recovers when Studio connects", () =>
  withDaemon({ divergence: 12, noPlugin: true }, async (daemon, port) => {
    // Connect the feed first: a connect rebuilds the pending review, so the
    // reviewId has to be read after it, the way the desktop reads it.
    const socket = await connectWebSocket(port);
    socket.send({ type: "hello", protocol: 1 });
    await socket.waitFor((frame) => frame.type === "hello");

    const { reviewId } = (await request(port, "GET", "/review")).body;
    const rows = await reviewRows(port, reviewId);
    const ids = rows.slice(0, 3).map((row) => row.id);

    // The retryable refusal: nothing moved, the review is exactly as it was.
    for (const body of [{ reviewId, ids }, { reviewId, mode: "all" }]) {
      const refused = await request(port, "POST", "/review/push", body);
      assert.equal(refused.status, 503);
      assert.equal(refused.body.ok, false);
    }
    assert.equal((await request(port, "GET", "/review")).body.stats.total, 12);

    // Studio connects: the daemon announces it, and the same push now lands.
    daemon.setPluginConnected(true);
    const announced = await socket.waitFor(
      (frame) => frame.topic === "plugin-status" && frame.connected === true,
    );
    assert.equal(typeof announced.clientName, "string");

    const accepted = await request(port, "POST", "/review/push", { reviewId, ids });
    assert.equal(accepted.status, 200);
    assert.equal(accepted.body.pushed, 3);
    assert.equal(accepted.body.remaining, 9);
    socket.close();
  }));

await test("a dismiss needs no plugin — skipping works before Studio ever connects", () =>
  withDaemon({ divergence: 12, noPlugin: true }, async (_daemon, port) => {
    const { reviewId } = (await request(port, "GET", "/review")).body;
    const answer = await request(port, "POST", "/review/dismiss", { reviewId });
    assert.equal(answer.status, 200);
    assert.equal(answer.body.ok, true);
    assert.deepEqual((await request(port, "GET", "/review")).body, { pending: false });
  }));

await test("dropClients severs sockets like a network drop, and the reconnect supersedes", () =>
  withDaemon({ divergence: 8 }, async (daemon, port) => {
    const first = await connectWebSocket(port);
    first.send({ type: "hello", protocol: 1 });
    await first.waitFor((frame) => frame.type === "hello");
    const announced = await first.waitFor((frame) => frame.topic === "disk-review");

    daemon.dropClients();
    // A drop, not a stop: the socket just dies, with no shutdown frame first.
    const last = await first.waitFor((frame) => frame.closed === true);
    assert.notEqual(last.type, "shutdown");
    assert.equal(daemon.clientCount, 0);

    // The daemon is still there, and the reconnect re-runs the connect flow —
    // which under a pending review rebuilds it under a new id.
    const second = await connectWebSocket(port);
    second.send({ type: "hello", protocol: 1 });
    await second.waitFor((frame) => frame.type === "hello");
    const replaced = await second.waitFor((frame) => frame.topic === "disk-review");
    assert.notEqual(replaced.reviewId, announced.reviewId);
    second.close();
  }));

await test("disk-review is announced on connect, and a reconnect replaces it", () =>
  withDaemon({ divergence: 40 }, async (_daemon, port) => {
    const first = await connectWebSocket(port);
    first.send({ type: "hello", protocol: 1 });
    await first.waitFor((frame) => frame.type === "hello");
    const announced = await first.waitFor((frame) => frame.topic === "disk-review");
    assert.equal(typeof announced.reviewId, "string");
    assert.equal(announced.total, 40);
    assert.equal(announced.diskOnly + announced.differs, 40);
    assert.equal((await request(port, "GET", "/review")).body.reviewId, announced.reviewId);
    first.close();

    // Design 7.0: a connect *is* a sync, so the review it raises replaces the
    // one before it — the old ids describe an apply that has happened again.
    const second = await connectWebSocket(port);
    second.send({ type: "hello", protocol: 1 });
    await second.waitFor((frame) => frame.type === "hello");
    const replaced = await second.waitFor((frame) => frame.topic === "disk-review");
    assert.notEqual(replaced.reviewId, announced.reviewId);
    assert.equal((await request(port, "GET", "/review")).body.reviewId, replaced.reviewId);
    // The superseded id is refused everywhere.
    assert.equal(
      (await request(port, "GET", `/review/details?reviewId=${announced.reviewId}`)).status,
      409,
    );
    assert.equal(
      (await request(port, "POST", "/review/push", { reviewId: announced.reviewId, mode: "all" })).status,
      404,
    );
    second.close();
  }));

await test("no review is announced when there is nothing to review", () =>
  withDaemon({}, async (_daemon, port) => {
    assert.deepEqual((await request(port, "GET", "/review")).body, { pending: false });
    assert.equal((await request(port, "POST", "/review/push", { reviewId: "rv_x", mode: "all" })).status, 404);
    assert.equal((await request(port, "POST", "/review/dismiss", { reviewId: "rv_x" })).status, 404);
  }));

await test("GET /choice reports aggregate stats and nothing else", () =>
  withDaemon({ divergence: 1500, fullScope: true }, async (_daemon, port) => {
    const { status, body } = await request(port, "GET", "/choice");
    assert.equal(status, 200);
    assert.equal(body.pending, true);
    assert.equal(typeof body.choiceId, "string");
    for (const field of ["total", "studioCount", "diskCount", "onlyOnDisk", "differs", "missingOnDisk"]) {
      assert.equal(typeof body.stats[field], "number", `stats.${field}`);
    }
    assert.equal(body.stats.total, 1500);
    assert.equal(
      body.stats.onlyOnDisk + body.stats.differs + body.stats.missingOnDisk,
      1500,
      "the three groups must partition the set",
    );
    // Design 7.2: the broadcast is stats-only — never the path list.
    assert.ok(!("items" in body) && !("paths" in body));
  }));

await test("GET /choice is {pending:false} with no divergence", () =>
  withDaemon({}, async (_daemon, port) => {
    assert.deepEqual((await request(port, "GET", "/choice")).body, { pending: false });
    assert.equal((await request(port, "GET", "/choice/details?cursor=0&limit=10")).status, 404);
  }));

await test("GET /choice/details pages with dense sequential ids", () =>
  withDaemon({ divergence: 1500, fullScope: true }, async (_daemon, port) => {
    const { choiceId } = (await request(port, "GET", "/choice")).body;

    let cursor = 0;
    let pages = 0;
    const seen = new Set();
    for (;;) {
      const { status, body } = await request(
        port,
        "GET",
        `/choice/details?choiceId=${choiceId}&cursor=${cursor}&limit=512`,
      );
      assert.equal(status, 200);
      assert.equal(body.choiceId, choiceId);
      assert.equal(body.totalCount, 1500);
      pages += 1;

      body.items.forEach((item, index) => {
        assert.equal(item.id, cursor + index, "ids must be dense and sequential from the cursor");
        assert.ok(!seen.has(item.id), `id ${item.id} arrived twice`);
        seen.add(item.id);
        assert.ok(["only-on-disk", "differs", "missing-on-disk"].includes(item.state));
        assert.equal(typeof item.instancePath, "string");
        assert.ok(item.path === null || typeof item.path === "string");
      });

      if (body.nextCursor === undefined) {
        assert.equal(cursor + body.items.length, body.totalCount, "the last page must reach the end");
        break;
      }
      assert.equal(body.nextCursor, cursor + body.items.length);
      cursor = body.nextCursor;
    }
    assert.equal(seen.size, 1500);
    assert.equal(pages, 3);

    // A Studio-only entry with no predictable file path is a real answer.
    assert.ok(
      (await request(port, "GET", `/choice/details?choiceId=${choiceId}&cursor=0&limit=8`)).body.items.some(
        (item) => item.path === null,
      ),
    );
  }));

await test("GET /choice/details refuses a stale choiceId and an oversized page", () =>
  withDaemon({ divergence: 40, fullScope: true }, async (_daemon, port) => {
    const { choiceId } = (await request(port, "GET", "/choice")).body;
    assert.equal((await request(port, "GET", "/choice/details?choiceId=nope&cursor=0&limit=10")).status, 409);
    assert.equal(
      (await request(port, "GET", `/choice/details?choiceId=${choiceId}&cursor=0&limit=2048`)).status,
      400,
      "the contract caps a page at 1024",
    );
    assert.equal(
      (await request(port, "GET", `/choice/details?choiceId=${choiceId}&cursor=-1&limit=10`)).status,
      400,
    );
  }));

// --- GET /choice/source: the staging list's inline diff ---------------------

/** Every row of the frozen set, so a test can pick one of a given shape. */
async function divergenceRows(port, choiceId, limit = 64) {
  const { body } = await request(
    port,
    "GET",
    `/choice/details?choiceId=${choiceId}&cursor=0&limit=${limit}`,
  );
  return body.items;
}

await test("GET /choice/source answers a script row with both sides", () =>
  withDaemon({ divergence: 40, fullScope: true }, async (_daemon, port) => {
    const { choiceId } = (await request(port, "GET", "/choice")).body;
    const rows = await divergenceRows(port, choiceId);
    const row = rows.find(
      (entry) => entry.state === "differs" && SCRIPT_CLASSES.has(entry.class) && !isOversizedRow(entry.id),
    );
    assert.ok(row, "the fixture set must contain a plain script row that differs");

    const { status, body } = await request(
      port,
      "GET",
      `/choice/source?choiceId=${choiceId}&id=${row.id}`,
    );
    assert.equal(status, 200);
    for (const field of ["id", "path", "instancePath", "state", "disk", "studio"]) {
      assert.ok(field in body, `/choice/source is missing ${field}`);
    }
    assert.equal(body.id, row.id);
    assert.equal(body.state, row.state);
    assert.equal(body.path, row.path);

    // Both sides present, both real text, and genuinely different — a "differs"
    // row whose two sides are identical would make the whole affordance a lie.
    assert.equal(body.disk.present, true);
    assert.equal(body.studio.present, true);
    assert.equal(typeof body.disk.source, "string");
    assert.equal(typeof body.studio.source, "string");
    assert.notEqual(body.disk.source, body.studio.source);
    assert.ok(body.disk.source.includes("--!strict"), "the source should look like Luau");
    assert.equal(body.disk.truncated, false);
    assert.equal(body.studio.truncated, false);
  }));

await test("GET /choice/source clips an oversized pair to 256 KiB and says so", () =>
  withDaemon({ divergence: 40, fullScope: true }, async (_daemon, port) => {
    const { choiceId } = (await request(port, "GET", "/choice")).body;
    const rows = await divergenceRows(port, choiceId);
    const row = rows.find((entry) => isOversizedRow(entry.id) && SCRIPT_CLASSES.has(entry.class));
    assert.ok(row, "an oversized script row must be reachable from the first page");

    const { status, body } = await request(
      port,
      "GET",
      `/choice/source?choiceId=${choiceId}&id=${row.id}`,
    );
    assert.equal(status, 200);
    for (const side of ["disk", "studio"]) {
      assert.equal(body[side].truncated, true, `${side} should report the cut`);
      assert.equal(
        Buffer.byteLength(body[side].source, "utf8"),
        SOURCE_LIMIT,
        `${side} must be clipped to exactly the ceiling`,
      );
    }
  }));

await test("GET /choice/source refuses a property row with 400", () =>
  withDaemon({ divergence: 40, fullScope: true }, async (_daemon, port) => {
    const { choiceId } = (await request(port, "GET", "/choice")).body;
    const rows = await divergenceRows(port, choiceId);
    const row = rows.find((entry) => !SCRIPT_CLASSES.has(entry.class));
    assert.ok(row, "the fixture set must contain a non-script row");

    const { status, body } = await request(
      port,
      "GET",
      `/choice/source?choiceId=${choiceId}&id=${row.id}`,
    );
    assert.equal(status, 400);
    assert.equal(typeof body.error, "string");
    assert.ok(!("disk" in body) && !("studio" in body), "a refusal carries no sources");
  }));

await test("GET /choice/source 404s an unknown row, a stale set, and no set at all", async () => {
  await withDaemon({ divergence: 40, fullScope: true }, async (_daemon, port) => {
    const { choiceId } = (await request(port, "GET", "/choice")).body;
    for (const id of [40, 99999, -1, "abc"]) {
      assert.equal(
        (await request(port, "GET", `/choice/source?choiceId=${choiceId}&id=${id}`)).status,
        404,
        `id ${id} should be unknown`,
      );
    }
    // A stale set has no rows to read: the client takes the supersede path.
    assert.equal(
      (await request(port, "GET", "/choice/source?choiceId=ch_gone&id=0")).status,
      404,
    );
  });
  await withDaemon({}, async (_daemon, port) => {
    assert.equal((await request(port, "GET", "/choice/source?choiceId=x&id=0")).status, 404);
  });
});

await test("--no-plugin answers 503 for a diffable row, but still 404s and 400s first", () =>
  withDaemon({ divergence: 40, fullScope: true, noPlugin: true }, async (_daemon, port) => {
    const { choiceId } = (await request(port, "GET", "/choice")).body;
    const rows = await divergenceRows(port, choiceId);
    const script = rows.find((entry) => SCRIPT_CLASSES.has(entry.class));
    const property = rows.find((entry) => !SCRIPT_CLASSES.has(entry.class));

    const unavailable = await request(
      port,
      "GET",
      `/choice/source?choiceId=${choiceId}&id=${script.id}`,
    );
    assert.equal(unavailable.status, 503);
    assert.equal(typeof unavailable.body.error, "string");

    // Whether a row exists and whether it is a script are decidable without
    // Studio, so those answers do not change when the plugin is away.
    assert.equal(
      (await request(port, "GET", `/choice/source?choiceId=${choiceId}&id=${property.id}`)).status,
      400,
    );
    assert.equal(
      (await request(port, "GET", `/choice/source?choiceId=${choiceId}&id=9999`)).status,
      404,
    );
  }));

await test("POST /choice/selection receipts follow the chunk sequence", () =>
  withDaemon({ divergence: 40, fullScope: true }, async (_daemon, port) => {
    const { choiceId } = (await request(port, "GET", "/choice")).body;
    const submissionId = "sub-1";

    const first = await request(port, "POST", "/choice/selection", {
      choiceId,
      submissionId,
      chunkIndex: 0,
      finalChunk: false,
      restart: true,
      ids: [0, 1, 2],
    });
    assert.equal(first.status, 200);
    assert.deepEqual(first.body, {
      ok: true,
      acceptedChunk: 0,
      nextChunk: 1,
      selectedCount: 3,
      committed: false,
    });

    // Out of order is refused rather than silently accepted.
    const skipped = await request(port, "POST", "/choice/selection", {
      choiceId,
      submissionId,
      chunkIndex: 2,
      finalChunk: true,
      ids: [3],
    });
    assert.equal(skipped.status, 409);

    const final = await request(port, "POST", "/choice/selection", {
      choiceId,
      submissionId,
      chunkIndex: 1,
      finalChunk: true,
      ids: [3, 4],
    });
    assert.deepEqual(final.body, {
      ok: true,
      acceptedChunk: 1,
      nextChunk: 2,
      selectedCount: 5,
      committed: true,
    });

    // Committed means committed: the same submission cannot be extended.
    assert.equal(
      (
        await request(port, "POST", "/choice/selection", {
          choiceId,
          submissionId,
          chunkIndex: 2,
          finalChunk: true,
          ids: [5],
        })
      ).status,
      409,
    );
  }));

await test("POST /choice/selection validates the chunk envelope", () =>
  withDaemon({ divergence: 40, fullScope: true }, async (_daemon, port) => {
    const { choiceId } = (await request(port, "GET", "/choice")).body;
    const base = { choiceId, submissionId: "sub-2", chunkIndex: 0, finalChunk: true };

    // Chunk 0 without `restart` would inherit an abandoned submission's ids.
    assert.equal((await request(port, "POST", "/choice/selection", { ...base, ids: [1] })).status, 400);
    // An id outside the frozen set can only be a client bug.
    assert.equal(
      (await request(port, "POST", "/choice/selection", { ...base, restart: true, ids: [999] })).status,
      400,
    );
    assert.equal(
      (await request(port, "POST", "/choice/selection", { ...base, restart: true, ids: new Array(2049).fill(0) }))
        .status,
      400,
    );
    assert.equal(
      (await request(port, "POST", "/choice/selection", { ...base, restart: true, submissionId: "", ids: [1] }))
        .status,
      400,
    );
  }));

await test("--bad-receipt corrupts exactly the chunk it names", () =>
  withDaemon({ divergence: 40, fullScope: true, badReceipt: 1 }, async (_daemon, port) => {
    const { choiceId } = (await request(port, "GET", "/choice")).body;
    const submissionId = "sub-3";
    const first = await request(port, "POST", "/choice/selection", {
      choiceId, submissionId, chunkIndex: 0, finalChunk: false, restart: true, ids: [0, 1],
    });
    assert.equal(first.body.selectedCount, 2, "chunk 0 is honest");

    const second = await request(port, "POST", "/choice/selection", {
      choiceId, submissionId, chunkIndex: 1, finalChunk: true, ids: [2, 3],
    });
    assert.equal(second.body.selectedCount, 5, "chunk 1's count is off by one, on purpose");
    assert.equal(second.status, 200, "a corrupt receipt is a 200 — that is what makes it dangerous");
  }));

await test("POST /choice: studio is recorded but not applied, disk needs a selection", () =>
  withDaemon({ divergence: 40, fullScope: true }, async (_daemon, port) => {
    const { choiceId } = (await request(port, "GET", "/choice")).body;

    // Keep Disk with neither `mode:"all"` nor a committed selection would be
    // the daemon guessing at what to pull.
    assert.equal((await request(port, "POST", "/choice", { choiceId, choice: "disk" })).status, 409);
    assert.equal((await request(port, "POST", "/choice", { choiceId, choice: "sideways" })).status, 400);
    assert.equal((await request(port, "POST", "/choice", { choiceId, choice: "disk", mode: "some" })).status, 400);

    const studio = await request(port, "POST", "/choice", { choiceId, choice: "studio" });
    assert.equal(studio.status, 200);
    assert.deepEqual(studio.body, { ok: true, applied: false, pendingApplication: true });

    // Answered once, gone: everything after is the 409 the modal closes on.
    assert.deepEqual((await request(port, "GET", "/choice")).body, { pending: false });
    const again = await request(port, "POST", "/choice", { choiceId, choice: "disk", mode: "all" });
    assert.equal(again.status, 409);
    assert.equal(again.body.error, "resolved");
  }));

await test("POST /choice: a committed selection makes disk applicable", () =>
  withDaemon({ divergence: 40, fullScope: true }, async (_daemon, port) => {
    const { choiceId } = (await request(port, "GET", "/choice")).body;
    await request(port, "POST", "/choice/selection", {
      choiceId, submissionId: "sub-4", chunkIndex: 0, finalChunk: true, restart: true, ids: [1, 2, 3],
    });
    const applied = await request(port, "POST", "/choice", { choiceId, choice: "disk" });
    assert.deepEqual(applied.body, { ok: true, applied: true });
  }));

await test("--resolved-elsewhere refuses every write with the pinned 409", () =>
  withDaemon({ divergence: 40, fullScope: true, resolvedElsewhere: true }, async (_daemon, port) => {
    // The read still reports the choice: the race is between reading and
    // writing, which is exactly the case the modal has to survive.
    const { choiceId } = (await request(port, "GET", "/choice")).body;
    assert.equal(typeof choiceId, "string");

    const choice = await request(port, "POST", "/choice", { choiceId, choice: "studio" });
    assert.equal(choice.status, 409);
    assert.deepEqual(choice.body, { ok: false, error: "resolved" });

    const selection = await request(port, "POST", "/choice/selection", {
      choiceId, submissionId: "sub-5", chunkIndex: 0, finalChunk: true, restart: true, ids: [1],
    });
    assert.equal(selection.status, 409);
    assert.deepEqual(selection.body, { ok: false, error: "resolved" });
  }));

await test("choice-needed reaches a client that connected late", () =>
  withDaemon({ divergence: 40, fullScope: true }, async (_daemon, port) => {
    const socket = await connectWebSocket(port);
    socket.send({ type: "hello", clientId: 11, role: "app", protocol: 1, name: "test" });
    await socket.waitFor((frame) => frame.type === "hello");
    const needed = await socket.waitFor((frame) => frame.topic === "choice-needed");
    assert.equal(typeof needed.choiceId, "string");
    assert.equal(needed.total, 40);
    assert.equal(typeof needed.onlyOnDisk, "number");
    socket.close();
  }));

await test("choice-made is broadcast when the decision lands", () =>
  withDaemon({ divergence: 40, fullScope: true }, async (_daemon, port) => {
    const socket = await connectWebSocket(port);
    socket.send({ type: "hello", clientId: 12, role: "app", protocol: 1, name: "test" });
    const needed = await socket.waitFor((frame) => frame.topic === "choice-needed");

    await request(port, "POST", "/choice", { choiceId: needed.choiceId, choice: "cancel" });
    const made = await socket.waitFor((frame) => frame.topic === "choice-made");
    assert.equal(made.choiceId, needed.choiceId);
    assert.equal(made.choice, "cancel");
    socket.close();
  }));

await test("conflict events name a conflict /resolve actually lists", () =>
  withDaemon({ conflicts: 2 }, async (_daemon, port) => {
    const socket = await connectWebSocket(port);
    socket.send({ type: "hello", clientId: 13, role: "app", protocol: 1, name: "test" });
    const event = await socket.waitFor((frame) => frame.topic === "conflict");
    for (const field of ["id", "path", "instancePath", "classification"]) {
      assert.ok(field in event, `the conflict event is missing ${field}`);
    }
    const listed = (await request(port, "GET", "/resolve")).body.conflicts;
    assert.ok(listed.some((entry) => entry.id === event.id));
    socket.close();
  }));

await test("manager routes require the owner token", () =>
  withDaemon({ ownerToken: "sekret", managedBy: "desktop" }, async (_daemon, port) => {
    assert.equal((await request(port, "POST", "/manager-heartbeat", { token: "nope" })).status, 401);
    assert.equal((await request(port, "POST", "/manager-heartbeat", { token: "sekret" })).status, 204);
  }));

await test("/stop needs the exact boot id and token", () =>
  withDaemon({ ownerToken: "sekret" }, async (daemon, port) => {
    assert.equal((await request(port, "POST", "/stop", { bootId: daemon.bootId, token: "x" })).status, 401);
    const mismatch = await request(port, "POST", "/stop", { bootId: "not-mine", token: "sekret" });
    assert.equal(mismatch.status, 409);
    const accepted = await request(port, "POST", "/stop", { bootId: daemon.bootId, token: "sekret" });
    assert.equal(accepted.status, 200);
  }));

await test("the WebSocket handshake computes a correct accept key", () =>
  withDaemon({}, async (_daemon, port) => {
    // connectWebSocket verifies Sec-WebSocket-Accept itself and rejects on a
    // mismatch, so reaching this line is the assertion.
    const socket = await connectWebSocket(port);
    socket.close();
  }));

await test("hello is answered with the server hello (Design 5.3)", () =>
  withDaemon({}, async (_daemon, port) => {
    const socket = await connectWebSocket(port);
    socket.send({
      type: "hello",
      clientId: crypto.randomInt(0, 2 ** 32),
      role: "app",
      protocol: 1,
      name: "WSync Desktop",
    });
    const hello = await socket.next();
    assert.equal(hello.type, "hello");
    for (const field of ["name", "version", "gameId", "placeIds", "rootRefs"]) {
      assert.ok(field in hello, `server hello is missing ${field}`);
    }
    socket.close();
  }));

await test("event frames are flat, tagged, and topic-labelled", () =>
  withDaemon({}, async (_daemon, port) => {
    const socket = await connectWebSocket(port);
    socket.send({ type: "hello", clientId: 1, role: "app", protocol: 1, name: "test" });
    await socket.waitFor((frame) => frame.type === "hello");

    const event = await socket.waitFor((frame) => frame.type === "event");
    assert.ok(["sync-activity", "plugin-status"].includes(event.topic), `topic ${event.topic}`);
    // Flat envelope: payload fields sit inline beside `type`, not nested.
    assert.equal(typeof event.at, "number");
    assert.ok(!("payload" in event), "frames must be flat (Design 5.3)");
    socket.close();
  }));

await test("sync-activity carries direction, counts and bounded names", () =>
  withDaemon({}, async (_daemon, port) => {
    const socket = await connectWebSocket(port);
    socket.send({ type: "hello", clientId: 21, role: "app", protocol: 1, name: "test" });
    await socket.waitFor((frame) => frame.type === "hello");

    // Every sync-activity frame in the script, not just the first: the
    // last-edited store is fed by all of them, so one without `names` would be
    // a silent hole in the history.
    for (let seen = 0; seen < 4; seen += 1) {
      const event = await socket.waitFor((frame) => frame.topic === "sync-activity");
      assert.equal(typeof event.direction, "string", "direction is pinned");
      assert.ok(event.counts && typeof event.counts === "object", "counts is pinned");
      assert.ok(Array.isArray(event.names), "names is pinned");
      assert.ok(
        event.names.length > 0 && event.names.length <= MAX_ACTIVITY_NAMES,
        `names must hold 1…${MAX_ACTIVITY_NAMES} paths, got ${event.names.length}`,
      );
      for (const name of event.names) {
        assert.equal(typeof name, "string");
        assert.ok(name.length > 0 && !name.includes("\n"), `${JSON.stringify(name)} is not a path`);
      }
    }
    socket.close();
  }));

await test("sync-activity names real paths from the pending divergence set", () =>
  withDaemon({ divergence: 40, fullScope: true }, async (_daemon, port) => {
    const { choiceId } = (await request(port, "GET", "/choice")).body;
    const rows = await divergenceRows(port, choiceId);
    const known = new Set(rows.map((entry) => entry.path).filter(Boolean));

    const socket = await connectWebSocket(port);
    socket.send({ type: "hello", clientId: 22, role: "app", protocol: 1, name: "test" });
    await socket.waitFor((frame) => frame.type === "hello");

    // The point of the dynamic slot: the desktop's last-edited store has to end
    // up holding stamps for paths the divergence modal will actually list, or
    // "Recently edited" sorts a set it has never seen.
    const event = await socket.waitFor(
      (frame) => frame.topic === "sync-activity" && frame.names?.some((name) => known.has(name)),
    );
    assert.ok(event.names.every((name) => known.has(name)), "every name must be in the set");
    assert.ok(event.names.length <= MAX_ACTIVITY_NAMES);
    socket.close();
  }));

await test("event-sub narrows the feed to the chosen topics", () =>
  withDaemon({}, async (_daemon, port) => {
    const socket = await connectWebSocket(port);
    socket.send({ type: "hello", clientId: 2, role: "app", protocol: 1, name: "test" });
    await socket.waitFor((frame) => frame.type === "hello");
    socket.send({ type: "event-sub", topics: ["plugin-status"] });

    for (let index = 0; index < 4; index += 1) {
      const event = await socket.waitFor((frame) => frame.type === "event");
      assert.equal(event.topic, "plugin-status");
    }
    socket.close();
  }));

await test("the server pings and accepts a pong", () =>
  withDaemon({}, async (_daemon, port) => {
    const socket = await connectWebSocket(port);
    socket.send({ type: "hello", clientId: 3, role: "app", protocol: 1, name: "test" });
    await socket.waitFor((frame) => frame.type === "hello");
    const ping = await socket.waitFor((frame) => frame.type === "ping");
    assert.equal(typeof ping.at, "number");
    socket.send({ type: "pong" });
    // Still alive after the pong: the next event still arrives.
    await socket.waitFor((frame) => frame.type === "event");
    socket.close();
  }));

await test("a protocol mismatch is a non-retryable shutdown", () =>
  withDaemon({}, async (_daemon, port) => {
    const socket = await connectWebSocket(port);
    socket.send({ type: "hello", clientId: 4, role: "app", protocol: 99, name: "test" });
    const frame = await socket.waitFor((message) => message.type === "shutdown");
    assert.equal(frame.retryable, false);
    socket.close();
  }));

await test("a large text frame survives the 16-bit length path", () =>
  withDaemon({ eventInterval: 0, pingInterval: 0 }, async (_daemon, port) => {
    const socket = await connectWebSocket(port);
    // 300 bytes forces the 126 + uint16 header on the way in; the reply is
    // small, so this exercises the client's extended-length encoder.
    socket.send({
      type: "hello",
      clientId: 5,
      role: "app",
      protocol: 1,
      name: `WSync Desktop ${"x".repeat(300)}`,
    });
    const hello = await socket.next();
    assert.equal(hello.type, "hello");
    socket.close();
  }));

await test("unmasked client frames are refused (RFC 6455 5.1)", () =>
  withDaemon({}, async (_daemon, port) => {
    const socket = await connectWebSocket(port);
    socket.sendUnmasked({ type: "hello", clientId: 6, role: "app", protocol: 1, name: "bad" });
    const frame = await socket.next();
    assert.ok(frame.closed, "the mock must close on an unmasked client frame");
    socket.close();
  }));

await test("shutdown reaches connected clients", () =>
  withDaemon({}, async (daemon, port) => {
    const socket = await connectWebSocket(port);
    socket.send({ type: "hello", clientId: 7, role: "app", protocol: 1, name: "test" });
    await socket.waitFor((frame) => frame.type === "hello");
    daemon.shutdown("stop");
    const frame = await socket.waitFor((message) => message.type === "shutdown" || message.closed);
    assert.ok(frame.type === "shutdown" || frame.closed);
    socket.close();
  }));

// --- the spawn contract, driven as a real child process ---------------------

function spawnMock(args, env = {}) {
  const child = spawn(process.execPath, [MOCK, ...args], {
    env: { ...process.env, ...env },
    stdio: ["ignore", "pipe", "pipe"],
  });
  let stdout = "";
  let stderr = "";
  child.stdout.setEncoding("utf8");
  child.stderr.setEncoding("utf8");
  child.stdout.on("data", (chunk) => (stdout += chunk));
  child.stderr.on("data", (chunk) => (stderr += chunk));
  return {
    child,
    firstLine(timeout = 8000) {
      return new Promise((resolve, reject) => {
        const timer = setTimeout(() => reject(new Error(`no ready line; stderr: ${stderr}`)), timeout);
        const check = () => {
          const end = stdout.indexOf("\n");
          if (end === -1) return;
          clearTimeout(timer);
          child.stdout.off("data", check);
          resolve(stdout.slice(0, end));
        };
        child.stdout.on("data", check);
        check();
      });
    },
    exit() {
      return new Promise((resolve) => child.on("exit", (code) => resolve(code)));
    },
    get stderr() {
      return stderr;
    },
  };
}

await test("the spawn contract prints exactly one JSON ready line", async () => {
  const run = spawnMock(
    [
      "daemon",
      "start",
      "--project",
      "/tmp/wsync-mock-project",
      "--managed-by",
      "desktop",
      "--owner-token-env",
      "WSYNC_OWNER_TOKEN",
      "--data-dir",
      "/tmp",
      "--raw",
    ],
    { WSYNC_OWNER_TOKEN: "a".repeat(64) },
  );
  try {
    const line = await run.firstLine();
    const ready = JSON.parse(line);
    assert.equal(ready.ok, true);
    assert.ok(Number.isInteger(ready.port) && ready.port > 0);
    assert.ok(Number.isInteger(ready.pid));
    assert.equal(typeof ready.bootId, "string");
    assert.equal(ready.project, "/tmp/wsync-mock-project");
    assert.equal(typeof ready.canonicalProject, "string");

    // The child *is* the daemon: it is still serving after the ready line.
    const hello = await request(ready.port, "GET", "/hello");
    assert.equal(hello.status, 200);
    assert.equal(hello.body.bootId, ready.bootId);

    // ...and it honours an authenticated stop.
    const stopped = await request(ready.port, "POST", "/stop", {
      bootId: ready.bootId,
      token: "a".repeat(64),
    });
    assert.equal(stopped.status, 200);
    assert.equal(await run.exit(), 0);
  } finally {
    run.child.kill("SIGKILL");
  }
});

await test("a refusal is a single {ok:false} line and a nonzero exit", async () => {
  const run = spawnMock([
    "daemon",
    "start",
    "--project",
    "/tmp/wsync-mock-project",
    "--raw",
    "--fail",
    "port 7978 serves another project",
  ]);
  const ready = JSON.parse(await run.firstLine());
  assert.equal(ready.ok, false);
  assert.equal(ready.error.message, "port 7978 serves another project");
  assert.equal(await run.exit(), 1);
});

await test("--already-running reports the adopt path and exits", async () => {
  const run = spawnMock([
    "daemon",
    "start",
    "--project",
    "/tmp/wsync-mock-project",
    "--already-running",
    "--raw",
  ]);
  const ready = JSON.parse(await run.firstLine());
  assert.equal(ready.ok, true);
  assert.equal(ready.alreadyRunning, true);
  assert.equal(await run.exit(), 0, "an adopted daemon's launcher must not linger");
});

await test("a named owner-token env that is unset is a refusal, not a silent no-auth", async () => {
  const run = spawnMock(
    ["daemon", "start", "--project", "/tmp/p", "--owner-token-env", "WSYNC_OWNER_TOKEN", "--raw"],
    { WSYNC_OWNER_TOKEN: "" },
  );
  const ready = JSON.parse(await run.firstLine());
  assert.equal(ready.ok, false);
  assert.equal(ready.error.code, "missing_owner_token");
  assert.equal(await run.exit(), 1);
});

process.stdout.write(failures === 0 ? "\nall mock daemon tests passed\n" : `\n${failures} failing\n`);
process.exit(failures === 0 ? 0 : 1);
