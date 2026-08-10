local ScriptEditorService = game:GetService("ScriptEditorService")

local WSync = script:FindFirstAncestor("WSync")

local Promise = require(WSync.Packages.Promise)

local Transport = require(WSync.Transport)
local Config = require(WSync.Config)
local Log = require(WSync.Log)
local Util = require(WSync.Util)
local Types = require(WSync.Types)
local Watcher = require(WSync.Watcher)
local Executor = require(WSync.Executor)
local SemVer = require(WSync.SemVer)
local SyncScope = require(WSync.SyncScope)
local manifest = require(WSync.manifest)
local equals = require(WSync.Helpers.equals)
local Http = require(WSync.Transport.Http)

-- Remote-op modules wired into the sync engine (no cycle: the handler chain
-- never requires back into Core/init): History owns the transaction state
-- that gates per-batch waypoints and threshold prompts during daemon-driven
-- bulk application; Bulk needs the session bridge for read_subtree /
-- source_read (Design §7.4-A).
local History = require(WSync.Remote.Handlers.History)
local Bulk = require(WSync.Remote.Handlers.Bulk)

local Processor = require(script.Processor)
local ReadProcessor = require(script.Processor.Read)
local Changes = require(script.Changes)
local Tree = require(script.Tree)
local Error = require(script.Error)
local Divergence = require(script.Divergence)
local Review = require(script.Divergence.Review)

local SYNCBACK_RATE = 0.5
local SYNCBACK_DEBOUNCE = 0.1
-- The gentle divergence-decision poll (pinned: 2 s, bounded to the pending
-- state) — the long-poll transport's resolution mechanism and the WebSocket
-- transport's backstop when no `choice-made` event arrives
local DECISION_POLL = 2
local DECISION_POLL_FAILURE_LIMIT = 5

local Core = {
	Status = {
		Disconnected = 0,
		Connecting = 1,
		Connected = 2,
		Disconnecting = 3,
	},
}
Core.__index = Core

