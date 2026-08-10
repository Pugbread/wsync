--!strict
-- Bulk-transfer ops for the daemon-driven "Keep Studio" pull (Design §7.4-A;
-- pinned wave-4 contracts): `read_subtree` snapshots a live Studio subtree
-- in the AddedSnapshot wire shape with every script `Source` elided into a
-- SHA-256 manifest, and `source_read` streams the exact captured bytes in
-- bounded base64 parts (offset + total, eof-terminated) so the daemon can
-- verify each script against the manifest.
--
-- Stability contract (pinned): the bytes served by `source_read` are the
-- bytes captured by `read_subtree` (editor-aware, ScriptEditorService first)
-- — cached per ref in Remote/SourceCache with a sliding 60 s TTL and
-- transfer-scoped completion invalidation, so a mid-transfer editor
-- keystroke can never tear a stream. A cache miss re-captures from the live
-- instance; the daemon's SHA-256 check catches any drift and restarts that
-- entry's transfer with a fresh `read_subtree`.
--
-- Refs: tracked instances use their tree refs; untracked descendants (and
-- pending Studio-only divergence entries) get plugin-minted refs held in a
-- transfer-local registry — never inserted into the live tree, so transfer
-- handles cannot leak into live syncback. Core wires the tree and the
-- divergence side registry in through `Bulk.attach` for the session's
-- lifetime; without a session both ops answer CONFLICT.

local ScriptEditorService = game:GetService("ScriptEditorService")

local WSync = script:FindFirstAncestor("WSync")

local Log = require(WSync.Log)
local SyncScope = require(WSync.SyncScope)
local Sha256 = require(WSync.Lib.Sha256)
local Base64 = require(WSync.Lib.Base64)
local Hex = require(WSync.Transport.Hex)
local generateRef = require(WSync.Helpers.generateRef)

local Error = require(script.Parent.Parent.Error)
local SourceCache = require(script.Parent.Parent.SourceCache)

type Args = { [string]: any }

export type Session = {
	getInstanceForRef: (ref: buffer) -> Instance?,
	getRefForInstance: (instance: Instance) -> buffer?,
	resolvePendingRemoval: (refHex: string) -> Instance?,
	pendingRefForInstance: (instance: Instance) -> string?,
	-- The read-processor snapshotting pair (Core/Processor/Read), injected so
	-- this module reuses the exact live-sync property semantics without a
	-- Remote -> Core require
	snapshotProperties: (instance: Instance, skipProperties: { [string]: boolean }?) -> { [string]: any },
	shouldSyncProperties: (instance: Instance) -> boolean,
}

local Bulk = {}

Bulk.LIMITS = {
	sourceReadMaxLen = 65536,
	subtreeMaxNodes = 20000,
	sourceTtlSeconds = SourceCache.TTL_SECONDS,
}

local SKIP_SOURCE: { [string]: boolean } = { Source = true }

local session: Session? = nil
local cache = SourceCache.new()

-- Transfer-local minted refs for untracked instances, stable across repeated
-- reads (sliding TTL, swept together with the source cache)
local mintedInstances: { [string]: Instance } = {}
local mintedRefs: { [Instance]: string } = {}
local mintedTouched: { [string]: number } = {}

--------------------------------------------------------------------------------
-- Session + registry lifecycle
--------------------------------------------------------------------------------

local function clearTransferState()
	SourceCache.clear(cache)
	table.clear(mintedInstances)
	table.clear(mintedRefs)
	table.clear(mintedTouched)
end

--- Core wires the active session in after the processor hydrates.
function Bulk.attach(newSession: Session)
	session = newSession
	clearTransferState()
end

--- Core detaches on stop; a torn-down session invalidates every transfer.
function Bulk.detach()
	session = nil
	clearTransferState()
end

--- Transport hook (Remote.onTransportDisconnect): a transfer cannot outlive
--- the socket that drives it — the daemon restarts from `read_subtree`.
function Bulk.onTransportDisconnect()
	clearTransferState()
end

local function sweepMinted(now: number)
	for refHex, touched in mintedTouched do
		if now - touched > SourceCache.TTL_SECONDS then
			local instance = mintedInstances[refHex]

			if instance then
				mintedRefs[instance] = nil
			end

			mintedInstances[refHex] = nil
			mintedTouched[refHex] = nil
		end
	end
end

local function requireSession(): Session
	if session == nil then
		error(Error.new(Error.CONFLICT, "no active sync session — connect the plugin before bulk transfers"), 0)
	end

	return session :: Session
end

--------------------------------------------------------------------------------
-- Ref resolution
--------------------------------------------------------------------------------

local function validateRefArg(value: any): (string, buffer)
	local decoded = Hex.decode(value)

	if decoded == nil then
		error(Error.new(Error.INVALID_ARGUMENT, "ref must be a 32-char hex string"), 0)
	end

	return (value :: string):lower(), decoded
