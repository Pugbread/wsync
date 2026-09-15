-- SyncScope's pure rules (the module is require-free on purpose, so this
-- needs nothing from harness/fakes.luau beyond the loader): the two exclusion
-- markers, the hello scope, and the code-scope allowlist.

local SyncScope = require(ref("SyncScope"))

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

print("rule: sourceIgnoresSync")
do
	local ignores = SyncScope.sourceIgnoresSync

	expect(ignores("--!ignoresync"), "the bare directive")
	expect(ignores("--!ignoresync\nreturn {}"), "the directive on the first line")
	expect(ignores("\t  --!ignoresync  \nreturn {}"), "leading and trailing whitespace")
	expect(ignores("--!IgnoreSync\n"), "case-insensitive")
	expect(ignores("--!ignoresync\r\nreturn {}"), "a CRLF line ending")
	expect(ignores("-- header\n\n--!ignoresync -- bulk data below\nreturn {}"), "later within the header window")
	expect(not ignores("--!ignoresyncing\n"), "a longer word is not the directive")
	expect(not ignores("-- --!ignoresync\n"), "the directive must start the line")
	expect(not ignores("return '--!ignoresync'"), "inside a string literal on a code line is not it")
	expect(not ignores(""), "an empty source")

	local window = SyncScope.IGNORE_DIRECTIVE_LINES
	local onLastLine = string.rep("\n", window - 1) .. "--!ignoresync"
	local pastWindow = string.rep("\n", window) .. "--!ignoresync"

	expect(ignores(onLastLine), `line {window} is still inside the window`)
	expect(not ignores(pastWindow), `line {window + 1} is past the window`)

	-- The reason the scan copies nothing past the line it looks at: a script
	-- this exists for may carry megabytes on one line right after the header
	local huge = "--!ignoresync\n" .. string.rep("x", 4 * 1024 * 1024)

	expect(ignores(huge), "a megabytes-long body after the directive")
	expect(
		ignores(string.rep("x", 4 * 1024 * 1024) .. "\n--!ignoresync"),
		"a megabytes-long first line does not hide a directive on line 2"
	)
end

print("rule: attributeAvoidsSync")
do
	expect(SyncScope.AVOID_SYNC_ATTRIBUTE == "AvoidSync", "the attribute is named AvoidSync")
	expect(SyncScope.attributeAvoidsSync(true), "true opts out")
	expect(not SyncScope.attributeAvoidsSync(false), "false does not")
	expect(not SyncScope.attributeAvoidsSync(nil), "an absent attribute does not")
	expect(not SyncScope.attributeAvoidsSync("true"), "the string 'true' does not")
	expect(not SyncScope.attributeAvoidsSync(1), "a number does not")
	expect(not SyncScope.attributeAvoidsSync({}), "a table does not")
end

print("rule: hello scope")
do
	expect(SyncScope.normalize("code") == "code", "'code' opts in")
	expect(SyncScope.normalize("full") == "full", "'full' is full")
	expect(SyncScope.normalize(nil) == "full", "absent is full")
	expect(SyncScope.normalize("CODE") == "full", "anything else is full")
	expect(SyncScope.get() == "full", "the default session scope is full")

	SyncScope.set("code")
	expect(SyncScope.isCode(), "set stamps the session")
	SyncScope.reset()
	expect(not SyncScope.isCode(), "reset returns to full")
end

print("rule: code-scope allowlist")
do
	for _, class in
		{ "Folder", "Script", "LocalScript", "ModuleScript", "StarterPlayerScripts", "StarterCharacterScripts" }
	do
		expect(SyncScope.isCodeClass(class), `{class} is a code class`)
	end

	for _, class in { "Part", "Model", "StringValue", "Workspace" } do
		expect(not SyncScope.isCodeClass(class), `{class} is not`)
	end

	expect(not SyncScope.isCodeClass(nil), "nil is not")
end

print("")

if failures > 0 then
	error(`{failures} of {checks} checks failed`, 0)
end

print(`all {checks} checks passed`)
