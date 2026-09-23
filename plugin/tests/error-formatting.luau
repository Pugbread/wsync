-- Error and log text is safe for any argument, and refs print as hex. Runs the
-- real Core and Transport Error modules, Util and Log over the fakes in
-- harness/; `Fakes`, `ref` and `require` arrive from harness/loader.luau.
--
-- The regression this guards: the daemon sent a removal for a ref the plugin
-- did not know, the warning carried the raw 16-byte ref, and when one of
-- those bytes was `%` (~6% of refs) `Error.new`'s `gsub` raised "invalid use
-- of '%' in replacement string" from inside the sync loop — which dropped
-- the connection.

local Error = require(ref("Error"))
local TransportError = require(ref("TransportError"))
local Util = require(ref("Util"))
local RealLog = require(ref("RealLog"))

local failures = 0
local checks = 0

local function expect(condition: boolean, label: string)
	checks += 1

	if condition then
		print(`  ok    {label}`)
	else
		failures += 1
		print(`  FAIL  {label}`)
	end
end

--- `fn()` must not throw; returns its result (or nil when it did)
local function noThrow(label: string, fn: () -> any): any
	local ok, result = pcall(fn)

	expect(ok, `{label} does not throw{if ok then "" else ` (threw: {result})`}`)

	return if ok then result else nil
end

local function contains(haystack: string?, needle: string): boolean
	return haystack ~= nil and string.find(haystack, needle, 1, true) ~= nil
end

local function isHexRef(text: string?): boolean
	return text ~= nil and #text == 32 and string.match(text, "^[0-9a-f]+$") ~= nil
end

local function hexOf(bytes: buffer): string
	local out = {}

	for i = 0, buffer.len(bytes) - 1 do
		table.insert(out, string.format("%02x", buffer.readu8(bytes, i)))
	end

	return table.concat(out)
end

-- A ref made of nothing but `%` bytes, and one with a lone `%` before a
-- letter — the exact shape that used to throw
local PERCENT_REF = buffer.fromstring(string.rep("%", 16))
local MIXED_REF = buffer.fromstring("ab%cd\0\255%%ef\n\t\"q%")
local PERCENT_HEX = string.rep("25", 16)

--------------------------------------------------------------------------------

print("scenario: Error.new with a raw ref carrying `%` bytes")
do
	local err = noThrow("Error.new(NoInstanceRemove, <ref with a `%c` in it>)", function()
		return Error.new(Error.NoInstanceRemove, MIXED_REF)
	end)

	expect(err ~= nil and contains(err.message, hexOf(MIXED_REF)), "the ref prints as lowercase hex")
	expect(err ~= nil and not contains(err.message, "%"), "no raw `%` byte reaches the message")
	expect(err ~= nil and err == Error.NoInstanceRemove, "the filled error still compares equal to its kind")

	err = noThrow("Error.new(NoInstanceRemove, <ref of % bytes>)", function()
		return Error.new(Error.NoInstanceRemove, PERCENT_REF)
	end)

	expect(err ~= nil and contains(err.message, PERCENT_HEX), "an all-`%` ref prints as hex too")

	for _ = 1, 64 do
		-- Random refs: the ~6% of them that carry a `%` are what used to throw
		local random = buffer.create(16)

		for i = 0, 15 do
			buffer.writeu8(random, i, math.random(0, 255))
		end

		local ok = pcall(Error.new, Error.NoInstanceUpdate, random)

		if not ok then
			expect(false, "Error.new never throws on a random ref")
			break
		end
	end

	expect(isHexRef(Util.stringify(MIXED_REF)), "Util.stringify(ref) is 32-char lowercase hex")
	expect(not contains(Util.stringify({ id = MIXED_REF }), "%"), "a ref nested in a table prints as hex too")
end

