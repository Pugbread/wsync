--!strict
-- Pure connection-flow policy (Design §9.2/§9.4 as amended; user-ruled
-- Ro-Sync model): portless auto-discovery is the only primary flow, and the
-- Not Connected page is a status card driven by a small state machine.
--
-- This module owns every decision that does not need a socket or a Roblox
-- API:
--
--   * candidate ordering  — the Settings `Host:Port` override first, then
--                           `127.0.0.1:7978-7990` (deduplicated); with
--                           `AutoDiscover = false` the Settings candidate is
--                           the whole list (pure manual mode)
--   * matching            — strict-GameId ranking of `/hello` responses
--                           against the open place
--   * the state machine   — idle → scanning → found/create-ready/no-broker,
--                           the two-step Create Project confirm (Confirm is
--                           the only transition that emits a create request),
--                           bounded daemon wait, retry/cancel with a
--                           generation counter that aborts stale async work
--   * presentation        — the status line, dot color and action button for
--                           every state (Design §9.2 vocabulary)
--
-- Discovery orchestrates the HTTP side; App executes the effects this
-- machine returns. No requires, no Roblox APIs — loadable under a standalone
-- `luau` binary for behavior tests (the Broker/WriteRules pattern).

local Policy = {}

Policy.LOCAL_HOST = "127.0.0.1"
Policy.PORT_MIN = 7978
Policy.PORT_MAX = 7990

-- Match ranks, higher wins; the scan may stop early at MATCH_EXACT.
Policy.MATCH_NONE = 0
Policy.MATCH_WEAK = 1 -- claimless daemon for an unpublished place, or a placeIds claim that excludes this place
Policy.MATCH_GAME = 2 -- gameId matches, no placeIds claim
Policy.MATCH_EXACT = 3 -- gameId and placeId both match

export type Candidate = {
	host: string,
	port: number,
	https: boolean,
}

export type Settings = {
	host: string,
	port: number,
	https: boolean?,
	autoDiscover: boolean,
}

export type PlaceIdentity = {
	gameId: number,
	placeId: number,
}

export type HelloClaim = {
	gameId: number?,
	placeIds: { number }?,
	[string]: any,
}

--- Hosts the local discovery range already covers — used to deduplicate the
--- Settings candidate against the `127.0.0.1` port scan.
function Policy.isLoopback(host: string): boolean
	local hostname = string.lower(host)

	return hostname == "localhost" or hostname == "127.0.0.1" or hostname == "0.0.0.0"
end

--- The ordered `/hello` probe list: the Settings `Host:Port` override first
--- (it participates in discovery, so pointing it at a remote daemon needs no
--- extra UI), then the local range — minus the Settings address when it
--- already falls inside it. `autoDiscover = false` restricts the scan to the
--- Settings candidate alone.
function Policy.candidates(settings: Settings): { Candidate }
	local list: { Candidate } = {
		{
			host = settings.host,
			port = settings.port,
			https = settings.https == true,
		},
	}

	if not settings.autoDiscover then
		return list
	end

	local coveredPort = if Policy.isLoopback(settings.host) then settings.port else nil

	for port = Policy.PORT_MIN, Policy.PORT_MAX do
		if port ~= coveredPort then
			table.insert(list, {
				host = Policy.LOCAL_HOST,
				port = port,
				https = false,
			})
		end
	end

	return list
end

--- Rank a daemon's `/hello` claim against the open place (strict GameId):
--- for a published place only daemons claiming our GameId qualify — a
--- claimless daemon never matches — with `placeIds` refining among them; an
--- unpublished place takes the first claimless daemon.
function Policy.rankHello(hello: HelloClaim, place: PlaceIdentity): number
	if place.gameId ~= 0 then
		if hello.gameId ~= place.gameId then
			return Policy.MATCH_NONE
		end

		local placeIds = hello.placeIds

		if placeIds and #placeIds > 0 then
			return if table.find(placeIds, place.placeId) then Policy.MATCH_EXACT else Policy.MATCH_WEAK
		end

		return Policy.MATCH_GAME
	else
		if not hello.gameId or hello.gameId == 0 then
			return Policy.MATCH_WEAK
		end

		return Policy.MATCH_NONE
	end
end

--------------------------------------------------------------------------------
-- Connection state machine
--------------------------------------------------------------------------------

export type StateName =
	"idle"
	| "scanning"
	| "found"
	| "create-ready"
	| "create-confirm"
	| "creating"
	| "waiting-daemon"
	| "no-broker"
	| "error"

