--!strict
-- Bounded chunk-transfer registries for the capture and clipboard op families
-- (pinned wave-5 contracts). Two shapes, both keyed by an id the handler
-- mints:
--
--   * Out — a finished payload served to the daemon in bounded parts
--     (`capture_read`, `clipboard_read`). Entries carry the payload buffer,
--     its SHA-256, and an opaque meta table (capture dimensions). The
--     `dropOnEof` flag selects between the SourceCache completion semantics
--     (clipboard: the part that serves the final byte drops the entry) and
--     session semantics (capture: the entry lives until `capture_close`,
--     disconnect, overwrite, or TTL).
--   * In — a payload assembled from the daemon in strictly sequential parts
--     (`clipboard_paste_begin/chunk/commit`). Offsets must equal the byte
--     count received so far; anything else is rejected before a byte lands.
--
-- Lifetime rules (pinned): sliding TTL — every access refreshes it — with a
-- 120 s default, an optional entry cap ("full" instead of eviction, the
-- handler answers CONFLICT), and registry-wide `clear` for transport
-- disconnects. Pure state machine with an injected clock (the
-- SourceCache/WriteRules pattern) — no requires, no Roblox APIs — loadable
-- under a standalone `luau` binary for behavior tests.

local Transfers = {}

Transfers.TTL_SECONDS = 120

export type OutEntry = {
	bytes: buffer,
	sha256: string,
	meta: { [string]: any }?,
	touchedAt: number,
}

export type OutStore = {
	ttl: number,
	maxEntries: number?,
	dropOnEof: boolean,
	entries: { [string]: OutEntry },
}

export type InEntry = {
	bytes: buffer,
	sha256: string,
	total: number,
	received: number,
	touchedAt: number,
}

export type InStore = {
	ttl: number,
	maxEntries: number?,
	entries: { [string]: InEntry },
}

export type ReadResult = {
	data: string,
	eof: boolean,
	totalBytes: number,
}

--------------------------------------------------------------------------------
-- Out: finished payloads served in bounded parts
--------------------------------------------------------------------------------

type OutConfig = { ttl: number?, maxEntries: number?, dropOnEof: boolean? }

function Transfers.newOutStore(config: OutConfig?): OutStore
	local supplied: OutConfig = config or {}

	return {
		ttl = supplied.ttl or Transfers.TTL_SECONDS,
		maxEntries = supplied.maxEntries,
		dropOnEof = supplied.dropOnEof == true,
		entries = {},
	}
end

--- Drop every entry idle for longer than the TTL.
function Transfers.sweepOut(store: OutStore, now: number)
	for key, entry in store.entries do
		if now - entry.touchedAt > store.ttl then
			store.entries[key] = nil
		end
	end
end

function Transfers.countOut(store: OutStore): number
	local count = 0

	for _ in store.entries do
		count += 1
	end

	return count
end

--- Store a finished payload under `key`. Re-using a live key overwrites it
--- (a new transfer replaces the old one — pinned). Returns "full" without
--- storing when the entry cap is reached by *other* live entries.
function Transfers.putOut(
	store: OutStore,
	key: string,
	bytes: buffer,
	sha256: string,
	meta: { [string]: any }?,
	now: number
): "ok" | "full"
	Transfers.sweepOut(store, now)

	if store.maxEntries ~= nil and store.entries[key] == nil and Transfers.countOut(store) >= store.maxEntries then
		return "full"
	end

	store.entries[key] = {
		bytes = bytes,
		sha256 = sha256,
		meta = meta,
		touchedAt = now,
	}

	return "ok"
end

--- The live entry for `key`, with its TTL refreshed — or nil when it never
--- existed, completed, or idled out.
function Transfers.getOut(store: OutStore, key: string, now: number): OutEntry?
	Transfers.sweepOut(store, now)

	local entry = store.entries[key]

	if entry == nil then
		return nil
	end

	entry.touchedAt = now

	return entry
end

