--!strict
-- Class/enum introspection ops: `class_info`, `enums`, `enum_list`
-- (classinfo.json, enums.json, enum.json).

local Error = require(script.Parent.Parent.Error)
local Reflection = require(script.Parent.Parent.Reflection)

type Args = { [string]: any }

local ClassInfo = {}

ClassInfo.ops = {} :: { [string]: (args: Args, ctx: any) -> any }

--- `class_info {class}` — properties (grouped by category) and methods, from
--- Studio reflection when available, the bundled rbx-dom database otherwise,
--- or the baked common-class table as the last resort.
function ClassInfo.ops.class_info(args: Args, _ctx)
	if type(args.class) ~= "string" or args.class == "" then
		error(Error.new(Error.INVALID_ARGUMENT, "class must be a non-empty string"), 0)
	end

	local className: string = args.class
	local info = Reflection.classInfo(className)

	if info == nil then
		error(Error.new(Error.NOT_FOUND, `no class metadata available for '{className}'`), 0)
	end

	local properties = table.clone(info.properties)

	table.sort(properties, function(a, b)
		if a.category ~= b.category then
			return a.category < b.category
		end

		return a.name < b.name
	end)

	local result: { [string]: any } = {
		class = className,
		source = info.source,
		superclass = info.superclass,
		properties = properties,
		propertyCount = #properties,
		methods = info.methods,
		methodCount = #info.methods,
	}

	if info.events ~= nil and #info.events > 0 then
		result.events = info.events
		result.eventCount = #info.events
	end

	return result
end

--- `enums` — every Enum type name exposed by this Studio session.
function ClassInfo.ops.enums(_args: Args, _ctx)
	local ok, enumTypes = pcall(function()
		return Enum:GetEnums()
	end)

	if not ok or type(enumTypes) ~= "table" then
		error(Error.new(Error.PLUGIN_ERROR, "Enum:GetEnums() is not available in this Studio build"), 0)
	end

	local names = {}

	for _, enumType in enumTypes do
		local name = tostring(enumType)

		-- tostring renders either "Enum.Material" or bare "Material"
		-- depending on build; normalize to the bare name
		if name:sub(1, 5) == "Enum." then
			name = name:sub(6)
		end

		table.insert(names, name)
	end

	table.sort(names)

	return { enums = names, count = #names }
end

--- `enum_list {name}` — the items of one Enum type as `{name, value}`.
function ClassInfo.ops.enum_list(args: Args, _ctx)
	if type(args.name) ~= "string" or args.name == "" then
		error(Error.new(Error.INVALID_ARGUMENT, "name must be a non-empty string"), 0)
	end

	local enumName: string = args.name

	local okEnum, enumType = pcall(function()
		return (Enum :: any)[enumName]
	end)

	if not okEnum or enumType == nil then
		error(Error.new(Error.NOT_FOUND, `unknown enum '{enumName}'`), 0)
	end

	local okItems, enumItems = pcall(function()
		return enumType:GetEnumItems()
	end)

	if not okItems or type(enumItems) ~= "table" then
		error(Error.new(Error.PLUGIN_ERROR, `failed to read the items of Enum.{enumName}`), 0)
	end

	local items = {}

	for _, item in enumItems do
		table.insert(items, { name = tostring(item.Name), value = item.Value })
	end

	table.sort(items, function(a, b)
		return a.value < b.value
	end)

	return { enum = enumName, items = items, count = #items }
end

return ClassInfo
