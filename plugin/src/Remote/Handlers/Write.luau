--!strict
-- Mutating ops: `set`, `set_batch`, `new`, `rm`, `mv`, the attribute and tag
-- families, `call`, `eval`, `save`, plus the `open_in_editor` Studio-editor
-- control (set.json, new.json, rm.json, mv.json, attr.json, tag.json,
-- call.json, eval.json, save.json, open.json; Design §5.4, §6.4, §10.6).
--
-- Safety framing (Design §10.6): these ops are user-initiated escape hatches
-- routed through the daemon — never autonomous tools. Guardrails enforced
-- here, defense in depth with the CLI:
--   * `set Parent` requires forceParent:true and points at `mv`
--     (WriteRules.checkParentGuard);
--   * batches are pre-validated whole — any Parent write rejects the entire
--     batch before anything is touched;
--   * `mv` across top-level service boundaries requires force:true;
--   * script `Source` writes go through ScriptEditorService:UpdateSourceAsync
--     only — a failure is a failed op, never a raw `.Source` fallback (§6.1);
--   * every mutation brackets itself in a `WSync: <op> "<target>"` waypoint
--     (History.opWaypoint) unless an explicit transaction is open, and tags
--     its response meta with `mutation: true` plus a compact audit object for
--     the daemon's writes.log (ctx.audit -> WriteRules.shapeAudit).
--
-- `attr_ls` and `tag_ls` are read-only conveniences colocated with their
-- families; `open_in_editor` changes Studio UI state only (the select_set
-- precedent). Neither carries the mutation/audit tag.

local ScriptEditorService = game:GetService("ScriptEditorService")
local CollectionService = game:GetService("CollectionService")

local WSync = script:FindFirstAncestor("WSync")

local Log = require(WSync.Log)

local Error = require(script.Parent.Parent.Error)
local Codec = require(script.Parent.Parent.Codec)
local Paths = require(script.Parent.Parent.Paths)
local WriteRules = require(script.Parent.Parent.WriteRules)

local History = require(script.Parent.History)

type Args = { [string]: any }

local Write = {}

Write.LIMITS = {
	maxBatchWrites = WriteRules.LIMITS.maxBatchWrites,
	maxCallArgs = WriteRules.LIMITS.maxCallArgs,
	maxEvalSourceBytes = WriteRules.LIMITS.maxEvalSourceBytes,
	-- Readback values larger than this are reported as byte counts, not
	-- payloads, so set responses stay small.
	maxReadbackStringBytes = 16 * 1024,
}

--------------------------------------------------------------------------------
-- Helpers
--------------------------------------------------------------------------------

local function invalid(message: string): never
	error(Error.new(Error.INVALID_ARGUMENT, message), 0)
end

--- Write one already-decoded value onto a property, or raise a classified
--- RemoteError. Script `Source` goes through UpdateSourceAsync exclusively so
--- an open editor draft is updated too (Design §6.1); its failure is a failed
--- op (PLUGIN_ERROR), never a silent raw-`.Source` fallback.
local function applyProperty(instance: Instance, prop: string, decoded: any)
	-- An empty or slashed Name would leave the instance unaddressable by the
	-- `/`-separated path grammar (WriteRules.checkNameWrite)
	WriteRules.checkNameWrite(prop, decoded)

	if prop == "Source" and instance:IsA("LuaSourceContainer") then
		if type(decoded) ~= "string" then
			invalid("Source must be a string")
		end

		local ok, err = pcall(function()
			ScriptEditorService:UpdateSourceAsync(instance :: any, function()
				return decoded
			end)
		end)

		if not ok then
			error(Error.new(Error.PLUGIN_ERROR, `UpdateSourceAsync failed for '{instance.Name}': {tostring(err)}`), 0)
		end

		return
	end

	local ok, err = pcall(function()
		(instance :: any)[prop] = decoded
	end)

	if ok then
		return
	end

	local message = tostring(err)
	local code = WriteRules.classifyWriteFailure(message)

	if code == Error.NOT_FOUND then
		-- Normalize the unknown-property message to match the read path
		-- (Handlers/Inspect.get).
		error(Error.new(Error.NOT_FOUND, `property '{prop}' is not a member of class {instance.ClassName}`), 0)
	end

	error(Error.new(code, message), 0)
