--!strict
-- Typed remote-op errors (Design §5.3): the seven wire codes, kept verbatim
-- from Ro-Sync. Handlers raise these with `error(Error.new(code, message))`;
-- the dispatcher in Remote/init.luau turns anything else that escapes a
-- handler into a PLUGIN_ERROR with a truncated, prefix-scrubbed message.
--
-- The `__remoteError` marker shape is also constructed inline by the
-- dependency-free modules (Codec, Selector) so they stay loadable under
-- standalone Luau — keep the three fields in sync with `RemoteError`.

local Error = {}

export type RemoteError = {
	__remoteError: true,
	code: string,
	message: string,
}

export type ErrorPayload = {
	code: string,
	message: string,
}

Error.UNKNOWN_OP = "UNKNOWN_OP"
Error.NOT_FOUND = "NOT_FOUND"
Error.PERMISSION_REQUIRED = "PERMISSION_REQUIRED"
Error.TIMEOUT = "TIMEOUT"
Error.INVALID_ARGUMENT = "INVALID_ARGUMENT"
Error.CONFLICT = "CONFLICT"
Error.PLUGIN_ERROR = "PLUGIN_ERROR"

local VALID_CODES: { [string]: true } = {
	[Error.UNKNOWN_OP] = true,
	[Error.NOT_FOUND] = true,
	[Error.PERMISSION_REQUIRED] = true,
	[Error.TIMEOUT] = true,
	[Error.INVALID_ARGUMENT] = true,
	[Error.CONFLICT] = true,
	[Error.PLUGIN_ERROR] = true,
}

-- Outbound error messages are bounded so a failing handler can never leak an
-- unbounded payload back over the wire.
local MESSAGE_LIMIT = 512

--- Truncate to `limit` bytes without splitting a UTF-8 codepoint, appending a
--- marker when anything was cut. Invalid UTF-8 input is replaced outright —
--- error messages must never make the JSON frame unencodable.
function Error.truncateMessage(message: string, limit: number?): string
	local max = limit or MESSAGE_LIMIT

	if utf8.len(message) == nil then
		return "<non-UTF-8 error message>"
	end

	if #message <= max then
		return message
	end

	local cut = max

	-- Back off a partial multi-byte sequence (continuation bytes are 0x80-0xBF)
	while cut > 1 do
		local byte = string.byte(message, cut + 1)

		if byte == nil or byte < 0x80 or byte > 0xBF then
			break
		end

		cut -= 1
	end

	return message:sub(1, cut) .. "<truncated>"
end

function Error.new(code: string, message: string): RemoteError
	if not VALID_CODES[code] then
		-- A bad code is a plugin bug; keep the wire contract intact.
		code = Error.PLUGIN_ERROR
	end

	return {
		__remoteError = true :: true,
		code = code,
		message = Error.truncateMessage(message),
	}
end

function Error.is(value: any): boolean
	return type(value) == "table" and value.__remoteError == true and type(value.code) == "string"
end

--- Normalize anything caught from a handler into a wire `{code, message}`
--- payload. Non-RemoteError values become PLUGIN_ERROR; the standard Luau
--- `<chunk>:<line>:` prefix is scrubbed so internal script paths and line
--- numbers never leak into responses.
function Error.toPayload(caught: any): ErrorPayload
	if Error.is(caught) then
		local code = caught.code

		if not VALID_CODES[code] then
			code = Error.PLUGIN_ERROR
		end

		return {
			code = code,
			message = Error.truncateMessage(tostring(caught.message)),
		}
	end

	local message = tostring(caught)

	-- `error("msg")` produces "<chunk>:<line>: msg" — strip that one prefix.
	local scrubbed = message:gsub("^[^\n]-:%d+:%s*", "", 1)

	return {
		code = Error.PLUGIN_ERROR,
		message = Error.truncateMessage(scrubbed),
	}
end

return Error
