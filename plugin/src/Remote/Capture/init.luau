--!strict
-- The capture renderer (capture.json; pinned wave-5 contract). Produces
-- tightly-packed straight-alpha RGBA8 buffers for the three capture kinds —
-- `ui`, `model`, `viewport` — without Studio screenshot permission, OS screen
-- recording, or any place-provided dependency.
--
-- Technique (the path Ro-Sync's Photo engine proved workable in Studio):
-- `CaptureService:CaptureScreenshot` renders the 3D viewport to a temporary
-- content id, `AssetService:CreateEditableImageAsync` decodes it, and
-- `EditableImage:ReadPixelsBuffer` reads the raw pixels back — all inside
-- the plugin sandbox. Screenshots are inherently opaque, so transparency is
-- *recovered*, not captured: the scene is photographed twice over a black
-- and a white backdrop and the per-pixel difference yields coverage
-- (alpha) plus the un-blended color. Three backdrop variants:
--
--   * ui       — a bottom-most ScreenGui backdrop covers every 3D pixel and
--                gives exact 0/255 endpoints, so edit-mode ScreenGui layers
--                reconstruct with precise alpha;
--   * model    — an invisible far-plane Part carrying a SurfaceGui backdrop
--                sits behind the isolated subject; a second backdrop-only
--                pass (subject removed) calibrates per-pixel endpoints so
--                exposure/atmosphere remapping cannot smear the alpha crop;
--   * viewport — the same world backdrop behind the whole scene; when an
--                entirely opaque world hides the backdrop completely, a
--                probe pass (backdrop AlwaysOnTop) proves the pipeline
--                works, and the capture degrades to opaque honestly.
--
-- Constraints of the technique (report-honest): captures are sourced at the
-- current viewport resolution — requesting a larger `size` resamples
-- (bilinear) rather than re-rendering; each transparent capture costs 2–4
-- screenshot passes (~0.1 s settle each); animated scenes may shimmer
-- between passes.
--
-- Cleanup discipline (pinned): every no-skybox capture evacuates `Sky` and
-- `Atmosphere` descendants of Lighting (watching for ones added
-- mid-capture) with a protected restore to the exact original parent after
-- success *and* failure, falling back to Lighting only when that parent
-- vanished; camera, UI enablement, and every temporary instance are
-- restored/destroyed through the same LIFO cleanup stack.

local AssetService = game:GetService("AssetService")
local CaptureService = game:GetService("CaptureService")
local CoreGui = game:GetService("CoreGui")
local Lighting = game:GetService("Lighting")
local Players = game:GetService("Players")
local RunService = game:GetService("RunService")
local StarterGui = game:GetService("StarterGui")
local Workspace = game:GetService("Workspace")

local Options = require(script.Options)

local Capture = {}

Capture.Options = Options

-- One settle beat before every screenshot so backdrop/visibility changes are
-- rendered before they are photographed
local SETTLE_SECONDS = 0.08
-- Hard wall-clock budget for one render, every pass included
local RENDER_TIMEOUT_SECONDS = 30

-- Coverage below the floor is treated as renderer noise, not translucency.
-- World backdrops pass through lighting/exposure (loose floor); UI and
-- calibrated passes have exact endpoints (tight floor).
local WORLD_ALPHA_NOISE_FLOOR = 0.04
local EXACT_ALPHA_NOISE_FLOOR = 1 / 510
-- A channel span this small cannot resolve alpha reliably
local MIN_BACKDROP_SPAN = 4

type Region = Options.Region

type RawImage = {
	width: number,
	height: number,
	pixels: buffer,
}

export type RenderRequest = {
	kind: "ui" | "model" | "viewport",
	target: Instance?,
	options: Options.Normalized,
}

export type Result = {
	width: number,
	height: number,
	bytes: buffer,
}

--------------------------------------------------------------------------------
-- Cleanup scaffolding
--------------------------------------------------------------------------------

type CleanupRegistrar = (callback: () -> ()) -> ()

--- Run `body` with a LIFO cleanup stack. Every registered callback runs —
--- pcall-guarded, newest first — after success and failure alike, then the
--- original error (if any) is re-raised untouched.
local function withCleanup<T>(body: (register: CleanupRegistrar) -> T): T
	local stack: { () -> () } = {}

	local function register(callback: () -> ())
		table.insert(stack, callback)
	end

	local ok, result = xpcall(function()
		return body(register)
	end, function(caught)
		return caught
	end)

	for index = #stack, 1, -1 do
		local callback = stack[index]
		local cleanupOk, cleanupError = pcall(function()
			callback()
		end)

		if not cleanupOk then
			warn(`[WSync] capture cleanup step failed: {tostring(cleanupError)}`)
		end
	end

	if not ok then
		error(result, 0)
	end

	return result :: T
end

local function assertTime(deadline: number, label: string)
	if os.clock() >= deadline then
		error(`capture timed out during {label}`, 0)
	end
end

local function invalidArgument(message: string): never
	-- Inline RemoteError marker (the Codec/Selector pattern) so option
	-- violations detected at render time stay typed INVALID_ARGUMENT
	error({ __remoteError = true, code = "INVALID_ARGUMENT", message = message }, 0)
end

--------------------------------------------------------------------------------
-- Screenshot readback
--------------------------------------------------------------------------------

local function settle(deadline: number)
	RunService.Heartbeat:Wait()
	assertTime(deadline, "render settling")
	task.wait(SETTLE_SECONDS)
	assertTime(deadline, "render settling")
end

local function decodeScreenshot(content: string): EditableImage
	local editable: EditableImage? = nil

	local ok, err = pcall(function()
		editable = AssetService:CreateEditableImageAsync(Content.fromUri(content))
	end)

	if not ok or editable == nil then
		-- Older Studio builds accepted the raw content string
		local okRaw, errRaw = pcall(function()
			editable = (AssetService :: any):CreateEditableImageAsync(content)
		end)

		if not okRaw or editable == nil then
			error(`unable to decode the screenshot: {tostring(errRaw or err)}`, 0)
		end
	end

	return editable :: EditableImage
end

--- One settled viewport screenshot, read back as raw RGBA8.
local function takeScreenshot(deadline: number): RawImage
	settle(deadline)

	local content: string? = nil
	local failed: string? = nil

	local requestOk, requestError = pcall(function()
		CaptureService:CaptureScreenshot(function(result)
			if type(result) == "string" and result ~= "" then
				content = result
			else
				failed = "the screenshot returned no image content"
			end
		end)
	end)

	if not requestOk then
		error(`unable to request a screenshot: {tostring(requestError)}`, 0)
	end

	while content == nil and failed == nil do
		assertTime(deadline, "the screenshot request")
		RunService.Heartbeat:Wait()
	end

	if failed ~= nil then
		error(failed, 0)
	end

	local editable = decodeScreenshot(content :: string)

	local readOk, readResult = pcall(function()
		local size = editable.Size

		if size.X < 1 or size.Y < 1 then
			error("the decoded screenshot has invalid dimensions")
		end

		return {
			width = size.X,
			height = size.Y,
			pixels = editable:ReadPixelsBuffer(Vector2.zero, size),
		}
	end)

	editable:Destroy()

	if not readOk then
		error(`unable to read the screenshot pixels: {tostring(readResult)}`, 0)
	end

	assertTime(deadline, "the screenshot readback")

	return readResult :: RawImage
end

--------------------------------------------------------------------------------
-- Scene preparation (all restore-registered)
--------------------------------------------------------------------------------

--- Move every `Sky`/`Atmosphere` descendant out of Lighting for the capture,
--- watching for matching descendants added while it is active. The restore
--- (registered before the first mutation) reparents each object to its exact
--- original parent and falls back to Lighting only when that parent
--- vanished; objects that cannot be restored at all keep the holder alive
--- with a loud warning rather than being destroyed.
local function evacuateSkyEffects(register: CleanupRegistrar)
	local holder = Instance.new("Folder")
	holder.Name = "WSyncCaptureHiddenLighting"
	holder.Archivable = false

	local originalParents: { [Instance]: Instance? } = {}
	local movedOrder: { Instance } = {}
	local watch: RBXScriptConnection? = nil

	register(function()
		if watch ~= nil then
			watch:Disconnect()
			watch = nil
		end

		for index = #movedOrder, 1, -1 do
			local item = movedOrder[index]
			local parent = originalParents[item]

			if parent ~= nil then
				pcall(function()
					item.Parent = parent
				end)
			end
		end

		-- Anything still in the holder lost its exact parent mid-capture;
		-- Lighting is the lossless fallback (never destroy user lighting)
		for _, item in movedOrder do
			if item.Parent == holder then
				pcall(function()
					item.Parent = Lighting
				end)
			end
		end

		local retained = false

		for _, item in movedOrder do
			if item.Parent == holder then
				retained = true
				break
			end
		end

		if retained then
			warn("[WSync] capture kept its lighting holder alive: a Sky or Atmosphere could not be restored")
		else
			holder:Destroy()
		end
	end)

	local function evacuate(item: Instance)
		if originalParents[item] ~= nil or (not item:IsA("Sky") and not item:IsA("Atmosphere")) then
			return
		end

		local parent = item.Parent

		if parent == nil or not item:IsDescendantOf(Lighting) then
			return
		end

		originalParents[item] = parent
		table.insert(movedOrder, item);
		(item :: Instance).Parent = holder
	end

	watch = Lighting.DescendantAdded:Connect(evacuate)

	for _, item in Lighting:GetDescendants() do
		evacuate(item)
	end
end

--- Disable every enabled ScreenGui under CoreGui, StarterGui, and (when
--- present) the local PlayerGui, restoring the exact enablement afterwards.
local function hideScreenGuis(register: CleanupRegistrar)
	local hidden: { ScreenGui } = {}
	local seen: { [Instance]: boolean } = {}
	local roots: { Instance } = { CoreGui, StarterGui }
	local player: Player? = (Players :: any).LocalPlayer

	if player ~= nil then
		local playerGui = player:FindFirstChildOfClass("PlayerGui")

		if playerGui ~= nil then
			table.insert(roots, playerGui)
		end
	end

	register(function()
		for index = #hidden, 1, -1 do
			local item = hidden[index]

			if item.Parent ~= nil then
				pcall(function()
					item.Enabled = true
				end)
			end
		end
	end)

	for _, root in roots do
		for _, item in root:GetDescendants() do
			if item:IsA("ScreenGui") and not seen[item] then
				seen[item] = true

				local ok, enabled = pcall(function()
					return item.Enabled
				end)

				if ok and enabled then
					local wrote = pcall(function()
						item.Enabled = false
					end)

					if wrote then
						table.insert(hidden, item)
					end
				end
			end
		end
	end
end

--- Disable every LayerCollector except the exclusions (the target layer and
--- the backdrop) so a `ui` TARGET capture shows nothing unrelated — new
--- collectors appearing mid-capture are hidden too. Restores enablement.
local function hideOtherLayerCollectors(exclusions: { Instance }, register: CleanupRegistrar)
	local states: { { item: LayerCollector, enabled: boolean } } = {}
	local seen: { [Instance]: boolean } = {}
	local roots: { Instance } = { CoreGui, StarterGui, Workspace }
	local player: Player? = (Players :: any).LocalPlayer

	if player ~= nil then
		local playerGui = player:FindFirstChildOfClass("PlayerGui")

		if playerGui ~= nil then
			table.insert(roots, playerGui)
		end
	end

	local function isExcluded(item: Instance): boolean
		for _, exclusion in exclusions do
			if item == exclusion or item:IsDescendantOf(exclusion) then
				return true
			end
		end

		return false
	end

	local function hide(item: Instance)
		if seen[item] or not item:IsA("LayerCollector") or isExcluded(item) then
			return
		end

		seen[item] = true

		local ok, enabled = pcall(function()
			return item.Enabled
		end)

		if ok and enabled then
			local wrote = pcall(function()
				item.Enabled = false
			end)

			if wrote then
				table.insert(states, { item = item, enabled = true })
			end
		end
	end

	register(function()
		for index = #states, 1, -1 do
			local state = states[index]

			if state.item.Parent ~= nil then
				pcall(function()
					state.item.Enabled = state.enabled
				end)
			end
		end
	end)

	local connections: { RBXScriptConnection } = {}

	register(function()
		for _, connection in connections do
			connection:Disconnect()
		end
	end)

	for _, root in roots do
		table.insert(connections, root.DescendantAdded:Connect(hide))

		local ok, descendants = pcall(function()
			return root:GetDescendants()
		end)

		if ok then
			for _, item in descendants do
				hide(item)
			end
		end
	end
end

local function saveCamera(camera: Camera, register: CleanupRegistrar)
	local oldCFrame = camera.CFrame
	local oldFocus = camera.Focus
	local oldFieldOfView = camera.FieldOfView
	local oldCameraType = camera.CameraType

	register(function()
		camera.CameraType = oldCameraType
		camera.CFrame = oldCFrame
		camera.Focus = oldFocus
		camera.FieldOfView = oldFieldOfView
	end)
end

local function currentCamera(): Camera
	local camera: Camera? = (Workspace :: any).CurrentCamera

	if camera == nil then
		error("Workspace has no current camera", 0)
	end

	return camera :: Camera
end

--- The host for temporary capture ScreenGuis: the local PlayerGui when one
--- exists (playtest), otherwise StarterGui (edit mode, the normal case).
local function screenGuiHost(): Instance
	local player: Player? = (Players :: any).LocalPlayer

	if player ~= nil then
		local playerGui = player:FindFirstChildOfClass("PlayerGui")

		if playerGui ~= nil then
			return playerGui
		end
	end

	return StarterGui
end

--------------------------------------------------------------------------------
-- Backdrops
--------------------------------------------------------------------------------

--- An invisible far-plane Part carrying a SurfaceGui color surface, sized to
--- cover the camera frustum behind everything visible. The returned Frame's
--- BackgroundColor3 is the backdrop color; the surface starts non-AlwaysOnTop.
local function createWorldBackdrop(camera: Camera, register: CleanupRegistrar): (Frame, SurfaceGui)
	-- Distance: past the farthest part that could intersect the view
	local farthest = 256
	local viewport = camera.ViewportSize
	local aspect = viewport.X / math.max(1, viewport.Y)
	local tangentY = math.tan(math.rad(camera.FieldOfView * 0.5))
	local tangentX = tangentY * aspect

	for _, item in Workspace:GetDescendants() do
		if item:IsA("BasePart") then
			local relative = camera.CFrame:PointToObjectSpace(item.Position)
			local radius = item.Size.Magnitude * 0.5
			local depth = -relative.Z
			local projectedDepth = math.max(depth, 0)
			local intersectsView = depth + radius > 0
				and math.abs(relative.X) <= projectedDepth * tangentX + radius
				and math.abs(relative.Y) <= projectedDepth * tangentY + radius

			if intersectsView then
				farthest = math.max(farthest, depth + radius)
			end
		end
	end

	local distance = math.clamp(farthest + math.max(32, farthest * 0.05), 256, 50000)
	local height = 2 * distance * tangentY * 1.2
	local width = height * aspect * 1.2

	local part = Instance.new("Part")

	register(function()
		part:Destroy()
	end)

	part.Name = "WSyncCaptureBackdrop"
	part.Archivable = false
	part.Anchored = true
	part.CanCollide = false
	part.CanQuery = false
	part.CanTouch = false
	part.CastShadow = false
	part.Transparency = 1

	-- Parts cap at 2048 studs; a Brick SpecialMesh scales the visual surface
	-- beyond that without touching physics
	local physicalWidth = math.min(width, 2048)
	local physicalHeight = math.min(height, 2048)
	part.Size = Vector3.new(physicalWidth, physicalHeight, 0.1)
	part.CFrame = CFrame.lookAt(
		camera.CFrame.Position + camera.CFrame.LookVector * distance,
		camera.CFrame.Position,
		camera.CFrame.UpVector
	)

	local mesh = Instance.new("SpecialMesh")
	mesh.MeshType = Enum.MeshType.Brick
	mesh.Scale = Vector3.new(width / physicalWidth, height / physicalHeight, 1)
	mesh.Parent = part

	local surface = Instance.new("SurfaceGui")
	surface.Name = "BackdropSurface"
	surface.Adornee = part
	surface.Face = Enum.NormalId.Front
	surface.AlwaysOnTop = false
	surface.LightInfluence = 0
	surface.MaxDistance = 0
	surface.SizingMode = Enum.SurfaceGuiSizingMode.FixedSize
	surface.CanvasSize = Vector2.new(64, 64)
	surface.Parent = part

	local color = Instance.new("Frame")
	color.Name = "Color"
	color.BorderSizePixel = 0
	color.Size = UDim2.fromScale(1, 1)
	color.BackgroundColor3 = Color3.new(0, 0, 0)
	color.Parent = surface

	part.Parent = Workspace

	return color, surface
end

--- A bottom-most ScreenGui backdrop: composited in the same layer as the UI
--- being captured, it reliably covers every 3D pixel while staying behind
--- all ordinary ScreenGuis — photographing it in black and in white gives
--- exact backdrop endpoints for straight-alpha UI reconstruction.
local function createUiBackdrop(register: CleanupRegistrar): (Frame, ScreenGui)
	local screen = Instance.new("ScreenGui")

	register(function()
		screen:Destroy()
	end)

	screen.Name = "WSyncCaptureUiBackdrop"
	screen.Archivable = false
	screen.DisplayOrder = -2147483648
	screen.IgnoreGuiInset = true
	screen.ResetOnSpawn = false
	screen.ZIndexBehavior = Enum.ZIndexBehavior.Global

	local color = Instance.new("Frame")
	color.Name = "Color"
	color.BorderSizePixel = 0
	color.Position = UDim2.fromScale(0, 0)
	color.Size = UDim2.fromScale(1, 1)
	color.BackgroundColor3 = Color3.new(0, 0, 0)
	color.Parent = screen

	-- Parent only once the hierarchy is complete so a half-built backdrop
	-- can never flash into a screenshot
	screen.Parent = screenGuiHost()

	return color, screen
end

--------------------------------------------------------------------------------
-- UI target isolation
--------------------------------------------------------------------------------

--- Clone a UI tree with Archivable forced (restored afterwards) and every
--- script container stripped from the clone.
local function cloneUiTree(target: Instance): Instance
	local changed: { Instance } = {}
	local candidates = target:GetDescendants()
	table.insert(candidates, target)

	for _, item in candidates do
		if not item.Archivable then
			local wrote = pcall(function()
				item.Archivable = true
			end)

			if wrote then
				table.insert(changed, item)
			end
		end
	end

	local clone: Instance? = nil
	local cloneOk, cloneError = pcall(function()
		clone = target:Clone()
	end)

	for index = #changed, 1, -1 do
		pcall(function()
			changed[index].Archivable = false
		end)
	end

	if not cloneOk or clone == nil then
		error(`unable to clone the UI target: {tostring(cloneError)}`, 0)
	end

	local cloned = clone :: Instance
	local descendants = cloned:GetDescendants()

	for index = #descendants, 1, -1 do
		local item = descendants[index]

		if item:IsA("LuaSourceContainer") then
			item:Destroy()
		end
	end

	return cloned
end

local function ancestorUiScale(target: GuiObject, owner: ScreenGui): number
	local scale = 1
	local cursor = target.Parent

	while cursor ~= nil and cursor ~= owner do
		if cursor:IsA("GuiObject") then
			local scaler = cursor:FindFirstChildOfClass("UIScale")

			if scaler ~= nil and scaler.Scale == scaler.Scale and scaler.Scale > 0 then
				scale *= scaler.Scale
			end
		end

		cursor = cursor.Parent
	end

	return scale
end

local function uiClipRect(target: GuiObject, owner: ScreenGui, viewport: Vector2): Rect
	local minimum = Vector2.zero
	local maximum = viewport
	local cursor = target.Parent

	while cursor ~= nil and cursor ~= owner do
		if cursor:IsA("GuiObject") and cursor.ClipsDescendants then
			minimum = minimum:Max(cursor.AbsolutePosition)
			maximum = maximum:Min(cursor.AbsolutePosition + cursor.AbsoluteSize)
		end

		cursor = cursor.Parent
	end

	if maximum.X <= minimum.X or maximum.Y <= minimum.Y then
		invalidArgument("the UI target is completely clipped by its ancestors")
	end

	return Rect.new(minimum, maximum)
end

local function copyNearestStyleLink(target: Instance, destination: Instance)
	local cursor = target.Parent

	while cursor ~= nil do
		local styleLink = cursor:FindFirstChildWhichIsA("StyleLink")

		if styleLink ~= nil then
			local ok, copy = pcall(function()
				return styleLink:Clone()
			end)

			if ok then
				copy.Parent = destination
			end

			return
		end

		cursor = cursor.Parent
	end
end

--- Reproduce the TARGET in a temporary layer of its own: a ScreenGui clones
--- directly; a GuiObject clone is placed at its absolute position/rotation/
--- size inside a holder that reproduces the ancestor clip rectangle and the
--- accumulated UIScale. The nearest StyleLink follows so styled UI renders.
local function createUiTargetLayer(target: Instance, camera: Camera, register: CleanupRegistrar): ScreenGui
	if target.Parent == nil or not target:IsDescendantOf(game) then
		invalidArgument("the capture target must be inside the DataModel")
	end

	if target:IsA("ScreenGui") then
		local clone = cloneUiTree(target)

		if not clone:IsA("ScreenGui") then
			clone:Destroy()
			error("cloning the ScreenGui target produced an unexpected instance", 0)
		end

		register(function()
			clone:Destroy()
		end)

		clone.Name = "WSyncCaptureUiTarget"
		clone.Archivable = false
		clone.DisplayOrder = 0
		clone.Enabled = true
		clone.ResetOnSpawn = false
		copyNearestStyleLink(target, clone)
		clone.Parent = screenGuiHost()

		return clone
	end

	if not target:IsA("GuiObject") then
		invalidArgument("a ui capture target must be a ScreenGui or GuiObject")
	end

	local guiTarget = target :: GuiObject
	local owner = guiTarget:FindFirstAncestorWhichIsA("ScreenGui")

	if owner == nil then
		invalidArgument("a GuiObject capture target must be beneath a ScreenGui")
	end

	local clone = cloneUiTree(guiTarget)

	if not clone:IsA("GuiObject") then
		clone:Destroy()
		error("cloning the GuiObject target produced an unexpected instance", 0)
	end

	register(function()
		clone:Destroy()
	end)

	local screen = Instance.new("ScreenGui")

	register(function()
		screen:Destroy()
	end)

	screen.Name = "WSyncCaptureUiTarget"
	screen.Archivable = false
	screen.DisplayOrder = 0
	screen.IgnoreGuiInset = true
	screen.ResetOnSpawn = false
	screen.ZIndexBehavior = (owner :: ScreenGui).ZIndexBehavior

	local clip = uiClipRect(guiTarget, owner :: ScreenGui, camera.ViewportSize)
	local holder = Instance.new("Frame")
	holder.Name = "TargetClip"
	holder.BackgroundTransparency = 1
	holder.BorderSizePixel = 0
	holder.ClipsDescendants = true
	holder.Position = UDim2.fromOffset(clip.Min.X, clip.Min.Y)
	holder.Size = UDim2.fromOffset(clip.Width, clip.Height)
	holder.Parent = screen

	-- The clone is laid out absolutely; layout constraints fighting that
	-- placement are removed from the root
	for _, item in clone:GetChildren() do
		if item:IsA("UIAspectRatioConstraint") or item:IsA("UISizeConstraint") then
			item:Destroy()
		end
	end

	local scale = ancestorUiScale(guiTarget, owner :: ScreenGui)
	local cloneScale = clone:FindFirstChildOfClass("UIScale")

	if cloneScale ~= nil and cloneScale.Scale == cloneScale.Scale and cloneScale.Scale > 0 then
		scale *= cloneScale.Scale
		cloneScale.Scale = scale
	elseif math.abs(scale - 1) > 1e-6 then
		local addedScale = Instance.new("UIScale")
		addedScale.Scale = scale
		addedScale.Parent = clone
	end

	scale = math.max(scale, 1e-6)
	clone.AnchorPoint = Vector2.zero
	clone.AutomaticSize = Enum.AutomaticSize.None
	clone.Visible = true
	clone.Position =
		UDim2.fromOffset(guiTarget.AbsolutePosition.X - clip.Min.X, guiTarget.AbsolutePosition.Y - clip.Min.Y)
	clone.Rotation = guiTarget.AbsoluteRotation
	clone.Size = UDim2.fromOffset(guiTarget.AbsoluteSize.X / scale, guiTarget.AbsoluteSize.Y / scale)
	clone.Parent = holder
	copyNearestStyleLink(guiTarget, holder)

	screen.Parent = screenGuiHost()

	return screen
end

--------------------------------------------------------------------------------
-- Alpha recovery
--------------------------------------------------------------------------------

--- Recover straight-alpha RGBA from a dark-backdrop and a light-backdrop
--- pass. With `exactEndpoints` (UI backdrop) the endpoints are known to be
--- 0/255; otherwise the per-channel endpoint separation is measured from the
--- passes themselves (exposure and color grading can remap both endpoints),
--- averaging the dark endpoint over every maximum-span pixel to avoid a
--- noisy baseline.
local function recoverAlpha(dark: RawImage, light: RawImage, exactEndpoints: boolean): RawImage
	if dark.width ~= light.width or dark.height ~= light.height then
		error("the backdrop passes returned different dimensions", 0)
	end

	local byteLength = dark.width * dark.height * 4
	local spans: { number } = if exactEndpoints then { 255, 255, 255 } else { -1, -1, -1 }
	local baselineSums: { number } = { 0, 0, 0 }
	local baselineCounts: { number } = if exactEndpoints then { 1, 1, 1 } else { 0, 0, 0 }

	if not exactEndpoints then
		for offset = 0, byteLength - 4, 4 do
			for channel = 0, 2 do
				local darkValue = buffer.readu8(dark.pixels, offset + channel)
				local lightValue = buffer.readu8(light.pixels, offset + channel)
				local span = math.max(0, lightValue - darkValue)
				local index = channel + 1

				if span > spans[index] then
					spans[index] = span
					baselineSums[index] = darkValue
					baselineCounts[index] = 1
				elseif span == spans[index] then
					baselineSums[index] += darkValue
					baselineCounts[index] += 1
				end
			end
		end
	end

	for channel = 1, 3 do
		if spans[channel] < MIN_BACKDROP_SPAN or baselineCounts[channel] < 1 then
			error("the backdrop passes do not contain enough contrast to recover alpha", 0)
		end
	end

	local baselines: { number } = {
		baselineSums[1] / baselineCounts[1],
		baselineSums[2] / baselineCounts[2],
		baselineSums[3] / baselineCounts[3],
	}

	local output = buffer.create(byteLength)
	local noiseFloor = if exactEndpoints then EXACT_ALPHA_NOISE_FLOOR else WORLD_ALPHA_NOISE_FLOOR

	for offset = 0, byteLength - 4, 4 do
		local darkR = buffer.readu8(dark.pixels, offset)
		local darkG = buffer.readu8(dark.pixels, offset + 1)
		local darkB = buffer.readu8(dark.pixels, offset + 2)
		local deltaR = math.max(0, buffer.readu8(light.pixels, offset) - darkR)
		local deltaG = math.max(0, buffer.readu8(light.pixels, offset + 1) - darkG)
		local deltaB = math.max(0, buffer.readu8(light.pixels, offset + 2) - darkB)

		-- A pixel whose delta reaches the full span on every channel shows
		-- bare backdrop: force it fully transparent even when quantization
		-- left a one-count residue
		local emptyBackdrop = deltaR >= spans[1] - 1 and deltaG >= spans[2] - 1 and deltaB >= spans[3] - 1
		local backgroundShare = (
			math.clamp(deltaR / spans[1], 0, 1)
			+ math.clamp(deltaG / spans[2], 0, 1)
			+ math.clamp(deltaB / spans[3], 0, 1)
		) / 3
		local coverage = if emptyBackdrop then 0 else math.clamp(1 - backgroundShare, 0, 1)

		if coverage <= noiseFloor then
			coverage = 0
		end

		local alpha = math.floor(coverage * 255 + 0.5)

		if alpha > 0 then
			-- Un-blend: dark = subject*coverage + baseline*(1-coverage)
			local encoded = alpha / 255
			local recoveredR = (darkR - baselines[1] * (1 - encoded)) / encoded
			local recoveredG = (darkG - baselines[2] * (1 - encoded)) / encoded
			local recoveredB = (darkB - baselines[3] * (1 - encoded)) / encoded

			buffer.writeu8(output, offset, math.floor(math.clamp(recoveredR, 0, 255) + 0.5))
			buffer.writeu8(output, offset + 1, math.floor(math.clamp(recoveredG, 0, 255) + 0.5))
			buffer.writeu8(output, offset + 2, math.floor(math.clamp(recoveredB, 0, 255) + 0.5))
		else
			buffer.writeu8(output, offset, 0)
			buffer.writeu8(output, offset + 1, 0)
			buffer.writeu8(output, offset + 2, 0)
		end

		buffer.writeu8(output, offset + 3, alpha)
	end

	return { width = dark.width, height = dark.height, pixels = output }
end

--- Calibrated variant for isolated model renders: two extra backdrop-only
--- passes (subject removed) give per-pixel endpoints, so renderer residue —
--- atmosphere, exposure, bloom remapping a distant backdrop unevenly —
--- cannot masquerade as subject alpha and stretch the tight crop to the
--- viewport edges. Genuinely translucent geometry survives. Writes in place
--- over `dark` and returns it.
local function recoverAlphaCalibrated(
	dark: RawImage,
	light: RawImage,
	backdropDark: RawImage,
	backdropLight: RawImage
): RawImage
	if
		dark.width ~= light.width
		or dark.height ~= light.height
		or dark.width ~= backdropDark.width
		or dark.height ~= backdropDark.height
		or dark.width ~= backdropLight.width
		or dark.height ~= backdropLight.height
	then
		error("the calibrated backdrop passes returned different dimensions", 0)
	end

	local byteLength = dark.width * dark.height * 4
	local output = dark.pixels

	for offset = 0, byteLength - 4, 4 do
		local darkR = buffer.readu8(dark.pixels, offset)
		local darkG = buffer.readu8(dark.pixels, offset + 1)
		local darkB = buffer.readu8(dark.pixels, offset + 2)
		local backdropDarkR = buffer.readu8(backdropDark.pixels, offset)
		local backdropDarkG = buffer.readu8(backdropDark.pixels, offset + 1)
		local backdropDarkB = buffer.readu8(backdropDark.pixels, offset + 2)
		local backdropSpanR = math.max(0, buffer.readu8(backdropLight.pixels, offset) - backdropDarkR)
		local backdropSpanG = math.max(0, buffer.readu8(backdropLight.pixels, offset + 1) - backdropDarkG)
		local backdropSpanB = math.max(0, buffer.readu8(backdropLight.pixels, offset + 2) - backdropDarkB)
		local deltaR = math.max(0, buffer.readu8(light.pixels, offset) - darkR)
		local deltaG = math.max(0, buffer.readu8(light.pixels, offset + 1) - darkG)
		local deltaB = math.max(0, buffer.readu8(light.pixels, offset + 2) - darkB)

		local backgroundShare = 0
		local validChannels = 0
		local emptyBackdrop = true

		if backdropSpanR >= MIN_BACKDROP_SPAN then
			backgroundShare += math.clamp(deltaR / backdropSpanR, 0, 1)
			validChannels += 1
			emptyBackdrop = emptyBackdrop and deltaR >= backdropSpanR - 1
		end

		if backdropSpanG >= MIN_BACKDROP_SPAN then
			backgroundShare += math.clamp(deltaG / backdropSpanG, 0, 1)
			validChannels += 1
			emptyBackdrop = emptyBackdrop and deltaG >= backdropSpanG - 1
		end

		if backdropSpanB >= MIN_BACKDROP_SPAN then
			backgroundShare += math.clamp(deltaB / backdropSpanB, 0, 1)
			validChannels += 1
			emptyBackdrop = emptyBackdrop and deltaB >= backdropSpanB - 1
		end

		if validChannels == 0 then
			-- The backdrop never reached this pixel (occluded in both
			-- calibration passes): nothing of the subject can be resolved
			buffer.writeu8(output, offset, 0)
			buffer.writeu8(output, offset + 1, 0)
			buffer.writeu8(output, offset + 2, 0)
			buffer.writeu8(output, offset + 3, 0)
			continue
		end

		local coverage = if emptyBackdrop then 0 else math.clamp(1 - backgroundShare / validChannels, 0, 1)

		if coverage <= EXACT_ALPHA_NOISE_FLOOR then
			coverage = 0
		end

		local alpha = math.floor(coverage * 255 + 0.5)

		if alpha > 0 then
			local encoded = alpha / 255
			local recoveredR = (darkR - backdropDarkR * (1 - encoded)) / encoded
			local recoveredG = (darkG - backdropDarkG * (1 - encoded)) / encoded
			local recoveredB = (darkB - backdropDarkB * (1 - encoded)) / encoded

			buffer.writeu8(output, offset, math.floor(math.clamp(recoveredR, 0, 255) + 0.5))
			buffer.writeu8(output, offset + 1, math.floor(math.clamp(recoveredG, 0, 255) + 0.5))
			buffer.writeu8(output, offset + 2, math.floor(math.clamp(recoveredB, 0, 255) + 0.5))
		else
			buffer.writeu8(output, offset, 0)
			buffer.writeu8(output, offset + 1, 0)
			buffer.writeu8(output, offset + 2, 0)
		end

		buffer.writeu8(output, offset + 3, alpha)
	end

	return dark
end

--- Whether the two passes differ enough on every channel to resolve alpha.
local function hasBackdropContrast(dark: RawImage, light: RawImage): boolean
	if dark.width ~= light.width or dark.height ~= light.height then
		return false
	end

	local spans = { 0, 0, 0 }
	local byteLength = dark.width * dark.height * 4

	for offset = 0, byteLength - 4, 4 do
		for channel = 0, 2 do
			local span = math.max(
				0,
				buffer.readu8(light.pixels, offset + channel) - buffer.readu8(dark.pixels, offset + channel)
			)

			spans[channel + 1] = math.max(spans[channel + 1], span)
		end

		if spans[1] >= MIN_BACKDROP_SPAN and spans[2] >= MIN_BACKDROP_SPAN and spans[3] >= MIN_BACKDROP_SPAN then
			return true
		end
	end

	return false
end

--- Whether *anything* moved between the passes (any channel, either way) —
--- distinguishes "the world hid the backdrop" from "the render changed".
local function hasAnyBackdropContrast(dark: RawImage, light: RawImage): boolean
	if dark.width ~= light.width or dark.height ~= light.height then
		return false
	end

	local byteLength = dark.width * dark.height * 4

	for offset = 0, byteLength - 4, 4 do
		for channel = 0, 2 do
			local difference = buffer.readu8(light.pixels, offset + channel)
				- buffer.readu8(dark.pixels, offset + channel)

			if math.abs(difference) >= MIN_BACKDROP_SPAN then
				return true
			end
		end
	end

	return false
end

local function forceOpaque(image: RawImage): RawImage
	local byteLength = image.width * image.height * 4

	for offset = 3, byteLength - 1, 4 do
		buffer.writeu8(image.pixels, offset, 255)
	end

	return image
end

--------------------------------------------------------------------------------
-- Raster operations
--------------------------------------------------------------------------------

--- The bounding rectangle of every pixel with non-zero alpha.
local function alphaBounds(image: RawImage, deadline: number, label: string): Region
	local width = image.width
	local height = image.height
	local minimumX, minimumY = width, height
	local maximumX, maximumY = -1, -1

	for y = 0, height - 1 do
		if y % 64 == 0 then
			assertTime(deadline, `{label} bounds`)
		end

		for x = 0, width - 1 do
			if buffer.readu8(image.pixels, (y * width + x) * 4 + 3) > 0 then
				minimumX = math.min(minimumX, x)
				minimumY = math.min(minimumY, y)
				maximumX = math.max(maximumX, x)
				maximumY = math.max(maximumY, y)
			end
		end
	end

	if maximumX < minimumX or maximumY < minimumY then
		error(`{label} rendered no visible pixels`, 0)
	end

	return { x = minimumX, y = minimumY, w = maximumX + 1 - minimumX, h = maximumY + 1 - minimumY }
end

--- Resample `region` of the source into `destination` of a fresh
--- `outW`×`outH` RGBA buffer (transparent outside the destination).
--- Integer-aligned same-size copies go row-by-row; everything else is
--- bilinear with premultiplied color so transparent texels never bleed dark
--- fringes into the result.
local function resample(source: RawImage, region: Region, outW: number, outH: number, destination: Region?): buffer
	local sourceWidth = source.width
	local sourceHeight = source.height
	local minX = region.x
	local minY = region.y
	local maxX = region.x + region.w
	local maxY = region.y + region.h

	if minX < 0 or minY < 0 or maxX > sourceWidth or maxY > sourceHeight or maxX <= minX or maxY <= minY then
		error("the crop region must be a non-empty rectangle inside the capture", 0)
	end

	local output = buffer.create(outW * outH * 4)
	local dest = destination or { x = 0, y = 0, w = outW, h = outH }

	if
		dest.x < 0
		or dest.y < 0
		or dest.x + dest.w > outW
		or dest.y + dest.h > outH
		or dest.w < 1
		or dest.h < 1
		or dest.x % 1 ~= 0
		or dest.y % 1 ~= 0
		or dest.w % 1 ~= 0
		or dest.h % 1 ~= 0
	then
		error("the destination must be an integer rectangle inside the output", 0)
	end

	local regionWidth = maxX - minX
	local regionHeight = maxY - minY

	if minX % 1 == 0 and minY % 1 == 0 and regionWidth == dest.w and regionHeight == dest.h then
		local bytesPerRow = dest.w * 4

		for row = 0, dest.h - 1 do
			local sourceOffset = ((minY + row) * sourceWidth + minX) * 4
			local destinationOffset = ((dest.y + row) * outW + dest.x) * 4

			buffer.copy(output, destinationOffset, source.pixels, sourceOffset, bytesPerRow)
		end

		return output
	end

	for destinationY = 0, dest.h - 1 do
		local sourceY = math.clamp(minY + (destinationY + 0.5) * regionHeight / dest.h - 0.5, 0, sourceHeight - 1)
		local y0 = math.floor(sourceY)
		local y1 = math.min(y0 + 1, sourceHeight - 1)
		local yWeight = sourceY - y0
		local outputY = dest.y + destinationY

		for destinationX = 0, dest.w - 1 do
			local sourceX = math.clamp(minX + (destinationX + 0.5) * regionWidth / dest.w - 0.5, 0, sourceWidth - 1)
			local x0 = math.floor(sourceX)
			local x1 = math.min(x0 + 1, sourceWidth - 1)
			local xWeight = sourceX - x0

			local topLeft = (y0 * sourceWidth + x0) * 4
			local topRight = (y0 * sourceWidth + x1) * 4
			local bottomLeft = (y1 * sourceWidth + x0) * 4
			local bottomRight = (y1 * sourceWidth + x1) * 4
			local outputOffset = (outputY * outW + dest.x + destinationX) * 4

			local inverseX = 1 - xWeight
			local inverseY = 1 - yWeight
			local topLeftWeight = inverseX * inverseY
			local topRightWeight = xWeight * inverseY
			local bottomLeftWeight = inverseX * yWeight
			local bottomRightWeight = xWeight * yWeight
			local topLeftAlpha = buffer.readu8(source.pixels, topLeft + 3) / 255
			local topRightAlpha = buffer.readu8(source.pixels, topRight + 3) / 255
			local bottomLeftAlpha = buffer.readu8(source.pixels, bottomLeft + 3) / 255
			local bottomRightAlpha = buffer.readu8(source.pixels, bottomRight + 3) / 255
			local outputAlpha = topLeftAlpha * topLeftWeight
				+ topRightAlpha * topRightWeight
				+ bottomLeftAlpha * bottomLeftWeight
				+ bottomRightAlpha * bottomRightWeight

			for channel = 0, 2 do
				local premultiplied = buffer.readu8(source.pixels, topLeft + channel) * topLeftAlpha * topLeftWeight
					+ buffer.readu8(source.pixels, topRight + channel) * topRightAlpha * topRightWeight
					+ buffer.readu8(source.pixels, bottomLeft + channel) * bottomLeftAlpha * bottomLeftWeight
					+ buffer.readu8(source.pixels, bottomRight + channel) * bottomRightAlpha * bottomRightWeight
				local value = if outputAlpha > 1e-8 then premultiplied / outputAlpha else 0

				buffer.writeu8(output, outputOffset + channel, math.floor(math.clamp(value, 0, 255) + 0.5))
			end

			buffer.writeu8(output, outputOffset + 3, math.floor(math.clamp(outputAlpha * 255, 0, 255) + 0.5))
		end
	end

	return output
end

--- Enforce the axis/total-pixel limits on dimensions only known after the
--- render (native viewport output) — a capture never returns an image
--- outside them, and the message says how to shrink it.
local function ensureOutputDims(width: number, height: number)
	local problem = Options.checkDims(width, height)

	if problem ~= nil then
		invalidArgument(`the native capture is {width}×{height}: {problem} — pass options.size to fit the limits`)
	end
end

--------------------------------------------------------------------------------
-- Subject staging (model captures)
--------------------------------------------------------------------------------

local function boundsOf(subject: Instance): (CFrame, Vector3)
	if subject:IsA("BasePart") then
		return subject.CFrame, subject.Size
	end

	if subject:IsA("Model") then
		return subject:GetBoundingBox()
	end

	local minimum = Vector3.new(math.huge, math.huge, math.huge)
	local maximum = Vector3.new(-math.huge, -math.huge, -math.huge)
	local found = false

	for _, item in subject:GetDescendants() do
		if item:IsA("BasePart") then
			found = true

			local half = item.Size * 0.5

			for _, x in { -1, 1 } do
				for _, y in { -1, 1 } do
					for _, z in { -1, 1 } do
						local point = (item.CFrame * CFrame.new(half.X * x, half.Y * y, half.Z * z)).Position

						minimum = Vector3.new(
							math.min(minimum.X, point.X),
							math.min(minimum.Y, point.Y),
							math.min(minimum.Z, point.Z)
						)
						maximum = Vector3.new(
							math.max(maximum.X, point.X),
							math.max(maximum.Y, point.Y),
							math.max(maximum.Z, point.Z)
						)
					end
				end
			end
		end
	end

	if not found then
		invalidArgument("the capture target contains no renderable parts")
	end

	return CFrame.new((minimum + maximum) * 0.5), maximum - minimum
end

--- Position the camera along `direction` so every corner of the subject's
--- bounding box fits the frustum at `fieldOfView`, scaled out by `padding`.
local function frameCamera(
	camera: Camera,
	boundsCFrame: CFrame,
	boundsSize: Vector3,
	direction: Vector3,
	padding: number,
	aspect: number,
	fieldOfView: number
)
	local center = boundsCFrame.Position
	local upHint = if math.abs(direction:Dot(Vector3.yAxis)) > 0.98 then Vector3.zAxis else Vector3.yAxis
	local basis = CFrame.lookAt(center + direction, center, upHint)
	local tangentY = math.tan(math.rad(fieldOfView * 0.5))
	local tangentX = tangentY * aspect
	local required = 0
	local half = boundsSize * 0.5

	for _, x in { -1, 1 } do
		for _, y in { -1, 1 } do
			for _, z in { -1, 1 } do
				local point = (boundsCFrame * CFrame.new(half.X * x, half.Y * y, half.Z * z)).Position
				local offset = point - center
				local towardCamera = offset:Dot(direction)
				local horizontal = math.abs(offset:Dot(basis.RightVector)) / tangentX
				local vertical = math.abs(offset:Dot(basis.UpVector)) / tangentY

				required = math.max(required, towardCamera + horizontal, towardCamera + vertical)
			end
		end
	end

	local distance = math.max(required * padding, boundsSize.Magnitude * 0.05, 0.1)

	camera.FieldOfView = fieldOfView
	camera.CFrame = CFrame.lookAt(center + direction * distance, center, upHint)
end

--- A script-free, physics-inert clone of the subject for isolated renders.
local function stageClone(subject: Instance, register: CleanupRegistrar): Instance
	if subject:IsA("LuaSourceContainer") then
		invalidArgument("a script container cannot be rendered as the capture subject")
	end

	local previousArchivable = subject.Archivable

	if not previousArchivable then
		subject.Archivable = true
	end

	local clone: Instance? = nil
	local cloneOk, cloneError = pcall(function()
		clone = subject:Clone()
	end)

	if not previousArchivable then
		subject.Archivable = false
	end

	if not cloneOk or clone == nil then
		error(`unable to clone the capture subject: {tostring(cloneError)}`, 0)
	end

	local cloned = clone :: Instance

	register(function()
		cloned:Destroy()
	end)

	local candidates = cloned:GetDescendants()
	table.insert(candidates, cloned)

	for _, item in candidates do
		if item:IsA("BasePart") then
			item.Anchored = true
			item.CanCollide = false
			item.CanQuery = false
			item.CanTouch = false
			item.AssemblyLinearVelocity = Vector3.zero
			item.AssemblyAngularVelocity = Vector3.zero
		end
	end

	for index = #candidates, 1, -1 do
		local item = candidates[index]

		if item:IsA("LuaSourceContainer") then
			item:Destroy()
		end
	end

	return cloned
end

--- A staging point far outside everything in Workspace, below the world (or
--- above it when the camera looks up) so no scenery can enter the frame.
local function remoteStageCenter(boundsSize: Vector3, direction: Vector3): Vector3
	local lowest = 0
	local highest = 0
	local horizontalReach = 0

	for _, item in Workspace:GetDescendants() do
		if item:IsA("BasePart") then
			local radius = item.Size.Magnitude * 0.5

			lowest = math.min(lowest, item.Position.Y - radius)
			highest = math.max(highest, item.Position.Y + radius)
			horizontalReach =
				math.max(horizontalReach, Vector2.new(item.Position.X, item.Position.Z).Magnitude + radius)
		end
	end

	local clearance = math.max(10000, boundsSize.Magnitude * 8, horizontalReach * 2.2)
	local y = if direction.Y > 0.05 then lowest - clearance else highest + clearance

	return Vector3.new(0, y, 0)
end

local function moveCenterTo(subject: Instance, target: Vector3)
	local boundsCFrame = boundsOf(subject)
	local translation = target - boundsCFrame.Position
	local candidates = subject:GetDescendants()

	if subject:IsA("BasePart") then
		table.insert(candidates, subject)
	end

	for _, item in candidates do
		if item:IsA("BasePart") then
			item.CFrame = CFrame.new(translation) * item.CFrame
		end
	end
end

local function toVector3(vector: Options.Vec3): Vector3
	return Vector3.new(vector.x, vector.y, vector.z)
end

local function toCFrame(components: { number }): CFrame
	return CFrame.new(table.unpack(components))
end

--------------------------------------------------------------------------------
-- Shared view composite (viewport + world-model captures)
--------------------------------------------------------------------------------

--- Photograph the current view: one pass for an opaque scene, or the
--- black/white world-backdrop pair with alpha recovery for a transparent
--- one. An entirely opaque world legally hides the whole backdrop; the probe
--- (backdrop AlwaysOnTop) proves the backdrop pipeline works before the
--- capture honestly degrades to opaque.
local function captureCurrentView(scene: boolean, deadline: number, register: CleanupRegistrar): RawImage
	if scene then
		return takeScreenshot(deadline)
	end

	local camera = currentCamera()
	local backdropColor, backdropSurface = createWorldBackdrop(camera, register)

	backdropColor.BackgroundColor3 = Color3.new(0, 0, 0)
	local dark = takeScreenshot(deadline)
	backdropColor.BackgroundColor3 = Color3.new(1, 1, 1)
	local light = takeScreenshot(deadline)

	local recovered, result = pcall(recoverAlpha, dark, light, false)

	if recovered then
		return result :: RawImage
	end

	local failure = tostring(result)

	if not failure:find("do not contain enough contrast", 1, true) then
		error(result, 0)
	end

	if hasAnyBackdropContrast(dark, light) then
		-- Something moved between passes but not enough to resolve alpha —
		-- a real failure, not an opaque world
		error(result, 0)
	end

	backdropSurface.AlwaysOnTop = true
	backdropColor.BackgroundColor3 = Color3.new(0, 0, 0)
	local probeDark = takeScreenshot(deadline)
	backdropColor.BackgroundColor3 = Color3.new(1, 1, 1)
	local probeLight = takeScreenshot(deadline)
	backdropSurface.AlwaysOnTop = false

	if not hasBackdropContrast(probeDark, probeLight) then
		error(result, 0)
	end

	return forceOpaque(dark)
end

--------------------------------------------------------------------------------
-- Kind renderers
--------------------------------------------------------------------------------

local function renderUi(request: RenderRequest, deadline: number, register: CleanupRegistrar): Result
	local opts = request.options
	local camera = currentCamera()

	-- Doc conformance (capture.json): every no-skybox capture evacuates the
	-- sky effects, ui included — harmless here since the backdrop covers the
	-- 3D scene entirely
	evacuateSkyEffects(register)

	local targetLayer: ScreenGui? = nil

	if request.target ~= nil then
		targetLayer = createUiTargetLayer(request.target :: Instance, camera, register)
	end

	local backdropColor, backdropScreen = createUiBackdrop(register)

	if targetLayer ~= nil then
		hideOtherLayerCollectors({ targetLayer :: Instance, backdropScreen }, register)
	end

	backdropColor.BackgroundColor3 = Color3.new(0, 0, 0)
	local dark = takeScreenshot(deadline)
	backdropColor.BackgroundColor3 = Color3.new(1, 1, 1)
	local light = takeScreenshot(deadline)

	local raw = recoverAlpha(dark, light, true)

	-- Crop precedence: an explicit region wins; a TARGET otherwise
	-- tight-crops to its rendered alpha; else the full canvas
	local region: Region? = nil
	local targetCropped = false

	if opts.region ~= nil then
		region = Options.boundRegion(opts.region :: Region, raw.width, raw.height)
	elseif targetLayer ~= nil then
		region = alphaBounds(raw, deadline, "the UI target")
		targetCropped = true
	end

	local selected: Region = region or { x = 0, y = 0, w = raw.width, h = raw.height }
	local outW, outH

	if opts.size ~= nil then
		outW, outH = (opts.size :: Options.Size).w, (opts.size :: Options.Size).h
	else
		outW, outH = selected.w, selected.h
		ensureOutputDims(outW, outH)
	end

	-- A tight-cropped TARGET aspect-contains into the requested size; every
	-- other combination resamples the selection to the output directly
	local destination: Region? = if targetCropped and opts.size ~= nil
		then Options.containRect(selected.w, selected.h, outW, outH)
		else nil

	return {
		width = outW,
		height = outH,
		bytes = resample(raw, selected, outW, outH, destination),
	}
end

local function renderModelIsolated(request: RenderRequest, deadline: number, register: CleanupRegistrar): Result
	local opts = request.options
	local subject = request.target :: Instance
	local camera = currentCamera()
	local size = opts.size or Options.LIMITS.defaultModelSize
	local outW, outH = size.w, size.h

	if not opts.skybox then
		evacuateSkyEffects(register)
	end

	hideScreenGuis(register)
	saveCamera(camera, register)

	local clone = stageClone(subject, register)
	local initialBounds, initialSize = boundsOf(clone)

	-- An exact cameraCFrame is re-anchored relative to the subject bounds so
	-- it survives the move to the staging point; the offset doubles as the
	-- staging direction
	local relativeCamera: CFrame? = nil
	local direction: Vector3

	if opts.cameraCFrame ~= nil then
		local authored = toCFrame(opts.cameraCFrame :: { number })

		relativeCamera = initialBounds:ToObjectSpace(authored)

		local offset = authored.Position - initialBounds.Position

		direction = if offset.Magnitude > 1e-5 then offset.Unit else -authored.LookVector
	else
		direction = toVector3(opts.direction :: Options.Vec3)
	end

	moveCenterTo(clone, remoteStageCenter(initialSize, direction))
	clone.Name = "WSyncCaptureSubject"
	clone.Parent = Workspace

	local boundsCFrame, boundsSize = boundsOf(clone)
	local liveAspect = camera.ViewportSize.X / math.max(1, camera.ViewportSize.Y)
	local outputAspect = outW / outH

	camera.CameraType = Enum.CameraType.Scriptable

	if relativeCamera ~= nil then
		camera.FieldOfView = opts.fov
		camera.CFrame = boundsCFrame * (relativeCamera :: CFrame)
	else
		-- Frame against the tighter of the screenshot and output aspects so
		-- the subject fits both canvases
		frameCamera(
			camera,
			boundsCFrame,
			boundsSize,
			direction,
			opts.padding,
			math.min(liveAspect, outputAspect),
			opts.fov
		)
	end

	camera.Focus = boundsCFrame

	local raw: RawImage

	if opts.skybox then
		raw = takeScreenshot(deadline)
	else
		local backdropColor = createWorldBackdrop(camera, register)

		backdropColor.BackgroundColor3 = Color3.new(0, 0, 0)
		local dark = takeScreenshot(deadline)
		backdropColor.BackgroundColor3 = Color3.new(1, 1, 1)
		local light = takeScreenshot(deadline)

		if opts.tightCrop then
			-- Backdrop-only calibration passes (subject removed) — see
			-- recoverAlphaCalibrated
			clone.Parent = nil
			backdropColor.BackgroundColor3 = Color3.new(0, 0, 0)
			local backdropDark = takeScreenshot(deadline)
			backdropColor.BackgroundColor3 = Color3.new(1, 1, 1)
			local backdropLight = takeScreenshot(deadline)

			if hasBackdropContrast(backdropDark, backdropLight) then
				raw = recoverAlphaCalibrated(dark, light, backdropDark, backdropLight)
			else
				raw = recoverAlpha(dark, light, false)
			end
		else
			raw = recoverAlpha(dark, light, false)
		end
	end

	local crop: Region
	local destination: Region? = nil

	if opts.tightCrop then
		crop = alphaBounds(raw, deadline, "the capture subject")
		destination = Options.containRect(crop.w, crop.h, outW, outH)
	else
		crop = Options.centerCropRect(raw.width, raw.height, outW, outH)
	end

	return {
		width = outW,
		height = outH,
		bytes = resample(raw, crop, outW, outH, destination),
	}
end

local function renderModelWorld(request: RenderRequest, deadline: number, register: CleanupRegistrar): Result
	local opts = request.options
	local subject = request.target :: Instance

	if not subject:IsDescendantOf(Workspace) then
		invalidArgument("a world capture requires a target inside Workspace")
	end

	local camera = currentCamera()
	local size = opts.size or Options.LIMITS.defaultModelSize
	local outW, outH = size.w, size.h

	if not opts.skybox then
		evacuateSkyEffects(register)
	end

	hideScreenGuis(register)
	saveCamera(camera, register)

	local boundsCFrame, boundsSize = boundsOf(subject)

	camera.CameraType = Enum.CameraType.Scriptable

	if opts.cameraCFrame ~= nil then
		camera.FieldOfView = opts.fov
		camera.CFrame = toCFrame(opts.cameraCFrame :: { number })
	else
		frameCamera(
			camera,
			boundsCFrame,
			boundsSize,
			toVector3(opts.direction :: Options.Vec3),
			opts.padding,
			outW / outH,
			opts.fov
		)
	end

	camera.Focus = boundsCFrame

	local raw = captureCurrentView(opts.skybox, deadline, register)
	local crop = Options.centerCropRect(raw.width, raw.height, outW, outH)

	return {
		width = outW,
		height = outH,
		bytes = resample(raw, crop, outW, outH),
	}
end

local function renderViewport(request: RenderRequest, deadline: number, register: CleanupRegistrar): Result
	local opts = request.options

	if not opts.skybox then
		evacuateSkyEffects(register)
	end

	if not opts.uiOverlay then
		hideScreenGuis(register)
	end

	local raw = captureCurrentView(opts.skybox, deadline, register)

	local selected: Region = if opts.region ~= nil
		then Options.boundRegion(opts.region :: Region, raw.width, raw.height)
		else { x = 0, y = 0, w = raw.width, h = raw.height }

	local outW, outH

	if opts.size ~= nil then
		outW, outH = (opts.size :: Options.Size).w, (opts.size :: Options.Size).h
	else
		outW, outH = selected.w, selected.h
		ensureOutputDims(outW, outH)
	end

	return {
		width = outW,
		height = outH,
		bytes = resample(raw, selected, outW, outH),
	}
end

--------------------------------------------------------------------------------
-- Entry point
--------------------------------------------------------------------------------

--- Render one validated capture request to a tightly-packed straight-alpha
--- RGBA8 buffer. Raises RemoteError markers for argument-shaped failures and
--- plain messages (PLUGIN_ERROR at the dispatcher) for renderer failures;
--- every temporary scene/UI/Lighting/camera change is restored either way.
function Capture.render(request: RenderRequest): Result
	return withCleanup(function(register)
		local deadline = os.clock() + RENDER_TIMEOUT_SECONDS

		if request.kind == "ui" then
			return renderUi(request, deadline, register)
		elseif request.kind == "model" then
			if request.options.world then
				return renderModelWorld(request, deadline, register)
			end

			return renderModelIsolated(request, deadline, register)
		end

		return renderViewport(request, deadline, register)
	end)
end

return Capture
