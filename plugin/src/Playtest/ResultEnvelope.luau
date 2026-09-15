--!strict
-- Pure packing/encoding rules shared by exec and playscript result handling.
-- Roblox Bindable transports can drop the `n` field from a mixed-key table;
-- runners therefore send its count as a separate argument. Never infer it
-- with #packed: that loses trailing nil returns and can lose interior holes.
local ResultEnvelope = {}

export type Packed = { n: number, [number]: any }

function ResultEnvelope.restore(packed: any, transportedCount: any): Packed
	if type(packed) ~= "table" then
		return table.pack(false, { message = "runner returned an invalid result envelope" })
	end
	local total = if transportedCount ~= nil then transportedCount else packed.n
	if type(total) ~= "number" or total % 1 ~= 0 or total < 1 or total > 20000 then
		return table.pack(false, { message = "runner result is missing a valid explicit return count" })
	end
	if type(packed[1]) ~= "boolean" then
		return table.pack(false, { message = "runner result is missing its success flag" })
	end
	packed.n = total
	return packed
end

local function capText(value: string, limit: number): string
	if #value > limit then return string.sub(value, 1, limit) .. "<truncated>" end
	return value
end

-- encode is injected so this module remains require-free in standalone tests.
-- The caller shares one Codec budget across all returned values.
function ResultEnvelope.build(packed: Packed, encode: (any) -> any): { [string]: any }
	local restored = ResultEnvelope.restore(packed, nil)
	if restored[1] ~= true then
		local failure = restored[2]
		if type(failure) == "table" and failure.message ~= nil then
			return {
				ok = false,
				error = capText(tostring(failure.message), 64 * 1024),
				traceback = capText(tostring(failure.traceback or failure.message), 256 * 1024),
			}
		end
		return { ok = false, error = capText(tostring(failure), 64 * 1024) }
	end
	local count = restored.n - 1
	local envelope: { [string]: any } = { ok = true, count = count }
	local results: { { [string]: any } } = {}
	local hasNil = false
	local values = {}
	for index = 1, count do
		local value = restored[index + 1]
		if value == nil then
			hasNil = true
			results[index] = { index = index, isNil = true }
			continue
		end
		local ok, encoded = pcall(encode, value)
		if not ok then
			local message = if type(encoded) == "table" and encoded.message ~= nil then encoded.message else tostring(encoded)
			return { ok = false, error = capText(`result encoding failed: {message}`, 64 * 1024) }
		end
		results[index] = { index = index, value = encoded }
		if count == 1 then envelope.value = encoded else values[index] = encoded end
	end
	-- Serialize each value only once. Duplicating it into both `results` and
	-- `value` would halve the useful payload under the 1 MiB envelope limit.
	-- Use indexed slots only when nils make the legacy representation lossy.
	if hasNil then
		envelope.results = results
	elseif count > 1 then
		envelope.value = values
	end
	return envelope
end

return ResultEnvelope
