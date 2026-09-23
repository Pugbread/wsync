local WSync = script:FindFirstAncestor("WSync")

local Dom = require(WSync.Lib.Dom)

local Log = require(WSync.Log)
local Util = require(WSync.Util)
local Types = require(WSync.Types)
local Config = require(WSync.Config)

local equals = require(WSync.Helpers.equals)

local Error = require(script.Parent.Parent.Error)

-- The exclusion rule (SyncScope: `AvoidSync` / `--!ignoresync`) lives on the
-- read processor; nothing from disk is applied under or over an instance it
-- keeps in Studio
local ReadProcessor = require(script.Parent.Read)

local MESH_RELOAD_PROPERTIES = { "MeshContent", "InitialSize" }

-- An update for a ref the tree does not know is a change that did NOT reach
-- Studio, so it stays a warning — but a bounded one: a daemon replaying
-- changes for refs this session never received would otherwise flood the
-- Output. At most this many per window; the rest are counted and summed up
-- in one line at the end of each batch.
local UNKNOWN_UPDATE_WARNINGS = 5
local UNKNOWN_UPDATE_WINDOW = 10

--- Where an instance sits, for log lines. Never throws.
local function pathOf(instance: Instance?): string?
	if instance == nil then
		return nil
	end

	local ok, path = pcall(function()
		return instance:GetFullName()
	end)

	return if ok then path else nil
end

--- An addition, for log lines: name, class and hex ref.
local function describeAddition(snapshot: Types.AddedSnapshot): string
	return `'{Util.safeStringify(snapshot.name)}' ({Util.safeStringify(snapshot.class)}) {Util.safeStringify(snapshot.id)}`
end

local function loadMesh(path: string): MeshPart?
	-- The daemon (WSync engine fork) registers the project dir under the
	-- `wsync` content alias, mirroring Argon's `rbxasset://argon/` trick.
	local success, mesh = pcall(function()
		return game:GetObjects("rbxasset://wsync/" .. path)[1]
	end)

	if not success then
		local err = Error.new(Error.MeshLoadFailed, mesh)
		Log.warn(err)

		return nil
	end

	return mesh
end

local function shouldReloadMesh(mesh: MeshPart, properties: Types.Properties?): boolean
	if not properties then
		return false
	end

	for _, property in MESH_RELOAD_PROPERTIES do
		local value = properties[property]

		if not value then
			continue
		end

		local decodeSuccess, snapshotValue = Dom.EncodedValue.decode(value)

		if not decodeSuccess then
			local err = Error.new(Error.DecodeFailed, property, value)
			Log.warn(err)

			continue
		end

		property = property == "InitialSize" and "MeshSize" or property

		if not equals(mesh[property], snapshotValue) then
			return true
		end
	end

	return false
end

local function findChild(parent: Instance, name: string, class: string): Instance?
	for _, child in parent:GetChildren() do
		if child.Name == name and child.ClassName == class then
			return child
		end
	end

	return nil
end

local function warnKeptInStudio(instance: Instance)
	Log.warn(
		`'{instance.Name}' is kept in Studio ({ReadProcessor.exclusionOf(instance)}) — a change from disk was not applied to it`
	)
end

local function warnNotCreatable(class: string)
	local err = Error.new(Error.NotCreatable, class)

	if class == "PackageLink" and not Config:get("OverridePackages") then
		Log.info(err)
	else
		Log.warn(err)
	end
end

local WriteProcessor = {}
WriteProcessor.__index = WriteProcessor

function WriteProcessor.new(tree, transport, read)
	return setmetatable({
		tree = tree,
		-- The transport interface (Transport/init.luau) — used for on-demand
		-- subtree fetches when `LiveHydrate` resolves missing parents
		transport = transport,
		-- The session's read processor, told about the moves a class swap
		-- makes so syncback does not read them as Studio edits
		-- (`swapInstance`). Optional: without one, nothing is announced
		read = read,
		-- Rate limit for unknown-ref update warnings (UNKNOWN_UPDATE_*)
		unknownUpdates = { windowStart = -math.huge, count = 0, suppressed = 0 },
	}, WriteProcessor)
end

