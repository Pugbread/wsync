--!strict
-- Pure protocol logic for the connect-time divergence flow (Design §7.1–§7.3
-- as amended; pinned wave-4 contracts): local-diff → `POST /compare` entry
-- mapping, bounded chunk planning (≤512 entries / ≤512 KiB per request), and
-- the upload state machine with exact receipt verification and the
-- single-restart rule.
--
-- Everything here is decision logic — no Roblox APIs, no requires — so the
-- module stays loadable under a standalone `luau` binary for behavior tests
-- (the Codec/Selector/WriteRules pattern). Instance access (tree lookups,
-- path building, ref minting) is injected through `Env` by the coordinator
-- (Core/Divergence).

local Protocol = {}

Protocol.LIMITS = {
	maxEntriesPerChunk = 512,
	maxBodyBytes = 512 * 1024,
	-- Reserved for the request envelope (submissionId, indexes, punctuation)
	-- when packing entries against maxBodyBytes
	envelopeReserveBytes = 256,
}

--- One `/compare` entry (pinned shape): `add` = disk/server has it, Studio
--- lacks it; `update` = differs; `remove` = Studio-only.
export type Entry = {
	ref: string, -- 32-char lowercase hex
	change: "add" | "update" | "remove",
	class: string,
	name: string,
	instancePath: string,
}

--- Injected environment: how the mapper touches refs and instances. In
--- production these close over Core's tree, Transport/Hex, Remote/Paths and
--- Helpers/generateRef; tests inject fakes.
export type Env = {
	encodeRef: (ref: any) -> string,
	mintRef: () -> any,
	isRef: (value: any) -> boolean,
	getInstance: (ref: any) -> any,
	instanceName: (instance: any) -> string?,
	instanceClass: (instance: any) -> string?,
	pathOf: (instance: any) -> string?,
}

export type MappedEntries = {
	entries: { Entry },
	-- Studio-only instances keyed by the ref hex minted for their `remove`
	-- entry: when the daemon later stages those removals, the replay sync
	-- frame carries these refs back and the coordinator resolves them here.
	pendingRemovals: { [string]: any },
	skipped: number,
}

--------------------------------------------------------------------------------
-- Diff → entry mapping
--------------------------------------------------------------------------------

local function joinPath(parentPath: string?, name: string): string
	if parentPath == nil or parentPath == "" then
		return name
	end

	return parentPath .. "/" .. name
end

--- Map a local `Types.Changes` diff onto wire entries (pinned):
--- snapshot-side additions → `add`, updates → `update`, instance-side
--- unknowns → `remove` (with a freshly minted ref registered in
--- `pendingRemovals`). Unresolvable records are skipped and counted — a
--- partial upload beats none, and the daemon re-freezes on every connect.
function Protocol.mapEntries(changes: any, env: Env): MappedEntries
	local entries: { Entry } = {}
	local pendingRemovals: { [string]: any } = {}
	local skipped = 0

	for _, snapshot in changes.additions :: { any } do
		local name = tostring(snapshot.name)
		local parentPath: string? = nil

		local parentInstance = snapshot.parent ~= nil and env.getInstance(snapshot.parent) or nil

		if parentInstance ~= nil then
			parentPath = env.pathOf(parentInstance)
		end

		table.insert(entries, {
			ref = env.encodeRef(snapshot.id),
			change = "add" :: "add",
			class = tostring(snapshot.class),
			name = name,
			instancePath = joinPath(parentPath, name),
		})
	end

	for _, snapshot in changes.updates :: { any } do
		local instance = env.getInstance(snapshot.id)
		local name = instance ~= nil and env.instanceName(instance) or nil

		if instance == nil or name == nil then
			-- An update for an instance the tree no longer resolves cannot be
			-- described; the daemon still owns the authoritative disk side
			skipped += 1
			continue
		end

		table.insert(entries, {
			ref = env.encodeRef(snapshot.id),
			change = "update" :: "update",
			class = env.instanceClass(instance) or "",
			name = name,
			instancePath = env.pathOf(instance) or name,
		})
	end

	for _, removal in changes.removals :: { any } do
		local instance = removal
		local ref: any = nil

		if env.isRef(removal) then
			-- Tracked ref removals never appear in the initial diff, but the
			-- Changes shape allows them; resolve through the tree.
			instance = env.getInstance(removal)
			ref = removal
		end

		local name = instance ~= nil and env.instanceName(instance) or nil

		if instance == nil or name == nil then
			skipped += 1
			continue
		end

		local refHex: string

		if ref ~= nil then
			refHex = env.encodeRef(ref)
		else
			-- Studio-only instance: unknown to both trees, so the plugin mints
			-- the ref that names it for the rest of the divergence flow
			refHex = env.encodeRef(env.mintRef())
			pendingRemovals[refHex] = instance
		end

		table.insert(entries, {
			ref = refHex,
			change = "remove" :: "remove",
			class = env.instanceClass(instance) or "",
			name = name,
			instancePath = env.pathOf(instance) or name,
		})
	end

	return {
		entries = entries,
		pendingRemovals = pendingRemovals,
		skipped = skipped,
	}
end

--------------------------------------------------------------------------------
-- Chunk planning
--------------------------------------------------------------------------------

--- Default serialized-size estimate for one entry (per-field JSON overhead
--- plus a margin for string escaping). The coordinator injects an exact
--- JSON-encoded measurement instead; this keeps the pure module testable.
function Protocol.estimateEntryBytes(entry: Entry): number
	return 64 + #entry.ref + #entry.change + 2 * (#entry.class + #entry.name + #entry.instancePath)
end

