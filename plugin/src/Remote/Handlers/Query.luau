--!strict
-- Search ops: `query`, `find`, `find_by_attr` (query.json, find.json,
-- find-attr.json). All walks are iterative (no recursion into 25k-instance
-- places), budgeted by matches, visited nodes, wall time and response bytes,
-- and report a machine-readable `truncationReason` whenever capped.

local HttpService = game:GetService("HttpService")
local CollectionService = game:GetService("CollectionService")

local Error = require(script.Parent.Parent.Error)
local Codec = require(script.Parent.Parent.Codec)
local Paths = require(script.Parent.Parent.Paths)
local Selector = require(script.Parent.Parent.Selector)

type Args = { [string]: any }

local Query = {}

Query.LIMITS = {
	maxLimit = 10000,
	defaultLimit = 5000,
	findDefaultLimit = 500,
	maxProjectedProps = 32,
	maxPropNameBytes = 128,
	defaultNodeBudget = 100000,
	maxNodeBudget = 200000,
	defaultTimeSeconds = 2,
	maxTimeSeconds = 5,
	responseBudgetBytes = 4 * 1024 * 1024,
}

--------------------------------------------------------------------------------
-- Shared validation
--------------------------------------------------------------------------------

local function invalid(message: string): never
	error(Error.new(Error.INVALID_ARGUMENT, message), 0)
end

local function clampedLimit(raw: any, default: number): number
	if raw == nil then
		return default
	end

	if type(raw) ~= "number" then
		invalid("limit must be a number")
	end

	return math.clamp(math.floor(raw), 1, Query.LIMITS.maxLimit)
end

local function nodeBudgetOf(raw: any): number
	if raw == nil then
		return Query.LIMITS.defaultNodeBudget
	end

	if type(raw) ~= "number" then
		invalid("nodeBudget must be a number")
	end

	return math.clamp(math.floor(raw), 1, Query.LIMITS.maxNodeBudget)
end

local function deadlineOf(raw: any): number
	local seconds = Query.LIMITS.defaultTimeSeconds

	if raw ~= nil then
		if type(raw) ~= "number" then
			invalid("timeoutSeconds must be a number")
		end

		seconds = math.clamp(raw, 0.05, Query.LIMITS.maxTimeSeconds)
	end

	return os.clock() + seconds
end

local function safeChildren(instance: Instance): { Instance }
	local ok, children = pcall(function()
		return instance:GetChildren()
	end)

	return if ok then children else {}
end

--------------------------------------------------------------------------------
-- query
--------------------------------------------------------------------------------

type QueryFormat = "json" | "paths" | "classes"

local function formatOf(raw: any): QueryFormat
	if raw == nil or raw == "json" then
		return "json"
	elseif raw == "paths" then
		return "paths"
	elseif raw == "classes" then
		return "classes"
	end

	invalid("format must be 'json', 'paths' or 'classes'")
	error("unreachable")
end

local function projectedProps(raw: any): { string }
	local out: { string } = {}

	if raw == nil then
		return out
	end

	if type(raw) ~= "table" then
		invalid("props must be an array of property names")
	end

	for _, name in ipairs(raw :: { any }) do
		if #out >= Query.LIMITS.maxProjectedProps then
			invalid(`query supports at most {Query.LIMITS.maxProjectedProps} projected properties`)
		end

		if type(name) ~= "string" or name == "" or #name > Query.LIMITS.maxPropNameBytes then
			invalid(`query property names must be strings of 1-{Query.LIMITS.maxPropNameBytes} bytes`)
		end

		table.insert(out, name)
	end

	return out
end

Query.ops = {} :: { [string]: (args: Args, ctx: any) -> any }

