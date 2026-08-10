-- WebSocket transport — protocol `wsync/1` (Design §5.3).
--
-- One WebSocket per client, JSON text frames via
-- `HttpService:CreateWebStreamClient(Enum.WebStreamClientType.WebSocket, { Url = url })`.
--
-- Handshake: client sends `{type="hello", clientId, role="plugin", protocol=1, name}`;
-- the daemon replies with its own hello (project details incl. rootRefs) or a
-- `shutdown{reason, code, retryable}` frame. Heartbeat is `ping`/`pong` at ~2 s
-- with a timeout-forced reconnect. Reconnects back off exponentially with a
-- visible countdown published through `onStatus`; a `retryable = false`
-- shutdown halts the loop for good.
--
-- Refs on this path are 32-char lowercase hex strings on the wire; this module
-- converts them to/from Core's 16-byte buffer Refs at the boundary (see Hex).

local HttpService = game:GetService("HttpService")

local WSync = script:FindFirstAncestor("WSync")

local Promise = require(WSync.Packages.Promise)

local Types = require(WSync.Types)
local Place = require(WSync.Place)
local Util = require(WSync.Util)
local Log = require(WSync.Log)
local Remote = require(WSync.Remote)

local Http = require(script.Parent.Http)
local Hex = require(script.Parent.Hex)
local Error = require(script.Parent.Error)

local PROTOCOL = 1

local OPEN_TIMEOUT = 10
local HANDSHAKE_TIMEOUT = 10
local HEARTBEAT_INTERVAL = 2
local HEARTBEAT_TIMEOUT = 8
local BACKOFF_START = 1
local BACKOFF_MAX = 30
local QUEUE_POLL = 0.05

--------------------------------------------------------------------------------
-- Ref codecs (wire hex <-> Core buffer)
--------------------------------------------------------------------------------

local function decodeSnapshot(pod: { [string]: any }): Types.Snapshot?
	local id = Hex.decode(pod.id)

	if not id then
		return nil
	end

	local children = {}

	for index, child in pod.children or {} do
		local decoded = decodeSnapshot(child)

		if not decoded then
			return nil
		end

		children[index] = decoded
	end

	local snapshot: { [string]: any } = {
		id = id,
		meta = pod.meta or { keepUnknowns = false },
		name = pod.name,
		class = pod.class,
		properties = pod.properties or {},
		children = children,
	}

	local parent = Hex.decode(pod.parent)

	if parent then
		snapshot.parent = parent
	end

	return snapshot :: any
end

local function decodeUpdatedSnapshot(pod: { [string]: any }): Types.UpdatedSnapshot?
	local id = Hex.decode(pod.id)

	if not id then
		return nil
	end

	local snapshot = table.clone(pod)
	snapshot.id = id

	return snapshot :: any
end

local function decodeChanges(pod: { [string]: any }): Types.Changes?
	local changes = {
		additions = {},
		updates = {},
		removals = {},
	}

	for index, addition in pod.additions or {} do
		local snapshot = decodeSnapshot(addition)

		if not snapshot then
			return nil
		end

		changes.additions[index] = snapshot
	end

	for index, update in pod.updates or {} do
		local snapshot = decodeUpdatedSnapshot(update)

		if not snapshot then
			return nil
		end

		changes.updates[index] = snapshot
	end

	for index, removal in pod.removals or {} do
		local ref = Hex.decode(removal)

		if not ref then
			return nil
		end

		changes.removals[index] = ref
	end

	return changes :: any
end

local function encodeSnapshot(snapshot: { [string]: any }): { [string]: any }
	local pod = table.clone(snapshot)

	pod.id = Hex.encode(snapshot.id)

	if snapshot.parent then
		pod.parent = Hex.encode(snapshot.parent)
	end

	if snapshot.children then
		local children = {}

		for index, child in snapshot.children do
			children[index] = encodeSnapshot(child)
		end

		pod.children = children
	end

	return pod
end

local function encodeChanges(changes: Types.Changes): { [string]: any }
	local pod = {
		additions = {},
		updates = {},
		removals = {},
	}

	for index, addition in changes.additions do
		pod.additions[index] = encodeSnapshot(addition :: any)
	end

	for index, update in changes.updates do
		pod.updates[index] = encodeSnapshot(update :: any)
	end

	for _, removal in changes.removals do
		if typeof(removal) == "buffer" then
			table.insert(pod.removals, Hex.encode(removal))
		else
			-- Instances only appear in removals on the server->client path
			Log.warn("Skipped a non-Ref removal in a push frame:", removal)
		end
	end

	return pod
