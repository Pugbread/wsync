--!strict
-- Sync-state ops: `sync_barrier` (wait-synced.json) and the read-only
-- `verify_sources` / `verify_tree` pair (verify.json).
--
-- `sync_barrier` is the Studio end of the daemon's disk -> Studio barrier
-- (server/synced.rs). The transport queues it behind the sync frames it
-- already holds, exactly like the transaction bracket (Transport/WebSocket),
-- so Core's sync loop applies every earlier frame before it runs; during a
-- connect-time decision wait Core stashes it with those frames and replays
-- it in order. Script sources land through UpdateSourceAsync on their own
-- threads, so the handler then waits out the writes still in flight
-- (Lib/Dom/SourceWrites) — its answer means "applied", not "received".
--
-- `verify_sources` answers per daemon ref: is the instance in Studio (the
-- live tree first, then the Studio path the daemon sent), does its editor
-- text — ScriptEditorService first, `.Source` as the fallback, CRLF folded
-- to LF like the daemon — hash to the disk text, and does an open editor
-- draft differ from the committed Source. `verify_tree` walks the synced
-- roots under the same scope and exclusion rules as the bulk pull and lists
-- Studio's scripts plus its same-named siblings, paged. Refs resolve through
-- Core's live tree via the Bulk session bridge.

local ScriptEditorService = game:GetService("ScriptEditorService")

local WSync = script:FindFirstAncestor("WSync")

local SyncScope = require(WSync.SyncScope)
local Sha256 = require(WSync.Lib.Sha256)
local SourceWrites = require(WSync.Lib.Dom.SourceWrites)
local Hex = require(WSync.Transport.Hex)

local Error = require(script.Parent.Parent.Error)
local Paths = require(script.Parent.Parent.Paths)
local Bulk = require(script.Parent.Bulk)

type Args = { [string]: any }

local Sync = {}

Sync.LIMITS = {
	barrierDefaultWaitMs = 10000,
	barrierMaxWaitMs = 600000,
	verifyMaxEntries = 256,
	treeDefaultLimit = 2000,
	treeMaxLimit = 5000,
	treeMaxNodes = 200000,
	maxDuplicateGroups = 500,
	-- Yield after hashing this many bytes or walking this many instances, so
	-- a large chunk never runs into Studio's script execution timeout
	hashYieldBytes = 1024 * 1024,
	walkYieldNodes = 10000,
}

--------------------------------------------------------------------------------
-- Helpers
--------------------------------------------------------------------------------

--- The engine's line-ending rule (daemon `core::conflict::content`)
local function normalize(text: string): string
	if string.find(text, "\r", 1, true) then
		return (string.gsub(text, "\r\n", "\n"))
	end

	return text
end

local function readEditor(instance: Instance): string?
	local ok, source = pcall(function()
		return ScriptEditorService:GetEditorSource(instance :: any)
	end)

	return if ok and type(source) == "string" then source else nil
end

local function readCommitted(instance: Instance): string?
	local ok, source = pcall(function()
		return (instance :: any).Source
	end)

	return if ok and type(source) == "string" then source else nil
end

--- Tracked but deleted (or parented out of the DataModel) is not in Studio
local function isLive(instance: Instance): boolean
	local ok, live = pcall(function()
		return instance:IsDescendantOf(game)
	end)

	return ok and live == true
end

local function requireSession(): Bulk.Session
	local session = Bulk.activeSession()

	if session == nil then
		error(Error.new(Error.CONFLICT, "no active sync session — connect the plugin before verifying"), 0)
	end

	return session :: Bulk.Session
end

local function optionalCount(value: any, name: string, default: number, max: number): number
	if value == nil then
		return default
	end

	if type(value) ~= "number" or value < 0 or value % 1 ~= 0 then
		error(Error.new(Error.INVALID_ARGUMENT, `{name} must be a non-negative integer`), 0)
	end

	return math.min(value, max)
end

--------------------------------------------------------------------------------
-- Ops
--------------------------------------------------------------------------------

