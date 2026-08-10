--!strict
-- Bounded, cycle-safe value codec for the remote-op surface (Design §5.4).
--
-- Wire format: JSON-safe Luau values, with Roblox datatypes carried as tagged
-- `__type` objects — `{"__type":"Vector3","x":…,"y":…,"z":…}`, Color3 as
-- r/g/b, CFrame as 12 components — matching the registry examples
-- (docs/commands/set.json, find-attr.json). Budgets are Ro-Sync's RemoteCodec
-- limits, normative per Design §5.4: depth ≤64, ≤20 000 nodes, ≤10 000 table
-- entries, ≤4 MiB of string bytes per value. Every budget violation raises a
-- typed INVALID_ARGUMENT — truncation is never silent.
--
-- Safety rules enforced here:
--   * Instances are never serialized; they encode as `__type:"InstancePath"`
--     tags carrying their `/`-separated Studio path (Remote/Paths).
--   * Outbound strings are UTF-8 validated; invalid ones become
--     `__type:"BinaryString"` base64 payloads so a JSON text frame can never
--     carry broken UTF-8.
--   * Non-finite numbers become `__type:"Number"` tags (JSON has no nan/inf).
--   * Values with no representation encode as `__type:"Unencodable"` markers
--     instead of being dropped.
--
-- Standalone testability: Roblox globals are read through the `rt` table so
-- behavior tests can run under a plain `luau` binary (which lacks Vector3,
-- CFrame, …). Production code never touches `_injectGlobals`; Remote/init
-- installs the Instance codec at startup. This module must stay require-free.

local Codec = {}

Codec.LIMITS = {
	maxDepth = 64,
	maxNodes = 20000,
	maxTableEntries = 10000,
	maxStringBytes = 4 * 1024 * 1024,
}

export type Budget = {
	nodes: number,
	stringBytes: number,
	seen: { [any]: boolean },
}

type InstanceCodec = {
	-- Returns the `/`-separated path and whether the instance is rooted at
	-- the DataModel.
	pathOf: (instance: any) -> (string, boolean),
	-- Resolves a path or raises a RemoteError (NOT_FOUND naming the segment).
	resolve: (path: string) -> any,
}

--------------------------------------------------------------------------------
-- Runtime globals (injectable for standalone tests)
--------------------------------------------------------------------------------

-- Reading an absent global yields nil, so under standalone Luau these start
-- out nil and the test harness injects stubs; in Studio they bind the real
-- constructors at require time.
local rt: { [string]: any } = {
	typeof = typeof,
	Vector2 = Vector2,
	Vector3 = Vector3,
	Vector2int16 = Vector2int16,
	Vector3int16 = Vector3int16,
	CFrame = CFrame,
	Color3 = Color3,
	BrickColor = BrickColor,
	UDim = UDim,
	UDim2 = UDim2,
	NumberRange = NumberRange,
	Rect = Rect,
	NumberSequence = NumberSequence,
	NumberSequenceKeypoint = NumberSequenceKeypoint,
	ColorSequence = ColorSequence,
	ColorSequenceKeypoint = ColorSequenceKeypoint,
	Font = Font,
	Faces = Faces,
	Axes = Axes,
	PhysicalProperties = PhysicalProperties,
	Enum = Enum,
}

--- TEST-ONLY: standalone Luau has no Roblox globals; behavior tests inject
--- stub constructors and a tag-aware `typeof` here. Never called in Studio.
function Codec._injectGlobals(overrides: { [string]: any })
	for name, value in overrides do
		rt[name] = value
	end
end

local instanceCodec: InstanceCodec? = nil

--- Install the Instance <-> path bridge (done once by Remote/init). Kept as
--- an injection seam so this module stays free of cross-module requires.
function Codec.setInstanceCodec(codec: InstanceCodec)
	instanceCodec = codec
end

