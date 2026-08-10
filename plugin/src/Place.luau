--!strict
local MarketPlaceService = game:GetService("MarketplaceService")

local name = game.Name
local isFile = (name:find(".rbxl") or name:find(".rbxlx")) ~= nil

-- Edit-mode `game.Name` for a published place is Studio's internal place-file
-- name ("Place7"), not what the creator titled it; MarketplaceService is the
-- one API that returns the real name. An unpublished place (PlaceId 0) has
-- nothing to look up, so its `game.Name` stands.
if not isFile and game.PlaceId ~= 0 then
	pcall(function()
		name = MarketPlaceService:GetProductInfo(game.PlaceId).Name
	end)
end

return {
	Name = name,
	IsFile = isFile,
	Version = game.PlaceVersion,
}
