--!strict
-- Run-lifecycle state machine for the playscript-owned `playtest run` family
-- (playtest.json; pinned wave-6 contracts): everything `playtest_run_start` /
-- `playtest_run_poll` / `playtest_run_cancel` promise that does not touch a
-- connection lives here —
--
--   * run-flag validation (script ≤1 MiB, context grammar, mode/players/
--     identity/logs enums, timeout 1-3600 s, Run-mode restrictions);
--   * the bounded record queue: the registry's NDJSON objects (`started`,
--     `ready`, `event`, `log`, `clientResult`, `dropped`, `aborted`,
--     `result`) with monotonic `seq`, run-relative `t`, byte-and-count caps,
--     and *accurate* dropped accounting — a record is never silently lost:
--     droppable telemetry (`event`, `log`) that cannot fit is counted into a
--     `dropped` marker, and lifecycle records evict old telemetry instead of
--     being lost themselves;
--   * first-completion-wins: one completion claim, taken by the first
--     `complete` frame and only released if its result cannot be read;
--   * the deadline pair (hard timeout → exit 3; main context never booted
--     within min(45 s, timeout) → exit 5) and the heartbeat supervision
--     decision, including the pinned status re-check before `aborted` —
--     "Missing heartbeats cause a status confirmation before `aborted` is
--     emitted";
--   * the exit-code mapping: success 0, failure 2, timeout 3, aborted 4,
--     boot failure 5;
--   * destructive-ack poll pagination: records at or below `sinceSeq` are
--     acknowledged and freed, everything newer is delivered oldest-first
--     under record/byte caps.
--
-- Pure: explicit state, injected clock, no requires, no Roblox APIs —
-- loadable under a standalone `luau` binary for behavior tests. Violations
-- raise inline `__remoteError` INVALID_ARGUMENT markers (the Codec/Selector/
-- Options pattern — keep the shape in sync with Remote/Error).

local RunState = {}

RunState.LIMITS = {
	-- Keep in sync with Playtest/Protocol.LIMITS.maxScriptBytes (both modules
	-- stay require-free for standalone tests)
	maxScriptBytes = 1024 * 1024,
	maxPathBytes = 4096,
	minTimeoutSec = 1,
	maxTimeoutSec = 3600,
	defaultTimeoutSec = 600,
	maxPlayers = 8,
	-- Queue caps: at most this many undelivered records / bytes are retained
	maxRecords = 2048,
	maxQueueBytes = 8 * 1024 * 1024,
	-- Poll slice caps
	pollDefaultRecords = 200,
	pollMaxRecords = 200,
	pollMaxBytes = 512 * 1024,
	-- Boot deadline: the main context must boot within min(this, timeoutSec)
	bootDeadlineSec = 45,
	-- Heartbeat staleness threshold (keep in sync with Protocol.LIMITS)
	heartbeatStaleSec = 6,
}

export type Outcome = "success" | "failure" | "timeout" | "aborted" | "bootFailure"

export type Terminal = {
	outcome: Outcome,
	ok: boolean?,
	value: any?,
	error: string?,
	traceback: string?,
	reason: string?,
	context: string?,
	jobStatus: string?,
	elapsed: number?,
	stopError: string?,
	-- The outcome before a teardown failure downgraded this terminal
	originalOutcome: string?,
	-- Set when the playtest itself already ended (no teardown to perform)
	external: boolean?,
}

export type ScriptSpec = {
	source: string,
	path: string,
	sha256: string?,
}

export type Config = {
	script: ScriptSpec,
	clientScript: ScriptSpec?,
	context: string,
	mode: "play" | "run" | "multiplayer",
	players: number,
	timeoutSec: number,
	identity: "game" | "plugin",
	logs: "off" | "info" | "warn" | "error",
	keepOpen: boolean,
}

type QueueEntry = {
	bytes: number,
	record: { [string]: any },
}