--- Serve one bounded part of the payload, or nil on a miss. `offset` is
--- 0-based; a reader past the end gets an empty eof part. With `dropOnEof`,
--- the part that serves the final byte completes the transfer and drops the
--- entry (the SourceCache completion contract).
function Transfers.readOut(store: OutStore, key: string, offset: number, maxLen: number, now: number): ReadResult?
	local entry = Transfers.getOut(store, key, now)

	if entry == nil then
		return nil
	end

	local totalBytes = buffer.len(entry.bytes)
	local count = math.min(maxLen, math.max(0, totalBytes - offset))
	local data = if count > 0 then buffer.readstring(entry.bytes, offset, count) else ""
	local eof = offset + count >= totalBytes

	if eof and store.dropOnEof then
		store.entries[key] = nil
	end

	return {
		data = data,
		eof = eof,
		totalBytes = totalBytes,
	}
end

function Transfers.dropOut(store: OutStore, key: string)
	store.entries[key] = nil
end

function Transfers.clearOut(store: OutStore)
	table.clear(store.entries)
end

--------------------------------------------------------------------------------
-- In: payloads assembled from strictly sequential parts
--------------------------------------------------------------------------------

type InConfig = { ttl: number?, maxEntries: number? }

function Transfers.newInStore(config: InConfig?): InStore
	local supplied: InConfig = config or {}

	return {
		ttl = supplied.ttl or Transfers.TTL_SECONDS,
		maxEntries = supplied.maxEntries,
		entries = {},
	}
end

function Transfers.sweepIn(store: InStore, now: number)
	for key, entry in store.entries do
		if now - entry.touchedAt > store.ttl then
			store.entries[key] = nil
		end
	end
end

function Transfers.countIn(store: InStore): number
	local count = 0

	for _ in store.entries do
		count += 1
	end

	return count
end

--- Open an assembly of exactly `total` bytes whose finished SHA-256 must be
--- `sha256`. The caller validates both before calling; re-using a live key
--- restarts its assembly from scratch.
function Transfers.beginIn(store: InStore, key: string, total: number, sha256: string, now: number): "ok" | "full"
	Transfers.sweepIn(store, now)

	if store.maxEntries ~= nil and store.entries[key] == nil and Transfers.countIn(store) >= store.maxEntries then
		return "full"
	end

	store.entries[key] = {
		bytes = buffer.create(total),
		sha256 = sha256,
		total = total,
		received = 0,
		touchedAt = now,
	}

	return "ok"
end

--- The live assembly for `key`, with its TTL refreshed — or nil when it
--- never existed, was consumed, or idled out.
function Transfers.getIn(store: InStore, key: string, now: number): InEntry?
	Transfers.sweepIn(store, now)

	local entry = store.entries[key]

	if entry == nil then
		return nil
	end

	entry.touchedAt = now

	return entry
end

export type AppendResult = "ok" | "missing" | "gap" | "overflow" | "empty"

--- Append one part. Sequential offsets only (pinned): `offset` must equal
--- the byte count received so far — a gap, a rewind, or a repeat is rejected
--- without landing a byte, as is a part that would overrun the declared
--- total. Empty parts are rejected so an assembly always makes progress.
function Transfers.appendIn(store: InStore, key: string, offset: number, data: string, now: number): AppendResult
	Transfers.sweepIn(store, now)

	local entry = store.entries[key]

	if entry == nil then
		return "missing"
	end

	entry.touchedAt = now

	if #data == 0 then
		return "empty"
	end

	if offset ~= entry.received then
		return "gap"
	end

	if entry.received + #data > entry.total then
		return "overflow"
	end

	buffer.writestring(entry.bytes, entry.received, data)
	entry.received += #data

	return "ok"
end

--- Take the assembly out of the store (commit consumes it whatever the
--- outcome — a failed commit restarts from `begin`). Returns nil on a miss;
--- the caller checks `received == total` and verifies the SHA-256.
function Transfers.takeIn(store: InStore, key: string, now: number): InEntry?
	Transfers.sweepIn(store, now)

	local entry = store.entries[key]

	store.entries[key] = nil

	return entry
end

function Transfers.dropIn(store: InStore, key: string)
	store.entries[key] = nil
end

function Transfers.clearIn(store: InStore)
	table.clear(store.entries)
end

return Transfers
