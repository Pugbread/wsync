--!strict
local HttpService = game:GetService("HttpService")

local WSync = script:FindFirstAncestor("WSync")

local Hex = require(WSync.Transport.Hex)

-- Nesting past this depth prints as `{...}`: a cyclic table must not take a
-- log line (and the thread writing it) down with a stack overflow
local STRINGIFY_MAX_DEPTH = 16

-- How much of a non-ref buffer a log line shows, in bytes
local BUFFER_PREVIEW_BYTES = 32

local Util = {}

--- Get length of non-numerically indexed table
function Util.len(table: { [any]: any }): number
	local count = 0

	for _ in table do
		count += 1
	end

	return count
end

--- Join second table to the first one
function Util.join(table1: { any }, table2: { any }): { any }
	for _, value in table2 do
		table.insert(table1, value)
	end

	return table1
end

--- Convert dictionary to array, ignoring keys
function Util.dictionaryToArray(dictionary: { [any]: any }, __lastArray: { any }?): { any }
	local array = __lastArray or {}

	for _, value in dictionary do
		if type(value) == "table" then
			Util.dictionaryToArray(value, array)
		else
			table.insert(array, value)
		end
	end

	return array
end

--- Find the value and key of the first element that passes the filter
function Util.find<K, V>(table: { [K]: V }, compare: (value: V, key: K) -> boolean): (V?, K?)
	for key, value in table do
		if compare(value, key) then
			return value, key
		end
	end

	return nil
end

--- A buffer as log text. Refs (the 16-byte instance IDs) print as the same
--- 32-char lowercase hex the daemon logs and the wire carries — never as raw
--- bytes, which are unreadable and, as a `gsub` replacement or a format
--- string, not even safe to print. Any other buffer prints its length and a
--- hex preview.
function Util.stringifyBuffer(value: buffer): string
	local length = buffer.len(value)

	if length == 16 then
		return Hex.encode(value)
	end

	local preview = {}

	for i = 0, math.min(length, BUFFER_PREVIEW_BYTES) - 1 do
		table.insert(preview, string.format("%02x", buffer.readu8(value, i)))
	end

	return `<buffer {length} bytes: {table.concat(preview)}{if length > BUFFER_PREVIEW_BYTES then "…" else ""}>`
end

--- Stringify the value (pretty print)
function Util.stringify(value: any, __indent: number?): string
	if type(value) == "table" then
		local meta = getmetatable(value)

		if meta and meta.__tostring then
			return tostring(value)
		end

		if not next(value) then
			return "{}"
		end

		if (__indent or 0) >= STRINGIFY_MAX_DEPTH then
			return "{...}"
		end

		local first = next(value)

		if type(first) == "number" then
			local containsTable = false

			for _, v in value do
				if type(v) == "table" then
					containsTable = true
					break
				end
			end

			if not containsTable then
				-- Element by element: `table.concat` throws on anything but
				-- strings and numbers (a boolean, a buffer ref, an Instance)
				local parts = {}

				for _, v in value do
					table.insert(parts, if type(v) == "string" then v else Util.stringify(v, __indent))
				end

				return "{" .. table.concat(parts, ", ") .. "}"
			end

			local str = "{\n"
			local indent = __indent and __indent + 1 or 1

			for i, v in value do
				str ..= str.rep("\t", indent)
				str ..= `[{i}] = {Util.stringify(v, indent)},\n`
			end

			return str .. string.rep("\t", indent - 1) .. "}"
		else
			local str = "{\n"
			local indent = __indent and __indent + 1 or 1

			for k, v in value do
				local key = type(k) == "string" and `"{k}"` or Util.stringify(k, indent)
				local val = type(v) == "string" and `"{v}"` or Util.stringify(v, indent)

				str ..= str.rep("\t", indent)
				str ..= `[{key}] = {val},\n`
			end

			return str .. string.rep("\t", indent - 1) .. "}"
		end
	elseif typeof(value) == "Instance" then
		return value:GetFullName()
	elseif type(value) == "buffer" then
		return Util.stringifyBuffer(value)
	else
		return tostring(value)
	end
end

--- `Util.stringify` that never throws — for log and error text, where a
--- value that cannot be printed (a `__tostring` that errors, an Instance
--- Studio refuses to name) must not take the caller down with it.
function Util.safeStringify(value: any): string
	local ok, text = pcall(Util.stringify, value)

	if ok and type(text) == "string" then
		return text
	end

	return `<unprintable {typeof(value)}>`
end

--- Fill the `$1`..`$n` placeholders of a message template with the given
--- values, in one pass. The replacement is a function, so argument text is
--- inserted verbatim: a `%` in it is never read as a capture reference
--- (`invalid use of '%' in replacement string`), and a `$2` inside the first
--- argument is not substituted again. A placeholder with no argument stays.
function Util.interpolate(template: string, ...: any): string
	local count = select("#", ...)
	local args = { ... }

	local filled = template:gsub("%$(%d+)", function(index: string): string
		local i = tonumber(index) :: number

		if i < 1 or i > count then
			return "$" .. index
		end

		return Util.safeStringify(args[i])
	end)

	return filled
end

--- Generate a GUID, example: 04AEBFEA-87FC-480F-A98B-E5E221007A90
function Util.generateGUID(): string
	return HttpService:GenerateGUID(false)
end

--- Cast the value to the provided type (number, string, boolean, any)
function Util.cast(value: any, target: string): any
	if type(value) == target then
		return value
	end

	if target == "number" then
		return tonumber(value)
	elseif target == "string" then
		return tostring(value)
	elseif target == "boolean" then
		return value == "true"
	else
		return value
	end
end

--- Clean all provided connections and instances
function Util.clean(...: () -> () | RBXScriptConnection | Instance | { [any]: any })
	local function clean(object: any)
		local kind = typeof(object)

		if kind == "function" then
			object()
		elseif kind == "RBXScriptConnection" then
			object:Disconnect()
		elseif kind == "Instance" then
			object:Destroy()
		elseif kind == "table" then
			if typeof(object.Destroy) == "function" then
				object:Destroy()
			else
				for _, object in object do
					clean(object)
				end
			end
		end
	end

	for _, arg in { ... } do
		clean(arg)
	end
end

--- Get the plural form of a word
function Util.plural(count: number): string
	return math.abs(count) ~= 1 and "s" or ""
end

return Util
