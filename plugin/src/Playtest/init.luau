--!strict
-- Edit-side playtest coordinator (playtest.json; pinned wave-6 contracts).
-- The impure half of the playtest op family: everything that touches
-- StudioTestService, PluginConnectionService, or wall-clock scheduling lives
-- here, driving the pure state machines (Playtest/Jobs, Playtest/RunState)
-- and the frame discipline of Playtest/Protocol.
--
-- Responsibilities:
--   * job generations — `playtest_start` mints a job id plus a private
--     generation token, plants both in the StudioTestService test args, and
--     launches Play/Run/Multiplayer mode; the launch call yields until the
--     playtest ends, which is the authoritative "job ended" signal;
--   * the runtime registry — plugin copies inside the playtest DataModels
--     hello over PluginConnectionService with token-bound auth; Jobs assigns
--     them their `server` / `client:N` names and rejects stale generations;
--   * correlated request/response RPC to any live context (the machinery
--     behind `playtest_exec` / `logs` / `ui` / `input` / `capture` /
--     `request`), with chunked, SHA-256-verified retrieval of result
--     envelopes too large to ride inline;
--   * the run lifecycle — boot frames (retried until acknowledged), signal
--     relay (generation-scoped, broadcast to every live context including
--     the sender), client-count updates, heartbeat supervision with the
--     pinned status re-check before `aborted`, deadline enforcement, and
--     the completion/teardown ordering (first completion wins; runs stop the
--     playtest automatically unless keepOpen; a teardown that cannot be
--     confirmed downgrades the outcome to `aborted`, never lies).
--
-- Runtime changes never sync: this module exchanges only JSON strings with
-- the playtest DataModels — no Instance from a playtest DataModel ever
-- reaches Core, so the edit watcher (which only ever connects to instances
-- of the edit DataModel it was started on) cannot observe playtest state.

local HttpService = game:GetService("HttpService")

local WSync = script:FindFirstAncestor("WSync")

local Log = require(WSync.Log)
local Base64 = require(WSync.Lib.Base64)
local Error = require(WSync.Remote.Error)
local BufferHash = require(WSync.Remote.BufferHash)

local Protocol = require(script.Protocol)
local Jobs = require(script.Jobs)
local RunState = require(script.RunState)
local Host = require(script.Host)

local LIMITS = Protocol.LIMITS

type Any = any

local Playtest = {}

Playtest.LIMITS = {
	waitMinimumMax = 9,
	waitDefaultTimeoutMs = 30000,
	waitMaxTimeoutMs = 120000,
	pollMaxWaitMs = 2000,
	cancelDeadlineSec = 20,
	captureDeadlineSec = 120,
	requestDefaultTimeoutSec = 15,
}

local function sha256Hex(value: string): string
	return BufferHash.hex(buffer.fromstring(value))
end

local function describeCaught(caught: Any): string
	if Error.is(caught) then
		return tostring((caught :: Any).message)
	end

	return tostring(caught)
end

--------------------------------------------------------------------------------
-- State
--------------------------------------------------------------------------------

type Link = {
	runtimeId: string,
	connection: Any,
	inboundSeq: number,
	outboundSeq: number,
	lastSeen: number,
}

type Pending = {
	event: BindableEvent,
	response: { [string]: Any }?,
	runtimeId: string,
}

local registry = Jobs.newRegistry()
local links: { [string]: Link } = {}
local pendingRequests: { [string]: Pending } = {}
local boundConnections: { [Any]: boolean } = setmetatable({}, { __mode = "k" }) :: Any
local requestCounter = 0
-- Guards against duplicate boot-retry loops, keyed by run state then boot key
local bootLoops: { [Any]: { [string]: boolean } } = setmetatable({}, { __mode = "k" }) :: Any

local transportAvailable = Host.connectionServiceAvailable()

local function requireAvailable()
	if not transportAvailable then
		error(
			Error.new(
				Error.PLUGIN_ERROR,
				"this Studio build has no PluginConnectionService — the playtest op family is unavailable"
			),
			0
		)
	end
end

--------------------------------------------------------------------------------
-- Outbound frames
--------------------------------------------------------------------------------

local function sendToRuntime(link: Link, job: Jobs.Job, body: { [string]: Any }): boolean
	if not link.connection.Connected then
		return false
	end

	local okBody, bodyJson = pcall(function()
		return HttpService:JSONEncode(body)
	end)

	if not okBody then
		return false
	end

	link.outboundSeq += 1

	local runtimeId = link.runtimeId
	local seq = link.outboundSeq
	local envelope = {
		kind = Protocol.KIND,
		jobId = job.id,
		runtimeId = runtimeId,
		seq = seq,
		bodyJson = bodyJson,
		auth = Protocol.frameAuth(sha256Hex, job.token, Protocol.DIRECTION_EDIT_TO_RUNTIME, runtimeId, seq, bodyJson),
	}

	local okEnvelope, encoded = pcall(function()
		return HttpService:JSONEncode(envelope)
	end)

	if not okEnvelope then
		return false
	end

	return (pcall(function()
		link.connection:SendMessage(encoded)
	end))
end

--------------------------------------------------------------------------------
-- Context bookkeeping
--------------------------------------------------------------------------------

local function pruneDeadContexts()
	for runtimeId, link in links do
		if not link.connection.Connected then
			Jobs.dropContext(registry, runtimeId)
		end
	end
end

--- Resolve a live context of the active generation or raise NOT_FOUND.
local function requireContext(contextName: Any): (Jobs.Context, Link, Jobs.Job)
	requireAvailable()

	if type(contextName) ~= "string" or contextName == "" then
		error(Error.new(Error.INVALID_ARGUMENT, "context must be a non-empty string ('server' or 'client:N')"), 0)
	end

	pruneDeadContexts()

	local context = Jobs.liveContext(registry, contextName)

	if context == nil then
		error(
			Error.new(
				Error.NOT_FOUND,
				`no live playtest context '{contextName}' — check playtest_contexts for what is ready`
			),
			0
		)
	end

	local link = links[context.runtimeId]

	if link == nil or not link.connection.Connected then
		error(Error.new(Error.NOT_FOUND, `playtest context '{contextName}' is not connected`), 0)
	end

	local job = registry.jobs[context.jobId] :: Jobs.Job

	return context, link, job
