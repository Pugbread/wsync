--!native

local WSync = script:FindFirstAncestor("WSync")

local Types = require(WSync.Types)

local Tree = {}
Tree.__index = Tree

function Tree.new()
	local self = {
		instanceMap = {},
		metaMap = {},
		idMap = {},
	}

	return setmetatable(self, Tree)
end

function Tree:insertInstance(instance: Instance, id: Types.Ref, meta: Types.Meta)
	local strId = buffer.tostring(id)

	self.instanceMap[strId] = instance
	self.idMap[instance] = strId

	self:insertMeta(id, meta)
end

function Tree:updateInstance(id: Types.Ref, instance: Instance)
	local strId = buffer.tostring(id)
	local old = self.instanceMap[strId]

	if not old then
		return
	end

	self.idMap[old] = nil

	self.instanceMap[strId] = instance
	self.idMap[instance] = strId
end

function Tree:getId(instance: Instance): Types.Ref?
	local strId = self.idMap[instance]
	return strId and buffer.fromstring(strId)
end

function Tree:getInstance(id: Types.Ref): Instance?
	return self.instanceMap[buffer.tostring(id)]
end

function Tree:removeByInstance(instance: Instance): Types.Ref?
	local strId = self.idMap[instance]

	if not strId then
		return nil
	end

	self.instanceMap[strId] = nil
	self.idMap[instance] = nil

	local id = buffer.fromstring(strId)

	self:removeMeta(id)

	return id
end

function Tree:removeById(id: Types.Ref): Instance?
	local strId = buffer.tostring(id)
	local instance = self.instanceMap[strId]

	if not instance then
		return nil
	end

	self.instanceMap[strId] = nil
	self.idMap[instance] = nil

	self:removeMeta(id)

	return instance
end

--- Untrack an instance together with everything under it. The entry for a
--- removed instance is never the only one that dies with it: its
--- descendants' files go from disk in the same removal, and if the very same
--- objects come back later — a reparent is remove-then-add, an Undo
--- restores them — they must read as new, or the addition that re-syncs
--- them skips every still-tracked child and lands without them. Returns the
--- instance's ref when it was tracked.
function Tree:removeSubtree(instance: Instance): Types.Ref?
	for _, descendant in instance:GetDescendants() do
		self:removeByInstance(descendant)
	end

	return self:removeByInstance(instance)
end

function Tree:insertMeta(id: Types.Ref, meta: Types.Meta)
	self.metaMap[buffer.tostring(id)] = meta
end

function Tree:getMeta(id: Types.Ref): Types.Meta?
	return self.metaMap[buffer.tostring(id)]
end

function Tree:updateMeta(id: Types.Ref, meta: Types.Meta)
	local strId = buffer.tostring(id)
	local old = self.metaMap[strId]

	if not old then
		return
	end

	self.metaMap[strId] = meta
end

function Tree:removeMeta(id: Types.Ref): Types.Meta?
	local strId = buffer.tostring(id)
	local meta = self.metaMap[strId]

	self.metaMap[strId] = nil

	return meta
end

return Tree
