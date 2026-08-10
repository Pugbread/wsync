--!strict
-- Captured-source cache for the bulk-transfer ops (Design §7.4-A; pinned
-- wave-4 contract): `read_subtree` captures each script's editor-aware
-- source once, and `source_read` serves those exact bytes across the whole
-- chunked transfer — a mid-transfer editor keystroke can never tear the byte
-- stream out from under the daemon's SHA-256 verification.
--
-- Lifetime rules (pinned): an entry is invalidated on transfer-scoped
-- completion (the part that serves the final byte drops it) or after 60
-- seconds. The TTL is sliding — every access refreshes it — so a slow
-- multi-part transfer stays stable for as long as it keeps reading, while an
-- abandoned capture expires.
--
-- Pure state machine with explicit state and an injected clock (the
-- WriteRules pattern) — no requires, no Roblox APIs — loadable under a
-- standalone `luau` binary for behavior tests.

local SourceCache = {}

SourceCache.TTL_SECONDS = 60

export type Entry = {
	bytes: string,
	sha256: string,
	touchedAt: number,
}

export type Cache = {
	ttl: number,
	entries: { [string]: Entry },
}

export type ReadResult = {
	data: string,
	eof: boolean,
	totalBytes: number,
}

function SourceCache.new(ttlSeconds: number?): Cache
	return {
		ttl = ttlSeconds or SourceCache.TTL_SECONDS,
		entries = {},
	}
end

--- Drop every entry idle for longer than the TTL.
function SourceCache.sweep(cache: Cache, now: number)
	for key, entry in cache.entries do
		if now - entry.touchedAt > cache.ttl then
			cache.entries[key] = nil
		end
	end
end

--- Capture (or re-capture) the bytes served for `key`. Overwrites any
--- previous capture — a new capture starts a new transfer.
function SourceCache.capture(cache: Cache, key: string, bytes: string, sha256: string, now: number)
	SourceCache.sweep(cache, now)

	cache.entries[key] = {
		bytes = bytes,
		sha256 = sha256,
		touchedAt = now,
	}
end

--- The live entry for `key`, with its TTL refreshed — or nil when it never
--- existed, completed, or idled out.
function SourceCache.get(cache: Cache, key: string, now: number): Entry?
	SourceCache.sweep(cache, now)

	local entry = cache.entries[key]

	if entry == nil then
		return nil
	end

	entry.touchedAt = now

	return entry
end

--- Serve one bounded part of the captured bytes, or nil on a cache miss.
--- `offset` is 0-based. The part that serves the final byte (eof) completes
--- the transfer and drops the entry (pinned: invalidate on transfer-scoped
--- completion); a reader past the end gets an empty eof part the same way.
function SourceCache.read(cache: Cache, key: string, offset: number, maxLen: number, now: number): ReadResult?
	local entry = SourceCache.get(cache, key, now)

	if entry == nil then
		return nil
	end

	local totalBytes = #entry.bytes
	local data = if offset >= totalBytes then "" else entry.bytes:sub(offset + 1, offset + maxLen)
	local eof = offset + #data >= totalBytes

	if eof then
		cache.entries[key] = nil
	end

	return {
		data = data,
		eof = eof,
		totalBytes = totalBytes,
	}
end

function SourceCache.invalidate(cache: Cache, key: string)
	cache.entries[key] = nil
end

function SourceCache.clear(cache: Cache)
	table.clear(cache.entries)
end

return SourceCache