--- `query {selector, props?, attributes?, tags?, limit?, format?}` — matches
--- a `*`/`**` name selector inside Studio and optionally projects properties,
--- attributes and tags (query.json). Result shape is normative:
--- `{matches, count, truncated, truncationReason, limit, visitedNodes,
--- responseBytes}`.
function Query.ops.query(args: Args, _ctx)
	local pattern = Selector.parse(args.selector)
	local limit = clampedLimit(args.limit, Query.LIMITS.defaultLimit)
	local format = formatOf(args.format)
	local props = projectedProps(args.props)
	local includeAttributes = args.attributes == true
	local includeTags = args.tags == true
	local nodeBudget = nodeBudgetOf(args.nodeBudget)
	local deadline = deadlineOf(args.timeoutSeconds)

	local matches: { any } = {}
	local truncated = false
	local truncationReason: string? = nil
	local visitedNodes = 0
	local responseBytes = 0

	-- Returns false when the response-byte budget is exhausted
	local function appendMatch(instance: Instance, path: string): boolean
		local item: any
		local itemBytes: number

		if format == "paths" then
			item = path
			itemBytes = #path + 8
		elseif format == "classes" then
			item = { path = path, class = instance.ClassName }
			itemBytes = #path + #instance.ClassName + 24
		else
			local record: { [string]: any } = {
				path = path,
				class = instance.ClassName,
				name = instance.Name,
				childrenCount = #safeChildren(instance),
			}

			-- Fresh codec budget per match (the response budget is the
			-- byte counter); failures degrade to explicit markers.
			local budget = Codec.newBudget()

			if #props > 0 then
				local values: { [string]: any } = {}

				for _, prop in props do
					local ok, value = pcall(function()
						return (instance :: any)[prop]
					end)

					if ok then
						local encodeOk, encoded = pcall(Codec.encode, value, budget)

						if encodeOk then
							values[prop] = encoded
						else
							values[prop] = {
								__type = "Skipped",
								reason = if Error.is(encoded)
									then encoded.message
									else Error.truncateMessage(tostring(encoded)),
							}
						end
					end
				end

				if next(values) ~= nil then
					record.properties = values
				end
			end

			if includeAttributes then
				local ok, attributes = pcall(function()
					return instance:GetAttributes()
				end)

				if ok and next(attributes) ~= nil then
					local encodeOk, encoded = pcall(Codec.encode, attributes, budget)

					if encodeOk then
						record.attributes = encoded
					end
				end
			end

			if includeTags then
				local ok, tags = pcall(function()
					return CollectionService:GetTags(instance)
				end)

				if ok and #tags > 0 then
					table.sort(tags)
					record.tags = tags
				end
			end

			local encodeOk, encoded = pcall(function()
				return HttpService:JSONEncode(record)
			end)

			item = record
			itemBytes = if encodeOk then #encoded else 1024
		end

		if responseBytes + itemBytes > Query.LIMITS.responseBudgetBytes then
			truncated = true
			truncationReason = "response-bytes"
			return false
		end

		responseBytes += itemBytes
		table.insert(matches, item)

		return true
	end

	-- Iterative preorder DFS from the DataModel services; each stack entry
	-- carries its NFA state set so dead subtrees are pruned outright.
	type StackEntry = { instance: Instance, path: string, states: Selector.StateSet }

	local initial = Selector.initialStates(pattern)
	local stack: { StackEntry } = {}

	local services = safeChildren(game)

	for index = #services, 1, -1 do
		local service = services[index]
		local okName, name = pcall(function()
			return service.Name
		end)

		if not okName then
			continue
		end

		local states = Selector.step(pattern, initial, name)

		if not Selector.isDead(states) then
			table.insert(stack, { instance = service, path = name, states = states })
		end
	end

	while #stack > 0 do
		if #matches >= limit then
			truncated = true
			truncationReason = "matches"
			break
		end

		if visitedNodes >= nodeBudget then
			truncated = true
			truncationReason = "nodes"
			break
		end

		if os.clock() >= deadline then
			truncated = true
			truncationReason = "time"
			break
		end

		local entry = table.remove(stack) :: StackEntry
		visitedNodes += 1

		if Selector.isMatch(pattern, entry.states) and not appendMatch(entry.instance, entry.path) then
			break
		end

		local children = safeChildren(entry.instance)

		for index = #children, 1, -1 do
			local child = children[index]
			local okName, name = pcall(function()
				return child.Name
			end)

			if not okName then
				continue
			end

			local states = Selector.step(pattern, entry.states, name)

			if not Selector.isDead(states) then
				table.insert(stack, {
					instance = child,
					path = entry.path .. "/" .. name,
					states = states,
				})
			end
		end
	end

	return {
		matches = matches,
		count = #matches,
		truncated = truncated,
		truncationReason = truncationReason,
		limit = limit,
		visitedNodes = visitedNodes,
		responseBytes = responseBytes,
	}
