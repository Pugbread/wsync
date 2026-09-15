-- The Studio-side exclusion markers keep an instance in Studio: the
-- `AvoidSync` attribute (any class) and the `--!ignoresync` directive
-- (scripts), on one plumbing in ReadProcessor. Runs the real Watcher,
-- ReadProcessor, Tree and SyncScope over the fakes in harness/; `Fakes`,
-- `ref` and `require` arrive from harness/loader.luau.
--
-- What is covered: the Attributes-driven leave/return transitions, the strict
-- boolean reading of the attribute, an instance that is born marked, edits
-- under a kept-out subtree, and the arrival pass that untracks a daemon
-- change carrying a marker without pushing anything back.

local Tree = require(ref("Tree"))
local ReadProcessor = require(ref("Read"))
local Watcher = require(ref("Watcher"))
local Changes = require(ref("Changes"))
local SyncScope = require(ref("SyncScope"))
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

local function childNames(snapshot): string
	local names = {}

	for _, child in snapshot.children do
		table.insert(names, child.name)
	end

	table.sort(names)

	return table.concat(names, ",")
end

local function isEmpty(changes): boolean
	return #changes.additions == 0 and #changes.updates == 0 and #changes.removals == 0
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

	local function track(instance)
		tree:insertInstance(instance, generateRef(), { keepUnknowns = false })

		for _, child in instance:GetChildren() do
			track(child)
		end
	end

	track(game)

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
		--- What WriteProcessor leaves behind for an arrival: the subtree,
		--- tracked, parented into the DataModel
		arrive = function(instance, parent)
			track(instance)
			instance.Parent = parent
		end,
		--- Everything pushed since the last take, as one aggregate
		take = function()
			local changes = aggregate
			aggregate = Changes.new()
			return changes
		end,
	}
end

--------------------------------------------------------------------------------

