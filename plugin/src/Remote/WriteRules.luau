--!strict
-- Pure guard/shaping logic for the mutating remote-op slice (Design §5.4,
-- §10.6; set.json, mv.json): the `set Parent` guard, batch pre-validation,
-- cross-service boundary detection, property-write error classification, the
-- audit-object shape, and the single-transaction state machine.
--
-- Everything here is decision logic — no Roblox APIs, no requires — so the
-- module stays loadable under a standalone `luau` binary for behavior tests
-- (the Codec/Selector pattern). Side effects (ChangeHistoryService calls,
-- actual property writes) are injected by Handlers/Write and
-- Handlers/History.
--
-- Error tables constructed here match Remote/Error.luau's RemoteError shape;
-- keep the three fields in sync.

local WriteRules = {}

WriteRules.LIMITS = {
	maxPropNameBytes = 128, -- mirrors Handlers/Inspect and query prop names
	maxBatchWrites = 500,
	maxWaypointNameBytes = 200,
	maxAttrNameBytes = 100, -- the engine's attribute-name limit
	maxTagBytes = 512,
	maxCallArgs = 64,
	maxEvalSourceBytes = 1024 * 1024,
}

-- Error codes (Remote/Error.luau, Design §5.3) — inlined so this module
-- stays require-free.
local NOT_FOUND = "NOT_FOUND"
local PERMISSION_REQUIRED = "PERMISSION_REQUIRED"
local INVALID_ARGUMENT = "INVALID_ARGUMENT"
local CONFLICT = "CONFLICT"
local PLUGIN_ERROR = "PLUGIN_ERROR"

local function fail(code: string, message: string): never
	error({ __remoteError = true, code = code, message = message }, 0)
end

--------------------------------------------------------------------------------
-- Property-name and Parent-guard validation
--------------------------------------------------------------------------------

--- Validate a property name (also used for method names by `call`). Raises
--- INVALID_ARGUMENT with `label` naming the offending field.
function WriteRules.validatePropName(prop: any, label: string?): string
	local field = label or "prop"

	if type(prop) ~= "string" or prop == "" or #prop > WriteRules.LIMITS.maxPropNameBytes then
		fail(INVALID_ARGUMENT, `{field} must be a string of 1-{WriteRules.LIMITS.maxPropNameBytes} bytes`)
	end

	return prop
end

