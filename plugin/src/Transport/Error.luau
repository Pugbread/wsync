local WSync = script:FindFirstAncestor("WSync")

local Util = require(WSync.Util)

export type Error = {
	message: string,
	kind: string,
}

local Error = {
	Unknown = "Unknown HTTP error: $1",
	-- WSync client
	AlreadySubscribed = "Client with this ID: $1, already subscribed to the server",
	NotSubscribed = "Client with this ID: $1, was not subscribed to the server",
	-- HttpService
	ConnectFail = "Failed to connect! Make sure the server is running and the address is correct",
	ConnectionClosed = "Connection closed! The server has been stopped",
	DnsResolve = "Host name is corrupted or not found",
	Timedout = "HTTP request timed out",
	-- WebSocket transport (protocol wsync/1)
	WebSocketUnavailable = "This Roblox Studio build does not support CreateWebStreamClient",
	OpenFailed = "Failed to open the WebSocket connection: $1",
	HandshakeFailed = "The server did not accept the WebSocket handshake: $1",
	NotConnected = "The WebSocket connection is not established",
	MalformedFrame = "Received a malformed frame from the server: $1",
	NotSupported = "Operation '$1' is not supported by the $2 transport",
}

function Error.__new(message: string, kind: string): Error
	local err = setmetatable({
		message = message,
		kind = kind,
	}, Error)

	return err
end

function Error.new(err: Error, ...): Error
	err = table.clone(err)

	for i, v in { ... } do
		err.message = err.message:gsub("$" .. i, Util.stringify(v))
	end

	return err
end

function Error.fromResponse(response: { [string]: any }): Error
	local message = response.Body ~= "" and response.Body or response.StatusMessage
	return Error.__new(Error.Unknown.message:gsub("$1", message), "Unknown")
end

function Error.fromMessage(message: string): Error
	for kind, err in Error do
		if type(err) == "table" and message:find(kind) and kind ~= "Unknown" then
			return Error.__new(err.message, kind)
		end
	end

	if message == "HttpError: Unknown" or message == "HttpError: NetFail" then
		return Error.new(Error.ConnectFail)
	end

	return Error.__new(Error.Unknown.message:gsub("$1", message), "Unknown")
end

function Error:__eq(other: Error): boolean
	return self.kind == other.kind
end

function Error:__tostring(): string
	return self.message
end

-- Convert all strings to Error objects
for kind, message in Error do
	if type(message) == "string" then
		Error[kind] = Error.__new(message, kind)
	end
end

return Error