end

local function decodeDetails(frame: { [string]: any }): Types.Project?
	local rootRefs = {}

	for index, hex in frame.rootRefs or {} do
		local ref = Hex.decode(hex)

		if not ref then
			return nil
		end

		rootRefs[index] = ref
	end

	return {
		name = frame.name,
		version = frame.version,
		gameId = frame.gameId,
		placeIds = frame.placeIds or {},
		rootRefs = rootRefs,
	} :: any
end

local function rootRefsFingerprint(frame: { [string]: any }): string
	return table.concat(frame.rootRefs or {}, ",")
end

--------------------------------------------------------------------------------
-- Transport
--------------------------------------------------------------------------------

local WebSocket = {}
WebSocket.__index = WebSocket

--- Feature-detect `CreateWebStreamClient` (recent Studio API — Design §16)
function WebSocket.isSupported(): boolean
	local success, supported = pcall(function()
		return typeof((HttpService :: any).CreateWebStreamClient) == "function"
			and Enum.WebStreamClientType.WebSocket ~= nil
	end)

	return success and supported == true
end

function WebSocket.new(host: string, port: number, https: boolean, scope: string?)
	local self = setmetatable({}, WebSocket)

	self.kind = "WebSocket" :: "WebSocket"
	self.isSubscribed = false
	-- This transport is only constructed after `/hello` answered with
	-- `protocol >= 1` (Transport.detect); Core's version gate keys off this
	-- (Design §5.1: protocol acceptance, semver informational).
	self.protocol = PROTOCOL
	-- Sync scope (Design §7.0) — seeded from the `GET /hello` probe; the WS
	-- server hello re-stamps it whenever it carries the field.
	self.scope = scope
	self.clientId = Util.generateGUID()
	self.placeName = Place.Name
	self.host = host
	self.port = port
	self.https = https

	self.__alive = true
	self.__connected = false
	self.__client = nil
	self.__connections = {}
	self.__queue = {}
	self.__details = nil
	self.__detailsFingerprint = nil
	self.__fatalError = nil
	self.__lastTraffic = 0
	self.__heartbeatThread = nil
	self.__onStatus = nil :: ((status: Types.TransportStatus) -> ())?

	return self
end

function WebSocket:getHttpUrl(endpoint: string?): string
	local protocol = self.https and "https" or "http"
	return `{protocol}://{self.host}:{self.port}/{endpoint or ""}`
end

function WebSocket:getWsUrl(): string
	local protocol = self.https and "wss" or "ws"
	return `{protocol}://{self.host}:{self.port}/ws`
end

--- Register a status listener (connect/reconnect lifecycle incl. the visible
--- backoff countdown). Replaces any previous listener.
function WebSocket:onStatus(callback: (status: Types.TransportStatus) -> ())
	self.__onStatus = callback
end

function WebSocket:__publishStatus(status: Types.TransportStatus)
	local callback = self.__onStatus

	if callback then
		task.spawn(callback, status)
	end
end

--- Interface: resolve project details. Establishes the socket on first call —
--- on this transport the server hello IS the details payload (incl. rootRefs).
function WebSocket:fetchDetails(): Promise.TypedPromise<Types.Project>
	return Promise.new(function(resolve)
		if not self.__connected then
			self:__connect():expect()
		end

		resolve(self.__details)
	end)
end

--- Interface: mark the client as subscribed. The WebSocket connection IS the
--- subscription (`sync` frames start flowing after the hello exchange), so
--- this only flips the flag `Core:stop` checks.
function WebSocket:subscribe(): Promise.Promise
	return Promise.new(function(resolve)
		if not self.__connected then
			self:__connect():expect()
		end

		self.isSubscribed = true
		resolve()
	end)
end

function WebSocket:unsubscribe(): Promise.Promise
	self.isSubscribed = false
	self:destroy()

	return Promise.resolve()
end

--- Interface: fetch a full or subtree snapshot. Protocol v1 serves snapshots
--- over HTTP (`GET /snapshot?ref=<hex>`, JSON — Design §5.2), not the socket.
function WebSocket:getSnapshot(instance: Types.Ref?): Promise.TypedPromise<Types.AddedSnapshot>
	Log.trace("Client request: snapshot")

	local ref = instance and Hex.encode(instance) or Hex.ZERO
	local url = self:getHttpUrl(`snapshot?ref={ref}`)

	return Http.getJson(url):andThen(function(response)
		local snapshot = decodeSnapshot(response:decode())

		if not snapshot then
			return Promise.reject(Error.new(Error.MalformedFrame, "snapshot with invalid refs"))
		end

		return snapshot
	end)