print("scenario: Error.new with `%`, `$n` and odd values in its arguments")
do
	local err = noThrow("Error.new with `%` in text arguments", function()
		return Error.new(Error.GameId, "100%", "%1%%")
	end)

	expect(
		err ~= nil and err.message == "Current GameId: 100% does not match the server gameId: %1%%",
		`argument text is inserted verbatim (got {err and err.message})`
	)

	err = noThrow("Error.new with a placeholder inside an argument", function()
		return Error.new(Error.GameId, "$2", "x")
	end)

	expect(
		err ~= nil and err.message == "Current GameId: $2 does not match the server gameId: x",
		`an argument's own \`$2\` is not substituted again (got {err and err.message})`
	)

	err = noThrow("Error.new with too few arguments", function()
		return Error.new(Error.TooManyChanges, 1, 2)
	end)

	expect(
		err ~= nil and err.message == "There are 1 additions, 2 updates, $3 deletions compared to the server",
		`a placeholder with no argument stays (got {err and err.message})`
	)

	err = noThrow("Error.new with a nil argument first", function()
		return Error.new(Error.GameId, nil, 5)
	end)

	expect(err ~= nil and err.message == "Current GameId: nil does not match the server gameId: 5", "nil prints as nil")

	local hostile = setmetatable({}, {
		__tostring = function()
			error("tostring refused")
		end,
	})

	err = noThrow("Error.new with a value whose __tostring throws", function()
		return Error.new(Error.Unknown, hostile)
	end)

	expect(err ~= nil and contains(err.message, "<unprintable"), "an unprintable value prints as a placeholder")

	noThrow("Error.new with a boolean array and a cyclic table", function()
		local cyclic = {}
		cyclic.self = cyclic

		return Error.new(Error.GameId, { true, false, PERCENT_REF }, cyclic)
	end)
end

print("scenario: the transport's errors take the same text safely")
do
	local err = noThrow("TransportError.new with `%`", function()
		return TransportError.new(TransportError.OpenFailed, "50% done %d")
	end)

	expect(
		err ~= nil and err.message == "Failed to open the WebSocket connection: 50% done %d",
		`verbatim (got {err and err.message})`
	)

	err = noThrow("TransportError.fromResponse with `%` in the body", function()
		return TransportError.fromResponse({ Body = "bad %1 request %", StatusMessage = "" })
	end)

	expect(err ~= nil and err.message == "Unknown HTTP error: bad %1 request %", `verbatim (got {err and err.message})`)

	err = noThrow("TransportError.fromMessage with `%`", function()
		return TransportError.fromMessage("HttpError: weird 100%")
	end)

	expect(err ~= nil and err.message == "Unknown HTTP error: HttpError: weird 100%", `verbatim (got {err and err.message})`)
end

print("scenario: Util.stringify and the real Log never throw")
do
	local long = buffer.create(40)
	local text = Util.stringify(long)

	expect(contains(text, "<buffer 40 bytes: ") and contains(text, "…"), `a non-ref buffer prints its length and a preview ({text})`)

	local nested = {}
	local current = nested

	for _ = 1, 64 do
		current.next = {}
		current = current.next
	end

	noThrow("Util.stringify on deep nesting", function()
		return Util.stringify(nested)
	end)

	local written = {}
	local previousWarn = warn

	warn = function(message)
		table.insert(written, message)
	end

	noThrow("Log.warn with a `%` ref, `%` text and an unprintable value", function()
		RealLog.warn("Tried to remove:", PERCENT_REF, "100%", setmetatable({}, {
			__tostring = function()
				error("no")
			end,
		}))
	end)

	warn = previousWarn

	local line = written[1]

	expect(line ~= nil and contains(line, PERCENT_HEX), `the logged ref is hex ({line})`)
	expect(line ~= nil and contains(line, " 100% "), "the logged text is verbatim")
end

print("")

if failures > 0 then
	error(`{failures} of {checks} checks failed`, 0)
end

print(`all {checks} checks passed`)
