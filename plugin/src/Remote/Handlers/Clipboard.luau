--!strict
-- Clipboard ops (copy.json / paste.json; pinned wave-5 contracts):
-- `clipboard_copy` serializes native Studio instance trees through
-- SerializationService and pins the payload for `clipboard_read`;
-- `clipboard_paste_begin/chunk/commit` assemble a payload back and paste it
-- as one undoable change.
--
-- Payload envelope: the streamed bytes are `WSCLIP1\n` + u32le header
-- length + a JSON header + the raw SerializationService buffer. The header
-- carries what paste routing needs — each root's `{name, class, route}`
-- with the parent route as `{name, class, ordinal}` segments (Remote/
-- Routes) — because the pinned `clipboard_paste_commit {clipId, to?,
-- noSelect?}` shape has nowhere else to carry it. The envelope is opaque to
-- the daemon and the CLI (they see `bytes` + `sha256` and verify the stream
-- end to end); only this module reads or writes it.
--
-- Copy rules (copy.json): no paths = the current Studio selection; services
-- and DataModel-parented instances refuse; nested selections collapse into
-- their outermost roots; ≤256 roots and ≤128 MiB; open Script-editor drafts
-- are refreshed into the payload (serialize → rehydrate detached → patch
-- editor sources → serialize again, only when a draft actually differs).
--
-- Paste rules (paste.json): sha256 verified before deserializing; default
-- routing restores each root's recorded parent route, `to` overrides for
-- all roots; one ChangeHistory recording (one Studio undo); new roots are
-- selected unless `noSelect`.
--
-- Registries: outbound clips live in a 120 s sliding-TTL transfer store —
-- a new copy atomically replaces the previous clip, and the part serving
-- the final byte completes the transfer (SourceCache semantics). Paste
-- assemblies accept strictly sequential offsets, are size-checked at begin
-- and on every chunk, and are consumed by commit whatever its outcome.

local HttpService = game:GetService("HttpService")
local ScriptEditorService = game:GetService("ScriptEditorService")
local Selection = game:GetService("Selection")

local WSync = script:FindFirstAncestor("WSync")

local Log = require(WSync.Log)
local Base64 = require(WSync.Lib.Base64)
local Hex = require(WSync.Transport.Hex)
local generateRef = require(WSync.Helpers.generateRef)

local Error = require(script.Parent.Parent.Error)
local Paths = require(script.Parent.Parent.Paths)
local Routes = require(script.Parent.Parent.Routes)
local Transfers = require(script.Parent.Parent.Transfers)
local BufferHash = require(script.Parent.Parent.BufferHash)
local WriteRules = require(script.Parent.Parent.WriteRules)

local History = require(script.Parent.History)

type Args = { [string]: any }

local Clipboard = {}

Clipboard.LIMITS = {
	maxRoots = 256,
	maxBytes = 128 * 1024 * 1024,
	readMaxLen = 65536,
	chunkMaxLen = 65536,
	ttlSeconds = Transfers.TTL_SECONDS,
	maxPasteSessions = 2,
}

local MAGIC = "WSCLIP1\n"
local ENVELOPE_VERSION = 1

local clips = Transfers.newOutStore({
	ttl = Clipboard.LIMITS.ttlSeconds,
	dropOnEof = true,
})

local assemblies = Transfers.newInStore({
	ttl = Clipboard.LIMITS.ttlSeconds,
	maxEntries = Clipboard.LIMITS.maxPasteSessions,
})

--- Transport hook (Remote.onTransportDisconnect): a clip or assembly cannot
--- outlive the socket that streams it.
function Clipboard.onTransportDisconnect()
	Transfers.clearOut(clips)
	Transfers.clearIn(assemblies)
end

--------------------------------------------------------------------------------
-- Shared plumbing
--------------------------------------------------------------------------------

local INSTANCE_ADAPTER: Routes.Adapter = {
	parentOf = function(node)
		return (node :: Instance).Parent
	end,
	nameOf = function(node)
		return (node :: Instance).Name
	end,
	classOf = function(node)
		return (node :: Instance).ClassName
	end,
	childrenOf = function(node)
		return (node :: Instance):GetChildren()
	end,
	isRoot = function(node)
		return node == game
	end,
}

local function invalid(message: string): never
	error(Error.new(Error.INVALID_ARGUMENT, message), 0)
