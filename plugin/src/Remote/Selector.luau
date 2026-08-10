--!strict
-- Query selector matcher for the `query` remote op (Design §5.4, query.json).
--
-- Selectors are `/`-separated segment patterns matched against Studio *names*
-- rooted at the DataModel: a literal segment matches an instance whose Name is
-- exactly that string, `*` matches exactly one segment of any name, and `**`
-- matches zero or more segments. There are no partial-segment globs in v1
-- (`Work*` is a literal five-character name), mirroring Ro-Sync's matcher.
--
-- The matcher is an NFA over pattern positions so tree walks can prune dead
-- subtrees: a state set is the set of pattern indices still reachable, and an
-- empty set means no descendant of the current node can ever match.
--
-- This module is pure Luau (no Roblox APIs) so it stays testable with a
-- standalone `luau` binary; keep it dependency-free.

local Selector = {}

Selector.MAX_SELECTOR_BYTES = 4096
Selector.MAX_SEGMENTS = 128

export type Pattern = { string }
-- Sorted, deduplicated pattern indices in 1..#pattern+1; index #pattern+1
-- means "the whole pattern is consumed" (a match).
export type StateSet = { number }

-- Matches Remote/Error.luau's RemoteError shape (kept inline so this module
-- has no requires and loads under standalone Luau).
local function fail(message: string): never
	error({ __remoteError = true, code = "INVALID_ARGUMENT", message = message }, 0)
end

--- Split and validate a selector string. Raises INVALID_ARGUMENT on an empty
--- or oversized selector. Empty segments (`//`, leading/trailing `/`) collapse.
function Selector.parse(selector: string): Pattern
	if type(selector) ~= "string" or selector == "" then
		fail("selector must be a non-empty string")
	end

	if #selector > Selector.MAX_SELECTOR_BYTES then
		fail(`selector exceeds {Selector.MAX_SELECTOR_BYTES} bytes`)
	end

	local pattern: Pattern = {}

	for segment in selector:gmatch("[^/]+") do
		table.insert(pattern, segment)
	end

	if #pattern == 0 then
		fail("selector contains no path segments")
	end

	if #pattern > Selector.MAX_SEGMENTS then
		fail(`selector exceeds {Selector.MAX_SEGMENTS} path segments`)
	end

	return pattern
end

-- Epsilon closure: every `**` can match zero segments, so a state pointing at
-- one also reaches the state past it.
local function close(pattern: Pattern, indices: { [number]: boolean })
	local changed = true

	while changed do
		changed = false

		for index in indices do
			if pattern[index] == "**" and not indices[index + 1] then
				indices[index + 1] = true
				changed = true
			end
		end
	end
end

local function toSortedSet(indices: { [number]: boolean }): StateSet
	local out: StateSet = {}

	for index in indices do
		table.insert(out, index)
	end

	table.sort(out)

	return out
end

--- The state set before any segment is consumed (the DataModel root).
function Selector.initialStates(pattern: Pattern): StateSet
	local indices = { [1] = true }
	close(pattern, indices)

	return toSortedSet(indices)
end

--- Advance the state set over one path segment (an instance Name).
function Selector.step(pattern: Pattern, states: StateSet, segment: string): StateSet
	local next: { [number]: boolean } = {}

	for _, index in states do
		local token = pattern[index]

		if token == nil then
			-- Fully-consumed state: matched at a shallower node, cannot
			-- consume further segments.
			continue
		end

		if token == "**" then
			-- `**` consumes the segment and keeps matching.
			next[index] = true
		elseif token == "*" or token == segment then
			next[index + 1] = true
		end
	end

	close(pattern, next)

	return toSortedSet(next)
end

--- Whether the node whose segments produced this state set matches the pattern.
function Selector.isMatch(pattern: Pattern, states: StateSet): boolean
	local target = #pattern + 1

	for _, index in states do
		if index == target then
			return true
		end
	end

	return false
end

--- Whether no descendant of the current node can match (prune the subtree).
function Selector.isDead(states: StateSet): boolean
	return #states == 0
end

return Selector
