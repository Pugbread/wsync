--!strict
-- Playtest runtime agent (playtest.json; pinned wave-6 contracts). Studio
-- clones the plugin into every playtest DataModel; init.server.luau routes
-- those clones here instead of booting the edit UI. The agent:
--
--   * authenticates itself: the private generation token and job id arrive
--     through StudioTestService test args (planted by the edit coordinator at
--     launch); without them the bridge stays disabled, so a manually started
--     playtest — or a stale generation — can never serve a job;
--   * talks only to the edit-mode plugin copy over PluginConnectionService
--     (never localhost), wrapping every message in the authenticated,
--     sequenced envelope of Playtest/Protocol;
--   * serves the request ops behind `playtest_exec` / `logs` / `ui` /
--     `input` / `capture` / `request`, executing sources through temporary
--     Script/LocalScript runners (`game` identity) or plugin-context runners
--     (`plugin` identity) and encoding every value with Remote/Codec;
--   * hosts playscript runners for `playtest run`: the injected `playtest.*`
--     namespace (args/mode/context/jobId/emit/log/done/fail/signal/await/
--     awaitClients) bridges into this module over a BindableFunction, with
--     the ~20/s+burst rate limit, the 64 KiB payload cap, accurate `dropped`
--     accounting, the 1 MiB result-envelope cap, and ~2 s heartbeats.
--
-- Everything the agent installs is temporary and lives inside the disposable
-- playtest DataModel: nothing here ever reaches the edit DataModel or the
-- daemon, so runtime changes cannot sync (the edit copy's watcher only ever
-- observes edit-DataModel instances).

local RunService = game:GetService("RunService")
local HttpService = game:GetService("HttpService")
local LogService = game:GetService("LogService")
local Players = game:GetService("Players")
local ScriptContext = game:GetService("ScriptContext")
local ScriptEditorService = game:GetService("ScriptEditorService")
local ServerScriptService = game:GetService("ServerScriptService")
local StudioTestService = game:GetService("StudioTestService")
local UserInputService = game:GetService("UserInputService")

local WSync = script:FindFirstAncestor("WSync")

local Log = require(WSync.Log)
local Base64 = require(WSync.Lib.Base64)
local Codec = require(WSync.Remote.Codec)
local Paths = require(WSync.Remote.Paths)
local BufferHash = require(WSync.Remote.BufferHash)
local LogBuffer = require(WSync.Remote.LogBuffer)
local Renderer = require(WSync.Remote.Capture)

local Protocol = require(script.Parent.Protocol)
local RateLimiter = require(script.Parent.RateLimiter)
local InputActions = require(script.Parent.InputActions)

local LIMITS = Protocol.LIMITS

local Runtime = {}

type Any = any

local LOG_RANK: { [string]: number } = { info = 0, warn = 1, error = 2 }
local MAX_LOG_MESSAGE_BYTES = 8 * 1024
local MAX_ERROR_BYTES = 64 * 1024
local MAX_TRACEBACK_BYTES = 256 * 1024
local MAX_SEEN_REQUEST_IDS = 512
local MAX_PENDING_IMPORTANT = 32
local CAPTURE_TTL_SECONDS = 120
local CAPTURE_MAX_SESSIONS = 2

local function sha256Hex(value: string): string
	return BufferHash.hex(buffer.fromstring(value))
end

local function capText(value: string, limit: number): string
	if #value > limit then
		return string.sub(value, 1, limit) .. "<truncated>"
	end

	return value
end

local function finiteNumber(value: Any, fallback: number): number
	local number = tonumber(value)

	if number == nil or number ~= number or number == math.huge or number == -math.huge then
		return fallback
	end

	return number
end

--------------------------------------------------------------------------------
-- Agent state
--------------------------------------------------------------------------------

type Runner = {
	kind: "main" | "client",
	identity: "game" | "plugin",
	logs: string,
	installState: "installing" | "installed" | "failed",
	bootError: string?,
	holder: Instance?,
	scriptInstance: Instance?,
	finished: boolean,
	destroyed: boolean,
	signalQueues: { [string]: { Any } },
	queuedSignalCount: number,
	signalWaiters: { [string]: { BindableEvent } },
	clientWaiters: { { minimum: number, event: BindableEvent } },
	lastSignalSequence: number,
}

type ResultSession = {
	bytes: string,
	createdAt: number,
}

type CaptureSession = {
	pixels: buffer,
	width: number,
	height: number,
	sha256: string,
	createdAt: number,
}

type Agent = {
	runtimeId: string,
	jobId: string,
	token: string,
	role: "server" | "client",
	startedAt: number,
	connections: { [Any]: boolean },
	activeConnection: Any?,
	inboundSeqs: { [Any]: number },
	outboundSeq: number,
	seenRequestIds: { [string]: boolean },
	seenRequestOrder: { string },
	bucket: RateLimiter.Bucket,
	pendingImportant: { { [string]: Any } },
	runners: { [string]: Runner },
	results: { [string]: ResultSession },
	resultOrder: { string },
	resultCounter: number,
	captures: { [string]: CaptureSession },
	captureCounter: number,
	rendering: boolean,
	signalBacklog: { { name: string, payload: Any, seq: number, createdAt: number } },
	signalSequence: number,
	clientCount: number,
	cancelled: boolean,
	heartbeatStarted: boolean,
	execCounter: number,
}

--------------------------------------------------------------------------------
-- Outbound frames
--------------------------------------------------------------------------------

local function connectionOf(agent: Agent): Any?
	local active = agent.activeConnection

	if active ~= nil and active.Connected then
		return active
	end

	agent.activeConnection = nil

	for connection in agent.connections do
		if connection.Connected then
			agent.activeConnection = connection

			return connection
		end
	end

	return nil
end

--- Send one authenticated envelope frame. Returns whether it went out.
local function send(agent: Agent, body: { [string]: Any }): boolean
	local connection = connectionOf(agent)

	if connection == nil then
		return false
	end

	local okBody, bodyJson = pcall(function()
		return HttpService:JSONEncode(body)
	end)

	if not okBody then
		return false
	end

	agent.outboundSeq += 1

	local seq = agent.outboundSeq
	local envelope = {
		kind = Protocol.KIND,
		jobId = agent.jobId,
		runtimeId = agent.runtimeId,
		seq = seq,
		bodyJson = bodyJson,
		auth = Protocol.frameAuth(
			sha256Hex,
			agent.token,
			Protocol.DIRECTION_RUNTIME_TO_EDIT,
			agent.runtimeId,
			seq,
			bodyJson
		),
	}

	local okEnvelope, encoded = pcall(function()
		return HttpService:JSONEncode(envelope)
	end)

	if not okEnvelope then
		return false
	end

	return (pcall(function()
		connection:SendMessage(encoded)
	end))
end

--- Send a lifecycle frame that must not be lost: on failure it is queued
--- (bounded) and retried by the heartbeat loop.
local function sendImportant(agent: Agent, body: { [string]: Any }): boolean
	if send(agent, body) then
		return true
	end

	table.insert(agent.pendingImportant, body)

	while #agent.pendingImportant > MAX_PENDING_IMPORTANT do
		table.remove(agent.pendingImportant, 1)
	end

	return false
end

local function flushImportant(agent: Agent)
	while #agent.pendingImportant > 0 do
		if not send(agent, agent.pendingImportant[1]) then
			break
		end

		table.remove(agent.pendingImportant, 1)
	end
end