--- Apply one daemon sync batch. Every change is applied on its own: one that
--- fails — a property Studio rejects, a class it refuses to create, a bug —
--- is logged with its hex ref (and path, when the tree knows it) and
--- skipped, and the rest of the batch and the connection carry on. An error
--- that escaped here used to unwind through Core's sync loop and end the
--- session, over a single change.
function WriteProcessor:applyChanges(changes: Types.Changes, initial: boolean?)
	Log.trace("Applying changes..")

	for _, snapshot in changes.additions do
		self:__isolated(function()
			self:applyAddition(snapshot, initial)
		end, function()
			return `addition of {describeAddition(snapshot)}`
		end)
	end

	for _, snapshot in changes.updates do
		self:__isolated(function()
			self:applyUpdate(snapshot, initial)
		end, function()
			local path = pathOf(self.tree:getInstance(snapshot.id))
			return `update of {Util.safeStringify(snapshot.id)}{if path then ` ({path})` else ""}`
		end)
	end

	for _, object in changes.removals do
		self:__isolated(function()
			self:applyRemoval(object)
		end, function()
			local path = pathOf(if type(object) == "buffer" then self.tree:getInstance(object) else object)
			return `removal of {Util.safeStringify(object)}{if path then ` ({path})` else ""}`
		end)
	end

	self:__flushUnknownUpdates()
end

--- Run one change's application; a failure is logged and swallowed.
--- `describe` is only called on failure, and a failure in it is swallowed
--- too.
function WriteProcessor:__isolated(apply: () -> (), describe: () -> string)
	local ok, err = pcall(apply)

	if ok then
		return
	end

	local described, what = pcall(describe)

	Log.warn(
		`Skipped the {if described then what else "change"} from the server: {Util.safeStringify(err)}`
	)
end

--- Warn about an update for a ref the tree does not know, within the
--- UNKNOWN_UPDATE_* budget.
function WriteProcessor:__warnUnknownUpdate(snapshot: Types.UpdatedSnapshot)
	local limiter = self.unknownUpdates
	local now = os.clock()

	if now - limiter.windowStart >= UNKNOWN_UPDATE_WINDOW then
		limiter.windowStart = now
		limiter.count = 0
	end

	if limiter.count >= UNKNOWN_UPDATE_WARNINGS then
		limiter.suppressed += 1
		return
	end

	limiter.count += 1

	local target = Util.safeStringify(snapshot.id)

	if snapshot.class then
		target ..= ` (class change to {Util.safeStringify(snapshot.class)})`
	end

	Log.warn(Error.new(Error.NoInstanceUpdate, target))
end

--- The one summary line for the unknown-ref updates the budget held back.
function WriteProcessor:__flushUnknownUpdates()
	local suppressed = self.unknownUpdates.suppressed

	if suppressed == 0 then
		return
	end

	self.unknownUpdates.suppressed = 0

	Log.warn(
		`…and {suppressed} more update{Util.plural(suppressed)} for instances that do not exist in the tree — not applied in Studio`
	)
end

function WriteProcessor:applyAddition(
	snapshot: Types.AddedSnapshot,
	initial: boolean?,
	skipExisting: boolean?
): Instance?
	Log.trace("Applying addition of", snapshot)

	local parent = self.tree:getInstance(snapshot.parent)

	if not parent then
		if Config:get("LiveHydrate") then
			local addedSnapshot = self.transport:getSnapshot(snapshot.parent):expect()

			if addedSnapshot then
				parent = self:applyAddition(addedSnapshot, initial, true)
				skipExisting = true

				Log.info("Live hydrated parent:", parent)
			end
		end

		if not parent then
			local err = Error.new(Error.NoInstanceAdd, `{describeAddition(snapshot)}, parent {Util.safeStringify(snapshot.parent)}`)
			Log.warn(err)

			return
		end
	end

	if skipExisting then
		local instance = findChild(parent, snapshot.name, snapshot.class)

		if instance then
			if ReadProcessor.isExcluded(instance) then
				-- A live hydrate pairs by name — and the instance it found is
				-- kept in Studio, so the addition it was fetched for must
				-- not land under it
				warnKeptInStudio(instance)
				return nil
			end

			return instance
		end
	end

	local instance
	local isService = false

	if Dom.isCreatable(snapshot.class) then
		instance = Instance.new(snapshot.class)

		if snapshot.meta.meshSource then
			instance = loadMesh(snapshot.meta.meshSource)

			if not instance then
				return
			end
		end
	elseif parent == game then
		local service = game:FindFirstChildOfClass(snapshot.class)

		if service then
			instance = service
			isService = true
		else
			local err = Error.new(Error.HydrationFailed, describeAddition(snapshot))
			Log.warn(err)

			return
		end
	else
		warnNotCreatable(snapshot.class)
		return
	end

	instance.Name = snapshot.name

	for property, value in snapshot.properties do
		local decodeSuccess, decodedValue = Dom.EncodedValue.decode(value)

		if not decodeSuccess then
			local err = Error.new(Error.DecodeFailed, property, value)
			Log.warn(err)

			continue
		end

		local writeSuccess, reason = Dom.writeProperty(instance, property, decodedValue)

		if not writeSuccess then
			local err = Error.new(Error.WriteFailed, property, instance, reason.kind)
			Log.warn(err)
		end
	end

	-- Tracked before it is parented: a watcher that sees the arrival at once
	-- (Immediate signal behavior) must read it as already synced, not as a
	-- new Studio instance to push back to disk under a fresh ref
	self.tree:insertInstance(instance, snapshot.id, snapshot.meta)

	if not isService then
		instance.Parent = parent
	end

	for _, child in snapshot.children do
		child.parent = snapshot.id
		self:applyAddition(child, initial)
	end

	return instance