end

--- Interface: next server message, translated to Core's `Types.Message` shape.
--- Blocks while the internal queue is empty and drives reconnection (with the
--- visible countdown) whenever the socket dropped retryably.
function WebSocket:read(): Promise.TypedPromise<Types.Message?>
	return Promise.new(function(resolve, reject)
		while self.__alive do
			local message = table.remove(self.__queue, 1)

			if message then
				return resolve(message)
			end

			if self.__fatalError then
				return reject(self.__fatalError)
			end

			if not self.__connected then
				local success, err = self:__reconnect()

				if not success then
					return reject(err)
				end

				continue
			end

			task.wait(QUEUE_POLL)
		end

		-- Transport destroyed — resolve empty so Core's loop can exit cleanly
		return resolve(nil)
	end)
end

--- Interface: push a Studio -> disk syncback batch (`push` frame, Design §5.3)
function WebSocket:write(changes: Types.Changes): Promise.Promise
	return Promise.new(function(resolve, reject)
		if not self.__connected then
			return reject(Error.new(Error.NotConnected))
		end

		local success, err = self:__send({
			type = "push",
			ops = encodeChanges(changes),
		})

		if success then
			return resolve()
		else
			return reject(err)
		end
	end)
end

--- Interface: open a script in the external editor.
--- Protocol v1 folds editor-opening into the remote-op surface as
--- `open_in_editor` (Design §5.4): a daemon -> plugin request served locally
--- by Remote/Handlers, which replaces the legacy plugin -> daemon `/open`
--- HTTP call on this transport (the LongPoll transport keeps the msgpack
--- `/open` route for Argon-compat daemons). This inverse direction — the
--- plugin asking the daemon to launch an *external* editor when a document
--- opens in Studio — has no route in wsync/1, and dispatching the local
--- `open_in_editor` op here would be circular (Core closes the document on
--- success, so the freshly opened script would vanish). Settle locally with
--- a typed rejection instead of a doomed network call; Core degrades exactly
--- as before: the failure is logged at debug and the document stays open.
function WebSocket:open(_instance: Types.Ref, _line: number): Promise.Promise
	Log.trace("Client request: open (served by the op surface on wsync/1)")

	return Promise.reject(Error.new(Error.NotSupported, "open", self.kind))
end

function WebSocket:destroy()
	if not self.__alive then
		return
	end

	self.__alive = false
	self:__teardown()
	table.clear(self.__queue)
end

--------------------------------------------------------------------------------
-- Internals
--------------------------------------------------------------------------------

function WebSocket:__send(frame: { [string]: any }): (boolean, Error.Error?)
	local client = self.__client

	if not client then
		return false, Error.new(Error.NotConnected)
	end

	local encodeSuccess, encoded = pcall(function()
		return HttpService:JSONEncode(frame)
	end)

	if not encodeSuccess then
		return false, Error.new(Error.Unknown, encoded)
	end

	local sendSuccess, sendErr = pcall(function()
		client:Send(encoded)
	end)

	if not sendSuccess then
		return false, Error.new(Error.Unknown, sendErr)
	end

	return true, nil
end

function WebSocket:__teardown()
	Util.clean(self.__connections)
	table.clear(self.__connections)

	if self.__heartbeatThread then
		task.cancel(self.__heartbeatThread)
		self.__heartbeatThread = nil
	end

	local client = self.__client
	self.__client = nil
	self.__connected = false

	if client then
		-- Close() has been observed to block for a very long time; isolate it
		task.spawn(function()
			pcall(function()
				client:Close()
			end)
		end)
	end

	-- The requester behind any open explicit transaction can no longer send
	-- its transaction_finish — auto-cancel with a visible warning (no-op when
	-- nothing is open, so repeated teardowns during reconnects are free).
	Remote.onTransportDisconnect()
end

function WebSocket:__enqueue(message: Types.Message)
	table.insert(self.__queue, message)
end

--- Force a full re-init through Core: enqueue a typed `Disconnect` and drop
--- the socket. When `fatal` is true the transport never reconnects again.
function WebSocket:__fail(reason: string, fatal: boolean)
	if fatal then
		self.__fatalError = Error.new(Error.HandshakeFailed, reason)
	end

	self:__enqueue({
		Disconnect = {
			message = reason,
			fatal = fatal,
		},
	} :: any)

	self:__teardown()
