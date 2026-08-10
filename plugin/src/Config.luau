local plugin = script:FindFirstAncestorWhichIsA("Plugin")

local WSync = script:FindFirstAncestor("WSync")

local Util = require(WSync.Util)
local Log = require(WSync.Log)
local SyncScope = require(WSync.SyncScope)

export type Level = "Place" | "Game" | "Global"
-- Keys marked "retired" (Design §7.0 — the Studio-first ruling) stay in the
-- type for compatibility, but `Config:get` resolves them to fixed values
-- (SyncScope.resolveRetired) regardless of any stored override, and the
-- Settings widget no longer shows them.
export type Setting =
	"InitialSyncPriority" -- retired: fixed "Server" (full-scope legacy path only)
	| "Host"
	| "Port"
	| "AutoConnect"
	| "AutoReconnect"
	| "AutoDiscover"
	| "PreferAppModal" -- hidden: never wired into behavior (superseded by §7.0)
	| "LiveHydrate" -- retired: fixed true
	| "KeepUnknowns" -- retired: structural on code scope (allowlist), default on full
	| "TwoWaySync" -- retired: fixed true
	| "SyncbackProperties" -- retired: fixed false
	| "OnlyCodeMode" -- retired: derived from the hello scope, never stored
	| "OpenInEditor"
	| "Https"
	| "DiffLinesLimit"
	| "DisplayPrompts" -- retired: "Never" on code scope, pre-ruling default on full
	| "ChangesThreshold" -- retired: ignored on code scope, default gate on full
	| "LogLevel"
	| "OverridePackages" -- retired: fixed true

type Config = {
	__configs: { [Level]: { [Setting]: any } },
	__callbacks: { [Setting]: { (value: any) -> () } },
	load: () -> (),
	get: (self: Config, setting: Setting, level: Level?, exact: boolean?) -> any,
	getDefault: (self: Config, setting: Setting) -> any,
	set: (self: Config, setting: Setting, value: any, level: Level) -> (),
	restoreDefaults: (self: Config, level: Level) -> (),
	onChanged: (self: Config, setting: Setting, callback: (value: any) -> ()) -> () -> (),
}

-- Defaults per Design §9.3, as amended by §7.0 (Studio-first): the settings
-- surface shrank to connectivity only (Host, Port, AutoDiscover, Https,
-- LogLevel). The retired keys keep their defaults here for `getDefault`
-- compatibility, but their effective values come from
-- `SyncScope.resolveRetired` — `Config:get` never reads stored overrides for
-- them.
local DEFAULTS = {
	InitialSyncPriority = "Server",
	Host = "localhost",
	Port = 7978,
	AutoConnect = true,
	AutoReconnect = false,
	AutoDiscover = true,
	-- `PreferAppModal` is stored + displayed only: the connect-time divergence
	-- decision surface is the daemon's ruling — the final `/compare` receipt
	-- carries `resolver:"app"|"plugin"` (pinned wave-4 contract) and the
	-- plugin follows it. TODO(phase-5): feed this preference into the upload
	-- so the daemon can weigh it when an app is attached.
	PreferAppModal = true,
	LiveHydrate = true,
	KeepUnknowns = false,
	TwoWaySync = true,
	SyncbackProperties = false,
	OnlyCodeMode = false,
	OpenInEditor = false,
	Https = false,
	DiffLinesLimit = 3000,
	DisplayPrompts = "Always",
	ChangesThreshold = 5,
	LogLevel = "Warn",
	OverridePackages = true,
}

local CONFIGS_ORDERED = { "Place", "Game", "Global" }
-- Fresh settings namespace on purpose: WSync flips two Argon defaults
-- (TwoWaySync, OnlyCodeMode), so inheriting stored Argon values would
-- silently override them with stale explicit entries.
local CONFIGS = {
	Place = "WSyncConfigPlace_" .. game.PlaceId,
	Game = "WSyncConfigGame_" .. game.GameId,
	Global = "WSyncConfigGlobal",
}

local Config: Config = {
	__configs = {},
	__callbacks = {},
} :: Config

function Config.load()
	for level, config in CONFIGS do
		config = plugin:GetSetting(config)

		if config and type(config) == "table" then
			Config.__configs[level] = config
		else
			Config.__configs[level] = {}
		end
	end
end

function Config:get(setting, level, exact)
	local default = DEFAULTS[setting]
	local relevant = not level

	if default == nil then
		Log.error(`Setting '{setting}' does not exist!`)
	end

	-- Retired settings (Design §7.0): fixed by the Studio-first ruling —
	-- OnlyCodeMode/DisplayPrompts/ChangesThreshold/KeepUnknowns resolve
	-- against the live session scope, the rest are constants. Stored
	-- overrides (including level-exact reads) are dead for these keys.
	local retired, fixed = SyncScope.resolveRetired(setting, SyncScope.get())

	if retired then
		return fixed
	end

	if exact then
		return self.__configs[level][setting]
	end

	if level and level ~= "Global" then
		local index = table.find(CONFIGS_ORDERED, level)

		if not index then
			Log.error(`Config level '{level}' does not exist!`)
		end

		level = CONFIGS_ORDERED[index + 1]
	end

	for _, config in CONFIGS_ORDERED do
		if not relevant then
			if config == level then
				relevant = true
			else
				continue
			end
		end

		local value = self.__configs[config][setting]

		if value ~= nil then
			return value
		end
	end

	return default
end

function Config:getDefault(setting)
	local default = DEFAULTS[setting]

	if default == nil then
		Log.error(`Setting '{setting}' does not exist!`)
	end

	return default
end

function Config:set(setting, value, level)
	local default = DEFAULTS[setting]
	local config = self.__configs[level]

	if default == nil then
		Log.error(`Setting '{setting}' does not exist!`)
	end

	if value == config[setting] then
		return
	end

	value = Util.cast(value, type(default))
	config[setting] = if value == default then nil else value

	if self.__callbacks[setting] then
		for _, callback in self.__callbacks[setting] do
			callback(self:get(setting))
		end
	end

	if not next(config) then
		plugin:SetSetting(CONFIGS[level], nil)
	else
		plugin:SetSetting(CONFIGS[level], config)
	end
end

function Config:restoreDefaults(level)
	self.__configs[level] = {}

	for setting, _ in self.__configs[level] do
		if self.__callbacks[setting] then
			for _, callback in self.__callbacks[setting] do
				callback(self:getDefault(setting))
			end
		end
	end

	plugin:SetSetting(CONFIGS[level], nil)
end

function Config:onChanged(setting, callback)
	if not self.__callbacks[setting] then
		self.__callbacks[setting] = {}
	end

	local id = Util.generateGUID()
	self.__callbacks[setting][id] = callback

	return function()
		self.__callbacks[setting][id] = nil
	end
end

return Config