export type State = {
	config: Config,
	argsJson: string,
	status: string,
	terminal: Terminal?,
	pendingTerminal: Terminal?,
	completionClaim: string?,
	pendingResults: { [string]: boolean },
	startedClock: number,
	timeoutAt: number,
	bootDeadlineAt: number,
	records: { QueueEntry },
	queueBytes: number,
	queueDropped: number,
	nextSeq: number,
	readyContexts: { [string]: boolean },
	bootStates: { [string]: "sent" | "ready" | "failed" },
	bootSentAt: { [string]: number },
	mainBooted: boolean,
	mainBootClock: number?,
	heartbeats: { [string]: number },
	heartbeatStale: { [string]: any }?,
}

local function invalid(message: string): never
	error({ __remoteError = true, code = "INVALID_ARGUMENT", message = message }, 0)
end

local function isFinite(value: any): boolean
	return type(value) == "number" and value == value and value > -math.huge and value < math.huge
end

--------------------------------------------------------------------------------
-- Validation
--------------------------------------------------------------------------------

local function validateScript(value: any, label: string): ScriptSpec
	local source: any
	local path: any = ""

	if type(value) == "string" then
		source = value
	elseif type(value) == "table" then
		source = value.source

		if value.path ~= nil then
			path = value.path
		end
	else
		invalid(`{label} must be a source string or an object with a source field`)
	end

	if type(source) ~= "string" or source == "" then
		invalid(`{label}.source must be a non-empty string`)
	end

	if #source > RunState.LIMITS.maxScriptBytes then
		invalid(`{label}.source exceeds the {RunState.LIMITS.maxScriptBytes}-byte limit`)
	end

	if type(path) ~= "string" or #path > RunState.LIMITS.maxPathBytes then
		invalid(`{label}.path must be a string of at most {RunState.LIMITS.maxPathBytes} bytes`)
	end

	return { source = source, path = path, sha256 = nil }
end

--- Validate a `server` / `client:N` context name. Returns the normalized name.
function RunState.validateContextName(value: any, label: string): string
	if type(value) ~= "string" then
		invalid(`{label} must be a string`)
	end

	if value == "server" then
		return "server"
	end

	local index = string.match(value :: string, "^client:(%d+)$")

	if index ~= nil and tonumber(index) ~= nil and (tonumber(index) :: number) >= 1 then
		return value :: string
	end

	invalid(`{label} must be 'server' or 'client:N'`)
	error("unreachable")
end

--- Validate and normalize the `playtest_run_start` flags (playtest.json).
--- Raises INVALID_ARGUMENT on the first violation.
function RunState.validate(args: any): Config
	if type(args) ~= "table" then
		invalid("run configuration must be an object")
	end

	local script = validateScript(args.script or args.source, "script")

	local clientScript: ScriptSpec? = nil

	if args.clientScript ~= nil then
		clientScript = validateScript(args.clientScript, "clientScript")
	end

	local context = if args.context == nil then "server" else RunState.validateContextName(args.context, "context")

	local mode = args.mode or "play"

	if mode ~= "play" and mode ~= "run" and mode ~= "multiplayer" then
		invalid("mode must be 'play', 'run' or 'multiplayer'")
	end

	local players = 1

	if args.players ~= nil then
		if
			not isFinite(args.players)
			or args.players % 1 ~= 0
			or args.players < 1
			or args.players > RunState.LIMITS.maxPlayers
		then
			invalid(`players must be an integer of 1-{RunState.LIMITS.maxPlayers}`)
		end

		players = args.players
	end

	local timeoutSec = RunState.LIMITS.defaultTimeoutSec

	if args.timeoutSec ~= nil then
		if
			not isFinite(args.timeoutSec)
			or args.timeoutSec < RunState.LIMITS.minTimeoutSec
			or args.timeoutSec > RunState.LIMITS.maxTimeoutSec
		then
			invalid(`timeoutSec must be a number of {RunState.LIMITS.minTimeoutSec}-{RunState.LIMITS.maxTimeoutSec}`)
		end

		timeoutSec = args.timeoutSec
	end

	local identity = args.identity or "game"

	if identity ~= "game" and identity ~= "plugin" then
		invalid("identity must be 'game' or 'plugin'")
	end

	local logs = args.logs or "off"

	if logs ~= "off" and logs ~= "info" and logs ~= "warn" and logs ~= "error" then
		invalid("logs must be 'off', 'info', 'warn' or 'error'")
	end

	local keepOpen = false

	if args.keepOpen ~= nil then
		if type(args.keepOpen) ~= "boolean" then
			invalid("keepOpen must be a boolean")
		end

		keepOpen = args.keepOpen
	end

	-- Studio Run mode simulates the server only: no client DataModel ever
	-- exists, so neither a client main context nor a client script can run.
	if mode == "run" then
		if context ~= "server" then
			invalid("Studio run mode has no client contexts")
		end

		if clientScript ~= nil then
			invalid("Studio run mode cannot execute a client script")
		end
	end

	return {
		script = script,
		clientScript = clientScript,
		context = context,
		mode = mode :: "play" | "run" | "multiplayer",
		players = players,
		timeoutSec = timeoutSec,
		identity = identity :: "game" | "plugin",
		logs = logs :: "off" | "info" | "warn" | "error",
		keepOpen = keepOpen,
	}