end

--- Post-write readback for set responses: the canonical value Studio kept
--- (normalization made visible). Returns the encoded value, or nil plus a
--- byte count for oversized strings. Best-effort — unreadable properties and
--- codec failures simply omit the readback.
local function encodeReadback(instance: Instance, prop: string): (any, number?)
	local okRead, value = pcall(function()
		return (instance :: any)[prop]
	end)

	if not okRead or value == nil then
		return nil, nil
	end

	if type(value) == "string" and #value > Write.LIMITS.maxReadbackStringBytes then
		return nil, #value
	end

	local okEncode, encoded = pcall(Codec.encode, value)

	if okEncode then
		return encoded, nil
	end

	return nil, nil
end

--- Dense-array check shared by `call` args (batch entries have their own in
--- WriteRules.validateBatch).
local function expectArray(value: any, label: string, maxEntries: number): { any }
	if type(value) ~= "table" then
		invalid(`{label} must be an array`)
	end

	local total = 0

	for _ in value do
		total += 1
	end

	if total ~= #value then
		invalid(`{label} must be a dense array`)
	end

	if total > maxEntries then
		invalid(`{label} exceeds the {maxEntries}-entry limit`)
	end

	return value
end

--- Scrub a caught runtime error into a bounded PLUGIN_ERROR (chunk/line
--- prefix stripped by Error.toPayload, so script paths never leak).
local function pluginErrorFrom(caught: any, context: string): never
	local payload = Error.toPayload(caught)
	error(Error.new(Error.PLUGIN_ERROR, `{context}: {payload.message}`), 0)
end

--------------------------------------------------------------------------------
-- set / set_batch
--------------------------------------------------------------------------------

Write.ops = {} :: { [string]: (args: Args, ctx: any) -> any }

--- `set {path, prop, value, forceParent?}` — one guarded property write.
function Write.ops.set(args: Args, ctx)
	local instance = Paths.resolve(args.path)
	local prop = WriteRules.validatePropName(args.prop)

	WriteRules.checkParentGuard(prop, args.forceParent == true)

	local decoded = Codec.decode(args.value)

	if prop == "Parent" and decoded ~= nil and typeof(decoded) ~= "Instance" then
		invalid("Parent must be an InstancePath tag or null")
	end

	applyProperty(instance, prop, decoded)

	local path = (Paths.pathOf(instance))

	History.opWaypoint("set", path)
	ctx.audit(WriteRules.shapeAudit("set", path, { class = instance.ClassName, prop = prop }))

	local response: { [string]: any } = {
		path = path,
		prop = prop,
	}

	if prop == "Source" then
		response.bytes = #(decoded :: string)
	else
		local readback, bytes = encodeReadback(instance, prop)
		response.value = readback
		response.bytes = bytes
	end

	return response
end

