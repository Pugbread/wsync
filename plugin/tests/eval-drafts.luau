-- `eval --drafts`: the drafts `require` (Remote/DraftRequire.luau) runs the
-- editor text of every ModuleScript an eval reaches, without touching the
-- committed `.Source`. Runs the real module over a fake host; `Fakes`, `ref`
-- and `require` arrive from harness/loader.luau.
--
-- The regression this guards: WSync writes script text through
-- ScriptEditorService, edit-mode `require` runs the committed `.Source`, so
-- `wsync eval` of a freshly written module failed with "Module code did not
-- return exactly one value".

local DraftRequire = require(ref("DraftRequire"))

local failures = 0
local checks = 0

local function expect(condition: boolean, label: string)
	checks += 1

	if condition then
		print(`  ok    {label}`)
	else
		failures += 1
		print(`  FAIL  {label}`)
	end
end

local function contains(haystack: any, needle: string): boolean
	return type(haystack) == "string" and string.find(haystack, needle, 1, true) ~= nil
end

-- A fake ModuleScript: `Source` is the committed text, `editor` the editor
-- text (nil = unreadable). Children are plain fields, so `script.Parent.X`
-- resolves the way it does on a real Instance
local function folder(name: string, parent: any?): any
	local node = { Name = name, Parent = parent, kind = "Folder" }

	if parent ~= nil then
		parent[name] = node
	end

	return node
end

local function module(name: string, parent: any, committed: string?, editor: string?): any
	local node = { Name = name, Parent = parent, kind = "ModuleScript", Source = committed, editor = editor }

	parent[name] = node

	return node
end

local function fullName(node: any): string
	local parts = {}
	local current = node

	while current ~= nil do
		table.insert(parts, 1, current.Name)
		current = current.Parent
	end

	return table.concat(parts, ".")
end

local fallbackCalls = {}

local function host(): DraftRequire.Host
	return {
		isModule = function(target: any): boolean
			return type(target) == "table" and target.kind == "ModuleScript"
		end,
		editorSource = function(node: any): string?
			return node.editor
		end,
		committedSource = function(node: any): string?
			return node.Source
		end,
		fullName = fullName,
		path = function(node: any): string
			return (string.gsub(fullName(node), "%.", "/"))
		end,
		compile = function(source: string, chunkName: string): (any, string?)
			return loadstring(source, chunkName)
		end,
		setEnvironment = function(fn: any, env: { [any]: any })
			setfenv(fn, env)
		end,
		fallbackRequire = function(target: any): any
			table.insert(fallbackCalls, target)

			return `fallback:{tostring(target)}`
		end,
	}
end

local globals = {
	marker = "eval-globals",
	error = error,
	math = math,
	string = string,
	tostring = tostring,
}

local function session()
	return DraftRequire.new(host(), globals)
end

--- Run an eval chunk through a session, the way Write.runSource does
local function eval(draftSession: any, source: string): (boolean, any)
	local fn = assert(loadstring(source, "=wsync-eval"))

	setfenv(fn, globals)
	DraftRequire.install(draftSession, fn)

	return pcall(fn)
end

local game = folder("game")
local shared = folder("Shared", game)

-- A module WSync just wrote: nothing committed yet, the text is a draft
local fresh = module("Fresh", shared, "", "return 41")
-- A clean module (no draft) that requires a drafted sibling through `script`
local clean = module("Clean", shared, "return require(script.Parent.Fresh) + 1", nil)
clean.editor = clean.Source
-- An edited module: the draft differs from the committed text
local edited = module("Edited", shared, "return 'old'", "return 'new'")

-- The eval chunk and every module resolve `game` through the eval's globals
globals.game = game