end

--------------------------------------------------------------------------------
-- Correlated RPC
--------------------------------------------------------------------------------

--- One request/response round-trip to a live context. Raises TIMEOUT when
--- the runtime does not answer in time, or a typed error carrying whatever
--- the runtime reported.
local function request(contextName: string, op: string, args: { [string]: Any }, timeoutSec: number): Any
	local context, link, job = requireContext(contextName)

	requestCounter += 1

	local requestId = `{HttpService:GenerateGUID(false)}-{requestCounter}`
	local pending: Pending = {
		event = Instance.new("BindableEvent"),
		response = nil,
		runtimeId = context.runtimeId,
	}

	pendingRequests[requestId] = pending

	local sent = sendToRuntime(link, job, {
		type = "request",
		requestId = requestId,
		op = op,
		args = args,
	})

	if not sent then
		pendingRequests[requestId] = nil
		pending.event:Destroy()
		error(Error.new(Error.PLUGIN_ERROR, `could not reach playtest context '{contextName}'`), 0)
	end

	task.delay(timeoutSec, function()
		if pendingRequests[requestId] == pending and pending.response == nil then
			pending.event:Fire()
		end
	end)

	if pending.response == nil then
		pending.event.Event:Wait()
	end

	pendingRequests[requestId] = nil
	pending.event:Destroy()

	local response = pending.response

	if response == nil then
		error(
			Error.new(Error.TIMEOUT, `playtest context '{contextName}' did not answer '{op}' within {timeoutSec}s`),
			0
		)
	end

	link.lastSeen = os.clock()

	if response.ok ~= true then
		local caught = response.error

		if type(caught) == "table" and type((caught :: Any).code) == "string" then
			error(Error.new((caught :: Any).code, tostring((caught :: Any).message)), 0)
		end

		error(Error.new(Error.PLUGIN_ERROR, tostring(caught or "the playtest runtime request failed")), 0)
	end

	return response.value
end

--- Resolve a `{inline}` / `{ref}` result package into its envelope table,
--- fetching pinned envelopes in bounded, SHA-256-verified parts.
local function resolveEnvelope(contextName: string, package: Any): { [string]: Any }
	if type(package) ~= "table" then
		error(Error.new(Error.PLUGIN_ERROR, "the runtime returned an invalid result package"), 0)
	end

	if type(package.inline) == "table" then
		return package.inline
	end

	local ref = package.ref

	if type(ref) ~= "table" then
		error(Error.new(Error.PLUGIN_ERROR, "the runtime returned neither an inline result nor a reference"), 0)
	end

	local resultId = tostring(ref.id or "")
	local totalBytes = math.floor(tonumber(ref.bytes) or -1)

	if resultId == "" or totalBytes < 2 or totalBytes > LIMITS.resultBytes + 4096 then
		error(Error.new(Error.PLUGIN_ERROR, "the runtime returned invalid result metadata"), 0)
	end

	local deadline = os.clock() + 30
	local parts: { string } = {}
	local offset = 0

	local okTransfer, transferError = pcall(function()
		while offset < totalBytes do
			local remaining = deadline - os.clock()

			if remaining <= 0 then
				error("the result transfer timed out", 0)
			end

			local part = request(contextName, "result_read", {
				id = resultId,
				offset = offset,
				maxLen = LIMITS.resultChunkBytes,
			}, math.max(1, math.min(remaining, 10)))

			local data = Base64.decode(tostring((part :: Any).data or ""))

			if data == nil or #data == 0 or offset + #data > totalBytes then
				error("the result transfer returned an invalid part", 0)
			end

			table.insert(parts, data :: string)
			offset += #(data :: string)
		end
	end)

	pcall(function()
		request(contextName, "result_close", { id = resultId }, 2)
	end)

	if not okTransfer then
		error(Error.new(Error.PLUGIN_ERROR, `unable to read the result: {describeCaught(transferError)}`), 0)
	end

	local encoded = table.concat(parts)

	if #encoded ~= totalBytes or sha256Hex(encoded) ~= tostring(ref.sha256 or "") then
		error(Error.new(Error.PLUGIN_ERROR, "the result payload failed SHA-256 verification"), 0)
	end

	local okDecode, envelope = pcall(function()
		return HttpService:JSONDecode(encoded)
	end)

	if not okDecode or type(envelope) ~= "table" then
		error(Error.new(Error.PLUGIN_ERROR, "the result payload is not a valid JSON envelope"), 0)
	end

	return envelope
end

--------------------------------------------------------------------------------
-- Run terminals (declared up-front; definitions below)
--------------------------------------------------------------------------------

local performTerminal: (job: Jobs.Job, terminal: RunState.Terminal) -> ()

local function requestRunTerminal(job: Jobs.Job, terminal: RunState.Terminal): boolean
	local run = job.run :: RunState.State?

	if run == nil then
		return false
	end

	if not RunState.requestTerminal(run, terminal, os.clock()) then
		return false
	end

	task.spawn(function()
		performTerminal(job, terminal)
	end)

	return true
end

--------------------------------------------------------------------------------
-- Run wiring: boots, signals, client counts
--------------------------------------------------------------------------------

--- Boot-frame retry loop for one runner on one context: SendMessage can fail
--- transiently while a fresh runtime settles, so frames repeat (spaced ≥1 s,
--- decided by RunState.shouldSendBoot) until the authenticated booted /
--- bootFailure ack lands.
local function driveBoot(job: Jobs.Job, context: Jobs.Context, runnerKind: string, scriptSpec: RunState.ScriptSpec)
	local run = job.run :: RunState.State?

	if run == nil then
		return
	end

	local key = `{context.runtimeId}|{runnerKind}`
	local loops = bootLoops[run]

	if loops == nil then
		loops = {}
		bootLoops[run] = loops
	end

	if loops[key] then
		return
	end

	loops[key] = true

	task.spawn(function()
		while true do
			if job.run ~= run or run.terminal ~= nil or run.pendingTerminal ~= nil then
				break
			end

			local bootState = run.bootStates[key]

			if bootState == "ready" or bootState == "failed" then
				break
			end

			local liveContext = Jobs.liveContext(registry, context.name)

			if liveContext == nil or liveContext.runtimeId ~= context.runtimeId then
				-- The context vanished; a re-hello starts a fresh loop
				break
			end

			if RunState.shouldSendBoot(run, key, os.clock()) then
				local link = links[context.runtimeId]

				if link ~= nil then
					local sent = sendToRuntime(link, job, {
						type = "boot",
						runner = runnerKind,
						context = context.name,
						mode = run.config.mode,
						identity = run.config.identity,
						source = scriptSpec.source,
						argsJson = run.argsJson,
						logs = run.config.logs,
					})

					if sent then
						RunState.noteBootSent(run, key, os.clock())
					end
				end
			end

			task.wait(0.5)
		end

		loops[key] = nil
	end)