--- Flush the accumulated dropped count as a `dropped` frame ("dropped
--- records never silent"). The count survives a failed send.
local function flushDropped(agent: Agent)
	local count = RateLimiter.takeDropped(agent.bucket)

	if count <= 0 then
		return
	end

	if not send(agent, { type = "dropped", count = count }) then
		RateLimiter.restoreDropped(agent.bucket, count)
	end
end

--- Rate-limited telemetry frame: `data` must already be JSON-safe (codec
--- output). Oversized payloads and rate-limited sends are counted, never
--- silently discarded.
local function sendLimited(agent: Agent, body: { [string]: Any }): boolean
	if not RateLimiter.take(agent.bucket, os.clock()) then
		RateLimiter.drop(agent.bucket, 1)

		return false
	end

	local okBody, bodyJson = pcall(function()
		return HttpService:JSONEncode(body)
	end)

	if not okBody or #bodyJson > LIMITS.eventBytes then
		RateLimiter.drop(agent.bucket, 1)

		return false
	end

	flushDropped(agent)

	if not send(agent, body) then
		RateLimiter.drop(agent.bucket, 1)

		return false
	end

	return true
end

--------------------------------------------------------------------------------
-- Value encoding (Remote/Codec — the same codec as the edit-mode op surface)
--------------------------------------------------------------------------------

--- Codec-encode a value from a runner, protected: budget violations become
--- descriptive failures instead of crashes.
local function encodeValue(value: Any): (boolean, Any)
	return pcall(function()
		return Codec.encode(value)
	end)
end

--- Build the result envelope for a packed `xpcall` result
--- (`{ok, valueOrFailure, ...}` from table.pack). Multi-value returns encode
--- as an array with an explicit count so `return nil` stays distinguishable.
local function buildEnvelope(packed: { n: number, [number]: Any }): { [string]: Any }
	if packed[1] == true then
		local count = packed.n - 1
		local envelope: { [string]: Any } = { ok = true, count = count }

		if count == 1 then
			local okEncode, encoded = encodeValue(packed[2])

			if not okEncode then
				return { ok = false, error = capText(`result encoding failed: {tostring(encoded)}`, MAX_ERROR_BYTES) }
			end

			envelope.value = encoded
		elseif count > 1 then
			local values = table.create(count)

			for index = 1, count do
				values[index] = packed[index + 1]
			end

			local okEncode, encoded = encodeValue(values)

			if not okEncode then
				return { ok = false, error = capText(`result encoding failed: {tostring(encoded)}`, MAX_ERROR_BYTES) }
			end

			envelope.value = encoded
		end

		return envelope
	end

	local failure = packed[2]

	if type(failure) == "table" and failure.message ~= nil then
		return {
			ok = false,
			error = capText(tostring(failure.message), MAX_ERROR_BYTES),
			traceback = capText(tostring(failure.traceback or failure.message), MAX_TRACEBACK_BYTES),
		}
	end

	return {
		ok = false,
		error = capText(tostring(failure), MAX_ERROR_BYTES),
	}
end

--- Enforce the pinned 1 MiB envelope cap: an oversized success value is
--- replaced by `{truncated = true, bytes = N}` (playtest.json), an oversized
--- failure envelope by a bounded marker. Returns the envelope and its JSON.
local function capEnvelope(envelope: { [string]: Any }): ({ [string]: Any }, string)
	local encoded = HttpService:JSONEncode(envelope)

	if #encoded <= LIMITS.resultBytes then
		return envelope, encoded
	end

	local originalBytes = #encoded

	if envelope.ok == true then
		envelope = {
			ok = true,
			count = envelope.count,
			value = { truncated = true, bytes = originalBytes },
			truncated = true,
		}
	else
		envelope = {
			ok = false,
			error = "result envelope exceeded 1 MiB",
			truncated = true,
			bytes = originalBytes,
		}
	end

	return envelope, HttpService:JSONEncode(envelope)
end

--------------------------------------------------------------------------------
-- Result sessions (large envelopes are pinned and fetched in bounded parts)
--------------------------------------------------------------------------------

local function cleanupResults(agent: Agent)
	local now = os.clock()
	local kept: { string } = {}

	for _, resultId in agent.resultOrder do
		local session = agent.results[resultId]

		if session ~= nil and now - session.createdAt < LIMITS.resultTtlSeconds then
			table.insert(kept, resultId)
		else
			agent.results[resultId] = nil
		end
	end

	agent.resultOrder = kept

	while #agent.resultOrder > LIMITS.resultMaxSessions do
		local evicted = table.remove(agent.resultOrder, 1) :: string

		agent.results[evicted] = nil
	end
end

--- Pin an encoded envelope and return its `{id, bytes, sha256}` reference.
local function storeResult(agent: Agent, encoded: string): { [string]: Any }
	agent.resultCounter += 1

	local resultId = `{agent.resultCounter}-{HttpService:GenerateGUID(false)}`

	agent.results[resultId] = { bytes = encoded, createdAt = os.clock() }
	table.insert(agent.resultOrder, resultId)
	cleanupResults(agent)

	return {
		id = resultId,
		bytes = #encoded,
		sha256 = sha256Hex(encoded),
	}
end

--- The response payload carrying one result envelope: inline when small,
--- a pinned reference otherwise.
local function packageEnvelope(agent: Agent, envelope: { [string]: Any }): { [string]: Any }
	local capped, encoded = capEnvelope(envelope)

	if #encoded <= LIMITS.inlineResultBytes then
		return { inline = capped }
	end

	return { ref = storeResult(agent, encoded) }
end

--------------------------------------------------------------------------------
-- Temporary script runners (the shared exec/playscript mechanism)
--------------------------------------------------------------------------------

--- The parent and script class for a runner: plugin identity runs inside the
--- plugin sandbox (never the game tree); game identity uses a temporary
--- Script in ServerScriptService or a LocalScript in PlayerScripts
--- (playtest.json identity rules).
local function runnerParent(identity: string, role: string): (Instance?, string)
	if identity == "plugin" then
		return script, "Script"
	end

	if role == "server" then
		return ServerScriptService, "Script"
	end

	local player = Players.LocalPlayer

	if player == nil then
		return nil, "LocalScript"
	end

	local playerScripts = player:FindFirstChildOfClass("PlayerScripts") or player:WaitForChild("PlayerScripts", 5)

	return playerScripts, "LocalScript"
end

--- Install and enable one temporary runner script. Returns the holder Folder
--- (already parented) or raises with a reason. `body` is the full wrapper
--- source; `children` are pre-parented into the holder before the script
--- starts.
local function installRunnerScript(
	holderName: string,
	identity: string,
	role: string,
	wrapper: string,
	children: { Instance }
): (Instance, Instance)
	local parent, className = runnerParent(identity, role)

	if parent == nil then
		error("runner parent is unavailable (LocalPlayer is not ready)", 0)
	end

	local holder = Instance.new("Folder")
	holder.Name = holderName

	for _, child in children do
		child.Parent = holder
	end

	local runnerScript = Instance.new(className) :: Any
	runnerScript.Name = "Runner"
	runnerScript.Disabled = true

	if identity == "plugin" then
		local okContext, contextError = pcall(function()
			runnerScript.RunContext = Enum.RunContext.Plugin
		end)

		if not okContext then
			holder:Destroy()
			error(`plugin identity is unavailable: {tostring(contextError)}`, 0)
		end
	end

	local okSource, sourceError = pcall(function()
		runnerScript.Source = wrapper
	end)

	if not okSource then
		-- Some Studio builds refuse direct Source writes here; the editor
		-- path needs the script parented first.
		holder.Parent = parent
		runnerScript.Parent = holder

		okSource, sourceError = pcall(function()
			ScriptEditorService:UpdateSourceAsync(runnerScript, function()
				return wrapper
			end)
		end)
	end

	if not okSource then
		holder:Destroy()
		error(`unable to install the runner script: {tostring(sourceError)}`, 0)
	end

	if holder.Parent == nil then
		holder.Parent = parent
	end

	if runnerScript.Parent == nil then
		runnerScript.Parent = holder
	end

	return holder, runnerScript :: Instance
end

--- Watch for compile errors (LogService names the runner) and runtime errors
--- that escape the wrapper (ScriptContext). Returns a disconnect closure.
local function watchRunnerErrors(
	holderName: string,
	runnerScript: Instance,
	onError: (message: string, detail: string) -> ()
): () -> ()
	local needle = holderName .. ".Runner"

	local logConnection = LogService.MessageOut:Connect(function(message: string, messageType: Enum.MessageType)
		if messageType == Enum.MessageType.MessageError and string.find(tostring(message), needle, 1, true) ~= nil then
			onError(`compile: {tostring(message)}`, tostring(message))
		end
	end)

	local errorConnection = ScriptContext.Error:Connect(function(message: string, stack: string, errorSource: Instance?)
		if
			errorSource == runnerScript
			or string.find(tostring(message), holderName, 1, true) ~= nil
			or string.find(tostring(stack), holderName, 1, true) ~= nil
		then
			onError(tostring(message), tostring(stack))
		end
	end)

	return function()
		logConnection:Disconnect()
		errorConnection:Disconnect()
	end
end

local EXEC_WRAPPER_HEAD = table.concat({
	'local __event = script.Parent:WaitForChild("Result")',
	"local function __wsync_run()",
}, "\n")

local EXEC_WRAPPER_TAIL = table.concat({
	"end",
	"local __packed = table.pack(xpcall(__wsync_run, function(errorValue)",
	"\treturn { message = tostring(errorValue), traceback = debug.traceback(tostring(errorValue), 2) }",
	"end))",
	"__event:Fire(__packed)",
}, "\n")

--- Execute one source through a temporary runner and return the packed
--- xpcall result. Raises on installation failure or timeout. Plugin-identity
--- scripts cannot be interrupted (cooperative timeout): the runner is
--- disabled and destroyed either way.
local function executeSource(
	agent: Agent,
	source: string,
	timeoutSec: number,
	identity: string
): { n: number, [number]: Any }
	agent.execCounter += 1

	local holderName = `__WSyncExec_{agent.runtimeId}_{agent.execCounter}`
	local resultEvent = Instance.new("BindableEvent")
	resultEvent.Name = "Result"

	local doneEvent = Instance.new("BindableEvent")
	local finished = false
	local packedResult: { n: number, [number]: Any }? = nil

	local function finish(packed: Any)
		if finished then
			return
		end

		finished = true
		packedResult = packed
		doneEvent:Fire()
	end

	local wrapper = EXEC_WRAPPER_HEAD .. "\n" .. source .. "\n" .. EXEC_WRAPPER_TAIL

	local holder, runnerScript = installRunnerScript(holderName, identity, agent.role, wrapper, { resultEvent })

	local resultConnection = resultEvent.Event:Connect(finish)
	local unwatch = watchRunnerErrors(holderName, runnerScript, function(message, detail)
		finish(table.pack(false, { message = message, traceback = detail }))
	end);

	(runnerScript :: Any).Disabled = false

	task.delay(timeoutSec, function()
		if not finished then
			finished = true
			doneEvent:Fire()
		end
	end)

	if not finished then
		doneEvent.Event:Wait()
	end

	resultConnection:Disconnect()
	unwatch()
	doneEvent:Destroy()

	pcall(function()
		(runnerScript :: Any).Disabled = true
	end)
	pcall(function()
		holder:Destroy()
	end)

	local packed = packedResult

	if packed == nil then
		error(`execution timed out after {timeoutSec}s`, 0)
	end

	return packed
end

--------------------------------------------------------------------------------
-- Playscript runners
--------------------------------------------------------------------------------

local function destroyRunner(runner: Runner)
	if runner.destroyed then
		return
	end

	runner.destroyed = true

	local signalWaiters = runner.signalWaiters
	runner.signalWaiters = {}

	for _, waiters in signalWaiters do
		local claimed = table.clone(waiters)

		table.clear(waiters)

		for _, waiter in claimed do
			waiter:Fire(false)
		end
	end

	local clientWaiters = runner.clientWaiters
	runner.clientWaiters = {}

	for _, waiter in clientWaiters do
		waiter.event:Fire(false)
	end

	runner.signalQueues = {}
	runner.queuedSignalCount = 0

	pcall(function()
		(runner.scriptInstance :: Any).Disabled = true
	end)
	pcall(function()
		(runner.holder :: Instance):Destroy()
	end)
end

--- Complete a runner exactly once with a pre-built envelope: cap/package it
--- and push the lifecycle frame — `complete` for the main runner and for any
--- explicit done/fail (which finish the whole run from any context, first
--- wins), or `clientResult` for a client script's own return.
local function finishRunnerWithEnvelope(
	agent: Agent,
	runner: Runner,
	envelope: { [string]: Any },
	explicitCompletion: boolean?
): boolean
	if runner.finished then
		return false
	end

	runner.finished = true
	flushDropped(agent)

	local package = packageEnvelope(agent, envelope)
	local frameType = if runner.kind == "main" or explicitCompletion == true then "complete" else "clientResult"
	local body: { [string]: Any } = { type = frameType, runner = runner.kind }

	if package.inline ~= nil then
		body.inline = package.inline
	else
		body.ref = package.ref
	end

	sendImportant(agent, body)

	task.defer(function()
		destroyRunner(runner)
	end)

	return true
end

--- Complete a runner from a raw success value or failure message.
local function finishRunner(
	agent: Agent,
	runner: Runner,
	ok: boolean,
	value: Any,
	traceback: string?,
	explicitCompletion: boolean?
): boolean
	local envelope: { [string]: Any }

	if ok then
		envelope = buildEnvelope(table.pack(true, value))
	else
		envelope = {
			ok = false,
			error = capText(tostring(value), MAX_ERROR_BYTES),
			traceback = capText(tostring(traceback or value), MAX_TRACEBACK_BYTES),
		}
	end

	return finishRunnerWithEnvelope(agent, runner, envelope, explicitCompletion)
end

local function queueSignal(agent: Agent, runner: Runner, name: string, payload: Any): boolean
	if runner.queuedSignalCount >= LIMITS.signalQueueMax then
		RateLimiter.drop(agent.bucket, 1)

		return false
	end

	local queue = runner.signalQueues[name]

	if queue == nil then
		queue = {}
		runner.signalQueues[name] = queue
	end

	table.insert(queue, payload)
	runner.queuedSignalCount += 1

	return true
end

local function takeQueuedSignal(runner: Runner, name: string): (boolean, Any)
	local queue = runner.signalQueues[name]

	if queue == nil or #queue == 0 then
		return false, nil
	end

	local payload = table.remove(queue, 1)

	runner.queuedSignalCount = math.max(0, runner.queuedSignalCount - 1)

	if #queue == 0 then
		runner.signalQueues[name] = nil
	end

	return true, payload
end

local function validateSignalName(name: Any, label: string): string
	local value = tostring(name or "")

	if value == "" then
		error(`{label} name must not be empty`, 0)
	end

	if #value > LIMITS.signalNameBytes then
		error(`{label} name exceeds {LIMITS.signalNameBytes} bytes`, 0)
	end

	return value
end

local function awaitSignal(runner: Runner, name: Any, timeoutSeconds: Any): Any
	local signalName = validateSignalName(name, "playtest.await")
	local hadQueued, queuedPayload = takeQueuedSignal(runner, signalName)

	if hadQueued then
		return queuedPayload
	end

	local timeout = math.clamp(finiteNumber(timeoutSeconds, 30), 0, 3600)

	if timeout <= 0 then
		return nil
	end

	local waiter = Instance.new("BindableEvent")
	local waiters = runner.signalWaiters[signalName]

	if waiters == nil then
		waiters = {}
		runner.signalWaiters[signalName] = waiters
	end

	table.insert(waiters, waiter)

	task.delay(timeout, function()
		local index = table.find(waiters, waiter)

		if index == nil then
			return
		end

		table.remove(waiters, index)

		if #waiters == 0 and runner.signalWaiters[signalName] == waiters then
			runner.signalWaiters[signalName] = nil
		end

		waiter:Fire(false)
	end)

	local received, payload = waiter.Event:Wait()

	local index = table.find(waiters, waiter)

	if index ~= nil then
		table.remove(waiters, index)
	end

	if #waiters == 0 and runner.signalWaiters[signalName] == waiters then
		runner.signalWaiters[signalName] = nil
	end

	waiter:Destroy()

	return if received then payload else nil
end

local function awaitClients(agent: Agent, runner: Runner, minimum: Any, timeoutSeconds: Any): Any
	if agent.role ~= "server" then
		error("playtest.awaitClients is only available in the server context", 0)
	end

	local needed = math.clamp(math.floor(finiteNumber(minimum, 1)), 1, 8)

	if agent.clientCount >= needed then
		return agent.clientCount
	end

	local timeout = math.clamp(finiteNumber(timeoutSeconds, 30), 0, 3600)

	if timeout <= 0 then
		return nil
	end

	local event = Instance.new("BindableEvent")
	local waiter = { minimum = needed, event = event }

	table.insert(runner.clientWaiters, waiter)

	task.delay(timeout, function()
		local index = table.find(runner.clientWaiters, waiter)

		if index == nil then
			return
		end

		table.remove(runner.clientWaiters, index)
		event:Fire(false)
	end)

	local ready, count = event.Event:Wait()

	local index = table.find(runner.clientWaiters, waiter)

	if index ~= nil then
		table.remove(runner.clientWaiters, index)
	end

	event:Destroy()

	return if ready then count else nil
end

--- The BindableFunction bridge behind the injected `playtest.*` namespace.
local function bridgeCall(agent: Agent, runner: Runner, action: string, ...: Any): Any
	if runner.finished or runner.destroyed then
		if action ~= "__return" then
			return nil
		end
	end

	local arguments = table.pack(...)

	if action == "emit" then
		local okEncode, encoded = encodeValue(arguments[1])

		if not okEncode then
			RateLimiter.drop(agent.bucket, 1)

			return false
		end

		return sendLimited(agent, { type = "event", data = encoded })
	elseif action == "log" then
		return sendLimited(agent, {
			type = "log",
			level = "info",
			message = capText(tostring(arguments[1]), MAX_LOG_MESSAGE_BYTES),
		})
	elseif action == "signal" then
		local name = validateSignalName(arguments[1], "playtest.signal")

		if not RateLimiter.take(agent.bucket, os.clock()) then
			RateLimiter.drop(agent.bucket, 1)

			return false
		end

		local okEncode, encoded = encodeValue(arguments[2])

		if not okEncode then
			RateLimiter.drop(agent.bucket, 1)

			return false
		end

		local okBody, bodyJson = pcall(function()
			return HttpService:JSONEncode(encoded)
		end)

		if not okBody or #bodyJson > LIMITS.eventBytes then
			RateLimiter.drop(agent.bucket, 1)

			return false
		end

		flushDropped(agent)

		local sent = send(agent, { type = "signal", name = name, payload = encoded })

		if not sent then
			RateLimiter.drop(agent.bucket, 1)
		end

		return sent
	elseif action == "await" then
		return awaitSignal(runner, arguments[1], arguments[2])
	elseif action == "awaitClients" then
		return awaitClients(agent, runner, arguments[1], arguments[2])
	elseif action == "done" then
		finishRunner(agent, runner, true, arguments[1], nil, true)

		return true
	elseif action == "fail" then
		finishRunner(agent, runner, false, arguments[1], tostring(arguments[2] or arguments[1]), true)

		return true
	elseif action == "__return" then
		local packed = arguments[1]

		if type(packed) ~= "table" then
			return finishRunner(agent, runner, false, "playscript returned an invalid result envelope")
		end

		if packed[1] == true then
			-- Every returned value is preserved (multi-return encodes as an
			-- array with an explicit count)
			if packed.n == nil then
				packed.n = #packed
			end

			return finishRunnerWithEnvelope(agent, runner, buildEnvelope(packed))
		end

		local failure = packed[2]

		if type(failure) == "table" then
			return finishRunner(
				agent,
				runner,
				false,
				tostring(failure.message or "playscript failed"),
				tostring(failure.traceback or failure.message or "playscript failed")
			)
		end

		return finishRunner(agent, runner, false, failure, tostring(failure))
	end

	error(`unknown playscript bridge action: {action}`, 0)
end

local PLAYSCRIPT_WRAPPER_HEAD = table.concat({
	'local HttpService = game:GetService("HttpService")',
	'local __config = HttpService:JSONDecode(script.Parent:WaitForChild("Config").Value)',
	'local __bridge = script.Parent:WaitForChild("Bridge")',
	"local playtest = {",
	"\targs = HttpService:JSONDecode(__config.argsJson),",
	"\tmode = __config.mode,",
	"\tcontext = __config.context,",
	"\tjobId = __config.jobId,",
	"}",
	'function playtest.emit(data) return __bridge:Invoke("emit", data) end',
	'function playtest.log(message) return __bridge:Invoke("log", tostring(message)) end',
	'function playtest.done(value) return __bridge:Invoke("done", value) end',
	"function playtest.fail(message)",
	'\treturn __bridge:Invoke("fail", tostring(message), debug.traceback(tostring(message), 2))',
	"end",
	'function playtest.signal(name, payload) return __bridge:Invoke("signal", name, payload) end',
	'function playtest.await(name, timeoutSeconds) return __bridge:Invoke("await", name, timeoutSeconds) end',
	"function playtest.awaitClients(count, timeoutSeconds)",
	'\treturn __bridge:Invoke("awaitClients", count, timeoutSeconds)',
	"end",
	"local function __wsync_run()",
}, "\n")

local PLAYSCRIPT_WRAPPER_TAIL = table.concat({
	"end",
	"local __packed = table.pack(xpcall(__wsync_run, function(errorValue)",
	"\treturn { message = tostring(errorValue), traceback = debug.traceback(tostring(errorValue), 2) }",
	"end))",
	'__bridge:Invoke("__return", __packed)',
}, "\n")

--- Install a playscript runner from a coordinator boot frame. Boot frames
--- are retried until acknowledged; the runner sentinel keeps a retry from
--- installing the same source twice, re-acking instead.
local function bootRunner(agent: Agent, body: { [string]: Any })
	if agent.cancelled then
		return
	end

	local kind = tostring(body.runner or "")

	if kind ~= "main" and kind ~= "client" then
		return
	end

	local existing = agent.runners[kind]

	if existing ~= nil then
		if existing.installState == "installed" then
			sendImportant(agent, { type = "booted", runner = kind, identity = existing.identity })
		elseif existing.installState == "failed" then
			sendImportant(agent, {
				type = "bootFailure",
				runner = kind,
				error = existing.bootError or "playscript installation failed",
			})
		end

		return
	end

	local source = tostring(body.source or "")

	if source == "" or #source > LIMITS.maxScriptBytes then
		sendImportant(agent, {
			type = "bootFailure",
			runner = kind,
			error = if source == "" then "playscript source is empty" else "playscript source exceeds 1 MiB",
		})

		return
	end

	local identity = tostring(body.identity or "game"):lower()

	if identity ~= "game" and identity ~= "plugin" then
		identity = "game"
	end

	-- Reserve the runner before anything can yield (runnerParent may wait
	-- for PlayerScripts) so retried boot frames cannot double-install.
	local runner: Runner = {
		kind = kind :: "main" | "client",
		identity = identity :: "game" | "plugin",
		logs = tostring(body.logs or "off"):lower(),
		installState = "installing",
		bootError = nil,
		holder = nil,
		scriptInstance = nil,
		finished = false,
		destroyed = false,
		signalQueues = {},
		queuedSignalCount = 0,
		signalWaiters = {},
		clientWaiters = {},
		lastSignalSequence = 0,
	}

	agent.runners[kind] = runner

	local holderName = `__WSyncPlayscript_{agent.runtimeId}_{kind}`

	local bridge = Instance.new("BindableFunction")
	bridge.Name = "Bridge"
	bridge.OnInvoke = function(action: Any, ...: Any)
		return bridgeCall(agent, runner, tostring(action), ...)
	end

	local configValue = Instance.new("StringValue")
	configValue.Name = "Config"
	configValue.Value = HttpService:JSONEncode({
		argsJson = tostring(body.argsJson or "{}"),
		mode = tostring(body.mode or "play"),
		context = tostring(body.context or ""),
		jobId = agent.jobId,
	})

	-- Replay the recent signal backlog so a signal fired moments before this
	-- runner booted is not lost (generation-scoped, bounded, time-limited).
	local backlogCutoff = os.clock() - LIMITS.signalBacklogSeconds

	for _, signal in agent.signalBacklog do
		if signal.createdAt >= backlogCutoff and signal.seq > runner.lastSignalSequence then
			queueSignal(agent, runner, signal.name, signal.payload)
			runner.lastSignalSequence = signal.seq
		end
	end

	local wrapper = PLAYSCRIPT_WRAPPER_HEAD .. "\n" .. source .. "\n" .. PLAYSCRIPT_WRAPPER_TAIL

	local okInstall, installResult, runnerScript = pcall(function()
		local holder, installed =
			installRunnerScript(holderName, identity, agent.role, wrapper, { bridge, configValue })

		return holder, installed
	end)

	if agent.cancelled or runner.destroyed or agent.runners[kind] ~= runner then
		if okInstall then
			pcall(function()
				(installResult :: Instance):Destroy()
			end)
		end

		return
	end

	if not okInstall then
		runner.installState = "failed"
		runner.bootError = tostring(installResult)
		sendImportant(agent, { type = "bootFailure", runner = kind, error = runner.bootError })

		return
	end

	runner.holder = installResult :: Instance
	runner.scriptInstance = runnerScript

	local unwatch = watchRunnerErrors(holderName, runnerScript :: Instance, function(message, detail)
		finishRunner(agent, runner, false, message, detail)
	end);

	(runnerScript :: Any).Disabled = false
	runner.installState = "installed"
	sendImportant(agent, { type = "booted", runner = kind, identity = identity })

	task.spawn(function()
		while not runner.finished and not runner.destroyed do
			task.wait(0.25)
		end

		unwatch()
	end)
end

--- Deliver a coordinator-relayed signal to every live runner: waiters fire,
--- everything else queues (bounded, drop-counted). The backlog lets a runner
--- that boots moments later still observe it.
local function deliverSignal(agent: Agent, name: string, payload: Any)
	if name == "" or #name > LIMITS.signalNameBytes then
		RateLimiter.drop(agent.bucket, 1)

		return
	end

	agent.signalSequence += 1

	table.insert(agent.signalBacklog, {
		name = name,
		payload = payload,
		seq = agent.signalSequence,
		createdAt = os.clock(),
	})

	while #agent.signalBacklog > LIMITS.signalBacklogMax do
		table.remove(agent.signalBacklog, 1)
	end

	for _, runner in agent.runners do
		if not runner.finished and not runner.destroyed then
			runner.lastSignalSequence = agent.signalSequence

			local waiters = runner.signalWaiters[name]

			if waiters ~= nil and #waiters > 0 then
				local claimed = table.clone(waiters)

				table.clear(waiters)

				if runner.signalWaiters[name] == waiters then
					runner.signalWaiters[name] = nil
				end

				for _, waiter in claimed do
					waiter:Fire(true, payload)
				end
			else
				queueSignal(agent, runner, name, payload)
			end
		end
	end
end

local function updateClients(agent: Agent, count: Any)
	agent.clientCount = math.clamp(math.floor(finiteNumber(count, 0)), 0, 8)

	for _, runner in agent.runners do
		if not runner.finished and not runner.destroyed then
			for _, waiter in table.clone(runner.clientWaiters) do
				if agent.clientCount >= waiter.minimum then
					local index = table.find(runner.clientWaiters, waiter)

					if index ~= nil then
						table.remove(runner.clientWaiters, index)
						waiter.event:Fire(true, agent.clientCount)
					end
				end
			end
		end
	end
end

local function cancelRunners(agent: Agent, _reason: string?)
	flushImportant(agent)
	flushDropped(agent)

	for _, runner in agent.runners do
		destroyRunner(runner)
	end
end

--------------------------------------------------------------------------------
-- Captures (viewport RGBA through the shared Remote/Capture renderer)
--------------------------------------------------------------------------------

local function cleanupCaptures(agent: Agent)
	local now = os.clock()

	for captureId, session in agent.captures do
		if now - session.createdAt >= CAPTURE_TTL_SECONDS then
			agent.captures[captureId] = nil
		end
	end
end

local function captureCount(agent: Agent): number
	local count = 0

	for _ in agent.captures do
		count += 1
	end

	return count
end

--------------------------------------------------------------------------------
-- Request handlers (`playtest_request` reaches these by op name)
--------------------------------------------------------------------------------

local function contextInfo(agent: Agent): { [string]: Any }
	local playerName: string? = nil
	local userId: number? = nil

	if agent.role == "client" then
		pcall(function()
			local player = Players.LocalPlayer

			if player ~= nil then
				playerName = player.Name
				userId = player.UserId
			end
		end)
	end

	return {
		runtimeId = agent.runtimeId,
		jobId = agent.jobId,
		role = agent.role,
		protocol = Protocol.PROTOCOL,
		placeId = tostring(game.PlaceId),
		gameJobId = tostring(game.JobId),
		playerName = playerName,
		userId = userId,
		captureAvailable = agent.role == "client",
		uptimeSeconds = os.clock() - agent.startedAt,
	}
end

local function buildHandlers(agent: Agent): { [string]: (args: { [string]: Any }) -> Any }
	local handlers: { [string]: (args: { [string]: Any }) -> Any } = {}

	function handlers.info(_args)
		return contextInfo(agent)
	end

	--- `exec {source, identity?, timeoutSec?}` — the temporary-runner
	--- execution behind `playtest_exec`. The response carries the result
	--- envelope inline or as a pinned reference.
	function handlers.exec(args)
		local source = tostring(args.source or "")

		if source == "" then
			error("source must not be empty", 0)
		end

		if #source > LIMITS.maxScriptBytes then
			error("source exceeds 1 MiB", 0)
		end

		local identity = tostring(args.identity or "game"):lower()

		if identity ~= "game" and identity ~= "plugin" then
			error("identity must be game or plugin", 0)
		end

		local timeoutSec = math.clamp(finiteNumber(args.timeoutSec, 10), 0.1, LIMITS.execMaxTimeoutMs / 1000)
		local packed = executeSource(agent, source, timeoutSec, identity)

		return packageEnvelope(agent, buildEnvelope(packed))
	end

	--- `logs {sinceSeq?, limit?}` — the runtime DataModel's LogService ring
	--- (the same LogBuffer as the edit `logs` op, capturing here since agent
	--- start). Delivery is contiguous from the cursor under a count AND byte
	--- budget, so paging with `nextSeq` is lossless and the response frame
	--- always fits the edit-side body cap.
	function handlers.logs(args)
		local sinceSeq = math.max(0, math.floor(finiteNumber(args.sinceSeq, 0)))
		local limit = math.clamp(math.floor(finiteNumber(args.limit, 200)), 1, LogBuffer.CAPACITY)

		-- Read everything past the cursor (oldest first), then keep the
		-- oldest slice within budget — newer entries wait for the next page.
		local matched = LogBuffer.read({
			sinceSeq = if sinceSeq > 0 then sinceSeq else nil,
			level = nil,
			limit = nil,
		})

		local entries: { LogBuffer.Entry } = {}
		local budget = LIMITS.logsBytesBudget
		local nextSeq = sinceSeq

		for _, entry in matched do
			if #entries >= limit then
				break
			end

			local cost = #entry.message + 64

			if #entries > 0 and cost > budget then
				break
			end

			budget -= cost
			table.insert(entries, entry)
			nextSeq = math.max(nextSeq, entry.seq)
		end

		return { entries = entries, nextSeq = nextSeq }
	end

	--- `ui {root?, class?, name?, limit?}` — resolved GuiObject records
	--- (PlayClient only: the server DataModel renders no UI).
	function handlers.ui(args)
		if agent.role ~= "client" then
			error("UI inspection requires a PlayClient context", 0)
		end

		local roots: { Instance } = {}

		if type(args.root) == "string" and args.root ~= "" then
			local normalized = (args.root :: string):gsub("^/+", ""):gsub("/+$", "")
			local resolved: Instance? = nil

			local firstSegment = string.match(normalized, "^([^/]+)")
			local alias = if firstSegment ~= nil then firstSegment:lower() else ""

			if alias == "playergui" then
				local player = Players.LocalPlayer
				local playerGui = if player ~= nil then player:FindFirstChildOfClass("PlayerGui") else nil

				if playerGui ~= nil then
					local remainder = string.match(normalized, "^[^/]+/(.*)$")

					if remainder == nil or remainder == "" then
						resolved = playerGui
					else
						local cursor: Instance? = playerGui

						for segment in string.gmatch(remainder :: string, "[^/]+") do
							cursor = (cursor :: Instance):FindFirstChild(segment)

							if cursor == nil then
								break
							end
						end

						resolved = cursor
					end
				end
			else
				local okResolve, instance = pcall(function()
					return Paths.resolve(args.root)
				end)

				resolved = if okResolve then instance else nil
			end

			if resolved == nil then
				error(`UI root not found: {args.root}`, 0)
			end

			table.insert(roots, resolved)
		else
			local player = Players.LocalPlayer
			local playerGui = if player ~= nil then player:FindFirstChildOfClass("PlayerGui") else nil

			if playerGui ~= nil then
				table.insert(roots, playerGui)
			end

			pcall(function()
				table.insert(roots, game:GetService("CoreGui") :: Instance)
			end)
		end

		local limit = math.clamp(math.floor(finiteNumber(args.limit, 500)), 1, 2000)
		local classFilter = if args.class ~= nil then tostring(args.class) else nil
		local nameFilter = if args.name ~= nil then tostring(args.name):lower() else nil

		local items: { { [string]: Any } } = {}

		local function isEffectivelyVisible(instance: Instance): boolean
			local cursor: Instance? = instance

			while cursor ~= nil and cursor ~= game do
				if cursor:IsA("GuiObject") and not (cursor :: GuiObject).Visible then
					return false
				end

				if cursor:IsA("LayerCollector") and not (cursor :: LayerCollector).Enabled then
					return false
				end

				cursor = cursor.Parent
			end

			return true
		end

		local function add(instance: Instance)
			if #items >= limit then
				return
			end

			if not (instance:IsA("GuiObject") or instance:IsA("LayerCollector")) then
				return
			end

			if classFilter ~= nil and instance.ClassName ~= classFilter then
				return
			end

			if nameFilter ~= nil and string.find(instance.Name:lower(), nameFilter, 1, true) == nil then
				return
			end

			local item: { [string]: Any } = {
				path = (Paths.pathOf(instance)),
				name = instance.Name,
				class = instance.ClassName,
				visible = isEffectivelyVisible(instance),
			}

			if instance:IsA("GuiObject") then
				local gui = instance :: GuiObject

				item.localVisible = gui.Visible
				item.active = gui.Active
				item.position = { x = gui.AbsolutePosition.X, y = gui.AbsolutePosition.Y }
				item.size = { x = gui.AbsoluteSize.X, y = gui.AbsoluteSize.Y }
				item.zIndex = gui.ZIndex
			end

			pcall(function()
				item.enabled = (instance :: Any).Enabled
			end)
			pcall(function()
				item.text = capText(tostring((instance :: Any).Text), 500)
			end)

			table.insert(items, item)
		end

		for _, root in roots do
			add(root)

			for _, descendant in root:GetDescendants() do
				if #items >= limit then
					break
				end

				add(descendant)
			end
		end

		return { items = items, count = #items, truncated = #items >= limit }
	end

	--- `input {actions, timeoutMs?}` — validated virtual-input injection
	--- (PlayClient only).
	function handlers.input(args)
		if agent.role ~= "client" then
			error("virtual input requires a PlayClient context", 0)
		end

		local plan = InputActions.validate(args.actions, args.timeoutMs)

		local okInput, virtualInput = pcall(function()
			return (UserInputService :: Any):CreateVirtualInput()
		end)

		if not okInput or virtualInput == nil then
			error(`virtual input is unavailable in this PlayClient: {tostring(virtualInput)}`, 0)
		end

		local function keyCodeOf(name: string): Enum.KeyCode
			local okKey, keyCode = pcall(function()
				return (Enum.KeyCode :: Any)[name]
			end)

			if not okKey or typeof(keyCode) ~= "EnumItem" then
				error(`unknown KeyCode: {name}`, 0)
			end

			return keyCode :: Enum.KeyCode
		end

		local function buttonOf(name: string): Enum.UserInputType
			local okButton, button = pcall(function()
				return (Enum.UserInputType :: Any)[name]
			end)

			if not okButton or typeof(button) ~= "EnumItem" then
				error(`unknown mouse button: {name}`, 0)
			end

			return button :: Enum.UserInputType
		end

		local deadline = os.clock() + plan.budgetSeconds + 5
		local performed = 0

		for _, action in plan.actions do
			if os.clock() >= deadline then
				error("input sequence exceeded its execution deadline", 0)
			end

			if action.kind == "key" then
				virtualInput:SendKey(action.down ~= false, keyCodeOf(action.key :: string), action.repeatKey == true)
			elseif action.kind == "key_press" then
				local keyCode = keyCodeOf(action.key :: string)

				virtualInput:SendKey(true, keyCode, false)
				task.wait(action.seconds)
				virtualInput:SendKey(false, keyCode, false)
			elseif action.kind == "mouse_move" then
				virtualInput:SendMousePosition(Vector2.new(action.x :: number, action.y :: number))
			elseif action.kind == "mouse_delta" then
				virtualInput:SendMouseDelta(Vector2.new(action.x :: number, action.y :: number))
			elseif action.kind == "mouse_button" then
				virtualInput:SendMouseButton(
					Vector2.new(action.x :: number, action.y :: number),
					buttonOf(action.button :: string),
					action.down ~= false,
					action.repeatCount or 0
				)
			elseif action.kind == "click" then
				local button = buttonOf(action.button :: string)
				local position = Vector2.new(action.x :: number, action.y :: number)

				virtualInput:SendMouseButton(position, button, true, 0)
				task.wait(action.seconds)
				virtualInput:SendMouseButton(position, button, false, 0)
			elseif action.kind == "text" then
				virtualInput:SendTextInput(action.text :: string)
			elseif action.kind == "wait" then
				task.wait(action.seconds)
			end

			performed += 1
		end

		return { performed = performed }
	end

	--- `capture_prepare {options?}` — render one viewport capture through the
	--- shared Remote/Capture engine and pin its RGBA8 buffer (PlayClient
	--- only; the server DataModel has no viewport to photograph).
	function handlers.capture_prepare(args)
		if agent.role ~= "client" then
			error("playtest capture requires a PlayClient context", 0)
		end

		if agent.rendering then
			error("another capture is rendering — retry once it finishes", 0)
		end

		cleanupCaptures(agent)

		if captureCount(agent) >= CAPTURE_MAX_SESSIONS then
			error(`{CAPTURE_MAX_SESSIONS} runtime capture sessions are already open`, 0)
		end

		local options = Renderer.Options.normalize("viewport", args.options)

		agent.rendering = true

		local okRender, rendered = pcall(function()
			return Renderer.render({
				kind = "viewport",
				target = nil,
				options = options,
			})
		end)

		agent.rendering = false

		if not okRender then
			local caught: Any = rendered

			if type(caught) == "table" and caught.message ~= nil then
				error(tostring(caught.message), 0)
			end

			error(tostring(caught), 0)
		end

		local result = rendered :: Renderer.Result

		agent.captureCounter += 1

		local captureId = `{agent.captureCounter}-{HttpService:GenerateGUID(false)}`

		agent.captures[captureId] = {
			pixels = result.bytes,
			width = result.width,
			height = result.height,
			sha256 = BufferHash.hex(result.bytes),
			createdAt = os.clock(),
		}

		return {
			captureId = captureId,
			width = result.width,
			height = result.height,
			bytes = buffer.len(result.bytes),
			sha256 = agent.captures[captureId].sha256,
		}
	end

	function handlers.capture_read(args)
		cleanupCaptures(agent)

		local captureId = tostring(args.captureId or "")
		local session = agent.captures[captureId]

		if session == nil then
			error(`no runtime capture session {captureId}`, 0)
		end

		local totalBytes = buffer.len(session.pixels)
		local offset = math.max(0, math.floor(finiteNumber(args.offset, 0)))
		local maxLen =
			math.clamp(math.floor(finiteNumber(args.maxLen, LIMITS.captureChunkBytes)), 1, LIMITS.captureChunkBytes)
		local count = math.min(maxLen, math.max(0, totalBytes - offset))
		local data = if count > 0 then buffer.readstring(session.pixels, offset, count) else ""

		session.createdAt = os.clock()

		return {
			data = Base64.encode(data),
			eof = offset + count >= totalBytes,
			totalBytes = totalBytes,
		}
	end

	function handlers.capture_close(args)
		agent.captures[tostring(args.captureId or "")] = nil

		return { ok = true }
	end

	--- `result_read {id, offset?, maxLen?}` — one bounded base64 part of a
	--- pinned result envelope (exec or playscript completion).
	function handlers.result_read(args)
		cleanupResults(agent)

		local resultId = tostring(args.id or "")
		local session = agent.results[resultId]

		if session == nil then
			error(`no result session {resultId}`, 0)
		end

		local totalBytes = #session.bytes
		local offset = math.max(0, math.floor(finiteNumber(args.offset, 0)))
		local maxLen =
			math.clamp(math.floor(finiteNumber(args.maxLen, LIMITS.resultChunkBytes)), 1, LIMITS.resultChunkBytes)
		local count = math.min(maxLen, math.max(0, totalBytes - offset))
		local data = if count > 0 then string.sub(session.bytes, offset + 1, offset + count) else ""

		session.createdAt = os.clock()

		return {
			data = Base64.encode(data),
			eof = offset + count >= totalBytes,
			totalBytes = totalBytes,
		}
	end

	function handlers.result_close(args)
		local resultId = tostring(args.id or "")

		agent.results[resultId] = nil

		local index = table.find(agent.resultOrder, resultId)

		if index ~= nil then
			table.remove(agent.resultOrder, index)
		end

		return { ok = true }
	end

	--- `cancel_runners {reason?}` — destroy the playscript runners (the run
	--- ended or was cancelled); the agent itself stays up for keep-open use.
	function handlers.cancel_runners(args)
		cancelRunners(agent, tostring(args.reason or "cancelled"))

		return { cancelled = true }
	end

	--- `end_test {result?}` — the graceful in-playtest stop: only the
	--- PlayServer can end the whole test from inside.
	function handlers.end_test(args)
		if agent.role ~= "server" then
			error("ending a playtest requires the PlayServer context", 0)
		end

		local result = if args.result == nil then "WSync playtest stopped" else args.result

		-- Give the response a moment to reach the edit coordinator before
		-- EndTest tears this DataModel (and its connection) down.
		task.delay(0.1, function()
			local okEnd, endError = pcall(function()
				(StudioTestService :: Any):EndTest(result)
			end)

			if not okEnd then
				warn(`[WSync runtime] unable to end the playtest: {tostring(endError)}`)
			end
		end)

		return { requested = true }
	end

	return handlers
end

--------------------------------------------------------------------------------
-- Inbound frames
--------------------------------------------------------------------------------

local function rememberRequestId(agent: Agent, requestId: string)
	agent.seenRequestIds[requestId] = true
	table.insert(agent.seenRequestOrder, requestId)

	while #agent.seenRequestOrder > MAX_SEEN_REQUEST_IDS do
		local evicted = table.remove(agent.seenRequestOrder, 1) :: string

		agent.seenRequestIds[evicted] = nil
	end
end

local function handleRequest(
	agent: Agent,
	handlers: { [string]: (args: { [string]: Any }) -> Any },
	body: { [string]: Any }
)
	local requestId = tostring(body.requestId or "")
	local op = tostring(body.op or "")

	if requestId == "" or agent.seenRequestIds[requestId] then
		return
	end

	rememberRequestId(agent, requestId)

	local handler = handlers[op]

	if handler == nil then
		send(agent, {
			type = "response",
			requestId = requestId,
			ok = false,
			error = `unknown runtime op: {op}`,
		})

		return
	end

	task.spawn(function()
		local okCall, result = pcall(handler, if type(body.args) == "table" then body.args else {})

		local response: { [string]: Any }

		if okCall then
			response = { type = "response", requestId = requestId, ok = true, value = result }
		elseif type(result) == "table" and (result :: Any).__remoteError == true then
			-- Inline RemoteError markers (InputActions, Capture/Options) keep
			-- their typed code across the bridge.
			response = {
				type = "response",
				requestId = requestId,
				ok = false,
				error = {
					code = tostring((result :: Any).code),
					message = capText(tostring((result :: Any).message), MAX_ERROR_BYTES),
				},
			}
		else
			local message = tostring(result)

			-- Strip the standard `<chunk>:<line>:` prefix so runner internals
			-- never leak into op errors (the Remote/Error discipline).
			message = message:gsub("^[^\n]-:%d+:%s*", "", 1)
			response = {
				type = "response",
				requestId = requestId,
				ok = false,
				error = capText(message, MAX_ERROR_BYTES),
			}
		end

		if not send(agent, response) then
			send(agent, {
				type = "response",
				requestId = requestId,
				ok = false,
				error = "runtime response could not be serialized or sent",
			})
		end
	end)
end

local function handleEnvelope(
	agent: Agent,
	handlers: { [string]: (args: { [string]: Any }) -> Any },
	connection: Any,
	message: { [string]: Any }
)
	if tostring(message.jobId or "") ~= agent.jobId or tostring(message.runtimeId or "") ~= agent.runtimeId then
		return
	end

	local lastSeq = agent.inboundSeqs[connection] or 0
	local seq = message.seq

	if not Protocol.acceptSequence(lastSeq, seq) then
		return
	end

	local bodyJson = message.bodyJson

	if type(bodyJson) ~= "string" or bodyJson == "" or #bodyJson > LIMITS.maxBodyBytesToRuntime then
		return
	end

	local expected =
		Protocol.frameAuth(sha256Hex, agent.token, Protocol.DIRECTION_EDIT_TO_RUNTIME, agent.runtimeId, seq, bodyJson)

	if tostring(message.auth or "") ~= expected then
		return
	end

	local okBody, body = pcall(function()
		return HttpService:JSONDecode(bodyJson)
	end)

	if not okBody or type(body) ~= "table" then
		return
	end

	agent.inboundSeqs[connection] = seq

	local frameType = tostring(body.type or "")

	if frameType == "request" then
		handleRequest(agent, handlers, body)
	elseif frameType == "boot" then
		task.spawn(function()
			bootRunner(agent, body)
		end)
	elseif frameType == "signal" then
		deliverSignal(agent, tostring(body.name or ""), body.payload)
	elseif frameType == "clients" then
		updateClients(agent, body.count)
	elseif frameType == "cancel" then
		cancelRunners(agent, tostring(body.reason or "cancelled"))
	end
end

local function sendHello(agent: Agent, connection: Any)
	local hello = contextInfo(agent)

	hello.kind = Protocol.KIND_HELLO
	hello.auth = Protocol.helloAuth(sha256Hex, agent.token, agent.jobId, agent.runtimeId, agent.role, Protocol.PROTOCOL)

	local okEncode, encoded = pcall(function()
		return HttpService:JSONEncode(hello)
	end)

	if okEncode then
		pcall(function()
			connection:SendMessage(encoded)
		end)
	end
end

local function startHeartbeat(agent: Agent)
	if agent.heartbeatStarted then
		return
	end

	agent.heartbeatStarted = true

	task.spawn(function()
		while true do
			task.wait(LIMITS.heartbeatSeconds)
			flushImportant(agent)
			flushDropped(agent)
			send(agent, { type = "heartbeat", uptime = os.clock() - agent.startedAt })
			cleanupResults(agent)
			cleanupCaptures(agent)
		end
	end)
end

local function attachConnection(agent: Agent, handlers: { [string]: (args: { [string]: Any }) -> Any }, connection: Any)
	if agent.connections[connection] then
		return
	end

	agent.connections[connection] = true
	-- Prefer the newest connection: after an edit-side rebind the old one may
	-- briefly still report Connected.
	agent.activeConnection = connection

	local okBind = pcall(function()
		connection:BindToMessage(function(payload: Any)
			local okDecode, message = pcall(function()
				return HttpService:JSONDecode(tostring(payload))
			end)

			if not okDecode or type(message) ~= "table" then
				return
			end

			if message.kind == Protocol.KIND_DISCOVER then
				sendHello(agent, connection)
			elseif message.kind == Protocol.KIND then
				handleEnvelope(agent, handlers, connection, message)
			end
		end)
	end)

	if okBind then
		sendHello(agent, connection)
		startHeartbeat(agent)
	end
end

--------------------------------------------------------------------------------
-- Entry
--------------------------------------------------------------------------------

--- Boot the runtime agent. Called by init.server.luau in playtest DataModels;
--- returns quietly when this playtest carries no WSync generation handoff
--- (a manually started playtest is none of our business).
function Runtime.start()
	local role: "server" | "client" = if RunService:IsServer() then "server" else "client"

	-- Studio can start the runtime clone before Run-mode test args become
	-- observable; poll briefly instead of giving up on the first empty read.
	local token = ""
	local jobId = ""
	local deadline = os.clock() + 5

	repeat
		pcall(function()
			local testArgs = (StudioTestService :: Any):GetTestArgs()

			if type(testArgs) == "table" then
				token = tostring(testArgs[Protocol.TEST_ARG_TOKEN] or "")
				jobId = tostring(testArgs[Protocol.TEST_ARG_JOB] or "")
			end
		end)

		if token ~= "" and jobId ~= "" then
			break
		end

		task.wait(0.05)
	until os.clock() >= deadline

	if token == "" or jobId == "" then
		Log.debug("No authenticated WSync playtest generation; the runtime bridge stays disabled")

		return
	end

	-- The runtime copy encodes values with the same codec as the edit op
	-- surface; wire the Instance <-> path bridge for THIS DataModel (the edit
	-- copy's Remote/init does the same in its own VM).
	Codec.setInstanceCodec({
		pathOf = Paths.pathOf,
		resolve = Paths.resolve,
	})

	-- Capture starts now so `playtest_logs` has history from agent boot.
	LogBuffer.start()

	local agent: Agent = {
		runtimeId = HttpService:GenerateGUID(false),
		jobId = jobId,
		token = token,
		role = role,
		startedAt = os.clock(),
		connections = setmetatable({}, { __mode = "k" }) :: Any,
		activeConnection = nil,
		inboundSeqs = setmetatable({}, { __mode = "k" }) :: Any,
		outboundSeq = 0,
		seenRequestIds = {},
		seenRequestOrder = {},
		bucket = RateLimiter.new(os.clock()),
		pendingImportant = {},
		runners = {},
		results = {},
		resultOrder = {},
		resultCounter = 0,
		captures = {},
		captureCounter = 0,
		rendering = false,
		signalBacklog = {},
		signalSequence = 0,
		clientCount = 0,
		cancelled = false,
		heartbeatStarted = false,
		execCounter = 0,
	}

	local handlers = buildHandlers(agent)

	-- Stream Studio logs into active runs at their requested level (the ring
	-- above serves polling; this serves the live `log` records).
	pcall(function()
		LogService.MessageOut:Connect(function(message: string, messageType: Enum.MessageType)
			local level = if messageType == Enum.MessageType.MessageError
				then "error"
				elseif messageType == Enum.MessageType.MessageWarning then "warn"
				else "info"

			for _, runner in agent.runners do
				if not runner.finished and not runner.destroyed and runner.logs ~= "off" then
					local threshold = LOG_RANK[runner.logs]

					if threshold ~= nil and LOG_RANK[level] >= threshold then
						sendLimited(agent, {
							type = "log",
							level = level,
							message = capText(tostring(message), MAX_LOG_MESSAGE_BYTES),
						})

						break
					end
				end
			end
		end)
	end)

	local okService, connectionService = pcall(function()
		return game:GetService("PluginConnectionService")
	end)

	if not okService or connectionService == nil then
		warn("[WSync runtime] PluginConnectionService is unavailable; the playtest bridge cannot start")

		return
	end

	pcall(function()
		for _, connection in connectionService:GetPluginConnectionsOfType(Enum.PluginConnectionTargetType.Edit) do
			attachConnection(agent, handlers, connection)
		end
	end)

	connectionService.Connected:Connect(function(connection)
		if connection.Type == Enum.PluginConnectionTargetType.Edit then
			attachConnection(agent, handlers, connection)
		end
	end)

	Log.debug(`Playtest runtime agent up: {role} for job {jobId}`)
end

return Runtime
