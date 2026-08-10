-- Connect-time divergence coordinator (Design §7.1–§7.3 as amended; pinned
-- wave-4 contracts). Owns the plugin side of the daemon's divergence
-- endpoints — all plain HTTP JSON, so they work identically over the
-- WebSocket and long-poll transports:
--
--   * `POST /compare` — the chunked local-diff upload with verified receipts
--     (state machine in ./Protocol; one restart on mismatch, then error);
--   * `GET  /choice`  — pending-decision status (the long-poll wait mechanism
--     and the WebSocket backstop poll);
--   * `POST /choice`  — decision submission (`disk`/`studio`/`cancel`).
--
-- It also carries the pending-removal side registry: Studio-only instances
-- are named by plugin-minted refs in the upload, and when the daemon stages
-- their removal the replay sync frame carries those refs back —
-- `primeRemovals` registers the instances in the tree just in time for the
-- write processor to resolve them. Until then the instances stay untracked,
-- so an undecided or cancelled divergence never leaks transfer-local refs
-- into live syncback.

local HttpService = game:GetService("HttpService")

local WSync = script:FindFirstAncestor("WSync")

local Promise = require(WSync.Packages.Promise)

local Log = require(WSync.Log)
local Util = require(WSync.Util)
local Types = require(WSync.Types)
local SyncScope = require(WSync.SyncScope)

local Hex = require(WSync.Transport.Hex)
local Http = require(WSync.Transport.Http)
local Paths = require(WSync.Remote.Paths)
local generateRef = require(WSync.Helpers.generateRef)

local Meta = require(script.Parent.Meta)
local Protocol = require(script.Protocol)

export type UploadOutcome = {
	choiceId: string,
	resolver: "app" | "plugin",
}

export type ChoiceStatus = {
	pending: boolean,
	choiceId: string?,
}

local Divergence = {}
Divergence.__index = Divergence

Divergence.Protocol = Protocol

function Divergence.new(tree: any, host: string, port: number, https: boolean?)
	local self = setmetatable({}, Divergence)

	self.tree = tree
	self.baseUrl = `{https and "https" or "http"}://{host}:{port}`
	self.pendingRemovals = {} :: { [string]: Instance }
	self.pendingRefs = {} :: { [Instance]: string }

	return self
end

function Divergence:__setPendingRemovals(pendingRemovals: { [string]: Instance })
	self.pendingRemovals = pendingRemovals
	self.pendingRefs = {}

	for refHex, instance in pendingRemovals do
		self.pendingRefs[instance] = refHex
	end
end

--------------------------------------------------------------------------------
-- Env plumbing (the pure Protocol module never touches Roblox APIs itself)
--------------------------------------------------------------------------------

function Divergence:__buildEnv(): Protocol.Env
	local tree = self.tree

	return {
		encodeRef = function(ref: any)
			return Hex.encode(ref)
		end,
		mintRef = function()
			return generateRef()
		end,
		isRef = function(value: any)
			return typeof(value) == "buffer"
		end,
		getInstance = function(ref: any)
			return tree:getInstance(ref)
		end,
		instanceName = function(instance: any): string?
			local ok, name = pcall(function()
				return (instance :: Instance).Name
			end)

			return if ok then name else nil
		end,
		instanceClass = function(instance: any): string?
			local ok, class = pcall(function()
				return (instance :: Instance).ClassName
			end)

			return if ok then class else nil
		end,
		pathOf = function(instance: any): string?
			local ok, path = pcall(function()
				-- Parenthesized: pathOf's second return (rootedness) is not
				-- part of this env contract
				return (Paths.pathOf(instance :: Instance))
			end)

			return if ok then path else nil
		end,
	}
end

--- Exact serialized size of one entry (JSON), for the chunk packer.
local function measureEntry(entry: Protocol.Entry): number
	local ok, encoded = pcall(function()
		return HttpService:JSONEncode(entry)
	end)

	if ok then
		-- +1 for the separating comma inside the entries array
		return #encoded + 1
	end

	return Protocol.estimateEntryBytes(entry)
end

--------------------------------------------------------------------------------
-- POST /compare — the chunked upload
--------------------------------------------------------------------------------

