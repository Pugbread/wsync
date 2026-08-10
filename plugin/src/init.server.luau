--!strict
-- Boot gate. Studio runs this script once per DataModel: the edit DataModel
-- boots the full plugin, while the clones Studio creates inside playtest
-- DataModels (the PlayServer and each PlayClient) must never start the edit
-- UI, connect to the localhost daemon, or sync — they boot the playtest
-- runtime agent instead and talk only to the edit copy over
-- PluginConnectionService (playtest.json; Playtest/Host owns the detection,
-- connection-direction-first because Run mode's clone can report an "Edit"
-- HostDataModelType).
--
-- Runtime changes never sync: the runtime path below requires none of
-- Core/App/Transport, so no watcher ever starts in a playtest DataModel; and
-- the edit copy's watcher only ever connects to instances of the edit
-- DataModel it resolved at start (Core.rootInstances), so playtest state is
-- unobservable to it by construction.

local RunService = game:GetService("RunService")

local Host = require(script.Playtest.Host)

local host = Host.detect()

if host == "PlayServer" or host == "PlayClient" then
	local ok, err = pcall(function()
		require(script.Playtest.Runtime).start()
	end)

	if not ok then
		warn(`[WSync] playtest runtime agent failed to start: {tostring(err)}`)
	end

	return
end

if RunService:IsRunning() then
	-- Defensive: a running DataModel that detection did not classify must
	-- still never boot the edit UI or sync.
	return
end

local Config = require(script.Config)
local Log = require(script.Log)
local App = require(script.App)

Config.load()
Log.__setLevel(Config:get("LogLevel"))
App.new()

Config:onChanged("LogLevel", function(level)
	Log.__setLevel(level)
end)