end

local function pluginError(message: string): never
	error(Error.new(Error.PLUGIN_ERROR, message), 0)
end

local function validateClipId(value: any): string
	if type(value) ~= "string" or Hex.decode(value) == nil then
		invalid("clipId must be a 32-char hex string")
	end

	return (value :: string):lower()
end

local function serializationService(): any
	local ok, service = pcall(function()
		return game:GetService("SerializationService")
	end)

	local usable = false

	if ok and service ~= nil then
		local methodsOk, methodsPresent = pcall(function()
			return (service :: any).SerializeInstancesAsync ~= nil and (service :: any).DeserializeInstancesAsync ~= nil
		end)

		usable = methodsOk and methodsPresent == true
	end

	if not usable then
		pluginError("SerializationService is unavailable in this Studio build")
	end

	return service
end

--- Read a script's source through ScriptEditorService so an open editor
--- draft is reflected (the Inspect/Bulk convention); falls back to the raw
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

local function setArchivableTree(root: Instance, changed: { Instance }?)
	local items = root:GetDescendants()
	table.insert(items, root)

	for _, item in items do
		local ok, current = pcall(function()
			return item.Archivable
		end)

		if ok and current ~= true then
			local wrote = pcall(function()
				item.Archivable = true
			end)

			if wrote and changed ~= nil then
				table.insert(changed, item)
			end
		end
	end
end

local function restoreArchivable(changed: { Instance })
	for index = #changed, 1, -1 do
		pcall(function()
			changed[index].Archivable = false
		end)
	end
end

local function destroyAll(instances: { Instance })
	for _, instance in instances do
		pcall(function()
			instance:Destroy()
		end)
	end
end

--------------------------------------------------------------------------------
-- Copy: root normalization and serialization
--------------------------------------------------------------------------------

--- Resolve the requested roots: explicit paths, or the current Studio
--- selection. Rejects services, unparented or non-DataModel instances, and
--- collapses nested picks into their outermost roots (descendants ride
--- along in the serialized payload anyway).
local function normalizeRoots(paths: any): { Instance }
	local requested: { Instance } = {}

	if paths ~= nil then
		if type(paths) ~= "table" then
			invalid("paths must be an array of Studio paths")
		end

		if #paths > Clipboard.LIMITS.maxRoots then
			invalid(`copy supports at most {Clipboard.LIMITS.maxRoots} root paths`)
		end

		for _, path in paths :: { any } do
			table.insert(requested, Paths.resolve(path))
		end
	end

	if #requested == 0 then
		requested = Selection:Get()

		if #requested == 0 then
			invalid("the Studio selection is empty — select instances or pass paths")
		end

		if #requested > Clipboard.LIMITS.maxRoots then
			invalid(`copy supports at most {Clipboard.LIMITS.maxRoots} selected roots`)
		end
	end

	local selected: { [Instance]: boolean } = {}

	for _, instance in requested do
		if instance == game or instance.Parent == game then
			invalid("DataModel services cannot be copied — copy their children instead")
		end

		if instance.Parent == nil or not instance:IsDescendantOf(game) then
			invalid("only parented DataModel instances can be copied")
		end

		selected[instance] = true
	end

	local roots: { Instance } = {}
	local inserted: { [Instance]: boolean } = {}

	for _, instance in requested do
		if inserted[instance] then
			continue
		end

		local nested = false
		local ancestor = instance.Parent

		while ancestor ~= nil and ancestor ~= game do
			if selected[ancestor] then
				nested = true
				break
			end

			ancestor = ancestor.Parent
		end

		if not nested then
			inserted[instance] = true
			table.insert(roots, instance)
		end
	end

	return roots
end

--- Whether any script under the roots has an open editor draft differing
--- from its serialized `Source` — only then is the patch round-trip needed.
local function hasEditorDrift(roots: { Instance }): boolean
	for _, root in roots do
		local items = root:GetDescendants()
		table.insert(items, root)

		for _, item in items do
			if item:IsA("LuaSourceContainer") then
				local ok, drifted = pcall(function()
					return ScriptEditorService:GetEditorSource(item :: any) ~= (item :: any).Source
				end)

				if ok and drifted then
					return true
				end
			end
		end
	end

	return false
end

