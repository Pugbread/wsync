--!strict
-- Change-history ops: `waypoint`, `undo`, `redo`, `transaction_begin`,
-- `transaction_finish` (waypoint.json, undo.json, redo.json; Design §5.4).
--
-- This module also owns the ChangeHistory side of the write guardrails
-- (Design §6.4): every mutating op in Handlers/Write brackets itself with
-- `History.opWaypoint` — a `WSync: <op> "<target>"` waypoint matching the
-- write-processor convention (Helpers/setWaypoint) — unless an explicit
-- transaction is open, in which case the transaction's recording brackets
-- everything until transaction_finish.
--
-- Transactions are ChangeHistoryService recordings
-- (TryBeginRecording/FinishRecording): exactly one may be open at a time
-- (CONFLICT on nesting — WriteRules owns that state machine), and the
-- transport auto-cancels an open transaction when it disconnects
-- (Remote.onTransportDisconnect -> History.onTransportDisconnect), because a
-- requester that lost its socket can never finish what it began.

local ChangeHistoryService = game:GetService("ChangeHistoryService")

local WSync = script:FindFirstAncestor("WSync")

local Log = require(WSync.Log)
local setWaypoint = require(WSync.Helpers.setWaypoint)

local Error = require(script.Parent.Parent.Error)
local WriteRules = require(script.Parent.Parent.WriteRules)

type Args = { [string]: any }

local History = {}

History.LIMITS = {
	maxWaypointNameBytes = WriteRules.LIMITS.maxWaypointNameBytes,
}

local transactionState = WriteRules.newTransactionState()

--------------------------------------------------------------------------------
-- ChangeHistoryService effects (injected into the pure state machine)
--------------------------------------------------------------------------------

--- TryBeginRecording, pcall-guarded (the API can be refused when another
--- recording is in progress, and older Studio builds may lack it entirely).
local function tryBeginRecording(name: string): string?
	local recordingId: string? = nil

	pcall(function()
		local id = (ChangeHistoryService :: any):TryBeginRecording(name, name)

		if type(id) == "string" then
			recordingId = id
		end
	end)

	return recordingId
end

local function finishRecording(recordingId: string, commit: boolean): boolean
	local operation = if commit then Enum.FinishRecordingOperation.Commit else Enum.FinishRecordingOperation.Cancel

	return (pcall(function()
		(ChangeHistoryService :: any):FinishRecording(recordingId, operation)
	end))
end

--------------------------------------------------------------------------------
-- Shared write-guardrail surface (used by Handlers/Write)
--------------------------------------------------------------------------------

function History.isTransactionOpen(): boolean
	return WriteRules.isTransactionOpen(transactionState)
end

--- The sync-frame waypoint gate (pinned wave-4 contract): while a
--- transaction recording is open, `Core/Processor/Write` skips its per-batch
--- waypoints so a daemon-driven bulk application (Keep Disk replayed as sync
--- frames inside one `transaction_begin`/`transaction_finish` bracket) is
--- exactly one Studio Undo.
function History.shouldSuppressBatchWaypoints(): boolean
	return WriteRules.shouldSuppressBatchWaypoints(transactionState)
end

--- Bracket one mutating op with a `WSync: <op> "<target>"` waypoint (the
--- write-processor convention), unless an explicit transaction is open — its
--- recording already brackets every write until transaction_finish.
function History.opWaypoint(op: string, target: string?)
	if History.isTransactionOpen() then
		return
	end

	local ok, err = pcall(setWaypoint, op, target)

	if not ok then
		Log.debug("Failed to set the op waypoint:", err)
	end
end

--- Begin a named recording for a `set_batch --waypoint` bracket (set.json:
--- one Studio undo reverses the batch). Returns nil when Studio refuses.
function History.tryBeginBatchRecording(name: string): string?
	return tryBeginRecording(name)
end

function History.finishBatchRecording(recordingId: string, commit: boolean)
	if not finishRecording(recordingId, commit) then
		Log.debug("Failed to finish the batch recording")
	end
end

--- Transport hook: auto-cancel an open transaction whose owner disconnected.
--- Loudly, per the safety framing — an agent's half-finished transaction
--- being rolled back must be visible in the Studio output.
function History.onTransportDisconnect()
	local cancelled = WriteRules.disconnectTransaction(transactionState, function(recordingId)
		return finishRecording(recordingId, false)
	end)

	if cancelled ~= nil then
		Log.warn(`Open transaction "{cancelled}" was auto-cancelled: the transport disconnected mid-transaction`)
	end
end

--------------------------------------------------------------------------------
-- Ops
--------------------------------------------------------------------------------

History.ops = {} :: { [string]: (args: Args, ctx: any) -> any }

--- `waypoint {name}` — a named change-history waypoint, verbatim (the label
--- is the user's own; auto-waypoints are the ones that get the WSync prefix).
function History.ops.waypoint(args: Args, ctx)
	local name = args.name

	if type(name) ~= "string" or name == "" or #name > History.LIMITS.maxWaypointNameBytes then
		error(
			Error.new(Error.INVALID_ARGUMENT, `name must be a string of 1-{History.LIMITS.maxWaypointNameBytes} bytes`),
			0
		)
	end

	local ok, err = pcall(function()
		(ChangeHistoryService :: any):SetWaypoint(name)
	end)

	if not ok then
		error(Error.new(Error.PLUGIN_ERROR, `SetWaypoint failed: {tostring(err)}`), 0)
	end

	ctx.audit(WriteRules.shapeAudit("waypoint", "", { prop = name }))

	return { name = name }
end

--- `undo` — request one change-history undo. The API reports nothing, so the
--- response acks the request (undo.json).
function History.ops.undo(_args: Args, ctx)
	local ok, err = pcall(function()
		(ChangeHistoryService :: any):Undo()
	end)

	if not ok then
		error(Error.new(Error.PLUGIN_ERROR, `Undo failed: {tostring(err)}`), 0)
	end

	ctx.audit(WriteRules.shapeAudit("undo", ""))

	return { requested = true }
end

--- `redo` — request one change-history redo (redo.json).
function History.ops.redo(_args: Args, ctx)
	local ok, err = pcall(function()
		(ChangeHistoryService :: any):Redo()
	end)

	if not ok then
		error(Error.new(Error.PLUGIN_ERROR, `Redo failed: {tostring(err)}`), 0)
	end

	ctx.audit(WriteRules.shapeAudit("redo", ""))

	return { requested = true }
end

--- `transaction_begin {name?}` — open the single explicit transaction
--- (a ChangeHistoryService recording). CONFLICT when one is already open.
function History.ops.transaction_begin(args: Args, ctx)
	local name = WriteRules.beginTransaction(transactionState, args.name, tryBeginRecording)

	ctx.audit(WriteRules.shapeAudit("transaction_begin", "", { prop = name }))

	return { name = name, open = true }
end

--- `transaction_finish {commit}` — close the open transaction, committing
--- (`commit:true`) or cancelling (`commit:false`) its recording.
function History.ops.transaction_finish(args: Args, ctx)
	local result = WriteRules.finishTransaction(transactionState, args.commit, finishRecording)

	ctx.audit(WriteRules.shapeAudit("transaction_finish", "", { prop = result.name }))

	return { name = result.name, committed = result.committed, open = false }
end

return History
