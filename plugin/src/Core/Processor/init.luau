local WSync = script:FindFirstAncestor("WSync")

local Dom = require(WSync.Lib.Dom)

local Log = require(WSync.Log)
local Util = require(WSync.Util)
local Types = require(WSync.Types)
local Config = require(WSync.Config)
local SyncScope = require(WSync.SyncScope)
local equals = require(WSync.Helpers.equals)

local Error = require(script.Parent.Error)
local Changes = require(script.Parent.Changes)

local ReadProcessor = require(script.Read)
local WriteProcessor = require(script.Write)

local function isPackageDescendant(instance: Instance): boolean
	repeat
		if instance:FindFirstChildWhichIsA("PackageLink") then
			return true
		end

		instance = instance.Parent or game
	until instance == game

	return false
end

local Processor = {}
Processor.__index = Processor

function Processor.new(tree, transport)
	local read = ReadProcessor.new(tree)

	local self = {
		tree = tree,
		read = read,
		-- The write processor announces a class swap's moves to the read
		-- processor, so syncback does not push them back as deletions
		write = WriteProcessor.new(tree, transport, read),
	}

	return setmetatable(self, Processor)
end

function Processor:init(snapshot: Types.Snapshot, initialSyncPriority: string): Types.Changes
	Log.trace("Hydrating initial snapshot..")

	self:hydrate(snapshot, game)

	if initialSyncPriority == "None" then
		return Changes.new()
	end

	Log.trace("Diffing initial snapshot..")

	local changes = Changes.new()

	for _, child in snapshot.children do
		changes:join(self:diff(child, snapshot.id, initialSyncPriority == "Client"))
	end

	if initialSyncPriority == "Server" and not Config:get("OverridePackages") then
		local temp = {}

		for _, snapshot in changes.additions do
			local instance = self.tree:getInstance(snapshot.parent)

			if instance and isPackageDescendant(instance) then
				table.insert(temp, snapshot)
			end
		end

		for i, snapshot in temp do
			table.remove(changes.additions, table.find(changes.additions, snapshot))
			temp[i] = nil
		end

		for _, snapshot in changes.updates do
			local instance = self.tree:getInstance(snapshot.id)

			if instance and isPackageDescendant(instance) then
				table.insert(temp, snapshot)
			end
		end

		for i, snapshot in temp do
			table.remove(changes.updates, table.find(changes.updates, snapshot))
			temp[i] = nil
		end

		for _, instance in changes.removals do
			if isPackageDescendant(instance) then
				table.insert(temp, instance)
			end
		end

		for i, instance in temp do
			table.remove(changes.removals, table.find(changes.removals, instance))
			temp[i] = nil
		end
	end

	return changes
end

