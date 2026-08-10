local WSync = script:FindFirstAncestor("WSync")

local Dom = require(WSync.Lib.Dom)

local Log = require(WSync.Log)
local Types = require(WSync.Types)
local Config = require(WSync.Config)

local equals = require(WSync.Helpers.equals)
local setWaypoint = require(WSync.Helpers.setWaypoint)

-- History owns the ChangeHistoryService transaction state; no cycle — the
-- handler chain never requires back into Core (see Remote/Handlers/History)
local History = require(WSync.Remote.Handlers.History)

local Error = require(script.Parent.Parent.Error)

local MESH_RELOAD_PROPERTIES = { "MeshContent", "InitialSize" }

--- Per-batch waypoint, gated (pinned wave-4 contract): while a daemon
--- transaction recording is open (`transaction_begin` .. sync frames ..
--- `transaction_finish`), the recording brackets every applied change, so
--- the per-batch waypoints are suppressed — a daemon-driven bulk pull lands
--- as exactly one Studio Undo, and SetWaypoint never fires mid-recording.
local function batchWaypoint(action: string, name: string?)
	if History.shouldSuppressBatchWaypoints() then
		return
	end

	setWaypoint(action, name)
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

function WriteProcessor.new(tree, transport)
	return setmetatable({
		tree = tree,
		-- The transport interface (Transport/init.luau) — used for on-demand
		-- subtree fetches when `LiveHydrate` resolves missing parents
		transport = transport,
	}, WriteProcessor)
end

function WriteProcessor:applyChanges(changes: Types.Changes, initial: boolean?)
	Log.trace("Applying changes..")

	batchWaypoint("Sync")

	for _, snapshot in changes.additions do
		self:applyAddition(snapshot, initial)
	end

	for _, snapshot in changes.updates do
		self:applyUpdate(snapshot, initial)
	end

	for _, object in changes.removals do
		self:applyRemoval(object, initial)
	end

	if initial then
		batchWaypoint("Initial Sync")
	end
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
			local err = Error.new(Error.NoInstanceAdd, snapshot)
			Log.warn(err)

			return
		end
	end

	if skipExisting then
		local instance = findChild(parent, snapshot.name, snapshot.class)

		if instance then
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
			local err = Error.new(Error.HydrationFailed, snapshot)
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

	if not isService then
		instance.Parent = parent
	end

	self.tree:insertInstance(instance, snapshot.id, snapshot.meta)

	for _, child in snapshot.children do
		child.parent = snapshot.id
		self:applyAddition(child, initial)
	end

	if not initial then
		batchWaypoint("Add", instance.Name)
	end

	return instance
end

function WriteProcessor:applyUpdate(snapshot: Types.UpdatedSnapshot, initial: boolean?): Instance?
	Log.trace("Applying update of", snapshot)

	local instance = self.tree:getInstance(snapshot.id)

	if not instance then
		if Config:get("LiveHydrate") then
			local addedSnapshot = self.transport:getSnapshot(snapshot.id):expect()

			if addedSnapshot then
				instance = self:applyAddition(addedSnapshot, initial)
				Log.info("Live hydrated instance:", instance)
			end
		end

		if not instance then
			local err = Error.new(Error.NoInstanceUpdate, snapshot)
			Log.warn(err)

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

	if
		snapshot.class
		or (snapshot.meta and snapshot.meta.meshSource and shouldReloadMesh(instance, snapshot.properties))
	then
		local newInstance

		if snapshot.class then
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

		for _, child in instance:GetChildren() do
			child.Parent = newInstance
		end

		newInstance.Parent = instance.Parent
		instance.Parent = nil

		self.tree:updateInstance(snapshot.id, newInstance)

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

	if not initial then
		batchWaypoint("Update", instance.Name)
	end

	return instance
end

function WriteProcessor:applyRemoval(object: Types.Ref | Instance, initial: boolean?): Instance?
	Log.trace("Applying removal of", object)

	if typeof(object) == "Instance" then
		if not Dom.isCreatable(object.ClassName) then
			return
		end

		self.tree:removeByInstance(object)
		object.Parent = nil
	else
		local instance = self.tree:getInstance(object)

		if not instance then
			local err = Error.new(Error.NoInstanceRemove, object)
			Log.warn(err)

			return
		end

		if not Dom.isCreatable(instance.ClassName) then
			local err = Error.new(Error.NotRemovable, instance.ClassName)
			Log.warn(err)

			return
		end

		self.tree:removeById(object)
		instance.Parent = nil

		object = instance
	end

	if not initial then
		batchWaypoint("Remove", (object :: Instance).Name)
	end

	return object
end

return WriteProcessor
