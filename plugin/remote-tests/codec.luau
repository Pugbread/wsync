-- The runner prepends the canonical JSON catalog as a Luau table. These tests
-- execute the real Codec; only Roblox datatype constructors are stand-ins.
local Codec = require("./Codec")
local checks = 0
local function check(condition, message)
	checks += 1
	assert(condition, message)
end
local function native(kind, fields)
	fields.__native = kind
	return fields
end
local function fieldsCtor(kind, fields)
	return { new = function(...)
		local values = { ... }
		local result = {}
		for i, field in fields do result[field] = values[i] end
		return native(kind, result)
	end }
end
local enum = {}
for _, kind in { "Material", "FontWeight", "FontStyle", "Axis", "NormalId" } do
	local enumType = setmetatable({}, { __tostring = function() return "Enum." .. kind end })
	enum[kind] = enumType
	local names = if kind == "Material" then {"Neon"}
		elseif kind == "FontWeight" then {"Thin","ExtraLight","Light","Regular","Medium","SemiBold","Bold","ExtraBold","Heavy"}
		elseif kind == "FontStyle" then {"Normal","Italic"}
		elseif kind == "Axis" then {"X","Y","Z"}
		else {"Top","Bottom","Left","Right","Front","Back"}
	for i, name in names do enumType[name] = native("EnumItem", { EnumType = enumType, Name = name, Value = i }) end
end
local Vector2 = fieldsCtor("Vector2", {"X","Y"})
local UDim = fieldsCtor("UDim", {"Scale","Offset"})
Codec._injectGlobals({
	typeof = function(value)
		if type(value) == "table" and value.__native then return value.__native end
		return typeof(value)
	end,
	Vector2 = Vector2,
	Vector3 = fieldsCtor("Vector3", {"X","Y","Z"}),
	Vector2int16 = fieldsCtor("Vector2int16", {"X","Y"}),
	Vector3int16 = fieldsCtor("Vector3int16", {"X","Y","Z"}),
	CFrame = { new = function(...)
		local values = {...}
		if #values == 3 then
			for _, n in {1,0,0,0,1,0,0,0,1} do table.insert(values, n) end
		end
		return native("CFrame", { GetComponents = function() return table.unpack(values) end })
	end },
	Color3 = fieldsCtor("Color3", {"R","G","B"}),
	BrickColor = fieldsCtor("BrickColor", {"Name"}),
	Enum = enum,
	UDim = UDim,
	UDim2 = { new = function(xs,xo,ys,yo) return native("UDim2", {X=UDim.new(xs,xo),Y=UDim.new(ys,yo)}) end },
	NumberRange = fieldsCtor("NumberRange", {"Min","Max"}),
	Rect = {new = function(x0,y0,x1,y1) return native("Rect", {Min=Vector2.new(x0,y0),Max=Vector2.new(x1,y1)}) end},
	NumberSequenceKeypoint = fieldsCtor("NumberSequenceKeypoint", {"Time","Value","Envelope"}),
	NumberSequence = fieldsCtor("NumberSequence", {"Keypoints"}),
	ColorSequenceKeypoint = fieldsCtor("ColorSequenceKeypoint", {"Time","Value"}),
	ColorSequence = fieldsCtor("ColorSequence", {"Keypoints"}),
	Font = fieldsCtor("Font", {"Family","Weight","Style"}),
	Faces = { new = function(...)
		local result = {Top=false,Bottom=false,Left=false,Right=false,Front=false,Back=false}
		for _, item in {...} do result[item.Name] = true end
		return native("Faces", result)
	end },
	Axes = { new = function(...)
		local result = {X=false,Y=false,Z=false}
		for _, item in {...} do result[item.Name] = true end
		return native("Axes", result)
	end },
	PhysicalProperties = fieldsCtor("PhysicalProperties", {"Density","Friction","Elasticity","FrictionWeight","ElasticityWeight","AcousticAbsorption"}),
})
local target = native("Instance", {ClassName="Part"})
Codec.setInstanceCodec({
	pathOf = function() return "Workspace/Box", true end,
	resolve = function(path)
		if path ~= "Workspace/Box" then error({__remoteError=true,code="NOT_FOUND",message="missing path"}, 0) end
		return target
	end,
})
local function deepEqual(left, right)
	if type(left) ~= type(right) then return false end
	if type(left) ~= "table" then return left == right end
	for key, value in left do if not deepEqual(value, right[key]) then return false end end
	for key in right do if left[key] == nil then return false end end
	return true
end
local function rejects(value, contains, code)
	local ok, err = pcall(Codec.decode, value)
	check(not ok, "malformed value unexpectedly decoded")
	check(type(err) == "table" and err.__remoteError and err.code == (code or "INVALID_ARGUMENT"), "expected typed error")
	if contains then check(err.message:find(contains, 1, true) ~= nil, `missing hint '{contains}' in {err.message}`) end
