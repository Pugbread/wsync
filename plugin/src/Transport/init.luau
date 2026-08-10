-- Transport layer — the transport-agnostic client interface consumed by Core.
--
-- Two implementations (Design §9.1):
--   * LongPoll  — Argon's original msgpack-over-HTTP protocol, preserved.
--                 Compat path and default until the daemon speaks wsync/1.
--   * WebSocket — protocol wsync/1 (JSON text frames over
--                 `HttpService:CreateWebStreamClient`).
--
-- Selection is automatic (Design §5.1): a `GET /hello` probe that answers
-- with JSON and `protocol >= 1` marks a wsync/1 daemon, in which case the
-- WebSocket transport is used when the Studio build supports it; every other
-- outcome (404, non-JSON, no `CreateWebStreamClient`) falls back to LongPoll.
-- The chosen transport is logged.
--
-- Interface (structural, both implementations satisfy it):
--   kind: "LongPoll" | "WebSocket"
--   isSubscribed: boolean
--   protocol: number?                        -- wsync protocol from `/hello`;
--                                               nil = no-hello (stock Argon)
--                                               daemon, strict semver applies
--   scope: string?                           -- sync scope from the hello
--                                               (Design §7.0): "code"|"full",
--                                               absent = full
--   fetchDetails(): Promise<Types.Project>
--   subscribe(): Promise
--   unsubscribe(): Promise
--   getSnapshot(ref?): Promise<Types.AddedSnapshot>
--   read(): Promise<Types.Message?>          -- next server message (nil = destroyed)
--   write(changes): Promise                  -- Studio -> server syncback batch
--   open(ref, line): Promise                 -- open-in-editor
--   onStatus(callback)                       -- connect/reconnect lifecycle
--   destroy()
--
-- WebSocket-only extensions (nil on LongPoll — Core feature-checks):
--   performRequest(frame)                    -- execute a queue-ordered request
--                                               (DispatchRequest messages)
--   push(message)                            -- inject a message into the read
--                                               queue (divergence backstop)

local WSync = script:FindFirstAncestor("WSync")

local Promise = require(WSync.Packages.Promise)

local Types = require(WSync.Types)
local Log = require(WSync.Log)

local Http = require(script.Http)
local LongPoll = require(script.LongPoll)
local WebSocket = require(script.WebSocket)

export type Transport = {
	kind: "LongPoll" | "WebSocket",
	isSubscribed: boolean,
	protocol: number?,
	scope: string?,
	fetchDetails: (self: Transport) -> any,
	subscribe: (self: Transport) -> any,
	unsubscribe: (self: Transport) -> any,
	getSnapshot: (self: Transport, instance: Types.Ref?) -> any,
	read: (self: Transport) -> any,
	write: (self: Transport, changes: Types.Changes) -> any,
	open: (self: Transport, instance: Types.Ref, line: number) -> any,
	onStatus: (self: Transport, callback: (status: Types.TransportStatus) -> ()) -> (),
	destroy: (self: Transport) -> (),
	-- WebSocket-only (nil on LongPoll)
	performRequest: ((self: Transport, frame: { [string]: any }) -> ())?,
	push: ((self: Transport, message: any) -> ())?,
}

local Transport = {}

--- Probe `GET /hello` (JSON, Design §5.2). Resolves the decoded hello, or
--- nil when the address does not answer with a wsync/1 identity (including
--- connection failures — the caller decides how to proceed).
function Transport.hello(host: string, port: number, https: boolean?): Promise.TypedPromise<Types.DaemonHello?>
	local protocol = https and "https" or "http"
	local url = `{protocol}://{host}:{port}/hello`

	return Http.getJson(url)
		:andThen(function(response)
			local success, hello = pcall(function()
				return response:decode()
			end)

			if success and type(hello) == "table" and type(hello.protocol) == "number" then
				return hello
			end

			return nil
		end)
		:catch(function()
			-- 404 (pre-wsync/1 daemon), connection refused, non-JSON body...
			return nil
		end)
end

--- Pick and construct the transport for an address (never rejects — when
--- nothing is listening, LongPoll is returned and its first request will
--- produce the proper connection error).
function Transport.detect(host: string, port: number, https: boolean): Promise.TypedPromise<Transport>
	return Promise.new(function(resolve)
		local hello = Transport.hello(host, port, https):expect()

		if hello and hello.protocol >= 1 then
			if WebSocket.isSupported() then
				Log.info(`Using WebSocket transport (daemon protocol {hello.protocol})`)
				return resolve(WebSocket.new(host, port, https, hello.scope))
			end

			Log.warn(
				"Daemon speaks protocol wsync/1, but this Studio build lacks "
					.. "CreateWebStreamClient — falling back to the long-poll transport"
			)

			-- Version gating is surface-scoped (Design §5.1): this daemon
			-- already identified as wsync/1 via `/hello`, so the long-poll
			-- path accepts it by protocol and Core treats the msgpack
			-- `/details` version as informational.
			return resolve(LongPoll.new(host, port, https, hello.protocol, hello.scope))
		end

		Log.info("Daemon did not answer the wsync/1 hello probe, using the long-poll transport")

		-- No `/hello`: behavior unchanged — strict `/details` semver gating
		return resolve(LongPoll.new(host, port, https))
	end)
end

return Transport
