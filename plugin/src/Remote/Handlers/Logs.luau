--!strict
-- `logs {sinceSeq?, sinceSeconds?, level?, limit?}` — a bounded slice of the
-- plugin's LogService capture ring (logs.json). The ring itself lives in
-- Remote/LogBuffer and starts capturing at plugin load.

local Error = require(script.Parent.Parent.Error)
local LogBuffer = require(script.Parent.Parent.LogBuffer)

type Args = { [string]: any }

local Logs = {}

Logs.LIMITS = {
	defaultLimit = 200,
	maxLimit = LogBuffer.CAPACITY,
}

Logs.ops = {} :: { [string]: (args: Args, ctx: any) -> any }

function Logs.ops.logs(args: Args, _ctx)
	local sinceSeq: number? = nil

	if args.sinceSeq ~= nil then
		if type(args.sinceSeq) ~= "number" or args.sinceSeq < 0 then
			error(Error.new(Error.INVALID_ARGUMENT, "sinceSeq must be a non-negative number"), 0)
		end

		sinceSeq = math.floor(args.sinceSeq)
	end

	local sinceSeconds: number? = nil

	if args.sinceSeconds ~= nil then
		if type(args.sinceSeconds) ~= "number" or args.sinceSeconds < 0 then
			error(Error.new(Error.INVALID_ARGUMENT, "sinceSeconds must be a non-negative number"), 0)
		end

		sinceSeconds = args.sinceSeconds
	end

	local level: ("info" | "warn" | "error")? = nil

	if args.level ~= nil then
		if args.level ~= "info" and args.level ~= "warn" and args.level ~= "error" then
			error(Error.new(Error.INVALID_ARGUMENT, "level must be 'info', 'warn' or 'error'"), 0)
		end

		level = args.level
	end

	local limit = Logs.LIMITS.defaultLimit

	if args.limit ~= nil then
		if type(args.limit) ~= "number" then
			error(Error.new(Error.INVALID_ARGUMENT, "limit must be a number"), 0)
		end

		limit = math.clamp(math.floor(args.limit), 1, Logs.LIMITS.maxLimit)
	end

	local entries = LogBuffer.read({
		sinceSeq = sinceSeq,
		sinceSeconds = sinceSeconds,
		level = level,
		limit = limit,
	})

	return {
		entries = entries,
		count = #entries,
		-- `t` fields are os.clock()-relative; ship both clocks so the daemon
		-- can render wall times and page with `sinceSeq`
		now = os.clock(),
		wall = os.time(),
		buffer = LogBuffer.state(),
	}
end

return Logs