end

local function broadcastClients(job: Jobs.Job)
	local count = Jobs.liveClientCount(registry, job.id)

	for _, context in registry.contextsByName do
		if context.jobId == job.id and context.alive then
			local link = links[context.runtimeId]

			if link ~= nil then
				sendToRuntime(link, job, { type = "clients", count = count })
			end
		end
	end
end

--- A context of the active generation helloed: record readiness, boot the
--- runners it should host, refresh everyone's client count.
local function onContextReady(job: Jobs.Job, context: Jobs.Context)
	local run = job.run :: RunState.State?

	if run ~= nil and run.terminal == nil and run.pendingTerminal == nil then
		RunState.markReady(run, context.name, os.clock())

		if context.name == run.config.context then
			driveBoot(job, context, "main", run.config.script)
		end

		if context.role == "client" and run.config.clientScript ~= nil then
			driveBoot(job, context, "client", run.config.clientScript :: RunState.ScriptSpec)
		end
	end

	broadcastClients(job)
end

--------------------------------------------------------------------------------
-- Completions (first wins) and result resolution
--------------------------------------------------------------------------------

local function resolveCompletion(
	job: Jobs.Job,
	contextName: string,
	frameType: string,
	resultKey: string,
	okFetch: boolean,
	envelope: Any
)
	local run = job.run :: RunState.State?

	if run == nil then
		return
	end

	RunState.endResult(run, resultKey)

	if frameType == "clientResult" then
		if not okFetch or type(envelope) ~= "table" then
			RunState.enqueue(run, {
				type = "clientResult",
				context = contextName,
				ok = false,
				error = `unable to read the client result: {describeCaught(envelope)}`,
			}, os.clock())
		else
			RunState.enqueue(run, {
				type = "clientResult",
				context = contextName,
				ok = envelope.ok == true,
				value = envelope.value,
				error = envelope.error,
				traceback = envelope.traceback,
				count = envelope.count,
			}, os.clock())
		end

		return
	end

	-- complete: this held the completion claim
	if not okFetch or type(envelope) ~= "table" then
		RunState.releaseCompletion(run, resultKey)
		requestRunTerminal(job, {
			outcome = "failure",
			ok = false,
			error = `unable to read the playscript result: {describeCaught(envelope)}`,
			context = contextName,
			jobStatus = job.status,
		})

		return
	end

	RunState.releaseCompletion(run, resultKey)

	if envelope.ok == true then
		requestRunTerminal(job, {
			outcome = "success",
			ok = true,
			value = envelope.value,
			context = contextName,
			jobStatus = job.status,
		})
	else
		requestRunTerminal(job, {
			outcome = "failure",
			ok = false,
			error = tostring(envelope.error or "playscript failed"),
			traceback = if envelope.traceback ~= nil then tostring(envelope.traceback) else nil,
			context = contextName,
			jobStatus = job.status,
		})
	end
end

local function handleCompletionFrame(job: Jobs.Job, context: Jobs.Context, body: { [string]: Any })
	local run = job.run :: RunState.State?

	if run == nil or run.terminal ~= nil or run.pendingTerminal ~= nil then
		return
	end

	local frameType = tostring(body.type)
	local refId = if type(body.ref) == "table" then tostring((body.ref :: Any).id or "") else "inline"
	local resultKey = `{context.runtimeId}|{frameType}|{refId}`

	if not RunState.beginResult(run, resultKey) then
		return
	end

	if frameType == "complete" and not RunState.claimCompletion(run, resultKey) then
		-- First completion wins; late completions are dropped entirely
		RunState.endResult(run, resultKey)

		return
	end

	local contextName = context.name

	task.spawn(function()
		local okFetch, envelope = pcall(function()
			if type(body.inline) == "table" then
				return body.inline
			end

			return resolveEnvelope(contextName, { ref = body.ref })
		end)

		resolveCompletion(job, contextName, frameType, resultKey, okFetch, envelope)
	end)
end

--------------------------------------------------------------------------------
-- Inbound frames
--------------------------------------------------------------------------------