--- Upload the local diff. Resolves an UploadOutcome (`choiceId` + the
--- daemon's `resolver` ruling), or nil when nothing was uploadable (every
--- record skipped — treated like the empty-diff fast path). Rejects with a
--- descriptive message after the single mismatch-restart is exhausted.
function Divergence:upload(changes: Types.Changes)
	return Promise.new(function(resolve, reject)
		local mapped = Protocol.mapEntries(changes, self:__buildEnv())

		if mapped.skipped > 0 then
			Log.debug(`Divergence upload skipped {mapped.skipped} unresolvable diff records`)
		end

		-- Code scope (Design §7.0): only the four allowlisted classes enter
		-- the upload. The daemon drops out-of-scope entries too — this is the
		-- plugin-side half of that contract, and it keeps dropped Studio-only
		-- instances out of the pending-removal registry.
		if SyncScope.isCode() then
			local filtered, dropped = SyncScope.filterMappedEntries(mapped)
			mapped = filtered

			if dropped > 0 then
				Log.debug(`Divergence upload dropped {dropped} out-of-scope diff entries (code scope)`)
			end
		end

		if #mapped.entries == 0 then
			return resolve(nil)
		end

		self:__setPendingRemovals(mapped.pendingRemovals)

		local chunks = Protocol.planChunks(mapped.entries, measureEntry)
		local upload = Protocol.newUpload(chunks, Util.generateGUID)
		local url = `{self.baseUrl}/compare`

		Log.trace(`Uploading divergence set: {#mapped.entries} entries in {#chunks} chunks`)

		while true do
			local request = Protocol.nextRequest(upload)

			if not request then
				-- Committed without a final verdict — unreachable by
				-- construction, guarded for safety
				return reject("Divergence upload ended without a committed receipt")
			end

			local receipt: any = nil
			local ok, response = Http.postJson(url, request :: any):await()

			if ok then
				local decodeOk, decoded = pcall(function()
					return response:decode()
				end)

				if decodeOk then
					receipt = decoded
				end
			else
				Log.debug("Compare chunk request failed:", response)
			end

			local verdict = Protocol.applyReceipt(upload, receipt)

			if verdict.kind == "committed" then
				Log.stage(
					`Connect: compare upload committed — {#mapped.entries} entries`
						.. ` in {#chunks} chunks, receipts verified`
				)

				return resolve({
					choiceId = verdict.choiceId,
					resolver = verdict.resolver,
				} :: UploadOutcome)
			elseif verdict.kind == "restart" then
				-- Same entries (and the same minted refs), fresh submission id
				Log.debug("Restarting the divergence upload once:", verdict.reason)
			elseif verdict.kind == "failed" then
				return reject(`Divergence upload failed after restart: {tostring(verdict.reason)}`)
			end

			-- "continue" and "restart" both loop into the next request
		end
	end)
end

--------------------------------------------------------------------------------
-- GET /choice + POST /choice
--------------------------------------------------------------------------------

--- Pending-decision status (`{pending, choiceId?}`).
function Divergence:fetchStatus()
	return Http.getJson(`{self.baseUrl}/choice`):andThen(function(response)
		local ok, decoded = pcall(function()
			return response:decode()
		end)

		if not ok or type(decoded) ~= "table" then
			return Promise.reject("Malformed /choice status response")
		end

		return {
			pending = decoded.pending == true,
			choiceId = if type(decoded.choiceId) == "string" then decoded.choiceId else nil,
		} :: ChoiceStatus
	end)
end

--- Submit the decision. Resolves "ok" when accepted, "resolved" when another
--- client already decided this choiceId (the daemon answers 409 `resolved` —
--- resuming is correct either way); rejects on real transport failures.
function Divergence:submitChoice(choiceId: string?, choice: "disk" | "studio" | "cancel", mode: string?)
	local body: { [string]: any } = { choice = choice }

	-- The pinned cancel body is `{choice:"cancel"}`; the choiceId is included
	-- whenever known so the daemon can match the pending set exactly.
	if choiceId ~= nil then
		body.choiceId = choiceId
	end

	if mode ~= nil then
		body.mode = mode
	end

	return Http.postJson(`{self.baseUrl}/choice`, body):andThen(function()
		return "ok"
	end, function(err)
		local message = tostring(err)

		if message:find("resolved", 1, true) then
			Log.debug("Divergence choice was already resolved elsewhere")
			return "resolved"
		end

		return Promise.reject(err)
	end)
end

--------------------------------------------------------------------------------
-- Pending-removal resolution (staged `missing-on-disk` replay)
--------------------------------------------------------------------------------

--- Called before every sync-frame application: any removal ref that names a
--- pending Studio-only instance gets that instance registered in the tree
--- just in time, so the write processor's removal path resolves it. Refs
--- stay registered here (not in the tree) until the daemon actually stages
--- the removal — see the module comment.
function Divergence:primeRemovals(changes: Types.Changes)
	if next(self.pendingRemovals) == nil then
		return
	end

	for _, removal in changes.removals do
		if typeof(removal) ~= "buffer" then
			continue
		end

		local refHex = Hex.encode(removal)
		local instance = self.pendingRemovals[refHex]

		if instance == nil then
			continue
		end

		self.pendingRemovals[refHex] = nil
		self.pendingRefs[instance] = nil

		-- The instance may have been deleted or tracked since the upload
		if instance.Parent ~= nil and not self.tree:getId(instance) and not self.tree:getInstance(removal) then
			self.tree:insertInstance(instance, removal, Meta.new())
		end
	end
end

--- Look up a pending Studio-only instance by its minted ref hex (the bulk
--- ops resolve `read_subtree`/`source_read` targets through this).
function Divergence:resolvePendingRemoval(refHex: string): Instance?
	return self.pendingRemovals[refHex]
end

--- Reverse lookup: the minted ref hex the upload named this Studio-only
--- instance with, so a bulk subtree walk reuses it instead of minting anew.
function Divergence:pendingRefFor(instance: Instance): string?
	return self.pendingRefs[instance]
end

return Divergence
