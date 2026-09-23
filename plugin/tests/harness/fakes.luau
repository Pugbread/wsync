-- The fake Roblox surface the syncback scenarios run on (see
-- scripts/check-plugin-syncback.mjs). Only what Watcher, the processors, Tree
-- and their dependencies touch: instances with a parent/children hierarchy
-- that fires ChildAdded/ChildRemoved/Changed the way Studio does (with
-- Immediate signal behavior — the strictest case for echo suppression), the
-- services those modules resolve at load, `Instance.new`, and stand-ins for
-- the impure modules (Log, Config, Dom, Signal). Everything else the
-- scenarios load is the real plugin source.

local Fakes = {}

--------------------------------------------------------------------------------
-- Signals
--------------------------------------------------------------------------------

local FakeSignal = {}
FakeSignal.__index = FakeSignal

function FakeSignal.new()
	return setmetatable({ connections = {} }, FakeSignal)
end

function FakeSignal:Connect(callback)
	local connection = { callback = callback, connected = true }

	function connection.Disconnect(self_)
		self_.connected = false
	end

	table.insert(self.connections, connection)

	return connection
end

function FakeSignal:Fire(...)
	for _, connection in table.clone(self.connections) do
		if connection.connected then
			connection.callback(...)
		end
	end
end

function FakeSignal:Wait()
	error("Wait is not supported by the fake signal; Connect instead")
end

--- How many connections are still live — the leak detector for the Watcher.
function FakeSignal:liveCount(): number
	local count = 0

	for _, connection in self.connections do
		if connection.connected then
			count += 1
		end
	end

	return count
end

Fakes.Signal = FakeSignal

--------------------------------------------------------------------------------
-- Instances
--------------------------------------------------------------------------------

local ANCESTRY = {
	DataModel = { "DataModel", "ServiceProvider", "Instance" },
	ReplicatedStorage = { "ReplicatedStorage", "Instance" },
	ServerScriptService = { "ServerScriptService", "Instance" },
	Folder = { "Folder", "Instance" },
	Script = { "Script", "LuaSourceContainer", "Instance" },
	LocalScript = { "LocalScript", "Script", "LuaSourceContainer", "Instance" },
	ModuleScript = { "ModuleScript", "LuaSourceContainer", "Instance" },
	Part = { "Part", "BasePart", "PVInstance", "Instance" },
}

local FakeInstance = {}
local methods = {}

function methods:IsA(className: string): boolean
	return table.find(ANCESTRY[self.__props.ClassName], className) ~= nil
end

function methods:GetChildren()
	return table.clone(self.__children)
end

function methods:GetDescendants()
	local out = {}

	local function walk(instance)
		for _, child in instance.__children do
			table.insert(out, child)
			walk(child)
		end
	end

	walk(self)

	return out
end

--- Studio's form: dot-joined names from below the DataModel down.
function methods:GetFullName(): string
	local names = {}
	local current = self

	while current and current.__props.ClassName ~= "DataModel" do
		table.insert(names, 1, current.__props.Name)
		current = current.__props.Parent
	end

	return table.concat(names, ".")
end

--- Detach (firing the usual events) and mark destroyed, descendants first.
function methods:Destroy()
	for _, child in self:GetChildren() do
		child:Destroy()
	end

	self.Parent = nil
	rawset(self, "__destroyed", true)
end

function methods:FindFirstAncestor(name: string)
	local current = self.__props.Parent

	while current do
		if current.__props.Name == name then
			return current
		end

		current = current.__props.Parent
	end

	return nil
end

function methods:FindFirstChildWhichIsA(className: string)
	for _, child in self.__children do
		if child:IsA(className) then
			return child
		end
	end

	return nil
end

function methods:GetTags()
	return {}
end

function methods:GetAttribute(name: string)
	return rawget(self, "__attributes")[name]
end

function methods:GetAttributes()
	return table.clone(rawget(self, "__attributes"))
end

--- Studio fires `AttributeChanged` (never `Changed`) for an attribute write;
--- the Watcher turns it into a Change("Attributes") event.
function methods:SetAttribute(name: string, value: any)
	rawget(self, "__attributes")[name] = value
	self.AttributeChanged:Fire(name)
end

function methods:GetService(name: string)
	return Fakes.services[name] or error(`no fake service {name}`)
end

FakeInstance.__index = function(self, key)
	local method = methods[key]

	if method then
		return method
	end

	local value = rawget(self, "__props")[key]

	if value ~= nil then
		return value
	end

	-- Child access by name (`parent.Child`)
	for _, child in rawget(self, "__children") do
		if child.__props.Name == key then
			return child
		end
	end

	return nil
end

FakeInstance.__newindex = function(self, key, value)
	local props = rawget(self, "__props")

	if key == "Parent" and rawget(self, "__destroyed") then
		error(`The Parent property of {props.Name} is locked, current parent: NULL, new parent {value}`)
	end

	if key ~= "Parent" then
		props[key] = value
		self.Changed:Fire(key)
		return
	end

	local old = props.Parent

	if old == value then
		return
	end

	props.Parent = value

	-- Studio's order on a reparent: the old parent's ChildRemoved, then the
	-- new parent's ChildAdded, then Changed("Parent") on the instance
	if old then
		local index = table.find(old.__children, self)

		if index then
			table.remove(old.__children, index)
		end

		old.ChildRemoved:Fire(self)
	end

	if value then
		table.insert(value.__children, self)
		value.ChildAdded:Fire(self)
	end

	self.Changed:Fire("Parent")
