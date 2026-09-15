-- Syncback keeps a subtree whole when the same instances leave and come
-- back: a reparent (remove-then-add of the same objects), a delete followed
-- by Undo, and a daemon-applied removal. Runs the real Watcher, ReadProcessor
-- and Tree over the fakes in harness/; `Fakes`, `ref` and `require` arrive
-- from harness/loader.luau.
--
-- The regression this guards: `onRemove` untracked only the removed instance,
-- so when the same objects were re-added `onAdd` skipped every still-tracked
-- descendant as an echo and the addition reached disk without its children —
-- while the removal it followed had already deleted their files.

local Tree = require(ref("Tree"))
local ReadProcessor = require(ref("Read"))
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

local function trackedCount(tree): number
	local count = 0

	for _ in tree.instanceMap do
		count += 1
	end

	return count
end

local function liveInstances(root): number
	return 1 + #root:GetDescendants()
end

local function childNames(snapshot): string
	local names = {}

	for _, child in snapshot.children do
		table.insert(names, child.name)
	end

	table.sort(names)

	return table.concat(names, ",")
end

local function childNamed(snapshot, name)
	for _, child in snapshot.children do
		if child.name == name then
			return child
		end
	end

	return nil
end

--- One session: a hydrated tree over
---   game
---   ├ ReplicatedStorage
---   │ └ Shared (Folder)
---   │   ├ A (ModuleScript)
---   │   └ B (Script)
---   │     └ C (ModuleScript)
---   └ ServerScriptService
--- with a running watcher feeding ReadProcessor.onEvent, aggregated the way
--- Core's syncback loop aggregates.
local function session()
	local game = Fakes.installGlobals()
	local replicated = Fakes.new("ReplicatedStorage", nil, game)
	local server = Fakes.new("ServerScriptService", nil, game)

	local shared = Fakes.new("Folder", "Shared", replicated)
	local a = Fakes.new("ModuleScript", "A", shared)
	a.Source = "return 1"
	local b = Fakes.new("Script", "B", shared)
	b.Source = "print(1)"
	local c = Fakes.new("ModuleScript", "C", b)
	c.Source = "return 3"

	local tree = Tree.new()

	local function hydrate(instance)
		tree:insertInstance(instance, generateRef(), { keepUnknowns = false })

		for _, child in instance:GetChildren() do
			hydrate(child)
		end
	end

	hydrate(game)

	local read = ReadProcessor.new(tree)
	local watcher = Watcher.new()
	local aggregate = Changes.new()

	watcher.signal:Connect(function(event)
		aggregate:join(read:onEvent(event), true)
	end)

	watcher:start({ replicated, server })

	return {
		game = game,
		replicated = replicated,
		server = server,
		shared = shared,
		a = a,
		b = b,
		c = c,
		tree = tree,
		read = read,
		watcher = watcher,
		--- Everything pushed since the last take, as one aggregate
		take = function()
			local changes = aggregate
			aggregate = Changes.new()
			return changes
		end,
	}
end

--------------------------------------------------------------------------------

print("scenario: reparent a folder with scripts to another service")
do
	local s = session()
	local oldId = s.tree:getId(s.shared)
	local oldAId = s.tree:getId(s.a)

	s.shared.Parent = s.server

	local changes = s.take()

	expect(#changes.removals == 1, "one removal is pushed")
	expect(#changes.removals == 1 and sameRef(changes.removals[1], oldId), "the removal names the folder's old ref")
	expect(#changes.additions == 1, "one addition is pushed")

	local addition = changes.additions[1]

	if addition then
		expect(sameRef(addition.parent, s.tree:getId(s.server)), "the addition is parented under ServerScriptService")
		expect(
			childNames(addition) == "A,B",
			`the addition carries the folder's children (got "{childNames(addition)}")`
		)

		local b = childNamed(addition, "B")

		expect(b ~= nil and childNames(b) == "C", "the nested script keeps its own child")
		expect(
			b ~= nil and b.properties.Source ~= nil and b.properties.Source.String == "print(1)",
			"the nested script carries its Source"
		)
	end

	local newAId = s.tree:getId(s.a)

	expect(newAId ~= nil, "A is tracked after the move")
	expect(newAId ~= nil and not sameRef(newAId, oldAId), "A is tracked under a fresh ref, not its pre-move orphan")
	expect(
		trackedCount(s.tree) == liveInstances(s.game),
		`the tree holds exactly the live instances ({trackedCount(s.tree)} tracked, {liveInstances(s.game)} live)`
	)
	expect(s.a.Changed:liveCount() == 1, `A's Changed signal has one watcher connection ({s.a.Changed:liveCount()})`)
	expect(s.c.Changed:liveCount() == 1, `C's Changed signal has one watcher connection ({s.c.Changed:liveCount()})`)

	-- A later edit on a moved child must reach disk as an update on the ref
	-- the daemon knows
	s.c.Source = "return 4"

	local later = s.take()

	expect(#later.updates == 1, "an edit on the moved nested script pushes one update")
	expect(#later.updates == 1 and sameRef(later.updates[1].id, s.tree:getId(s.c)), "that update names C's current ref")
end

print("scenario: delete a folder, then undo (the same instances come back)")
do
	local s = session()

	s.shared.Parent = nil

	local removed = s.take()

	expect(#removed.removals == 1 and #removed.additions == 0, "the delete pushes exactly one removal")
	expect(
		trackedCount(s.tree) == liveInstances(s.game),
		`nothing under the deleted folder stays tracked ({trackedCount(s.tree)} tracked, {liveInstances(s.game)} live)`
	)

	s.shared.Parent = s.replicated

	local restored = s.take()
	local addition = restored.additions[1]

	expect(#restored.additions == 1, "the undo pushes one addition")
	expect(
		addition ~= nil and childNames(addition) == "A,B",
		`the restored folder carries its children (got "{addition and childNames(addition) or "-"}")`
	)
	expect(s.a.Changed:liveCount() == 1, "A is wired exactly once after the undo")
end

print("scenario: a daemon-applied removal leaves no orphans behind")
do
	-- What WriteProcessor.applyRemoval does to the tree when the daemon
	-- deleted the folder's file on disk, without loading WriteProcessor
	local s = session()
	local id = s.tree:getId(s.shared)

	s.tree:removeSubtree(s.shared)
	s.shared.Parent = nil
	s.take()

	expect(s.tree:getInstance(id) == nil, "the folder is untracked")
	expect(
		trackedCount(s.tree) == liveInstances(s.game),
		`its descendants are untracked with it ({trackedCount(s.tree)} tracked, {liveInstances(s.game)} live)`
	)
end

print("scenario: reparenting keeps an --!ignoresync script out")
do
	local s = session()
	local d = Fakes.new("ModuleScript", "D", s.shared)
	d.Source = "--!ignoresync\nreturn 'bulk'"
	s.take()

	s.shared.Parent = s.server

	local changes = s.take()
	local addition = changes.additions[1]

	expect(
		addition ~= nil and childNames(addition) == "A,B",
		`the ignored script stays out of the moved folder's addition (got "{addition and childNames(addition) or "-"}")`
	)
	expect(s.tree:getId(d) == nil, "the ignored script stays untracked")
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