end

function WebSocket:__handleFrame(frame: { [string]: any }, resolveHello: ((frame: any) -> ())?)
	local kind = frame.type

	self.__lastTraffic = os.clock()

	if kind == "hello" then
		if resolveHello then
			resolveHello(frame)
		else
			Log.debug("Received an unexpected hello frame, ignoring")
		end
	elseif kind == "ping" then
		self:__send({ type = "pong" })
	elseif kind == "pong" then
		-- Liveness already stamped above
	elseif kind == "sync" then
		local changes = decodeChanges(frame)

		if changes then
			self:__enqueue({ SyncChanges = changes } :: any)
		else
			Log.error(Error.new(Error.MalformedFrame, "sync with invalid refs"))
			self:__fail("Received a malformed sync frame — resync required", false)
		end
	elseif kind == "details" then
		local details = decodeDetails(frame)

		if details then
			self.__details = details
			self.__detailsFingerprint = rootRefsFingerprint(frame)
			self:__enqueue({ SyncDetails = details } :: any)
		else
			Log.error(Error.new(Error.MalformedFrame, "details with invalid refs"))
			self:__fail("Received a malformed details frame — resync required", false)
		end
	elseif kind == "execute" then
		self:__enqueue({ ExecuteCode = { code = frame.code } } :: any)
	elseif kind == "push-result" then
		-- TODO(phase-4): feed conflicts into the divergence/conflict UI once
		-- the daemon-side conflict engine ships. For now surface failures.
		if frame.ok == false or (frame.errors and #frame.errors > 0) then
			Log.warn("Server did not fully apply a push:", frame)
		else
			Log.trace("Push applied:", frame.applied)
		end
	elseif kind == "request" then
		self:__handleRequest(frame)
	elseif kind == "shutdown" then
		local reason = tostring(frame.reason or "Server requested a disconnect")
		local retryable = frame.retryable ~= false

		Log.info(`Server shutdown [{tostring(frame.code)}]: {reason} (retryable: {tostring(retryable)})`)

		if retryable then
			-- Drop the socket; `read` drives the backoff reconnect loop
			self:__teardown()
			self:__publishStatus({ kind = "disconnected" })
		else
			self:__fail(reason, true)
		end
	elseif kind == "event" then
		local topic = frame.topic

		if topic == "choice-made" or topic == "choice-needed" then
			-- Divergence-decision lifecycle (Design §7.2/§7.3): surfaced to
			-- Core through the message queue — the connect-time wait loop
			-- consumes `choice-made`; outside a wait Core drops them quietly.
			self:__enqueue({
				DivergenceEvent = {
					topic = topic,
					choiceId = if type(frame.choiceId) == "string" then frame.choiceId else nil,
					choice = if type(frame.choice) == "string" then frame.choice else nil,
				},
			} :: any)
		elseif topic == "disk-review" then
			-- Studio-first disk review (Design §7.0, code scope): the daemon's
			-- passive review-count push after auto-applying Studio → disk.
			-- Core feeds it into the Connected-page indicator; nothing waits
			-- on it.
			self:__enqueue({
				DivergenceEvent = {
					topic = topic,
					reviewId = if type(frame.reviewId) == "string" then frame.reviewId else nil,
					total = if type(frame.total) == "number" then frame.total else nil,
					diskOnly = if type(frame.diskOnly) == "number" then frame.diskOnly else nil,
					differs = if type(frame.differs) == "number" then frame.differs else nil,
				},
			} :: any)
		else
			-- Other app/watch feed topics — not consumed by the plugin
			Log.trace("Ignoring event frame:", frame)
		end
	else
		Log.debug("Received an unknown frame type:", kind)
	end
end

--- Remote-op dispatch (Design §5.4): route the `request` frame through
--- `Remote.dispatch` and always answer, even on failure. Most ops run
--- detached so a slow handler (a wide `query` can legally spend ~2 s) never
--- stalls frame handling or the heartbeat; the daemon correlates by
--- `request_id`.
---
--- Exception (pinned wave-4 contract): `transaction_begin` and
--- `transaction_finish` must stay ordered with the sync frames they bracket.
--- Sync frames queue for Core while requests would otherwise execute
--- immediately — a finish jumping the queue would close the recording before
--- the bracketed frames applied. Both ops are therefore enqueued as
--- `DispatchRequest` messages and executed by Core in queue order.
function WebSocket:__handleRequest(frame: { [string]: any })
	local requestId = frame.request_id

	if type(requestId) ~= "number" then
		Log.debug("Dropping a request frame without a numeric request_id")
		return
	end

	if frame.op == "transaction_begin" or frame.op == "transaction_finish" then
		self:__enqueue({ DispatchRequest = frame } :: any)
		return
	end

	task.spawn(function()
		self:performRequest(frame)
	end)
end

--- Execute one `request` frame and answer it. Public so Core can dispatch
--- queue-ordered requests (`DispatchRequest` messages) at the right moment.
function WebSocket:performRequest(frame: { [string]: any })
	local requestId = frame.request_id

	-- Protected end to end: dispatch never raises (unknown ops keep the
	-- typed UNKNOWN_OP envelope, handler crashes become PLUGIN_ERROR
	-- with a truncated message).
	local result = Remote.dispatch(frame.op, frame.args)

	local response: { [string]: any } = {
		type = "response",
		request_id = requestId,
		ok = result.ok,
		meta = result.meta,
	}

	if result.ok then
		response.value = result.value
	else
		response.error = result.error
	end

	local sent = self:__send(response)

	if not sent and self.__client ~= nil then
		-- JSON encoding of the value is the only expected failure here;
		-- never leave a request unanswered while the socket is up. The
		-- mutation/audit stamp is JSON-safe (op names and paths) and must
		-- survive the fallback so the daemon's writes.log stays complete.
		self:__send({
			type = "response",
			request_id = requestId,
			ok = false,
			error = { code = "PLUGIN_ERROR", message = "response encoding failed" },
			meta = {
				op = result.meta.op,
				durationMs = result.meta.durationMs,
				protocol = PROTOCOL,
				mutation = result.meta.mutation,
				audit = result.meta.audit,
			},
		})
	end
end

--- Inject a message into the read queue (Core-side use: the divergence
--- backstop poll synthesizes a `choice-vanished` DivergenceEvent so the
--- blocked wait loop wakes even when no server frame arrives).
function WebSocket:push(message: Types.Message)
	self:__enqueue(message)
end

--- Open the socket and run the hello handshake. Resolves once the server
--- hello arrived; rejects with a transport Error otherwise.
function WebSocket:__connect(): Promise.Promise
	return Promise.new(function(resolve, reject)
		self:__teardown()

		if not self.__alive then
			return reject(Error.new(Error.NotConnected))
		end

		self:__publishStatus({ kind = "connecting" })

		local url = self:getWsUrl()

		Log.trace("Opening WebSocket:", url)

		local client
		local createSuccess, createErr = pcall(function()
			-- CreateWebStreamClient takes a config table, not a bare URL
			client = HttpService:CreateWebStreamClient(Enum.WebStreamClientType.WebSocket, { Url = url })
		end)

		if not createSuccess or not client then
			return reject(Error.new(Error.OpenFailed, createErr or "unknown"))
		end

		self.__client = client

		local opened = false
		local closed = false
		local openError = nil
		local helloFrame = nil

		local function resolveHello(frame)
			helloFrame = frame
		end

		table.insert(
			self.__connections,
			client.Opened:Connect(function()
				opened = true
			end)
		)

		table.insert(
			self.__connections,
			client.Error:Connect(function(code, message)
				openError = `{tostring(code)}: {tostring(message)}`
				closed = true
			end)
		)

		table.insert(
			self.__connections,
			client.Closed:Connect(function()
				closed = true

				if self.__client == client and self.__connected then
					Log.debug("WebSocket closed by the server")
					self.__connected = false
					self:__publishStatus({ kind = "disconnected" })
				end
			end)
		)

		table.insert(
			self.__connections,
			client.MessageReceived:Connect(function(payload)
				local text = if typeof(payload) == "buffer" then buffer.tostring(payload) else payload

				if type(text) ~= "string" or text == "" then
					return
				end

				local decodeSuccess, frame = pcall(function()
					return HttpService:JSONDecode(text)
				end)

				if not decodeSuccess or type(frame) ~= "table" then
					Log.warn(Error.new(Error.MalformedFrame, text:sub(1, 128)))
					return
				end

				self:__handleFrame(frame, resolveHello)
			end)
		)

		-- The client connects asynchronously; sending while it is still in the
		-- Connecting state can silently drop the frame. Wait for Opened.
		opened = client.ConnectionState == Enum.WebStreamClientState.Open

		local openDeadline = os.clock() + OPEN_TIMEOUT

		while not opened and not closed and self.__alive and os.clock() < openDeadline do
			task.wait(QUEUE_POLL)
		end

		if not opened or closed then
			self:__teardown()
			return reject(Error.new(Error.OpenFailed, openError or "connection refused or timed out"))
		end

		-- Client hello (Design §5.3)
		local sendSuccess, sendErr = self:__send({
			type = "hello",
			clientId = self.clientId,
			role = "plugin",
			protocol = PROTOCOL,
			name = self.placeName,
		})

		if not sendSuccess then
			self:__teardown()
			return reject(sendErr)
		end

		local helloDeadline = os.clock() + HANDSHAKE_TIMEOUT

		while not helloFrame and not closed and not self.__fatalError and self.__alive and os.clock() < helloDeadline do
			task.wait(QUEUE_POLL)
		end

		if self.__fatalError then
			-- The server answered our hello with a non-retryable shutdown
			return reject(self.__fatalError)
		end

		if not helloFrame then
			self:__teardown()
			return reject(
				Error.new(Error.HandshakeFailed, closed and (openError or "connection closed") or "timed out")
			)
		end

		local details = decodeDetails(helloFrame)

		if not details then
			self:__teardown()
			return reject(Error.new(Error.HandshakeFailed, "hello carried invalid project details"))
		end

		local fingerprint = rootRefsFingerprint(helloFrame)

		if self.__detailsFingerprint and self.__detailsFingerprint ~= fingerprint then
			-- The daemon restarted or switched projects while we were away;
			-- our buffer Refs no longer mean anything. Force a full re-init.
			self:__fail("Server session changed while disconnected — reconnect to resync", false)
			return reject(Error.new(Error.HandshakeFailed, "server session changed"))
		end

		-- The WS server hello carries the sync scope too (Design §7.0, pinned:
		-- absent = full); prefer it over the probe-time value when present.
		if helloFrame.scope ~= nil then
			self.scope = helloFrame.scope
		end

		self.__details = details
		self.__detailsFingerprint = fingerprint
		self.__lastTraffic = os.clock()
		self.__connected = true

		self:__startHeartbeat()
		self:__publishStatus({ kind = "connected" })

		Log.trace("WebSocket connected, server:", details.name)

		return resolve()
	end)
end

function WebSocket:__startHeartbeat()
	if self.__heartbeatThread then
		task.cancel(self.__heartbeatThread)
	end

	-- ~2 s ping/pong; any inbound traffic counts as liveness. A silent socket
	-- past HEARTBEAT_TIMEOUT is force-closed so `read` can reconnect.
	self.__heartbeatThread = task.spawn(function()
		while self.__alive and self.__connected do
			task.wait(HEARTBEAT_INTERVAL)

			if not self.__connected then
				break
			end

			self:__send({ type = "ping" })

			if os.clock() - self.__lastTraffic > HEARTBEAT_TIMEOUT then
				Log.debug("Heartbeat timed out, forcing reconnect")

				-- Clear the field first so `__teardown` does not try to
				-- cancel the thread it is running on
				self.__heartbeatThread = nil

				self:__teardown()
				self:__publishStatus({ kind = "disconnected" })

				break
			end
		end
	end)
end

--- Reconnect with exponential backoff, publishing a per-second countdown.
--- Returns `true` once connected, or `false, err` when the transport died
--- (destroyed or fatal shutdown) while waiting.
function WebSocket:__reconnect(): (boolean, Error.Error?)
	local backoff = BACKOFF_START
	local attempt = 1

	while self.__alive and not self.__fatalError do
		-- Visible countdown state (Design §9.4)
		local remaining = backoff

		while remaining > 0 and self.__alive and not self.__fatalError do
			self:__publishStatus({
				kind = "reconnecting",
				attempt = attempt,
				secondsLeft = remaining,
			})

			task.wait(math.min(remaining, 1))
			remaining -= 1
		end

		if not self.__alive or self.__fatalError then
			break
		end

		local success, err = pcall(function()
			self:__connect():expect()
		end)

		if success then
			return true
		end

		-- A queued Disconnect (fatal shutdown or session change) must reach
		-- Core before we keep spinning
		if self.__fatalError or #self.__queue > 0 then
			return true
		end

		Log.debug(`Reconnect attempt {attempt} failed:`, err)

		attempt += 1
		backoff = math.min(backoff * 2, BACKOFF_MAX)
	end

	if self.__fatalError then
		return false, self.__fatalError
	end

	return false, Error.new(Error.NotConnected)
end

return WebSocket