local function handleHello(connection: Any, message: { [string]: Any })
	local runtimeId = tostring(message.runtimeId or "")
	local jobId = tostring(message.jobId or "")
	local role = tostring(message.role or "")

	if runtimeId == "" or jobId == "" or (role ~= "server" and role ~= "client") then
		return
	end

	local job = registry.jobs[jobId]

	if job == nil or not Jobs.isActiveStatus(job.status) then
		return
	end

	if tonumber(message.protocol) ~= Protocol.PROTOCOL then
		return
	end

	local expected = Protocol.helloAuth(sha256Hex, job.token, jobId, runtimeId, role, Protocol.PROTOCOL)

	if tostring(message.auth or "") ~= expected then
		return
	end

	local context = Jobs.registerHello(registry, {
		jobId = jobId,
		runtimeId = runtimeId,
		role = role :: "server" | "client",
		now = os.clock(),
	})

	if context == nil then
		return
	end

	local link = links[runtimeId]

	if link == nil then
		links[runtimeId] = {
			runtimeId = runtimeId,
			connection = connection,
			inboundSeq = 0,
			outboundSeq = 0,
			lastSeen = os.clock(),
		}
	else
		-- Prefer the newest connection after a reconnect; sequence counters
		-- continue (the runtime's outbound counter never resets)
		link.connection = connection
		link.lastSeen = os.clock()
	end

	Log.trace(`Playtest context ready: {context.name} ({runtimeId})`)
	onContextReady(job, context)
end

local function handleEnvelope(connection: Any, message: { [string]: Any })
	local runtimeId = tostring(message.runtimeId or "")
	local link = links[runtimeId]
	local context = registry.contextsById[runtimeId]

	if link == nil or context == nil then
		return
	end

	local job = registry.jobs[context.jobId]

	if job == nil or tostring(message.jobId or "") ~= context.jobId then
		return
	end

	if not Protocol.acceptSequence(link.inboundSeq, message.seq) then
		return
	end

	local bodyJson = message.bodyJson

	if type(bodyJson) ~= "string" or bodyJson == "" or #bodyJson > LIMITS.maxBodyBytesToEdit then
		return
	end

	local expected = Protocol.frameAuth(
		sha256Hex,
		job.token,
		Protocol.DIRECTION_RUNTIME_TO_EDIT,
		runtimeId,
		message.seq :: number,
		bodyJson
	)

	if tostring(message.auth or "") ~= expected then
		return
	end

	local okBody, body = pcall(function()
		return HttpService:JSONDecode(bodyJson)
	end)

	if not okBody or type(body) ~= "table" then
		return
	end

	link.inboundSeq = message.seq :: number
	link.lastSeen = os.clock()

	-- The connection carrying valid frames is the one to answer on
	if link.connection ~= connection and connection.Connected then
		link.connection = connection
	end

	local frameType = tostring(body.type or "")

	if frameType == "response" then
		local pending = pendingRequests[tostring(body.requestId or "")]

		if pending ~= nil and pending.runtimeId == runtimeId and pending.response == nil then
			pending.response = body
			pending.event:Fire()
		end

		return
	end

	local run = job.run :: RunState.State?

	if frameType == "heartbeat" then
		if run ~= nil then
			RunState.heartbeat(run, context.name, os.clock())
		end
	elseif frameType == "booted" then
		if run ~= nil then
			local runner = tostring(body.runner or "")

			RunState.markBooted(run, `{runtimeId}|{runner}`, runner, os.clock())
		end
	elseif frameType == "bootFailure" then
		if run ~= nil then
			local runner = tostring(body.runner or "")
			local verdict = RunState.markBootFailed(run, `{runtimeId}|{runner}`, runner)

			if verdict == "main" then
				requestRunTerminal(job, {
					outcome = "bootFailure",
					ok = false,
					error = tostring(body.error or "playscript failed to boot"),
					context = context.name,
					jobStatus = job.status,
				})
			elseif verdict == "client" then
				RunState.enqueue(run, {
					type = "clientResult",
					context = context.name,
					ok = false,
					error = tostring(body.error or "client playscript failed to boot"),
				}, os.clock())
			end
		end
	elseif frameType == "event" then
		if run ~= nil then
			RunState.enqueue(run, { type = "event", context = context.name, data = body.data }, os.clock())
		end
	elseif frameType == "log" then
		if run ~= nil then
			RunState.enqueue(run, {
				type = "log",
				context = context.name,
				level = tostring(body.level or "info"),
				message = tostring(body.message or ""),
			}, os.clock())
		end
	elseif frameType == "dropped" then
		if run ~= nil then
			RunState.enqueue(run, {
				type = "dropped",
				context = context.name,
				count = math.max(1, math.floor(tonumber(body.count) or 1)),
			}, os.clock())
		end
	elseif frameType == "signal" then
		-- Generation-scoped relay: every live context of this job receives
		-- the signal, the sender included (uniform ordering for all).
		local name = tostring(body.name or "")

		if name ~= "" and #name <= LIMITS.signalNameBytes then
			for _, target in registry.contextsByName do
				if target.jobId == job.id and target.alive then
					local targetLink = links[target.runtimeId]

					if targetLink ~= nil then
						sendToRuntime(targetLink, job, {
							type = "signal",
							name = name,
							payload = body.payload,
							origin = context.name,
						})
					end
				end
			end
		end
	elseif frameType == "complete" or frameType == "clientResult" then
		handleCompletionFrame(job, context, body)
	end
end

--------------------------------------------------------------------------------
-- Stop machinery
--------------------------------------------------------------------------------

--- Push cancel frames (and optionally confirm via RPC) to every live context
--- of the job. Returns nil, or the joined failure detail when confirmation
--- was requested and some context did not confirm.
local function cancelRunnersEverywhere(job: Jobs.Job, reason: string, confirm: boolean): string?
	local failures: { string } = {}

	for _, context in registry.contextsByName do
		if context.jobId == job.id and context.alive then
			local link = links[context.runtimeId]

			if link ~= nil then
				sendToRuntime(link, job, { type = "cancel", reason = reason })
			end

			if confirm then
				local okCancel, result = pcall(function()
					return request(context.name, "cancel_runners", { reason = reason }, 2)
				end)

				if not okCancel or type(result) ~= "table" or (result :: Any).cancelled ~= true then
					local detail = if okCancel
						then "the runtime did not confirm cancellation"
						else describeCaught(result)

					table.insert(failures, `{context.name}: {detail}`)
				end
			end
		end
	end

	return if #failures > 0 then table.concat(failures, "; ") else nil
end

--- Stop the playtest of one job: the graceful ladder is PlayServer EndTest
--- (from inside), then edit-side LeaveTest, then — forced only — edit-side
--- EndTest. A forced stop that exhausts every rung marks the job failed so a
--- generation can never stay active forever. Returns `{ok = true, via?}`.
local function stopJob(job: Jobs.Job, opts: { reason: string?, result: Any? }, force: boolean): { [string]: Any }
	local priorStatus = job.status
	local beginResult = Jobs.beginStop(registry, job.id)

	if beginResult == "stopping" then
		error(Error.new(Error.CONFLICT, "a playtest stop is already in progress"), 0)
	end

	if beginResult == "inactive" then
		return { ok = true, alreadyEnded = true }
	end

	local restoreStatus: Jobs.JobStatus = if priorStatus == "starting" then "starting" else "running"

	if job.run ~= nil then
		cancelRunnersEverywhere(job, opts.reason or "playtest stopped", false)
	end

	local okService, service = pcall(function()
		return game:GetService("StudioTestService")
	end)

	if not okService or service == nil then
		Jobs.failStop(registry, job.id, "StudioTestService is unavailable", os.clock())
		error(Error.new(Error.PLUGIN_ERROR, "StudioTestService is unavailable"), 0)
	end

	local testService = service :: Any

	local function waitForEnd(seconds: number): boolean
		local deadline = os.clock() + seconds

		while job.status == "stopping" and os.clock() < deadline do
			task.wait(0.05)
		end

		return job.status ~= "stopping"
	end

	local function leaveFromEdit(): (boolean, string?)
		local okCan, allowed = pcall(function()
			return testService:CanLeaveTest()
		end)

		local reason: string? = nil

		if not okCan then
			reason = `CanLeaveTest failed: {tostring(allowed)}`
		elseif allowed ~= true then
			reason = "Studio reported that it cannot leave this playtest"
		end

		if reason ~= nil and not force then
			return false, reason
		end

		local okLeave, leaveError = pcall(function()
			testService:LeaveTest()
		end)

		if not okLeave then
			local detail = `LeaveTest failed: {tostring(leaveError)}`

			if reason ~= nil then
				detail = `{reason}; {detail}`
			end

			return false, detail
		end

		return true, nil
	end

	local function endTestFallback(previousError: string?): (boolean, string?)
		if not force then
			return false, previousError
		end

		local okEnd, endError = pcall(function()
			return testService:EndTest(opts.result or "WSync playtest stopped")
		end)

		local detail: string?

		if okEnd then
			if waitForEnd(3) then
				return true, nil
			end

			detail = "Studio accepted EndTest but the playtest remained active"
		else
			detail = `EndTest failed: {tostring(endError)}`
		end

		if previousError ~= nil and previousError ~= "" then
			return false, `{previousError}; {detail}`
		end

		return false, detail
	end

	local function failStop(detail: string): never
		if force then
			-- A terminal cleanup request must never resurrect the generation
			-- as an immortal active job.
			Jobs.failStop(registry, job.id, detail, os.clock())
		else
			Jobs.abortStop(registry, job.id, restoreStatus)
		end

		error(Error.new(Error.PLUGIN_ERROR, `unable to stop the playtest: {detail}`), 0)
	end

	-- Rung 1: ask the PlayServer to end the test from inside (cleanest — the
	-- server DataModel tears everything down itself).
	pruneDeadContexts()

	local serverContext = Jobs.liveContext(registry, "server")

	if serverContext ~= nil and serverContext.jobId == job.id then
		local okRequest = pcall(function()
			request("server", "end_test", { result = opts.result }, if force then 2 else 5)
		end)

		if okRequest then
			waitForEnd(if force then 3 else 10)

			if job.status ~= "stopping" then
				return { ok = true, via = "PlayServer.EndTest" }
			end
		end
	end

	-- Rung 2: leave the test from the edit side.
	local left, leaveError = leaveFromEdit()

	if left then
		if waitForEnd(3) then
			return { ok = true, via = "StudioTestService.LeaveTest" }
		end

		leaveError = "Studio accepted LeaveTest but the playtest remained active"
	end

	-- Rung 3 (forced only): end the test from the edit side.
	local ended, endError = endTestFallback(leaveError)

	if ended then
		return { ok = true, via = "StudioTestService.EndTest" }
	end

	failStop(tostring(endError))
	error("unreachable")
end

--------------------------------------------------------------------------------
-- Terminal execution (auto-stop unless keepOpen; downgrade on unconfirmed
-- teardown)
--------------------------------------------------------------------------------

local function downgradeToAborted(terminal: RunState.Terminal, message: string): RunState.Terminal
	local downgraded = table.clone(terminal)

	downgraded.originalOutcome = terminal.outcome
	downgraded.outcome = "aborted"
	downgraded.ok = false
	downgraded.error = message
	downgraded.reason = message

	return downgraded
end

performTerminal = function(job: Jobs.Job, terminal: RunState.Terminal)
	local run = job.run :: RunState.State?

	if run == nil or run.terminal ~= nil then
		return
	end

	-- The playtest already ended on its own: nothing to tear down.
	if terminal.external == true then
		terminal.jobStatus = job.status
		RunState.finalize(run, terminal, os.clock())

		return
	end

	-- keepOpen: leave the playtest running for later exec/logs/capture/stop;
	-- only the runners are cancelled (confirmed — an unconfirmable cleanup
	-- downgrades the outcome instead of lying).
	if run.config.keepOpen then
		if terminal.outcome ~= "aborted" then
			local cancelError = cancelRunnersEverywhere(job, terminal.outcome, true)
			local finalTerminal = terminal

			if cancelError ~= nil then
				finalTerminal = downgradeToAborted(terminal, `playscript cleanup could not be confirmed: {cancelError}`)
			end

			finalTerminal.jobStatus = job.status
			RunState.finalize(run, finalTerminal, os.clock(), cancelError)

			return
		end
	end

	local force = terminal.outcome == "timeout" or terminal.outcome == "bootFailure"

	local okStop, stopResult = pcall(function()
		return stopJob(job, { reason = terminal.outcome, result = "WSync playscript completed" }, force)
	end)

	local confirmed = okStop and type(stopResult) == "table" and (stopResult :: Any).ok == true
	local finalTerminal = terminal
	local stopError: string? = nil

	if not confirmed then
		stopError = if okStop then "the playtest stop returned without confirmation" else describeCaught(stopResult)
		finalTerminal = downgradeToAborted(terminal, `playtest teardown could not be confirmed: {stopError}`)
	end

	finalTerminal.jobStatus = job.status
	RunState.finalize(run, finalTerminal, os.clock(), stopError)
end

--- The launch call returned while a run was still alive: the job ended
--- externally (exit 4), or never booted at all (exit 5).
local function onJobEnded(job: Jobs.Job)
	local run = job.run :: RunState.State?

	if run == nil or run.terminal ~= nil or run.pendingTerminal ~= nil or run.completionClaim ~= nil then
		return
	end

	local bootFailure = not run.mainBooted and job.status == "failed"
	local message = if bootFailure
		then "the playtest ended before the main playscript booted"
		else "the playtest job ended externally"

	requestRunTerminal(job, {
		outcome = if bootFailure then "bootFailure" else "aborted",
		ok = false,
		error = if job.error ~= nil then `{message}: {job.error}` else message,
		reason = message,
		jobStatus = job.status,
		external = true,
	})
end

--------------------------------------------------------------------------------
-- Launch and supervision
--------------------------------------------------------------------------------

local function launchJob(job: Jobs.Job)
	task.spawn(function()
		Jobs.markRunning(registry, job.id, os.clock())

		local ok, result = pcall(function()
			local service = game:GetService("StudioTestService") :: Any
			local testArgs: { [string]: Any } = {}

			testArgs[Protocol.TEST_ARG_TOKEN] = job.token
			testArgs[Protocol.TEST_ARG_JOB] = job.id

			if job.mode == "run" then
				return service:ExecuteRunModeAsync(testArgs)
			elseif job.mode == "multiplayer" and job.players > 1 then
				return service:ExecuteMultiplayerTestAsync(job.players, testArgs)
			end

			-- Play mode, and single-player multiplayer: Studio's one-player
			-- multiplayer launcher creates only a PlayClient DataModel — no
			-- server to install a playscript into — so play mode provides the
			-- same one-server/one-client topology (the requested mode is
			-- still what playscripts observe).
			return service:ExecutePlayModeAsync(testArgs)
		end)

		Jobs.markEnded(registry, job.id, ok, if ok then nil else tostring(result), os.clock())
		pruneDeadContexts()

		if not ok then
			Log.debug("Playtest launch ended with an error:", result)
		end

		if job.run ~= nil then
			onJobEnded(job)
		end
	end)
end

--- The per-run supervision loop: deadlines, heartbeat staleness (with the
--- pinned job-status re-check before `aborted`), and client-count refresh.
local function superviseRun(job: Jobs.Job)
	task.spawn(function()
		local run = job.run :: RunState.State
		local lastClientCount = -1

		while run.terminal == nil and job.run == run do
			task.wait(1)

			if run.terminal ~= nil then
				break
			end

			pruneDeadContexts()

			-- Client-count refresh on change (drives awaitClients)
			local clientCount = Jobs.liveClientCount(registry, job.id)

			if clientCount ~= lastClientCount then
				lastClientCount = clientCount
				broadcastClients(job)
			end

			-- Deadlines first: a hard timeout outranks staleness
			local deadlineTerminal = RunState.checkDeadlines(run, os.clock(), job.status)

			if deadlineTerminal ~= nil then
				requestRunTerminal(job, deadlineTerminal)
				continue
			end

			-- Heartbeat supervision with the status re-check: a silent
			-- runtime inside a playtest that still reports active is only
			-- marked stale (visible in poll responses), never aborted.
			local mainContext = Jobs.liveContext(registry, run.config.context)
			local link = if mainContext ~= nil then links[mainContext.runtimeId] else nil
			local connected = mainContext ~= nil and link ~= nil and link.connection.Connected == true

			local action = RunState.superviseHeartbeat(run, {
				now = os.clock(),
				connected = connected,
				jobStatus = job.status,
				jobActive = Jobs.isActiveStatus(job.status),
			})

			if action ~= nil and action.kind == "aborted" then
				requestRunTerminal(job, {
					outcome = "aborted",
					ok = false,
					error = action.reason,
					reason = action.reason,
					jobStatus = action.jobStatus,
				})
			end
		end
	end)
end

--------------------------------------------------------------------------------
-- Public surface (the ops' backends — handlers validate wire shapes)
--------------------------------------------------------------------------------

--- `playtest_start`: mint a generation and launch the playtest.
function Playtest.startJob(mode: string, players: number): { [string]: Any }
	requireAvailable()

	local jobId = HttpService:GenerateGUID(false)
	local token = HttpService:GenerateGUID(false) .. HttpService:GenerateGUID(false)

	local job, activeId = Jobs.start(registry, {
		jobId = jobId,
		token = token,
		mode = mode :: "play" | "run" | "multiplayer",
		players = players,
		now = os.clock(),
	})

	if job == nil then
		error(Error.new(Error.CONFLICT, `a playtest job is already active: {activeId}`), 0)
	end

	table.clear(links)
	launchJob(job)

	return { jobId = job.id, status = Jobs.statusValue(job) }
end

--- `playtest_run_start`: validate the run flags, mint the generation, attach
--- the run state, launch, and supervise. Returns `{jobId, run, audit}`.
function Playtest.startRun(args: { [string]: Any }): { [string]: Any }
	requireAvailable()

	local config = RunState.validate(args)

	-- args are already-decoded JSON from the wire; re-encoding them is the
	-- canonical argsJson (playtest.json validates JSON CLI-side).
	local argsJson = "{}"

	if args.args ~= nil then
		if type(args.args) ~= "table" then
			error(Error.new(Error.INVALID_ARGUMENT, "args must be an object"), 0)
		end

		local okEncode, encoded = pcall(function()
			return HttpService:JSONEncode(args.args)
		end)

		if not okEncode then
			error(Error.new(Error.INVALID_ARGUMENT, `args could not be encoded: {tostring(encoded)}`), 0)
		end

		if #encoded > LIMITS.maxArgsJsonBytes then
			-- The args ride every boot frame; oversized payloads belong in
			-- the script itself or in emit-driven streaming.
			error(Error.new(Error.INVALID_ARGUMENT, `args exceed the {LIMITS.maxArgsJsonBytes}-byte encoded limit`), 0)
		end

		argsJson = encoded
	end

	config.script.sha256 = sha256Hex(config.script.source)

	if config.clientScript ~= nil then
		(config.clientScript :: RunState.ScriptSpec).sha256 =
			sha256Hex((config.clientScript :: RunState.ScriptSpec).source)
	end

	local jobId = HttpService:GenerateGUID(false)
	local token = HttpService:GenerateGUID(false) .. HttpService:GenerateGUID(false)
	local previousLatest = registry.latestJobId

	local job, activeId = Jobs.start(registry, {
		jobId = jobId,
		token = token,
		mode = config.mode,
		players = config.players,
		now = os.clock(),
	})

	if job == nil then
		error(Error.new(Error.CONFLICT, `a playtest job is already active: {activeId}`), 0)
	end

	local okCreate, createError = pcall(function()
		job.run = RunState.create(config, argsJson, job.id, os.clock())
	end)

	if not okCreate then
		Jobs.abandonStart(registry, jobId, previousLatest)
		error(createError, 0)
	end

	table.clear(links)
	launchJob(job)
	superviseRun(job)

	local audit: { [string]: Any } = {
		jobId = job.id,
		mode = config.mode,
		players = config.players,
		context = config.context,
		identity = config.identity,
		timeoutSec = config.timeoutSec,
		keepOpen = config.keepOpen,
		script = {
			path = config.script.path,
			sha256 = config.script.sha256,
			bytes = #config.script.source,
		},
	}

	if config.clientScript ~= nil then
		local client = config.clientScript :: RunState.ScriptSpec

		audit.clientScript = { path = client.path, sha256 = client.sha256, bytes = #client.source }
	end

	return {
		jobId = job.id,
		run = RunState.snapshot(job.run :: RunState.State),
		audit = audit,
	}
end

--- `playtest_status`: the pinned `{jobId?, status, mode?, contexts?}` shape
--- (plus the raw job status and run snapshot, additive).
function Playtest.status(jobId: string?): { [string]: Any }
	pruneDeadContexts()

	local job = Jobs.resolveJob(registry, jobId)

	if job == nil then
		if jobId ~= nil then
			error(Error.new(Error.NOT_FOUND, `no playtest job {jobId}`), 0)
		end

		return { status = "idle" }
	end

	local response: { [string]: Any } = {
		jobId = job.id,
		status = Jobs.statusValue(job),
		mode = job.mode,
		players = job.players,
		jobStatus = job.status,
		contexts = Jobs.contextsOf(registry, job.id),
	}

	if job.error ~= nil then
		response.error = job.error
	end

	local run = job.run :: RunState.State?

	if run ~= nil then
		response.run = RunState.snapshot(run)
	end

	return response
end

--- `playtest_contexts`: the live contexts of the latest (or named) job.
function Playtest.contextList(jobId: string?): { [string]: Any }
	pruneDeadContexts()

	local job = Jobs.resolveJob(registry, jobId)

	if job == nil then
		if jobId ~= nil then
			error(Error.new(Error.NOT_FOUND, `no playtest job {jobId}`), 0)
		end

		return { contexts = {} }
	end

	return { contexts = Jobs.contextsOf(registry, job.id) }
end

--- `playtest_wait`: block until ≥ minimum contexts are ready. TIMEOUT on the
--- deadline; CONFLICT when the job ends first.
function Playtest.waitForContexts(minimum: number, timeoutMs: number, jobId: string?): { [string]: Any }
	requireAvailable()

	local job = Jobs.resolveJob(registry, jobId)

	if job == nil then
		error(Error.new(Error.NOT_FOUND, "no playtest job to wait on — playtest_start one first"), 0)
	end

	local deadline = os.clock() + timeoutMs / 1000

	while true do
		pruneDeadContexts()

		local contexts = Jobs.contextsOf(registry, job.id)

		if #contexts >= minimum then
			local ready = table.create(#contexts)

			for index, context in contexts do
				ready[index] = context.name
			end

			return { ready = ready, contexts = contexts }
		end

		if not Jobs.isActiveStatus(job.status) then
			error(
				Error.new(Error.CONFLICT, `the playtest ended ({job.status}) before {minimum} context(s) connected`),
				0
			)
		end

		if os.clock() >= deadline then
			error(
				Error.new(Error.TIMEOUT, `timed out waiting for {minimum} playtest context(s) ({#contexts} ready)`),
				0
			)
		end

		task.wait(0.05)
	end
end

--- `playtest_stop`: stop the active (or named) generation.
function Playtest.stop(jobId: string?, force: boolean, reason: string?): { [string]: Any }
	requireAvailable()

	local job = Jobs.resolveJob(registry, jobId)

	if job == nil then
		error(Error.new(Error.NOT_FOUND, "no playtest job to stop"), 0)
	end

	if registry.latestJobId ~= job.id then
		error(Error.new(Error.CONFLICT, `refusing to stop a stale playtest generation: {job.id}`), 0)
	end

	if not Jobs.isActiveStatus(job.status) then
		return { ok = true, alreadyEnded = true, jobStatus = job.status }
	end

	return stopJob(job, { reason = reason }, force)
end

--- `playtest_exec`: run one source in a context and return its envelope.
function Playtest.exec(contextName: string, source: string, identity: string, timeoutMs: number): { [string]: Any }
	local timeoutSec = timeoutMs / 1000

	local package = request(contextName, "exec", {
		source = source,
		identity = identity,
		timeoutSec = timeoutSec,
	}, timeoutSec + 10)

	return resolveEnvelope(contextName, package)
end

--- `playtest_logs`: the runtime DataModel's log ring slice.
function Playtest.logs(contextName: string, sinceSeq: number?, limit: number?): Any
	return request(contextName, "logs", { sinceSeq = sinceSeq, limit = limit }, 10)
end

--- `playtest_ui`: resolved UI records from a PlayClient.
function Playtest.ui(contextName: string, filters: { [string]: Any }): Any
	return request(contextName, "ui", filters, 15)
end

--- `playtest_input`: validated virtual input in a PlayClient.
function Playtest.input(contextName: string, actions: Any, timeoutMs: number): Any
	return request(contextName, "input", {
		actions = actions,
		timeoutMs = timeoutMs,
	}, timeoutMs / 1000 + 10)
end

--- `playtest_request`: the raw escape hatch.
function Playtest.rawRequest(contextName: string, op: string, args: { [string]: Any }, timeoutSec: number): Any
	return request(contextName, op, args, timeoutSec)
end

--- `playtest_capture`: render in the PlayClient, then pull the RGBA bytes
--- over the connection in bounded parts and verify them. Returns the pixels
--- plus dimensions for the handler to adopt into the capture session
--- registry.
function Playtest.capture(
	contextName: string,
	options: { [string]: Any }?
): { pixels: buffer, width: number, height: number, sha256: string }
	local context = (requireContext(contextName))

	if context.role ~= "client" then
		error(
			Error.new(
				Error.INVALID_ARGUMENT,
				"playtest capture requires a PlayClient context — the PlayServer renders no viewport"
			),
			0
		)
	end

	local deadline = os.clock() + Playtest.LIMITS.captureDeadlineSec

	local function remaining(): number
		local left = deadline - os.clock()

		if left <= 0 then
			error(Error.new(Error.TIMEOUT, "the runtime capture exceeded its end-to-end deadline"), 0)
		end

		return math.max(1, left)
	end

	local prepared = request(contextName, "capture_prepare", { options = options }, math.min(60, remaining()))

	if type(prepared) ~= "table" then
		error(Error.new(Error.PLUGIN_ERROR, "the runtime returned invalid capture metadata"), 0)
	end

	local width = math.floor(tonumber((prepared :: Any).width) or 0)
	local height = math.floor(tonumber((prepared :: Any).height) or 0)
	local totalBytes = math.floor(tonumber((prepared :: Any).bytes) or 0)
	local captureId = tostring((prepared :: Any).captureId or "")
	local sha256 = tostring((prepared :: Any).sha256 or "")

	if
		captureId == ""
		or width < 1
		or height < 1
		or width > 4096
		or height > 4096
		or width * height > 16777216
		or totalBytes ~= width * height * 4
		or #sha256 ~= 64
	then
		pcall(function()
			request(contextName, "capture_close", { captureId = captureId }, 2)
		end)
		error(Error.new(Error.PLUGIN_ERROR, "the runtime returned invalid bounded RGBA capture metadata"), 0)
	end

	local pixels = buffer.create(totalBytes)
	local offset = 0

	local okTransfer, transferError = pcall(function()
		while offset < totalBytes do
			local chunk = request(contextName, "capture_read", {
				captureId = captureId,
				offset = offset,
				maxLen = LIMITS.captureChunkBytes,
			}, math.min(20, remaining()))

			local data = Base64.decode(tostring((chunk :: Any).data or ""))

			if data == nil or #data == 0 or offset + #data > totalBytes then
				error("the capture transfer returned an invalid part", 0)
			end

			buffer.writestring(pixels, offset, data :: string)
			offset += #(data :: string)
		end
	end)

	pcall(function()
		request(contextName, "capture_close", { captureId = captureId }, 2)
	end)

	if not okTransfer then
		error(Error.new(Error.PLUGIN_ERROR, `unable to read the capture: {describeCaught(transferError)}`), 0)
	end

	if BufferHash.hex(pixels) ~= sha256 then
		error(Error.new(Error.PLUGIN_ERROR, "the capture bytes failed SHA-256 verification"), 0)
	end

	return { pixels = pixels, width = width, height = height, sha256 = sha256 }
end

--- `playtest_run_poll`: one destructive-ack page of run records.
function Playtest.runPoll(jobId: string?, sinceSeq: number, waitMs: number): { [string]: Any }
	local job = Jobs.resolveJob(registry, jobId)
	local run = if job ~= nil then job.run :: RunState.State? else nil

	if job == nil or run == nil then
		error(Error.new(Error.NOT_FOUND, "no playtest run — playtest_run_start one first"), 0)
	end

	if waitMs > 0 and run.terminal == nil and not RunState.hasRecordsAfter(run, sinceSeq) then
		local deadline = os.clock() + math.min(waitMs, Playtest.LIMITS.pollMaxWaitMs) / 1000

		while os.clock() < deadline and run.terminal == nil and not RunState.hasRecordsAfter(run, sinceSeq) do
			task.wait(0.05)
		end
	end

	local page = RunState.poll(run, sinceSeq, nil, nil, os.clock())

	return {
		records = page.records,
		nextSeq = page.nextSeq,
		done = page.done,
		exit = page.exit,
		hasMore = page.hasMore,
		status = Jobs.statusValue(job),
		jobStatus = job.status,
		run = RunState.snapshot(run),
	}
end

--- `playtest_run_cancel`: request the aborted terminal and wait for it to
--- finalize (force also tears down a keepOpen playtest that already has its
--- terminal).
function Playtest.runCancel(jobId: string?, reason: string?, force: boolean): { [string]: Any }
	local job = Jobs.resolveJob(registry, jobId)
	local run = if job ~= nil then job.run :: RunState.State? else nil

	if job == nil or run == nil then
		error(Error.new(Error.NOT_FOUND, "no playtest run to cancel"), 0)
	end

	local effectiveReason = reason or "playtest run cancelled"

	if run.terminal == nil and run.pendingTerminal == nil then
		requestRunTerminal(job, {
			outcome = "aborted",
			ok = false,
			error = effectiveReason,
			reason = effectiveReason,
			jobStatus = job.status,
		})
	end

	if force and run.terminal ~= nil and Jobs.isActiveStatus(job.status) then
		pcall(function()
			stopJob(job, { reason = effectiveReason }, true)
		end)
	end

	local deadline = os.clock() + Playtest.LIMITS.cancelDeadlineSec

	while run.terminal == nil and os.clock() < deadline do
		task.wait(0.05)
	end

	local terminal = run.terminal

	if terminal == nil then
		error(
			Error.new(
				Error.PLUGIN_ERROR,
				`the run cancellation did not finish within {Playtest.LIMITS.cancelDeadlineSec}s`
			),
			0
		)
	end

	return {
		ok = true,
		outcome = terminal.outcome,
		exit = RunState.exitCode(terminal.outcome),
		jobStatus = job.status,
	}
end

--------------------------------------------------------------------------------
-- Transport binding (edit copies only — the runtime copy never requires this
-- module; init.server.luau routes it to Playtest/Runtime instead)
--------------------------------------------------------------------------------

local function bindConnection(connection: Any)
	if boundConnections[connection] then
		return
	end

	boundConnections[connection] = true

	local okBind = pcall(function()
		connection:BindToMessage(function(payload: Any)
			local okDecode, message = pcall(function()
				return HttpService:JSONDecode(tostring(payload))
			end)

			if not okDecode or type(message) ~= "table" then
				return
			end

			if message.kind == Protocol.KIND_HELLO then
				handleHello(connection, message)
			elseif message.kind == Protocol.KIND then
				handleEnvelope(connection, message)
			end
		end)
	end)

	if okBind then
		pcall(function()
			connection:SendMessage(HttpService:JSONEncode({ kind = Protocol.KIND_DISCOVER }))
		end)
	end
end

if transportAvailable then
	local okService, connectionService = pcall(function()
		return game:GetService("PluginConnectionService")
	end)

	if okService and connectionService ~= nil then
		pcall(function()
			for _, connection in connectionService:GetPluginConnectionsOfType(Enum.PluginConnectionTargetType.Test) do
				bindConnection(connection)
			end
		end)

		connectionService.Connected:Connect(function(connection)
			if connection.Type == Enum.PluginConnectionTargetType.Test then
				bindConnection(connection)
			end
		end)
	else
		transportAvailable = false
	end
end

return Playtest
