--!strict
-- `eval --drafts` (eval.json): a `require` that runs editor drafts.
--
-- WSync writes script text through ScriptEditorService, so a script's editor
-- text (`GetEditorSource`) can run ahead of its committed `.Source` — and an
-- edit-mode `require` runs the committed `.Source`. A module WSync just
-- created therefore requires as empty ("Module code did not return exactly
-- one value"), and an edited one runs its old code.
--
-- With `--drafts`, the eval chunk's environment carries this module's
-- `require`. Every ModuleScript it reaches — directly, or through the modules
-- it loads — is compiled from its editor text (the committed `.Source` when
-- there is no draft) with `loadstring` and run in an environment carrying
-- the real `script` and this same `require`, so `script.Parent` requires
-- resolve and drafts are honored all the way down, including behind modules
-- that have no draft of their own. Nothing in the DataModel is created or
-- written: no temporary clones (those would sync to disk and enter
-- ChangeHistory), and no committed `.Source` is touched.
--
-- Kept from `require`: one execution per module per eval (cached), "exactly
-- one value", and a recursive require fails. Deliberate limits:
--   * only `require(<ModuleScript>)` sees drafts; string requires and asset
--     ids go to Studio's own `require`, which runs committed sources
--   * a module whose source cannot be read falls back to Studio's `require`
--   * each eval builds a fresh module graph: module state is not shared with
--     modules Studio or other plugin code already required, nor kept between
--     evals
--   * modules run as `loadstring` chunks, so `--!native` is ignored and
--     errors name the module by its full name
--
-- Pure over an injected host (the SourceCache/WriteRules pattern), so the
-- behavior is tested under the standalone `luau` binary
-- (plugin/tests/eval-drafts.luau); `DraftRequire.studioHost` binds the real
-- Roblox APIs at run time.

local DraftRequire = {}

export type Host = {
	--- The value `require` was called with is a ModuleScript instance
	isModule: (target: any) -> boolean,
	--- Editor text, or nil when it cannot be read
	editorSource: (module: any) -> string?,
	--- Committed `.Source`, or nil when it cannot be read
	committedSource: (module: any) -> string?,
	--- Dotted name used as the chunk name (`ReplicatedStorage.Shared.Util`)
	fullName: (module: any) -> string,
	--- WSync path reported back to the caller (`ReplicatedStorage/Shared/Util`)
	path: (module: any) -> string,
	--- `loadstring`
	compile: (source: string, chunkName: string) -> (any, string?),
	--- `setfenv`
	setEnvironment: (fn: any, env: { [any]: any }) -> (),
	--- Studio's own `require`, for everything the drafts loader does not own
	fallbackRequire: (target: any) -> any,
}

export type Session = {
	host: Host,
	--- The environment the eval chunk was compiled with; every module
	--- environment reads globals through it
	globals: { [any]: any },
	loaded: { [any]: { value: any } },
	loading: { [any]: boolean },
	--- ModuleScripts this session compiled and ran
	moduleCount: number,
	--- Paths of the modules whose editor text differed from `.Source`
	fromEditor: { string },
}

function DraftRequire.new(host: Host, globals: { [any]: any }): Session
	return {
		host = host,
		globals = globals,
		loaded = {},
		loading = {},
		moduleCount = 0,
		fromEditor = {},
	}
end

--- The environment one chunk runs in: globals through the eval's own
--- environment, `require` bound to the session, and — for a module — the
--- module itself as `script`
function DraftRequire.environment(session: Session, script: any?): { [any]: any }
	local env: { [any]: any } = setmetatable({}, { __index = session.globals }) :: any

	env.require = function(target: any): any
		return DraftRequire.require(session, target)
	end

	if script ~= nil then
		env.script = script
	end

	return env
end

--- Point an already compiled eval chunk at the session's `require`
function DraftRequire.install(session: Session, fn: any)
	session.host.setEnvironment(fn, DraftRequire.environment(session, nil))
end

--- `require` with drafts: see the header for the rules
function DraftRequire.require(session: Session, target: any): any
	local host = session.host

	if not host.isModule(target) then
		return host.fallbackRequire(target)
	end

	local cached = session.loaded[target]

	if cached ~= nil then
		return cached.value
	end

	local name = host.fullName(target)

	if session.loading[target] then
		error(`Requested module was required recursively ({name})`, 0)
	end

	local editor = host.editorSource(target)
	local committed = host.committedSource(target)
	local source = if editor ~= nil then editor else committed

	if source == nil then
		-- Nothing readable (a locked or core module): Studio's `require` is
		-- the only way to run it
		return host.fallbackRequire(target)
	end

	local drafted = editor ~= nil and editor ~= committed
	local fn, compileError = host.compile(source, `={name}`)

	if fn == nil then
		error(
			`Requested module {name} failed to compile from its {if drafted then "editor draft" else "source"}: {tostring(
				compileError
			)}`,
			0
		)
	end

	host.setEnvironment(fn, DraftRequire.environment(session, target))

	session.moduleCount += 1

	if drafted then
		table.insert(session.fromEditor, host.path(target))
	end

	session.loading[target] = true

	local packed = table.pack(pcall(fn))

	session.loading[target] = nil

	if not packed[1] then
		-- The module's own error, unchanged: its message already carries the
		-- module's name and line
		error(packed[2], 0)
	end

	if packed.n ~= 2 then
		error(`Module code did not return exactly one value ({name})`, 0)
	end

	session.loaded[target] = { value = packed[2] }

	return packed[2]
end

--- The real Roblox bindings. `pathOf` is the plugin's Paths.pathOf, passed
--- in so this module stays require-free
function DraftRequire.studioHost(pathOf: (instance: Instance) -> string): Host
	local ScriptEditorService = game:GetService("ScriptEditorService")

	return {
		isModule = function(target: any): boolean
			return typeof(target) == "Instance" and target:IsA("ModuleScript")
		end,
		editorSource = function(module: any): string?
			local ok, source = pcall(function()
				return (ScriptEditorService :: any):GetEditorSource(module)
			end)

			return if ok and type(source) == "string" then source else nil
		end,
		committedSource = function(module: any): string?
			local ok, source = pcall(function()
				return module.Source
			end)

			return if ok and type(source) == "string" then source else nil
		end,
		fullName = function(module: any): string
			local ok, name = pcall(function()
				return module:GetFullName()
			end)

			return if ok and type(name) == "string" then name else tostring(module)
		end,
		path = function(module: any): string
			local ok, path = pcall(pathOf, module)

			return if ok and type(path) == "string" then path else tostring(module)
		end,
		compile = function(source: string, chunkName: string): (any, string?)
			return (loadstring :: any)(source, chunkName)
		end,
		setEnvironment = function(fn: any, env: { [any]: any })
			(setfenv :: any)(fn, env)
		end,
		fallbackRequire = function(target: any): any
			return (require :: any)(target)
		end,
	}
end

return DraftRequire
