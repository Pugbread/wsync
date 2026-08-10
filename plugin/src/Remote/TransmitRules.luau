--!strict
-- Pure rules for the transmit op family (transmit.json; wave-7 pinned
-- contract). Transmit is the mirror of the capture renderer: instead of
-- manufacturing pixels it reads them *out* of image-bearing instances, and
-- the decisions that shape the batch live here —
--
--   * mode selection: exactly one of `source`/`paths` (both, or neither, is
--     INVALID_ARGUMENT — a prepare that silently ignored one of them would
--     hand back pixels the caller never asked for);
--   * the ≤16-items-per-prepare cap, rejected outright rather than truncated;
--   * per-item dimension limits (≤4096 per axis, ≤16 777 216 px), returned as
--     a *problem string* so the handler can record one oversize image as that
--     item's failure instead of the batch's;
--   * filename-safe item naming, since the CLI writes `<name>.png`, plus
--     case-insensitive de-duplication inside a batch so two instances called
--     `Icon` cannot collapse onto one file on macOS or Windows;
--   * the batch accumulator that shapes the parallel `items`/`failures`
--     arrays.
--
-- Split out of Handlers/Transmit for the reason Capture/Options and
-- WriteRules are: no requires, no Roblox APIs, so every decision above is
-- exercised under a standalone `luau` binary in the behavior tests.
--
-- Violations raise inline `__remoteError` INVALID_ARGUMENT markers (the
-- Codec/Options pattern — keep the shape in sync with Remote/Error).

local TransmitRules = {}

TransmitRules.LIMITS = {
	-- Per prepare (pinned). Each item pins a capture session until it is read
	-- and closed, so the batch cap and the transmit session allowance are the
	-- same number by construction.
	maxItems = 16,
	-- Per item — the capture limits applied per image, not per batch.
	maxAxis = 4096,
	maxPixels = 16777216,
	-- Filename budget for `<name>.png` plus whatever suffix the CLI adds.
	maxNameBytes = 64,
	-- Wall-clock budget for one prepare. `CreateEditableImageAsync` yields per
	-- item, so a 16-image batch needs far more than the 5 s op default; the
	-- ceiling exists so a typo cannot pin the plugin for an hour.
	defaultTimeoutMs = 30000,
	maxTimeoutMs = 600000,
}

export type Mode = "source" | "paths"

-- What a `source` chunk returned, packed (Handlers/Write.runSource)
export type Returns = { n: number, [number]: any }

export type Failure = {
	name: string,
	path: string?,
	error: { code: string, message: string },
}

export type Batch = {
	items: { { [string]: any } },
	failures: { Failure },
	-- Claimed names, lower-cased: case-insensitive filesystems would collapse
	-- `Icon.png` and `icon.png` onto one file.
	taken: { [string]: number },
}

local function invalid(message: string): never
	error({ __remoteError = true, code = "INVALID_ARGUMENT", message = message }, 0)
end

--------------------------------------------------------------------------------
-- Argument rules
--------------------------------------------------------------------------------

--- Which batch the arguments select. Exactly one of `source`/`paths` (pinned):
--- neither and both are equally INVALID_ARGUMENT.
function TransmitRules.selectMode(source: any, paths: any): Mode
	if source ~= nil and paths ~= nil then
		invalid("pass exactly one of source or paths, not both")
	end

	if source ~= nil then
		if type(source) ~= "string" or source == "" then
			invalid("source must be a non-empty string")
		end

		return "source"
	end

	if paths == nil then
		invalid("pass exactly one of source or paths")
	end

	return "paths"
end

--- Reject a batch larger than the cap. Truncating instead would hand back a
--- silently incomplete set of PNGs.
function TransmitRules.checkItemCount(count: number, label: string)
	if count > TransmitRules.LIMITS.maxItems then
		invalid(`{label} {count} images; a transmit prepare is limited to {TransmitRules.LIMITS.maxItems}`)
	end
end

--- Validate the `paths` array: a non-empty dense array of strings, within the
--- item cap. Whether each path *resolves* is a per-item question answered
--- later — a typo in one path must not cost the caller the other fifteen
--- images.
function TransmitRules.validatePaths(value: any): { string }
	if type(value) ~= "table" then
		invalid("paths must be an array of Studio path strings")
	end

	local total = 0

	for _ in value :: { [any]: any } do
		total += 1
	end

	if total ~= #(value :: { any }) then
		invalid("paths must be a dense array")
	end

	if total == 0 then
		invalid("paths must contain at least one Studio path")
	end

	TransmitRules.checkItemCount(total, "paths requests")

	local out = table.create(total)

	for index = 1, total do
		local path = (value :: { any })[index]

		if type(path) ~= "string" then
			invalid(`paths[{index}] must be a Studio path string`)
		end

		out[index] = path
	end

	return out
end

--- The prepare-wide wall-clock budget in milliseconds.
function TransmitRules.validateTimeout(value: any): number
	if value == nil then
		return TransmitRules.LIMITS.defaultTimeoutMs
	end

	if
		type(value) ~= "number"
		or value ~= value
		or value % 1 ~= 0
		or value < 1
		or value > TransmitRules.LIMITS.maxTimeoutMs
	then
		invalid(`timeoutMs must be an integer of 1-{TransmitRules.LIMITS.maxTimeoutMs}`)
	end

	return value
end

