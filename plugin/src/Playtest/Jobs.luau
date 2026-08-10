--!strict
-- Playtest job and runtime-context registry (playtest.json; pinned wave-6
-- contracts): the pure state machine behind `playtest_start` / `status` /
-- `contexts` / `stop` and the coordinator's hello handling.
--
-- Invariants owned here:
--   * at most one job is ever active (starting/running/stopping) — a second
--     start is refused until the previous generation ended;
--   * every job carries the private generation token minted at start; a
--     hello that names any other job — including a previous generation that
--     is still tearing down — is rejected, so stale runtime contexts can
--     never satisfy a later job;
--   * context names are assigned in hello order: the PlayServer runtime is
--     `server`, PlayClient runtimes are `client:1` … `client:N`, stable for
--     the life of the generation (a reconnecting runtime keeps its name);
--   * the surface status is the pinned four-value enum — `idle`, `starting`,
--     `running`, `stopping` — with the richer terminal detail (`completed`/
--     `failed`/`stopped`, error text) kept on the job record;
--   * ended jobs are pruned to a bounded history so the registry never grows
--     without limit.
--
-- Pure: explicit state, injected clock, no requires, no Roblox APIs —
-- loadable under a standalone `luau` binary for behavior tests. The impure
-- coordinator (Playtest/init) owns connections, auth verification and
-- StudioTestService; by the time a hello reaches `registerHello` its auth has
-- already been checked against the job's token.

local Jobs = {}

Jobs.LIMITS = {
	maxHistoricalJobs = 20,
	maxPlayers = 8,
}

export type JobStatus = "starting" | "running" | "stopping" | "completed" | "failed" | "stopped"

export type Job = {
	id: string,
	token: string,
	mode: "play" | "run" | "multiplayer",
	players: number,
	status: JobStatus,
	createdAt: number,
	startedAt: number?,
	endedAt: number?,
	error: string?,
	stopRequested: boolean,
	-- Attached by the coordinator for `playtest_run_start` jobs; opaque here
	run: any?,
}

export type Context = {
	name: string,
	runtimeId: string,
	jobId: string,
	role: "server" | "client",
	alive: boolean,
	readyAt: number,
}

export type Registry = {
	jobs: { [string]: Job },
	latestJobId: string?,
	contextsById: { [string]: Context },
	contextsByName: { [string]: Context },
	nextClientIndex: number,
}

function Jobs.newRegistry(): Registry
	return {
		jobs = {},
		latestJobId = nil,
		contextsById = {},
		contextsByName = {},
		nextClientIndex = 1,
	}
end

local function isActiveStatus(status: JobStatus): boolean
	return status == "starting" or status == "running" or status == "stopping"
end

Jobs.isActiveStatus = isActiveStatus

--- The single active job (starting/running/stopping), or nil.
function Jobs.activeJob(registry: Registry): Job?
	for _, job in registry.jobs do
		if isActiveStatus(job.status) then
			return job
		end
	end

	return nil
end

--- Resolve a job by explicit id, falling back to the latest generation.
--- Returns nil when the id is unknown (or no job ever started).
function Jobs.resolveJob(registry: Registry, jobId: string?): Job?
	local id = jobId or registry.latestJobId

	if id == nil then
		return nil
	end

	return registry.jobs[id]
end

--- Prune ended jobs beyond the bounded history (oldest first). The active
--- job and the latest generation always survive.
local function pruneHistory(registry: Registry)
	local ended: { Job } = {}

	for id, job in registry.jobs do
		if not isActiveStatus(job.status) and id ~= registry.latestJobId then
			table.insert(ended, job)
		end
	end

	if #ended <= Jobs.LIMITS.maxHistoricalJobs then
		return
	end

	table.sort(ended, function(a, b)
		return a.createdAt < b.createdAt
	end)

	for index = 1, #ended - Jobs.LIMITS.maxHistoricalJobs do
		registry.jobs[ended[index].id] = nil
	end
end

export type StartConfig = {
	jobId: string,
	token: string,
	mode: "play" | "run" | "multiplayer",
	players: number,
	now: number,
}

--- Begin a new generation. Returns the job, or nil plus the id of the job
--- that is still active. Starting resets the context registry — contexts of
--- every previous generation are invalidated wholesale.
function Jobs.start(registry: Registry, config: StartConfig): (Job?, string?)
	local active = Jobs.activeJob(registry)

	if active ~= nil then
		return nil, active.id
	end

	local job: Job = {
		id = config.jobId,
		token = config.token,
		mode = config.mode,
		players = config.players,
		status = "starting",
		createdAt = config.now,
		startedAt = nil,
		endedAt = nil,
		error = nil,
		stopRequested = false,
		run = nil,
	}

	registry.jobs[config.jobId] = job
	registry.latestJobId = config.jobId
	registry.contextsById = {}
	registry.contextsByName = {}
	registry.nextClientIndex = 1

	pruneHistory(registry)

	return job, nil
end

--- Remove a job that failed before its launch was dispatched (run
--- configuration rejected), restoring the previous latest generation.
function Jobs.abandonStart(registry: Registry, jobId: string, previousLatest: string?)
	registry.jobs[jobId] = nil

	if registry.latestJobId == jobId then
		registry.latestJobId = previousLatest
	end
end