function Core.new(host: string?, port: string?)
	local self = setmetatable({}, Core)

	self.project = nil
	self.rootInstances = {}
	self.connections = {}
	self.status = 0
	self.lastSync = os.clock()

	self.host = host or Config:get("Host")
	self.port = port or Config:get("Port")

	self.tree = Tree.new()
	-- Transport and processor are created in `run` — picking the transport
	-- requires an async `/hello` probe (Transport.detect)
	self.transport = nil :: Transport.Transport?
	self.processor = nil
	self.watcher = Watcher.new()
	self.executor = Executor.new()
	-- Divergence coordinator (Design §7 as amended) — created in `run` when a
	-- wsync/1 daemon reports a non-empty connect-time diff
	self.divergence = nil
	-- Consume-once verify skip: the sync-frame replay right after a "disk"
	-- decision is a pre-approved application and must not re-prompt
	self.__skipNextVerify = false
	-- Passive disk-review indicator state (Design §7.0, code scope): fed by
	-- `disk-review` events and the lazy `GET /review` re-poll on (re)connect
	self.reviewState = Review.initial()

	self.__prompt = function(_message: string, _changes: Types.Changes?, _tree: any): boolean
		return true
	end
	self.__ready = function(_project: Types.Project) end
	self.__sync = function(_kind: Types.MessageKind, _data: any) end
	self.__transportStatus = function(_status: Types.TransportStatus) end
	self.__divergenceStatus = function(_event: Types.DivergenceStatus) end
	self.__diskReview = function(_message: string?) end

	self:__handleOpenInEditor(Config:get("OpenInEditor"))

	-- Watch for `OpenInEditor setting
	table.insert(
		self.connections,
		Config:onChanged("OpenInEditor", function(enabled)
			self:__handleOpenInEditor(enabled)
		end)
	)

	-- Watch for `TwoWaySync` setting
	table.insert(
		self.connections,
		Config:onChanged("TwoWaySync", function(enabled)
			if enabled then
				-- Never start mid-connect: a pending divergence decision
				-- pauses watching/syncback until it resolves (Design §7.3) —
				-- `run` starts the watcher once Connected
				if self.status == Core.Status.Connected then
					self.watcher:start(self.rootInstances)
				end
			else
				self.watcher:stop()
			end
		end)
	)

	return self
end

function Core:run(): Promise.Promise
	return Promise.new(function(_, reject)
		self.status = Core.Status.Connecting

		-- A fresh connect must never inherit the previous session's scope —
		-- Config resolves the scope-dependent retired settings through it
		-- (Design §7.0)
		SyncScope.reset()

		local initialSyncPriority = Config:get("InitialSyncPriority")

		Log.trace("Detecting transport..")

		local transport = Transport.detect(self.host, self.port, Config:get("Https")):expect()
		self.transport = transport
		self.processor = Processor.new(self.tree, transport)

		transport:onStatus(function(status)
			-- Lazy disk-review re-check (Design §7.0): a review resolved in
			-- the app while the socket was down must still clear the
			-- Connected-page indicator, so re-poll on every reconnect
			if status.kind == "connected" and self.status == Core.Status.Connected then
				self:__fetchReview()
			end

			self.__transportStatus(status)
		end)

		Log.trace("Fetching server details..")

		local project = transport:fetchDetails():expect()
		self.project = project

		-- The hello (probe or WS handshake) has resolved by now: stamp the
		-- session scope so every scope-dependent path — Config's retired
		-- keys, the read processor's allowlist, bulk subtree filtering —
		-- agrees for the rest of the session (Design §7.0)
		SyncScope.set(transport.scope)

		Log.stage(
			`Connect: hello ok — {self.host}:{self.port}, {transport.kind} transport, `
				.. `protocol {transport.protocol or 0}, scope {SyncScope.get()}`
		)

		self:__verifyProject(project, true):expect()

		Log.trace("Subscribing to the server queue..")

		transport:subscribe():expect()

		Log.trace("Getting initial snapshot..")

		local snapshot = transport:getSnapshot():expect()

		Log.trace("Initializing processor..")

		local changes = self.processor:init(snapshot, initialSyncPriority)

		Log.stage(
			`Connect: initial diff — {#changes.additions} additions, `
				.. `{#changes.updates} updates, {#changes.removals} removals`
		)

		if self.status ~= Core.Status.Connecting then
			return reject(Error.new(Error.Terminated))
		end

		for i, id in project.rootRefs do
			self.rootInstances[i] = self.tree:getInstance(id)
		end

		-- Session bridge for the daemon's bulk pull ops (Design §7.4-A)
		self:__attachBulkSession()

		if initialSyncPriority ~= "None" then
			-- A wsync/1 daemon owns divergence resolution (Design §7 as
			-- amended by §7.0): the local diff goes up via POST /compare and
			-- the daemon drives application — on code scope immediately and
			-- without any decision. The legacy local apply/reverse flow
			-- remains only for stock Argon daemons (no /hello identity, so
			-- no /compare either).
			if (transport.protocol or 0) >= 1 then
				self:__resolveDivergence(changes, initialSyncPriority):expect()
			else
				Log.trace("Processing initial snapshot..")

				if initialSyncPriority == "Server" then
					self:__verifyChanges(changes, true):expect()
					self.processor.write:applyChanges(changes, true)
				elseif changes:total() > 0 then
					local reversed = self.processor:reverseChanges(changes)
					transport:write(reversed):expect()
				end
			end
		end

		if self.status ~= Core.Status.Connecting then
			return reject(Error.new(Error.Terminated))
		end

		if Config:get("TwoWaySync") then
			self.watcher:start(self.rootInstances)
		end

		self.status = Core.Status.Connected
		self.__ready(project)

		Log.stage(`Connect: connected to '{project.name}' — live sync running`)

		-- Code scope: check for a review set left over from this or an
		-- earlier session (lazy poll — after this, only reconnects and
		-- `disk-review` events refresh the indicator)
		self:__fetchReview()

		Log.trace("Starting sync loops..")

		self:__startSyncbackLoop():catch(function(err)
			return reject(err)
		end)

		return self:__startSyncLoop():expect()
	end)
