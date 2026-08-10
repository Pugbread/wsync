--!strict
-- Inspection ops: `get`, `ls`, `tree`, `props` (get.json, ls.json, tree.json,
-- props.json). All read-only and bounded; truncation is always reported.

local ScriptEditorService = game:GetService("ScriptEditorService")
local CollectionService = game:GetService("CollectionService")

local Error = require(script.Parent.Parent.Error)
local Codec = require(script.Parent.Parent.Codec)
local Paths = require(script.Parent.Parent.Paths)
local Reflection = require(script.Parent.Parent.Reflection)

type Args = { [string]: any }

local Inspect = {}

Inspect.LIMITS = {
	treeDefaultDepth = 3,
	-- Each tree level nests two JSON levels (node -> children[]); 32 keeps
	-- the deepest response inside the codec's 64-level envelope bound.
	treeMaxDepth = 32,
	treeMaxNodes = 20000,
	maxChildren = 10000,
	maxPropNameBytes = 128,
}

--------------------------------------------------------------------------------
-- Helpers
--------------------------------------------------------------------------------

--- Read a script's source through ScriptEditorService so an open editor
--- draft is reflected (Design §10.6: raw `.Source` is unreliable while a
--- draft exists). Falls back to the raw property.
local function readScriptSource(instance: Instance): string
	local ok, source = pcall(function()
		return ScriptEditorService:GetEditorSource(instance :: any)
	end)

	if ok and type(source) == "string" then
		return source
	end

	return (instance :: any).Source
end

local function readProperty(instance: Instance, name: string): (boolean, any)
	if name == "Source" and instance:IsA("LuaSourceContainer") then
		return pcall(readScriptSource, instance)
	end

	return pcall(function()
		return (instance :: any)[name]
	end)
end

--- Encode with a shared budget, degrading to an explicit `Skipped` marker on
--- codec failure (budget exhaustion) — a multi-property view must stay
--- deliverable when a single huge value trips the limits, and the omission
--- must never be silent.
local function encodeOrMarker(value: any, budget: Codec.Budget): any
	local ok, encodedOrError = pcall(Codec.encode, value, budget)

	if ok then
		return encodedOrError
	end

	local reason = if Error.is(encodedOrError) then encodedOrError.message else tostring(encodedOrError)

	return { __type = "Skipped", reason = Error.truncateMessage(reason) }
end

local function encodedAttributes(instance: Instance, budget: Codec.Budget): { [string]: any }?
	local ok, attributes = pcall(function()
		return instance:GetAttributes()
	end)

	if not ok or type(attributes) ~= "table" or next(attributes) == nil then
		-- Omitted entirely when empty: HttpService:JSONEncode renders an
		-- empty Luau table as `[]`, which would flip the JSON type.
		return nil
	end

	local out: { [string]: any } = {}

	for name, value in attributes do
		out[tostring(name)] = encodeOrMarker(value, budget)
	end

	return out
end