--- The `Name` write guard (set.json, new.json — defense in depth: the CLI
--- refuses pre-network, and the plugin refuses again). Paths are
--- `/`-separated names with no escaping (Paths.luau's v1 limitation), so an
--- empty or slashed Name would leave the instance unaddressable by every
--- path-based op afterwards — only `eval` could still reach it. Raises
--- INVALID_ARGUMENT; non-string values fall through to the engine's own type
--- error on the actual write.
function WriteRules.checkNameWrite(prop: string, value: any)
	if prop ~= "Name" or type(value) ~= "string" then
		return
	end

	if value == "" then
		fail(INVALID_ARGUMENT, "Name must be a non-empty string")
	end

	if value:find("/", 1, true) ~= nil then
		fail(
			INVALID_ARGUMENT,
			"Name must not contain '/': paths are '/'-separated with no escaping, "
				.. "so a slashed name could never be addressed by any path-based op"
		)
	end
end

--- The `set Parent` guard (set.json, Design §10.6 — defense in depth: the CLI
--- refuses pre-network, and the plugin refuses again). Raises
--- PERMISSION_REQUIRED unless `force` is exactly true.
function WriteRules.checkParentGuard(prop: string, force: boolean)
	if prop ~= "Parent" or force == true then
		return
	end

	fail(
		PERMISSION_REQUIRED,
		"setting Parent directly is guarded: reparent with the mv op (wsync mv), "
			.. "or pass forceParent:true (--force-parent) only when raw parent assignment is intentional"
	)
end

--------------------------------------------------------------------------------
-- Batch pre-validation (set.json)
--------------------------------------------------------------------------------

export type BatchWrite = {
	path: string,
	prop: string,
	value: any,
}

--- Pre-validate a whole `set_batch` payload before anything is touched:
--- structural shape and the Parent guard are pre-flight, so one bad entry
--- rejects the entire batch (set.json: "`--batch` rejects the whole batch
--- before any network call when any entry trips a guardrail; `--keep-going`
--- only relaxes per-entry runtime failures, never the pre-flight guards").
--- Batches carry no per-entry force escape — a Parent write always rejects.
--- Path resolution and value decoding stay per-entry runtime concerns.
function WriteRules.validateBatch(writes: any): { BatchWrite }
	if type(writes) ~= "table" then
		fail(INVALID_ARGUMENT, "writes must be an array of {path, prop, value} objects")
	end

	local total = 0

	for _ in writes do
		total += 1
	end

	local count = #writes

	if count ~= total then
		fail(INVALID_ARGUMENT, "writes must be a dense array (non-array keys found)")
	end

	if count == 0 then
		fail(INVALID_ARGUMENT, "writes must contain at least one entry")
	end

	if count > WriteRules.LIMITS.maxBatchWrites then
		fail(INVALID_ARGUMENT, `writes exceeds the {WriteRules.LIMITS.maxBatchWrites}-entry batch limit`)
	end

	local normalized = table.create(count)

	for index, entry in ipairs(writes) do
		if type(entry) ~= "table" then
			fail(INVALID_ARGUMENT, `writes[{index}] must be an object`)
		end

		if type(entry.path) ~= "string" then
			fail(INVALID_ARGUMENT, `writes[{index}].path must be a string`)
		end

		local okProp, propOrError = pcall(WriteRules.validatePropName, entry.prop)

		if not okProp then
			fail(
				INVALID_ARGUMENT,
				`writes[{index}].prop must be a string of 1-{WriteRules.LIMITS.maxPropNameBytes} bytes`
			)
		end

		local prop = propOrError :: string

		if prop == "Parent" then
			-- No per-entry force in batch mode: the whole batch is rejected
			-- before anything is written (set.json).
			fail(
				PERMISSION_REQUIRED,
				`writes[{index}] sets Parent, which is never allowed in a batch: `
					.. "reparent with the mv op (wsync mv), or use a single set with "
					.. "forceParent:true (--force-parent) when raw parent assignment is intentional"
			)
		end

		normalized[index] = {
			path = entry.path,
			prop = prop,
			value = entry.value,
		}
	end

	return normalized
end

--------------------------------------------------------------------------------
-- Cross-service boundary detection (mv.json)
--------------------------------------------------------------------------------

--- The top-level service that owns a `/`-separated Studio path — its first
--- segment. The empty path (the DataModel itself) owns the empty service.
function WriteRules.serviceOf(path: string): string
	local first = path:match("[^/]+")
	return first or ""
end

--- Whether reparenting `fromPath` under the parent `toParentPath` crosses a
--- top-level service boundary (mv.json: cross-service moves require --force).
--- Also returns both service names for the refusal message. A destination of
--- `""` (the DataModel itself) always counts as crossing.
function WriteRules.crossesServiceBoundary(fromPath: string, toParentPath: string): (boolean, string, string)
	local fromService = WriteRules.serviceOf(fromPath)
	local toService = WriteRules.serviceOf(toParentPath)

	return fromService ~= toService, fromService, toService
end

--------------------------------------------------------------------------------
-- Property-write failure classification (set.json error semantics)
--------------------------------------------------------------------------------

-- Ordered (first match wins) engine-message fragments, matched lowercase.
-- Unknown property -> NOT_FOUND; read-only / reflection-rejected / security
-- -> PERMISSION_REQUIRED; type mismatch -> INVALID_ARGUMENT; anything else
-- stays PLUGIN_ERROR.
local FAILURE_PATTERNS: { { fragment: string, code: string } } = {
	{ fragment = "is not a valid member", code = NOT_FOUND },
	{ fragment = "read only", code = PERMISSION_REQUIRED },
	{ fragment = "read-only", code = PERMISSION_REQUIRED },
	{ fragment = "readonly", code = PERMISSION_REQUIRED },
	{ fragment = "not a scriptable", code = PERMISSION_REQUIRED },
	{ fragment = "not scriptable", code = PERMISSION_REQUIRED },
	{ fragment = "lacking permission", code = PERMISSION_REQUIRED },
	{ fragment = "security check", code = PERMISSION_REQUIRED },
	{ fragment = "current identity", code = PERMISSION_REQUIRED },
	{ fragment = "capability", code = PERMISSION_REQUIRED },
	{ fragment = "locked", code = PERMISSION_REQUIRED },
	{ fragment = "expected, got", code = INVALID_ARGUMENT },
	{ fragment = "unable to cast", code = INVALID_ARGUMENT },
	{ fragment = "invalid argument", code = INVALID_ARGUMENT },
	{ fragment = "cannot be converted", code = INVALID_ARGUMENT },
	{ fragment = "invalid value", code = INVALID_ARGUMENT },
	-- A forced Parent write into the moved instance's own subtree
	{ fragment = "circular reference", code = INVALID_ARGUMENT },
}

--- Map a raw engine error message from a failed property write onto the wire
--- error code contract.
function WriteRules.classifyWriteFailure(message: string): string
	local haystack = message:lower()

	for _, pattern in FAILURE_PATTERNS do
		if haystack:find(pattern.fragment, 1, true) ~= nil then
			return pattern.code
		end
	end

	return PLUGIN_ERROR
end

--------------------------------------------------------------------------------
-- Audit shape (Design §6.4: every daemon-side Studio mutation reaches
-- `writes.log`; the daemon consumes `meta.audit` next wave)
--------------------------------------------------------------------------------

export type Audit = {
	op: string,
	target: string,
	class: string?,
	prop: string?,
	to: string?,
	count: number?,
}

--- The compact audit object attached to every mutating response's meta:
--- `{op, target, class?, prop?, to?, count?}`. `target` is the instance path
--- that best identifies the affected instance ("" for DataModel-scoped ops
--- like eval/save/undo). `prop` carries the property name for set, the
--- attribute name for set_attr/rm_attr, the tag for add_tag/rm_tag, and the
--- method name for call. `to` is mv's resulting path; `count` is set_batch's
--- applied-write count.
function WriteRules.shapeAudit(
	op: string,
	target: string?,
	extra: { class: string?, prop: string?, to: string?, count: number? }?
): Audit
	local audit: Audit = {
		op = op,
		target = target or "",
	}

	if extra ~= nil then
		audit.class = extra.class
		audit.prop = extra.prop
		audit.to = extra.to
		audit.count = extra.count
	end

	return audit
end

--------------------------------------------------------------------------------
-- Transaction state machine (transaction_begin / transaction_finish)
--
-- Exactly one transaction may be open at a time; nesting is a CONFLICT. The
-- actual ChangeHistoryService recording calls are injected so the machine
-- stays pure: `tryBegin(name) -> recordingId?` and
-- `finishRecording(recordingId, commit) -> boolean`.
--------------------------------------------------------------------------------

export type TransactionState = {
	open: boolean,
	name: string,
	recordingId: string,
	startedAt: number,
}

local DEFAULT_TRANSACTION_NAME = "WSync transaction"

function WriteRules.newTransactionState(): TransactionState
	return {
		open = false,
		name = "",
		recordingId = "",
		startedAt = 0,
	}
end

function WriteRules.isTransactionOpen(state: TransactionState): boolean
	return state.open
end

--- The sync-frame waypoint gate (pinned wave-4 contract): while the explicit
--- transaction's recording is open, the write processor's per-batch
--- waypoints must be suppressed — the recording brackets every applied sync
--- frame, so a daemon-driven bulk application lands as exactly one Studio
--- Undo. `beginTransaction` only marks the state open once Studio granted
--- the recording, so "transaction open" and "recording open" coincide.
function WriteRules.shouldSuppressBatchWaypoints(state: TransactionState): boolean
	return state.open
end

--- Open the single transaction. Raises CONFLICT when one is already open or
--- when Studio refuses the recording, INVALID_ARGUMENT for a bad name.
--- Returns the transaction's display name.
function WriteRules.beginTransaction(state: TransactionState, rawName: any, tryBegin: (name: string) -> string?): string
	if state.open then
		fail(
			CONFLICT,
			`a transaction ('{state.name}') is already open; finish it with transaction_finish before beginning another`
		)
	end

	local name = DEFAULT_TRANSACTION_NAME

	if rawName ~= nil then
		if type(rawName) ~= "string" then
			fail(INVALID_ARGUMENT, "name must be a string")
		end

		if #rawName > WriteRules.LIMITS.maxWaypointNameBytes then
			fail(INVALID_ARGUMENT, `name exceeds {WriteRules.LIMITS.maxWaypointNameBytes} bytes`)
		end

		if rawName ~= "" then
			name = rawName
		end
	end

	local recordingId = tryBegin(name)

	if type(recordingId) ~= "string" or recordingId == "" then
		fail(
			CONFLICT,
			"Studio refused to begin a change-history recording (another recording may already be in progress)"
		)

		error("unreachable")
	end

	state.open = true
	state.name = name
	state.recordingId = recordingId
	state.startedAt = os.clock()

	return name
end

--- Close the open transaction, committing or cancelling its recording. The
--- state is cleared even when the injected finish fails — a broken recording
--- cannot be retried. Raises INVALID_ARGUMENT for a non-boolean commit,
--- CONFLICT when no transaction is open, PLUGIN_ERROR when the recording
--- finish fails.
function WriteRules.finishTransaction(
	state: TransactionState,
	commit: any,
	finishRecording: (recordingId: string, commit: boolean) -> boolean
): { name: string, committed: boolean }
	if type(commit) ~= "boolean" then
		fail(INVALID_ARGUMENT, "commit must be true (commit) or false (cancel)")
	end

	if not state.open then
		fail(CONFLICT, "no transaction is open")
	end

	local name = state.name
	local recordingId = state.recordingId

	state.open = false
	state.name = ""
	state.recordingId = ""
	state.startedAt = 0

	if not finishRecording(recordingId, commit) then
		fail(PLUGIN_ERROR, `FinishRecording ({if commit then "commit" else "cancel"}) failed for transaction '{name}'`)
	end

	return { name = name, committed = commit :: boolean }
end

--- Auto-cancel on transport disconnect: cancels any open transaction and
--- returns its name (nil when nothing was open). Never raises — the caller
--- logs the warning.
function WriteRules.disconnectTransaction(
	state: TransactionState,
	cancelRecording: (recordingId: string) -> boolean
): string?
	if not state.open then
		return nil
	end

	local name = state.name
	local recordingId = state.recordingId

	state.open = false
	state.name = ""
	state.recordingId = ""
	state.startedAt = 0

	cancelRecording(recordingId)

	return name
end

return WriteRules