print("scenario: setting AvoidSync on a synced folder takes its subtree out of sync")
do
	local s = session()
	local oldId = s.tree:getId(s.shared)

	s.shared:SetAttribute(SyncScope.AVOID_SYNC_ATTRIBUTE, true)

	local changes = s.take()

	expect(#changes.removals == 1, "one removal is pushed")
	expect(#changes.removals == 1 and sameRef(changes.removals[1], oldId), "the removal names the folder's ref")
	expect(#changes.additions == 0 and #changes.updates == 0, "nothing else is pushed")
	expect(s.tree:getId(s.shared) == nil, "the folder is untracked")
	expect(s.tree:getId(s.a) == nil and s.tree:getId(s.c) == nil, "its descendants are untracked with it")
	expect(s.read.excluded[s.shared] == true, "the folder is remembered as kept in Studio")
	expect(s.read:excludedCount() == 1, `one instance is counted as kept in Studio ({s.read:excludedCount()})`)

	s.a.Source = "return 2"

	expect(isEmpty(s.take()), "an edit under the kept-out folder pushes nothing")

	local e = Fakes.new("ModuleScript", "E", s.shared)
	e.Source = "return 5"

	expect(isEmpty(s.take()), "an instance added under the kept-out folder pushes nothing")

	s.shared:SetAttribute(SyncScope.AVOID_SYNC_ATTRIBUTE, nil)

	local back = s.take()
	local addition = back.additions[1]

	expect(#back.additions == 1 and #back.removals == 0, "clearing the attribute pushes one addition")
	expect(
		addition ~= nil and sameRef(addition.parent, s.tree:getId(s.replicated)),
		"the addition is parented under ReplicatedStorage"
	)
	expect(
		addition ~= nil and childNames(addition) == "A,B,E",
		`the addition carries everything now under the folder (got "{addition and childNames(addition) or "-"}")`
	)
	expect(s.tree:getId(s.a) ~= nil and s.tree:getId(e) ~= nil, "the folder's contents are tracked again")
	expect(s.read:excludedCount() == 0, "nothing is counted as kept in Studio any more")

	s.a.Source = "return 3"

	local later = s.take()

	expect(#later.updates == 1 and sameRef(later.updates[1].id, s.tree:getId(s.a)), "a later edit syncs again")
end

print("scenario: only the boolean true counts as AvoidSync")
do
	local s = session()

	for _, value in { "true", 1, false } do
		s.shared:SetAttribute(SyncScope.AVOID_SYNC_ATTRIBUTE, value)

		local changes = s.take()

		expect(
			#changes.removals == 0 and s.tree:getId(s.shared) ~= nil,
			`AvoidSync = {typeof(value)} {tostring(value)} keeps the folder in sync`
		)
	end

	expect(SyncScope.attributeAvoidsSync(true), "the rule accepts true")
	expect(not SyncScope.attributeAvoidsSync(false), "the rule rejects false")
	expect(not SyncScope.attributeAvoidsSync("true"), "the rule rejects the string 'true'")
	expect(not SyncScope.attributeAvoidsSync(1), "the rule rejects 1")
	expect(not SyncScope.attributeAvoidsSync(nil), "the rule rejects an absent attribute")
end

print("scenario: an instance born with AvoidSync never enters sync, and joins when cleared")
do
	local s = session()
	local data = Fakes.new("Folder", "Data")
	data:SetAttribute(SyncScope.AVOID_SYNC_ATTRIBUTE, true)
	data.Parent = s.shared

	expect(isEmpty(s.take()), "parenting it into a synced folder pushes nothing")
	expect(s.tree:getId(data) == nil, "it is untracked")
	expect(s.read.excluded[data] == true, "it is remembered as kept in Studio")

	local blob = Fakes.new("ModuleScript", "Blob", data)
	blob.Source = "return 'bulk'"

	expect(isEmpty(s.take()), "a script added under it pushes nothing")

	data:SetAttribute(SyncScope.AVOID_SYNC_ATTRIBUTE, nil)

	local changes = s.take()
	local addition = changes.additions[1]

	expect(#changes.additions == 1, "clearing the attribute pushes one addition")
	expect(
		addition ~= nil and addition.name == "Data" and childNames(addition) == "Blob",
		`the addition is the folder with its script (got "{addition and childNames(addition) or "-"}")`
	)
	expect(addition ~= nil and sameRef(addition.parent, s.tree:getId(s.shared)), "it is parented under Shared")
end

print("scenario: AvoidSync on a script works like its directive, driven by the attribute event")
do
	local s = session()
	local oldId = s.tree:getId(s.b)

	s.b:SetAttribute(SyncScope.AVOID_SYNC_ATTRIBUTE, true)

	local changes = s.take()

	expect(#changes.removals == 1 and sameRef(changes.removals[1], oldId), "the removal names the script's ref")
	expect(s.tree:getId(s.c) == nil, "the script's child is untracked with it")

	s.c.Source = "return 4"

	expect(isEmpty(s.take()), "an edit on the child pushes nothing")

	s.b:SetAttribute(SyncScope.AVOID_SYNC_ATTRIBUTE, nil)

	local back = s.take()
	local addition = back.additions[1]

	expect(
		addition ~= nil and addition.name == "B" and childNames(addition) == "C",
		"clearing it pushes the script back with its child"
	)
	expect(
		addition ~= nil and addition.properties.Source ~= nil and addition.properties.Source.String == "print(1)",
		"the returning script carries its Source"
	)
end

print("scenario: an arrival carrying AvoidSync is untracked at once, and nothing is pushed back")
do
	local s = session()
	local incoming = Fakes.new("Folder", "Incoming")
	incoming:SetAttribute(SyncScope.AVOID_SYNC_ATTRIBUTE, true)
	local inner = Fakes.new("ModuleScript", "Inner", incoming)
	inner.Source = "return 'from disk'"

	s.arrive(incoming, s.shared)

	expect(isEmpty(s.take()), "the arrival itself pushes nothing (it is already tracked)")

	s.read:excludeArrivals(incoming)

	expect(s.tree:getId(incoming) == nil, "the arrival is untracked")
	expect(s.tree:getId(inner) == nil, "its child is untracked with it")
	expect(s.read.excluded[incoming] == true, "it is remembered as kept in Studio")
	expect(isEmpty(s.take()), "no removal goes back to the daemon")

	inner.Source = "return 'edited in Studio'"

	expect(isEmpty(s.take()), "a Studio edit under it pushes nothing")

	incoming:SetAttribute(SyncScope.AVOID_SYNC_ATTRIBUTE, nil)

	local back = s.take()
	local addition = back.additions[1]

	expect(
		addition ~= nil and addition.name == "Incoming" and childNames(addition) == "Inner",
		"clearing the attribute in Studio syncs the arrival back"
	)
end

print("scenario: the arrival pass finds a marker below the arrived root, and above it")
do
	local s = session()

	-- Below: a plain folder arrives with one marked child and one plain child
	local root = Fakes.new("Folder", "Root")
	local marked = Fakes.new("Folder", "Marked", root)
	marked:SetAttribute(SyncScope.AVOID_SYNC_ATTRIBUTE, true)
	local plain = Fakes.new("ModuleScript", "Plain", root)

	s.arrive(root, s.shared)
	s.take()
	s.read:excludeArrivals(root)

	expect(s.tree:getId(root) ~= nil, "the plain root stays tracked")
	expect(s.tree:getId(plain) ~= nil, "the plain child stays tracked")
	expect(s.tree:getId(marked) == nil and s.read.excluded[marked] == true, "the marked child is untracked")

	-- Above: a live hydrate paired a marked parent while adding its leaf; the
	-- pass runs for the leaf and must cut at the outermost carrier
	local hydrated = Fakes.new("Folder", "Hydrated")
	hydrated:SetAttribute(SyncScope.AVOID_SYNC_ATTRIBUTE, true)
	local leaf = Fakes.new("ModuleScript", "Leaf", hydrated)

	s.arrive(hydrated, s.shared)
	s.take()
	s.read:excludeArrivals(leaf)

	expect(s.tree:getId(hydrated) == nil and s.read.excluded[hydrated] == true, "the marked parent is untracked")
	expect(s.tree:getId(leaf) == nil, "the leaf is untracked with it")
	expect(s.tree:getId(s.shared) ~= nil, "the walk stops at the marker: Shared stays tracked")
	expect(isEmpty(s.take()), "nothing is pushed back for either")
end

print("scenario: a script that arrives carrying --!ignoresync is untracked at once")
do
	local s = session()
	local f = Fakes.new("ModuleScript", "F")
	f.Source = "--!ignoresync\nreturn {}"

	s.arrive(f, s.shared)
	s.take()
	s.read:excludeArrivals(f)

	expect(s.tree:getId(f) == nil and s.read.excluded[f] == true, "the script is untracked and remembered")
	expect(isEmpty(s.take()), "no removal goes back to the daemon")

	f.Source = "return {}"

	local back = s.take()

	expect(#back.additions == 1 and back.additions[1].name == "F", "deleting the directive in Studio syncs it back")
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