end

--------------------------------------------------------------------------------
-- Byte accounting (deterministic structural estimate — the queue caps bound
-- memory, they are not a wire-format promise)
--------------------------------------------------------------------------------

local ESTIMATE_MAX_DEPTH = 8
local ESTIMATE_CAP = 2 * 1024 * 1024

local function estimateValue(value: any, depth: number): number
	local kind = type(value)

	if kind == "string" then
		return #(value :: string) + 8
	elseif kind == "number" or kind == "boolean" then
		return 16
	elseif kind == "table" and depth < ESTIMATE_MAX_DEPTH then
		local total = 24

		for key, item in value :: { [any]: any } do
			total += estimateValue(key, depth + 1) + estimateValue(item, depth + 1)

			if total > ESTIMATE_CAP then
				return total
			end
		end

		return total
	end

	return 32
end

function RunState.estimateBytes(record: { [string]: any }): number
	return estimateValue(record, 0)
end

--------------------------------------------------------------------------------
-- Record queue
--------------------------------------------------------------------------------

-- Lifecycle records are never dropped: they evict old droppable telemetry
-- (with accounting) instead. Telemetry that cannot fit is counted, then
-- surfaced by the next `dropped` marker.
local IMPORTANT: { [string]: boolean } = {
	started = true,
	ready = true,
	clientResult = true,
	dropped = true,
	aborted = true,
	result = true,
}

local function runClock(state: State, now: number): number
	return math.max(0, math.floor((now - state.startedClock) * 1000 + 0.5) / 1000)
end

local function queueIsFull(state: State, incomingBytes: number): boolean
	return #state.records >= RunState.LIMITS.maxRecords
		or state.queueBytes + incomingBytes > RunState.LIMITS.maxQueueBytes
end

local function evictForRoom(state: State, incomingBytes: number)
	local index = 1

	while queueIsFull(state, incomingBytes) and index <= #state.records do
		local entry = state.records[index]
		local recordType = entry.record.type

		if not IMPORTANT[recordType] then
			table.remove(state.records, index)
			state.queueBytes -= entry.bytes
			state.queueDropped += 1
		else
			index += 1
		end
	end
end

--- Append without any cap check (the caller has proven or made room).
local function appendRecord(state: State, record: { [string]: any }, now: number)
	local bytes = RunState.estimateBytes(record)

	record.seq = state.nextSeq
	state.nextSeq += 1
	record.t = runClock(state, now)

	table.insert(state.records, { bytes = bytes, record = record })
	state.queueBytes += bytes
end

local function rawEnqueue(state: State, record: { [string]: any }, now: number, important: boolean): boolean
	local bytes = RunState.estimateBytes(record)

	if queueIsFull(state, bytes) then
		if important then
			evictForRoom(state, bytes)
		else
			state.queueDropped += 1

			return false
		end
	end

	appendRecord(state, record, now)

	return true
end

