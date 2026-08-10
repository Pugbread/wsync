-- Daemon discovery (Design §9.4 / §3.3) and the desktop-broker Create
-- Project flow (Design §9.2 as amended; pinned wave-5 contract).
--
-- Discovery is the connection flow (user-ruled Ro-Sync model): `scan` probes
-- an ordered candidate list — the Settings `Host:Port` override first, then
-- `127.0.0.1:7978-7990` (deduplicated) — with `GET /hello` and matches the
-- responses against the open place:
--
--   * published place  — only daemons whose `gameId` equals `game.GameId`
--                        (preferring one whose `placeIds` include
--                        `game.PlaceId`; a claimless daemon never matches)
--   * unpublished place — the first responding daemon with no gameId claim
--
-- `AutoDiscover = false` restricts the scan to the Settings candidate alone
-- (pure manual mode). Candidate ordering, ranking and the connection state
-- machine live in `Policy` (require-free, standalone-testable); this module
-- owns the HTTP side and the Roblox globals. A gameId mismatch is NOT
-- resolved here: connecting to a non-matching daemon keeps Argon's existing
-- GameId/PlaceIds prompt behavior.
--
-- Create Project (pinned): when the scan finds no matching daemon, App
-- probes `127.0.0.1:7968-7971` for the desktop broker (`findBroker`). A
-- broker hello (`{broker: true, projectInit: true}`) surfaces the explicit
-- Create Project flow — never automatic. For a NEW game it is the two-step
-- confirm; when the hello's `games` list already names this GameId
-- (`brokerKnowsGame`) the button reads "Serve Project" and one click POSTs
-- straight away, because the broker's existing-gameId rule adopts and serves
-- the project it already has instead of creating a duplicate. Either way the
-- POST carries place *metadata only* (never a filesystem path) to
-- `/projects/init` (`createProject`), then `waitForDaemon` re-scans the
-- candidate list with bounded, cancellable retries until the project's
-- daemon answers (the broker stamps the project's gameId, so the strict
-- match holds).

local WSync = script:FindFirstAncestor("WSync")

local Promise = require(WSync.Packages.Promise)

local Types = require(WSync.Types)
local Transport = require(WSync.Transport)
local Config = require(WSync.Config)
local Broker = require(WSync.Broker)
local Log = require(WSync.Log)
local Hex = require(WSync.Transport.Hex)
local generateRef = require(WSync.Helpers.generateRef)

local Http = require(WSync.Transport.Http)

local Policy = require(script.Policy)

export type Match = {
	host: string,
	port: number,
	hello: Types.DaemonHello,
}

export type BrokerMatch = {
	host: string,
	port: number,
	hello: { [string]: any },
}

local Discovery = {}

Discovery.Policy = Policy
Discovery.PORT_MIN = Policy.PORT_MIN
Discovery.PORT_MAX = Policy.PORT_MAX
Discovery.BROKER_PORT_MIN = Broker.PORT_MIN
Discovery.BROKER_PORT_MAX = Broker.PORT_MAX

--- The ordered probe list for this scan, built from the live Settings values
--- (Policy owns the ordering and the dedup rules).
local function candidates(): { Policy.Candidate }
	return Policy.candidates({
		host = tostring(Config:get("Host")),
		port = tonumber(Config:get("Port")) or Config:getDefault("Port"),
		https = Config:get("Https") == true,
		autoDiscover = Config:get("AutoDiscover") == true,
	})
end

--- Probe the candidate list for a daemon matching the open place. Resolves a
--- `Match` or nil; never rejects. `shouldContinue` aborts the scan early
--- (e.g. on cancel).
function Discovery.scan(shouldContinue: (() -> boolean)?): Promise.TypedPromise<Match?>
	local function continuing(): boolean
		return not shouldContinue or shouldContinue()
	end

	return Promise.new(function(resolve)
		local place: Policy.PlaceIdentity = {
			gameId = game.GameId,
			placeId = game.PlaceId,
		}

		local best: Match? = nil
		local bestRank = Policy.MATCH_NONE

		for _, candidate in candidates() do
			if not continuing() then
				return resolve(nil)
			end

			local hello = Transport.hello(candidate.host, candidate.port, candidate.https):expect()

			if not hello then
				continue
			end

			Log.debug(`Discovery: daemon '{hello.project or hello.name}' on {candidate.host}:{candidate.port}`)

			local rank = Policy.rankHello(hello, place)

			if rank > bestRank then
				best = {
					host = candidate.host,
					port = candidate.port,
					hello = hello,
				}
				bestRank = rank
			end

			if rank >= Policy.MATCH_EXACT then
				-- Exact GameId + PlaceId match; no better candidate exists
				break
			end
		end

		return resolve(best)
	end)
end

--------------------------------------------------------------------------------
-- Desktop broker (Create Project)
--------------------------------------------------------------------------------