--- Copy the visible editor source of every script in `original` onto the
--- structurally-identical detached copy, verifying the shapes still match.
local function patchEditorSources(original: Instance, detached: Instance)
	if original:IsA("LuaSourceContainer") then
		if not detached:IsA("LuaSourceContainer") then
			error("the serialized script shape changed while preparing the clipboard", 0)
		end

		local source = readScriptSource(original)

		local applied = pcall(function()
			ScriptEditorService:UpdateSourceAsync(detached :: any, function()
				return source
			end)
		end)

		if not applied then
			applied = pcall(function()
				(detached :: any).Source = source
			end)
		end

		if not applied then
			error(`could not preserve the editor source of '{original:GetFullName()}'`, 0)
		end
	end

	local originalChildren = original:GetChildren()
	local detachedChildren = detached:GetChildren()

	if #originalChildren ~= #detachedChildren then
		error("the serialized instance shape changed while preparing the clipboard", 0)
	end

	for index, originalChild in originalChildren do
		local detachedChild = detachedChildren[index]

		if originalChild.ClassName ~= detachedChild.ClassName or originalChild.Name ~= detachedChild.Name then
			error("the serialized child order changed while preparing the clipboard", 0)
		end

		patchEditorSources(originalChild, detachedChild)
	end
end

--- Serialize the roots natively. Studio's own Copy ignores Archivable=false,
--- so the whole tree is temporarily forced archivable (restored even when
--- the yielding serializer throws). When an open editor draft differs from
--- the engine `Source`, the payload is rebuilt from detached copies patched
--- with the visible sources, so copied scripts match what the user sees.
local function serializeRoots(service: any, roots: { Instance }): buffer
	local changed: { Instance } = {}

	for _, root in roots do
		setArchivableTree(root, changed)
	end

	local drifted = hasEditorDrift(roots)

	local ok, first = pcall(function()
		return service:SerializeInstancesAsync(roots)
	end)

	restoreArchivable(changed)

	if not ok or typeof(first) ~= "buffer" then
		pluginError(`native Studio serialization failed: {tostring(first)}`)
	end

	if not drifted then
		return first :: buffer
	end

	local decodedOk, decoded = pcall(function()
		return service:DeserializeInstancesAsync(first)
	end)

	if not decodedOk or type(decoded) ~= "table" or #decoded ~= #roots then
		if decodedOk and type(decoded) == "table" then
			destroyAll(decoded :: { Instance })
		end

		pluginError("the serialized payload could not be rehydrated for the editor-source refresh")
	end

	local detached = decoded :: { Instance }

	local patchOk, patchError = pcall(function()
		for index, original in roots do
			local copy = detached[index]

			setArchivableTree(copy, nil)
			patchEditorSources(original, copy)
		end
	end)

	if not patchOk then
		destroyAll(detached)
		error(patchError, 0)
	end

	local finalOk, final = pcall(function()
		return service:SerializeInstancesAsync(detached)
	end)

	destroyAll(detached)

	if not finalOk or typeof(final) ~= "buffer" then
		pluginError(`native Studio serialization failed after the editor-source refresh: {tostring(final)}`)
	end

	return final :: buffer
end

--------------------------------------------------------------------------------
-- Envelope codec
--------------------------------------------------------------------------------

type RootMeta = {
	name: string,
	class: string,
	route: { Routes.Segment },
}