print("drafts are loaded from editor text, transitively")
do
	local draftSession = session()
	local ok, first, second = eval(draftSession, "return require(game.Shared.Clean), require(game.Shared.Edited)")

	expect(ok, `the eval runs ({tostring(first)})`)
	expect(first == 42, "a clean module reaches its drafted sibling's editor text through script.Parent")
	expect(second == "new", "an edited module runs its draft, not the committed text")
	expect(draftSession.moduleCount == 3, `three modules were loaded (got {draftSession.moduleCount})`)
	expect(#draftSession.fromEditor == 2, `two came from editor text (got {#draftSession.fromEditor})`)
	expect(
		table.find(draftSession.fromEditor, "game/Shared/Fresh") ~= nil
			and table.find(draftSession.fromEditor, "game/Shared/Edited") ~= nil,
		"the report names the drafted modules by path"
	)
	expect(fresh.Source == "" and edited.Source == "return 'old'", "committed sources are never written")
end

print("require keeps its one-execution and one-value rules")
do
	local runs = module("Runs", shared, "", "counter = (counter or 0) + 1\nreturn { script = script, marker = marker }")
	local draftSession = session()
	local ok, first, second = eval(draftSession, "return require(game.Shared.Runs), require(game.Shared.Runs)")

	expect(ok, `the eval runs ({tostring(first)})`)
	expect(first == second, "a module required twice runs once and returns the same value")
	expect(type(first) == "table" and first.script == runs, "a module's `script` is the real ModuleScript")
	expect(
		type(first) == "table" and first.marker == "eval-globals",
		"modules read globals through the eval's environment"
	)
	expect(globals.counter == nil, "a module's global writes stay in its own environment")

	module("Empty", shared, "", "local x = 1")
	local okEmpty, emptyError = eval(session(), "return require(game.Shared.Empty)")

	expect(
		not okEmpty and contains(emptyError, "did not return exactly one value"),
		`zero returns fail like require ({tostring(emptyError)})`
	)
	expect(contains(emptyError, "game.Shared.Empty"), "the one-value failure names the module")

	module("Two", shared, "", "return 1, 2")
	local okTwo, twoError = eval(session(), "return require(game.Shared.Two)")

	expect(not okTwo and contains(twoError, "did not return exactly one value"), "two returns fail like require")

	module("CycleA", shared, "", "return require(script.Parent.CycleB)")
	module("CycleB", shared, "", "return require(script.Parent.CycleA)")
	local okCycle, cycleError = eval(session(), "return require(game.Shared.CycleA)")

	expect(
		not okCycle and contains(cycleError, "required recursively"),
		`a cycle fails like require ({tostring(cycleError)})`
	)
end

print("errors keep the module's name and the draft/source distinction")
do
	module("Broken", shared, "return 1", "return (")
	local okCompile, compileError = eval(session(), "return require(game.Shared.Broken)")

	expect(
		not okCompile and contains(compileError, "failed to compile from its editor draft"),
		`a broken draft says so ({tostring(compileError)})`
	)
	expect(contains(compileError, "game.Shared.Broken"), "the compile failure names the module")

	module("Raises", shared, "", "error('boom')")
	local okRaise, raiseError = eval(session(), "return require(game.Shared.Raises)")

	expect(
		not okRaise and contains(raiseError, "game.Shared.Raises:1: boom"),
		`a runtime error keeps the module's chunk name ({tostring(raiseError)})`
	)
end

print("everything else goes to Studio's own require")
do
	table.clear(fallbackCalls)

	local locked = module("Locked", shared, nil, nil)
	local ok, byId, byLocked = eval(session(), "return require(12345), require(game.Shared.Locked)")

	expect(ok, `the eval runs ({tostring(byId)})`)
	expect(byId == "fallback:12345", "an asset id falls back to Studio's require")
	expect(byLocked == `fallback:{tostring(locked)}`, "an unreadable module falls back to Studio's require")
	expect(#fallbackCalls == 2, "exactly the two non-draftable requires fell back")
end

print(`{checks - failures}/{checks} checks passed`)

if failures > 0 then
	error(`{failures} eval-drafts check(s) failed`)
end