--- Pack entries into ordered chunks bounded by both limits. `measure` maps an
--- entry to its serialized byte size (defaults to the estimate above). An
--- entry that alone exceeds the byte budget still ships as a single-entry
--- chunk — the receipt path surfaces the daemon's verdict.
function Protocol.planChunks(entries: { Entry }, measure: ((entry: Entry) -> number)?): { { Entry } }
	local measureEntry = measure or Protocol.estimateEntryBytes
	local budget = Protocol.LIMITS.maxBodyBytes - Protocol.LIMITS.envelopeReserveBytes

	local chunks: { { Entry } } = {}
	local current: { Entry } = {}
	local currentBytes = 0

	for _, entry in entries do
		local bytes = measureEntry(entry)

		if #current > 0 and (#current >= Protocol.LIMITS.maxEntriesPerChunk or currentBytes + bytes > budget) then
			table.insert(chunks, current)
			current = {}
			currentBytes = 0
		end

		table.insert(current, entry)
		currentBytes += bytes
	end

	if #current > 0 then
		table.insert(chunks, current)
	end

	return chunks
end

--------------------------------------------------------------------------------
-- Upload state machine (chunked POST /compare with verified receipts)
--------------------------------------------------------------------------------

export type Upload = {
	chunks: { { Entry } },
	submissionId: string,
	nextIndex: number, -- 0-based index of the next chunk to send
	restarted: boolean,
	done: boolean,
	newId: () -> string,
}

export type Request = {
	submissionId: string,
	chunkIndex: number,
	finalChunk: boolean,
	restart: boolean?,
	entries: { Entry },
}

export type Verdict = {
	kind: "continue" | "committed" | "restart" | "failed",
	reason: string?,
	choiceId: string?,
	reviewId: string?,
	resolver: string?,
}

function Protocol.newUpload(chunks: { { Entry } }, newId: () -> string): Upload
	return {
		chunks = chunks,
		submissionId = newId(),
		nextIndex = 0,
		restarted = false,
		done = false,
		newId = newId,
	}
end

--- The next request body to send, or nil once the upload committed. Chunk 0
--- always carries `restart = true` (pinned) so a stale daemon-side upload or
--- pending set from a previous session is replaced, not collided with.
function Protocol.nextRequest(upload: Upload): Request?
	if upload.done or upload.nextIndex >= #upload.chunks then
		return nil
	end

	return {
		submissionId = upload.submissionId,
		chunkIndex = upload.nextIndex,
		finalChunk = upload.nextIndex == #upload.chunks - 1,
		restart = if upload.nextIndex == 0 then true else nil,
		entries = upload.chunks[upload.nextIndex + 1],
	}
end

local function mismatch(upload: Upload, reason: string): Verdict
	if not upload.restarted then
		-- Pinned rule: any receipt mismatch restarts the upload exactly once
		-- (fresh submission from chunk 0), then surfaces an error state.
		upload.restarted = true
		upload.nextIndex = 0
		upload.submissionId = upload.newId()

		return { kind = "restart", reason = reason, choiceId = nil, resolver = nil }
	end

	upload.done = true

	return { kind = "failed", reason = reason, choiceId = nil, resolver = nil }
end

--- Verify one receipt against the chunk just sent (pinned: verify every
--- receipt exactly). Advances the state machine on success; a mismatch
--- consumes the single restart or fails the upload for good.
function Protocol.applyReceipt(upload: Upload, receipt: any): Verdict
	local sentIndex = upload.nextIndex
	local isFinal = sentIndex == #upload.chunks - 1

	if type(receipt) ~= "table" then
		return mismatch(upload, "no receipt (request failed or returned a non-object body)")
	end

	if receipt.ok ~= true then
		return mismatch(upload, `receipt not ok: {tostring(receipt.error or receipt.ok)}`)
	end

	if receipt.acceptedChunk ~= sentIndex then
		return mismatch(upload, `receipt acceptedChunk {tostring(receipt.acceptedChunk)} ~= sent chunk {sentIndex}`)
	end

	if receipt.nextChunk ~= sentIndex + 1 then
		return mismatch(upload, `receipt nextChunk {tostring(receipt.nextChunk)} ~= expected {sentIndex + 1}`)
	end

	if receipt.committed ~= isFinal then
		return mismatch(
			upload,
			`receipt committed {tostring(receipt.committed)} ~= expected {tostring(isFinal)} for chunk {sentIndex}`
		)
	end

	if not isFinal then
		upload.nextIndex += 1
		return { kind = "continue", reason = nil, choiceId = nil, resolver = nil }
	end

	-- Final chunk. A committed receipt takes one of two shapes:
	--   * Studio-first (code scope, Design §7.0): the daemon already applied
	--     Studio → disk and reports `applied:true` (with an optional `reviewId`
	--     for the disk items it preserved). There is no decision to route — the
	--     caller resolves straight to Connected.
	--   * Decision-pending (full scope): the daemon minted a `choiceId` and a
	--     `resolver` ruling for who runs the decision UX.
	local reviewId = if type(receipt.reviewId) == "string" and receipt.reviewId ~= "" then receipt.reviewId else nil

	if receipt.applied == true or reviewId ~= nil then
		upload.done = true
		upload.nextIndex += 1

		return { kind = "committed", reason = nil, choiceId = nil, reviewId = reviewId, resolver = "studio-first" }
	end

	if type(receipt.choiceId) ~= "string" or receipt.choiceId == "" then
		return mismatch(upload, "final receipt committed without a choiceId or studio-first application")
	end

	upload.done = true
	upload.nextIndex += 1

	-- The daemon's ruling on who runs the decision UX. Anything other than an
	-- explicit "app" falls back to the plugin's own prompt flow — the surface
	-- that always exists.
	local resolver = if receipt.resolver == "app" then "app" else "plugin"

	return { kind = "committed", reason = nil, choiceId = receipt.choiceId, reviewId = nil, resolver = resolver }
end

return Protocol