end

function Core:stop()
	Log.trace("Stopping WSync core..")

	self.status = Core.Status.Disconnecting

	self.watcher:stop()
	Bulk.detach()

	-- Back to the full-scope default so nothing scope-dependent leaks past
	-- this session (Design §7.0); after the watcher stops, so no late event
	-- is ever classified under the wrong scope
	SyncScope.reset()

	local transport = self.transport

	if transport then
		if transport.isSubscribed then
			task.spawn(function()
				transport:unsubscribe():catch(function(err)
					Log.debug("Failed to unsubscribe from the server", err)
				end)
			end)
		else
			transport:destroy()
		end
	end

	Util.clean(self.connections)
end

function Core:onPrompt(callback: (message: string, changes: Types.Changes?, tree: any) -> boolean)
	self.__prompt = function(message, changes)
		if self.status == Core.Status.Disconnecting then
			return false
		end

		return callback(message, changes, self.tree)
	end
end

function Core:onReady(callback: (project: Types.Project) -> ())
	self.__ready = function(project)
		if self.status == Core.Status.Disconnecting then
			return
		end

		return callback(project)
	end
end

function Core:onSync(callback: (kind: Types.MessageKind, data: any) -> ())
	self.__sync = function(kind, data)
		if self.status == Core.Status.Disconnecting then
			return
		end

		return callback(kind, data)
	end
end

function Core:onTransportStatus(callback: (status: Types.TransportStatus) -> ())
	self.__transportStatus = function(status)
		if self.status == Core.Status.Disconnecting then
			return
		end

		return callback(status)
	end
end

--- Connect-time divergence decision state (Design §7.3 as amended): `waiting`
--- while a wsync/1 daemon routed the decision to the desktop app (the event
--- carries a `cancel` that submits `{choice:"cancel"}`), `resolved` when the
--- decision landed and normal operation resumes. Full scope only — a
--- code-scope connect never waits (Design §7.0).
function Core:onDivergence(callback: (event: Types.DivergenceStatus) -> ())
	self.__divergenceStatus = function(event)
		if self.status == Core.Status.Disconnecting then
			return
		end

		return callback(event)
	end
end

--- Passive disk-review indicator (Design §7.0, code scope): called with the
--- Connected-page line ("N disk changes await review in the WSync app") when
--- the pending-review count changes, and with nil when the review clears.
function Core:onDiskReview(callback: (message: string?) -> ())
	self.__diskReview = function(message)
		if self.status == Core.Status.Disconnecting then
			return
		end

		return callback(message)
	end
end

function Core.wasExitGraceful(err: Error.Error)
	return err == Error.GameId or err == Error.PlaceIds or err == Error.TooManyChanges or err == Error.Terminated
end

--- A non-retryable server shutdown (protocol/capability mismatch etc.) must
--- halt every reconnect loop, including the app-level `AutoReconnect` timer
function Core.wasFatal(err: Error.Error)
	return err == Error.ServerShutdown
end

-- Internal functions

