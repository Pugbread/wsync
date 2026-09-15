-- The runner prepends the exact wrapper literals extracted from Runtime.luau.
local ResultEnvelope = require("./ResultEnvelope")
local Codec = require("./Codec")
local checks = 0
local function check(ok, message)
	checks += 1
	assert(ok, message)
end
-- Compact JSON serialization for these finite, UTF-8 fixtures. The capEnvelope
-- function prepended by the runner is the real Runtime implementation; only
-- HttpService is unavailable in standalone Luau. This makes encoded-byte
-- capacity regressions visible as well as checking the envelope's Lua shape.
local function jsonEncode(value)
	if type(value) == "string" then
		local escaped = value:gsub("[\\\"\n\r\t]", { ["\\"]="\\\\", ['"']='\\"', ["\n"]="\\n", ["\r"]="\\r", ["\t"]="\\t" })
		return '"' .. escaped .. '"'
	elseif type(value) == "number" or type(value) == "boolean" then
		return tostring(value)
	elseif value == nil then
		return "null"
	end
	local parts = {}
	if #value > 0 then
		for _, item in value do table.insert(parts, jsonEncode(item)) end
		return "[" .. table.concat(parts, ",") .. "]"
	end
	for key, item in value do table.insert(parts, jsonEncode(key) .. ":" .. jsonEncode(item)) end
	return "{" .. table.concat(parts, ",") .. "}"
end
function HttpService:JSONEncode(value) return jsonEncode(value) end
local function copyArray(packed)
	local received = {}
	for key, value in packed do
		if type(key) == "number" then received[key] = value end
	end
	return received
end
local function execute(source, playscript)
	local received, total
	local function capture(packed, count)
		received, total = copyArray(packed), count
	end
	local event = {Fire = function(_, packed, count) capture(packed, count) end}
	local bridge = {Invoke = function(_, action, packed, count)
		check(action == "__return", "unexpected bridge action")
		capture(packed, count)
	end}
	local environment = setmetatable({
		script = {Parent = {WaitForChild = function(_, name)
			if name == "Result" then return event end
			if name == "Bridge" then return bridge end
			return {Value="config"}
		end}},
		game = {GetService = function()
			return {JSONDecode = function(_, text)
				return if text == "config" then {argsJson="{}",mode="play",context="server",jobId="test"} else {}
			end}
		end},
	}, {__index=getfenv()})
	local head = if playscript then PLAYSCRIPT_WRAPPER_HEAD else EXEC_WRAPPER_HEAD
	local tail = if playscript then PLAYSCRIPT_WRAPPER_TAIL else EXEC_WRAPPER_TAIL
	local chunk = assert(loadstring(head .. "\n" .. source .. "\n" .. tail))
	setfenv(chunk, environment)
	chunk()
	check(type(total) == "number", "wrapper failed to send explicit count")
	check(received.n == nil, "test did not reproduce dropped table.pack metadata")
	local packed = ResultEnvelope.restore(received, total)
	local budget = Codec.newBudget()
	return ResultEnvelope.build(packed, function(value) return Codec.encode(value,budget) end)
end
for _, playscript in {false,true} do
	local result = execute("return 1+1", playscript)
	check(result.ok and result.count == 1 and result.value == 2, "simple return lost")
	check(result.results == nil, "nonnil values must not be duplicated into slots")
	result = execute("return nil", playscript)
	check(result.ok and result.count == 1 and result.results[1].isNil, "single nil return lost")
	result = execute("return", playscript)
	check(result.ok and result.count == 0 and result.results == nil, "empty return misreported")
	result = execute("return 1,nil,3,nil", playscript)
	check(result.ok and result.count == 4 and #result.results == 4, "nil positions lost")
	check(result.results[1].value == 1 and result.results[2].isNil and result.results[3].value == 3 and result.results[4].isNil, "return slots corrupted")
	check(result.value == nil, "sparse legacy value must not pretend to be complete")
	for i, slot in result.results do check(slot.index == i, "slot order changed") end
	result = execute("return nil,nil", playscript)
	check(result.ok and result.count == 2 and #result.results == 2 and result.results[1].isNil and result.results[2].isNil, "all-nil tuple lost")
	result = execute("return 1,false,3", playscript)
	check(result.count == 3 and #result.value == 3 and result.value[2] == false and result.results == nil, "false is not nil")
	result = execute('return string.rep("x",600000)', playscript)
	local capped, encoded = capEnvelope(result)
	check(not capped.truncated and #capped.value == 600000 and #encoded < LIMITS.resultBytes, "600 KB return was duplicated and truncated")
	result = execute('return string.rep("x",300000),string.rep("y",300000)', playscript)
	capped, encoded = capEnvelope(result)
	check(not capped.truncated and #capped.value == 2 and #encoded < LIMITS.resultBytes, "dense multi-return capacity was halved")
	result = execute('return string.rep("x",600000),nil', playscript)
	capped, encoded = capEnvelope(result)
	check(not capped.truncated and capped.results[2].isNil and #encoded < LIMITS.resultBytes, "nil tuple payload must be encoded only once")
	result = execute('error("expected failure")', playscript)
	check(not result.ok and result.error:find("expected failure",1,true) and result.traceback, "failure details lost")
end
local _, emptyEncoded = capEnvelope(execute('return ""',false))
local boundary = LIMITS.resultBytes - #emptyEncoded
local exact, exactJson = capEnvelope(execute(`return string.rep("x",{boundary})`,false))
check(not exact.truncated and #exactJson == LIMITS.resultBytes, "exactly 1 MiB must remain accepted")
local oversized = capEnvelope(execute(`return string.rep("x",{boundary+1})`,false))
check(oversized.truncated and oversized.value.bytes == LIMITS.resultBytes+1, "over-limit bytes must still truncate")
local malformed = ResultEnvelope.build(ResultEnvelope.restore({true,1},nil), Codec.encode)
check(not malformed.ok and malformed.error:find("explicit return count",1,true), "missing count must fail instead of inventing one")
local cycle = {} cycle[1] = cycle
local failed = ResultEnvelope.build(table.pack(true,cycle), Codec.encode)
check(not failed.ok and failed.error:find("table cycle",1,true), "codec failure lost its typed message")
print(`Playtest result transport: {checks} checks passed using real exec/playscript wrappers, real envelope builder and real Codec.`)
