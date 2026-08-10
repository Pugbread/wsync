--!strict
-- Host-DataModel detection for the plugin boot gate (init.server.luau).
--
-- Studio clones every plugin into the playtest DataModels (the PlayServer
-- and each PlayClient). Those clones must never boot the edit UI, connect to
-- the localhost daemon, or sync — they run the playtest runtime agent
-- (Playtest/Runtime) instead, and the edit copy stays the sole external
-- bridge.
--
-- Detection is connection-direction-first (the Ro-Sync-proven order): a
-- DataModel that can hold Test-type plugin connections is the edit DataModel;
-- one that can hold Edit-type connections is a playtest DataModel, split into
-- PlayServer/PlayClient by RunService:IsServer(). This matters because Run
-- mode's runtime clone can report an "Edit" HostDataModelType — trusting that
-- property first would boot the edit UI inside the Run-mode DataModel and
-- never advertise a server context. HostDataModelType and RunService are kept
-- as compatibility fallbacks for Studio channels without
-- PluginConnectionService.

local RunService = game:GetService("RunService")

local Host = {}

export type HostType = "Edit" | "PlayServer" | "PlayClient"

--- Whether this Studio build exposes PluginConnectionService (the playtest
--- transport). When false, the playtest op family degrades to typed errors.
function Host.connectionServiceAvailable(): boolean
	local ok, service = pcall(function()
		return game:GetService("PluginConnectionService")
	end)

	return ok and service ~= nil
end

--- Classify the DataModel this plugin copy woke up in.
function Host.detect(): HostType
	local canConnectToEdit = false
	local canConnectToTest = false

	local inspected = pcall(function()
		local service = game:GetService("PluginConnectionService")

		canConnectToEdit = service:CanHaveConnectionType(Enum.PluginConnectionTargetType.Edit)
		canConnectToTest = service:CanHaveConnectionType(Enum.PluginConnectionTargetType.Test)
	end)

	if inspected and canConnectToTest then
		return "Edit"
	end

	if inspected and canConnectToEdit then
		return if RunService:IsServer() then "PlayServer" else "PlayClient"
	end

	-- Fallback 1: the plugin's HostDataModelType (absent on some channels,
	-- and unreliable for Run mode — hence the connection check above).
	local hostName = "Unknown"

	pcall(function()
		local pluginInstance = script:FindFirstAncestorWhichIsA("Plugin")

		if pluginInstance ~= nil then
			hostName = (pluginInstance :: any).HostDataModelType.Name
		end
	end)

	if hostName == "PlayServer" or hostName == "PlayClient" then
		return hostName :: HostType
	end

	-- Fallback 2: a running DataModel is a playtest DataModel of some kind.
	if RunService:IsRunning() then
		return if RunService:IsServer() then "PlayServer" else "PlayClient"
	end

	return "Edit"
end

return Host
