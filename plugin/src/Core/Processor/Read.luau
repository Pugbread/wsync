local ScriptEditorService = game:GetService("ScriptEditorService")

local WSync = script:FindFirstAncestor("WSync")

local Dom = require(WSync.Lib.Dom)

local Types = require(WSync.Types)
local Log = require(WSync.Log)
local Config = require(WSync.Config)
local SyncScope = require(WSync.SyncScope)

local equals = require(WSync.Helpers.equals)
local generateRef = require(WSync.Helpers.generateRef)

local Snapshot = require(script.Parent.Parent.Snapshot)
local Changes = require(script.Parent.Parent.Changes)
local Error = require(script.Parent.Parent.Error)

--- Code scope (Design §7.0): exactly the four allowlisted classes sync back —
--- `Folder` always (even scriptless: the old `isScriptRelated` gate is what
--- this replaces), scripts always, everything else never enters a push frame.
--- Full scope has no class gate (the retired `OnlyCodeMode` derives from the
--- scope and is false there).
local function passesScope(instance: Instance): boolean
	return not SyncScope.isCode() or SyncScope.isCodeClass(instance.ClassName)
end

local function validateValue(value: any, propertyType: string?): boolean
	if propertyType == "BinaryString" then
		return true
	end

	local valueType = type(value)

	if valueType == "string" then
		return utf8.len(value) ~= nil
	end

	if valueType == "table" then
		for k, v in value do
			if not validateValue(k) or not validateValue(v) then
				return false
			end
		end
	end

	return true
end

--- Editor-aware source read (the Bulk/Inspect convention): an open editor
--- draft is what the user is looking at, so it is what decides. Falls back
--- to the raw property; nil when the source cannot be read at all.
local function readSource(instance: Instance): string?
	local ok, source = pcall(function()
		return ScriptEditorService:GetEditorSource(instance :: any)
	end)

	if ok and type(source) == "string" then
		return source
	end

	local rawOk, raw = pcall(function()
		return (instance :: any).Source
	end)

	return if rawOk and type(raw) == "string" then raw else nil
end

local ReadProcessor = {}
ReadProcessor.__index = ReadProcessor

function ReadProcessor.new(tree)
	-- Instances kept out of sync by an exclusion marker (SyncScope: the
	-- `AvoidSync` attribute or `--!ignoresync`), weakly keyed so a destroyed
	-- instance never pins the set. Membership is what lets an instance
	-- re-enter sync when its marker is removed: only one seen leaving (or
	-- never let in) is a candidate, so an instance that is untracked for any
	-- other reason — a duplicate-name sibling, a KeepUnknowns subtree — keeps
	-- today's "unknown instance" behavior on its edits
	local excluded: { [Instance]: boolean } = {}
	setmetatable(excluded, { __mode = "k" })

	-- Children the write processor moved into a replacement instance during a
	-- class swap (WriteProcessor:swapInstance), each keyed to the parent it
	-- was moved to. Their removal from the old instance is the swap's own
	-- echo, not a Studio edit (`expectMove`). Weakly keyed like `excluded`
	local moving: { [Instance]: Instance } = {}
	setmetatable(moving, { __mode = "k" })

	return setmetatable({
		tree = tree,
		excluded = excluded,
		moving = moving,
	}, ReadProcessor)
end

--- The write processor is about to move `instance` under `parent` while it
--- swaps that instance's parent for one of a new class (a Folder gaining an
--- `init.luau`, a script changing suffix). Studio reports the move as the
--- child leaving the old instance, and read as an edit that is a deletion —
--- the child's files would go from disk, and nothing would bring them back,
--- because the swap moves children before the new instance is watched. The
--- mark is consumed by the first removal seen for the instance, and only
--- suppresses it while the instance still sits under `parent`: a real delete
--- or move that happens to come first is never swallowed.
function ReadProcessor:expectMove(instance: Instance, parent: Instance)
	self.moving[instance] = parent
end

--- Does this instance carry the `--!ignoresync` directive (SyncScope)? Only
--- scripts can. A script whose source cannot be read stays in sync rather
--- than silently dropping out of it.
function ReadProcessor.isIgnoredScript(instance: Instance): boolean
	if not instance:IsA("LuaSourceContainer") then
		return false
	end

	local source = readSource(instance)

	return source ~= nil and SyncScope.sourceIgnoresSync(source)
