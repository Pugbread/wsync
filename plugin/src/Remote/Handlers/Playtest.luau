--!strict
-- Playtest ops (playtest.json; pinned wave-6 contracts): the wire surface of
-- the playtest family. Argument shapes are validated here; the edit-side
-- coordinator (Playtest/init) owns jobs, generations, the
-- PluginConnectionService transport, and the run lifecycle; the runtime agent
-- (Playtest/Runtime) executes inside the playtest DataModels.
--
-- Pinned op shapes (a concurrent CLI consumes these exactly):
--   playtest_start    {mode, players?}                    → {jobId, status}
--   playtest_status   {jobId?}                            → {jobId?, status, mode?, contexts?}
--   playtest_contexts {jobId?}                            → {contexts: [{name, ready}]}
--   playtest_wait     {minimum, timeoutMs}                → {ready: [names]} | TIMEOUT
--   playtest_stop     {jobId?}                            → {ok}
--   playtest_exec     {context, source, identity, timeoutMs} → {ok, value|error}
--   playtest_logs     {context, sinceSeq?, limit?}        → {entries, nextSeq}
--   playtest_ui       {context, class?, name?}            → visibility/text/position/size records
--   playtest_input    {context, actions, timeoutMs}       → {ok, performed}
--   playtest_capture  {context, options?}                 → {captureId, width, height, bytes, sha256}
--   playtest_run_start / playtest_run_poll / playtest_run_cancel — the
--     playscript-owned run lifecycle (records, exit codes, first completion
--     wins — all decided plugin-side per playtest.json)
--   playtest_request  {context, op, args}                 → forwarded raw
--
-- `playtest_capture` adopts the pulled RGBA bytes into the SAME capture
-- session registry as edit captures — `capture_read`/`capture_close` serve
-- them unchanged.

local WSync = script:FindFirstAncestor("WSync")

local Playtest = require(WSync.Playtest)
local InputActions = require(WSync.Playtest.InputActions)
local RunState = require(WSync.Playtest.RunState)
local Protocol = require(WSync.Playtest.Protocol)

local Error = require(script.Parent.Parent.Error)
local Capture = require(script.Parent.Capture)

type Args = { [string]: any }

local PlaytestOps = {}

PlaytestOps.LIMITS = {
	maxScriptBytes = Protocol.LIMITS.maxScriptBytes,
	execDefaultTimeoutMs = Protocol.LIMITS.execDefaultTimeoutMs,
	execMaxTimeoutMs = Protocol.LIMITS.execMaxTimeoutMs,
	inputMaxActions = InputActions.LIMITS.maxActions,
	inputMaxTimeoutMs = InputActions.LIMITS.maxTimeoutMs,
	runMaxTimeoutSec = RunState.LIMITS.maxTimeoutSec,
	pollMaxRecords = RunState.LIMITS.pollMaxRecords,
	waitMaxTimeoutMs = Playtest.LIMITS.waitMaxTimeoutMs,
	eventBytes = Protocol.LIMITS.eventBytes,
	resultBytes = Protocol.LIMITS.resultBytes,
}

-- Sequence cursors are unbounded above (within safe-integer range)
local MAX_SEQ = 2 ^ 53

local function invalid(message: string): never
	error(Error.new(Error.INVALID_ARGUMENT, message), 0)
end

local function validateContext(value: any): string
	if type(value) ~= "string" or value == "" then
		invalid("context must be a non-empty string ('server' or 'client:N')")
	end

	return value :: string
end

local function optionalJobId(value: any): string?
	if value == nil then
		return nil
	end

	if type(value) ~= "string" or value == "" then
		invalid("jobId must be a non-empty string")
	end

	return value :: string
end

local function optionalInt(value: any, name: string, minimum: number, maximum: number, default: number): number
	if value == nil then
		return default
	end

	if type(value) ~= "number" or value ~= value or value % 1 ~= 0 or value < minimum or value > maximum then
		invalid(`{name} must be an integer of {minimum}-{maximum}`)
	end

	return value :: number
end

--------------------------------------------------------------------------------
-- Ops
--------------------------------------------------------------------------------

PlaytestOps.ops = {} :: { [string]: (args: Args, ctx: any) -> any }

--- `playtest_start {mode?, players?}` → `{jobId, status}`: mint a generation
--- and launch a Studio playtest asynchronously.
function PlaytestOps.ops.playtest_start(args: Args, ctx)
	local mode = args.mode or "play"

	if mode ~= "play" and mode ~= "run" and mode ~= "multiplayer" then
		invalid("mode must be 'play', 'run' or 'multiplayer'")
	end

	local players = optionalInt(args.players, "players", 1, 8, 1)

	local result = Playtest.startJob(mode :: string, players)

	ctx.audit({ op = "playtest_start", target = result.jobId, mode = mode, players = players })

	return result
end