local function sortedTags(instance: Instance): { string }?
	local ok, tags = pcall(function()
		return CollectionService:GetTags(instance)
	end)

	if not ok or type(tags) ~= "table" or #tags == 0 then
		return nil
	end

	local out = table.create(#tags)

	for _, tag in tags do
		table.insert(out, tostring(tag))
	end

	table.sort(out)

	return out
end

--- Direct children as `{name, class}` records, bounded. Child identity reads
--- are pcall-guarded: some DataModel services refuse access from plugins.
local function childRecords(instance: Instance): ({ { name: string, class: string } }, number, boolean)
	local children = instance:GetChildren()
	local out = {}
	local total = 0

	for _, child in children do
		local ok, record = pcall(function()
			return { name = child.Name, class = child.ClassName }
		end)

		if not ok then
			continue
		end

		total += 1

		if total <= Inspect.LIMITS.maxChildren then
			table.insert(out, record)
		end
	end

	return out, total, total > Inspect.LIMITS.maxChildren
end

local function requirePath(args: Args): Instance
	return Paths.resolve(args.path)
end

local function validatePropName(prop: any): string
	if type(prop) ~= "string" or prop == "" or #prop > Inspect.LIMITS.maxPropNameBytes then
		error(
			Error.new(Error.INVALID_ARGUMENT, `prop must be a string of 1-{Inspect.LIMITS.maxPropNameBytes} bytes`),
			0
		)
	end

	return prop
end

--------------------------------------------------------------------------------
-- Ops
--------------------------------------------------------------------------------

Inspect.ops = {} :: { [string]: (args: Args, ctx: any) -> any }

--- `get {path, prop?}` — one tagged-encoded property, or the full
--- inspectable view (class, name, reflected properties, attributes, tags).
function Inspect.ops.get(args: Args, _ctx)
	local instance = requirePath(args)

	if args.prop ~= nil then
		local prop = validatePropName(args.prop)
		local ok, value = readProperty(instance, prop)

		if not ok then
			local message = tostring(value)

			if message:find("is not a valid member", 1, true) then
				error(Error.new(Error.NOT_FOUND, `property '{prop}' is not a member of class {instance.ClassName}`), 0)
			end

			error(Error.new(Error.PLUGIN_ERROR, `failed to read property '{prop}': {message}`), 0)
		end

		-- Single-property reads fail hard on codec limits (a >4 MiB Source
		-- is INVALID_ARGUMENT, not a silent cut) — chunked source transfer
		-- is a later-slice op.
		return Codec.encode(value)
	end

	local budget = Codec.newBudget()
	local properties: { [string]: any } = {}
	local propertyList, propertySource = Reflection.classProperties(instance.ClassName)

	if propertyList ~= nil then
		for _, info in propertyList do
			local ok, value = readProperty(instance, info.name)

			-- Unreadable (write-only, security-locked) and nil-valued
			-- properties are omitted, matching the Ro-Sync view
			if ok and value ~= nil then
				properties[info.name] = encodeOrMarker(value, budget)
			end
		end
	end

	local children, totalChildren, childrenTruncated = childRecords(instance)
	local path = Paths.pathOf(instance)

	local view: { [string]: any } = {
		path = path,
		class = instance.ClassName,
		name = instance.Name,
		properties = properties,
		propertySource = propertySource,
		children = children,
		childrenCount = totalChildren,
	}

	if childrenTruncated then
		view.childrenTruncated = true
	end

	-- Empty maps/arrays are omitted (JSONEncode `[]`-vs-`{}` ambiguity)
	view.attributes = encodedAttributes(instance, budget)
	view.tags = sortedTags(instance)

	if next(properties) == nil then
		view.properties = nil
	end

	return view
end

--- `ls {path}` — direct children; `""` lists the DataModel services.
function Inspect.ops.ls(args: Args, _ctx)
	local instance = Paths.resolve(if args.path == nil then "" else args.path)
	local children, total, truncated = childRecords(instance)

	return {
		path = (Paths.pathOf(instance)),
		class = instance.ClassName,
		children = children,
		count = #children,
		total = total,
		truncated = truncated,
	}
end

--- `tree {path, depth=3}` — class+name tree, depth- and node-capped.
function Inspect.ops.tree(args: Args, _ctx)
	local instance = Paths.resolve(if args.path == nil then "" else args.path)

	local depth = Inspect.LIMITS.treeDefaultDepth

	if args.depth ~= nil then
		if type(args.depth) ~= "number" or args.depth < 0 then
			error(Error.new(Error.INVALID_ARGUMENT, "depth must be a non-negative number"), 0)
		end

		depth = math.floor(args.depth)
	end

	local depthClamped = depth > Inspect.LIMITS.treeMaxDepth

	if depthClamped then
		depth = Inspect.LIMITS.treeMaxDepth
	end

	local nodesLeft = Inspect.LIMITS.treeMaxNodes
	local visited = 0
	local anyTruncated = false

	local function build(current: Instance, remaining: number): { [string]: any }
		visited += 1

		local node: { [string]: any } = {
			name = current.Name,
			class = current.ClassName,
		}

		local ok, children = pcall(function()
			return current:GetChildren()
		end)

		if not ok then
			return node
		end

		if remaining <= 0 then
			if #children > 0 then
				node.truncated = true
				anyTruncated = true
			end

			return node
		end

		local out = {}

		for _, child in children do
			if nodesLeft <= 0 then
				node.truncated = true
				anyTruncated = true
				break
			end

			nodesLeft -= 1
			table.insert(out, build(child, remaining - 1))
		end

		if #out > 0 then
			node.children = out
		end

		return node
	end

	nodesLeft -= 1 -- the root itself
	local root = build(instance, depth)

	local result: { [string]: any } = {
		root = root,
		depth = depth,
		visitedNodes = visited,
		truncated = anyTruncated,
	}

	if depthClamped then
		result.depthClamped = true
	end

	return result
end

--- `props {path}` — reflected properties with values, grouped by category
--- where the metadata source provides one. `Ref`-typed and SharedString
--- values are omitted per props.json.
function Inspect.ops.props(args: Args, _ctx)
	local instance = requirePath(args)
	local propertyList, source = Reflection.classProperties(instance.ClassName)

	if propertyList == nil then
		error(Error.new(Error.NOT_FOUND, `no property metadata for class '{instance.ClassName}'`), 0)
	end

	local budget = Codec.newBudget()
	local records = {}

	for _, info in propertyList do
		-- Metadata-level filter (rbx-dom types); the typeof filter below
		-- covers sources without type metadata
		if info.type == "Ref" or info.type == "SharedString" or info.type == "BinaryString" then
			continue
		end

		local ok, value = readProperty(instance, info.name)

		if not ok or value == nil then
			continue
		end

		local kind = typeof(value)

		if kind == "Instance" or kind == "SharedString" then
			continue
		end

		table.insert(records, {
			name = info.name,
			category = info.category,
			type = info.type,
			value = encodeOrMarker(value, budget),
		})
	end

	table.sort(records, function(a, b)
		if a.category ~= b.category then
			return a.category < b.category
		end

		return a.name < b.name
	end)

	return {
		path = (Paths.pathOf(instance)),
		class = instance.ClassName,
		name = instance.Name,
		source = source,
		properties = records,
		count = #records,
	}
end

return Inspect
