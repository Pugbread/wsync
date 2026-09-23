-- One bad change never costs the rest of its sync batch (or the session).
-- Runs the real Processor (Read + Write halves), Tree and Error modules over
-- the fakes in harness/; `Fakes`, `ref` and `require` arrive from
-- harness/loader.luau.
--
-- The regression this guards: a removal for a ref the plugin did not know
-- raised from inside the write processor (its warning could not even be
-- formatted), the error unwound through Core's sync loop, and the plugin
-- disconnected — taking every other change of the batch with it. What must
-- hold instead: each addition, update and removal is applied on its own; a
-- failure is logged with its hex ref and skipped; an unknown-ref removal is
-- benign (debug, not warning); unknown-ref updates stay visible but bounded.

local Tree = require(ref("Tree"))
local Processor = require(ref("Processor"))
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

local function contains(haystack: string?, needle: string): boolean
	return haystack ~= nil and string.find(haystack, needle, 1, true) ~= nil
end

local function hexOf(bytes: buffer): string
	local out = {}

	for i = 0, buffer.len(bytes) - 1 do
		table.insert(out, string.format("%02x", buffer.readu8(bytes, i)))
	end

	return table.concat(out)
end

local function countContaining(lines: { string }, needle: string): number
	local count = 0

	for _, line in lines do
		if contains(line, needle) then
			count += 1
		end
	end

	return count
end

local function reset()
	table.clear(Fakes.warnings)
	table.clear(Fakes.debugs)
end

-- LiveHydrate is on in the fakes: a ref the tree does not know is looked up
-- on the daemon first, which knows nothing more here
local transport = {
	getSnapshot = function()
		return {
			expect = function()
				return nil
			end,
		}
	end,
}

--- A hydrated tree over
---   game
---   └ ServerScriptService
---     ├ Keep (ModuleScript)
---     └ Gone (ModuleScript)
local function session()
	local game = Fakes.installGlobals()
	local server = Fakes.new("ServerScriptService", nil, game)
	local keep = Fakes.new("ModuleScript", "Keep", server)
	local gone = Fakes.new("ModuleScript", "Gone", server)

	local tree = Tree.new()

	for _, instance in { game, server, keep, gone } do
		tree:insertInstance(instance, generateRef(), { keepUnknowns = false })
	end

	return {
		server = server,
		keep = keep,
		gone = gone,
		tree = tree,
		processor = Processor.new(tree, transport),
	}
end

-- Refs carrying a `%` byte followed by a letter: formatting one into a
-- warning is what used to raise
local UNKNOWN_REMOVAL = buffer.fromstring("%s%d-unknown-rm%")
local UNKNOWN_UPDATE = buffer.fromstring("%q-unknown-up-%%")

--------------------------------------------------------------------------------

print("scenario: a batch with failing and unknown changes applies everything else")
do
	local s = session()
	reset()

	local batch = {
		additions = {
			-- Instance.new refuses this class: the addition fails midway
			{
				id = generateRef(),
				parent = s.tree:getId(s.server),
				name = "Broken",
				class = "NotARealClass",
				properties = {},
				children = {},
				meta = {},
			},
			{
				id = generateRef(),
				parent = s.tree:getId(s.server),
				name = "Added",
				class = "ModuleScript",
				properties = { Source = { String = "return 'added'" } },
				children = {},
				meta = {},
			},
		},
		updates = {
			{ id = UNKNOWN_UPDATE, properties = { Source = { String = "lost" } } },
			{ id = s.tree:getId(s.keep), properties = { Source = { String = "return 'kept'" } } },
		},
		removals = { UNKNOWN_REMOVAL, s.tree:getId(s.gone) },
	}

	local ok, err = pcall(function()
		s.processor:applyChanges(batch)
	end)

	expect(ok, `applying the batch does not throw{if ok then "" else ` ({err})`}`)

	local added = s.server:FindFirstChildWhichIsA("ModuleScript") and s.server.Added

	expect(added ~= nil and added.Source == "return 'added'", "the good addition landed")
	expect(s.keep.Source == "return 'kept'", "the good update landed")
	expect(s.gone.Parent == nil, "the good removal landed")
	expect(s.server.Broken == nil, "the failed addition left nothing behind in Studio")

	expect(
		countContaining(Fakes.warnings, "Skipped the addition of 'Broken' (NotARealClass)") == 1,
		"the failed addition is logged once, by name and class"
	)
	expect(
		countContaining(Fakes.warnings, hexOf(UNKNOWN_UPDATE)) == 1,
		"the unknown-ref update is a warning, with its hex ref"
	)
	expect(countContaining(Fakes.warnings, hexOf(UNKNOWN_REMOVAL)) == 0, "the unknown-ref removal is not a warning")
	expect(countContaining(Fakes.debugs, hexOf(UNKNOWN_REMOVAL)) == 1, "it is a debug line, with its hex ref")

	local rawPercent = 0

	for _, line in Fakes.warnings do
		if contains(line, "%s%d") or contains(line, "%q-") then
			rawPercent += 1
		end
	end

	expect(rawPercent == 0, "no raw ref bytes reach a log line")
end

print("scenario: a burst of unknown-ref updates is bounded")
do
	local s = session()
	reset()

	local updates = {}

	for _ = 1, 40 do
		table.insert(updates, { id = generateRef(), properties = { Source = { String = "x" } } })
	end

	s.processor:applyChanges({ additions = {}, updates = updates, removals = {} })

	local individual = countContaining(Fakes.warnings, "Tried to update an instance that does not exist")
	local summaries = countContaining(Fakes.warnings, "more updates for instances that do not exist")

	expect(individual == 5, `the first 5 are warned individually (got {individual})`)
	expect(summaries == 1 and countContaining(Fakes.warnings, "…and 35 more") == 1, "the other 35 are summed up in one line")

	reset()

	s.processor:applyChanges({
		additions = {},
		updates = { { id = generateRef(), properties = {} } },
		removals = {},
	})

	expect(
		#Fakes.warnings == 1 and countContaining(Fakes.warnings, "…and 1 more update ") == 1,
		"within the same window, a later batch only adds a summary line"
	)
end

print("")

if failures > 0 then
	print("warnings logged by the plugin code:")

	for _, warning in Fakes.warnings do
		print("  " .. warning)
	end

	error(`{failures} of {checks} checks failed`, 0)
end

print(`all {checks} checks passed`)
