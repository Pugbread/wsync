-- Module loader for the syncback scenarios. scripts/check-plugin-syncback.mjs
-- emits `SOURCES` (module name → plugin source text), `FAKES_SOURCE` and
-- `SCENARIO_SOURCE` ahead of this text, then runs the whole thing with the
-- standalone `luau` binary.
--
-- Every plugin module runs as a loadstring chunk that receives its own
-- `script` and `require`, so the Instance-shaped require arguments the
-- sources use (`WSync.Types`, `script.Parent.Parent.Snapshot`) resolve
-- through module refs instead of a DataModel. Modules the fakes stand in for
-- (Log, Config, Dom, Signal, Types) come from STUBS; everything else is the
-- real source.

local Fakes = assert(loadstring(FAKES_SOURCE, "=fakes"))()

local function ref(name: string)
	return { __module = name }
end

local WSync = {
	Types = ref("Types"),
	Log = ref("Log"),
	Config = ref("Config"),
	SyncScope = ref("SyncScope"),
	Util = ref("Util"),
	Lib = { Dom = ref("Dom") },
	Helpers = { equals = ref("equals"), generateRef = ref("generateRef") },
	Packages = { Signal = ref("Signal") },
}

local core = {
	Tree = ref("Tree"),
	Changes = ref("Changes"),
	Meta = ref("Meta"),
	Snapshot = ref("Snapshot"),
	Error = ref("Error"),
}

local processor = { Parent = core, Read = ref("Read") }

local function scriptFor(parent)
	return {
		Parent = parent,
		FindFirstAncestor = function(_, name)
			assert(name == "WSync", `unexpected ancestor lookup {name}`)
			return WSync
		end,
	}
end

local SCRIPTS = {
	Read = scriptFor(processor),
	Snapshot = scriptFor(core),
}

local STUBS = {
	Types = {},
	Log = Fakes.Log,
	Config = Fakes.Config,
	Dom = Fakes.Dom,
	Signal = Fakes.Signal,
}

local loaded = {}

local function require(moduleRef)
	local name = assert(type(moduleRef) == "table" and moduleRef.__module, "require expects a module ref")

	if loaded[name] == nil then
		if STUBS[name] ~= nil then
			loaded[name] = STUBS[name]
		else
			local source = assert(SOURCES[name], `no source for module {name}`)
			-- The prepended line turns the module's hot comments (`--!strict`)
			-- into plain comments; the modules are type-checked elsewhere
			local chunk = assert(loadstring("local script, require = ...\n" .. source, "=" .. name))
			local result = chunk(SCRIPTS[name] or scriptFor(nil), require)

			loaded[name] = if result == nil then false else result
		end
	end

	return loaded[name]
end

-- `game` and `workspace` must exist before any module loads (Read and
-- Watcher resolve their services at load time); a scenario installs a fresh
-- DataModel for itself
Fakes.installGlobals()

local scenario = assert(loadstring("local Fakes, ref, require = ...\n" .. SCENARIO_SOURCE, "=scenario"))
scenario(Fakes, ref, require)