local function buildEnvelope(header: { [string]: any }, rbxm: buffer): buffer
	local encodedOk, encoded = pcall(function()
		return HttpService:JSONEncode(header)
	end)

	if not encodedOk or type(encoded) ~= "string" then
		pluginError("could not encode the clipboard metadata header")
	end

	local headerBytes = encoded :: string
	local total = #MAGIC + 4 + #headerBytes + buffer.len(rbxm)
	local envelope = buffer.create(total)

	buffer.writestring(envelope, 0, MAGIC)
	buffer.writeu32(envelope, #MAGIC, #headerBytes)
	buffer.writestring(envelope, #MAGIC + 4, headerBytes)
	buffer.copy(envelope, #MAGIC + 4 + #headerBytes, rbxm, 0, buffer.len(rbxm))

	return envelope
end

--- Split a verified payload back into its metadata and the native buffer.
--- Every field is untrusted: the shape is validated before anything is
--- deserialized.
local function parseEnvelope(envelope: buffer): ({ RootMeta }, buffer)
	local total = buffer.len(envelope)

	if total < #MAGIC + 4 or buffer.readstring(envelope, 0, #MAGIC) ~= MAGIC then
		invalid("the payload is not a WSync clipboard envelope — re-copy and try again")
	end

	local headerLength = buffer.readu32(envelope, #MAGIC)
	local rbxmOffset = #MAGIC + 4 + headerLength

	if rbxmOffset > total then
		invalid("the clipboard envelope header is truncated")
	end

	local decodedOk, header = pcall(function()
		return HttpService:JSONDecode(buffer.readstring(envelope, #MAGIC + 4, headerLength))
	end)

	if not decodedOk or type(header) ~= "table" or header.version ~= ENVELOPE_VERSION then
		invalid("the clipboard envelope header is malformed or from an incompatible version")
	end

	local rootsMeta = header.roots

	if type(rootsMeta) ~= "table" or #rootsMeta < 1 or #rootsMeta > Clipboard.LIMITS.maxRoots then
		invalid("the clipboard envelope carries no valid root metadata")
	end

	local validated: { RootMeta } = {}

	for _, meta in rootsMeta :: { any } do
		if
			type(meta) ~= "table"
			or type(meta.name) ~= "string"
			or type(meta.class) ~= "string"
			or not Routes.validate(meta.route)
		then
			invalid("the clipboard envelope carries malformed root metadata")
		end

		table.insert(validated, {
			name = meta.name :: string,
			class = meta.class :: string,
			route = meta.route :: { Routes.Segment },
		})
	end

	local rbxmLength = total - rbxmOffset

	if rbxmLength < 1 then
		invalid("the clipboard envelope carries no instance payload")
	end

	local rbxm = buffer.create(rbxmLength)
	buffer.copy(rbxm, 0, envelope, rbxmOffset, rbxmLength)

	return validated, rbxm
end

--------------------------------------------------------------------------------
-- Ops
--------------------------------------------------------------------------------

Clipboard.ops = {} :: { [string]: (args: Args, ctx: any) -> any }

--- `clipboard_copy {paths?}` → `{clipId, bytes, sha256, roots}`: serialize
--- the requested roots (no paths = the current Selection) and pin the
--- payload for `clipboard_read`. A successful copy atomically replaces any
--- previous clip.
function Clipboard.ops.clipboard_copy(args: Args, _ctx)
	local service = serializationService()
	local roots = normalizeRoots(args.paths)

	if #roots > Clipboard.LIMITS.maxRoots then
		invalid(`copy supports at most {Clipboard.LIMITS.maxRoots} roots`)
	end

	-- Capture routes and response metadata before the yielding serializer
	-- runs — parents may move under it
	local rootsMeta: { RootMeta } = {}
	local responseRoots: { { [string]: any } } = {}

	for _, root in roots do
		local parent = root.Parent

		if parent == nil then
			pluginError(`copy root '{root.Name}' lost its parent while preparing the clipboard`)
		end

		local route = Routes.routeOf(parent, INSTANCE_ADAPTER)

		if route == nil then
			pluginError(`could not encode the parent route of '{root:GetFullName()}'`)
		end

		table.insert(rootsMeta, {
			name = root.Name,
			class = root.ClassName,
			route = route :: { Routes.Segment },
		})

		table.insert(responseRoots, {
			path = (Paths.pathOf(root)),
			class = root.ClassName,
			name = root.Name,
		})
	end

	local rbxm = serializeRoots(service, roots)

	local envelope = buildEnvelope({
		version = ENVELOPE_VERSION,
		roots = rootsMeta,
		gameId = tostring(game.GameId),
		placeId = tostring(game.PlaceId),
	}, rbxm)

	local totalBytes = buffer.len(envelope)

	if totalBytes > Clipboard.LIMITS.maxBytes then
		invalid(
			`the serialized payload is {totalBytes} bytes — the clipboard is limited to {Clipboard.LIMITS.maxBytes}`
		)
	end

	local sha256 = BufferHash.hex(envelope)
	local clipId = Hex.encode(generateRef())

	-- Atomic replacement: the previous clip (if any) is dropped with the
	-- same stroke that pins the new one
	Transfers.clearOut(clips)

	if Transfers.putOut(clips, clipId, envelope, sha256, nil, os.clock()) ~= "ok" then
		pluginError("the clipboard registry rejected the payload")
	end

	Log.trace(`clipboard_copy: {#roots} roots, {totalBytes} bytes`)

	return {
		clipId = clipId,
		bytes = totalBytes,
		sha256 = sha256,
		roots = responseRoots,
	}
end

--- `clipboard_read {clipId, offset?, maxLen?}` → `{data, eof, totalBytes}`:
--- one bounded base64 part of the pinned payload. The part that serves the
--- final byte completes the transfer and drops the clip (SourceCache
--- semantics) — the daemon persists the bytes it verified.
function Clipboard.ops.clipboard_read(args: Args, _ctx)
	local clipId = validateClipId(args.clipId)

	local offset = 0

	if args.offset ~= nil then
		if type(args.offset) ~= "number" or args.offset < 0 or args.offset % 1 ~= 0 then
			invalid("offset must be a non-negative integer")
		end

		offset = args.offset
	end

	local maxLen = Clipboard.LIMITS.readMaxLen

	if args.maxLen ~= nil then
		if
			type(args.maxLen) ~= "number"
			or args.maxLen < 1
			or args.maxLen > Clipboard.LIMITS.readMaxLen
			or args.maxLen % 1 ~= 0
		then
			invalid(`maxLen must be an integer of 1-{Clipboard.LIMITS.readMaxLen}`)
		end

		maxLen = args.maxLen
	end

	local part = Transfers.readOut(clips, clipId, offset, maxLen, os.clock())

	if part == nil then
		error(
			Error.new(Error.NOT_FOUND, `no clip {clipId} (expired, completed, or replaced) — re-run clipboard_copy`),
			0
		)
	end

	return {
		data = Base64.encode(part.data),
		eof = part.eof,
		totalBytes = part.totalBytes,
	}
end

--- `clipboard_paste_begin {bytes, sha256}` → `{clipId}`: open an assembly
--- for exactly `bytes` bytes whose SHA-256 must come out as `sha256`.
function Clipboard.ops.clipboard_paste_begin(args: Args, _ctx)
	local total = args.bytes

	if type(total) ~= "number" or total % 1 ~= 0 or total < 1 or total > Clipboard.LIMITS.maxBytes then
		invalid(`bytes must be an integer of 1-{Clipboard.LIMITS.maxBytes}`)
	end

	local sha256 = args.sha256

	if type(sha256) ~= "string" or #sha256 ~= 64 or sha256:find("[^0-9a-fA-F]") ~= nil then
		invalid("sha256 must be a 64-char hex digest")
	end

	local clipId = Hex.encode(generateRef())

	if Transfers.beginIn(assemblies, clipId, total, (sha256 :: string):lower(), os.clock()) ~= "ok" then
		error(
			Error.new(
				Error.CONFLICT,
				`{Clipboard.LIMITS.maxPasteSessions} paste assemblies are already open — commit or let them expire`
			),
			0
		)
	end

	return { clipId = clipId }
end

--- `clipboard_paste_chunk {clipId, offset, data}` → `{ok}`: append one
--- base64 part. Offsets are strictly sequential — `offset` must equal the
--- byte count received so far.
function Clipboard.ops.clipboard_paste_chunk(args: Args, _ctx)
	local clipId = validateClipId(args.clipId)

	if type(args.offset) ~= "number" or args.offset < 0 or args.offset % 1 ~= 0 then
		invalid("offset must be a non-negative integer")
	end

	if type(args.data) ~= "string" or args.data == "" then
		invalid("data must be a non-empty base64 string")
	end

	local decoded = Base64.decode(args.data)

	if decoded == nil then
		invalid("data must be valid base64")
	end

	local bytes = decoded :: string

	if #bytes > Clipboard.LIMITS.chunkMaxLen then
		invalid(`a chunk carries at most {Clipboard.LIMITS.chunkMaxLen} decoded bytes`)
	end

	local now = os.clock()
	local result = Transfers.appendIn(assemblies, clipId, args.offset, bytes, now)

	if result == "missing" then
		error(Error.new(Error.NOT_FOUND, `no paste assembly {clipId} (expired or committed) — restart from begin`), 0)
	elseif result == "gap" then
		local entry = Transfers.getIn(assemblies, clipId, now)
		local expected = if entry ~= nil then entry.received else 0

		invalid(`offsets are strictly sequential — expected offset {expected}, got {args.offset}`)
	elseif result == "overflow" then
		invalid("the chunk overruns the declared payload size")
	elseif result == "empty" then
		invalid("data must decode to at least one byte")
	end

	return { ok = true }
end

--- `clipboard_paste_commit {clipId, to?, noSelect?}` → `{roots}`: verify the
--- assembled payload, deserialize it natively, and parent every root — the
--- recorded parent route by default, `to` overriding for all roots — inside
--- one ChangeHistory recording. The assembly is consumed either way.
function Clipboard.ops.clipboard_paste_commit(args: Args, ctx)
	local service = serializationService()
	local clipId = validateClipId(args.clipId)

	if args.to ~= nil and type(args.to) ~= "string" then
		invalid("to must be a Studio path string")
	end

	if args.noSelect ~= nil and type(args.noSelect) ~= "boolean" then
		invalid("noSelect must be a boolean")
	end

	local assembly = Transfers.takeIn(assemblies, clipId, os.clock())

	if assembly == nil then
		error(Error.new(Error.NOT_FOUND, `no paste assembly {clipId} (expired or committed) — restart from begin`), 0)
	end

	if assembly.received ~= assembly.total then
		invalid(`the assembly is incomplete — received {assembly.received} of {assembly.total} bytes`)
	end

	-- Pinned: verify before deserializing
	local digest = BufferHash.hex(assembly.bytes)

	if digest ~= assembly.sha256 then
		invalid(`the assembled payload failed SHA-256 verification (got {digest}) — restart from begin`)
	end

	local rootsMeta, rbxm = parseEnvelope(assembly.bytes)

	-- Resolve every destination before touching the place
	local explicitParent: Instance? = if args.to ~= nil then Paths.resolve(args.to) else nil

	local decodedOk, decoded = pcall(function()
		return service:DeserializeInstancesAsync(rbxm)
	end)

	if not decodedOk or type(decoded) ~= "table" or #decoded == 0 then
		pluginError(`native Studio deserialization failed: {tostring(decoded)}`)
	end

	local instances = decoded :: { Instance }

	if #instances ~= #rootsMeta then
		destroyAll(instances)
		pluginError(`the payload decoded {#instances} roots but its metadata describes {#rootsMeta}`)
	end

	local parents: { Instance } = {}

	for index, meta in rootsMeta do
		local parent: Instance? = explicitParent

		if parent == nil then
			parent = Routes.resolve(meta.route, game, INSTANCE_ADAPTER) :: Instance?
		end

		if parent == nil then
			destroyAll(instances)
			error(
				Error.new(
					Error.NOT_FOUND,
					`the original parent of root {index} ('{meta.name}') does not exist here — pass 'to' to choose a destination`
				),
				0
			)
		end

		parents[index] = parent :: Instance
	end

	-- One recording = one Studio undo for the whole paste
	local label = `WSync: paste ({#instances} root{if #instances == 1 then "" else "s"})`
	local recordingId = History.tryBeginBatchRecording(label)

	if recordingId == nil then
		destroyAll(instances)
		error(
			Error.new(
				Error.CONFLICT,
				"Studio refused to begin a change-history recording for the paste "
					.. "(another recording may already be in progress)"
			),
			0
		)
	end

	local applied, applyError = pcall(function()
		for index, instance in instances do
			instance.Archivable = true
			instance.Parent = parents[index]
		end
	end)

	if not applied then
		destroyAll(instances)
		History.finishBatchRecording(recordingId :: string, false)
		pluginError(`the paste was rolled back: {tostring(applyError)}`)
	end

	History.finishBatchRecording(recordingId :: string, true)

	if args.noSelect ~= true then
		pcall(function()
			Selection:Set(instances)
		end)
	end

	local paths: { string } = {}

	for _, instance in instances do
		table.insert(paths, (Paths.pathOf(instance)))
	end

	ctx.audit(WriteRules.shapeAudit("clipboard_paste_commit", if args.to ~= nil then args.to else "", {
		count = #instances,
	}))

	Log.trace(`clipboard_paste_commit: {#instances} roots pasted`)

	return { roots = paths }
end

return Clipboard
