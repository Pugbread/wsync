--!strict
-- Playtest transport protocol (playtest.json; pinned wave-6 contracts): the
-- constants and frame-authentication scheme shared by the edit-side
-- coordinator (Playtest/init) and the runtime-side agent (Playtest/Runtime).
--
-- Runtime plugin copies talk to the edit copy exclusively over
-- PluginConnectionService — they never open localhost connections. Every
-- job mints a private generation token (playtest.json: "Run coordination is
-- authenticated with the private job generation token"), handed to the
-- playtest DataModels through StudioTestService test args. Both directions
-- wrap every message in one authenticated envelope:
--
--   {kind = "wsync-playtest", jobId, runtimeId, seq, bodyJson,
--    auth = sha256(token .. "|wsync-playtest|" .. direction .. "|" ..
--                  runtimeId .. "|" .. seq .. "|" .. sha256(bodyJson))}
--
-- so a frame from a stale generation (old token), a tampered body, a replayed
-- sequence number, or the wrong direction never validates. The two
-- pre-authentication kinds are `discover` (edit asks who is there — carries
-- nothing) and `hello` (the runtime identifies itself; its auth binds the
-- token to jobId/runtimeId/role/protocol, so a hello without the current
-- generation's token is ignored).
--
-- Pure: no requires, no Roblox APIs — the SHA-256 implementation is injected
-- by the caller (BufferHash in Studio, a stub under the standalone `luau`
-- test binary).

local Protocol = {}

Protocol.PROTOCOL = 1

Protocol.KIND = "wsync-playtest"
Protocol.KIND_DISCOVER = "wsync-playtest-discover"
Protocol.KIND_HELLO = "wsync-playtest-hello"

Protocol.DIRECTION_EDIT_TO_RUNTIME = "edit-runtime"
Protocol.DIRECTION_RUNTIME_TO_EDIT = "runtime-edit"

-- The StudioTestService test-arg keys that carry the generation handoff into
-- the playtest DataModels.
Protocol.TEST_ARG_TOKEN = "__wsyncSessionToken"
Protocol.TEST_ARG_JOB = "__wsyncJobId"

Protocol.LIMITS = {
	-- Playscript / exec source cap (playtest.json: scripts are ≤1 MiB)
	maxScriptBytes = 1024 * 1024,
	-- Per-record payload cap for emit/log/signal data (playtest.json: 64 KiB)
	eventBytes = 64 * 1024,
	-- Encoded result-envelope cap; oversized values are replaced by
	-- `{truncated = true, bytes = N}` (playtest.json)
	resultBytes = 1024 * 1024,
	-- Result envelopes at or under this ride inline in their frame; larger
	-- ones are pinned runtime-side and fetched in bounded parts
	inlineResultBytes = 32 * 1024,
	-- result_read part cap (raw bytes before base64)
	resultChunkBytes = 64 * 1024,
	-- Runtime capture_read part cap (raw RGBA bytes before base64)
	captureChunkBytes = 256 * 1024,
	-- Runtime → edit heartbeat cadence and the staleness threshold beyond
	-- which the coordinator re-checks job status before declaring death
	heartbeatSeconds = 2,
	heartbeatStaleSeconds = 6,
	-- Envelope body caps: boot frames carry ≤1 MiB sources plus args
	-- (JSON-escaped), runtime frames carry ≤64 KiB events, ≤~350 KiB
	-- transfer parts, or byte-budgeted log slices
	maxBodyBytesToRuntime = 3 * 1024 * 1024,
	maxBodyBytesToEdit = 2 * 1024 * 1024,
	-- The run args JSON handed to playscripts (rides every boot frame)
	maxArgsJsonBytes = 256 * 1024,
	-- Total message bytes per runtime logs response (keeps the response
	-- frame far under maxBodyBytesToEdit even at worst-case JSON escaping)
	logsBytesBudget = 128 * 1024,
	-- Signal coordination bounds (Ro-Sync-proven)
	signalNameBytes = 256,
	signalQueueMax = 100,
	signalBacklogMax = 100,
	signalBacklogSeconds = 30,
	-- Runtime result-session registry
	resultTtlSeconds = 120,
	resultMaxSessions = 16,
	-- A run whose main context never boots fails after min(this, timeoutSec)
	bootDeadlineSeconds = 45,
	-- playtest_exec timeout bounds (milliseconds on the wire)
	execDefaultTimeoutMs = 10000,
	execMaxTimeoutMs = 60000,
	-- Upper bound for any single edit → runtime request round-trip
	requestMaxTimeoutSec = 120,
}

export type Sha256Hex = (value: string) -> string

--- The auth digest for one sequenced envelope frame. `direction` separates
--- the two travel directions so a frame can never be reflected back;
--- `runtimeId` and `seq` pin it to one runtime and one position; the body
--- hash makes any payload tamper visible.
function Protocol.frameAuth(
	sha256Hex: Sha256Hex,
	token: string,
	direction: string,
	runtimeId: string,
	seq: number,
	bodyJson: string
): string
	return sha256Hex(
		token
			.. "|wsync-playtest|"
			.. direction
			.. "|"
			.. runtimeId
			.. "|"
			.. tostring(seq)
			.. "|"
			.. sha256Hex(bodyJson)
	)
end

--- The auth digest for a runtime hello: binds the generation token to the
--- job, the runtime id, its role, and the protocol version it speaks.
function Protocol.helloAuth(
	sha256Hex: Sha256Hex,
	token: string,
	jobId: string,
	runtimeId: string,
	role: string,
	protocol: number
): string
	return sha256Hex(
		token .. "|wsync-playtest-hello|" .. jobId .. "|" .. runtimeId .. "|" .. role .. "|" .. tostring(protocol)
	)
end

--- Sequence gate for inbound envelope frames: only a strictly increasing
--- whole-number sequence is acceptable — anything else is a replay, a
--- reorder, or garbage.
function Protocol.acceptSequence(lastSeq: number, seq: any): boolean
	if type(seq) ~= "number" or seq ~= seq or seq % 1 ~= 0 then
		return false
	end

	return seq > lastSeq
end

return Protocol
