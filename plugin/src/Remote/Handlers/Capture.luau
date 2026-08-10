--!strict
-- Capture ops (capture.json; pinned wave-5 contract): `capture_prepare`
-- renders one ui/model/viewport capture through Remote/Capture and pins the
-- RGBA8 buffer in a session registry; `capture_read` streams it in bounded
-- base64 parts; `capture_close` drops the session.
--
-- Pinned lifetimes: at most 2 concurrent capture sessions (CONFLICT beyond),
-- a 120 s sliding TTL, and close/disconnect/overwrite dropping the pinned
-- buffer. Unlike `source_read`, serving the final byte does *not* complete
-- the session — the daemon verifies the SHA-256 and may re-read before it
-- closes, so only `capture_close`, the TTL, or a disconnect end it.
--
-- Renders are serialized: the renderer owns the camera, backdrops, and
-- Lighting while it works, so a second `capture_prepare` arriving mid-render
-- answers CONFLICT instead of tearing the scene from under the first.
--
-- Session allowances (wave-7): the registry is shared — `transmit_prepare`
-- adopts each image it reads as an ordinary session served by the same
-- `capture_read`/`capture_close` — but the caps are *per origin*. Every entry
-- is tagged `meta.origin`, and the gate counts only its own family, so the
-- pinned 2-session capture rule is untouched by a 16-image transmit batch
-- sitting in the same store, and vice versa. The registry's physical cap is
-- the two allowances summed; the per-origin gate is what actually enforces
-- them. TTL, disconnect clearing, and overwrite semantics are shared.

local WSync = script:FindFirstAncestor("WSync")

local Log = require(WSync.Log)
local Base64 = require(WSync.Lib.Base64)
local Hex = require(WSync.Transport.Hex)
local generateRef = require(WSync.Helpers.generateRef)

local Error = require(script.Parent.Parent.Error)
local Paths = require(script.Parent.Parent.Paths)
local Transfers = require(script.Parent.Parent.Transfers)
local BufferHash = require(script.Parent.Parent.BufferHash)
local Renderer = require(script.Parent.Parent.Capture)

local Options = Renderer.Options

type Args = { [string]: any }

local Capture = {}

Capture.LIMITS = {
	readMaxLen = 65536,
	maxSessions = 2,
	-- Transmit's own allowance (transmit.json): one prepare hands back up to
	-- 16 images and the daemon pulls them one at a time. The 2-session rule
	-- bounds *renders* — one in flight, one being verified — which is not
	-- what a batch of pixels already in hand costs.
	maxTransmitSessions = 16,
	ttlSeconds = Transfers.TTL_SECONDS,
	maxAxis = Options.LIMITS.maxAxis,
	maxPixels = Options.LIMITS.maxPixels,
	minPadding = Options.LIMITS.minPadding,
	maxPadding = Options.LIMITS.maxPadding,
	minFov = Options.LIMITS.minFov,
	maxFov = Options.LIMITS.maxFov,
	defaultModelSize = Options.LIMITS.defaultModelSize,
}

export type Origin = "capture" | "transmit"

local ORIGIN_LIMIT: { [string]: number } = {
	capture = Capture.LIMITS.maxSessions,
	transmit = Capture.LIMITS.maxTransmitSessions,
}

local sessions = Transfers.newOutStore({
	ttl = Capture.LIMITS.ttlSeconds,
	-- Physical backstop only: the per-origin gate below is the real cap
	maxEntries = Capture.LIMITS.maxSessions + Capture.LIMITS.maxTransmitSessions,
	dropOnEof = false,
})

-- The renderer mutates shared scene state (camera, Lighting, backdrops);
-- only one render may be in flight
local rendering = false

--- Transport hook (Remote.onTransportDisconnect): capture sessions cannot
--- outlive the socket that streams them. Transmit items ride the same
--- registry, so they are dropped by the same hook.
function Capture.onTransportDisconnect()
	Transfers.clearOut(sessions)
end

--- Whether a render currently owns the scene. Read by the transmit ops: a
--- `source` batch runs arbitrary Luau, which must not interleave with a
--- half-staged capture scene.
function Capture.isRendering(): boolean
	return rendering
end

--- Live sessions of one origin. Expired entries are swept first, so a stale
--- session can never block a fresh one.
local function countOrigin(origin: string, now: number): number
	Transfers.sweepOut(sessions, now)

	local count = 0

	for _, entry in sessions.entries do
		local meta = entry.meta

		if meta ~= nil and meta.origin == origin then
			count += 1
		end
	end

	return count
end

--- Free slots in one origin's allowance — lets `transmit_prepare` refuse a
--- batch that cannot fit *before* it reads a single pixel.
function Capture.freeSessions(origin: Origin): number
	return math.max(0, (ORIGIN_LIMIT[origin] or 0) - countOrigin(origin, os.clock()))
end

--- Raise CONFLICT unless `origin` has room for one more session.
local function admit(origin: string, now: number)
	local limit = ORIGIN_LIMIT[origin] or 0

	if countOrigin(origin, now) < limit then
		return
	end

	if origin == "transmit" then
		error(
			Error.new(Error.CONFLICT, `{limit} transmit sessions are already open — capture_close the finished ones`),
			0
		)
	end

	error(Error.new(Error.CONFLICT, `{limit} capture sessions are already open — capture_close one first`), 0)
end

local function validateCaptureId(value: any): string
	if type(value) ~= "string" or Hex.decode(value) == nil then
		error(Error.new(Error.INVALID_ARGUMENT, "captureId must be a 32-char hex string"), 0)
	end

	return (value :: string):lower()
end

