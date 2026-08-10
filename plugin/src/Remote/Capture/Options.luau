--!strict
-- Option validation and geometry math for the capture ops (capture.json;
-- pinned wave-5 contract, wire keys per the coordinator's alignment update).
-- `normalize` turns the wire `capture_prepare` options into one
-- fully-validated record per kind — every limit is enforced here, before the
-- renderer touches the scene ("reject before rendering"):
--
--   * `size {width, height}`: whole numbers of 1–4096 per axis and
--     ≤16 777 216 px per image;
--   * `region {x, y, width, height}`: integer, non-empty, and only valid
--     for ui and viewport captures (native viewport pixels);
--   * model framing: `view` presets, `direction {x, y, z}` (overrides
--     view), `cameraCFrame` (exactly the 12 finite
--     `CFrame:GetComponents()` values, replaces view/direction/padding
--     framing), `padding` 1.0–4.0, `fov` 1–120;
--   * `world`/`framed` are model-only, `skybox` model+viewport,
--     `ui "none"|"overlay"` viewport-only.
--
-- Also owns the pure raster geometry: aspect-contain destinations, framed
-- center-crops, and region clamping, shared by the renderer and the
-- standalone behavior tests.
--
-- Violations raise inline `__remoteError` INVALID_ARGUMENT markers (the
-- Codec/Selector pattern — keep the shape in sync with Remote/Error). No
-- requires, no Roblox APIs: loadable under a standalone `luau` binary.

local Options = {}

Options.LIMITS = {
	maxAxis = 4096,
	maxPixels = 16777216,
	minPadding = 1.0,
	maxPadding = 4.0,
	minFov = 1,
	maxFov = 120,
	-- Registry defaults (capture.json): model captures render 1024×1024
	-- unless sized; ui/viewport keep their native dimensions.
	defaultModelSize = { w = 1024, h = 1024 },
	defaultPadding = 1.15,
	defaultFov = 35,
}

-- Internal record shapes: the wire spells dimensions out (`width`/`height`);
-- the normalized records use the compact `w`/`h` the raster math shares.
export type Size = { w: number, h: number }
export type Region = { x: number, y: number, w: number, h: number }
export type Vec3 = { x: number, y: number, z: number }

export type Normalized = {
	kind: "ui" | "model" | "viewport",
	size: Size?,
	region: Region?,
	direction: Vec3?,
	cameraCFrame: { number }?,
	padding: number,
	fov: number,
	world: boolean,
	skybox: boolean,
	framed: boolean,
	uiOverlay: boolean,
	tightCrop: boolean,
}

local function invalid(message: string): never
	error({ __remoteError = true, code = "INVALID_ARGUMENT", message = message }, 0)
end

local function isFinite(value: any): boolean
	return type(value) == "number" and value == value and value > -math.huge and value < math.huge
end

local function isWhole(value: any): boolean
	return isFinite(value) and (value :: number) % 1 == 0
end

--------------------------------------------------------------------------------
-- Field validators
--------------------------------------------------------------------------------

--- Validate output dimensions against the axis and total-pixel limits. Used
--- for the requested `size` before rendering and again for native dimensions
--- once they are known — a capture may never return an image outside them.
function Options.checkDims(width: any, height: any): string?
	if not isWhole(width) or not isWhole(height) or width < 1 or height < 1 then
		return "size axes must be whole numbers of at least 1"
	end

	if width > Options.LIMITS.maxAxis or height > Options.LIMITS.maxAxis then
		return `size axes are limited to {Options.LIMITS.maxAxis} pixels`
	end

	if width * height > Options.LIMITS.maxPixels then
		return `the image is limited to {Options.LIMITS.maxPixels} pixels ({width}×{height} exceeds it)`
	end

	return nil
end

local function validateSize(value: any): Size
	if type(value) ~= "table" then
		invalid("size must be an object {width, height}")
	end

	local problem = Options.checkDims(value.width, value.height)

	if problem ~= nil then
		invalid(problem)
	end

	return { w = value.width :: number, h = value.height :: number }
end

local function validateRegion(value: any): Region
	if type(value) ~= "table" then
		invalid("region must be an object {x, y, width, height}")
	end

	local x, y, width, height = value.x, value.y, value.width, value.height

	if not isWhole(x) or not isWhole(y) or x < 0 or y < 0 then
		invalid("region x and y must be whole numbers of at least 0")
	end

	if not isWhole(width) or not isWhole(height) or width < 1 or height < 1 then
		invalid("region width and height must be whole numbers of at least 1")
	end

	return { x = x :: number, y = y :: number, w = width :: number, h = height :: number }
end

-- View presets (capture.json): the isometric default plus the six axis views.
-- Vectors are normalized by `viewDirection`.
local VIEW_DIRECTIONS: { [string]: Vec3 } = {
	isometric = { x = 1, y = 0.75, z = -1 },
	front = { x = 0, y = 0, z = -1 },
	back = { x = 0, y = 0, z = 1 },
	left = { x = -1, y = 0, z = 0 },
	right = { x = 1, y = 0, z = 0 },
	top = { x = 0, y = 1, z = 0 },
	bottom = { x = 0, y = -1, z = 0 },
}

local function normalizeVec3(vector: Vec3): Vec3
	local magnitude = math.sqrt(vector.x * vector.x + vector.y * vector.y + vector.z * vector.z)

	return { x = vector.x / magnitude, y = vector.y / magnitude, z = vector.z / magnitude }
end

--- The unit camera direction for a `view` preset name.
function Options.viewDirection(view: string): Vec3?
	local direction = VIEW_DIRECTIONS[view]

	if direction == nil then
		return nil
	end

	return normalizeVec3(direction)
end

local function validateDirection(value: any): Vec3
	if type(value) ~= "table" or not isFinite(value.x) or not isFinite(value.y) or not isFinite(value.z) then
		invalid("direction must be an object {x, y, z} of finite numbers")
	end

	local vector: Vec3 = { x = value.x, y = value.y, z = value.z }
	local magnitude = math.sqrt(vector.x * vector.x + vector.y * vector.y + vector.z * vector.z)

	if magnitude < 1e-5 then
		invalid("direction must not be the zero vector")
	end

	return normalizeVec3(vector)
end

local function validateCameraCFrame(value: any): { number }
	if type(value) ~= "table" or #value ~= 12 then
		invalid("cameraCFrame must be an array of exactly the 12 CFrame:GetComponents() values")
	end

	local components = table.create(12)

	for index = 1, 12 do
		local component = (value :: { any })[index]

		if not isFinite(component) then
			invalid("cameraCFrame components must be finite numbers")
		end

		components[index] = component
	end

	return components
end

local function validateBoolean(value: any, label: string): boolean
	if type(value) ~= "boolean" then
		invalid(`{label} must be a boolean`)
	end

	return value
end

local function validateRange(value: any, minimum: number, maximum: number, label: string): number
	if not isFinite(value) or value < minimum or value > maximum then
		invalid(`{label} must be a number of {minimum}-{maximum}`)
	end

	return value
end

--------------------------------------------------------------------------------
-- Per-kind normalization
--------------------------------------------------------------------------------

-- Which option keys each kind accepts (everything else is rejected loudly so
-- a silently ignored flag can never masquerade as applied).
local ALLOWED_KEYS: { [string]: { [string]: boolean } } = {
	ui = { size = true, region = true },
	model = {
		size = true,
		view = true,
		direction = true,
		cameraCFrame = true,
		padding = true,
		fov = true,
		world = true,
		skybox = true,
		framed = true,
	},
	viewport = { size = true, region = true, skybox = true, ui = true },
}

--- Validate and normalize the wire options for one capture kind. Raises
--- INVALID_ARGUMENT on the first violation; returns the fully-defaulted
--- record the renderer consumes.
function Options.normalize(kind: any, options: any): Normalized
	if kind ~= "ui" and kind ~= "model" and kind ~= "viewport" then
		invalid(`kind must be "ui", "model", or "viewport"`)
	end

	if options == nil then
		options = {}
	elseif type(options) ~= "table" then
		invalid("options must be an object")
	end

	local allowed = ALLOWED_KEYS[kind]

	for key in options :: { [any]: any } do
		if type(key) ~= "string" or not allowed[key] then
			invalid(`option '{tostring(key)}' is not valid for a {kind} capture`)
		end
	end

	local normalized: Normalized = {
		kind = kind,
		size = nil,
		region = nil,
		direction = nil,
		cameraCFrame = nil,
		padding = Options.LIMITS.defaultPadding,
		fov = Options.LIMITS.defaultFov,
		world = false,
		skybox = false,
		framed = false,
		uiOverlay = false,
		tightCrop = false,
	}

	if options.size ~= nil then
		normalized.size = validateSize(options.size)
	end

	if options.region ~= nil then
		normalized.region = validateRegion(options.region)
	end

	if kind == "model" then
		if options.padding ~= nil then
			normalized.padding =
				validateRange(options.padding, Options.LIMITS.minPadding, Options.LIMITS.maxPadding, "padding")
		end

		if options.fov ~= nil then
			normalized.fov = validateRange(options.fov, Options.LIMITS.minFov, Options.LIMITS.maxFov, "fov")
		end

		if options.world ~= nil then
			normalized.world = validateBoolean(options.world, "world")
		end

		if options.skybox ~= nil then
			normalized.skybox = validateBoolean(options.skybox, "skybox")
		end

		if options.framed ~= nil then
			normalized.framed = validateBoolean(options.framed, "framed")
		end

		-- Framing precedence (capture.json): an exact cameraCFrame replaces
		-- automatic view, direction, and padding framing; a direction
		-- overrides the view preset. Supplied values are validated either
		-- way — an invalid combination never half-applies.
		local direction: Vec3? = nil

		if options.view ~= nil then
			if type(options.view) ~= "string" then
				invalid("view must be a string")
			end

			direction = Options.viewDirection(options.view)

			if direction == nil then
				invalid("view must be isometric, front, back, left, right, top, or bottom")
			end
		end

		if options.direction ~= nil then
			direction = validateDirection(options.direction)
		end

		if options.cameraCFrame ~= nil then
			normalized.cameraCFrame = validateCameraCFrame(options.cameraCFrame)
		else
			normalized.direction = direction or Options.viewDirection("isometric")
		end

		-- The default subject-alpha tight crop applies only to the isolated
		-- transparent render: `world` keeps the camera-framed scene canvas,
		-- `skybox` is opaque, and `framed` opts out explicitly.
		normalized.tightCrop = not (normalized.world or normalized.skybox or normalized.framed)
	elseif kind == "viewport" then
		if options.skybox ~= nil then
			normalized.skybox = validateBoolean(options.skybox, "skybox")
		end

		if options.ui ~= nil then
			if options.ui ~= "none" and options.ui ~= "overlay" then
				invalid(`ui must be "none" or "overlay"`)
			end

			normalized.uiOverlay = options.ui == "overlay"
		end
	end

	return normalized
end

--------------------------------------------------------------------------------
-- Raster geometry (pure integers/floats — shared with the renderer)
--------------------------------------------------------------------------------

--- The centered integer destination rectangle that aspect-contains a source
--- of `srcW`×`srcH` inside an output of `dstW`×`dstH` (each axis ≥1).
function Options.containRect(srcW: number, srcH: number, dstW: number, dstH: number): Region
	local scale = math.min(dstW / srcW, dstH / srcH)
	local width = math.clamp(math.floor(srcW * scale + 0.5), 1, dstW)
	local height = math.clamp(math.floor(srcH * scale + 0.5), 1, dstH)

	return {
		x = math.floor((dstW - width) * 0.5),
		y = math.floor((dstH - height) * 0.5),
		w = width,
		h = height,
	}
end

--- The centered (possibly fractional) crop of a `srcW`×`srcH` source that
--- matches the aspect ratio `aspectW`/`aspectH` — the framed-canvas crop.
function Options.centerCropRect(srcW: number, srcH: number, aspectW: number, aspectH: number): Region
	local aspect = aspectW / aspectH
	local sourceAspect = srcW / srcH

	if sourceAspect > aspect then
		local width = srcH * aspect

		return { x = (srcW - width) * 0.5, y = 0, w = width, h = srcH }
	end

	local height = srcW / aspect

	return { x = 0, y = (srcH - height) * 0.5, w = srcW, h = height }
end

--- Clamp-check an explicit region against the rendered dimensions. Raises
--- INVALID_ARGUMENT when the rectangle leaves the image.
function Options.boundRegion(region: Region, fullW: number, fullH: number): Region
	if region.x + region.w > fullW or region.y + region.h > fullH then
		invalid(`region {region.x},{region.y},{region.w},{region.h} leaves the {fullW}×{fullH} capture`)
	end

	return region
end

return Options