--- `playtest_status {jobId?}` → `{jobId?, status, mode?, contexts?}`.
function PlaytestOps.ops.playtest_status(args: Args, _ctx)
	return Playtest.status(optionalJobId(args.jobId))
end

--- `playtest_contexts {jobId?}` → `{contexts: [{name, ready}]}`.
function PlaytestOps.ops.playtest_contexts(args: Args, _ctx)
	return Playtest.contextList(optionalJobId(args.jobId))
end

--- `playtest_wait {minimum, timeoutMs?}` → `{ready: [names]}` or TIMEOUT.
function PlaytestOps.ops.playtest_wait(args: Args, _ctx)
	local minimum = optionalInt(args.minimum, "minimum", 1, Playtest.LIMITS.waitMinimumMax, 1)
	local timeoutMs = optionalInt(
		args.timeoutMs,
		"timeoutMs",
		100,
		Playtest.LIMITS.waitMaxTimeoutMs,
		Playtest.LIMITS.waitDefaultTimeoutMs
	)

	return Playtest.waitForContexts(minimum, timeoutMs, optionalJobId(args.jobId))
end

--- `playtest_stop {jobId?, force?, reason?}` → `{ok}`.
function PlaytestOps.ops.playtest_stop(args: Args, ctx)
	local jobId = optionalJobId(args.jobId)

	if args.force ~= nil and type(args.force) ~= "boolean" then
		invalid("force must be a boolean")
	end

	local reason: string? = nil

	if args.reason ~= nil then
		if type(args.reason) ~= "string" then
			invalid("reason must be a string")
		end

		reason = args.reason
	end

	local result = Playtest.stop(jobId, args.force == true, reason)

	ctx.audit({ op = "playtest_stop", target = jobId or "", force = args.force == true })

	return result
end

