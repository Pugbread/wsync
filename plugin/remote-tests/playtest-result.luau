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
local transportChecks = checks

-- The run's terminal record repeats every client failure, so a caller reading
-- only the last record still sees why the main script sat waiting
local RunState = require("./RunState")
local function newRun()
	local config = RunState.validate({script="return 1", clientScript="error('x')", mode="multiplayer", players=2, timeoutSec=5})
	local run = RunState.create(config, "{}", "job-test", 0)
	RunState.markReady(run, "server", 0.1)
	RunState.markBooted(run, "rt-server|main", "main", 0.2)
	return run
end
local function finish(run, terminal, now)
	check(RunState.requestTerminal(run, terminal, now), "terminal request refused")
	check(RunState.finalize(run, nil, now), "finalize refused")
	local page = RunState.poll(run, 0, nil, nil, now)
	check(page.done, "terminal page not done")
	return page.records[#page.records], page
end
local clientTraceback = "Workspace.Door:12: attempt to index nil\nRunner:3 function __wsync_run"
-- The incident: a client errors, the server keeps waiting, the hard timeout fires
local run = newRun()
RunState.enqueue(run, {type="clientResult", context="client:1", ok=false, error="Workspace.Door:12: attempt to index nil", traceback=clientTraceback}, 1)
RunState.enqueue(run, {type="clientResult", context="client:2", ok=true, value=1, count=1}, 1.1)
RunState.enqueue(run, {type="log", context="client:2", level="error", message="LocalScript:4: nope"}, 1.2)
RunState.enqueue(run, {type="log", context="client:2", level="error", message="LocalScript:4: nope"}, 1.3)
RunState.enqueue(run, {type="log", context="server", level="error", message="server noise"}, 1.4)
RunState.enqueue(run, {type="log", context="client:1", level="warn", message="a warning"}, 1.5)
check(RunState.checkDeadlines(run, 4.9, "running") == nil, "hard timeout fired early")
local timeout = RunState.checkDeadlines(run, 5, "running")
check(timeout ~= nil and timeout.outcome == "timeout", "hard timeout not detected")
check(RunState.requestTerminal(run, timeout, 5), "timeout terminal refused")
-- A client failure landing during teardown still makes the terminal
RunState.enqueue(run, {type="clientResult", context="client:2", ok=false, error="client playscript failed to boot"}, 5.5)
timeout.jobStatus = "stopped"
check(RunState.finalize(run, nil, 6), "finalize refused")
local page = RunState.poll(run, 0, nil, nil, 6)
local last = page.records[#page.records]
check(page.done and page.exit == 3, "timeout must still exit 3")
check(last.type == "result" and last.outcome == "timeout" and last.exitCode == 3 and last.jobStatus == "stopped", "timeout record is not self-contained")
local failures = last.clientFailures
check(type(failures) == "table" and #failures == 3, "client failures missing from the timeout record")
check(failures[1].context == "client:1" and failures[1].source == "clientResult" and failures[1].traceback == clientTraceback and failures[1].t == 1, "client traceback lost")
check(failures[2].source == "log" and failures[2].context == "client:2" and failures[2].count == 2, "identical client error lines must collapse")
check(failures[3].error == "client playscript failed to boot" and failures[3].traceback == nil, "late client failure lost")
check(last.clientFailuresOmitted == nil, "nothing was omitted")
local carrier
for _, record in page.records do
	if record.seq == failures[1].seq then carrier = record end
end
check(carrier ~= nil and carrier.type == "clientResult" and carrier.traceback == clientTraceback, "seq must name the progress record that carried the failure")
-- Every outcome carries them; a clean run keeps the old shape
local success = finish(newRun(), {outcome="success", ok=true, value=1}, 1)
check(success.clientFailures == nil and success.exitCode == 0, "a clean run grew a clientFailures field")
run = newRun()
RunState.enqueue(run, {type="clientResult", context="client:1", ok=false, error="boom", traceback="boom\nRunner:1"}, 1)
success = finish(run, {outcome="success", ok=true, value=1}, 2)
check(success.ok and #success.clientFailures == 1, "a success must still report its failed clients")
run = newRun()
RunState.enqueue(run, {type="clientResult", context="client:1", ok=false, error="boom"}, 1)
local aborted = finish(run, {outcome="aborted", ok=false, reason="stopped from Studio", jobStatus="ended"}, 2)
check(aborted.type == "aborted" and aborted.exitCode == 4 and #aborted.clientFailures == 1, "aborted record lost its client failures")
-- Bounded: caps count the overflow, and text is cut on a UTF-8 boundary
run = newRun()
for index = 1, RunState.LIMITS.maxClientFailureResults + 3 do
	RunState.enqueue(run, {type="clientResult", context="client:1", ok=false, error=`boom {index}`}, 1)
end
for index = 1, RunState.LIMITS.maxClientFailureLogs + 2 do
	RunState.enqueue(run, {type="log", context="client:1", level="error", message=`line {index}`}, 1)
end
local wide = string.rep("é", RunState.LIMITS.clientFailureTracebackBytes)
RunState.enqueue(run, {type="clientResult", context="client:2", ok=false, error=wide, traceback=wide}, 1)
local capped = finish(run, {outcome="failure", ok=false, error="main failed"}, 2)
check(#capped.clientFailures == RunState.LIMITS.maxClientFailureResults + RunState.LIMITS.maxClientFailureLogs, "caps not applied")
check(capped.clientFailuresOmitted == 6, "overflow not counted")
run = newRun()
RunState.enqueue(run, {type="clientResult", context="client:2", ok=false, error=wide, traceback=wide}, 1)
local entry = finish(run, {outcome="failure", ok=false, error="main failed"}, 2).clientFailures[1]
check(entry.error:sub(-11) == "<truncated>" and #entry.error <= RunState.LIMITS.clientFailureErrorBytes + 11, "error text not capped")
check(entry.traceback:sub(-11) == "<truncated>" and utf8.len(entry.traceback:sub(1, -12)) ~= nil, "traceback cut mid code point")
print(`Playtest result transport: {transportChecks} checks passed using real exec/playscript wrappers, real envelope builder and real Codec.`)
print(`Playtest run terminal: {checks - transportChecks} checks passed using the real RunState.`)
