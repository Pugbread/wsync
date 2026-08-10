--!strict
-- Transmit ops (transmit.json; wave-7 pinned contract). Where the capture
-- renderer *manufactures* pixels, transmit reads pixels that already exist:
-- `transmit_prepare` resolves a batch of image-bearing instances, pulls each
-- one's straight-alpha RGBA8 out through `EditableImage:ReadPixelsBuffer`,
-- and registers every image as an ordinary capture session — so the daemon
-- streams and drops them with the *existing* `capture_read`/`capture_close`
-- pair (`transmit_read`/`transmit_close` are deliberately not implemented;
-- capabilities reports them false).
--
-- Two mutually exclusive modes (exactly one, INVALID_ARGUMENT otherwise —
-- TransmitRules.selectMode):
--
--   * `paths`  — resolve each Studio path and extract its image;
--   * `source` — run Luau at the `eval` trust level (Design §5.4;
--                Write.runSource, the same `loadstring`) and normalize what
--                it returns: an EditableImage, an image-bearing instance, or
--                an array of either, in any combination of return values.
--
-- Extraction, per kind — the object-backed path is preferred everywhere
-- because a `Content` that already holds an EditableImage needs no async
-- call, no asset permission, and no copy of the pixels:
--
--   * EditableImage          — read in place;
--   * ImageLabel/ImageButton — `ImageContent`, else the legacy `Image` id;
--   * Decal/Texture          — `TextureContent`, else the legacy `Texture` id
--                              (`Texture` derives from `Decal`);
--   * MeshPart               — `TextureContent`, else the legacy `TextureID`.
--
-- Anything else — a path that does not resolve, a Part with no texture, an
-- asset that will not decode, an image past the per-item limits — is *that
-- item's* failure, recorded in the parallel `failures` array with the same
-- `{code, message}` payload `set_batch` reports per entry. A batch of
-- sixteen never fails because one instance was wrong. Argument-shaped
-- problems (both modes, neither mode, a non-dense `paths` array, more than
-- 16 images, a source that returns something unusable) do fail the whole
-- prepare, before a single pixel is read.
--
-- Lifetime notes: every editable this module *creates* is destroyed straight
-- after its pixels are read (the place's own editables are never touched),
-- so a batch holds no Studio image memory once it answers; the pixel buffers
-- it hands over live in the capture registry under transmit's own
-- 16-session allowance with the shared 120 s sliding TTL.

local AssetService = game:GetService("AssetService")

local WSync = script:FindFirstAncestor("WSync")

local Log = require(WSync.Log)

local Error = require(script.Parent.Parent.Error)
local Paths = require(script.Parent.Parent.Paths)
local Rules = require(script.Parent.Parent.TransmitRules)
local WriteRules = require(script.Parent.Parent.WriteRules)

local Capture = require(script.Parent.Capture)
local History = require(script.Parent.History)
local Write = require(script.Parent.Write)

type Args = { [string]: any }

-- One resolved image ready to be read. `owned` marks the editables this
-- module created: those are destroyed after the read, the place's own never
-- are.
type Loaded = {
	editable: EditableImage,
	owned: boolean,
}

-- One batch entry before extraction: the value to read, the filename-safe
-- name already claimed for it, and where it came from.
type Target = {
	value: any,
	name: string,
	path: string?,
	label: string,
}

local Transmit = {}

Transmit.LIMITS = {
	maxItems = Rules.LIMITS.maxItems,
	maxAxis = Rules.LIMITS.maxAxis,
	maxPixels = Rules.LIMITS.maxPixels,
	maxSessions = Capture.LIMITS.maxTransmitSessions,
	-- Reads and closes are the capture ops; the daemon needs their bounds
	-- alongside the transmit ones to plan a pull.
	readMaxLen = Capture.LIMITS.readMaxLen,
	ttlSeconds = Capture.LIMITS.ttlSeconds,
	defaultTimeoutMs = Rules.LIMITS.defaultTimeoutMs,
	maxTimeoutMs = Rules.LIMITS.maxTimeoutMs,
}

-- One batch at a time: extraction yields (CreateEditableImageAsync), and two
-- interleaved batches would race the session allowance mid-flight.
local preparing = false

local function fail(code: string, message: string): never
	error(Error.new(code, message), 0)
end

--------------------------------------------------------------------------------
-- Value probing
--------------------------------------------------------------------------------

--- True for an EditableImage. Probed rather than typed: `EditableImage`
--- descends from `Object`, not `Instance`, so neither an `Instance`
--- annotation nor a `typeof` test can be relied on to recognize one.
local function isEditableImage(value: any): boolean
	local ok, result = pcall(function()
		return (value :: any):IsA("EditableImage")
	end)

	return ok and result == true
end

--- Whether a Content value carries nothing (`Content.none`, or a property
--- that was never set). `SourceType` is the authority, with the Uri/Object
--- pair as the fallback for builds that predate it.
local function isEmptyContent(content: any): boolean
	if content == nil then
		return true
	end

	if type(content) == "string" then
		return content == ""
	end

	local okSource, source = pcall(function()
		return (content :: any).SourceType
	end)

	if okSource and source ~= nil then
		-- pcall'd as a pair: a Studio build without the enum falls through to
		-- the Uri/Object test rather than raising mid-probe
		local okEnum, isNone = pcall(function()
			return source == Enum.ContentSourceType.None
		end)

		if okEnum then
			return isNone == true
		end
	end

	local _, uri = pcall(function()
		return (content :: any).Uri
	end)
	local _, object = pcall(function()
		return (content :: any).Object
	end)

	return (uri == nil or uri == "") and object == nil
end

--------------------------------------------------------------------------------
-- Extraction
--------------------------------------------------------------------------------

--- The EditableImage behind one Content value (or legacy content-id string).
--- Object-backed Content already *is* an editable image, so it is read in
--- place; everything else decodes through `CreateEditableImageAsync`, whose
--- failure (an unloadable asset, a texture the place has no rights to) is
--- this item's PLUGIN_ERROR.
local function loadContent(content: any, label: string): Loaded
	if type(content) ~= "string" then
		local okObject, object = pcall(function()
			return (content :: any).Object
		end)

		if okObject and object ~= nil and isEditableImage(object) then
			return { editable = object :: any, owned = false }
		end
	end

	local request: any = content

	if type(content) == "string" then
		-- `Content.fromUri` is the current signature; the raw-string retry
		-- below covers Studio builds that predate it (the Capture precedent)
		pcall(function()
			request = Content.fromUri(content)
		end)
	end

	local editable: EditableImage? = nil

	local ok, caught = pcall(function()
		editable = AssetService:CreateEditableImageAsync(request)
	end)

	if (not ok or editable == nil) and request ~= content then
		local okRaw = pcall(function()
			editable = (AssetService :: any):CreateEditableImageAsync(content)
		end)

		if okRaw and editable ~= nil then
			return { editable = editable :: EditableImage, owned = true }
		end
	end

	if not ok or editable == nil then
		fail(Error.PLUGIN_ERROR, `CreateEditableImageAsync failed for {label}: {tostring(caught)}`)
	end

	return { editable = editable :: EditableImage, owned = true }
end

--- The image behind one instance: the `Content`-typed property when it holds
--- something, else the legacy content-id string. An instance carrying
--- neither is NOT_FOUND — a real answer about that item, not a batch failure.
local function loadProperties(instance: Instance, contentProp: string, idProp: string, label: string): Loaded
	local okContent, content = pcall(function()
		return (instance :: any)[contentProp]
	end)

	if okContent and not isEmptyContent(content) then
		return loadContent(content, `{contentProp} of {label}`)
	end

	local okId, id = pcall(function()
		return (instance :: any)[idProp]
	end)

	if okId and type(id) == "string" and id ~= "" then
		return loadContent(id, `{idProp} of {label}`)
	end

	return fail(Error.NOT_FOUND, `{label} carries no image: {contentProp} and {idProp} are both empty`)
end

--- Resolve one batch value to a readable image, or raise this item's failure.
local function loadTarget(value: any, label: string): Loaded
	if isEditableImage(value) then
		return { editable = value :: any, owned = false }
	end

	if typeof(value) ~= "Instance" then
		fail(Error.INVALID_ARGUMENT, `{label} is a {typeof(value)}, not an EditableImage or an image-bearing instance`)
	end

	local instance = value :: Instance

	if instance:IsA("ImageLabel") or instance:IsA("ImageButton") then
		return loadProperties(instance, "ImageContent", "Image", label)
	end

	-- `Texture` derives from `Decal`, so this covers both
	if instance:IsA("Decal") then
		return loadProperties(instance, "TextureContent", "Texture", label)
	end

	if instance:IsA("MeshPart") then
		return loadProperties(instance, "TextureContent", "TextureID", label)
	end

	return fail(
		Error.INVALID_ARGUMENT,
		`{label} is a {instance.ClassName}; transmit reads EditableImage, ImageLabel, ImageButton, `
			.. "Decal, Texture, and MeshPart"
	)
end

--- Editables this module created are destroyed as soon as their pixels are
--- read; the place's own editables are left exactly as they were found.
local function release(loaded: Loaded)
	if not loaded.owned then
		return
	end

	pcall(function()
		loaded.editable:Destroy()
	end)
end

--- Read one loaded image as tightly-packed straight-alpha RGBA8. The
--- per-item dimension limits are enforced first, so an oversize image is
--- refused before a 64 MB read, not after it.
local function readPixels(loaded: Loaded, label: string): (buffer, number, number)
	local okSize, size = pcall(function()
		return loaded.editable.Size
	end)

	if not okSize or typeof(size) ~= "Vector2" then
		fail(Error.PLUGIN_ERROR, `unable to read the dimensions of {label}`)
	end

	local dimensions = size :: Vector2
	local width = dimensions.X
	local height = dimensions.Y
	local problem = Rules.checkDims(width, height)

	if problem ~= nil then
		fail(Error.INVALID_ARGUMENT, `{label}: {problem}`)
	end

	local pixels: buffer? = nil

	local ok, caught = pcall(function()
		pixels = loaded.editable:ReadPixelsBuffer(Vector2.zero, dimensions)
	end)

	if not ok or pixels == nil then
		fail(Error.PLUGIN_ERROR, `ReadPixelsBuffer failed for {label}: {tostring(caught)}`)
	end

	return pixels :: buffer, width, height
end

--------------------------------------------------------------------------------
-- Batch assembly
--------------------------------------------------------------------------------

--- The name an instance contributes. A freshly created EditableImage carries
--- its class name, which says nothing about the image — those fall through to
--- the caller's `source-<index>` fallback.
local function nameOf(value: any): string?
	local ok, name = pcall(function()
		return (value :: any).Name
	end)

	if not ok or type(name) ~= "string" or name == "" then
		return nil
	end

	if name == "EditableImage" and isEditableImage(value) then
		return nil
	end

	return name
end

--- The `paths` batch: one target per requested path, in request order.
--- Resolution failures are recorded per item — a typo in one path must not
--- cost the caller the other fifteen images.
local function collectPaths(batch: Rules.Batch, raw: any): { Target }
	local paths = Rules.validatePaths(raw)
	local targets: { Target } = {}

	for index, path in paths do
		local okResolve, resolved = pcall(Paths.resolve, path)

		if not okResolve then
			-- Name the failure after the leaf the caller asked for, so the
			-- report reads like the rest of the batch. Split is best-effort:
			-- a path malformed enough to fail parsing keeps its raw text.
			local okSplit, segments = pcall(Paths.split, path)
			local hint: any = path

			if okSplit and #(segments :: { string }) > 0 then
				hint = (segments :: { string })[#(segments :: { string })]
			end

			Rules.addFailure(batch, Rules.claimName(batch, hint, `path-{index}`), Error.toPayload(resolved), path)
			continue
		end

		local instance = resolved :: Instance
		local name = Rules.claimName(batch, instance.Name, `path-{index}`)

		table.insert(targets, {
			value = instance,
			name = name,
			path = path,
			label = `'{path}'`,
		})
	end

	return targets
end

--- The `source` batch: run the chunk at the `eval` trust level, then
--- normalize whatever it returned into targets.
---
--- The chunk is bracketed and audited exactly like `eval` the moment it
--- returns — before the batch can fail on anything downstream — so
--- place-visible Luau that ran is always undoable and always in the daemon's
--- writes.log (Design §6.4; the `ctx.audit` sink is designed to survive a
--- failed response for precisely this reason).
local function collectSource(batch: Rules.Batch, raw: any, ctx: any): { Target }
	local returns = Write.runSource(raw, "wsync-transmit", "the transmit source raised")

	-- Bracket and audit before anything downstream can raise (eval's shape,
	-- verbatim): the chunk has already run by this point.
	History.opWaypoint("transmit_prepare", "")
	ctx.audit(WriteRules.shapeAudit("transmit_prepare", ""))

	local values = Rules.flattenReturns(returns)

	Rules.checkItemCount(#values, "the transmit source returned")

	local targets: { Target } = table.create(#values)

	for index, value in values do
		local fallback = `source-{index}`
		local name = Rules.claimName(batch, nameOf(value), fallback)

		targets[index] = {
			value = value,
			name = name,
			path = nil,
			label = `the source result {index} ('{name}')`,
		}
	end

	return targets
end

--- Extract every target in order, adopting each success into the capture
--- registry under transmit's allowance and recording each failure beside it.
local function extractAll(batch: Rules.Batch, targets: { Target }, deadline: number, timeoutMs: number)
	for index, target in targets do
		if os.clock() >= deadline then
			fail(
				Error.TIMEOUT,
				`the transmit batch exceeded its {timeoutMs} ms budget after {#batch.items} of {#targets} images`
			)
		end

		local ok, caught = pcall(function()
			local loaded = loadTarget(target.value, target.label)
			local okRead, pixels, width, height = pcall(readPixels, loaded, target.label)

			release(loaded)

			if not okRead then
				error(pixels, 0)
			end

			local item = Capture.adopt(pixels :: buffer, width :: number, height :: number, "transmit")

			item.name = target.name
			item.path = target.path

			Rules.addItem(batch, item)
		end)

		if not ok then
			Log.trace(`transmit_prepare: item {index} ('{target.name}') failed`)
			Rules.addFailure(batch, target.name, Error.toPayload(caught), target.path)
		end
	end
end

--------------------------------------------------------------------------------
-- Ops
--------------------------------------------------------------------------------

Transmit.ops = {} :: { [string]: (args: Args, ctx: any) -> any }

--- `transmit_prepare {source?, paths?, timeoutMs?}` → `{items, failures}`.
--- Each item is `{captureId, width, height, bytes, sha256, name, path?}` —
--- a standard capture session, so the daemon pulls it with `capture_read`
--- and drops it with `capture_close`. Items and failures together cover
--- every requested image, in request order.
function Transmit.ops.transmit_prepare(args: Args, ctx)
	local mode = Rules.selectMode(args.source, args.paths)
	local timeoutMs = Rules.validateTimeout(args.timeoutMs)

	if preparing then
		fail(Error.CONFLICT, "another transmit batch is preparing — retry once it finishes")
	end

	-- A `source` chunk runs arbitrary Luau: it must not interleave with a
	-- half-staged capture scene (hidden GUIs, moved camera, backdrops).
	if mode == "source" and Capture.isRendering() then
		fail(Error.CONFLICT, "a capture is rendering — retry once it finishes")
	end

	preparing = true

	local ok, result = xpcall(function()
		local deadline = os.clock() + timeoutMs / 1000
		local batch = Rules.newBatch()
		local targets = if mode == "paths"
			then collectPaths(batch, args.paths)
			else collectSource(batch, args.source, ctx)

		-- Refuse a batch that cannot fit before reading a single pixel: every
		-- item pins a session until the daemon closes it.
		local free = Capture.freeSessions("transmit")

		if #targets > free then
			fail(
				Error.CONFLICT,
				`{#targets} images need {#targets} transmit sessions, but only {free} of `
					.. `{Capture.LIMITS.maxTransmitSessions} are free — capture_close the finished ones first`
			)
		end

		extractAll(batch, targets, deadline, timeoutMs)

		Log.trace(`transmit_prepare ({mode}): {#batch.items} images, {#batch.failures} failures`)

		return Rules.response(batch)
	end, function(caught)
		return caught
	end)

	preparing = false

	if not ok then
		error(result, 0)
	end

	return result
end

return Transmit