--- Flatten what a `source` chunk returned into a list of image values: every
--- return value contributes, and any of them may be an array of values. Nils
--- are skipped, so `return maybeImage()` degrades to an empty batch instead
--- of an error. Whether each value *is* an image is decided later, per item;
--- only the container shape is judged here.
function TransmitRules.flattenReturns(returns: Returns): { any }
	local values: { any } = {}

	for index = 1, returns.n do
		local value = returns[index]

		if value == nil then
			continue
		end

		if type(value) ~= "table" then
			table.insert(values, value)
			continue
		end

		local total = 0

		for _ in value :: { [any]: any } do
			total += 1
		end

		if total ~= #(value :: { any }) then
			invalid(
				"the transmit source returned a table that is not an array of images "
					.. "(return an EditableImage, an image-bearing instance, or an array of either)"
			)
		end

		for entry = 1, total do
			local item = (value :: { any })[entry]

			if item ~= nil then
				table.insert(values, item)
			end
		end
	end

	return values
end

--------------------------------------------------------------------------------
-- Per-item limits
--------------------------------------------------------------------------------

--- Check one image's dimensions against the per-item limits. Returns a
--- problem string (never raises): an oversize image is that item's failure,
--- not the batch's.
function TransmitRules.checkDims(width: any, height: any): string?
	if
		type(width) ~= "number"
		or type(height) ~= "number"
		or width ~= width
		or height ~= height
		or width % 1 ~= 0
		or height % 1 ~= 0
		or width < 1
		or height < 1
	then
		return "the image has no readable whole-pixel dimensions"
	end

	if width > TransmitRules.LIMITS.maxAxis or height > TransmitRules.LIMITS.maxAxis then
		return `the image is {width}×{height}; each axis is limited to {TransmitRules.LIMITS.maxAxis} pixels`
	end

	if width * height > TransmitRules.LIMITS.maxPixels then
		return `the image is {width}×{height} = {width * height} pixels; the limit is {TransmitRules.LIMITS.maxPixels}`
	end

	return nil
end

--------------------------------------------------------------------------------
-- Item naming
--------------------------------------------------------------------------------

-- Device names on Windows: `CON.png` is not a file there whatever the
-- directory says.
local WINDOWS_RESERVED: { [string]: boolean } = { CON = true, PRN = true, AUX = true, NUL = true }

for index = 1, 9 do
	WINDOWS_RESERVED[`COM{index}`] = true
	WINDOWS_RESERVED[`LPT{index}`] = true
end

--- Turn an instance name into a filename-safe stem, falling back when nothing
--- usable survives. Studio names carry spaces, slashes, quotes, and non-ASCII
--- bytes; the CLI writes `<name>.png`, so everything outside the portable
--- filename set becomes a dash.
function TransmitRules.sanitizeName(raw: any, fallback: string): string
	local text = if type(raw) == "string" then raw else ""
	local cleaned = text:gsub("[^%w%._%-]", "-")

	cleaned = cleaned:gsub("%-+", "-")
	-- A leading dot hides the file and a leading dash reads as a flag;
	-- Windows silently strips trailing dots.
	cleaned = cleaned:gsub("^[%.%-]+", "")
	cleaned = cleaned:gsub("[%.%-]+$", "")

	if #cleaned > TransmitRules.LIMITS.maxNameBytes then
		cleaned = cleaned:sub(1, TransmitRules.LIMITS.maxNameBytes)
		cleaned = cleaned:gsub("[%.%-]+$", "")
	end

	if cleaned == "" then
		return fallback
	end

	if WINDOWS_RESERVED[cleaned:upper()] then
		return `{cleaned}-image`
	end

	return cleaned
end

function TransmitRules.newBatch(): Batch
	return { items = {}, failures = {}, taken = {} }
end

--- Sanitize `raw` (falling back to `fallback`) and reserve it inside the
--- batch. A name already claimed gains the smallest free `-<n>` suffix, so a
--- batch of same-named instances still writes N distinct PNGs instead of
--- overwriting one.
function TransmitRules.claimName(batch: Batch, raw: any, fallback: string): string
	local base = TransmitRules.sanitizeName(raw, fallback)
	local key = base:lower()
	local claimed = batch.taken[key]

	if claimed == nil then
		batch.taken[key] = 1

		return base
	end

	local nth = claimed

	-- Bounded by the item cap: at most `maxItems` names are ever claimed, so
	-- one of the first `maxItems + 1` suffixes is always free.
	for _ = 1, TransmitRules.LIMITS.maxItems + 1 do
		nth += 1

		local candidate = `{base}-{nth}`
		local candidateKey = candidate:lower()

		if batch.taken[candidateKey] == nil then
			batch.taken[key] = nth
			batch.taken[candidateKey] = 1

			return candidate
		end
	end

	-- Unreachable while the cap holds; a distinct name still beats a collision
	local unique = `{base}-{nth + 1}`

	batch.taken[unique:lower()] = 1

	return unique
end

--------------------------------------------------------------------------------
-- Batch accumulation
--------------------------------------------------------------------------------

function TransmitRules.addItem(batch: Batch, item: { [string]: any })
	table.insert(batch.items, item)
end

--- Record one per-item failure: a single unreadable instance must not cost
--- the caller the rest of the batch. `error` carries the same `{code,
--- message}` payload shape `set_batch` reports per entry.
function TransmitRules.addFailure(batch: Batch, name: string, payload: { code: string, message: string }, path: string?)
	table.insert(batch.failures, {
		name = name,
		path = path,
		error = payload,
	})
end

--- The wire response. Both arrays are always present: `HttpService:JSONEncode`
--- renders an empty Luau table as `[]`, so an empty batch stays array-typed
--- for the daemon.
function TransmitRules.response(batch: Batch): { [string]: any }
	return {
		items = batch.items,
		failures = batch.failures,
	}
end

return TransmitRules