end

FakeInstance.__tostring = function(self)
	return `{self.__props.ClassName}({self.__props.Name})`
end

--- A fake instance. Scripts start with an empty `Source`.
function Fakes.new(className: string, name: string?, parent)
	assert(ANCESTRY[className], `unknown fake class {className}`)

	local instance = setmetatable({
		__props = { ClassName = className, Name = name or className, Parent = nil },
		__children = {},
		__attributes = {},
		ChildAdded = FakeSignal.new(),
		ChildRemoved = FakeSignal.new(),
		Changed = FakeSignal.new(),
		AttributeChanged = FakeSignal.new(),
	}, FakeInstance)

	if instance:IsA("LuaSourceContainer") then
		instance.__props.Source = ""
	end

	if parent then
		instance.Parent = parent
	end

	return instance
end

--------------------------------------------------------------------------------
-- Services + globals
--------------------------------------------------------------------------------

local tagSignals = {}

local function tagSignal(kind: string, tag: string)
	local key = kind .. ":" .. tag

	if not tagSignals[key] then
		tagSignals[key] = FakeSignal.new()
	end

	return tagSignals[key]
end

Fakes.services = {
	ScriptEditorService = {
		GetEditorSource = function(_, instance)
			return instance.Source
		end,
	},
	CollectionService = {
		TagAdded = FakeSignal.new(),
		TagRemoved = FakeSignal.new(),
		GetTagged = function()
			return {}
		end,
		GetInstanceAddedSignal = function(_, tag)
			return tagSignal("added", tag)
		end,
		GetInstanceRemovedSignal = function(_, tag)
			return tagSignal("removed", tag)
		end,
	},
	HttpService = {
		GenerateGUID = function()
			return "00000000-0000-0000-0000-000000000000"
		end,
	},
}

--- Install a fresh DataModel as the `game` global (and a `workspace` with a
--- camera the Watcher skips, and `Instance.new` over the fake classes).
--- Every loadstring chunk shares this env.
function Fakes.installGlobals()
	local dataModel = Fakes.new("DataModel", "game")

	game = dataModel
	workspace = { CurrentCamera = {} }
	Instance = {
		new = function(className: string)
			if not ANCESTRY[className] then
				error(`Unable to create an Instance of type "{className}"`)
			end

			return Fakes.new(className)
		end,
	}

	return dataModel
end

--------------------------------------------------------------------------------
-- Stand-ins for the impure modules
--------------------------------------------------------------------------------

--- Every `Log.warn` (and `Log.debug`) the plugin code emits, for scenarios
--- to assert on.
Fakes.warnings = {}
Fakes.debugs = {}

--- A log argument as text: error objects by message, buffers (refs) as hex
--- the way the real Log prints them.
local function logText(part: any): string
	if type(part) == "table" and part.message ~= nil then
		return tostring(part.message)
	end

	if type(part) == "buffer" then
		local hex = {}

		for i = 0, buffer.len(part) - 1 do
			table.insert(hex, string.format("%02x", buffer.readu8(part, i)))
		end

		return table.concat(hex)
	end

	return tostring(part)
end

local function logLine(...): string
	local parts = {}

	for i = 1, select("#", ...) do
		table.insert(parts, logText((select(i, ...))))
	end

	return table.concat(parts, " ")
end

Fakes.Log = {
	trace = function() end,
	debug = function(...)
		table.insert(Fakes.debugs, logLine(...))
	end,
	info = function() end,
	stage = function() end,
	warn = function(...)
		table.insert(Fakes.warnings, logLine(...))
	end,
	error = function(...)
		error(table.concat({ ... }, " "), 0)
	end,
}

Fakes.configValues = {
	SyncbackProperties = false,
	KeepUnknowns = true,
	OverridePackages = true,
	LiveHydrate = true,
}

Fakes.Config = {
	get = function(_, key)
		return Fakes.configValues[key]
	end,
}

-- The only property the code-scope classes carry is `Source`, encoded the
-- way Dom.EncodedValue does (`{ String = value }`)
local DEFAULTS = {
	Script = { Source = { String = "" } },
	LocalScript = { Source = { String = "" } },
	ModuleScript = { Source = { String = "" } },
}

local SERVICES = { DataModel = true, ReplicatedStorage = true, ServerScriptService = true }

local function hasProperty(instance, property): boolean
	return (DEFAULTS[instance.ClassName] or {})[property] ~= nil
end

Fakes.Dom = {
	getDefaultProperties = function(className)
		return DEFAULTS[className] or {}
	end,
	readProperty = function(instance, property)
		if not hasProperty(instance, property) then
			return false, { kind = "UnknownProperty" }
		end

		return true, instance[property]
	end,
	writeProperty = function(instance, property, value)
		if not hasProperty(instance, property) then
			return false, { kind = "UnknownProperty" }
		end

		instance[property] = value

		return true
	end,
	isCreatable = function(className)
		return not SERVICES[className]
	end,
	EncodedValue = {
		decode = function(encoded)
			local _, value = next(encoded)
			return true, value
		end,
		encode = function(value, propertyType)
			return true, { [propertyType] = value }
		end,
	},
}

return Fakes