--- Pin an externally-produced RGBA8 buffer into the SAME session registry the
--- edit captures use, so `capture_read`/`capture_close` serve it identically
--- (pinned wave-6 contract: `playtest_capture` returns the same `{captureId,
--- width, height, bytes, sha256}` shape, readable through the existing pair;
--- wave-7 `transmit_prepare` adopts every image it reads the same way). TTL
--- and disconnect semantics apply unchanged; `origin` selects which allowance
--- the session counts against and defaults to the 2-session capture rule.
function Capture.adopt(bytes: buffer, width: number, height: number, origin: Origin?): { [string]: any }
	local family: string = origin or "capture"
	local now = os.clock()

	admit(family, now)

	local captureId = Hex.encode(generateRef())
	local totalBytes = buffer.len(bytes)
	local sha256 = BufferHash.hex(bytes)

	local stored = Transfers.putOut(sessions, captureId, bytes, sha256, {
		width = width,
		height = height,
		origin = family,
	}, now)

	if stored ~= "ok" then
		error(Error.new(Error.CONFLICT, "the capture session registry filled up — retry"), 0)
	end

	Log.trace(`capture adopt ({family}): {width}×{height}, {totalBytes} bytes`)

	return {
		captureId = captureId,
		width = width,
		height = height,
		bytes = totalBytes,
		sha256 = sha256,
	}
end

--------------------------------------------------------------------------------
-- Ops
--------------------------------------------------------------------------------

Capture.ops = {} :: { [string]: (args: Args, ctx: any) -> any }

--- `capture_prepare {kind, target?, options?}` → `{captureId, width, height,
--- bytes, sha256}`: render one capture and pin its tightly-packed RGBA8
--- buffer for `capture_read`. Everything is validated — kind, target rules,
--- every option, the axis/pixel limits — before the renderer runs.
function Capture.ops.capture_prepare(args: Args, _ctx)
	local options = Options.normalize(args.kind, args.options)
	local kind = options.kind

	local target: Instance? = nil

	if args.target ~= nil then
		if type(args.target) ~= "string" then
			error(Error.new(Error.INVALID_ARGUMENT, "target must be a Studio path string"), 0)
		end

		if kind == "viewport" then
			error(Error.new(Error.INVALID_ARGUMENT, "a viewport capture takes no target"), 0)
		end

		target = Paths.resolve(args.target)
	elseif kind == "model" then
		error(Error.new(Error.INVALID_ARGUMENT, "a model capture requires a target path"), 0)
	end

	-- Reject before rendering: the gate sweeps expired sessions first, so a
	-- stale one never blocks a fresh capture
	admit("capture", os.clock())

	if rendering then
		error(Error.new(Error.CONFLICT, "another capture is rendering — retry once it finishes"), 0)
	end

	rendering = true

	-- `options.kind` is read directly (not through the refined local): the
	-- singleton-union type survives, which the RenderRequest field requires
	local request: Renderer.RenderRequest = {
		kind = options.kind,
		target = target,
		options = options,
	}

	local ok, result = xpcall(function()
		return Renderer.render(request)
	end, function(caught)
		return caught
	end)

	rendering = false

	if not ok then
		error(result, 0)
	end

	local rendered = result :: Renderer.Result
	local captureId = Hex.encode(generateRef())
	local totalBytes = buffer.len(rendered.bytes)
	local sha256 = BufferHash.hex(rendered.bytes)

	local stored = Transfers.putOut(sessions, captureId, rendered.bytes, sha256, {
		width = rendered.width,
		height = rendered.height,
		origin = "capture",
	}, os.clock())

	if stored ~= "ok" then
		error(Error.new(Error.CONFLICT, "the capture session registry filled up mid-render — retry"), 0)
	end

	Log.trace(`capture_prepare: {kind} {rendered.width}×{rendered.height}, {totalBytes} bytes`)

	return {
		captureId = captureId,
		width = rendered.width,
		height = rendered.height,
		bytes = totalBytes,
		sha256 = sha256,
	}
end

--- `capture_read {captureId, offset?, maxLen?}` → `{data, eof, totalBytes}`:
--- one bounded base64 part of the pinned RGBA8 bytes. Reading past the end
--- answers an empty eof part; the session stays pinned until closed.
function Capture.ops.capture_read(args: Args, _ctx)
	local captureId = validateCaptureId(args.captureId)

	local offset = 0

	if args.offset ~= nil then
		if type(args.offset) ~= "number" or args.offset < 0 or args.offset % 1 ~= 0 then
			error(Error.new(Error.INVALID_ARGUMENT, "offset must be a non-negative integer"), 0)
		end

		offset = args.offset
	end

	local maxLen = Capture.LIMITS.readMaxLen

	if args.maxLen ~= nil then
		if
			type(args.maxLen) ~= "number"
			or args.maxLen < 1
			or args.maxLen > Capture.LIMITS.readMaxLen
			or args.maxLen % 1 ~= 0
		then
			error(Error.new(Error.INVALID_ARGUMENT, `maxLen must be an integer of 1-{Capture.LIMITS.readMaxLen}`), 0)
		end

		maxLen = args.maxLen
	end

	local part = Transfers.readOut(sessions, captureId, offset, maxLen, os.clock())

	if part == nil then
		error(Error.new(Error.NOT_FOUND, `no capture session {captureId} (expired, closed, or never prepared)`), 0)
	end

	return {
		data = Base64.encode(part.data),
		eof = part.eof,
		totalBytes = part.totalBytes,
	}
end

--- `capture_close {captureId}` → `{ok}`: drop the pinned buffer. Idempotent —
--- closing an already-gone session still answers ok.
function Capture.ops.capture_close(args: Args, _ctx)
	local captureId = validateCaptureId(args.captureId)

	Transfers.dropOut(sessions, captureId)

	return { ok = true }
end

return Capture
