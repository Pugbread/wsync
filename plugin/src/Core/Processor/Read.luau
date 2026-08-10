local WSync = script:FindFirstAncestor("WSync")

local Dom = require(WSync.Lib.Dom)

local Types = require(WSync.Types)
local Log = require(WSync.Log)
local Config = require(WSync.Config)
local SyncScope = require(WSync.SyncScope)

local equals = require(WSync.Helpers.equals)
local generateRef = require(WSync.Helpers.generateRef)

local Snapshot = require(script.Parent.Parent.Snapshot)
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

local ReadProcessor = {}
ReadProcessor.__index = ReadProcessor

function ReadProcessor.new(tree)
	return setmetatable({
		tree = tree,
	}, ReadProcessor)
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

function ReadProcessor:onRemove(instance: Instance): Types.Ref?
	Log.trace("Detected removal of", instance)

	local id = self.tree:getId(instance)

	if not id then
		Log.trace("Unknown instance. Skipping..")
		return nil
	end

	self.tree:removeById(id)

	return id
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
