--!strict
-- Disk-review indicator state (Design §7.0 — the Studio-first ruling). After
-- a code-scope daemon auto-applies Studio → disk, the disk-only items and the
-- preserved disk copies of `differs` items form a passive review set owned by
-- the desktop app. The plugin's whole involvement is one amber line on the
-- Connected page ("N disk changes await review in the WSync app"), fed by:
--
--   * `{"type":"event","topic":"disk-review","reviewId","total",...}` frames
--     — the daemon's push, authoritative whenever one arrives;
--   * a lazy `GET /review` poll — re-checked only on (re)connect, so a review
--     resolved in the app while the socket was down still clears the line.
--
-- The indicator clears when a later poll or event shows none; a failed or
-- unparseable poll keeps the current state (never flicker on transport
-- noise). Pure state logic, deliberately require-free (the Protocol/Policy
-- pattern) so a standalone `luau` binary can load it for behavior tests;
-- Core owns the transport side.

local Review = {}

export type State = {
	total: number,
}

function Review.initial(): State
	return { total = 0 }
end

--- A `disk-review` event's `total` (authoritative): 0 clears the indicator.
--- Malformed totals keep the current state.
function Review.onEvent(state: State, total: any): State
	if type(total) ~= "number" or total ~= total or total < 0 then
		return state
	end

	return { total = math.floor(total) }
end

--- A `GET /review` poll outcome: `nil` means the poll failed or was
--- unparseable — keep the current state; a number is authoritative like an
--- event.
function Review.onPoll(state: State, total: number?): State
	if total == nil then
		return state
	end

	return Review.onEvent(state, total)
end

--- Extract the pending-review total from a decoded `GET /review` body.
--- The daemon answers `{pending=true, reviewId, stats={total,...}}` or
--- `{pending=false}`; the event-mirroring `{total}` and `{entries={...}}`
--- shapes stay accepted defensively; anything else is unparseable → nil
--- (the caller keeps its state).
function Review.parsePoll(decoded: any): number?
	if type(decoded) ~= "table" then
		return nil
	end

	local body = decoded :: { [string]: any }
	local stats = body.stats
	local total = if type(stats) == "table" then (stats :: { [string]: any }).total else body.total

	if type(total) == "number" and total == total and total >= 0 then
		return math.floor(total)
	end

	if (decoded :: { [string]: any }).pending == false then
		return 0
	end

	local entries = (decoded :: { [string]: any }).entries

	if type(entries) == "table" then
		return #entries
	end

	return nil
end

--- The Connected-page indicator line, or nil when nothing awaits review.
function Review.message(state: State): string?
	if state.total <= 0 then
		return nil
	end

	if state.total == 1 then
		return "1 disk change awaits review in the WSync app"
	end

	return `{state.total} disk changes await review in the WSync app`
end

return Review
