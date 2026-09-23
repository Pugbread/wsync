--!strict
-- In-flight script `Source` writes (WSync addition to the rbx-dom Lua port).
--
-- Sync writes script sources through `ScriptEditorService:UpdateSourceAsync`,
-- which yields, so customProperties starts each write on its own thread and
-- returns at once. That keeps the sync loop moving, but it also means "the
-- sync loop applied a frame" does not yet mean "Studio holds the new source".
-- This counter closes the gap: every write is registered until
-- UpdateSourceAsync returns, and the `sync_barrier` op
-- (Remote/Handlers/Sync) waits for it to reach zero before it answers.
--
-- No requires and nothing Roblox-specific beyond `task`, so it loads under a
-- standalone `luau` binary.

local SourceWrites = {}

local pending = 0

--- Run `write` on its own thread, counted until it returns or raises. A
--- failure is re-raised on that thread exactly as before, but only after the
--- count is released — a stuck counter would wedge every later barrier.
function SourceWrites.spawn(write: () -> ())
	pending += 1

	task.spawn(function()
		local ok, err = pcall(write)

		pending -= 1

		if not ok then
			error(err, 0)
		end
	end)
end

--- Writes started and not yet finished.
function SourceWrites.pending(): number
	return pending
end

--- Yield until no write is in flight or `timeout` seconds have passed.
--- Returns whether the writes drained.
function SourceWrites.waitIdle(timeout: number): boolean
	local deadline = os.clock() + timeout

	while pending > 0 do
		if os.clock() >= deadline then
			return false
		end

		task.wait()
	end

	return true
end

return SourceWrites