--- Flush the accumulated drop count as one `dropped` marker — but only when
--- it fits without evicting live records (a marker that eats an event to
--- announce dropped events would be self-defeating). While the queue is
--- saturated the count simply keeps accumulating; poll flushes it right
--- after its destructive ack frees room, and finalize reserves room
--- explicitly, so the count always reaches the consumer.
local function flushQueueDropped(state: State, now: number)
	if state.queueDropped <= 0 then
		return
	end

	local marker = { type = "dropped", context = "coordinator", count = state.queueDropped }

	if queueIsFull(state, RunState.estimateBytes(marker)) then
		return
	end

	state.queueDropped = 0
	appendRecord(state, marker, now)
end

--- Append one record. Producers are inert once the run is terminal — the
--- `result`/`aborted` record enqueued by `finalize` is always the last.
--- Returns whether the record landed (a false return has been counted).
function RunState.enqueue(state: State, record: { [string]: any }, now: number): boolean
	if state.terminal ~= nil then
		return false
	end

	flushQueueDropped(state, now)

	return rawEnqueue(state, record, now, IMPORTANT[record.type] == true)
end

--------------------------------------------------------------------------------
-- Lifecycle
--------------------------------------------------------------------------------

--- Create the run state and enqueue its `started` record (which carries the
--- audit identity: script paths, SHA-256 hashes and byte counts —
--- playtest.json "The start audit record includes script paths and SHA-256
--- hashes"). The caller fills `config.script.sha256` (and the client's)
--- before calling.
function RunState.create(config: Config, argsJson: string, jobId: string, now: number): State
	local state: State = {
		config = config,
		argsJson = argsJson,
		status = "booting",
		terminal = nil,
		pendingTerminal = nil,
		completionClaim = nil,
		pendingResults = {},
		startedClock = now,
		timeoutAt = now + config.timeoutSec,
		bootDeadlineAt = now + math.min(RunState.LIMITS.bootDeadlineSec, config.timeoutSec),
		records = {},
		queueBytes = 0,
		queueDropped = 0,
		nextSeq = 1,
		readyContexts = {},
		bootStates = {},
		bootSentAt = {},
		mainBooted = false,
		mainBootClock = nil,
		heartbeats = {},
		heartbeatStale = nil,
	}

	local started: { [string]: any } = {
		type = "started",
		jobId = jobId,
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
		started.clientScript = {
			path = config.clientScript.path,
			sha256 = config.clientScript.sha256,
			bytes = #config.clientScript.source,
		}
	end

	RunState.enqueue(state, started, now)

	return state
end

--- A runtime context of this generation became ready. Enqueues the `ready`
--- record once per context name.
function RunState.markReady(state: State, contextName: string, now: number): boolean
	if state.readyContexts[contextName] then
		return false
	end

	state.readyContexts[contextName] = true
	RunState.enqueue(state, { type = "ready", context = contextName }, now)

	return true
end

--------------------------------------------------------------------------------
-- Boot bookkeeping (retry timing is decided here; the coordinator sends)
--------------------------------------------------------------------------------

local BOOT_RESEND_SECONDS = 1

--- Whether a boot frame should be (re)sent for `key` (`runtimeId|runner`).
--- True until the runtime acknowledged (or failed) the boot, with at least
--- `BOOT_RESEND_SECONDS` between sends — coordinator boot frames are retried
--- because SendMessage can fail transiently while a fresh runtime settles.
function RunState.shouldSendBoot(state: State, key: string, now: number): boolean
	if state.terminal ~= nil or state.pendingTerminal ~= nil then
		return false
	end

	local bootState = state.bootStates[key]

	if bootState == "ready" or bootState == "failed" then
		return false
	end

	local sentAt = state.bootSentAt[key]

	if sentAt ~= nil and now - sentAt < BOOT_RESEND_SECONDS then
		return false
	end

	return true
end

function RunState.noteBootSent(state: State, key: string, now: number)
	if state.bootStates[key] == nil then
		state.bootStates[key] = "sent"
	end

	state.bootSentAt[key] = now
end

--- The runtime acknowledged a booted runner. A `main` boot moves the run to
--- its running status. Returns whether this ack was new.
function RunState.markBooted(state: State, key: string, runner: string, now: number): boolean
	if state.bootStates[key] == "ready" then
		return false
	end

	state.bootStates[key] = "ready"

	if runner == "main" and not state.mainBooted then
		state.mainBooted = true
		state.mainBootClock = now

		if state.status == "booting" then
			state.status = "running"
		end
	end

	return true
end

--- The runtime reported a boot failure. Returns "main" (the caller requests
--- a bootFailure terminal), "client" (the caller enqueues a failed
--- clientResult), or "duplicate".
function RunState.markBootFailed(state: State, key: string, runner: string): "main" | "client" | "duplicate"
	if state.bootStates[key] == "failed" then
		return "duplicate"
	end

	state.bootStates[key] = "failed"

	return if runner == "main" then "main" else "client"
end

--------------------------------------------------------------------------------
-- Heartbeats and deadlines
--------------------------------------------------------------------------------

function RunState.heartbeat(state: State, contextName: string, now: number)
	state.heartbeats[contextName] = now
end

export type SuperviseInfo = {
	now: number,
	-- Whether the main context's connection currently reports Connected
	connected: boolean,
	-- The owning job's raw status, and whether that status is still active —
	-- the pinned re-check: a silent runtime inside a live playtest is only
	-- *stale*, never aborted, until the job itself stops reporting active
	jobStatus: string,
	jobActive: boolean,
}

export type SuperviseAction = {
	kind: "aborted" | "stale",
	reason: string?,
	jobStatus: string?,
}

--- The heartbeat supervision decision for the main context. Returns an
--- `aborted` action (the caller requests the terminal), a `stale` marker
--- (visible in poll responses, no terminal — the job still reports active),
--- or nil while everything is healthy. Supervision only engages once the
--- main runner booted: before that the boot deadline owns failure.
function RunState.superviseHeartbeat(state: State, info: SuperviseInfo): SuperviseAction?
	if
		not state.mainBooted
		or state.terminal ~= nil
		or state.pendingTerminal ~= nil
		or state.completionClaim ~= nil
	then
		return nil
	end

	local mainContext = state.config.context
	local reference = state.heartbeats[mainContext] or state.mainBootClock or state.startedClock
	local age = math.max(0, info.now - reference)

	if not info.connected then
		state.heartbeatStale = {
			context = mainContext,
			ageSeconds = age,
			jobStatus = info.jobStatus,
			disconnected = true,
		}

		return { kind = "aborted", reason = "playtest runtime disconnected", jobStatus = info.jobStatus }
	end

	if age > RunState.LIMITS.heartbeatStaleSec then
		state.heartbeatStale = {
			context = mainContext,
			ageSeconds = age,
			jobStatus = info.jobStatus,
			disconnected = false,
		}

		if not info.jobActive then
			return { kind = "aborted", reason = "playtest runtime heartbeat stopped", jobStatus = info.jobStatus }
		end

		return { kind = "stale" }
	end

	state.heartbeatStale = nil

	return nil
end

--- The deadline check: returns the terminal to request when the hard timeout
--- or the main-boot deadline has passed, nil otherwise.
function RunState.checkDeadlines(state: State, now: number, jobStatus: string): Terminal?
	if state.terminal ~= nil or state.pendingTerminal ~= nil or state.completionClaim ~= nil then
		return nil
	end

	if not state.mainBooted and now >= state.bootDeadlineAt then
		return {
			outcome = "bootFailure",
			ok = false,
			error = "required playscript context did not become ready",
			jobStatus = jobStatus,
		}
	end

	if now >= state.timeoutAt then
		return {
			outcome = "timeout",
			ok = false,
			error = `playtest run timed out after {state.config.timeoutSec}s`,
			jobStatus = jobStatus,
		}
	end

	return nil
end

--------------------------------------------------------------------------------
-- Completion (first wins) and terminals
--------------------------------------------------------------------------------

--- Claim the single completion slot for a `complete` frame. The first claim
--- wins; everything after it is ignored.
function RunState.claimCompletion(state: State, key: string): boolean
	if state.terminal ~= nil or state.pendingTerminal ~= nil or state.completionClaim ~= nil then
		return false
	end

	state.completionClaim = key

	return true
end

--- Release a claim whose result could not be read — the failure terminal the
--- caller requests next must not be blocked by the dead claim.
function RunState.releaseCompletion(state: State, key: string)
	if state.completionClaim == key then
		state.completionClaim = nil
	end
end

--- Deduplicate result fetches by their `runtimeId|resultId` key.
function RunState.beginResult(state: State, key: string): boolean
	if state.pendingResults[key] then
		return false
	end

	state.pendingResults[key] = true

	return true
end

function RunState.endResult(state: State, key: string)
	state.pendingResults[key] = nil
end

--- Request the terminal outcome. Refused while another terminal is pending,
--- already final, or a completion claim is being resolved. The caller (the
--- coordinator) performs teardown, then calls `finalize`.
function RunState.requestTerminal(state: State, terminal: Terminal, now: number): boolean
	if state.terminal ~= nil or state.pendingTerminal ~= nil or state.completionClaim ~= nil then
		return false
	end

	terminal.elapsed = terminal.elapsed or runClock(state, now)
	state.pendingTerminal = terminal
	state.status = "stopping"

	return true
end

--- Make the pending (or given) terminal final and enqueue the last record:
--- `aborted` for aborted outcomes, `result` for everything else. Any pending
--- drop accounting is flushed first so its marker precedes the terminal.
function RunState.finalize(state: State, terminal: Terminal?, now: number, stopError: string?): boolean
	if state.terminal ~= nil then
		return false
	end

	local effective = terminal or state.pendingTerminal

	if effective == nil then
		return false
	end

	if stopError ~= nil then
		effective.stopError = stopError
	end

	effective.elapsed = effective.elapsed or runClock(state, now)
	state.pendingTerminal = nil
	state.terminal = effective
	state.status = effective.outcome

	local record: { [string]: any }

	if effective.outcome == "aborted" then
		record = {
			type = "aborted",
			ok = false,
			outcome = "aborted",
			reason = effective.reason or effective.error or "playtest job ended externally",
			jobStatus = effective.jobStatus,
			elapsed = effective.elapsed,
		}
	else
		record = {
			type = "result",
			ok = effective.outcome == "success",
			outcome = effective.outcome,
			value = effective.value,
			error = effective.error,
			traceback = effective.traceback,
			context = effective.context,
			elapsed = effective.elapsed,
		}
	end

	if effective.stopError ~= nil then
		record.stopError = effective.stopError
	end

	-- Reserve room for the final pair — the pending `dropped` marker (which
	-- must precede the terminal record, never trail it: consumers stop at
	-- `result`/`aborted`) plus the terminal record itself. Evicting old
	-- droppable telemetry to make that room is itself accounted, so the loop
	-- re-evaluates until the reservation holds.
	local terminalBytes = RunState.estimateBytes(record)

	while true do
		local wantMarker = state.queueDropped > 0
		local markerBytes = if wantMarker
			then RunState.estimateBytes({ type = "dropped", context = "coordinator", count = state.queueDropped })
			else 0
		local slots = if wantMarker then 2 else 1

		if
			#state.records + slots <= RunState.LIMITS.maxRecords
			and state.queueBytes + terminalBytes + markerBytes <= RunState.LIMITS.maxQueueBytes
		then
			break
		end

		local evicted = false

		for index, entry in state.records do
			if not IMPORTANT[entry.record.type] then
				table.remove(state.records, index)
				state.queueBytes -= entry.bytes
				state.queueDropped += 1
				evicted = true

				break
			end
		end

		if not evicted then
			-- Nothing droppable left; the remaining records are the bounded
			-- lifecycle set — append regardless, the caps are memory guards,
			-- not a wire promise.
			break
		end
	end

	if state.queueDropped > 0 then
		local marker = { type = "dropped", context = "coordinator", count = state.queueDropped }

		state.queueDropped = 0
		appendRecord(state, marker, now)
	end

	appendRecord(state, record, now)

	return true
end

--- The pinned exit-code mapping (playtest.json): main return or done → 0, a
--- main-script error or fail → 2, hard timeout → 3, externally ended → 4,
--- boot or required-context failure → 5.
function RunState.exitCode(outcome: string): number
	if outcome == "success" then
		return 0
	elseif outcome == "failure" then
		return 2
	elseif outcome == "timeout" then
		return 3
	elseif outcome == "aborted" then
		return 4
	end

	return 5
end

--------------------------------------------------------------------------------
-- Poll pagination
--------------------------------------------------------------------------------

export type PollResult = {
	records: { { [string]: any } },
	nextSeq: number,
	done: boolean,
	hasMore: boolean,
	exit: number?,
}

--- Whether any record newer than `sinceSeq` is waiting (drives the
--- coordinator's optional short-wait loop without touching the queue).
function RunState.hasRecordsAfter(state: State, sinceSeq: number): boolean
	if state.queueDropped > 0 then
		return true
	end

	for _, entry in state.records do
		if (entry.record.seq :: number) > sinceSeq then
			return true
		end
	end

	return false
end

--- Deliver one bounded page of records after `sinceSeq` (oldest first).
--- Everything at or below `sinceSeq` is acknowledged and freed — the caller
--- advances `sinceSeq` to `nextSeq`, so delivery is at-least-once. `done` is
--- true only when the run is terminal and this page exhausted the queue;
--- `exit` is present whenever the run is terminal.
function RunState.poll(state: State, sinceSeq: number, maxRecords: number?, maxBytes: number?, now: number?): PollResult
	local effectiveMaxRecords =
		math.clamp(maxRecords or RunState.LIMITS.pollDefaultRecords, 1, RunState.LIMITS.pollMaxRecords)
	local effectiveMaxBytes = math.clamp(maxBytes or RunState.LIMITS.pollMaxBytes, 1024, RunState.LIMITS.pollMaxBytes)

	-- Destructive ack first: the caller has everything at or below sinceSeq.
	-- The freed room then lets the pending `dropped` marker flush (it never
	-- evicts live records to announce itself).
	for index = #state.records, 1, -1 do
		local entry = state.records[index]

		if (entry.record.seq :: number) <= sinceSeq then
			state.queueBytes -= entry.bytes
			table.remove(state.records, index)
		end
	end

	if state.terminal == nil then
		flushQueueDropped(state, now or state.startedClock)
	end

	local records: { { [string]: any } } = {}
	local bytes = 0
	local lastDelivered = sinceSeq
	local hasMore = false

	for _, entry in state.records do
		local seq = entry.record.seq :: number

		if seq > sinceSeq then
			if #records >= effectiveMaxRecords or (#records > 0 and bytes + entry.bytes > effectiveMaxBytes) then
				hasMore = true
				break
			end

			table.insert(records, entry.record)
			bytes += entry.bytes
			lastDelivered = seq
		end
	end

	local terminal = state.terminal

	return {
		records = records,
		nextSeq = lastDelivered,
		done = terminal ~= nil and not hasMore,
		hasMore = hasMore,
		exit = if terminal ~= nil then RunState.exitCode(terminal.outcome) else nil,
	}
end

--------------------------------------------------------------------------------
-- Snapshot
--------------------------------------------------------------------------------

function RunState.snapshot(state: State): { [string]: any }
	return {
		status = state.status,
		context = state.config.context,
		mode = state.config.mode,
		players = state.config.players,
		identity = state.config.identity,
		logs = state.config.logs,
		timeoutSec = state.config.timeoutSec,
		keepOpen = state.config.keepOpen,
		mainBooted = state.mainBooted,
		lastSeq = state.nextSeq - 1,
		outcome = if state.terminal ~= nil then state.terminal.outcome else nil,
		exit = if state.terminal ~= nil then RunState.exitCode(state.terminal.outcome) else nil,
		heartbeatStale = state.heartbeatStale,
	}
end

return RunState