--- `playtest_exec {context, source, identity?, timeoutMs?}` →
--- `{ok, value|error}`: run one source in a live context through the
--- temporary Script/LocalScript mechanism (game identity) or the plugin
--- sandbox (plugin identity, the explicit escape hatch). Values are
--- codec-encoded; envelopes over 1 MiB collapse to
--- `{truncated: true, bytes: N}`.
function PlaytestOps.ops.playtest_exec(args: Args, ctx)
	local context = validateContext(args.context)

	if type(args.source) ~= "string" or args.source == "" then
		invalid("source must be a non-empty string")
	end

	if #(args.source :: string) > PlaytestOps.LIMITS.maxScriptBytes then
		invalid(`source exceeds the {PlaytestOps.LIMITS.maxScriptBytes}-byte limit`)
	end

	local identity = args.identity or "game"

	if identity ~= "game" and identity ~= "plugin" then
		invalid("identity must be 'game' or 'plugin'")
	end

	local timeoutMs = optionalInt(
		args.timeoutMs,
		"timeoutMs",
		100,
		PlaytestOps.LIMITS.execMaxTimeoutMs,
		PlaytestOps.LIMITS.execDefaultTimeoutMs
	)

	local envelope = Playtest.exec(context, args.source :: string, identity :: string, timeoutMs)

	ctx.audit({ op = "playtest_exec", target = context, identity = identity, bytes = #(args.source :: string) })

	local response: { [string]: any } = {
		ok = envelope.ok == true,
	}

	if envelope.ok == true then
		response.value = envelope.value
		response.count = envelope.count
		response.truncated = envelope.truncated
	else
		response.error = tostring(envelope.error or "execution failed")

		if envelope.traceback ~= nil then
			response.traceback = tostring(envelope.traceback)
		end
	end

	return response
end

--- `playtest_logs {context, sinceSeq?, limit?}` → `{entries, nextSeq}`: the
--- runtime DataModel's LogService ring.
function PlaytestOps.ops.playtest_logs(args: Args, _ctx)
	local context = validateContext(args.context)
	local sinceSeq = optionalInt(args.sinceSeq, "sinceSeq", 0, MAX_SEQ, 0)
	local limit = optionalInt(args.limit, "limit", 1, 500, 200)

	return Playtest.logs(context, sinceSeq, limit)
end

--- `playtest_ui {context, class?, name?, root?, limit?}` → resolved
--- visibility/text/position/size records from a PlayClient.
function PlaytestOps.ops.playtest_ui(args: Args, _ctx)
	local context = validateContext(args.context)
	local filters: { [string]: any } = {}

	if args.class ~= nil then
		if type(args.class) ~= "string" or args.class == "" then
			invalid("class must be a non-empty string")
		end

		filters.class = args.class
	end

	if args.name ~= nil then
		if type(args.name) ~= "string" or args.name == "" then
			invalid("name must be a non-empty string")
		end

		filters.name = args.name
	end

	if args.root ~= nil then
		if type(args.root) ~= "string" or args.root == "" then
			invalid("root must be a non-empty path string")
		end

		filters.root = args.root
	end

	filters.limit = optionalInt(args.limit, "limit", 1, 2000, 500)

	return Playtest.ui(context, filters)
end

--- `playtest_input {context, actions, timeoutMs?}` → `{ok, performed}`:
--- validated virtual input in a PlayClient. The action list is validated
--- here (fast typed rejection) and again by the runtime before any event is
--- injected.
function PlaytestOps.ops.playtest_input(args: Args, ctx)
	local context = validateContext(args.context)

	-- Raises typed INVALID_ARGUMENT markers on any structural violation
	local plan = InputActions.validate(args.actions, args.timeoutMs)
	local timeoutMs = InputActions.validateTimeout(args.timeoutMs)

	local result = Playtest.input(context, args.actions, timeoutMs)

	ctx.audit({ op = "playtest_input", target = context, count = #plan.actions })

	return {
		ok = true,
		performed = math.floor(tonumber((result :: any).performed) or 0),
	}
end

--- `playtest_capture {context, options?}` → the same
--- `{captureId, width, height, bytes, sha256}` shape as `capture_prepare`,
--- readable through the existing `capture_read`/`capture_close` pair
--- (PlayClient contexts only; the RGBA bytes are pulled over the plugin
--- connection, SHA-256-verified, and pinned into the shared session
--- registry).
function PlaytestOps.ops.playtest_capture(args: Args, _ctx)
	local context = validateContext(args.context)

	if args.options ~= nil and type(args.options) ~= "table" then
		invalid("options must be an object")
	end

	local pulled = Playtest.capture(context, args.options)

	return Capture.adopt(pulled.pixels, pulled.width, pulled.height)
end

--- `playtest_screenshot {context}` → `{ok, contentId}`: trigger one
--- CaptureScreenshot in the PlayClient. No pixels are pulled — the engine
--- writes the frame to Roblox's tmp-capture-storage directory and the local
--- wsync CLI reads the PNG off disk. This is the only capture path that works
--- in a running playtest (CreateEditableImageAsync is blocked there).
function PlaytestOps.ops.playtest_screenshot(args: Args, _ctx)
	local context = validateContext(args.context)

	return Playtest.screenshot(context)
end

--- `playtest_run_start {script, clientScript?, context, mode, players, args,
--- timeoutSec, identity, logs, keepOpen?}` → `{jobId}`: the playscript-owned
--- run. Completion ordering, first-completion-wins, deadlines and the exit
--- mapping all live plugin-side (playtest.json).
function PlaytestOps.ops.playtest_run_start(args: Args, ctx)
	local result = Playtest.startRun(args)

	ctx.audit((result :: { [string]: any }).audit :: { [string]: any })

	return {
		jobId = result.jobId,
		run = result.run,
	}
end

--- `playtest_run_poll {jobId?, sinceSeq, waitMs?}` →
--- `{records, nextSeq, done, exit?}`: one destructive-ack page of the run's
--- NDJSON records (`started`, `ready`, `event`, `log`, `clientResult`,
--- `dropped`, `aborted`, `result`).
function PlaytestOps.ops.playtest_run_poll(args: Args, _ctx)
	local jobId = optionalJobId(args.jobId)
	local sinceSeq = optionalInt(args.sinceSeq, "sinceSeq", 0, MAX_SEQ, 0)
	local waitMs = optionalInt(args.waitMs, "waitMs", 0, Playtest.LIMITS.pollMaxWaitMs, 0)

	return Playtest.runPoll(jobId, sinceSeq, waitMs)
end

--- `playtest_run_cancel {jobId?, reason?, force?}` → `{ok}`.
function PlaytestOps.ops.playtest_run_cancel(args: Args, ctx)
	local jobId = optionalJobId(args.jobId)

	if args.force ~= nil and type(args.force) ~= "boolean" then
		invalid("force must be a boolean")
	end

	local reason: string? = nil

	if args.reason ~= nil then
		if type(args.reason) ~= "string" then
			invalid("reason must be a string")
		end

		reason = args.reason
	end

	local result = Playtest.runCancel(jobId, reason, args.force == true)

	ctx.audit({ op = "playtest_run_cancel", target = jobId or "", reason = reason })

	return result
end

--- `playtest_request {context, op, args?, timeoutMs?}` → the runtime's raw
--- response value (the advanced escape hatch — no shape guarantees).
function PlaytestOps.ops.playtest_request(args: Args, ctx)
	local context = validateContext(args.context)

	if type(args.op) ~= "string" or args.op == "" then
		invalid("op must be a non-empty string")
	end

	if args.args ~= nil and type(args.args) ~= "table" then
		invalid("args must be an object")
	end

	local timeoutMs = optionalInt(args.timeoutMs, "timeoutMs", 100, 120000, 15000)

	local value =
		Playtest.rawRequest(context, args.op :: string, (args.args :: { [string]: any }?) or {}, timeoutMs / 1000)

	ctx.audit({ op = "playtest_request", target = context, prop = args.op })

	return value
end

return PlaytestOps
