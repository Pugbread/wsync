--!strict
-- Pure-Luau base64 codec (RFC 4648, standard alphabet with padding). The
-- encoder carries raw byte parts inside JSON frames (`source_read`,
-- `capture_read`, `clipboard_read`); the decoder accepts the
-- `clipboard_paste_chunk` payloads coming back the other way.
--
-- The existing Lib/Dom/base64 wraps Roblox's EncodingService and therefore
-- only runs inside Studio; this module has no requires and no Roblox APIs so
-- it loads under a standalone `luau` binary and is unit-tested against the
-- RFC 4648 vectors (see the wave-4/wave-5 test suites).

local Base64 = {}

local ALPHABET = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"

local CHARS: { [number]: string } = {}
for index = 0, 63 do
	CHARS[index] = ALPHABET:sub(index + 1, index + 1)
end

local VALUES: { [number]: number } = {}
for index = 0, 63 do
	VALUES[string.byte(ALPHABET, index + 1)] = index
end

--- Encode a byte string as base64 (with `=` padding).
function Base64.encode(input: string): string
	local length = #input
	local out = table.create(math.ceil(length / 3))
	local pieces = 0

	local wholeEnd = length - (length % 3)

	for offset = 1, wholeEnd, 3 do
		local byte1, byte2, byte3 = string.byte(input, offset, offset + 2)
		local word = byte1 * 65536 + byte2 * 256 + byte3

		pieces += 1
		out[pieces] = CHARS[math.floor(word / 262144)]
			.. CHARS[math.floor(word / 4096) % 64]
			.. CHARS[math.floor(word / 64) % 64]
			.. CHARS[word % 64]
	end

	local remainder = length - wholeEnd

	if remainder == 1 then
		local byte1 = string.byte(input, length)
		local word = byte1 * 65536

		pieces += 1
		out[pieces] = CHARS[math.floor(word / 262144)] .. CHARS[math.floor(word / 4096) % 64] .. "=="
	elseif remainder == 2 then
		local byte1, byte2 = string.byte(input, length - 1, length)
		local word = byte1 * 65536 + byte2 * 256

		pieces += 1
		out[pieces] = CHARS[math.floor(word / 262144)]
			.. CHARS[math.floor(word / 4096) % 64]
			.. CHARS[math.floor(word / 64) % 64]
			.. "="
	end

	return table.concat(out)
end

--- Decode a base64 string (strict RFC 4648: standard alphabet, `=` padding,
--- no whitespace). Returns the decoded bytes, or nil when the input is
--- malformed — the caller owns turning that into a typed error.
function Base64.decode(input: string): string?
	if type(input) ~= "string" or #input % 4 ~= 0 then
		return nil
	end

	if input == "" then
		return ""
	end

	local length = #input
	local padding = 0

	if input:sub(length, length) == "=" then
		padding = if input:sub(length - 1, length - 1) == "=" then 2 else 1
	end

	local out = table.create(length / 4)
	local pieces = 0
	local dataEnd = length - (if padding > 0 then 4 else 0)

	for offset = 1, dataEnd, 4 do
		local byte1, byte2, byte3, byte4 = string.byte(input, offset, offset + 3)
		local value1, value2, value3, value4 = VALUES[byte1], VALUES[byte2], VALUES[byte3], VALUES[byte4]

		if value1 == nil or value2 == nil or value3 == nil or value4 == nil then
			return nil
		end

		local word = value1 * 262144 + value2 * 4096 + value3 * 64 + value4

		pieces += 1
		out[pieces] = string.char(math.floor(word / 65536), math.floor(word / 256) % 256, word % 256)
	end

	if padding > 0 then
		local byte1, byte2, byte3 = string.byte(input, dataEnd + 1, dataEnd + 3)
		local value1, value2 = VALUES[byte1], VALUES[byte2]

		if value1 == nil or value2 == nil then
			return nil
		end

		pieces += 1

		if padding == 2 then
			-- One decoded byte: the low 4 bits of the second character must be
			-- zero for a canonical encoding
			if value2 % 16 ~= 0 then
				return nil
			end

			out[pieces] = string.char(value1 * 4 + math.floor(value2 / 16))
		else
			local value3 = VALUES[byte3]

			if value3 == nil or value3 % 4 ~= 0 then
				return nil
			end

			local word = value1 * 262144 + value2 * 4096 + value3 * 64

			out[pieces] = string.char(math.floor(word / 65536), math.floor(word / 256) % 256)
		end
	end

	return table.concat(out)
end

return Base64
