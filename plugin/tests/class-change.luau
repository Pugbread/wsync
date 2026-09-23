-- A class change from the daemon replaces the instance in place: adding
-- `init.luau` to a Folder's directory makes it a ModuleScript, removing it
-- makes it a Folder again, renaming `init.server.luau` ↔ `init.luau` or a
-- flat `X.server.luau` ↔ `X.luau` swaps Script ↔ ModuleScript. Runs the real
-- Processor (Read + Write halves), Watcher and Tree over the fakes in
-- harness/; `Fakes`, `ref` and `require` arrive from harness/loader.luau.
--
-- The regression this guards: `ServerScriptService/Tests` was a Folder,
-- `Tests/init.luau` was added, and Studio ended up with a second `Tests` (a
-- ModuleScript) beside the old Folder and its stale children. What must hold
-- instead: one instance under the ref, every child moved across (the same
-- objects, keeping their refs), the old instance destroyed, later updates
-- and removals by the ref reaching the replacement — and syncback, running
-- the whole time, pushing nothing back: the fakes fire events immediately,
-- the strictest case, so an unsuppressed echo shows up as a pushed change.

local Tree = require(ref("Tree"))
local Processor = require(ref("Processor"))
local Watcher = require(ref("Watcher"))
local Changes = require(ref("Changes"))
local generateRef = require(ref("generateRef"))

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

local function sameRef(a, b): boolean
	return a ~= nil and b ~= nil and buffer.tostring(a) == buffer.tostring(b)
end

local function isEmpty(changes): boolean
	return #changes.additions == 0 and #changes.updates == 0 and #changes.removals == 0
end

local function describe(changes): string
	return `{#changes.additions} additions, {#changes.updates} updates, {#changes.removals} removals`
end

local function childrenNamed(parent, name: string)
	local found = {}

	for _, child in parent:GetChildren() do
		if child.Name == name then
			table.insert(found, child)
		end
	end

	return found
end

local function batch(fields)
	return {
		additions = fields.additions or {},
		updates = fields.updates or {},
		removals = fields.removals or {},
	}
end

local function source(text: string)
	return { Source = { String = text } }
end

-- Nothing here reaches the transport: every ref is tracked
local transport = {
	getSnapshot = function()
		error("the class-change scenarios never fetch a snapshot")
	end,
}

--- One session: a hydrated tree over
---   game
---   ├ ReplicatedStorage
---   └ ServerScriptService
---     ├ Tests (Folder)
---     │ ├ A (ModuleScript)
---     │ └ Sub (Folder)
---     │   └ B (Script)
---     └ X (Script)
--- with a running watcher feeding the read processor, aggregated the way
--- Core's syncback loop aggregates, and a Processor applying daemon changes.
local function session(watch: boolean?)
	local game = Fakes.installGlobals()
	local replicated = Fakes.new("ReplicatedStorage", nil, game)
	local server = Fakes.new("ServerScriptService", nil, game)

	local tests = Fakes.new("Folder", "Tests", server)
	local a = Fakes.new("ModuleScript", "A", tests)
	a.Source = "return 1"
	local sub = Fakes.new("Folder", "Sub", tests)
	local b = Fakes.new("Script", "B", sub)
	b.Source = "print(1)"
	local x = Fakes.new("Script", "X", server)
	x.Source = "print('x')"

	local tree = Tree.new()

	local function hydrate(instance)
		tree:insertInstance(instance, generateRef(), { keepUnknowns = false })

		for _, child in instance:GetChildren() do
			hydrate(child)
		end
	end

	hydrate(game)

	local processor = Processor.new(tree, transport)
	local watcher = Watcher.new()
	local aggregate = Changes.new()

	watcher.signal:Connect(function(event)
		aggregate:join(processor.read:onEvent(event), true)
	end)

	if watch ~= false then
		watcher:start({ replicated, server })
	end

	return {
		game = game,
		server = server,
		tests = tests,
		a = a,
		sub = sub,
		b = b,
		x = x,
		tree = tree,
		processor = processor,
		watcher = watcher,
		testsId = tree:getId(tests),
		aId = tree:getId(a),
		subId = tree:getId(sub),
		bId = tree:getId(b),
		xId = tree:getId(x),
		--- Everything pushed since the last take, as one aggregate
		take = function()
			local changes = aggregate
			aggregate = Changes.new()
			return changes
		end,
	}
end