end

--- The Studio instance a fetched addition would stand beside, when Studio
--- keeps that instance (SyncScope): same name and class under the tracked
--- parent, and carrying a marker.
function WriteProcessor:findExcludedTwin(snapshot: Types.AddedSnapshot): Instance?
	local parent = self.tree:getInstance(snapshot.parent)
	local twin = if parent then findChild(parent, snapshot.name, snapshot.class) else nil

	if twin and ReadProcessor.isExcluded(twin) then
		return twin
	end

	return nil
end

function WriteProcessor:applyUpdate(snapshot: Types.UpdatedSnapshot, initial: boolean?): Instance?
	Log.trace("Applying update of", snapshot)

	local instance = self.tree:getInstance(snapshot.id)

	if not instance then
		if Config:get("LiveHydrate") then
			local addedSnapshot = self.transport:getSnapshot(snapshot.id):expect()

			if addedSnapshot then
				local twin = self:findExcludedTwin(addedSnapshot)

				if twin then
					-- The instance this update is for is untracked because
					-- Studio keeps it — hydrating would stand a second copy
					-- next to it
					warnKeptInStudio(twin)
					return
				end

				instance = self:applyAddition(addedSnapshot, initial)
				Log.info("Live hydrated instance:", instance)
			end
		end

		if not instance then
			self:__warnUnknownUpdate(snapshot)
			return
		end
	end

	local defaultProperties

	if snapshot.meta then
		local meta = self.tree:getMeta(snapshot.id)

		if
			meta.keepUnknowns ~= snapshot.meta.keepUnknowns
			and (not snapshot.meta.keepUnknowns and not Config:get("KeepUnknowns"))
		then
			for _, child in instance:GetChildren() do
				if self.tree:getId(child) then
					continue
				end

				self:applyRemoval(child)
			end
		end

		self.tree:updateMeta(snapshot.id, snapshot.meta)
	end

	-- The instance this update replaces, when it needs a new one: a class
	-- change or a mesh reload (`swapInstance`, at the end)
	local replaced: Instance? = nil

	if
		(snapshot.class and snapshot.class ~= instance.ClassName)
		or (snapshot.meta and snapshot.meta.meshSource and shouldReloadMesh(instance, snapshot.properties))
	then
		local newInstance

		if snapshot.class and snapshot.class ~= instance.ClassName then
			if not Dom.isCreatable(snapshot.class) then
				warnNotCreatable(snapshot.class)
				return
			end

			newInstance = Instance.new(snapshot.class)
			defaultProperties = Dom.getDefaultProperties(snapshot.class)
		else
			newInstance = loadMesh(snapshot.meta.meshSource :: string)

			if not newInstance then
				return
			end

			defaultProperties = Dom.getDefaultProperties(newInstance.ClassName)
		end

		newInstance.Name = instance.Name

		for property in defaultProperties do
			local readSuccess, instanceValue = Dom.readProperty(instance, property)

			if readSuccess then
				local writeSuccess, reason = Dom.writeProperty(newInstance, property, instanceValue)

				if not writeSuccess then
					local err = Error.new(Error.WriteFailed, property, newInstance, reason.kind)
					Log.warn(err)
				end
			end
		end

		-- Built in full — name and properties below — before it takes the old
		-- instance's place: unparented, none of those writes reach syncback
		replaced = instance
		instance = newInstance
	end

	if snapshot.name then
		instance.Name = snapshot.name
	end

	if snapshot.properties then
		if initial then
			for property, value in snapshot.properties do
				local decodeSuccess, decodedValue = Dom.EncodedValue.decode(value)

				if not decodeSuccess then
					local err = Error.new(Error.DecodeFailed, property, value)
					Log.warn(err)

					continue
				end

				local writeSuccess, reason = Dom.writeProperty(instance, property, decodedValue)

				if not writeSuccess then
					local err = Error.new(Error.WriteFailed, property, instance, reason.kind)
					Log.warn(err)
				end
			end
		else
			for property, default in (defaultProperties or Dom.getDefaultProperties(instance.ClassName)) do
				local value = snapshot.properties[property]

				if value then
					local decodeSuccess, snapshotValue = Dom.EncodedValue.decode(value)

					if not decodeSuccess then
						local err = Error.new(Error.DecodeFailed, property, value)
						Log.warn(err)

						continue
					end

					local writeSuccess, reason = Dom.writeProperty(instance, property, snapshotValue)

					if not writeSuccess then
						local err = Error.new(Error.WriteFailed, property, instance, reason.kind)
						Log.warn(err)
					end
				else
					local _, defaultValue = Dom.EncodedValue.decode(default)
					local writeSuccess, reason = Dom.writeProperty(instance, property, defaultValue)

					if not writeSuccess then
						local err = Error.new(Error.WriteFailed, property, instance, reason.kind)
						Log.warn(err)
					end
				end
			end
		end
	end

	if replaced then
		self:swapInstance(snapshot.id, replaced, instance)
	end

	return instance