--- The launch call was dispatched; the playtest is now booting/active.
function Jobs.markRunning(registry: Registry, jobId: string, now: number)
	local job = registry.jobs[jobId]

	if job ~= nil and job.status == "starting" then
		job.status = "running"
		job.startedAt = now
	end
end

--- The launch call returned: the playtest ended. `ok` is whether the launch
--- API resolved without error; a requested stop lands as `stopped` either
--- way, a clean end as `completed`, an errored one as `failed`.
function Jobs.markEnded(registry: Registry, jobId: string, ok: boolean, detail: string?, now: number)
	local job = registry.jobs[jobId]

	if job == nil or not isActiveStatus(job.status) then
		return
	end

	job.endedAt = now

	if job.stopRequested then
		job.status = "stopped"

		if not ok and detail ~= nil then
			job.error = detail
		end
	elseif ok then
		job.status = "completed"
	else
		job.status = "failed"
		job.error = detail
	end
end

--- Enter the stopping state. Returns "ok", or why not ("stopping" when a stop
--- is already in progress, "inactive" when there is nothing to stop).
function Jobs.beginStop(registry: Registry, jobId: string): "ok" | "stopping" | "inactive"
	local job = registry.jobs[jobId]

	if job == nil then
		return "inactive"
	end

	if job.status == "stopping" then
		return "stopping"
	end

	if not isActiveStatus(job.status) then
		return "inactive"
	end

	job.stopRequested = true
	job.status = "stopping"

	return "ok"
end

--- A retryable stop attempt failed: restore the job so a later stop (or the
--- playtest ending on its own) still works. Terminal states are untouched.
function Jobs.abortStop(registry: Registry, jobId: string, restoreStatus: JobStatus)
	local job = registry.jobs[jobId]

	if job ~= nil and job.status == "stopping" then
		job.stopRequested = false
		job.status = restoreStatus
	end
end

--- A forced stop ran out of options: the generation must not stay active
--- forever, so it is marked failed with the collected detail.
function Jobs.failStop(registry: Registry, jobId: string, detail: string, now: number)
	local job = registry.jobs[jobId]

	if job ~= nil and isActiveStatus(job.status) then
		job.status = "failed"
		job.error = detail
		job.endedAt = now
	end
end

export type HelloInfo = {
	jobId: string,
	runtimeId: string,
	role: "server" | "client",
	now: number,
}

--- Register an authenticated runtime hello. Returns the context, or nil when
--- the hello is stale: it names an unknown job, a job that is no longer
--- active, or a generation other than the latest. A runtime re-helloing
--- (reconnect) keeps its assigned name.
function Jobs.registerHello(registry: Registry, hello: HelloInfo): Context?
	local job = registry.jobs[hello.jobId]

	if job == nil or not isActiveStatus(job.status) or registry.latestJobId ~= hello.jobId then
		return nil
	end

	local existing = registry.contextsById[hello.runtimeId]

	if existing ~= nil then
		existing.alive = true
		existing.readyAt = hello.now

		return existing
	end

	local name: string

	if hello.role == "server" and registry.contextsByName.server == nil then
		name = "server"
	else
		repeat
			name = `client:{registry.nextClientIndex}`
			registry.nextClientIndex += 1
		until registry.contextsByName[name] == nil
	end

	local context: Context = {
		name = name,
		runtimeId = hello.runtimeId,
		jobId = hello.jobId,
		role = hello.role,
		alive = true,
		readyAt = hello.now,
	}

	registry.contextsById[hello.runtimeId] = context
	registry.contextsByName[name] = context

	return context
end

--- Drop a context whose connection died. Its name stays reserved for the
--- generation (the runtime may reconnect and re-hello under the same id).
function Jobs.dropContext(registry: Registry, runtimeId: string)
	local context = registry.contextsById[runtimeId]

	if context ~= nil then
		context.alive = false
	end
end

--- The live context for a name, restricted to the active generation.
function Jobs.liveContext(registry: Registry, name: string): Context?
	local context = registry.contextsByName[name]

	if context == nil or not context.alive then
		return nil
	end

	local job = registry.jobs[context.jobId]

	if job == nil or not isActiveStatus(job.status) then
		return nil
	end

	return context
end

--- Sorted live contexts of one job, in the pinned `{name, ready}` shape plus
--- the role (additive).
function Jobs.contextsOf(registry: Registry, jobId: string): { { name: string, ready: boolean, role: string } }
	local out: { { name: string, ready: boolean, role: string } } = {}

	for _, context in registry.contextsByName do
		if context.jobId == jobId and context.alive then
			table.insert(out, {
				name = context.name,
				ready = true,
				role = context.role :: string,
			})
		end
	end

	table.sort(out, function(a, b)
		return a.name < b.name
	end)

	return out
end

--- The count of live client contexts of one job (drives `awaitClients`).
function Jobs.liveClientCount(registry: Registry, jobId: string): number
	local count = 0

	for _, context in registry.contextsByName do
		if context.jobId == jobId and context.alive and context.role == "client" then
			count += 1
		end
	end

	return count
end

--- The pinned surface status: active states pass through, everything else —
--- terminal jobs and no job at all — reads `idle`.
function Jobs.statusValue(job: Job?): "idle" | "starting" | "running" | "stopping"
	if job == nil then
		return "idle"
	end

	local status = job.status

	if status == "starting" or status == "running" or status == "stopping" then
		return status
	end

	return "idle"
end

return Jobs
