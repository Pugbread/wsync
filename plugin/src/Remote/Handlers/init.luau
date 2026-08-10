--!strict
-- Handler registry: merges the op families into one `{ [op] = handler }`
-- table for the dispatcher. Meta/Inspect/Query/ClassInfo/Logs are read-only
-- (Selection.ops.select_set is a whitelisted UI-state exception — see
-- Handlers/Selection.luau); Write and History carry the mutating slice with
-- its guardrails (Parent guard, cross-service mv force, per-op waypoints,
-- `meta.mutation`/`meta.audit` tagging — see Handlers/Write.luau); Bulk
-- carries the read-only bulk-transfer pair (`read_subtree`/`source_read`)
-- behind Core's session bridge (Design §7.4-A); Capture renders and streams
-- RGBA captures and Transmit reads existing image pixels into the same
-- session registry; Clipboard serializes/pastes native instance payloads
-- (Design §5.4; wave-5 pinned contracts); Playtest drives Studio playtests
-- and their PlayServer/PlayClient runtime agents over PluginConnectionService
-- (playtest.json; wave-6 pinned contracts). Every handler returns a JSON-safe
-- value or raises a RemoteError (Remote/Error.luau).

local Meta = require(script.Meta)
local Inspect = require(script.Inspect)
local Query = require(script.Query)
local ClassInfo = require(script.ClassInfo)
local Selection = require(script.Selection)
local Logs = require(script.Logs)
local Write = require(script.Write)
local History = require(script.History)
local Bulk = require(script.Bulk)
local Capture = require(script.Capture)
local Transmit = require(script.Transmit)
local Clipboard = require(script.Clipboard)
local Playtest = require(script.Playtest)

export type Handler = (args: { [string]: any }, ctx: any) -> any

local ops: { [string]: Handler } = {}

for _, family in
	{
		Meta.ops,
		Inspect.ops,
		Query.ops,
		ClassInfo.ops,
		Selection.ops,
		Logs.ops,
		Write.ops,
		History.ops,
		Bulk.ops,
		Capture.ops,
		Transmit.ops,
		Clipboard.ops,
		Playtest.ops,
	} :: { { [string]: Handler } }
do
	for name, handler in family do
		if ops[name] ~= nil then
			-- A duplicate registration is a programming error; fail loudly
			-- at load instead of silently shadowing an op.
			error(`[WSync] duplicate remote op handler: '{name}'`)
		end

		ops[name] = handler
	end
end

return ops