function Core:__verifyProject(project: Types.Project, initial: boolean?): Promise.Promise
	if not self:__shouldPrompt(initial) then
		return Promise.resolve()
	end

	if project.gameId and project.gameId ~= game.GameId then
		local err = Error.new(Error.GameId, game.GameId, project.gameId)

		if not self.__prompt(err.message) then
			return Promise.reject(err)
		end
	end

	if #project.placeIds > 0 and not table.find(project.placeIds, game.PlaceId) then
		local err = Error.new(Error.PlaceIds, game.PlaceId, project.placeIds)

		if not self.__prompt(err.message) then
			return Promise.reject(err)
		end
	end

	-- Version gate (Design §5.1, surface-scoped): a daemon that answered the
	-- `/hello` probe with `protocol >= 1` is accepted by protocol; the
	-- version it reports through `fetchDetails` is informational only (in
	-- `compat_argon` mode the msgpack `/details` deliberately pins an
	-- Argon-compatible constant so stock Argon plugins pass *their* gate).
	-- Strict plugin<->server semver still applies to daemons with no
	-- `/hello` — the transport carries no `protocol` for those.
	local transport = self.transport
	local acceptedByProtocol = transport ~= nil and (transport.protocol or 0) >= 1

	if
		not acceptedByProtocol
		and not SemVer.parse(project.version):isCompatible(SemVer.parse(manifest.package.version))
	then
		local err = Error.new(Error.Version, manifest.package.version, project.version)

		return Promise.reject(err)
	end

	return Promise.resolve()
end