export type State = {
	name: StateName,
	--- Bumped by every user-initiated transition; async completions carry the
	--- generation they started under and are ignored when it went stale
	generation: number,
	-- found: the daemon to connect to
	host: string?,
	port: number?,
	project: string?,
	-- create flow: the broker match the request goes to (opaque to the machine)
	broker: any?,
	-- create flow: the broker already has a project for this game (its hello
	-- `games` list names the open place's GameId), so the POST adopts and
	-- serves it instead of creating anything — no confirm ceremony needed
	known: boolean?,
	-- waiting-daemon: bounded re-scan progress
	attempt: number?,
	total: number?,
	-- error: what went wrong
	message: string?,
}

export type EventKind =
	-- User intents
	"start" -- idle: the action button or AutoConnect
	| "cancel" -- scanning/found → idle; waiting-daemon → create-ready
	| "retry" -- no-broker/error: back to a fresh scan
	| "create" -- create-ready: confirm step for a new project; straight to the POST for a known one
	| "confirm" -- create-confirm: POST the request (step 2 — the only NEW-project emitter)
	| "back" -- create-confirm: return to create-ready (Escape does the same)
	| "reset" -- any: App dropped the flow (disconnect)
	-- Async completions (require a matching generation)
	| "scan-found"
	| "scan-none-broker"
	| "scan-none-no-broker"
	| "create-ok"
	| "create-error"
	| "wait-progress"
	| "wait-found"
	| "wait-exhausted"

export type Event = {
	kind: EventKind,
	generation: number?,
	host: string?,
	port: number?,
	project: string?,
	broker: any?,
	known: boolean?,
	attempt: number?,
	total: number?,
	message: string?,
}

--- What App must do after a transition: run the candidate scan, hand the
--- resolved address to Core, POST `/projects/init`, or start the bounded
--- daemon wait.
export type Effect = "scan" | "connect" | "create" | "wait"

local WAIT_EXHAUSTED_MESSAGE = "The daemon did not appear — check the WSync app, then retry"
local CREATE_FAILED_MESSAGE = "the WSync app could not create the project"

function Policy.initialState(): State
	return {
		name = "idle",
		generation = 0,
	}
end

local function make(name: StateName, generation: number, extra: { [string]: any }?): State
	local state: { [string]: any } = {
		name = name,
		generation = generation,
	}

	if extra then
		for key, value in extra do
			state[key] = value
		end
	end

	return (state :: any) :: State
end

--- Advance the machine. Returns the next state plus the effect App must run.
--- An event that does not apply — wrong source state, or an async completion
--- whose generation went stale — returns the *same* state table (identity)
--- and no effect, so callers can cheaply skip no-op updates.
function Policy.step(state: State, event: Event): (State, Effect?)
	local kind = event.kind

	-- User intents ----------------------------------------------------------

	if kind == "start" then
		if state.name == "idle" then
			return make("scanning", state.generation + 1), "scan"
		end
	elseif kind == "cancel" then
		if state.name == "scanning" or state.name == "found" then
			return make("idle", state.generation + 1), nil
		elseif state.name == "waiting-daemon" then
			return make("create-ready", state.generation + 1, { broker = state.broker, known = state.known }), nil
		end
	elseif kind == "retry" then
		if state.name == "no-broker" or state.name == "error" then
			return make("scanning", state.generation + 1), "scan"
		end
	elseif kind == "create" then
		if state.name == "create-ready" then
			-- A known game skips the confirm ceremony: the POST is an
			-- idempotent adopt-and-serve of the project the broker already
			-- has, not a creation (the broker's existing-gameId rule)
			if state.known then
				return make("creating", state.generation + 1, { broker = state.broker, known = true }), "create"
			end

			return make("create-confirm", state.generation, { broker = state.broker }), nil
		end
	elseif kind == "confirm" then
		if state.name == "create-confirm" then
			-- The one and only transition that emits a NEW-project request
			return make("creating", state.generation + 1, { broker = state.broker }), "create"
		end
	elseif kind == "back" then
		if state.name == "create-confirm" then
			return make("create-ready", state.generation, { broker = state.broker }), nil
		end
	elseif kind == "reset" then
		return make("idle", state.generation + 1), nil
	else
		-- Async completions -------------------------------------------------

		if event.generation ~= state.generation then
			return state, nil
		end

		if kind == "scan-found" then
			if state.name == "scanning" then
				return make("found", state.generation, {
					host = event.host,
					port = event.port,
					project = event.project,
				}),
					"connect"
			end
		elseif kind == "scan-none-broker" then
			if state.name == "scanning" then
				return make("create-ready", state.generation, {
					broker = event.broker,
					known = event.known == true,
				}),
					nil
			end
		elseif kind == "scan-none-no-broker" then
			if state.name == "scanning" then
				return make("no-broker", state.generation), nil
			end
		elseif kind == "create-ok" then
			if state.name == "creating" then
				return make("waiting-daemon", state.generation, { broker = state.broker, known = state.known }), "wait"
			end
		elseif kind == "create-error" then
			if state.name == "creating" then
				return make("error", state.generation, {
					message = event.message or CREATE_FAILED_MESSAGE,
				}),
					nil
			end
		elseif kind == "wait-progress" then
			if state.name == "waiting-daemon" then
				return make("waiting-daemon", state.generation, {
					broker = state.broker,
					attempt = event.attempt,
					total = event.total,
				}),
					nil
			end
		elseif kind == "wait-found" then
			if state.name == "waiting-daemon" then
				return make("found", state.generation, {
					host = event.host,
					port = event.port,
					project = event.project,
				}),
					"connect"
			end
		elseif kind == "wait-exhausted" then
			if state.name == "waiting-daemon" then
				return make("error", state.generation, { message = WAIT_EXHAUSTED_MESSAGE }), nil
			end
		end
	end

	return state, nil
end

--------------------------------------------------------------------------------
-- Presentation
--------------------------------------------------------------------------------

export type Dot = "neutral" | "green" | "amber" | "red"

export type Env = {
	placeName: string,
	autoDiscover: boolean,
	host: string,
	port: number,
}

export type Descriptor = {
	status: string,
	dot: Dot,
	--- Label of the single action button; nil while a broker request is in
	--- flight (nothing to click)
	action: string?,
	--- Whether the confirm step's secondary Back button is shown
	back: boolean,
}

--- The status line, dot color and button label for a state (Design §9.2
--- vocabulary). `env` carries the place name and the Settings values so the
--- idle/scanning lines stay truthful in pure manual mode.
function Policy.describe(state: State, env: Env): Descriptor
	local name = state.name

	if name == "idle" then
		return {
			status = if env.autoDiscover
				then `Auto-discover {Policy.LOCAL_HOST}:{Policy.PORT_MIN}–{Policy.PORT_MAX}`
				else `Connect to {env.host}:{env.port}`,
			dot = "neutral" :: Dot,
			action = "Connect",
			back = false,
		}
	elseif name == "scanning" then
		return {
			status = if env.autoDiscover
				then `Scanning localhost {Policy.PORT_MIN}–{Policy.PORT_MAX}…`
				else `Probing {env.host}:{env.port}…`,
			dot = "neutral" :: Dot,
			action = "Cancel",
			back = false,
		}
	elseif name == "found" then
		local project = state.project or "project"
		local where = if state.host == nil or Policy.isLoopback(state.host :: string)
			then `port {state.port}`
			else `{state.host}:{state.port}`

		return {
			status = `Found '{project}' on {where} — connecting…`,
			dot = "green" :: Dot,
			action = "Cancel",
			back = false,
		}
	elseif name == "create-ready" then
		-- A known game is the app's own project with its daemon down: the
		-- button serves it, and saying "Create" here would read as a duplicate
		if state.known then
			local placeName = if env.placeName ~= "" then env.placeName else "This place"

			return {
				status = `'{placeName}' is already in your WSync Projects · daemon not running`,
				dot = "amber" :: Dot,
				action = "Serve Project",
				back = false,
			}
		end

		return {
			status = "New Studio project · ready to create",
			dot = "amber" :: Dot,
			action = "Create Project",
			back = false,
		}
	elseif name == "create-confirm" then
		local placeName = if env.placeName ~= "" then env.placeName else "this place"

		return {
			status = `Create '{placeName}' in your WSync Projects folder?`,
			dot = "amber" :: Dot,
			action = "Confirm",
			back = true,
		}
	elseif name == "creating" then
		return {
			status = if state.known then "Starting the project daemon…" else "Creating project…",
			dot = "amber" :: Dot,
			action = nil,
			back = false,
		}
	elseif name == "waiting-daemon" then
		return {
			status = if state.attempt
				then `Project ready · waiting for the daemon… ({state.attempt}/{state.total})`
				else "Project ready · waiting for the daemon…",
			dot = "amber" :: Dot,
			action = "Cancel",
			back = false,
		}
	elseif name == "no-broker" then
		return {
			status = "No WSync daemon for this place. Open the WSync desktop app "
				.. "and choose a Projects folder (Settings) to enable project creation.",
			dot = "red" :: Dot,
			action = "Retry",
			back = false,
		}
	end

	return {
		status = state.message or "Connection error",
		dot = "red" :: Dot,
		action = "Retry",
		back = false,
	}
end

return Policy