end

--- Does this instance carry the `AvoidSync` attribute (SyncScope)? Any class
--- can. Read through pcall — a few engine classes refuse attribute access —
--- and an unreadable attribute keeps the instance in sync.
function ReadProcessor.hasAvoidSync(instance: Instance): boolean
	local ok, value = pcall(function()
		return instance:GetAttribute(SyncScope.AVOID_SYNC_ATTRIBUTE)
	end)

	return ok and SyncScope.attributeAvoidsSync(value)
end

--- The marker keeping this instance in Studio — the attribute name or the
--- directive, for log lines — or nil when it is in sync. The attribute is
--- read first: it costs one property read, the directive reads a Source.
function ReadProcessor.exclusionOf(instance: Instance): string?
	if ReadProcessor.hasAvoidSync(instance) then
		return SyncScope.AVOID_SYNC_ATTRIBUTE
	end

	if ReadProcessor.isIgnoredScript(instance) then
		return SyncScope.IGNORE_DIRECTIVE
	end

	return nil
end

--- Is this instance kept in Studio by either marker? The one predicate every
--- sync path asks: `onAdd`, hydrate, the connect diff, the arrival of daemon
--- changes and the bulk `read_subtree` walk.
function ReadProcessor.isExcluded(instance: Instance): boolean
	return ReadProcessor.exclusionOf(instance) ~= nil
end

--- Remember an instance that was left out of sync for a marker, so that
--- removing the marker later brings it back (`onExclusionChange`).
function ReadProcessor:markExcluded(instance: Instance)
	self.excluded[instance] = true
end

--- How many instances are currently kept in Studio by a marker.
function ReadProcessor:excludedCount(): number
	local count = 0

	for _ in self.excluded do
		count += 1
	end

	return count
end

--- Take a tracked instance out of sync: the whole subtree leaves the tree
--- with it, so a later return re-adds it complete (`onAdd` skips anything
--- still tracked), and it is remembered as a candidate for that return.
function ReadProcessor:exclude(instance: Instance)
	self.tree:removeSubtree(instance)
	self:markExcluded(instance)
end

--- Snapshot every non-default property of an instance in the encoded wire
--- form — the shared read/validate/encode pass behind `onAdd` and `onChange`,
--- also reused by the bulk-transfer `read_subtree` op (which skips `Source`
--- via `skipProperties` and carries it as a SHA-256 manifest instead).
function ReadProcessor.snapshotProperties(instance: Instance, skipProperties: { [string]: boolean }?): Types.Properties
	local properties = {}

	for property, default in Dom.getDefaultProperties(instance.ClassName) do
		if skipProperties and skipProperties[property] then
			continue
		end

		local readSuccess, instanceValue = Dom.readProperty(instance, property)

		if not readSuccess then
			local err = Error.new(Error.ReadFailed, property, instance, instanceValue.kind)
			Log.warn(err)

			continue
		end

		local _, defaultValue = Dom.EncodedValue.decode(default)

		if not equals(instanceValue, defaultValue) then
			local propertyType = next(default)

			if not validateValue(instanceValue, propertyType) then
				local err = Error.new(Error.ValidationFailed, property, instance, instanceValue)
				Log.warn(err)

				continue
			end

			local encodeSuccess, encodedValue = Dom.EncodedValue.encode(instanceValue, propertyType)

			if not encodeSuccess then
				local err = Error.new(Error.EncodeFailed, property, instanceValue)
				Log.warn(err)

				continue
			end

			properties[property] = encodedValue
		end
	end

	return properties
end