Sync.ops = {} :: { [string]: (args: Args, ctx: any) -> any }

--- `sync_barrier {waitMs?}` → `{applied, settled, sourceWritesAwaited,
--- pendingSourceWrites, waitedMs}`. Runs after every sync frame queued before
--- it was applied; `settled` is false only when script source writes were
--- still in flight after `waitMs`.
function Sync.ops.sync_barrier(args: Args, _ctx)
	local waitMs = optionalCount(args.waitMs, "waitMs", Sync.LIMITS.barrierDefaultWaitMs, Sync.LIMITS.barrierMaxWaitMs)
	local startedAt = os.clock()
	local awaited = SourceWrites.pending()
	local settled = SourceWrites.waitIdle(waitMs / 1000)

	return {
		applied = true,
		settled = settled,
		sourceWritesAwaited = awaited,
		pendingSourceWrites = SourceWrites.pending(),
		waitedMs = math.round((os.clock() - startedAt) * 1000),
	}
end

--- `verify_sources {entries: [{ref, path?, bytes?, sha256?}]}` →
--- `{results: [{ref, found, tracked, class?, path?, bytes?, sha256?, match?,
--- draft?: {bytes}}]}` in entry order. An entry without `sha256` is a
--- presence check only.
function Sync.ops.verify_sources(args: Args, _ctx)
	local session = requireSession()
	local entries = args.entries

	if type(entries) ~= "table" then
		error(Error.new(Error.INVALID_ARGUMENT, "entries must be an array"), 0)
	end

	if #entries > Sync.LIMITS.verifyMaxEntries then
		error(Error.new(Error.INVALID_ARGUMENT, `at most {Sync.LIMITS.verifyMaxEntries} entries per call`), 0)
	end

	local results = table.create(#entries)
	local hashedSinceYield = 0

	for index, entry in entries do
		if type(entry) ~= "table" then
			error(Error.new(Error.INVALID_ARGUMENT, `entries[{index}] must be an object`), 0)
		end

		local ref = Hex.decode(entry.ref)

		if ref == nil then
			error(Error.new(Error.INVALID_ARGUMENT, `entries[{index}].ref must be a 32-char hex string`), 0)
		end

		local result: { [string]: any } = {
			ref = string.lower(entry.ref),
			found = false,
			tracked = false,
		}

		local instance = session.getInstanceForRef(ref :: buffer)

		if instance ~= nil and not isLive(instance) then
			instance = nil
		end

		if instance ~= nil then
			result.tracked = true
		elseif type(entry.path) == "string" and entry.path ~= "" then
			local ok, resolved = pcall(Paths.tryResolve, entry.path)

			if ok and resolved ~= nil then
				instance = resolved
			end
		end

		if instance ~= nil then
			result.found = true
			result.class = instance.ClassName
			result.path = (Paths.pathOf(instance))

			if type(entry.sha256) == "string" then
				if instance:IsA("LuaSourceContainer") then
					local editor = readEditor(instance)
					local committed = readCommitted(instance)
					local text = normalize(editor or committed or "")

					result.bytes = #text

					if type(entry.bytes) == "number" and entry.bytes ~= #text then
						-- Different sizes already decide it; skip the hash
						result.match = false
					else
						local sha256 = Sha256.hash(text)

						result.sha256 = sha256
						result.match = sha256 == string.lower(entry.sha256)
						hashedSinceYield += #text

						if hashedSinceYield >= Sync.LIMITS.hashYieldBytes then
							hashedSinceYield = 0
							task.wait()
						end
					end

					if editor ~= nil and committed ~= nil then
						local committedText = normalize(committed)

						if committedText ~= text then
							result.draft = { bytes = #committedText }
						end
					end
				else
					-- The daemon holds a script here; Studio holds something else
					result.match = false
				end
			end
		end

		results[index] = result
	end

	return { results = results }
end

--- `verify_tree {roots: [{ref, path?}], offset?, limit?}` → `{scripts:
--- [{path, class, ref?, parentRef?}], total, nodes, truncated, missingRoots,
--- duplicates}`. `scripts` is the `[offset, offset + limit)` slice of a
--- deterministic walk; `duplicates` (same-named siblings) is only filled on
--- the first page. On code scope every same-named in-scope group counts; on
--- full scope only groups with a tracked or code-class member, so ordinary
--- Workspace clutter is not reported as sync drift.
function Sync.ops.verify_tree(args: Args, _ctx)
	local session = requireSession()
	local roots = args.roots

	if type(roots) ~= "table" then
		error(Error.new(Error.INVALID_ARGUMENT, "roots must be an array"), 0)
	end

	local offset = optionalCount(args.offset, "offset", 0, math.huge)
	local limit = optionalCount(args.limit, "limit", Sync.LIMITS.treeDefaultLimit, Sync.LIMITS.treeMaxLimit)
	local codeScope = SyncScope.isCode()

	local scripts = {}
	local duplicates = {}
	local missingRoots = {}
	local nodes = 0
	local truncated = false

	local function refHex(instance: Instance): string?
		local ref = session.getRefForInstance(instance)

		return if ref ~= nil then Hex.encode(ref) else nil
	end

	local function inScope(instance: Instance): boolean
		if codeScope and not SyncScope.isCodeClass(instance.ClassName) then
			return false
		end

		return not session.isExcluded(instance)
	end

	-- Iterative depth-first walk: `{instance, nearest tracked ancestor ref}`
	local stack: { { any } } = {}

	for index = #roots, 1, -1 do
		local root = roots[index]
		local ref = if type(root) == "table" then Hex.decode(root.ref) else nil
		local instance = if ref ~= nil then session.getInstanceForRef(ref) else nil

		if instance ~= nil and isLive(instance) then
			table.insert(stack, { instance, nil })
		else
			table.insert(missingRoots, if type(root) == "table" then tostring(root.ref) else tostring(root))
		end
	end

	while #stack > 0 do
		local frame = table.remove(stack) :: { any }
		local instance = frame[1] :: Instance
		local parentRef = frame[2] :: string?

		nodes += 1

		if nodes > Sync.LIMITS.treeMaxNodes then
			truncated = true
			break
		end

		if nodes % Sync.LIMITS.walkYieldNodes == 0 then
			task.wait()
		end

		local ref = refHex(instance)

		if instance:IsA("LuaSourceContainer") then
			table.insert(scripts, {
				path = (Paths.pathOf(instance)),
				class = instance.ClassName,
				ref = ref,
				parentRef = parentRef,
			})
		end

		local children = {}
		local byName: { [string]: { Instance } } = {}
		local order = {}

		for _, child in instance:GetChildren() do
			if inScope(child) then
				table.insert(children, child)

				local group = byName[child.Name]

				if group then
					table.insert(group, child)
				else
					byName[child.Name] = { child }
					table.insert(order, child.Name)
				end
			end
		end

		for _, name in order do
			local group = byName[name]

			if #group > 1 and #duplicates < Sync.LIMITS.maxDuplicateGroups then
				local relevant = codeScope
				local classes = {}

				for _, member in group do
					table.insert(classes, member.ClassName)

					if SyncScope.isCodeClass(member.ClassName) or session.getRefForInstance(member) ~= nil then
						relevant = true
					end
				end

				if relevant then
					table.insert(duplicates, {
						parent = (Paths.pathOf(instance)),
						name = name,
						count = #group,
						classes = classes,
					})
				end
			end
		end

		-- Reverse push keeps GetChildren order for the depth-first visit
		local nextParent = ref or parentRef

		for index = #children, 1, -1 do
			table.insert(stack, { children[index], nextParent })
		end
	end

	local page = {}

	for index = offset + 1, math.min(offset + limit, #scripts) do
		table.insert(page, scripts[index])
	end

	return {
		scripts = page,
		total = #scripts,
		nodes = nodes,
		truncated = truncated,
		missingRoots = missingRoots,
		duplicates = if offset == 0 then duplicates else {},
	}
end

return Sync