end

--------------------------------------------------------------------------------
-- find / find_by_attr
--------------------------------------------------------------------------------

type Predicate = (instance: Instance) -> boolean

--- Bounded iterative walk of every descendant under `root` (root excluded),
--- collecting `{path, class, name}` records for predicate matches.
local function walkFind(root: Instance, predicate: Predicate, limit: number)
	local matches = {}
	local truncated = false
	local truncationReason: string? = nil
	local visitedNodes = 0
	local nodeBudget = Query.LIMITS.maxNodeBudget

	local stack = safeChildren(root)
	-- Reverse for stable document order (stack pops from the end)
	for index = 1, math.floor(#stack / 2) do
		stack[index], stack[#stack - index + 1] = stack[#stack - index + 1], stack[index]
	end

	while #stack > 0 do
		if #matches >= limit then
			truncated = true
			truncationReason = "matches"
			break
		end

		if visitedNodes >= nodeBudget then
			truncated = true
			truncationReason = "nodes"
			break
		end

		local instance = table.remove(stack) :: Instance
		visitedNodes += 1

		local ok, matched = pcall(predicate, instance)

		if ok and matched then
			local record = {
				path = (Paths.pathOf(instance)),
				class = instance.ClassName,
				name = instance.Name,
			}

			table.insert(matches, record)
		end

		local children = safeChildren(instance)

		for index = #children, 1, -1 do
			table.insert(stack, children[index])
		end
	end

	return {
		matches = matches,
		count = #matches,
		truncated = truncated,
		truncationReason = truncationReason,
		limit = limit,
		visitedNodes = visitedNodes,
	}
end

--- `find {class?, name?, under?}` — class = ClassName or IsA match; name =
--- substring match (find.json); scope with `under`, default whole DataModel.
function Query.ops.find(args: Args, _ctx)
	local root = Paths.resolve(if args.under == nil then "" else args.under)
	local limit = clampedLimit(args.limit, Query.LIMITS.findDefaultLimit)

	local className: string? = nil
	local nameFilter: string? = nil

	if args.class ~= nil then
		if type(args.class) ~= "string" or args.class == "" then
			invalid("class must be a non-empty string")
		end

		className = args.class
	end

	if args.name ~= nil then
		if type(args.name) ~= "string" or args.name == "" then
			invalid("name must be a non-empty string")
		end

		nameFilter = args.name
	end

	return walkFind(root, function(instance)
		if className ~= nil then
			if instance.ClassName ~= className then
				local ok, isA = pcall(function()
					return instance:IsA(className :: string)
				end)

				if not ok or not isA then
					return false
				end
			end
		end

		if nameFilter ~= nil then
			-- Substring, plain (no patterns), case-sensitive
			if string.find(instance.Name, nameFilter, 1, true) == nil then
				return false
			end
		end

		return true
	end, limit)
end

--- `find_by_attr {name, value?, under?}` — instances carrying the attribute,
--- optionally matching a value decoded through the tagged codec and compared
--- with its equality rules (find-attr.json).
function Query.ops.find_by_attr(args: Args, _ctx)
	if type(args.name) ~= "string" or args.name == "" then
		invalid("name must be a non-empty string")
	end

	local attributeName: string = args.name
	local root = Paths.resolve(if args.under == nil then "" else args.under)
	local limit = clampedLimit(args.limit, Query.LIMITS.findDefaultLimit)

	local hasTarget = args.value ~= nil
	local target: any = nil

	if hasTarget then
		-- INVALID_ARGUMENT on malformed tags, NOT_FOUND on dead InstancePaths
		target = Codec.decode(args.value)
	end

	return walkFind(root, function(instance)
		local ok, value = pcall(function()
			return instance:GetAttribute(attributeName)
		end)

		if not ok or value == nil then
			return false
		end

		return not hasTarget or Codec.equals(value, target)
	end, limit)
end

return Query
