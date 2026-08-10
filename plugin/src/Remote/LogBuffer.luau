--!strict
-- Bounded LogService capture ring for the `logs` remote op (logs.json).
--
-- One `MessageOut` subscription feeds a fixed-size ring: ≤500 entries, each
-- message UTF-8 sanitized and capped at 2 KiB, stamped with a monotonic `seq`
-- (so `--tail` can page forward without duplicates), `t = os.clock()`
-- (monotonic) and `wall = os.time()` (for HH:MM:SS rendering daemon-side).
--
-- Capture starts when this module is first required, which happens during
-- plugin load (init.server -> App -> Core -> Transport -> Remote), and seeds
-- from `LogService:GetLogHistory()` so `wsync logs` works right after a
-- plugin reload.

local WSync = script:FindFirstAncestor("WSync")

local Log = require(WSync.Log)

local Codec = require(script.Parent.Codec)

local LogBuffer = {}

LogBuffer.CAPACITY = 500
LogBuffer.MAX_MESSAGE_BYTES = 2048

export type Entry = {
	seq: number,
	t: number,
	wall: number,
	level: "info" | "warn" | "error",
	message: string,
}

export type ReadOptions = {
	sinceSeq: number?,
	sinceSeconds: number?,
	level: ("info" | "warn" | "error")?,
	limit: number?,
}

local ring: { Entry } = table.create(LogBuffer.CAPACITY)
local head = 0 -- slot of the newest entry (0 = empty)
local count = 0
local nextSeq = 1
local started = false

local LEVEL_RANK: { [string]: number } = {
	info = 0,
	warn = 1,
	error = 2,
}

local function mapLevel(messageType: Enum.MessageType): "info" | "warn" | "error"
	if messageType == Enum.MessageType.MessageError then
		return "error"
	elseif messageType == Enum.MessageType.MessageWarning then
		return "warn"
	end

	-- MessageOutput and MessageInfo both rank as info
	return "info"
end

local function append(message: string, messageType: Enum.MessageType)
	local text = Codec.sanitizeUtf8(tostring(message))
	local truncated

	text, truncated = Codec.utf8Truncate(text, LogBuffer.MAX_MESSAGE_BYTES)

	if truncated then
		text ..= "<truncated>"
	end

	head += 1

	if head > LogBuffer.CAPACITY then
		head = 1
	end

	ring[head] = {
		seq = nextSeq,
		t = os.clock(),
		wall = os.time(),
		level = mapLevel(messageType),
		message = text,
	}

	nextSeq += 1

	if count < LogBuffer.CAPACITY then
		count += 1
	end
end

--- Start capture (idempotent). Called at require time below; safe to call
--- again after a failed first attempt.
function LogBuffer.start()
	if started then
		return
	end

	local ok, err = pcall(function()
		local LogService = game:GetService("LogService")

		LogService.MessageOut:Connect(function(message: string, messageType: Enum.MessageType)
			append(message, messageType)
		end)

		-- Seed with whatever LogService already accumulated (bounded by the
		-- ring capacity — older history simply falls off).
		local okHistory, history = pcall(function()
			return LogService:GetLogHistory()
		end)

		if okHistory and type(history) == "table" then
			for _, entry in history do
				append(entry.message, entry.messageType)
			end
		end
	end)

	if ok then
		started = true
	else
		Log.warn("LogService capture failed to start:", err)
	end
end

function LogBuffer.rank(level: string): number?
	return LEVEL_RANK[level]
end

--- Read entries oldest-to-newest with optional filters. `limit` keeps the
--- newest entries when trimming. Callers validate/clamp their arguments; this
--- only reads.
function LogBuffer.read(options: ReadOptions): { Entry }
	if count == 0 then
		return {}
	end

	local cutoffClock: number? = nil

	if options.sinceSeconds ~= nil then
		cutoffClock = os.clock() - options.sinceSeconds
	end

	local minRank = if options.level then LEVEL_RANK[options.level] or 0 else 0
	local sinceSeq = options.sinceSeq

	local start = head - count + 1

	if start < 1 then
		start += LogBuffer.CAPACITY
	end

	local collected: { Entry } = {}

	for offset = 0, count - 1 do
		local slot = start + offset

		if slot > LogBuffer.CAPACITY then
			slot -= LogBuffer.CAPACITY
		end

		local entry = ring[slot]

		if entry == nil then
			continue
		end

		local pass = true

		if cutoffClock ~= nil and entry.t < cutoffClock then
			pass = false
		end

		if pass and sinceSeq ~= nil and entry.seq <= sinceSeq then
			pass = false
		end

		if pass and LEVEL_RANK[entry.level] < minRank then
			pass = false
		end

		if pass then
			table.insert(collected, entry)
		end
	end

	local limit = options.limit

	if limit ~= nil and #collected > limit then
		-- Newest entries win when trimming
		local trimmed = table.create(limit)

		for index = #collected - limit + 1, #collected do
			table.insert(trimmed, collected[index])
		end

		collected = trimmed
	end

	return collected
end

--- Buffer state for the `logs` response meta (lets the daemon detect gaps).
function LogBuffer.state(): { count: number, capacity: number, newestSeq: number, oldestSeq: number }
	local newest = if count > 0 then ring[head].seq else 0
	local oldest = if count > 0 then newest - count + 1 else 0

	return {
		count = count,
		capacity = LogBuffer.CAPACITY,
		newestSeq = newest,
		oldestSeq = oldest,
	}
end

-- Capture starts at plugin load (module require time)
LogBuffer.start()

return LogBuffer