function ReadProcessor:onAdd(instance: Instance, __parentId: Types.Ref?): Types.AddedSnapshot?
	Log.trace("Detected addition of", instance)

	if self.tree:getId(instance) then
		Log.trace("Instance already tracked. Skipping..")
		return nil
	end

	if not passesScope(instance) then
		Log.trace("Instance class is outside the code sync scope. Skipping..")
		return nil
	end

	local marker = ReadProcessor.exclusionOf(instance)

	if marker then
		-- The whole subtree stays out with it: children are only ever added
		-- through this recursion, so an untracked parent keeps them untracked
		Log.trace("Instance carries", marker, "— kept in Studio. Skipping..")
		self:markExcluded(instance)
		return nil
	end

	local parentId = __parentId or self.tree:getId(instance.Parent)

	if not parentId then
		Log.trace("Unknown instance. Skipping..")
		return nil
	end

	local id = generateRef()
	local properties = {}
	local children = {}

	if self.shouldSyncProperties(instance) then
		properties = ReadProcessor.snapshotProperties(instance)
	end

	for _, child in instance:GetChildren() do
		local snapshot = self:onAdd(child, id)

		if snapshot then
			table.insert(children, snapshot)
		end
	end

	local snapshot = Snapshot.new(id)
		:withName(instance.Name)
		:withClass(instance.ClassName)
		:withProperties(properties)
		:withChildren(children)

	self.tree:insertInstance(instance, id, snapshot.meta)

	if __parentId then
		return snapshot
	end

	if parentId then
		snapshot.parent = parentId
	else
		local currentInstance = instance
		local currentSnapshot = nil
		local rootId = nil

		repeat
			parentId = generateRef()
			currentInstance = currentInstance.Parent
			rootId = self.tree:getId(currentInstance)

			if rootId then
				break
			end

			-- In theory this should never happen
			if currentInstance == game then
				Log.warn("Failed to find root instance for", instance)
				return nil
			end

			currentSnapshot = Snapshot.new(parentId)
				:withName(currentInstance.Name)
				:withClass(currentInstance.ClassName)
				:withChildren({ snapshot })

			self.tree:insertInstance(currentInstance, parentId, currentSnapshot.meta)
		until rootId

		currentSnapshot.parent = rootId
		snapshot = currentSnapshot
	end

	return snapshot
end

function ReadProcessor:onChange(instance: Instance, property: string?): Types.UpdatedSnapshot?
	Log.trace("Detected change of", instance, property)

	local id = self.tree:getId(instance)

	if not id then
		Log.trace("Unknown instance. Skipping..")
		return nil
	end

	if not passesScope(instance) then
		Log.trace("Instance class is outside the code sync scope. Skipping..")
		return nil
	end

	if property and not self.shouldSyncProperties(instance, property) then
		Log.trace("Instance does not pass sync filter. Skipping..")
		return nil
	end

	if property == "Name" then
		return Snapshot.newUpdated(id):withName(instance.Name)
	end

	return Snapshot.newUpdated(id):withProperties(ReadProcessor.snapshotProperties(instance))
end

--- The exclusion boundary (SyncScope), checked on every Source and Attributes
--- edit before the edit is read as an update — those two properties are where
--- the markers live. A tracked instance that now carries a marker leaves
--- sync: the daemon removes its disk copy, exactly as it would for an
--- instance deleted in Studio. One that was kept out and dropped its marker
--- comes back as an addition. Returns the resulting changes, or nil when the
--- edit did not cross the line and is an ordinary change for `onChange`.
function ReadProcessor:onExclusionChange(instance: Instance, property: string?): Types.Changes?
	if property ~= "Source" and property ~= "Attributes" then
		return nil
	end

	local id = self.tree:getId(instance)

	if id then
		local marker = ReadProcessor.exclusionOf(instance)

		if not marker then
			return nil
		end

		Log.info(`'{instance.Name}' now carries {marker} — it stays in Studio and its disk copy is removed`)

		self:exclude(instance)

		local changes = Changes.new()
		changes:remove(id)

		return changes
	end

	if not self.excluded[instance] or ReadProcessor.isExcluded(instance) then
		return nil
	end

	self.excluded[instance] = nil

	local snapshot = self:onAdd(instance)

	if not snapshot then
		-- Nowhere to attach it yet (its parent is not synced either): it
		-- stays out, and stays a candidate for when that changes
		self.excluded[instance] = true
		return nil
	end

	Log.info(`'{instance.Name}' is no longer kept in Studio — syncing it to disk`)

	local changes = Changes.new()
	changes:add(snapshot)

	return changes
end