--------------------------------------------------------------------------------
-- Errors (shape must match Remote/Error.luau's RemoteError)
--------------------------------------------------------------------------------

local function fail(code: string, message: string): never
	error({ __remoteError = true, code = code, message = message }, 0)
end

local function invalid(message: string): never
	return fail("INVALID_ARGUMENT", message)
end

--------------------------------------------------------------------------------
-- UTF-8 helpers
--------------------------------------------------------------------------------

function Codec.isValidUtf8(value: string): boolean
	return utf8.len(value) ~= nil
end

--- Truncate to `maxBytes` without splitting a UTF-8 sequence. Returns the
--- (possibly shortened) string and whether anything was cut. Assumes valid
--- UTF-8 input; run `sanitizeUtf8` first when the source is untrusted.
function Codec.utf8Truncate(value: string, maxBytes: number): (string, boolean)
	if #value <= maxBytes then
		return value, false
	end

	local cut = maxBytes

	while cut > 0 do
		local byte = string.byte(value, cut + 1)

		if byte == nil or byte < 0x80 or byte > 0xBF then
			break
		end

		cut -= 1
	end

	return value:sub(1, cut), true
end

--- Replace every invalid UTF-8 sequence with U+FFFD. Returns the sanitized
--- string and whether anything changed. Used for log capture, where Studio
--- output can contain arbitrary bytes.
function Codec.sanitizeUtf8(value: string): (string, boolean)
	if utf8.len(value) ~= nil then
		return value, false
	end

	local parts = {}
	local index = 1
	local length = #value

	while index <= length do
		local byte = string.byte(value, index)
		local size = 0

		if byte < 0x80 then
			size = 1
		elseif byte >= 0xC2 and byte <= 0xDF then
			size = 2
		elseif byte >= 0xE0 and byte <= 0xEF then
			size = 3
		elseif byte >= 0xF0 and byte <= 0xF4 then
			size = 4
		end

		local valid = size > 0 and index + size - 1 <= length

		if valid then
			for offset = 1, size - 1 do
				local continuation = string.byte(value, index + offset)

				if continuation < 0x80 or continuation > 0xBF then
					valid = false
					break
				end
			end
		end

		if valid then
			table.insert(parts, value:sub(index, index + size - 1))
			index += size
		else
			table.insert(parts, "\239\191\189") -- U+FFFD
			index += 1
		end
	end

	local out = table.concat(parts)

	if utf8.len(out) == nil then
		-- Edge sequences (overlongs, surrogates) slip past the fast scan;
		-- fall back to scrubbing every high byte.
		out = out:gsub("[\128-\255]", "?")
	end

	return out, true
end

--------------------------------------------------------------------------------
-- Base64 (for BinaryString / Buffer payloads)
--------------------------------------------------------------------------------

local B64_CHARS = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"
local B64_ENCODE: { [number]: string } = {}
local B64_DECODE: { [number]: number } = {}

for index = 1, 64 do
	local char = B64_CHARS:sub(index, index)
	B64_ENCODE[index - 1] = char
	B64_DECODE[string.byte(char)] = index - 1
end

local function base64Encode(raw: string): string
	local out = table.create(math.ceil(#raw / 3))

	for index = 1, #raw, 3 do
		local a, b, c = string.byte(raw, index, index + 2)
		local chunk = a * 65536 + (b or 0) * 256 + (c or 0)

		local one = B64_ENCODE[math.floor(chunk / 262144) % 64]
		local two = B64_ENCODE[math.floor(chunk / 4096) % 64]
		local three = if b then B64_ENCODE[math.floor(chunk / 64) % 64] else "="
		local four = if c then B64_ENCODE[chunk % 64] else "="

		table.insert(out, one .. two .. three .. four)
	end

	return table.concat(out)
end

local function base64Decode(encoded: string): string
	if type(encoded) ~= "string" or #encoded % 4 ~= 0 then
		invalid("malformed base64 payload: length must be a multiple of 4")
	end

	local out = table.create(math.ceil(#encoded / 4))

	for index = 1, #encoded, 4 do
		local chars = { string.byte(encoded, index, index + 3) }
		local values = {}
		local padding = 0

		for position, byte in chars do
			if byte == 61 then -- '='
				-- Padding is only valid in the last two positions
				if position < 3 or index + 3 < #encoded then
					invalid("malformed base64 payload: misplaced padding")
				end

				padding += 1
				values[position] = 0
			else
				local decoded = B64_DECODE[byte]

				if decoded == nil or padding > 0 then
					invalid("malformed base64 payload: invalid character")
				end

				values[position] = decoded
			end
		end

		local chunk = values[1] * 262144 + values[2] * 4096 + values[3] * 64 + values[4]
		local a = math.floor(chunk / 65536) % 256
		local b = math.floor(chunk / 256) % 256
		local c = chunk % 256

		if padding == 0 then
			table.insert(out, string.char(a, b, c))
		elseif padding == 1 then
			table.insert(out, string.char(a, b))
		else
			table.insert(out, string.char(a))
		end
	end

	return table.concat(out)
end

--------------------------------------------------------------------------------
-- Budgets
--------------------------------------------------------------------------------

function Codec.newBudget(): Budget
	return {
		nodes = 0,
		stringBytes = 0,
		seen = {},
	}
end

local function chargeNode(budget: Budget)
	budget.nodes += 1

	if budget.nodes > Codec.LIMITS.maxNodes then
		invalid(`value exceeds the {Codec.LIMITS.maxNodes}-node codec budget`)
	end
end

local function chargeString(budget: Budget, bytes: number)
	budget.stringBytes += bytes

	if budget.stringBytes > Codec.LIMITS.maxStringBytes then
		invalid(`value exceeds the {Codec.LIMITS.maxStringBytes}-byte codec string budget`)
	end
end

local function chargeDepth(depth: number)
	if depth > Codec.LIMITS.maxDepth then
		invalid(`value exceeds the maximum nesting depth of {Codec.LIMITS.maxDepth}`)
	end
end

--------------------------------------------------------------------------------
-- Field accessors for tagged decode (strict: malformed input must error,
-- never silently coerce — the daemon sends well-formed JSON)
--------------------------------------------------------------------------------

local function expectNumber(value: any, field: string): number
	if type(value) ~= "number" then
		invalid(`field '{field}' must be a number`)
	end

	return value
end

local function expectString(value: any, field: string): string
	if type(value) ~= "string" then
		invalid(`field '{field}' must be a string`)
	end

	return value
end

local function expectTable(value: any, field: string): { [any]: any }
	if type(value) ~= "table" then
		invalid(`field '{field}' must be a table`)
	end

	return value
end

--------------------------------------------------------------------------------
-- Encode (Luau/Roblox value -> wire)
--------------------------------------------------------------------------------

local function encodeValue(value: any, depth: number, budget: Budget): any
	chargeDepth(depth)
	chargeNode(budget)

	local kind = rt.typeof(value)

	if kind == "nil" or kind == "boolean" then
		return value
	elseif kind == "number" then
		if value ~= value then
			return { __type = "Number", value = "nan" }
		elseif value == math.huge then
			return { __type = "Number", value = "inf" }
		elseif value == -math.huge then
			return { __type = "Number", value = "-inf" }
		end

		return value
	elseif kind == "string" then
		chargeString(budget, #value)

		if utf8.len(value) ~= nil then
			return value
		end

		-- JSON text frames cannot carry invalid UTF-8 — tag and base64 it
		return { __type = "BinaryString", base64 = base64Encode(value), bytes = #value }
	elseif kind == "buffer" then
		local raw = buffer.tostring(value)
		chargeString(budget, #raw)

		return { __type = "Buffer", base64 = base64Encode(raw), bytes = #raw }
	elseif kind == "Vector2" then
		return { __type = "Vector2", x = value.X, y = value.Y }
	elseif kind == "Vector3" then
		return { __type = "Vector3", x = value.X, y = value.Y, z = value.Z }
	elseif kind == "Vector2int16" then
		return { __type = "Vector2int16", x = value.X, y = value.Y }
	elseif kind == "Vector3int16" then
		return { __type = "Vector3int16", x = value.X, y = value.Y, z = value.Z }
	elseif kind == "CFrame" then
		return { __type = "CFrame", components = { value:GetComponents() } }
	elseif kind == "Color3" then
		return { __type = "Color3", r = value.R, g = value.G, b = value.B }
	elseif kind == "BrickColor" then
		return { __type = "BrickColor", name = value.Name }
	elseif kind == "EnumItem" then
		return {
			__type = "EnumItem",
			enum = tostring(value.EnumType),
			name = value.Name,
			value = value.Value,
		}
	elseif kind == "UDim" then
		return { __type = "UDim", scale = value.Scale, offset = value.Offset }
	elseif kind == "UDim2" then
		return {
			__type = "UDim2",
			x = { scale = value.X.Scale, offset = value.X.Offset },
			y = { scale = value.Y.Scale, offset = value.Y.Offset },
		}
	elseif kind == "NumberRange" then
		return { __type = "NumberRange", min = value.Min, max = value.Max }
	elseif kind == "Rect" then
		return {
			__type = "Rect",
			min = { x = value.Min.X, y = value.Min.Y },
			max = { x = value.Max.X, y = value.Max.Y },
		}
	elseif kind == "NumberSequence" then
		local keypoints = {}

		for _, keypoint in value.Keypoints do
			table.insert(keypoints, {
				time = keypoint.Time,
				value = keypoint.Value,
				envelope = keypoint.Envelope,
			})
		end

		return { __type = "NumberSequence", keypoints = keypoints }
	elseif kind == "ColorSequence" then
		local keypoints = {}

		for _, keypoint in value.Keypoints do
			table.insert(keypoints, {
				time = keypoint.Time,
				color = { r = keypoint.Value.R, g = keypoint.Value.G, b = keypoint.Value.B },
			})
		end

		return { __type = "ColorSequence", keypoints = keypoints }
	elseif kind == "Font" then
		return {
			__type = "Font",
			family = value.Family,
			weight = value.Weight.Name,
			style = value.Style.Name,
		}
	elseif kind == "Faces" then
		return {
			__type = "Faces",
			top = value.Top,
			bottom = value.Bottom,
			left = value.Left,
			right = value.Right,
			front = value.Front,
			back = value.Back,
		}
	elseif kind == "Axes" then
		return { __type = "Axes", x = value.X, y = value.Y, z = value.Z }
	elseif kind == "PhysicalProperties" then
		return {
			__type = "PhysicalProperties",
			density = value.Density,
			friction = value.Friction,
			elasticity = value.Elasticity,
			frictionWeight = value.FrictionWeight,
			elasticityWeight = value.ElasticityWeight,
		}
	elseif kind == "Instance" then
		-- Never serialize Instances (safety rule) — encode the Studio path
		local codec = instanceCodec

		if codec == nil then
			fail("PLUGIN_ERROR", "instance codec is not installed")
			error("unreachable")
		end

		local path, rooted = codec.pathOf(value)
		local tag: { [string]: any } = {
			__type = "InstancePath",
			path = path,
			class = value.ClassName,
		}

		if not rooted then
			-- The path of an instance outside the DataModel is best-effort
			-- and cannot be resolved back — mark it rather than lie.
			tag.rooted = false
		end

		return tag
	elseif kind == "table" then
		if budget.seen[value] then
			invalid("value contains a table cycle")
		end

		budget.seen[value] = true

		-- Reference semantics (Ro-Sync RemoteCodec): all-number keys encode
		-- as a JSON array via ipairs (sparse tails drop); otherwise only
		-- string keys are kept. Mixed-key tables are degenerate on a JSON
		-- wire and lose their non-string keys by design.
		local entryCount = 0
		local isArray = true

		for key in value do
			entryCount += 1

			if entryCount > Codec.LIMITS.maxTableEntries then
				budget.seen[value] = nil
				invalid(`table exceeds the {Codec.LIMITS.maxTableEntries}-entry codec budget`)
			end

			if type(key) ~= "number" then
				isArray = false
			end
		end

		local out: { [any]: any } = {}

		if isArray then
			for index, item in ipairs(value) do
				out[index] = encodeValue(item, depth + 1, budget)
			end
		else
			for key, item in value do
				if type(key) == "string" then
					out[key] = encodeValue(item, depth + 1, budget)
				end
			end
		end

		budget.seen[value] = nil

		return out
	end

	-- No representation (Ray, TweenInfo, Content, SharedString, …): an
	-- explicit marker, never a silent drop and never a bare ambiguous string.
	local display = tostring(value)
	display = (Codec.sanitizeUtf8(display))
	display = (Codec.utf8Truncate(display, 256))

	return { __type = "Unencodable", luauType = kind, display = display }
end

--- Encode a Luau/Roblox value into its wire form. `budget` may be shared by
--- callers that assemble several values into one response so the combined
--- payload respects the codec limits. Raises INVALID_ARGUMENT (RemoteError)
--- on any budget violation or cycle.
function Codec.encode(value: any, budget: Budget?): any
	return encodeValue(value, 1, budget or Codec.newBudget())
end

--------------------------------------------------------------------------------
-- Decode (wire -> Luau/Roblox value)
--------------------------------------------------------------------------------

local decodeValue

local TAG_DECODERS: { [string]: (tag: { [any]: any }) -> any } = {
	Number = function(tag)
		local value = expectString(tag.value, "Number.value")

		if value == "nan" then
			return 0 / 0
		elseif value == "inf" then
			return math.huge
		elseif value == "-inf" then
			return -math.huge
		end

		invalid(`field 'Number.value' must be 'nan', 'inf' or '-inf', got '{value}'`)
		error("unreachable")
	end,

	BinaryString = function(tag)
		return base64Decode(expectString(tag.base64, "BinaryString.base64"))
	end,

	Buffer = function(tag)
		return buffer.fromstring(base64Decode(expectString(tag.base64, "Buffer.base64")))
	end,

	Vector2 = function(tag)
		return rt.Vector2.new(expectNumber(tag.x, "Vector2.x"), expectNumber(tag.y, "Vector2.y"))
	end,

	Vector3 = function(tag)
		return rt.Vector3.new(
			expectNumber(tag.x, "Vector3.x"),
			expectNumber(tag.y, "Vector3.y"),
			expectNumber(tag.z, "Vector3.z")
		)
	end,

	Vector2int16 = function(tag)
		return rt.Vector2int16.new(expectNumber(tag.x, "Vector2int16.x"), expectNumber(tag.y, "Vector2int16.y"))
	end,

	Vector3int16 = function(tag)
		return rt.Vector3int16.new(
			expectNumber(tag.x, "Vector3int16.x"),
			expectNumber(tag.y, "Vector3int16.y"),
			expectNumber(tag.z, "Vector3int16.z")
		)
	end,

	CFrame = function(tag)
		local components = expectTable(tag.components, "CFrame.components")
		local count = #components

		-- Positions come as 3 components, full transforms as 12 (set.json)
		if count ~= 3 and count ~= 12 then
			invalid(`field 'CFrame.components' must have 3 or 12 entries, got {count}`)
		end

		local numbers = table.create(count)

		for index = 1, count do
			numbers[index] = expectNumber(components[index], `CFrame.components[{index}]`)
		end

		return rt.CFrame.new(table.unpack(numbers))
	end,

	Color3 = function(tag)
		return rt.Color3.new(
			expectNumber(tag.r, "Color3.r"),
			expectNumber(tag.g, "Color3.g"),
			expectNumber(tag.b, "Color3.b")
		)
	end,

	BrickColor = function(tag)
		return rt.BrickColor.new(expectString(tag.name, "BrickColor.name"))
	end,

	EnumItem = function(tag)
		local enumName = expectString(tag.enum, "EnumItem.enum"):gsub("^Enum%.", "")
		local itemName = expectString(tag.name, "EnumItem.name")

		local okEnum, enumType = pcall(function()
			return (rt.Enum :: any)[enumName]
		end)

		if not okEnum or enumType == nil then
			invalid(`unknown Enum '{enumName}'`)
		end

		local okItem, item = pcall(function()
			return enumType[itemName]
		end)

		if not okItem or item == nil then
			invalid(`unknown EnumItem 'Enum.{enumName}.{itemName}'`)
		end

		return item
	end,

	UDim = function(tag)
		return rt.UDim.new(expectNumber(tag.scale, "UDim.scale"), expectNumber(tag.offset, "UDim.offset"))
	end,

	UDim2 = function(tag)
		local x = expectTable(tag.x, "UDim2.x")
		local y = expectTable(tag.y, "UDim2.y")

		return rt.UDim2.new(
			expectNumber(x.scale, "UDim2.x.scale"),
			expectNumber(x.offset, "UDim2.x.offset"),
			expectNumber(y.scale, "UDim2.y.scale"),
			expectNumber(y.offset, "UDim2.y.offset")
		)
	end,

	NumberRange = function(tag)
		return rt.NumberRange.new(expectNumber(tag.min, "NumberRange.min"), expectNumber(tag.max, "NumberRange.max"))
	end,

	Rect = function(tag)
		local min = expectTable(tag.min, "Rect.min")
		local max = expectTable(tag.max, "Rect.max")

		return rt.Rect.new(
			expectNumber(min.x, "Rect.min.x"),
			expectNumber(min.y, "Rect.min.y"),
			expectNumber(max.x, "Rect.max.x"),
			expectNumber(max.y, "Rect.max.y")
		)
	end,

	NumberSequence = function(tag)
		local keypoints = expectTable(tag.keypoints, "NumberSequence.keypoints")
		local out = {}

		for index, keypoint in ipairs(keypoints) do
			local entry = expectTable(keypoint, `NumberSequence.keypoints[{index}]`)

			table.insert(
				out,
				rt.NumberSequenceKeypoint.new(
					expectNumber(entry.time, `NumberSequence.keypoints[{index}].time`),
					expectNumber(entry.value, `NumberSequence.keypoints[{index}].value`),
					expectNumber(entry.envelope or 0, `NumberSequence.keypoints[{index}].envelope`)
				)
			)
		end

		if #out == 0 then
			invalid("field 'NumberSequence.keypoints' must not be empty")
		end

		return rt.NumberSequence.new(out)
	end,

	ColorSequence = function(tag)
		local keypoints = expectTable(tag.keypoints, "ColorSequence.keypoints")
		local out = {}

		for index, keypoint in ipairs(keypoints) do
			local entry = expectTable(keypoint, `ColorSequence.keypoints[{index}]`)
			local color = expectTable(entry.color, `ColorSequence.keypoints[{index}].color`)

			table.insert(
				out,
				rt.ColorSequenceKeypoint.new(
					expectNumber(entry.time, `ColorSequence.keypoints[{index}].time`),
					rt.Color3.new(
						expectNumber(color.r, `ColorSequence.keypoints[{index}].color.r`),
						expectNumber(color.g, `ColorSequence.keypoints[{index}].color.g`),
						expectNumber(color.b, `ColorSequence.keypoints[{index}].color.b`)
					)
				)
			)
		end

		if #out == 0 then
			invalid("field 'ColorSequence.keypoints' must not be empty")
		end

		return rt.ColorSequence.new(out)
	end,

	Font = function(tag)
		local family = expectString(tag.family, "Font.family")
		local weightName = expectString(tag.weight, "Font.weight")
		local styleName = expectString(tag.style, "Font.style")

		local ok, font = pcall(function()
			return rt.Font.new(family, rt.Enum.FontWeight[weightName], rt.Enum.FontStyle[styleName])
		end)

		if not ok then
			invalid(`invalid Font: family '{family}', weight '{weightName}', style '{styleName}'`)
		end

		return font
	end,

	Faces = function(tag)
		local normalIds = {}
		local names = { top = "Top", bottom = "Bottom", left = "Left", right = "Right", front = "Front", back = "Back" }

		for field, enumName in names do
			if tag[field] == true then
				table.insert(normalIds, rt.Enum.NormalId[enumName])
			end
		end

		return rt.Faces.new(table.unpack(normalIds))
	end,

	Axes = function(tag)
		local axes = {}

		if tag.x == true then
			table.insert(axes, rt.Enum.Axis.X)
		end
		if tag.y == true then
			table.insert(axes, rt.Enum.Axis.Y)
		end
		if tag.z == true then
			table.insert(axes, rt.Enum.Axis.Z)
		end

		return rt.Axes.new(table.unpack(axes))
	end,

	PhysicalProperties = function(tag)
		return rt.PhysicalProperties.new(
			expectNumber(tag.density, "PhysicalProperties.density"),
			expectNumber(tag.friction, "PhysicalProperties.friction"),
			expectNumber(tag.elasticity, "PhysicalProperties.elasticity"),
			expectNumber(tag.frictionWeight or 1, "PhysicalProperties.frictionWeight"),
			expectNumber(tag.elasticityWeight or 1, "PhysicalProperties.elasticityWeight")
		)
	end,

	InstancePath = function(tag)
		local codec = instanceCodec

		if codec == nil then
			fail("PLUGIN_ERROR", "instance codec is not installed")
			error("unreachable")
		end

		-- Raises NOT_FOUND naming the first missing segment
		return codec.resolve(expectString(tag.path, "InstancePath.path"))
	end,

	Unencodable = function(_tag)
		invalid("an Unencodable value cannot be decoded")
		error("unreachable")
	end,

	Skipped = function(_tag)
		invalid("a Skipped marker cannot be decoded")
		error("unreachable")
	end,
}

function decodeValue(value: any, depth: number, budget: Budget): any
	chargeDepth(depth)
	chargeNode(budget)

	local kind = type(value)

	if kind == "nil" or kind == "boolean" or kind == "number" then
		return value
	elseif kind == "string" then
		chargeString(budget, #value)
		return value
	elseif kind ~= "table" then
		invalid(`cannot decode a value of type '{kind}'`)
	end

	local tag = value.__type

	if tag ~= nil then
		if type(tag) ~= "string" then
			invalid("field '__type' must be a string")
		end

		local decoder = TAG_DECODERS[tag]

		if decoder == nil then
			invalid(`unknown __type tag '{tag}'`)
			error("unreachable")
		end

		return decoder(value)
	end

	-- Plain table: recurse with the same budgets. JSON cannot produce cycles,
	-- but the decoder also accepts Luau tables — stay cycle-safe regardless.
	if budget.seen[value] then
		invalid("value contains a table cycle")
	end

	budget.seen[value] = true

	local entryCount = 0
	local out: { [any]: any } = {}

	for key, item in value do
		entryCount += 1

		if entryCount > Codec.LIMITS.maxTableEntries then
			budget.seen[value] = nil
			invalid(`table exceeds the {Codec.LIMITS.maxTableEntries}-entry codec budget`)
		end

		if type(key) == "string" or type(key) == "number" then
			out[key] = decodeValue(item, depth + 1, budget)
		end
	end

	budget.seen[value] = nil

	return out
end

--- Decode a wire value back into a Luau/Roblox value. Raises INVALID_ARGUMENT
--- on malformed tags or budget violations, and NOT_FOUND for InstancePath
--- tags that no longer resolve.
function Codec.decode(value: any, budget: Budget?): any
	return decodeValue(value, 1, budget or Codec.newBudget())
end

--------------------------------------------------------------------------------
-- Equality (find_by_attr value matching)
--------------------------------------------------------------------------------

-- Userdata kinds whose `==` is identity-based or unavailable; compare their
-- canonical string forms instead (Ro-Sync behavior).
local TOSTRING_EQUALITY: { [string]: true } = {
	Vector2 = true,
	Vector3 = true,
	Vector2int16 = true,
	Vector3int16 = true,
	CFrame = true,
	Color3 = true,
	BrickColor = true,
	EnumItem = true,
	UDim = true,
	UDim2 = true,
	NumberRange = true,
	Rect = true,
	NumberSequence = true,
	ColorSequence = true,
	Font = true,
	Faces = true,
	Axes = true,
	PhysicalProperties = true,
}

--- Compare two decoded values the way `find_by_attr` does: exact equality
--- first, then tostring equality for Roblox datatypes whose `==` is not
--- structural across construction paths.
function Codec.equals(a: any, b: any): boolean
	if a == b then
		return true
	end

	local kindA = rt.typeof(a)

	if kindA ~= rt.typeof(b) then
		return false
	end

	if TOSTRING_EQUALITY[kindA] then
		return tostring(a) == tostring(b)
	end

	return false
end

return Codec