end

--- Resolve a wire ref to a live instance: the tree first, then the pending
--- divergence removals (Studio-only entries named by plugin-minted refs),
--- then the transfer-local minted registry.
local function resolveRef(activeSession: Session, refHex: string, ref: buffer, now: number): Instance?
	local tracked = activeSession.getInstanceForRef(ref)

	if tracked ~= nil then
		return tracked
	end

	local pending = activeSession.resolvePendingRemoval(refHex)

	if pending ~= nil then
		return pending
	end

	local minted = mintedInstances[refHex]

	if minted then
		mintedTouched[refHex] = now
		return minted
	end

	return nil
end

--- The wire ref hex for an instance inside a subtree walk: the tree ref when
--- tracked, then the ref the divergence upload already minted for it, then a
--- transfer-local minted ref (registry-stable across repeated reads).
local function refHexFor(activeSession: Session, instance: Instance, now: number): string
	local tracked = activeSession.getRefForInstance(instance)

	if tracked ~= nil then
		return Hex.encode(tracked)
	end

	local pending = activeSession.pendingRefForInstance(instance)

	if pending ~= nil then
		return pending
	end

	local existing = mintedRefs[instance]

	if existing ~= nil then
		mintedTouched[existing] = now
		return existing
	end

	local refHex = Hex.encode(generateRef())

	mintedInstances[refHex] = instance
	mintedRefs[instance] = refHex
	mintedTouched[refHex] = now

	return refHex
end

--------------------------------------------------------------------------------
-- Source capture (editor-aware, cache-backed)
--------------------------------------------------------------------------------

--- Read a script's source through ScriptEditorService so an open editor
--- draft is reflected (the Inspect/Write convention); falls back to the raw
--- property.
local function readScriptSource(instance: Instance): string
	local ok, source = pcall(function()
		return ScriptEditorService:GetEditorSource(instance :: any)
	end)

	if ok and type(source) == "string" then
		return source
	end

	return (instance :: any).Source
end

--- Capture a script's current source into the cache: these exact bytes (and
--- their SHA-256) are what the manifest reports and `source_read` serves.
local function captureSource(instance: Instance, refHex: string, now: number): SourceCache.Entry
	local ok, source = pcall(readScriptSource, instance)

	if not ok or type(source) ~= "string" then
		error(Error.new(Error.PLUGIN_ERROR, `failed to read the source of '{instance.Name}'`), 0)
	end

	SourceCache.capture(cache, refHex, source, Sha256.hash(source), now)

	return SourceCache.get(cache, refHex, now) :: SourceCache.Entry
end

--------------------------------------------------------------------------------
-- Subtree snapshotting (read-processor conventions, Source elided)
--------------------------------------------------------------------------------

type WalkState = {
	session: Session,
	now: number,
	nodes: number,
	codeScope: boolean,
	sources: { [string]: { bytes: number, sha256: string } },
}

local function buildPod(instance: Instance, state: WalkState, forcedRefHex: string?): { [string]: any }?
	if forcedRefHex == nil and state.codeScope and not SyncScope.isCodeClass(instance.ClassName) then
		-- Code scope (Design §7.0, pinned): subtree results carry only the
		-- four allowlisted classes as children — the daemon relies on this
		-- for the Studio → disk pull. Folders pass even when scriptless. The
		-- explicitly requested root always answers (the daemon chose its
		-- ref); the sources map is unaffected either way.
		return nil
	end

	state.nodes += 1

	if state.nodes > Bulk.LIMITS.subtreeMaxNodes then
		error(
			Error.new(
				Error.INVALID_ARGUMENT,
				`subtree exceeds {Bulk.LIMITS.subtreeMaxNodes} instances — page by reading child refs instead`
			),
			0
		)
	end

	-- The root keeps the exact ref the daemon asked for (it may have resolved
	-- through the pending-divergence registry), so the manifest keys, the pod
	-- id and later source_read calls all line up
	local refHex = forcedRefHex or refHexFor(state.session, instance, state.now)
	local isScript = instance:IsA("LuaSourceContainer")

	-- Snapshot everything, then keep exactly what the per-property rule lets
	-- sync: scripts carry their full property set (sans Source, manifested
	-- separately), and every in-scope instance carries Attributes and Tags —
	-- stripping those here while the connect diff counts them is what made a
	-- review row reappear at every reconnect
	local properties = state.session.snapshotProperties(instance, if isScript then SKIP_SOURCE else nil)

	for property in pairs(properties) do
		if not state.session.shouldSyncProperties(instance, property) then
			properties[property] = nil
		end
	end

	if isScript then
		local entry = captureSource(instance, refHex, state.now)

		state.sources[refHex] = {
			bytes = #entry.bytes,
			sha256 = entry.sha256,
		}
	end

	if next(properties) == nil then
		-- The ArgonEmpty marker keeps an empty property map an object through
		-- JSONEncode (Core/Snapshot.validateProperties, wire contract)
		properties.ArgonEmpty = { Bool = true }
	end

	local children = {}

	for _, child in instance:GetChildren() do
		local pod = buildPod(child, state)

		if pod ~= nil then
			table.insert(children, pod)
		end
	end

	return {
		id = refHex,
		meta = { keepUnknowns = false },
		name = instance.Name,
		class = instance.ClassName,
		properties = properties,
		children = children,
	}