--- Daemon changes land tracked (the write processor pairs whatever it
--- creates or updates), so an arrival that carries a marker — a sidecar with
--- `AvoidSync: true`, a file with the directive typed into it — would sync
--- both ways until its next marker edit in Studio. Instead it leaves at once:
--- the outermost tracked ancestor-or-self carrying a marker (a live-hydrated
--- parent can be one), else every carrier inside the arrived subtree. No
--- removal is pushed — the file on disk is simply a stale copy from here on,
--- and the next connect moves it to the backlog like any disk-only file.
function ReadProcessor:excludeArrivals(instance: Instance?)
	if not instance then
		return
	end

	local carrier: Instance? = nil
	local current: Instance? = instance

	while current and self.tree:getId(current) do
		if ReadProcessor.isExcluded(current) then
			carrier = current
		end

		current = current.Parent
	end

	if carrier then
		self:excludeArrival(carrier)
		return
	end

	for _, descendant in instance:GetDescendants() do
		-- A carrier's descendants are untracked by the time the walk reaches
		-- them, so a marker nested inside a marked subtree is not visited
		if self.tree:getId(descendant) and ReadProcessor.isExcluded(descendant) then
			self:excludeArrival(descendant)
		end
	end
end

function ReadProcessor:excludeArrival(instance: Instance)
	local marker = ReadProcessor.exclusionOf(instance)

	Log.info(`'{instance.Name}' arrived carrying {marker} — it stays in Studio from here on`)

	self:exclude(instance)
end

function ReadProcessor:onRemove(instance: Instance): Types.Ref?
	Log.trace("Detected removal of", instance)

	local movedTo = self.moving[instance]

	if movedTo ~= nil then
		self.moving[instance] = nil

		if instance.Parent == movedTo then
			-- A class swap's own move (`expectMove`): the instance keeps its
			-- ref under the replacement, nothing changed on disk
			Log.trace("Instance moved by a class swap. Skipping..")
			return nil
		end
	end

	local id = self.tree:getId(instance)

	if not id then
		-- An instance kept out by a marker that leaves the DataModel is no
		-- longer a candidate for coming back (the weak key would go on its
		-- own eventually; this is just prompt)
		self.excluded[instance] = nil

		Log.trace("Unknown instance. Skipping..")
		return nil
	end

	-- The subtree goes with it (Tree.removeSubtree): a reparent arrives as
	-- this removal followed by an addition of the same objects, and an Undo
	-- of a delete brings the same objects back — `onAdd` must see the
	-- descendants as new, or it skips them as already tracked and the
	-- addition lands on disk without them while this removal took their
	-- files away
	self.tree:removeSubtree(instance)

	return id
end

--- One watcher event → the changes it pushes: the mapping behind Core's
--- syncback loop, here so it stays testable without Core. A Source or
--- Attributes edit is first checked against the exclusion boundary (leaving
--- sync is a removal, returning is an addition), then read as an ordinary
--- change.
function ReadProcessor:onEvent(event: Types.WatcherEvent): Types.Changes
	local changes = Changes.new()
	local kind = event.kind

	if kind == "Add" then
		local snapshot = self:onAdd(event.instance)

		if snapshot then
			changes:add(snapshot)
		end
	elseif kind == "Change" then
		local crossed = self:onExclusionChange(event.instance, event.property)

		if crossed then
			changes:join(crossed)
		else
			local snapshot = self:onChange(event.instance, event.property)

			if snapshot then
				changes:update(snapshot)
			end
		end
	elseif kind == "Remove" then
		local id = self:onRemove(event.instance)

		if id then
			changes:remove(id)
		end
	end

	return changes
end

function ReadProcessor.shouldSyncProperties(instance: Instance, property: string?): boolean
	if instance:IsA("LuaSourceContainer") or property == "Name" then
		return true
	end

	-- Attributes and tags always ride along: the disk projection carries them
	-- in the instance-data sidecar (every scope, engine-side), and a connect
	-- diff that counts what the upload then strips would re-flag the same
	-- instances at every reconnect — the review row that never goes away
	if property == "Attributes" or property == "Tags" then
		return true
	end

	if property and not Dom.getDefaultProperties(instance.ClassName)[property] then
		return false
	end

	return Config:get("SyncbackProperties")
end

return ReadProcessor
