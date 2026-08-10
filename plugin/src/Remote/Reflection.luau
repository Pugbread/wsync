--!strict
-- Class metadata for `class_info`, `props` and `get` (classinfo.json,
-- props.json): property lists with categories and value types, plus method /
-- event signatures where Studio exposes them.
--
-- Three layers, best first (classinfo.json: "Studio reflection when
-- available, falls back to a baked table"):
--   1. ReflectionService — complete, documented reflection (properties with
--      Display categories, method/event signatures). Not exposed on every
--      Studio build, so every call is pcall-guarded.
--   2. The bundled rbx-dom database (Lib/Dom) — exact per-class property
--      names, value types and scriptability for all classes, but no
--      categories (the declaring class is used as the group) and no methods.
--   3. A baked table covering ~25 commonly-inspected classes — only reached
--      when both richer sources are unavailable.
--
-- Results are cached per class name; reflection metadata is stable for the
-- lifetime of a Studio session.

local WSync = script:FindFirstAncestor("WSync")

local Dom = require(WSync.Lib.Dom)

local Reflection = {}

export type PropertyInfo = {
	name: string,
	category: string,
	type: string,
}

export type ClassInfo = {
	source: "reflection" | "database" | "baked",
	superclass: string?,
	properties: { PropertyInfo },
	methods: { string },
	events: { string }?,
}

--------------------------------------------------------------------------------
-- Baked fallback (layer 3)
--
-- Compact `Name|type|Category` records with single-inheritance links; small
-- on purpose — the rbx-dom database is the real fallback in practice.
--------------------------------------------------------------------------------

type BakedClass = {
	superclass: string?,
	properties: { string },
	methods: { string },
}

local BAKED: { [string]: BakedClass } = {
	Instance = {
		properties = { "Name|string|Data", "ClassName|string|Data", "Parent|Ref|Data", "Archivable|bool|Behavior" },
		methods = {
			"FindFirstChild",
			"FindFirstChildOfClass",
			"FindFirstChildWhichIsA",
			"FindFirstAncestor",
			"GetChildren",
			"GetDescendants",
			"GetFullName",
			"GetAttribute",
			"GetAttributes",
			"GetTags",
			"HasTag",
			"IsA",
			"IsDescendantOf",
			"IsAncestorOf",
			"Clone",
			"Destroy",
		},
	},
	BasePart = {
		superclass = "Instance",
		properties = {
			"Anchored|bool|Behavior",
			"CanCollide|bool|Collision",
			"CanTouch|bool|Collision",
			"CanQuery|bool|Collision",
			"CollisionGroup|string|Collision",
			"Massless|bool|Behavior",
			"CastShadow|bool|Appearance",
			"Position|Vector3|Transform",
			"Orientation|Vector3|Transform",
			"Size|Vector3|Transform",
			"CFrame|CFrame|Transform",
			"Color|Color3|Appearance",
			"BrickColor|BrickColor|Appearance",
			"Material|Enum.Material|Appearance",
			"MaterialVariant|string|Appearance",
			"Transparency|float|Appearance",
			"Reflectance|float|Appearance",
			"Locked|bool|Behavior",
			"PivotOffset|CFrame|Transform",
		},
		methods = { "GetMass", "GetPivot", "GetConnectedParts", "GetTouchingParts", "GetJoints", "Resize" },
	},
	Part = {
		superclass = "BasePart",
		properties = { "Shape|Enum.PartType|Part" },
		methods = {},
	},
	MeshPart = {
		superclass = "BasePart",
		properties = { "MeshId|Content|Data", "TextureID|Content|Appearance", "DoubleSided|bool|Appearance" },
		methods = {},
	},
	Model = {
		superclass = "Instance",
		properties = {
			"PrimaryPart|Ref|Behavior",
			"WorldPivot|CFrame|Transform",
			"ModelStreamingMode|Enum.ModelStreamingMode|Behavior",
		},
		methods = {
			"GetBoundingBox",
			"GetExtentsSize",
			"GetPivot",
			"PivotTo",
			"MoveTo",
			"TranslateBy",
			"ScaleTo",
			"GetScale",
		},
	},
	Folder = { superclass = "Instance", properties = {}, methods = {} },
	Configuration = { superclass = "Instance", properties = {}, methods = {} },
	LuaSourceContainer = {
		superclass = "Instance",
		properties = { "Source|string|Data" },
		methods = {},
	},
	Script = {
		superclass = "LuaSourceContainer",
		properties = { "Enabled|bool|Behavior", "RunContext|Enum.RunContext|Behavior" },
		methods = {},
	},
	LocalScript = {
		superclass = "LuaSourceContainer",
		properties = { "Enabled|bool|Behavior" },
		methods = {},
	},
	ModuleScript = { superclass = "LuaSourceContainer", properties = {}, methods = {} },
	RemoteEvent = {
		superclass = "Instance",
		properties = {},
		methods = { "FireAllClients", "FireClient", "FireServer" },
	},
	UnreliableRemoteEvent = {
		superclass = "Instance",
		properties = {},
		methods = { "FireAllClients", "FireClient", "FireServer" },
	},
	RemoteFunction = { superclass = "Instance", properties = {}, methods = { "InvokeClient", "InvokeServer" } },
	BindableEvent = { superclass = "Instance", properties = {}, methods = { "Fire" } },
	BindableFunction = { superclass = "Instance", properties = {}, methods = { "Invoke" } },
	Camera = {
		superclass = "Instance",
		properties = {
			"CFrame|CFrame|Transform",
			"Focus|CFrame|Transform",
			"FieldOfView|float|Camera",
			"CameraType|Enum.CameraType|Camera",
			"CameraSubject|Ref|Camera",
		},
		methods = {
			"GetRenderCFrame",
			"ScreenPointToRay",
			"ViewportPointToRay",
			"WorldToScreenPoint",
			"WorldToViewportPoint",
		},
	},
	Lighting = {
		superclass = "Instance",
		properties = {
			"Ambient|Color3|Appearance",
			"OutdoorAmbient|Color3|Appearance",
			"Brightness|float|Appearance",
			"ClockTime|float|Data",
			"TimeOfDay|string|Data",
			"GeographicLatitude|float|Data",
			"GlobalShadows|bool|Appearance",
			"EnvironmentDiffuseScale|float|Appearance",
			"EnvironmentSpecularScale|float|Appearance",
			"FogStart|float|Fog",
			"FogEnd|float|Fog",
			"FogColor|Color3|Fog",
			"Technology|Enum.Technology|Appearance",
		},
		methods = { "GetSunDirection", "GetMoonDirection", "GetMinutesAfterMidnight", "SetMinutesAfterMidnight" },
	},
	Workspace = {
		superclass = "Instance",
		properties = {
			"Gravity|float|Behavior",
			"FallenPartsDestroyHeight|float|Behavior",
			"StreamingEnabled|bool|Streaming",
			"CurrentCamera|Ref|Data",
		},
		methods = {
			"Raycast",
			"Blockcast",
			"Spherecast",
			"GetPartBoundsInBox",
			"GetPartBoundsInRadius",
			"GetPartsInPart",
		},
	},
	Players = {
		superclass = "Instance",
		properties = {
			"CharacterAutoLoads|bool|Behavior",
			"RespawnTime|float|Behavior",
			"MaxPlayers|int|Data",
			"PreferredPlayers|int|Data",
		},
		methods = { "GetPlayers", "GetPlayerFromCharacter", "GetPlayerByUserId" },
	},
	ReplicatedStorage = { superclass = "Instance", properties = {}, methods = {} },
	ServerStorage = { superclass = "Instance", properties = {}, methods = {} },
	ServerScriptService = { superclass = "Instance", properties = {}, methods = {} },
	ScreenGui = {
		superclass = "Instance",
		properties = {
			"Enabled|bool|Behavior",
			"DisplayOrder|int|Behavior",
			"IgnoreGuiInset|bool|Behavior",
			"ResetOnSpawn|bool|Behavior",
			"ZIndexBehavior|Enum.ZIndexBehavior|Behavior",
		},
		methods = {},
	},
	GuiObject = {
		superclass = "Instance",
		properties = {
			"Position|UDim2|Transform",
			"Size|UDim2|Transform",
			"AnchorPoint|Vector2|Transform",
			"AutomaticSize|Enum.AutomaticSize|Behavior",
			"BackgroundColor3|Color3|Appearance",
			"BackgroundTransparency|float|Appearance",
			"BorderSizePixel|int|Appearance",
			"Visible|bool|Data",
			"ZIndex|int|Behavior",
			"LayoutOrder|int|Behavior",
			"ClipsDescendants|bool|Behavior",
			"Active|bool|Behavior",
			"Rotation|float|Transform",
		},
		methods = { "TweenPosition", "TweenSize", "TweenSizeAndPosition" },
	},
	Frame = { superclass = "GuiObject", properties = {}, methods = {} },
	TextLabel = {
		superclass = "GuiObject",
		properties = {
			"Text|string|Text",
			"TextColor3|Color3|Text",
			"TextSize|float|Text",
			"TextScaled|bool|Text",
			"TextWrapped|bool|Text",
			"RichText|bool|Text",
			"FontFace|Font|Text",
			"TextXAlignment|Enum.TextXAlignment|Text",
			"TextYAlignment|Enum.TextYAlignment|Text",
		},
		methods = {},
	},
	TextButton = {
		superclass = "TextLabel",
		properties = { "AutoButtonColor|bool|Behavior" },
		methods = {},
	},
	TextBox = {
		superclass = "GuiObject",
		properties = {
			"Text|string|Text",
			"PlaceholderText|string|Text",
			"TextEditable|bool|Behavior",
			"ClearTextOnFocus|bool|Behavior",
			"MultiLine|bool|Behavior",
		},
		methods = { "CaptureFocus", "ReleaseFocus" },
	},
	ImageLabel = {
		superclass = "GuiObject",
		properties = {
			"Image|Content|Image",
			"ImageColor3|Color3|Image",
			"ImageTransparency|float|Image",
			"ScaleType|Enum.ScaleType|Image",
			"SliceCenter|Rect|Image",
		},
		methods = {},
	},
	ImageButton = {
		superclass = "ImageLabel",
		properties = { "AutoButtonColor|bool|Behavior" },
		methods = {},
	},
	ScrollingFrame = {
		superclass = "GuiObject",
		properties = {
			"CanvasSize|UDim2|Scrolling",
			"CanvasPosition|Vector2|Scrolling",
			"AutomaticCanvasSize|Enum.AutomaticSize|Scrolling",
			"ScrollBarThickness|int|Scrolling",
			"ScrollingDirection|Enum.ScrollingDirection|Scrolling",
			"ScrollingEnabled|bool|Scrolling",
		},
		methods = {},
	},
	StringValue = { superclass = "Instance", properties = { "Value|string|Data" }, methods = {} },
	NumberValue = { superclass = "Instance", properties = { "Value|double|Data" }, methods = {} },
	IntValue = { superclass = "Instance", properties = { "Value|int64|Data" }, methods = {} },
	BoolValue = { superclass = "Instance", properties = { "Value|bool|Data" }, methods = {} },
	ObjectValue = { superclass = "Instance", properties = { "Value|Ref|Data" }, methods = {} },
}

local function parseBakedProperty(record: string): PropertyInfo
	local name, valueType, category = record:match("^([^|]+)|([^|]+)|([^|]+)$")

	return {
		name = name or record,
		type = valueType or "",
		category = category or "",
	}
end

local function bakedInfo(className: string): ClassInfo?
	if BAKED[className] == nil then
		return nil
	end

	local properties: { PropertyInfo } = {}
	local methods: { string } = {}
	local seenProps: { [string]: boolean } = {}
	local seenMethods: { [string]: boolean } = {}

	local current: string? = className
	local guard = 0

	while current ~= nil and guard < 16 do
		local class: BakedClass? = BAKED[current]

		if class == nil then
			break
		end

		for _, record in class.properties do
			local info = parseBakedProperty(record)

			if not seenProps[info.name] then
				seenProps[info.name] = true
				table.insert(properties, info)
			end
		end

		for _, method in class.methods do
			if not seenMethods[method] then
				seenMethods[method] = true
				table.insert(methods, method)
			end
		end

		current = class.superclass
		guard += 1
	end

	return {
		source = "baked" :: "baked",
		superclass = BAKED[className].superclass,
		properties = properties,
		methods = methods,
		events = nil,
	}
end

--------------------------------------------------------------------------------
-- ReflectionService (layer 1)
--------------------------------------------------------------------------------

local function reflectionTypeName(value: any): string
	if type(value) ~= "table" then
		return tostring(value or "")
	end

	return tostring(value.ScriptType or value.EngineType or "")
end

local function reflectionSignature(entry: any, withReturn: boolean): string
	local parts = {}

	if type(entry.Parameters) == "table" then
		for _, parameter in ipairs(entry.Parameters) do
			local rendered = `{tostring(parameter.Name or "?")}: {reflectionTypeName(parameter.Type)}`

			if parameter.DefaultValue ~= nil then
				rendered ..= ` = {tostring(parameter.DefaultValue)}`
			end

			table.insert(parts, rendered)
		end
	end

	local rendered = `{tostring(entry.Name or "")}({table.concat(parts, ", ")})`

	if withReturn then
		local returnType = reflectionTypeName(entry.ReturnType)

		if returnType ~= "" then
			rendered ..= ` -> {returnType}`
		end
	end

	return rendered
end

local function reflectionInfo(className: string): ClassInfo?
	local ok, info = pcall(function(): ClassInfo?
		-- Not exposed on every Studio build; the cast keeps the call from
		-- depending on generated service typings.
		local service: any = (game :: any):GetService("ReflectionService")

		local properties: { PropertyInfo } = {}

		for _, entry in ipairs(service:GetPropertiesOfClass(className)) do
			local display = entry.Display

			table.insert(properties, {
				name = tostring(entry.Name or ""),
				category = if type(display) == "table" then tostring(display.Category or "") else "",
				type = reflectionTypeName(entry.Type),
			})
		end

		local methods: { string } = {}

		for _, entry in ipairs(service:GetMethodsOfClass(className)) do
			table.insert(methods, reflectionSignature(entry, true))
		end

		local events: { string } = {}

		for _, entry in ipairs(service:GetEventsOfClass(className)) do
			table.insert(events, reflectionSignature(entry, false))
		end

		if #properties == 0 and #methods == 0 and #events == 0 then
			-- Unknown class (reflection returns empty lists, not errors)
			return nil
		end

		table.sort(methods)
		table.sort(events)

		return {
			source = "reflection" :: "reflection",
			superclass = nil,
			properties = properties,
			methods = methods,
			events = events,
		}
	end)

	if ok and info ~= nil then
		return info
	end

	return nil
end

--------------------------------------------------------------------------------
-- rbx-dom database (layer 2)
--------------------------------------------------------------------------------

local function databaseInfo(className: string): ClassInfo?
	local ok, info = pcall(function(): ClassInfo?
		local classes = (Dom :: any).database.Classes

		if classes[className] == nil then
			return nil
		end

		local properties: { PropertyInfo } = {}
		local seen: { [string]: boolean } = {}

		local current: string? = className
		local superclass: string? = classes[className].Superclass
		local guard = 0

		while current ~= nil and guard < 64 do
			local class = classes[current]

			if class == nil then
				break
			end

			for name, descriptor in class.Properties do
				if seen[name] or descriptor.Kind.Canonical == nil then
					continue
				end

				local scriptability = descriptor.Scriptability

				if scriptability ~= "ReadWrite" and scriptability ~= "Read" and scriptability ~= "Custom" then
					continue
				end

				seen[name] = true

				local dataType = descriptor.DataType
				local typeName = if dataType.Enum ~= nil then `Enum.{dataType.Enum}` else tostring(dataType.Value)

				table.insert(properties, {
					name = name,
					-- The rbx-dom database carries no Studio categories; the
					-- declaring class is the most useful stable grouping.
					category = current :: string,
					type = typeName,
				})
			end

			current = class.Superclass
			guard += 1
		end

		return {
			source = "database" :: "database",
			superclass = superclass,
			properties = properties,
			methods = {},
			events = nil,
		}
	end)

	if ok and info ~= nil then
		return info
	end

	return nil
end

--------------------------------------------------------------------------------
-- Public API
--------------------------------------------------------------------------------

local cache: { [string]: ClassInfo | false } = {}

--- Class metadata from the best available source, or nil for unknown classes.
function Reflection.classInfo(className: string): ClassInfo?
	local cached = cache[className]

	if cached ~= nil then
		if cached == false then
			return nil
		end

		return cached :: ClassInfo
	end

	local info = reflectionInfo(className) or databaseInfo(className) or bakedInfo(className)

	-- Merge baked methods in when the winning source has none: the database
	-- knows properties only, and agents use class_info to discover call
	-- surfaces too.
	if info ~= nil and #info.methods == 0 then
		local baked = bakedInfo(className)

		if baked ~= nil then
			info.methods = baked.methods
		end
	end

	cache[className] = if info == nil then false else info

	return info
end

--- Property list for an instance's class (get/props enumeration), best-source.
function Reflection.classProperties(className: string): ({ PropertyInfo }?, string?)
	local info = Reflection.classInfo(className)

	if info == nil then
		return nil, nil
	end

	return info.properties, info.source
end

return Reflection