end

--------------------------------------------------------------------------------
-- Ops
--------------------------------------------------------------------------------

Bulk.ops = {} :: { [string]: (args: Args, ctx: any) -> any }

--- `read_subtree {ref}` → `{snapshot}`: the AddedSnapshot-shaped subtree
--- read live from Studio. Every script `Source` is elided; the root carries
--- a `sources` map `{"<ref-hex>": {bytes, sha256}}` covering the subtree,
--- and the captured bytes are pinned in the source cache for `source_read`.
function Bulk.ops.read_subtree(args: Args, _ctx)
	local activeSession = requireSession()
	local refHex, ref = validateRefArg(args.ref)
	local now = os.clock()

	sweepMinted(now)

	local instance = resolveRef(activeSession, refHex, ref, now)

	if instance == nil then
		error(Error.new(Error.NOT_FOUND, `no instance for ref {refHex}`), 0)
	end

	local state: WalkState = {
		session = activeSession,
		now = now,
		nodes = 0,
		codeScope = SyncScope.isCode(),
		sources = {},
	}

	local pod = buildPod(instance, state, refHex)

	if pod == nil then
		-- Unreachable (the requested root always answers); kept for typing
		-- and as a tripwire
		error(Error.new(Error.NOT_FOUND, `no instance for ref {refHex}`), 0)
	end

	local okParent, parentInstance = pcall(function()
		return instance.Parent
	end)

	if okParent and parentInstance ~= nil then
		local parentRef = activeSession.getRefForInstance(parentInstance)

		if parentRef ~= nil then
			pod.parent = Hex.encode(parentRef)
		end
	end

	-- Omitted when the subtree holds no scripts: an empty Luau map would
	-- JSON-encode as an array
	local sourceCount = 0

	for _ in state.sources do
		sourceCount += 1
	end

	if sourceCount > 0 then
		pod.sources = state.sources
	end

	Log.trace(`read_subtree: {state.nodes} nodes, {sourceCount} script sources`)

	return { snapshot = pod }
end

--- `source_read {ref, offset?, maxLen?}` → `{data, eof, totalBytes}`: one
--- bounded base64 part of the captured source bytes. Serves the capture made
--- by `read_subtree` (re-capturing only on cache miss); the part that serves
--- the final byte completes the transfer and drops the capture.
function Bulk.ops.source_read(args: Args, _ctx)
	local activeSession = requireSession()
	local refHex, ref = validateRefArg(args.ref)

	local offset = 0

	if args.offset ~= nil then
		if type(args.offset) ~= "number" or args.offset < 0 or args.offset % 1 ~= 0 then
			error(Error.new(Error.INVALID_ARGUMENT, "offset must be a non-negative integer"), 0)
		end

		offset = args.offset
	end

	local maxLen = Bulk.LIMITS.sourceReadMaxLen

	if args.maxLen ~= nil then
		if
			type(args.maxLen) ~= "number"
			or args.maxLen < 1
			or args.maxLen > Bulk.LIMITS.sourceReadMaxLen
			or args.maxLen % 1 ~= 0
		then
			error(Error.new(Error.INVALID_ARGUMENT, `maxLen must be an integer of 1-{Bulk.LIMITS.sourceReadMaxLen}`), 0)
		end

		maxLen = args.maxLen
	end

	local now = os.clock()

	sweepMinted(now)

	local cached = SourceCache.read(cache, refHex, offset, maxLen, now)

	if cached == nil then
		-- Cache miss (expired, completed, or never captured): re-capture from
		-- the live instance — the daemon's SHA-256 check owns drift detection
		local instance = resolveRef(activeSession, refHex, ref, now)

		if instance == nil then
			error(Error.new(Error.NOT_FOUND, `no instance for ref {refHex}`), 0)
		end

		if not instance:IsA("LuaSourceContainer") then
			error(Error.new(Error.INVALID_ARGUMENT, `ref {refHex} is not a script`), 0)
		end

		captureSource(instance, refHex, now)

		cached = SourceCache.read(cache, refHex, offset, maxLen, now)
	end

	local part = cached :: SourceCache.ReadResult

	return {
		data = Base64.encode(part.data),
		eof = part.eof,
		totalBytes = part.totalBytes,
	}
end

return Bulk
