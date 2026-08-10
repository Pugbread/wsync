--!strict
-- Remote-op surface (Design §5.4, §15 Phase 5 — read-only inspection plus
-- the mutating write/history slice). The daemon routes CLI/app requests to
-- the plugin as `request` frames over the WebSocket; this module owns the op
-- registry, argument normalization, error normalization, and per-op timing.
-- The transport (Transport/WebSocket) wraps the result into the `response`
-- frame:
--
--   {"type":"response","request_id":<u32>,"ok":true,"value":…,
--    "meta":{"op":…,"durationMs":…,"protocol":1}}
--   {"type":"response","request_id":<u32>,"ok":false,
--    "error":{"code":…,"message":…},"meta":{…}}
--
-- Mutating ops additionally stamp their meta with `mutation: true` and a
-- compact `audit` object (`{op, target, class?, prop?, to?, count?}` —
-- WriteRules.shapeAudit) that the daemon appends to `writes.log`
-- (Design §6.4). Handlers report it through the per-dispatch `ctx.audit`
-- sink after their mutation lands, so a failed response can still carry the
-- audit of a partial batch.
--
-- Error codes are Ro-Sync's, kept verbatim (Design §5.3): UNKNOWN_OP,
-- NOT_FOUND, PERMISSION_REQUIRED, TIMEOUT, INVALID_ARGUMENT, CONFLICT,
-- PLUGIN_ERROR.

local WSync = script:FindFirstAncestor("WSync")

local manifest = require(WSync.manifest)

local Error = require(script.Error)
local Codec = require(script.Codec)
local Paths = require(script.Paths)
local Handlers = require(script.Handlers)
local History = require(script.Handlers.History)
local Bulk = require(script.Handlers.Bulk)
local CaptureOps = require(script.Handlers.Capture)
local Clipboard = require(script.Handlers.Clipboard)

local Remote = {}

Remote.PROTOCOL = 1

export type DispatchMeta = {
	op: string,
	durationMs: number,
	protocol: number,
	mutation: boolean?,
	audit: { [string]: any }?,
}

export type DispatchResult = {
	ok: boolean,
	value: any,
	error: Error.ErrorPayload?,
	meta: DispatchMeta,
}

-- The codec encodes Instances as `__type:"InstancePath"` tags; wire the
-- path bridge here so Codec stays require-free (standalone-testable).
Codec.setInstanceCodec({
	pathOf = Paths.pathOf,
	resolve = Paths.resolve,
})

local pluginVersion = tostring(manifest.package.version)

--- The registered op table (exposed for capabilities and tests).
Remote.ops = Handlers

--- Transport hook: the socket dropped (or is being torn down). An open
--- explicit transaction is auto-cancelled with a visible warning — its owner
--- can never send the matching transaction_finish (Design §5.4) — and every
--- in-flight transfer (captured sources, transfer-local refs, capture
--- sessions, clipboard clips and paste assemblies) is invalidated, since
--- the daemon restarts transfers from scratch.
function Remote.onTransportDisconnect()
	pcall(History.onTransportDisconnect)
	pcall(Bulk.onTransportDisconnect)
	pcall(CaptureOps.onTransportDisconnect)
	pcall(Clipboard.onTransportDisconnect)
end

local function metaFor(op: string, startedAt: number): DispatchMeta
	return {
		op = op,
		durationMs = math.max(0, math.round((os.clock() - startedAt) * 1000 * 100) / 100),
		protocol = Remote.PROTOCOL,
	}
end

--- Execute one remote op. Never raises: every outcome — including unknown
--- ops, malformed args, and handler crashes — comes back as a DispatchResult
--- so the transport can always answer the request.
function Remote.dispatch(op: any, args: any): DispatchResult
	local startedAt = os.clock()
	local opName = if type(op) == "string" then op else tostring(op)

	local handler = Handlers[opName]

	if handler == nil then
		return {
			ok = false,
			value = nil,
			error = {
				code = Error.UNKNOWN_OP,
				message = Error.truncateMessage(`Plugin does not implement op '{opName}'`),
			},
			meta = metaFor(opName, startedAt),
		}
	end

	if args == nil then
		args = {}
	elseif type(args) ~= "table" then
		return {
			ok = false,
			value = nil,
			error = { code = Error.INVALID_ARGUMENT, message = "args must be an object" },
			meta = metaFor(opName, startedAt),
		}
	end

	-- Per-dispatch context: mutating handlers report their audit object
	-- through the `audit` sink once the mutation lands (so it also reaches
	-- error responses after a partial batch or a post-write crash).
	local auditData: { [string]: any }? = nil

	local context = {
		protocol = Remote.PROTOCOL,
		pluginVersion = pluginVersion,
		ops = Handlers,
		audit = function(data: { [string]: any })
			auditData = data
		end,
	}

	local function metaWithAudit(): DispatchMeta
		local meta = metaFor(opName, startedAt)

		if auditData ~= nil then
			meta.mutation = true
			meta.audit = auditData
		end

		return meta
	end

	-- Plain xpcall with an identity trap: RemoteError tables pass through
	-- untouched, and no traceback is ever attached to what goes on the wire.
	local ok, result = xpcall(handler, function(caught)
		return caught
	end, args, context)

	if ok then
		return {
			ok = true,
			value = result,
			error = nil,
			meta = metaWithAudit(),
		}
	end

	-- Error.toPayload truncates and scrubs the `<chunk>:<line>:` prefix, so
	-- handler crashes surface as bounded PLUGIN_ERROR messages without
	-- leaking stacks or script paths.
	return {
		ok = false,
		value = nil,
		error = Error.toPayload(result),
		meta = metaWithAudit(),
	}
end

return Remote
