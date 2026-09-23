-- The connect upload's pure protocol (Core/Divergence/Protocol): what the
-- plugin reports about the Studio side so the daemon can decide each
-- difference three ways, the empty upload that lets a clean connect record
-- the sync base, and the receipts that come back.

local Protocol = require(ref("DivergenceProtocol"))

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

-- A fake Studio: instances are plain tables, refs are strings
local function fakeEnv(instances: { [string]: any })
	local minted = 0

	return {
		encodeRef = function(value)
			return value
		end,
		mintRef = function()
			minted += 1
			return `minted{minted}`
		end,
		isRef = function(value)
			return type(value) == "string"
		end,
		getInstance = function(value)
			return instances[value]
		end,
		instanceName = function(instance)
			return instance.name
		end,
		instanceClass = function(instance)
			return instance.class
		end,
		pathOf = function(instance)
			return instance.path
		end,
		sourceHashOf = function(instance)
			return instance.hash
		end,
		descendantsOf = function(instance)
			return instance.descendants or {}, instance.truncated == true
		end,
	}
end

print("mapEntries: the Studio side of each entry")
do
	local module = { name = "A", class = "ModuleScript", path = "SSS/A", hash = string.rep("a", 64) }
	local folder = {
		name = "Dir",
		class = "Folder",
		path = "SSS/Dir",
		descendants = { { path = "B", class = "ModuleScript", sourceSha256 = string.rep("b", 64) } },
	}
	local huge = { name = "Huge", class = "Folder", path = "SSS/Huge", truncated = true }
	local parent = { name = "SSS", class = "ServerScriptService", path = "SSS" }

	local env = fakeEnv({ refA = module, refSSS = parent })

	local mapped = Protocol.mapEntries({
		additions = { { id = "refNew", parent = "refSSS", name = "New", class = "ModuleScript" } },
		updates = { { id = "refA" } },
		removals = { folder, huge },
	}, env)

	local byPath = {}

	for _, entry in mapped.entries do
		byPath[entry.instancePath] = entry
	end

	local added = byPath["SSS/New"]
	expect(added ~= nil and added.change == "add", "an addition maps to add")
	expect(added ~= nil and added.sourceSha256 == nil, "an add carries no Studio hash (Studio lacks it)")

	local updated = byPath["SSS/A"]
	expect(updated ~= nil and updated.sourceSha256 == module.hash, "an update carries Studio's source hash")

	local removed = byPath["SSS/Dir"]
	expect(removed ~= nil and removed.change == "remove", "a Studio-only instance maps to remove")
	expect(
		removed ~= nil and removed.descendants ~= nil and #removed.descendants == 1,
		"a remove root lists its synced descendants"
	)
	expect(removed ~= nil and removed.descendantsTruncated == nil, "an untruncated list says nothing")
	expect(byPath["SSS/Huge"] ~= nil and byPath["SSS/Huge"].descendantsTruncated == true, "a truncated list says so")
	expect(mapped.pendingRemovals[removed.ref] == folder, "the minted ref resolves back to the instance")
end

print("mapEntries: an env without the three-way hooks (older callers)")
do
	local env = fakeEnv({ refA = { name = "A", class = "ModuleScript", path = "SSS/A" } })
	env.sourceHashOf = nil
	env.descendantsOf = nil

	local mapped = Protocol.mapEntries({ additions = {}, updates = { { id = "refA" } }, removals = {} }, env)

	expect(#mapped.entries == 1 and mapped.entries[1].sourceSha256 == nil, "still maps, without a hash")
end

print("planChunks + nextRequest: an empty diff still uploads, with the place")
do
	local chunks = Protocol.planChunks({})
	expect(#chunks == 1 and #chunks[1] == 0, "one empty chunk")

	local upload = Protocol.newUpload(chunks, function()
		return "submission"
	end, { placeId = 11, gameId = 22 })

	local request = Protocol.nextRequest(upload)
	expect(request ~= nil and request.finalChunk == true, "the empty chunk is final")
	expect(request ~= nil and request.place ~= nil and request.place.placeId == 11, "the request names the place")
end

print("applyReceipt: clean, applied and rehydrate receipts")
do
	local function finalUpload()
		return Protocol.newUpload({ {} }, function()
			return "submission"
		end)
	end

	local clean = Protocol.applyReceipt(
		finalUpload(),
		{ ok = true, acceptedChunk = 0, nextChunk = 1, committed = true, clean = true }
	)
	expect(clean.kind == "committed", "a clean receipt commits (no choiceId needed)")

	local applied = Protocol.applyReceipt(
		finalUpload(),
		{ ok = true, acceptedChunk = 0, nextChunk = 1, committed = true, applied = true, rehydrate = true }
	)
	expect(applied.kind == "committed" and applied.rehydrate == true, "an applied receipt carries rehydrate")

	local plain = Protocol.applyReceipt(
		finalUpload(),
		{ ok = true, acceptedChunk = 0, nextChunk = 1, committed = true, applied = true }
	)
	expect(plain.kind == "committed" and plain.rehydrate == false, "no rehydrate unless asked")

	local bare = Protocol.applyReceipt(finalUpload(), { ok = true, acceptedChunk = 0, nextChunk = 1, committed = true })
	expect(bare.kind == "restart", "a committed receipt with no verdict at all is still a mismatch")
end

print(`{checks - failures}/{checks} checks passed`)

if failures > 0 then
	error(`{failures} check(s) failed`)
end