function Processor:hydrate(snapshot: Types.Snapshot, instance: Instance)
	self.tree:insertInstance(instance, snapshot.id, snapshot.meta)

	local children = instance:GetChildren()
	local hydrated = table.create(#children, false)

	for _, snapshotChild in snapshot.children do
		for index, child in children do
			if hydrated[index] then
				continue
			end

			if child.Name == snapshotChild.name and child.ClassName == snapshotChild.class then
				if ReadProcessor.isExcluded(child) then
					-- `AvoidSync` / `--!ignoresync` (SyncScope): the instance
					-- is never paired with its file. Unpaired, the file is
					-- disk-only to the diff — Studio-first moves it to the
					-- backlog — and the instance stays untracked, so nothing
					-- it does reaches disk. Marked so removing the marker
					-- brings it back; a same-named sibling without one may
					-- still claim the file
					self.read:markExcluded(child)
					continue
				end

				self:hydrate(snapshotChild, child)
				hydrated[index] = true
				break
			end
		end
	end
end

function Processor:diff(snapshot: Types.Snapshot, parent: Types.Ref, clientPriority: boolean): Types.Changes
	local changes = Changes.new()

	-- Code scope (Design §7.0): only the four allowlisted classes enter the
	-- connect diff. A code-scope daemon never serves anything else, so this
	-- is defensive on the snapshot side; the removal side below is the load-
	-- bearing half.
	local codeScope = SyncScope.isCode()

	local instance = self.tree:getInstance(snapshot.id)

	-- Check if snapshot is new
	if not instance then
		if not codeScope or SyncScope.isCodeClass(snapshot.class) then
			changes:add(snapshot, parent)
		end

		return changes
	end

	-- Diff properties, find updated ones. On code scope only allowlisted
	-- classes carry properties into the diff — the tracked exceptions are the
	-- service roots, which map as plain directories and must never produce
	-- reset-to-default update entries (Design §7.0).
	if
		(not codeScope or SyncScope.isCodeClass(instance.ClassName))
		and (not clientPriority or ReadProcessor.shouldSyncProperties(instance))
	then
		local updatedProperties = {}

		for property, default in Dom.getDefaultProperties(instance.ClassName) do
			local value = snapshot.properties[property]

			if value then
				local readSuccess, instanceValue = Dom.readProperty(instance, property)

				if not readSuccess then
					local err = Error.new(Error.ReadFailed, property, instance, instanceValue.kind)
					Log.warn(err)

					continue
				end

				local decodeSuccess, snapshotValue = Dom.EncodedValue.decode(value)

				if not decodeSuccess then
					local err = Error.new(Error.DecodeFailed, property, value)
					Log.warn(err)

					continue
				end

				if not equals(instanceValue, snapshotValue) then
					updatedProperties[property] = value
				end

				-- If snapshot does not have the property we want it to be default
			else
				local readSuccess, instanceValue = Dom.readProperty(instance, property)

				if not readSuccess then
					local err = Error.new(Error.ReadFailed, property, instance, instanceValue.kind)
					Log.warn(err)

					continue
				end

				local _, defaultValue = Dom.EncodedValue.decode(default)

				if not equals(instanceValue, defaultValue) then
					updatedProperties[property] = default
				end
			end
		end

		if next(updatedProperties) then
			changes:update({
				id = snapshot.id,
				properties = updatedProperties,
				meta = snapshot.meta.meshSource and snapshot.meta or nil,
			})
		end
	end

	-- Diff snapshot children, find new ones
	for _, child in snapshot.children do
		local childInstance = self.tree:getInstance(child.id)

		if not childInstance and (not codeScope or SyncScope.isCodeClass(child.class)) then
			changes:add(child, snapshot.id)
		end
	end

	-- Diff instance children, find removed ones. On code scope the allowlist
	-- alone decides (Design §7.0): unknown code-class children always enter
	-- the diff as Studio-only entries — the daemon writes them to disk, never
	-- the reverse — while every other class stays out structurally (the
	-- KeepUnknowns guarantee).
	for _, child in instance:GetChildren() do
		local childId = self.tree:getId(child)

		if childId then
			local childSnapshot = Util.find(snapshot.children, function(child)
				return equals(child.id, childId)
			end)

			changes:join(self:diff(childSnapshot, snapshot.id, clientPriority))
		elseif ReadProcessor.isExcluded(child) then
			-- `AvoidSync` / `--!ignoresync` (SyncScope): not Studio-only
			-- content for the daemon to write out — the instance is in
			-- Studio alone on purpose
			self.read:markExcluded(child)
		else
			-- A Studio-only child. On code scope that means "the daemon writes
			-- it to disk", never "delete it from Studio", so whether Roblox
			-- could recreate the class is beside the point — and demanding it
			-- silently skipped the `StarterPlayerScripts` /
			-- `StarterCharacterScripts` containers, which are NotCreatable
			-- singletons, taking every script beneath them out of the diff with
			-- them. Full scope can still reverse a removal back into Studio, so
			-- there the class must stay creatable.
			local studioOnly = if codeScope
				then SyncScope.isCodeClass(child.ClassName)
				else (clientPriority or not (snapshot.meta.keepUnknowns or Config:get("KeepUnknowns")))
					and Dom.isCreatable(child.ClassName)

			if studioOnly then
				changes:remove(child)
			end
		end
	end

	return changes
end

--- Apply daemon changes to Studio, then hold the exclusion line on what
--- arrived: the write processor tracks whatever it creates or updates, and an
--- arrival carrying `AvoidSync` or `--!ignoresync` (SyncScope) must not stay
--- tracked — it would sync both ways until its next marker edit in Studio.
--- The marker itself is what the daemon sent, so the untracking is silent:
--- no removal goes back, the file just becomes a stale copy (disk-only at
--- the next connect, where Studio-first moves it to the backlog).
---
--- Both passes are isolated per change, like the write processor's own
--- (WriteProcessor:applyChanges): one arrival that cannot be checked is
--- logged and left alone, and the batch — and the session — carry on.
function Processor:applyChanges(changes: Types.Changes, initial: boolean?)
	self.write:applyChanges(changes, initial)

	local function excludeArrivals(id: Types.Ref)
		local ok, err = pcall(function()
			self.read:excludeArrivals(self.tree:getInstance(id))
		end)

		if not ok then
			Log.warn("Failed to check an arrival from the server for exclusion markers:", id, err)
		end
	end

	for _, snapshot in changes.additions do
		excludeArrivals(snapshot.id)
	end

	for _, snapshot in changes.updates do
		excludeArrivals(snapshot.id)
	end
end

function Processor:reverseChanges(changes: Types.Changes): Types.Changes
	Log.trace("Reversing changes..")

	local reversed = Changes.new()

	for _, snapshot in changes.additions do
		reversed:remove(snapshot.id)
	end

	for _, snapshot in changes.updates do
		snapshot = self.read:onChange(self.tree:getInstance(snapshot.id))

		if snapshot then
			reversed:update(snapshot)
		end
	end

	for _, instance in changes.removals do
		local snapshot = self.read:onAdd(instance)

		if snapshot then
			reversed:add(snapshot)
		end
	end

	return reversed
end

return Processor