--- The assertions every in-place replacement must pass
local function expectReplaced(s, old, className: string, label: string)
	local current = s.tree:getInstance(s.testsId)
	local named = childrenNamed(s.server, "Tests")

	expect(#named == 1, `{label}: exactly one 'Tests' under ServerScriptService (found {#named})`)
	expect(current ~= nil and current ~= old, `{label}: the ref points at a new instance`)
	expect(current ~= nil and current.ClassName == className, `{label}: the new instance is a {className}`)
	expect(current ~= nil and current.Parent == s.server, `{label}: it took the old instance's parent`)
	expect(old ~= nil and rawget(old, "__destroyed") == true, `{label}: the old instance is destroyed`)
	expect(old ~= nil and s.tree:getId(old) == nil, `{label}: the old instance is no longer tracked`)
	expect(current ~= nil and s.a.Parent == current and s.sub.Parent == current, `{label}: the children moved across (same objects)`)
	expect(s.b.Parent == s.sub, `{label}: grandchildren stay where they were`)
	expect(
		sameRef(s.tree:getId(s.a), s.aId) and sameRef(s.tree:getId(s.sub), s.subId) and sameRef(s.tree:getId(s.b), s.bId),
		`{label}: the children keep their refs`
	)

	local echoed = s.take()

	expect(isEmpty(echoed), `{label}: syncback pushes nothing back ({describe(echoed)})`)
	expect(
		current ~= nil and current.Changed:liveCount() == 1 and s.a.Changed:liveCount() == 1 and s.b.Changed:liveCount() == 1,
		`{label}: the replacement and its subtree are wired once`
	)

	return current
end

--------------------------------------------------------------------------------

print("scenario: a Folder gains init.luau and becomes a ModuleScript")
do
	local s = session()
	local old = s.tests

	s.processor:applyChanges(batch({
		updates = { { id = s.testsId, class = "ModuleScript", properties = source("return {}") } },
	}))

	local current = expectReplaced(s, old, "ModuleScript", "Folder → ModuleScript")

	expect(current ~= nil and current.Source == "return {}", "the replacement carries the new Source")

	-- Later changes by the same ref reach the replacement
	s.processor:applyChanges(batch({ updates = { { id = s.testsId, properties = source("return 2") } } }))

	expect(current ~= nil and current.Source == "return 2", "a later update by the ref edits the replacement")

	s.take()

	-- Studio edits keep syncing on the moved children, under their old refs
	s.a.Source = "return 'edited'"

	local edited = s.take()

	expect(#edited.updates == 1 and sameRef(edited.updates[1].id, s.aId), "an edit on a moved child pushes one update under its ref")

	-- A real delete is never swallowed as a swap echo
	s.sub.Parent = nil

	local deleted = s.take()

	expect(#deleted.removals == 1 and sameRef(deleted.removals[1], s.subId), "deleting a moved child still pushes its removal")

	s.processor:applyChanges(batch({ removals = { s.testsId } }))

	expect(#childrenNamed(s.server, "Tests") == 0, "a removal by the ref removes the replacement")
	expect(s.tree:getInstance(s.testsId) == nil, "and untracks it")
end

print("scenario: a ModuleScript loses init.luau and becomes a Folder")
do
	local s = session()

	s.processor:applyChanges(batch({
		updates = { { id = s.testsId, class = "ModuleScript", properties = source("return {}") } },
	}))
	s.take()

	local module = s.tree:getInstance(s.testsId)

	s.processor:applyChanges(batch({ updates = { { id = s.testsId, class = "Folder", properties = {} } } }))

	local current = expectReplaced(s, module, "Folder", "ModuleScript → Folder")

	expect(current ~= nil and rawget(current, "__props").Source == nil, "the Folder carries no Source")
end

print("scenario: init.server.luau ↔ init.luau swaps Script ↔ ModuleScript and keeps the children")
do
	local s = session()

	s.processor:applyChanges(batch({
		updates = { { id = s.testsId, class = "Script", properties = source("print('server')") } },
	}))

	local script = expectReplaced(s, s.tests, "Script", "Folder → Script")

	s.processor:applyChanges(batch({
		updates = { { id = s.testsId, class = "ModuleScript", properties = source("return 'module'") } },
	}))

	local module = expectReplaced(s, script, "ModuleScript", "Script → ModuleScript")

	s.processor:applyChanges(batch({
		updates = { { id = s.testsId, class = "Script", properties = source("print('back')") } },
	}))

	local back = expectReplaced(s, module, "Script", "ModuleScript → Script")

	expect(back ~= nil and back.Source == "print('back')", "the last swap carries its Source")
end

print("scenario: a flat X.server.luau → X.luau swaps a childless Script")
do
	local s = session()
	local old = s.x

	s.processor:applyChanges(batch({
		updates = { { id = s.xId, class = "ModuleScript", properties = source("return 'x'") } },
	}))

	local current = s.tree:getInstance(s.xId)

	expect(#childrenNamed(s.server, "X") == 1, "exactly one 'X' under ServerScriptService")
	expect(current ~= nil and current.ClassName == "ModuleScript" and current.Source == "return 'x'", "it is the ModuleScript")
	expect(rawget(old, "__destroyed") == true and s.tree:getId(old) == nil, "the Script is destroyed and untracked")

	local echoed = s.take()

	expect(isEmpty(echoed), `syncback pushes nothing back ({describe(echoed)})`)
end

print("scenario: an update naming the class the instance already has changes it in place")
do
	local s = session()

	s.processor:applyChanges(batch({
		updates = { { id = s.xId, class = "Script", properties = source("print('same')") } },
	}))

	expect(s.tree:getInstance(s.xId) == s.x, "no replacement for an unchanged class")
	expect(s.x.Source == "print('same')", "the properties still apply")
end

print("scenario: a swap made while syncback is off leaves nothing to swallow later")
do
	local s = session(false)

	s.processor:applyChanges(batch({
		updates = { { id = s.testsId, class = "ModuleScript", properties = source("return {}") } },
	}))

	local current = s.tree:getInstance(s.testsId)

	s.watcher:start({ s.server })
	s.take()

	-- The swap's move marks were never consumed; a real delete must still
	-- read as one
	s.a.Parent = nil

	local deleted = s.take()

	expect(#deleted.removals == 1 and sameRef(deleted.removals[1], s.aId), "a later delete of a moved child pushes its removal")
	expect(current ~= nil and current.Parent == s.server, "the replacement is in place")
end

print("")

if #Fakes.warnings > 0 then
	print("warnings logged by the plugin code:")

	for _, warning in Fakes.warnings do
		print("  " .. warning)
	end
end

if failures > 0 then
	error(`{failures} of {checks} checks failed`, 0)
end

print(`all {checks} checks passed`)
