local WSync = script:FindFirstAncestor("WSync")

local Types = require(WSync.Types)

local Meta = {}
Meta.__index = Meta

function Meta.new(): Types.Meta
	local self = setmetatable({}, Meta)

	self.keepUnknowns = false
	self.meshSource = nil

	return self
end

return Meta