--- Whether the broker's hello `games` list names the open place's GameId —
--- the app already has a project for this game, so `/projects/init` will
--- adopt and serve it (the broker's existing-gameId rule) rather than create
--- anything. An unpublished place (GameId 0) is never "known": zero is not an
--- identity two places can safely share.
function Discovery.brokerKnowsGame(hello: { [string]: any }): boolean
	if game.GameId == 0 then
		return false
	end

	local games = hello.games

	if type(games) ~= "table" then
		return false
	end

	for _, id in games do
		if id == game.GameId then
			return true
		end
	end

	return false
end

--- Probe the broker port range. Resolves a `BrokerMatch` when a hello
--- answers `{broker: true, projectInit: true}`, nil otherwise; never
--- rejects.
function Discovery.findBroker(shouldContinue: (() -> boolean)?)
	local function continuing(): boolean
		return not shouldContinue or shouldContinue()
	end

	return Promise.new(function(resolve)
		for port = Broker.PORT_MIN, Broker.PORT_MAX do
			if not continuing() then
				return resolve(nil)
			end

			local hello: { [string]: any }? = Http.getJson(`http://{Policy.LOCAL_HOST}:{port}/hello`)
				:andThen(function(response)
					local ok, decoded = pcall(function()
						return response:decode()
					end)

					return if ok then decoded else nil
				end)
				:catch(function()
					return nil
				end)
				:expect()

			if hello ~= nil and Broker.isBrokerHello(hello) then
				Log.debug(`Discovery: project-init broker on port {port}`)

				return resolve({
					host = Policy.LOCAL_HOST,
					port = port,
					hello = hello :: { [string]: any },
				})
			end
		end

		return resolve(nil)
	end)
end

--- The place metadata for a `/projects/init` request — ids and names only,
--- never a filesystem path.
local function placeInfo(): Broker.PlaceInfo
	local creatorType: string? = nil
	local creatorId: number? = nil

	local ok, creatorTypeName, creatorIdValue = pcall(function()
		return game.CreatorType.Name, game.CreatorId
	end)

	if ok then
		creatorType = creatorTypeName
		creatorId = creatorIdValue
	end

	-- The published place name. Studio's edit-mode `game.Name` is the internal
	-- place-file name ("Place3"), not what the creator titled the place, so ask
	-- MarketplaceService — the one API that returns the real name in edit mode.
	-- It yields (web call) and is only reachable for a published place; fall
	-- back to `game.Name` for an unpublished one.
	local placeName: string? = if game.Name ~= "" then game.Name else nil

	if game.PlaceId ~= 0 then
		local infoOk, info = pcall(function()
			return game:GetService("MarketplaceService"):GetProductInfo(game.PlaceId)
		end)

		if infoOk and type(info) == "table" and type(info.Name) == "string" and info.Name ~= "" then
			placeName = info.Name
		end
	end

	return {
		gameId = game.GameId,
		placeId = game.PlaceId,
		gameName = nil,
		placeName = placeName,
		creatorType = creatorType,
		creatorId = creatorId,
	}
end

--- `POST /projects/init` to the broker with a fresh random request id and
--- the open place's metadata. Resolves `{slug, status}` (`Broker.InitOk`);
--- rejects with a human-readable message on any failure (unreachable broker
--- included).
function Discovery.createProject(broker: BrokerMatch)
	return Promise.new(function(resolve, reject)
		local body = Broker.shapeInitRequest(Hex.encode(generateRef()), placeInfo())

		if body == nil then
			return reject("could not shape the project request from this place's metadata")
		end

		local responseOk, response = pcall(function()
			return Http.postJson(`http://{broker.host}:{broker.port}/projects/init`, body):expect()
		end)

		if not responseOk then
			return reject("the WSync app stopped answering — is it still running?")
		end

		local decodedOk, decoded = pcall(function()
			return response:decode()
		end)

		if not decodedOk then
			return reject("the WSync app answered with an unreadable response")
		end

		local result, failure = Broker.parseInitResponse(decoded)

		if result == nil then
			return reject(failure or "the WSync app rejected the request")
		end

		Log.info(`Create Project: '{result.slug}' ({result.status})`)

		return resolve(result)
	end)
end

--- Re-scan the candidate list until a matching daemon appears: bounded
--- (`Broker.WAIT_ATTEMPTS` scans, `Broker.WAIT_DELAY_SECONDS` apart) and
--- cancellable via `shouldContinue`. Resolves the `Match` or nil when the
--- attempts run out or the wait is cancelled. `onAttempt` reports progress.
function Discovery.waitForDaemon(onAttempt: ((attempt: number, total: number) -> ())?, shouldContinue: (() -> boolean)?)
	local function continuing(): boolean
		return not shouldContinue or shouldContinue()
	end

	return Promise.new(function(resolve)
		for attempt = 1, Broker.WAIT_ATTEMPTS do
			if not continuing() then
				return resolve(nil)
			end

			if onAttempt then
				onAttempt(attempt, Broker.WAIT_ATTEMPTS)
			end

			local match = Discovery.scan(shouldContinue):expect()

			if match ~= nil then
				return resolve(match)
			end

			if attempt < Broker.WAIT_ATTEMPTS then
				task.wait(Broker.WAIT_DELAY_SECONDS)
			end
		end

		return resolve(nil)
	end)
end

return Discovery