--- `set_batch {writes, keepGoing?, waypoint?}` — a pre-validated batch of
--- property writes. Pre-flight guards (shape, the Parent rejection) fail the
--- whole batch before anything is touched; `keepGoing` only relaxes
--- per-entry runtime failures. With `waypoint`, one ChangeHistory recording
--- brackets the batch so a single Studio undo reverses it (set.json) —
--- unless an explicit transaction is already open, whose recording brackets
--- everything instead.
function Write.ops.set_batch(args: Args, ctx)
	local writes = WriteRules.validateBatch(args.writes)
	local keepGoing = args.keepGoing == true

	local waypointName: string? = nil

	if args.waypoint ~= nil then
		if
			type(args.waypoint) ~= "string"
			or args.waypoint == ""
			or #args.waypoint > WriteRules.LIMITS.maxWaypointNameBytes
		then
			invalid(`waypoint must be a string of 1-{WriteRules.LIMITS.maxWaypointNameBytes} bytes`)
		end

		waypointName = args.waypoint
	end

	local recordingId: string? = nil

	if waypointName ~= nil and not History.isTransactionOpen() then
		recordingId = History.tryBeginBatchRecording(waypointName)

		if recordingId == nil then
			error(
				Error.new(
					Error.CONFLICT,
					"Studio refused to begin a change-history recording for the batch "
						.. "(another recording may already be in progress)"
				),
				0
			)
		end
	end

	local results: { { [string]: any } } = table.create(#writes)
	local applied = 0
	local failed = 0
	local stopped = false
	local firstAppliedPath: string? = nil

	for index, write in writes do
		if stopped then
			results[index] = { ok = false, skipped = true }
			continue
		end

		local okItem, caught = pcall(function()
			local instance = Paths.resolve(write.path)
			local decoded = Codec.decode(write.value)

			applyProperty(instance, write.prop, decoded)

			if firstAppliedPath == nil then
				firstAppliedPath = (Paths.pathOf(instance))
			end
		end)

		if okItem then
			applied += 1
			results[index] = { ok = true }
		else
			failed += 1
			results[index] = { ok = false, error = Error.toPayload(caught) }

			if not keepGoing then
				stopped = true
			end
		end
	end

	if recordingId ~= nil then
		-- Commit only when something changed; an all-failed batch cancels the
		-- recording so it never leaves an empty undo entry.
		History.finishBatchRecording(recordingId, applied > 0)
	elseif applied > 0 then
		History.opWaypoint("set_batch", `{applied} writes`)
	end

	if applied > 0 then
		ctx.audit(WriteRules.shapeAudit("set_batch", firstAppliedPath, { count = applied }))
	end

	return {
		results = results,
		total = #writes,
		applied = applied,
		failed = failed,
		stopped = stopped,
	}
end

--------------------------------------------------------------------------------
-- new / rm / mv
--------------------------------------------------------------------------------

--- `new {path(parent), class, name?, props?}` — create an instance under a
--- parent path. Atomic: any failure destroys the half-built instance and
--- fails the op. Props apply before parenting (one clean ChildAdded), except
--- `Source`, which needs the instance in the DataModel for
--- UpdateSourceAsync and applies last (the Ro-Sync ordering).
function Write.ops.new(args: Args, ctx)
	local parent = Paths.resolve(args.path)

	if type(args.class) ~= "string" or args.class == "" then
		invalid("class must be a non-empty string")
	end

	local className: string = args.class
	local okNew, created = pcall(Instance.new, className)

	if not okNew then
		invalid(`class '{className}' cannot be created: {tostring(created)}`)
	end

	local instance = created :: Instance

	-- Everything below must clean up the unparented instance on failure
	local function rollback(caught: any): never
		pcall(function()
			instance:Destroy()
		end)

		error(caught, 0)
	end

	local pendingSource: string? = nil

	if args.props ~= nil then
		if type(args.props) ~= "table" then
			rollback(Error.new(Error.INVALID_ARGUMENT, "props must be an object keyed by property name"))
		end

		-- Sorted for deterministic application order
		local names = {}

		for key in args.props do
			if type(key) ~= "string" then
				rollback(Error.new(Error.INVALID_ARGUMENT, "props must be an object keyed by property name"))
			end

			table.insert(names, key)
		end

		table.sort(names)

		for _, prop in names do
			local okProp, caught = pcall(function()
				WriteRules.validatePropName(prop, `props['{prop}']`)

				if prop == "Parent" then
					invalid("props must not contain Parent: the parent comes from the path argument")
				end

				local decoded = Codec.decode(args.props[prop])

				if prop == "Source" and instance:IsA("LuaSourceContainer") then
					if type(decoded) ~= "string" then
						invalid("Source must be a string")
					end

					pendingSource = decoded
				else
					applyProperty(instance, prop, decoded)
				end
			end)

			if not okProp then
				rollback(caught)
			end
		end
	end

	if args.name ~= nil then
		if type(args.name) ~= "string" or args.name == "" then
			rollback(Error.new(Error.INVALID_ARGUMENT, "name must be a non-empty string"))
		end

		local okName, caught = pcall(function()
			applyProperty(instance, "Name", args.name)
		end)

		if not okName then
			rollback(caught)
		end
	end

	local okParent, caughtParent = pcall(function()
		applyProperty(instance, "Parent", parent)
	end)

	if not okParent then
		rollback(caughtParent)
	end

	if pendingSource ~= nil then
		local okSource, caughtSource = pcall(function()
			applyProperty(instance, "Source", pendingSource)
		end)

		if not okSource then
			rollback(caughtSource)
		end
	end

	local path = (Paths.pathOf(instance))

	History.opWaypoint("new", path)
	ctx.audit(WriteRules.shapeAudit("new", path, { class = instance.ClassName }))

	return {
		path = path,
		class = instance.ClassName,
		name = instance.Name,
	}
end

--- `rm {path}` — destroy one instance.
function Write.ops.rm(args: Args, ctx)
	local instance = Paths.resolve(args.path)

	if instance == game then
		invalid("cannot destroy the DataModel")
	end

	-- Captured before Destroy: afterwards the instance is unrooted and its
	-- path is gone.
	local path = (Paths.pathOf(instance))
	local className = instance.ClassName

	local ok, err = pcall(function()
		instance:Destroy()
	end)

	if not ok then
		local message = tostring(err)
		error(Error.new(WriteRules.classifyWriteFailure(message), message), 0)
	end

	History.opWaypoint("rm", path)
	ctx.audit(WriteRules.shapeAudit("rm", path, { class = className }))

	return { path = path, class = className, destroyed = true }
end

--- `mv {from, to, force?}` — reparent to a destination parent path.
--- Cross-service moves require force (mv.json: an accidental
--- Workspace -> ServerStorage drop usually means losing replication).
--- Duplicate names at the destination are allowed (Roblox allows them), but
--- the resulting path is reported and flagged when it becomes ambiguous.
function Write.ops.mv(args: Args, ctx)
	local instance = Paths.resolve(args.from)

	if instance == game then
		invalid("cannot reparent the DataModel")
	end

	local newParent = Paths.resolve(args.to)

	if newParent == instance or newParent:IsDescendantOf(instance) then
		invalid("the destination parent is inside the moved instance")
	end

	local fromPath = (Paths.pathOf(instance))
	local toParentPath = (Paths.pathOf(newParent))
	local crosses, fromService, toService = WriteRules.crossesServiceBoundary(fromPath, toParentPath)

	if crosses and args.force ~= true then
		local destination = if toService == "" then "the DataModel root" else `'{toService}'`

		error(
			Error.new(
				Error.PERMISSION_REQUIRED,
				`moving '{fromPath}' from '{fromService}' to {destination} crosses a top-level service boundary; `
					.. "pass force:true (--force) only when the cross-service move is intentional"
			),
			0
		)
	end

	applyProperty(instance, "Parent", newParent)

	local resultPath = (Paths.pathOf(instance))

	History.opWaypoint("mv", fromPath)
	ctx.audit(WriteRules.shapeAudit("mv", fromPath, { class = instance.ClassName, to = resultPath }))

	local response: { [string]: any } = {
		from = fromPath,
		path = resultPath,
		parent = toParentPath,
	}

	-- A same-name sibling makes the reported path resolve ambiguously
	-- (FindFirstChild picks the first match — the documented v1 limitation).
	local okSiblings = pcall(function()
		for _, sibling in newParent:GetChildren() do
			if sibling ~= instance and sibling.Name == instance.Name then
				response.duplicateSiblings = true
				break
			end
		end
	end)

	if not okSiblings then
		response.duplicateSiblings = nil
	end

	return response
end

--------------------------------------------------------------------------------
-- Attributes (attr.json)
--------------------------------------------------------------------------------

local function validateAttrName(name: any): string
	if type(name) ~= "string" or name == "" or #name > WriteRules.LIMITS.maxAttrNameBytes then
		invalid(`name must be a string of 1-{WriteRules.LIMITS.maxAttrNameBytes} bytes`)
	end

	return name
end

--- `set_attr {path, name, value}` — set one attribute (tagged-value codec).
function Write.ops.set_attr(args: Args, ctx)
	local instance = Paths.resolve(args.path)
	local name = validateAttrName(args.name)
	local decoded = Codec.decode(args.value)

	if decoded == nil then
		invalid("value must be provided; use rm_attr to clear an attribute")
	end

	if typeof(decoded) == "Instance" then
		invalid("attributes cannot hold Instances")
	end

	local ok, err = pcall(function()
		instance:SetAttribute(name, decoded)
	end)

	if not ok then
		-- SetAttribute failures are argument problems (bad name charset, the
		-- RBX prefix, an unsupported value type), not permissions.
		invalid(`SetAttribute failed: {tostring(err)}`)
	end

	local path = (Paths.pathOf(instance))

	History.opWaypoint("set_attr", path)
	ctx.audit(WriteRules.shapeAudit("set_attr", path, { class = instance.ClassName, prop = name }))

	return { path = path, name = name }
end

--- `rm_attr {path, name}` — clear one attribute; NOT_FOUND when it isn't set.
function Write.ops.rm_attr(args: Args, ctx)
	local instance = Paths.resolve(args.path)
	local name = validateAttrName(args.name)

	local okRead, existing = pcall(function()
		return instance:GetAttribute(name)
	end)

	if not okRead or existing == nil then
		error(Error.new(Error.NOT_FOUND, `attribute '{name}' is not set on '{args.path}'`), 0)
	end

	local ok, err = pcall(function()
		instance:SetAttribute(name, nil)
	end)

	if not ok then
		error(Error.new(Error.PLUGIN_ERROR, `SetAttribute failed: {tostring(err)}`), 0)
	end

	local path = (Paths.pathOf(instance))

	History.opWaypoint("rm_attr", path)
	ctx.audit(WriteRules.shapeAudit("rm_attr", path, { class = instance.ClassName, prop = name }))

	return { path = path, name = name, cleared = true }
end

--- `attr_ls {path}` — list attributes, tagged-encoded (read-only).
function Write.ops.attr_ls(args: Args, _ctx)
	local instance = Paths.resolve(args.path)

	local ok, attributes = pcall(function()
		return instance:GetAttributes()
	end)

	if not ok or type(attributes) ~= "table" then
		error(Error.new(Error.PLUGIN_ERROR, "GetAttributes failed"), 0)
	end

	local budget = Codec.newBudget()
	local out: { [string]: any } = {}
	local count = 0

	for name, value in attributes do
		local okEncode, encoded = pcall(Codec.encode, value, budget)

		if okEncode then
			out[tostring(name)] = encoded
		else
			local reason = if Error.is(encoded) then encoded.message else tostring(encoded)
			out[tostring(name)] = { __type = "Skipped", reason = Error.truncateMessage(reason) }
		end

		count += 1
	end

	local response: { [string]: any } = {
		path = (Paths.pathOf(instance)),
		count = count,
	}

	-- Omitted when empty: JSONEncode renders an empty Luau table as `[]`,
	-- which would flip the JSON type of the attributes map.
	if count > 0 then
		response.attributes = out
	end

	return response
end

--------------------------------------------------------------------------------
-- CollectionService tags (tag.json)
--------------------------------------------------------------------------------

local function validateTag(tag: any): string
	if type(tag) ~= "string" or tag == "" or #tag > WriteRules.LIMITS.maxTagBytes then
		invalid(`tag must be a string of 1-{WriteRules.LIMITS.maxTagBytes} bytes`)
	end

	return tag
end

--- `add_tag {path, tag}` — idempotent: tagging an already-tagged instance
--- reports `already = true` and mutates nothing.
function Write.ops.add_tag(args: Args, ctx)
	local instance = Paths.resolve(args.path)
	local tag = validateTag(args.tag)
	local path = (Paths.pathOf(instance))

	local okHas, already = pcall(function()
		return CollectionService:HasTag(instance, tag)
	end)

	if okHas and already == true then
		return { path = path, tag = tag, added = false, already = true }
	end

	local ok, err = pcall(function()
		CollectionService:AddTag(instance, tag)
	end)

	if not ok then
		error(Error.new(Error.PLUGIN_ERROR, `AddTag failed: {tostring(err)}`), 0)
	end

	History.opWaypoint("add_tag", path)
	ctx.audit(WriteRules.shapeAudit("add_tag", path, { class = instance.ClassName, prop = tag }))

	return { path = path, tag = tag, added = true }
end

--- `rm_tag {path, tag}` — NOT_FOUND when the tag isn't present.
function Write.ops.rm_tag(args: Args, ctx)
	local instance = Paths.resolve(args.path)
	local tag = validateTag(args.tag)

	local okHas, has = pcall(function()
		return CollectionService:HasTag(instance, tag)
	end)

	if not okHas or has ~= true then
		error(Error.new(Error.NOT_FOUND, `tag '{tag}' is not present on '{args.path}'`), 0)
	end

	local ok, err = pcall(function()
		CollectionService:RemoveTag(instance, tag)
	end)

	if not ok then
		error(Error.new(Error.PLUGIN_ERROR, `RemoveTag failed: {tostring(err)}`), 0)
	end

	local path = (Paths.pathOf(instance))

	History.opWaypoint("rm_tag", path)
	ctx.audit(WriteRules.shapeAudit("rm_tag", path, { class = instance.ClassName, prop = tag }))

	return { path = path, tag = tag, removed = true }
end

--- `tag_ls {path}` — sorted tag list (read-only).
function Write.ops.tag_ls(args: Args, _ctx)
	local instance = Paths.resolve(args.path)

	local ok, tags = pcall(function()
		return CollectionService:GetTags(instance)
	end)

	if not ok or type(tags) ~= "table" then
		error(Error.new(Error.PLUGIN_ERROR, "GetTags failed"), 0)
	end

	local out = table.create(#tags)

	for _, tag in tags do
		table.insert(out, tostring(tag))
	end

	table.sort(out)

	return {
		path = (Paths.pathOf(instance)),
		tags = out,
		count = #out,
	}
end

--------------------------------------------------------------------------------
-- call / eval
--------------------------------------------------------------------------------

--- `call {path, method, args?}` — invoke one method; args decoded through the
--- value codec, every return value codec-encoded (multi-return as an array).
--- Methods with side effects count as writes (call.json), so every call is
--- bracketed and audited — a spurious waypoint after a pure read is harmless.
function Write.ops.call(args: Args, ctx)
	local instance = Paths.resolve(args.path)
	local method = WriteRules.validatePropName(args.method, "method")

	local okLookup, member = pcall(function()
		return (instance :: any)[method]
	end)

	if not okLookup then
		error(Error.new(Error.NOT_FOUND, `'{method}' is not a member of class {instance.ClassName}`), 0)
	end

	if type(member) ~= "function" then
		invalid(`'{method}' is a {typeof(member)} on class {instance.ClassName}, not a method`)
	end

	local decodedArgs = {}
	local argCount = 0

	if args.args ~= nil then
		local rawArgs = expectArray(args.args, "args", WriteRules.LIMITS.maxCallArgs)
		local budget = Codec.newBudget()

		argCount = #rawArgs

		for index = 1, argCount do
			decodedArgs[index] = Codec.decode(rawArgs[index], budget)
		end
	end

	local packed = table.pack(pcall(member, instance, table.unpack(decodedArgs, 1, argCount)))

	if not packed[1] then
		pluginErrorFrom(packed[2], `'{method}' raised`)
	end

	local path = (Paths.pathOf(instance))
	local returnCount = packed.n - 1

	local budget = Codec.newBudget()
	local results = table.create(returnCount)

	for index = 1, returnCount do
		results[index] = Codec.encode(packed[index + 1], budget)
	end

	History.opWaypoint("call", `{path}.{method}`)
	ctx.audit(WriteRules.shapeAudit("call", path, { class = instance.ClassName, prop = method }))

	local response: { [string]: any } = {
		path = path,
		method = method,
		count = returnCount,
	}

	-- Omitted when empty (JSONEncode `[]`-vs-`{}` ambiguity is moot for
	-- arrays, but a zero-return call reads cleaner as just `count = 0`)
	if returnCount > 0 then
		response.results = results
	end

	return response
end

export type Returns = { n: number, [number]: any }

--- Compile and run one Luau source string in the plugin's real Studio
--- environment — the `eval` trust level (eval.json: the broadest escape
--- hatch, Studio plugin security, the same `loadstring` the Executor's
--- `wsync exec` path uses, so ordinary globals like `game`, `require`, and
--- `Vector2` are available without a custom sandbox). Shared with the
--- transmit ops (transmit.json: "treat it like `wsync eval`") so both escape
--- hatches carry exactly one trust level and one classification: compile
--- errors are INVALID_ARGUMENT, runtime errors PLUGIN_ERROR with the message
--- scrubbed and truncated. Returns the values the chunk returned, packed.
function Write.runSource(source: any, chunkName: string, context: string): Returns
	if type(source) ~= "string" or source == "" then
		invalid("source must be a non-empty string")
	end

	if #source > WriteRules.LIMITS.maxEvalSourceBytes then
		invalid(`source exceeds the {WriteRules.LIMITS.maxEvalSourceBytes}-byte limit`)
	end

	-- pcall'd so a Studio build without plugin loadstring degrades to a
	-- typed error instead of a crash.
	local okLoad, fn, compileError = pcall(loadstring, source, chunkName)

	if not okLoad then
		error(Error.new(Error.PLUGIN_ERROR, `loadstring is not available: {tostring(fn)}`), 0)
	end

	if fn == nil then
		invalid(`compile error: {tostring(compileError)}`)
	end

	local packed = table.pack(pcall(fn :: any))

	if not packed[1] then
		pluginErrorFrom(packed[2], context)
	end

	-- Drop the pcall status so callers see exactly the chunk's return values
	return (table.pack(table.unpack(packed, 2, packed.n)) :: any) :: Returns
end

--- `eval {source}` — run Luau in the plugin environment and codec-encode
--- every return value as an array (eval.json).
function Write.ops.eval(args: Args, ctx)
	local returns = Write.runSource(args.source, "wsync-eval", "eval raised")

	local returnCount = returns.n
	local budget = Codec.newBudget()
	local results = table.create(returnCount)

	for index = 1, returnCount do
		results[index] = Codec.encode(returns[index], budget)
	end

	History.opWaypoint("eval", "")
	ctx.audit(WriteRules.shapeAudit("eval", ""))

	local response: { [string]: any } = {
		count = returnCount,
	}

	if returnCount > 0 then
		response.results = results
	end

	return response
end

--------------------------------------------------------------------------------
-- save
--------------------------------------------------------------------------------

--- `save` — ask Studio to save the current place. Per save.json the save is
--- asynchronous and the op acks when Studio accepts the request: DataModel
--- Save() can take seconds, which would blow the 5 s remote-op timeout, so
--- it runs detached and any failure surfaces in the Studio output.
function Write.ops.save(_args: Args, ctx)
	local okLookup, saveFn = pcall(function()
		return (game :: any).Save
	end)

	if not okLookup or type(saveFn) ~= "function" then
		error(Error.new(Error.PLUGIN_ERROR, "this Studio build does not expose DataModel Save()"), 0)
	end

	task.spawn(function()
		local ok, err = pcall(function()
			(game :: any):Save()
		end)

		if not ok then
			Log.warn("Async place save failed:", err)
		end
	end)

	ctx.audit(WriteRules.shapeAudit("save", ""))

	return { requested = true }
end

--------------------------------------------------------------------------------
-- open_in_editor (Studio control — not a mutation)
--------------------------------------------------------------------------------

--- `open_in_editor {path, line?}` — open a script document in the Studio
--- script editor, optionally placing the cursor at a line (Design §5.4:
--- Argon's legacy `/open` behavior folded into the op surface). Line
--- positioning is best-effort: a failure still reports the document opened.
function Write.ops.open_in_editor(args: Args, _ctx)
	local instance = Paths.resolve(args.path)

	if not instance:IsA("LuaSourceContainer") then
		invalid(`'{args.path}' is a {instance.ClassName}, not a script`)
	end

	local line: number? = nil

	if args.line ~= nil then
		if type(args.line) ~= "number" or args.line < 1 then
			invalid("line must be a positive number")
		end

		line = math.floor(args.line)
	end

	local okOpen, success, openError = pcall(function()
		return (ScriptEditorService :: any):OpenScriptDocumentAsync(instance)
	end)

	if not okOpen then
		error(Error.new(Error.PLUGIN_ERROR, `OpenScriptDocumentAsync failed: {tostring(success)}`), 0)
	end

	if success == false then
		error(Error.new(Error.PLUGIN_ERROR, `Studio could not open the document: {tostring(openError)}`), 0)
	end

	local positioned = false

	if line ~= nil then
		pcall(function()
			local document = (ScriptEditorService :: any):FindScriptDocument(instance)

			if document ~= nil then
				local lineCount = document:GetLineCount()
				local target = math.clamp(line :: number, 1, lineCount)

				document:ForceSetSelectionAsync(target, 1)
				positioned = true
			end
		end)
	end

	local response: { [string]: any } = {
		path = (Paths.pathOf(instance)),
		opened = true,
	}

	if line ~= nil then
		response.line = line
		response.positioned = positioned
	end

	return response
end

return Write