end

--- Put `replacement` in the place of `instance`: the class-change half of an
--- update (a Folder gaining `init.luau` becomes a ModuleScript, a script
--- renamed to another suffix changes class) and the mesh reload. Same parent,
--- every child moved across, the ref moved with it, the old instance
--- destroyed — the instance is replaced, never duplicated, and later updates
--- and removals by the same ref reach the replacement. `replacement` arrives
--- built (name, properties) and unparented.
---
--- The order is what keeps syncback from echoing the swap to disk as a
--- delete and re-create of the subtree, whether Studio fires the events at
--- once or deferred:
---   1. the tree points at the replacement before anything moves, so its
---      arrival reads as already synced and the old instance's departure as
---      an unknown instance's
---   2. each child's move is announced to the read processor
---      (`ReadProcessor:expectMove`), which would otherwise read the child
---      leaving the old instance as a deletion
---   3. the children move before the replacement is parented, so the
---      Watcher — which drops a child's wiring when it leaves the old
---      instance — rewires the whole subtree when the replacement arrives
function WriteProcessor:swapInstance(id: Types.Ref, instance: Instance, replacement: Instance)
	local parent = instance.Parent

	self.tree:updateInstance(id, replacement)

	for _, child in instance:GetChildren() do
		if self.read then
			self.read:expectMove(child, replacement)
		end

		child.Parent = replacement
	end

	replacement.Parent = parent
	instance:Destroy()
end

function WriteProcessor:applyRemoval(object: Types.Ref | Instance): Instance?
	Log.trace("Applying removal of", object)

	-- The subtree is untracked with the instance (Tree.removeSubtree): its
	-- descendants leave the DataModel with it, and if an Undo brings the same
	-- objects back they must read as new to the read processor, or the
	-- addition that re-syncs them skips every still-tracked child
	if typeof(object) == "Instance" then
		if not Dom.isCreatable(object.ClassName) then
			return
		end

		self.tree:removeSubtree(object)
		object.Parent = nil
	else
		local instance = self.tree:getInstance(object)

		if not instance then
			-- Benign: whatever the ref named is already gone from Studio's
			-- view, which is where the removal wanted it
			Log.debug(Error.new(Error.NoInstanceRemove, object))

			return
		end

		if not Dom.isCreatable(instance.ClassName) then
			local err = Error.new(Error.NotRemovable, instance.ClassName)
			Log.warn(err)

			return
		end

		self.tree:removeSubtree(instance)
		instance.Parent = nil

		object = instance
	end

	return object
end

return WriteProcessor