end

-- Every accepted example goes through the real tag decoder, encode, decode,
-- and encode again. Canonical readback can add metadata/defaults, expand
-- CFrame, or emit valid UTF-8 BinaryString as a plain string.
for _, entry in catalog.types do
	if entry.decode then
		local decoded = Codec.decode(entry.example)
		local encoded = Codec.encode(decoded)
		local again = Codec.encode(Codec.decode(encoded))
		check(deepEqual(encoded, again), "round-trip differs for " .. entry.name)
	else
		rejects(entry.example, "output-only")
	end
end

rejects({__type="Vector3",x=1,y=2}, '"__type":"Vector3"')
rejects({__type="Color3",r=1,g=0,b="0"}, "Color3.b")
rejects({__type="Color3",r=1,g=0,b=0,R=255}, "not a supported field")
rejects({__type="Vector3int16",x="32768",y=0,z=0}, "finite number")
check(Codec.decode({__type="Vector3int16",x=32768,y=0.5,z=0}) ~= nil, "constructor normalization must stay available")
check(Codec.decode({__type="UDim",scale=0.5,offset=0.75}) ~= nil, "numeric offsets must stay available")
check(Codec.decode({__type="Color3",r=5,g=-1,b=0}) ~= nil, "out-of-range Color3 channels must reach constructor")
check(Codec.decode({__type="PhysicalProperties",density=200,friction=-1,elasticity=2}) ~= nil, "physical normalization must reach constructor")
rejects({__type="Vector3",x=math.huge,y=0,z=0}, "finite")
rejects({__type="Axes",x="true"}, "boolean")
rejects({__type="Faces",front=1}, "boolean")
rejects({__type="UDim2",x={scale=0,offset=1},y={scale=0}}, "offset is required")
rejects({__type="CFrame",components={1,2,3,4}}, "3 or 12")
rejects({__type="CFrame",components={[1]=1,[2]=2,[3]=3,extra=4}}, "dense array")
rejects({__type="NumberRange",min=3,max=1}, "min must be <= max")
rejects({__type="NumberSequence",keypoints={{time=0,value=0}}}, "at least 2")
rejects({__type="NumberSequence",keypoints={{time=0,value=0},{time=0.5,value=1}}}, "end at time 1")
rejects({__type="NumberSequence",keypoints={{time=0,value=0},{time=1,value=1,envelope=-1}}}, ">= 0")
rejects({__type="ColorSequence",keypoints={{time=0,color={r=0,g=0,b=0}},{time=0.8,color={r=0,g=0,b=0}},{time=0.2,color={r=0,g=0,b=0}},{time=1,color={r=0,g=0,b=0}}}}, "nondecreasing")
rejects({__type="Font",family="a",weight="Heavy",style="Bad"}, "Normal, Italic")
rejects({__type="EnumItem",enum="NotReal",name="Nope"}, "unknown Enum")
rejects({__type="EnumItem",enum="Material",name="Nope"}, "unknown EnumItem")
rejects({__type="InstancePath",path="Workspace/Box",rooted=false}, "must be true")
rejects({__type="InstancePath",path="missing"}, "missing path", "NOT_FOUND")
rejects({__type="Buffer",base64="!!!!"}, "base64")
rejects({__type="Buffer",base64="Zh=="}, "padding bits")
rejects({__type="BinaryString",base64="aGk=",bytes=3}, "decoded byte length 2")
rejects({__type="Content",uri="rbxassetid://123"}, "Content.fromUri")
rejects({__type="vector3",x=0,y=0,z=0}, "wsync commands types")
rejects({__type=false}, "must be a string")
rejects({__type="Font",family=string.rep("x",4194305),weight="Regular",style="Normal"}, "string budget")
local cycle = {__type="Axes"}
cycle.x = cycle
rejects(cycle, "table cycle")
local tooDeep = {__type="Axes"}
local leaf = tooDeep
for _ = 1, 65 do leaf.x = {} leaf = leaf.x end
rejects(tooDeep, "nesting depth")
check(Codec.describe("Vector3"):find('"__type":"Vector3"', 1, true) ~= nil, "property mismatch lacks sample")
check(Codec.describe("Content"):find("Content.fromUri", 1, true) ~= nil, "Content guidance missing")
local physical = {__type="PhysicalProperties",density=1,friction=0.5,elasticity=0,frictionWeight=1,elasticityWeight=1,acousticAbsorption=0.5}
check(deepEqual(Codec.encode(Codec.decode(physical)),physical), "acoustic physical property lost")
local input = {child={__type="Vector3",x=3,y=4,z=5},plain={true,"text",3}}
check(deepEqual(Codec.encode(Codec.decode(input)),input), "nested value round-trip")
print(`Codec: {checks} checks passed across {#catalog.types} catalog tags (real codec, stub Roblox constructors).`)
