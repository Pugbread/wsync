-- HTTP long-poll transport — Argon's original protocol, preserved verbatim
-- (msgpack over HttpService:RequestAsync, /subscribe + /read loop).
--
-- This is the compatibility path: it remains the default until the daemon's
-- WebSocket protocol (wsync/1) ships, and stays as the fallback for Studio
-- builds without `CreateWebStreamClient`. Transport selection lives in
-- Transport/init.luau.

local WSync = script:FindFirstAncestor("WSync")

local Promise = require(WSync.Packages.Promise)

local Types = require(WSync.Types)
local Place = require(WSync.Place)
local Log = require(WSync.Log)

local Http = require(script.Parent.Http)
local Error = require(script.Parent.Error)

local function generateId(): number
	return math.floor(math.random() * 10 ^ 9)
end

local LongPoll = {}
LongPoll.__index = LongPoll

--- `helloProtocol` is the `protocol` field from a successful `GET /hello`
--- probe, or nil when the daemon has no `/hello` (a stock Argon server).
--- When present and >= 1, the daemon is accepted by protocol and Core treats
--- the msgpack `/details` version as informational — a wsync/1 daemon in
--- `compat_argon` mode deliberately pins that field to an Argon-compatible
--- constant (Design §5.1). Without it, the strict semver gate applies
--- unchanged. `helloScope` is the hello's sync scope (Design §7.0); nil =
--- full.
function LongPoll.new(host: string, port: number, https: boolean, helloProtocol: number?, helloScope: string?)
	local self = setmetatable({}, LongPoll)

	self.kind = "LongPoll" :: "LongPoll"
	self.isSubscribed = false
	self.protocol = helloProtocol
	self.scope = helloScope
	self.clientId = generateId()
	self.placeName = Place.Name
	self.host = host
	self.port = port
	self.https = https
	self.__onStatus = nil :: ((status: Types.TransportStatus) -> ())?

	return self
end

function LongPoll:getUrl(endpoint: string?): string
	local protocol = self.https and "https" or "http"
	return `{protocol}://{self.host}:{self.port}/{endpoint or ""}`
end

--- Register a status listener. Long-polling has no internal reconnect
--- machinery (every request already retries at the HTTP layer), so this
--- transport never emits status updates — the setter exists to satisfy the
--- transport interface.
function LongPoll:onStatus(callback: (status: Types.TransportStatus) -> ())
	self.__onStatus = callback
end

function LongPoll:fetchDetails(): Promise.TypedPromise<Types.Project>
	Log.trace("Client request: details")

	local url = self:getUrl("details")

	return Http.get(url):andThen(function(response)
		return response:decode()
	end)
end

function LongPoll:subscribe(): Promise.Promise
	Log.trace("Client request: subscribe")

	local url = self:getUrl("subscribe")

	return Http.post(url, {
		clientId = self.clientId,
		name = self.placeName,
	})
		:andThen(function()
			self.isSubscribed = true
		end)
		:catch(function(err)
			if err == Error.Unknown then
				return Promise.reject(Error.new(Error.AlreadySubscribed, self.clientId))
			else
				return Promise.reject(err)
			end
		end)
end

function LongPoll:unsubscribe(): Promise.Promise
	Log.trace("Client request: unsubscribe")

	local url = self:getUrl("unsubscribe")

	return Http.post(url, {
		clientId = self.clientId,
	})
		:andThen(function()
			self.isSubscribed = false
		end)
		:catch(function(err)
			if err == Error.Unknown then
				return Promise.reject(Error.new(Error.NotSubscribed, self.clientId))
			else
				return Promise.reject(err)
			end
		end)
end

function LongPoll:getSnapshot(instance: Types.Ref?): Promise.TypedPromise<Types.AddedSnapshot>
	Log.trace("Client request: snapshot")

	local url = self:getUrl("snapshot")

	return Http.post(url, {
		instance = instance or buffer.create(16),
	}):andThen(function(response)
		return response:decode()
	end)
end

function LongPoll:read(): Promise.TypedPromise<Types.Message>
	Log.trace("Client request: read")

	local url = self:getUrl("read")

	return Http.post(url, {
		clientId = self.clientId,
	})
		:andThen(function(response)
			return response:decode()
		end)
		:catch(function(err)
			-- We don't need to handle this as Rust queue uses `recv_timeout`
			if err == Error.Timedout then
				return self:read()
			else
				return Promise.reject(err)
			end
		end)
end

function LongPoll:write(changes: Types.Changes): Promise.Promise
	Log.trace("Client request: write")

	local url = self:getUrl("write")

	return Http.post(url, {
		clientId = self.clientId,
		changes = changes,
	})
end

function LongPoll:open(instance: Types.Ref, line: number): Promise.Promise
	Log.trace("Client request: open")

	local url = self:getUrl("open")

	return Http.post(url, {
		instance = instance,
		line = line,
	})
end

function LongPoll:destroy()
	-- Long-polling holds no persistent connection; nothing to tear down.
	-- Pending `read` requests resolve or time out on their own.
end

return LongPoll
