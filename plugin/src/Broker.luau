--!strict
-- Pure rules for the desktop-broker Create Project flow (Design §9.2 as
-- amended; pinned wave-5 contract). The broker is the desktop app's helper
-- listening on `127.0.0.1:7968-7971`; its `GET /hello` answers
-- `{broker: true, projectInit: true}`, and `POST /projects/init` creates a
-- project from place *metadata only* — never a filesystem path.
--
-- This module owns everything about that exchange that does not need a
-- socket: hello recognition, init-request shaping, init-response parsing,
-- and the bounded daemon-wait schedule. Discovery orchestrates the HTTP
-- side. No requires, no Roblox APIs — loadable under a standalone `luau`
-- binary for behavior tests (the WriteRules pattern).

local Broker = {}

Broker.PORT_MIN = 7968
Broker.PORT_MAX = 7971

-- After a successful init, re-scan the daemon range until the new project's
-- daemon appears: bounded, cancellable (Discovery.waitForDaemon).
Broker.WAIT_ATTEMPTS = 15
Broker.WAIT_DELAY_SECONDS = 2

export type PlaceInfo = {
	gameId: number,
	placeId: number,
	gameName: string?,
	placeName: string?,
	creatorType: string?,
	creatorId: number?,
}

export type InitOk = {
	slug: string,
	status: string,
}

--- Whether a decoded `/hello` body identifies a project-init-capable broker.
function Broker.isBrokerHello(value: any): boolean
	return type(value) == "table" and value.broker == true and value.projectInit == true
end

local function isCountingNumber(value: any): boolean
	return type(value) == "number" and value == value and value >= 0 and value < math.huge and value % 1 == 0
end

--- Shape the `POST /projects/init` body: the request id plus place metadata,
--- with empty or unknown fields omitted rather than sent blank. Returns nil
--- when the inputs cannot form a valid request (malformed request id or
--- ids) — the caller treats that as a programming error, not a wire error.
function Broker.shapeInitRequest(requestId: any, place: PlaceInfo): { [string]: any }?
	if type(requestId) ~= "string" or #requestId < 16 or requestId:find("[^0-9a-f]") ~= nil then
		return nil
	end

	if not isCountingNumber(place.gameId) or not isCountingNumber(place.placeId) then
		return nil
	end

	local body: { [string]: any } = {
		requestId = requestId,
		gameId = place.gameId,
		placeId = place.placeId,
	}

	if type(place.gameName) == "string" and place.gameName ~= "" then
		body.gameName = place.gameName
	end

	if type(place.placeName) == "string" and place.placeName ~= "" then
		body.placeName = place.placeName
	end

	if
		(place.creatorType == "User" or place.creatorType == "Group")
		and isCountingNumber(place.creatorId)
		and (place.creatorId :: number) > 0
	then
		body.creatorType = place.creatorType
		body.creatorId = place.creatorId
	end

	return body
end

--- Parse a `/projects/init` response body. Returns the `{slug, status}` on
--- `{ok: true, slug, status}`, or nil plus a human-readable reason.
function Broker.parseInitResponse(value: any): (InitOk?, string?)
	if type(value) ~= "table" then
		return nil, "the broker answered with a malformed response"
	end

	if value.ok ~= true then
		local message = value.error

		if type(message) == "table" then
			message = message.message or message.code
		end

		return nil, if type(message) == "string" and message ~= "" then message else "the broker rejected the request"
	end

	if type(value.slug) ~= "string" or value.slug == "" then
		return nil, "the broker did not name the created project"
	end

	return {
		slug = value.slug :: string,
		status = if type(value.status) == "string" and value.status ~= "" then value.status else "created",
	},
		nil
end

return Broker