function Core:__verifyChanges(changes: Types.Changes, initial: boolean?): Promise.Promise
	if not self:__shouldPrompt(initial) then
		return Promise.resolve()
	end

	if Changes.Total(changes) > Config:get("ChangesThreshold") then
		local err = Error.new(Error.TooManyChanges, #changes.additions, #changes.updates, #changes.removals)

		if not self.__prompt(err.message, changes) then
			return Promise.reject(err)
		end
	end

	return Promise.resolve()
end

function Core:__startSyncLoop()
	return Promise.new(function(resolve, reject)
		while self.status == Core.Status.Connected do
			local message = self.transport:read():expect() :: Types.Message?

			if not message then
				continue
			end

			local kind = next(message) :: Types.MessageKind
			local data = message[kind] :: any

			Log.trace("Received message:", kind)

			if kind == "Disconnect" then
				if data.fatal then
					reject(Error.new(Error.ServerShutdown, data.message))
				else
					reject(Error.new(Error.Disconnected, data.message))
				end
			else
				self:__dispatchMessage(kind, data)
			end
		end

		resolve()
	end)
end

--- Handle one non-Disconnect server message. Shared by the live sync loop
--- and the divergence-wait stash replay so both apply identical semantics.
function Core:__dispatchMessage(kind: Types.MessageKind, data: any)
	if kind == "SyncChanges" then
		-- Daemon-driven bulk application is a pre-approved decision: inside a
		-- transaction bracket (WS `transaction_begin` .. frames .. finish) or
		-- as the replay right after a "disk" decision (consume-once flag on
		-- the long-poll path), the threshold prompt must not re-ask what the
		-- divergence choice already answered.
		if self.__skipNextVerify then
			self.__skipNextVerify = false
		elseif not History.isTransactionOpen() then
			self:__verifyChanges(data):expect()
		end

		local divergence = self.divergence

		if divergence then
			-- Staged Studio-only removals arrive as plugin-minted refs; put
			-- their instances into the tree just in time to resolve
			divergence:primeRemovals(data)
		end

		self.processor.write:applyChanges(data)
		self.lastSync = os.clock()

		self.__sync(kind, data)
	elseif kind == "SyncDetails" then
		self:__verifyProject(data):expect()

		if not equals(self.project.rootRefs, data.rootRefs) then
			self.rootInstances = {}

			for i, id in data.rootRefs do
				self.rootInstances[i] = self.tree:getInstance(id)
			end

			self.watcher:restart(self.rootInstances)
		end

		self.project = data

		self.__sync(kind, data)
	elseif kind == "ExecuteCode" then
		self.executor:execute(data.code)
	elseif kind == "DispatchRequest" then
		-- Queue-ordered request op (transaction_begin/transaction_finish):
		-- executing it here — after every earlier queued sync frame applied —
		-- preserves the daemon's bracket ordering (pinned wave-4 contract)
		local transport = self.transport

		if transport and transport.performRequest then
			transport:performRequest(data)
		end
	elseif kind == "DivergenceEvent" then
		if data.topic == "disk-review" then
			-- Studio-first (Design §7.0): the daemon finished (or updated) a
			-- Studio → disk auto-apply and reports how many disk changes
			-- await review in the app. Passive — only the Connected-page
			-- indicator moves.
			self:__updateReview(Review.onEvent(self.reviewState, data.total))
		else
			-- choice-* topics are only meaningful inside a decision wait;
			-- quietly dropped elsewhere
			Log.trace("Ignoring a divergence event outside a decision wait:", data.topic)
		end
	else
		local err = Error.new(Error.UnknownEvent, kind, data)
		Log.warn(err)
	end
end

-- Divergence flow (Design §7.1–§7.4 as amended; pinned wave-4 contracts)

--- Wire the live tree (and the pending-divergence registry) into the bulk
--- transfer ops for this session.
function Core:__attachBulkSession()
	local tree = self.tree

	Bulk.attach({
		getInstanceForRef = function(ref: Types.Ref)
			return tree:getInstance(ref)
		end,
		getRefForInstance = function(instance: Instance)
			return tree:getId(instance)
		end,
		resolvePendingRemoval = function(refHex: string)
			local divergence = self.divergence
			return if divergence then divergence:resolvePendingRemoval(refHex) else nil
		end,
		pendingRefForInstance = function(instance: Instance)
			local divergence = self.divergence
			return if divergence then divergence:pendingRefFor(instance) else nil
		end,
		-- The read-processor snapshotting pair, so read_subtree reuses the
		-- exact live-sync property semantics (Design §7.4-A)
		snapshotProperties = ReadProcessor.snapshotProperties,
		shouldSyncProperties = ReadProcessor.shouldSyncProperties,
	})
end

--- Connect-time divergence resolution against a wsync/1 daemon: a non-empty
--- diff is uploaded via the chunked `POST /compare` (never applied locally —
--- the daemon drives application). Empty diff → no `/compare` at all
--- (unchanged fast path).
---
--- Code scope (Design §7.0 — the Studio-first ruling): the commit receipt IS
--- the resolution. There is no decision — the daemon auto-applies Studio →
--- disk (fenced, backed up) and later surfaces disk-only/preserved content
--- as a passive `disk-review`. No waiting page, no prompt, no
--- InitialSyncPriority flow, no `reverseChanges`: this resolves straight to
--- Connected and the live loops start while the daemon pulls what it needs
--- via `read_subtree`/`source_read`.
---
--- Full scope keeps the existing resolver flow: the final receipt's
--- `resolver` ruling routes the decision UX (app wait or local prompt).
function Core:__resolveDivergence(changes: Types.Changes, initialSyncPriority: string)
	return Promise.new(function(resolve, reject)
		if Changes.Total(changes) == 0 then
			return resolve()
		end

		local divergence = Divergence.new(self.tree, self.host, self.port, Config:get("Https"))
		self.divergence = divergence

		Log.trace("Uploading the divergence set..")

		local uploaded, outcome = divergence:upload(changes):await()

		if not uploaded then
			Log.warn("Connect: divergence upload failed:", outcome)
			return reject(Error.new(Error.DivergenceFailed, outcome))
		end

		if self.status ~= Core.Status.Connecting then
			return reject(Error.new(Error.Terminated))
		end

		if not outcome then
			-- Nothing was uploadable (all records skipped) — treated like the
			-- clean fast path
			return resolve()
		end

		if SyncScope.isCode() or outcome.resolver == "studio-first" then
			-- Studio wins automatically — proceed straight to Connected. The
			-- daemon already applied Studio → disk; disk-only/preserved items
			-- surface later as a passive `disk-review`.
			return resolve()
		end

		if outcome.resolver == "app" then
			Log.trace("Waiting for the divergence decision in the app..")
			self:__awaitAppDecision(divergence, outcome.choiceId):expect()
		else
			Log.trace("Resolving the divergence with the local prompt flow..")
			self:__resolveLocally(divergence, outcome.choiceId, changes, initialSyncPriority):expect()
		end

		return resolve()
	end)
end

--- The plugin-resolver path (pinned): the existing local prompt flow —
--- `InitialSyncPriority`/`DisplayPrompts`/`ChangesThreshold` exactly as today
--- — translated into the daemon decision. Server priority (or an accepted
--- prompt) declares disk the source of truth (`{choice:"disk", mode:"all"}`);
--- Client priority declares Studio (`{choice:"studio"}`); a declined prompt
--- cancels and keeps today's graceful-exit outcome. The legacy local
--- `applyChanges`/`reverseChanges` paths never run here — the daemon is the
--- applier, and double-application is the failure mode this flow prevents.
function Core:__resolveLocally(divergence, choiceId: string, changes: Types.Changes, initialSyncPriority: string)
	return Promise.new(function(resolve, reject)
		if initialSyncPriority == "Server" then
			local accepted, verifyErr = self:__verifyChanges(changes, true):await()

			if not accepted then
				-- Cancel/None → `{choice:"cancel"}`, then today's graceful
				-- exit (the connection tears down exactly as before)
				local submitted, err = divergence:submitChoice(choiceId, "cancel"):await()

				if not submitted then
					Log.debug("Failed to submit the cancel decision:", err)
				end

				return reject(verifyErr)
			end

			local submitted, err = divergence:submitChoice(choiceId, "disk", "all"):await()

			if not submitted then
				return reject(Error.new(Error.DivergenceFailed, err))
			end

			-- The replay arriving next is the approved application
			self.__skipNextVerify = true
		else
			-- Client priority: Keep Studio. The daemon pulls the divergent
			-- subtrees through the bulk ops; the legacy reverse-write is
			-- exactly what must not also run.
			local submitted, err = divergence:submitChoice(choiceId, "studio"):await()

			if not submitted then
				return reject(Error.new(Error.DivergenceFailed, err))
			end
		end

		return resolve()
	end)
end

--- The app-resolver path (pinned): keep the connection alive and idle —
--- watcher and syncback have not started yet, so the pending decision pauses
--- the plugin entirely — until the daemon reports the decision. On the
--- WebSocket transport the `choice-made` event is the primary signal (the
--- read queue is consumed here, with non-divergence messages stashed and
--- replayed after resolution); a gentle `GET /choice` poll backstops lost
--- events. The long-poll transport polls `GET /choice` exclusively — its
--- server-side queue simply buffers frames until the sync loop starts.
function Core:__awaitAppDecision(divergence, choiceId: string)
	return Promise.new(function(resolve, reject)
		local transport = self.transport
		local waiting = true
		local resolvedChoice: string? = nil
		local stash = {}

		self.__divergenceStatus({
			kind = "waiting",
			cancel = function()
				-- POST the cancel; resolution then arrives like any other
				-- decision (choice-made event or the status poll)
				task.spawn(function()
					local ok, err = divergence:submitChoice(choiceId, "cancel"):await()

					if not ok then
						Log.debug("Failed to cancel the divergence decision:", err)
					end
				end)
			end,
		})

		local function halt(err): any
			waiting = false
			return reject(err)
		end

		if transport.kind == "WebSocket" then
			-- Backstop poll: wakes the blocked reader when the pending set
			-- disappeared without a decision event (daemon restart, socket
			-- drop invalidation). A short grace period lets an in-flight
			-- ordered event win first.
			-- Drains itself through the `waiting` flag once the wait ends
			task.spawn(function()
				while waiting do
					task.wait(DECISION_POLL)

					if not waiting then
						break
					end

					local ok, status = divergence:fetchStatus():await()

					if ok and waiting and (status.pending ~= true or status.choiceId ~= choiceId) then
						task.wait(DECISION_POLL)

						if waiting and transport.push then
							transport:push({ DivergenceEvent = { topic = "choice-vanished" } } :: any)
						end

						break
					end
				end
			end)

			while waiting do
				if self.status ~= Core.Status.Connecting then
					return halt(Error.new(Error.Terminated))
				end

				local ok, message = transport:read():await()

				if not ok then
					return halt(message)
				end

				if message == nil then
					-- Transport destroyed (stop during the wait)
					return halt(Error.new(Error.Terminated))
				end

				local kind = next(message)
				local data = (message :: any)[kind]

				if kind == "DivergenceEvent" then
					if data.topic == "choice-made" then
						if data.choiceId == nil or data.choiceId == choiceId then
							resolvedChoice = data.choice
							waiting = false
						else
							return halt(Error.new(Error.Disconnected, "the pending divergence decision was replaced"))
						end
					elseif data.topic == "choice-vanished" then
						return halt(
							Error.new(
								Error.Disconnected,
								"the pending divergence decision was lost — reconnect to compare again"
							)
						)
					end
					-- choice-needed: the broadcast echo of our own upload
				elseif kind == "DispatchRequest" then
					-- Queue-ordered transaction ops keep executing in order.
					-- A transaction_begin is also proof the daemon started
					-- applying a disk decision — the authoritative signal
					-- when the choice-made event was lost.
					if transport.performRequest then
						transport:performRequest(data)
					end

					if data.op == "transaction_begin" then
						resolvedChoice = resolvedChoice or "disk"
						waiting = false
					end
				elseif kind == "Disconnect" then
					if data.fatal then
						return halt(Error.new(Error.ServerShutdown, data.message))
					else
						return halt(Error.new(Error.Disconnected, data.message))
					end
				else
					table.insert(stash, message)
				end
			end

			-- The poll thread drains itself: every cycle re-checks `waiting`,
			-- so it exits within one interval (cancelling it mid-await could
			-- resume a dead thread from the promise library instead)
		else
			-- Long-poll transport (pinned): GET /choice every 2 s until the
			-- pending state clears
			local failures = 0

			while waiting do
				if self.status ~= Core.Status.Connecting then
					return halt(Error.new(Error.Terminated))
				end

				local ok, status = divergence:fetchStatus():await()

				if ok then
					failures = 0

					if status.pending ~= true then
						-- Resolved; GET /choice cannot carry which way, so
						-- the stashless long-poll flow just resumes
						waiting = false
						break
					elseif status.choiceId ~= choiceId then
						return halt(Error.new(Error.Disconnected, "the pending divergence decision was replaced"))
					end
				else
					failures += 1

					if failures >= DECISION_POLL_FAILURE_LIMIT then
						return halt(Error.new(Error.DivergenceFailed, status))
					end
				end

				task.wait(DECISION_POLL)
			end

			-- The replay after a "disk" decision must not re-prompt; consumed
			-- by the first sync frame (a cancel/studio decision produces no
			-- replay and the flag simply covers one ordinary frame)
			self.__skipNextVerify = true
		end

		if resolvedChoice == "disk" then
			self.__skipNextVerify = true
		end

		self.__divergenceStatus({ kind = "resolved", choice = resolvedChoice })

		self:__applyStash(stash, resolvedChoice)

		return resolve()
	end)
end

--- Replay the messages stashed while a decision was pending. Pre-decision
--- sync frames only apply when disk won — after "Keep Studio" or a cancel
--- they would overwrite the state the user just chose to keep, so they are
--- dropped (LiveHydrate re-resolves anything they carried on demand).
function Core:__applyStash(stash: { Types.Message }, choice: string?)
	if #stash == 0 then
		return
	end

	local dropped = 0

	for _, message in stash do
		local kind = next(message) :: Types.MessageKind
		local data = (message :: any)[kind]

		if kind == "SyncChanges" and choice ~= "disk" then
			dropped += 1
			continue
		end

		local ok, err = pcall(function()
			self:__dispatchMessage(kind, data)
		end)

		if not ok then
			Log.warn("Failed to apply a message stashed during the divergence wait:", err)
		end
	end

	if dropped > 0 then
		Log.debug(`Dropped {dropped} pre-decision sync frames (the decision kept Studio state)`)
	end
end

-- Disk-review indicator (Design §7.0, code scope) — pure transitions in
-- Divergence/Review; this side owns the transport and the app callback.

--- Adopt a new review state and re-publish the indicator line when the
--- pending count actually moved.
function Core:__updateReview(state)
	if state.total == self.reviewState.total then
		self.reviewState = state
		return
	end

	self.reviewState = state
	self.__diskReview(Review.message(state))
end

--- The lazy `GET /review` poll: only called on (re)connect — between those
--- moments `disk-review` events keep the count fresh. A failed or
--- unparseable poll keeps the current state (Review.onPoll), so a daemon
--- without the endpoint simply never shows the indicator.
function Core:__fetchReview()
	if not SyncScope.isCode() then
		return
	end

	local url = `{Config:get("Https") and "https" or "http"}://{self.host}:{self.port}/review`

	task.spawn(function()
		local ok, response = Http.getJson(url):await()
		local total: number? = nil

		if ok then
			local decodeOk, decoded = pcall(function()
				return response:decode()
			end)

			if decodeOk then
				total = Review.parsePoll(decoded)
			end
		else
			Log.debug("Disk-review poll failed:", response)
		end

		if self.status == Core.Status.Disconnecting then
			return
		end

		self:__updateReview(Review.onPoll(self.reviewState, total))
	end)
end

function Core:__startSyncbackLoop()
	local aggregateChanges = Changes.new()

	return Promise.new(function(resolve)
		task.spawn(function()
			while self.status == Core.Status.Connected do
				task.wait(SYNCBACK_RATE)

				if aggregateChanges:isEmpty() then
					continue
				end

				local changes = aggregateChanges
				aggregateChanges = Changes.new()

				self.transport:write(changes):catch(function(err)
					Log.warn("Failed to write changes to the server:", err)
				end)

				self.__sync("SyncbackChanges", changes)
			end
		end)

		while self.status == Core.Status.Connected do
			local event = self.watcher:listen() :: Types.WatcherEvent

			if os.clock() - self.lastSync <= SYNCBACK_DEBOUNCE then
				continue
			end

			local kind = event.kind
			local changes = Changes.new()

			Log.trace("Received event:", kind)

			if kind == "Add" then
				local snapshot = self.processor.read:onAdd(event.instance)

				if snapshot then
					changes:add(snapshot)
				end
			elseif kind == "Change" then
				local snapshot = self.processor.read:onChange(event.instance, event.property)

				if snapshot then
					changes:update(snapshot)
				end
			elseif kind == "Remove" then
				local id = self.processor.read:onRemove(event.instance)

				if id then
					changes:remove(id)
				end
			end

			aggregateChanges:join(changes, true)
		end

		resolve()
	end)
end

function Core:__handleOpenInEditor(enabled: boolean)
	if not enabled then
		local connection = self.connections["openInEditor"]

		if connection then
			connection:Disconnect()
			self.connections["openInEditor"] = nil
		end

		return
	end

	self.connections["openInEditor"] = ScriptEditorService.TextDocumentDidOpen:Connect(function(document)
		if self.status ~= Core.Status.Connected then
			return
		end

		if document:IsCommandBar() then
			Log.trace("Document is a command bar, ignoring")
			return
		end

		local id = self.tree:getId(document:GetScript())

		if not id then
			Log.trace("Document is not synced by WSync, ignoring")
			return
		end

		local line = document:GetSelectionStart()

		self.transport
			:open(id, line)
			:andThen(function()
				document:CloseAsync()
			end)
			:catch(function(err)
				Log.debug("Failed to open document in editor:", err)
			end)
	end)
end

function Core:__cleanConnection(id: string)
	local connection = self.connections[id]

	if connection then
		connection:Disconnect()
		self.connections[id] = nil
	end
end

function Core:__shouldPrompt(initial: boolean?): boolean
	local mode = Config:get("DisplayPrompts")

	if mode == "Always" then
		return true
	elseif mode == "Initial" and initial then
		return true
	else
		return false
	end
end

return Core
