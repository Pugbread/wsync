local HttpService = game:GetService("HttpService")

local WSync = script:FindFirstAncestor("WSync")

local Promise = require(WSync.Packages.Promise)
local MsgPack = require(WSync.Lib.MsgPack)

local Error = require(script.Parent.Error)

type Encoding = "msgpack" | "json"

type Response = {
	Body: string,
	Headers: { [string]: string },
	StatusCode: number,
	StatusMessage: string,
	Success: boolean,
	decode: () -> { [string]: any },
}

local CONTENT_TYPES: { [Encoding]: string } = {
	msgpack = "application/msgpack",
	json = "application/json",
}

local function encodeBody(body: { [string]: any }, encoding: Encoding): string
	if encoding == "json" then
		return HttpService:JSONEncode(body)
	else
		return MsgPack.encode(body)
	end
end

local function methodify(response: { [string]: any }, encoding: Encoding): any
	return setmetatable(response, {
		__index = {
			decode = function(self)
				if encoding == "json" then
					return HttpService:JSONDecode(self.Body)
				else
					return MsgPack.decode(self.Body)
				end
			end,
		},
	})
end

local function request(
	method: string,
	url: string,
	body: { [string]: any }?,
	encoding: Encoding?
): Promise.TypedPromise<Response>
	local format: Encoding = encoding or "msgpack"

	return Promise.new(function(resolve, reject)
		local success, response = pcall(function()
			return HttpService:RequestAsync({
				Url = url,
				Method = method,
				Headers = {
					["Content-Type"] = CONTENT_TYPES[format],
				},
				Body = body and encodeBody(body, format) or nil,
			})
		end)

		if success then
			if response.Success then
				resolve(methodify(response, format))
			else
				reject(Error.fromResponse(response))
			end
		else
			reject(Error.fromMessage(response))
		end
	end)
end

local Http = {}

-- MessagePack routes (Argon-compatible long-poll protocol)

function Http.get(url: string)
	return request("GET", url)
end

function Http.post(url: string, body: { [string]: any }?)
	return request("POST", url, body)
end

-- JSON routes (protocol wsync/1: /hello, /snapshot, ...)

function Http.getJson(url: string)
	return request("GET", url, nil, "json")
end

function Http.postJson(url: string, body: { [string]: any }?)
	return request("POST", url, body, "json")
end

return Http
