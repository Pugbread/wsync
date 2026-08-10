--!strict
-- Studio Selection ops: `select_get`, `select_set` (select.json).
--
-- SAFETY: `select_set` is the single deliberate mutation in the read-only
-- op slice, and it is whitelisted because `Selection:Set` changes Studio
-- *UI state only* — which instances are highlighted in the Explorer — never
-- the DataModel itself. No instance is created, destroyed, reparented, or
-- edited, nothing syncs back to disk, and an undo waypoint is not required.
-- Every other mutating op (set/new/rm/mv/…) belongs to a later slice with
-- write guardrails and audit logging.

local StudioSelection = game:GetService("Selection")

local Error = require(script.Parent.Parent.Error)
local Paths = require(script.Parent.Parent.Paths)

type Args = { [string]: any }

local Selection = {}

Selection.LIMITS = {
	maxPaths = 10000,
}

Selection.ops = {} :: { [string]: (args: Args, ctx: any) -> any }

--- `select_get` — the current Studio selection as instance paths.
function Selection.ops.select_get(_args: Args, _ctx)
	local ok, selected = pcall(function()
		return StudioSelection:Get()
	end)

	if not ok or type(selected) ~= "table" then
		error(Error.new(Error.PLUGIN_ERROR, "failed to read the Studio selection"), 0)
	end

	local paths = {}

	for _, instance in selected do
		local okPath, path = pcall(function()
			-- Selections outside the DataModel keep a best-effort path
			return (Paths.pathOf(instance))
		end)

		if okPath then
			table.insert(paths, path)
		end
	end

	return { paths = paths, count = #paths }
end

--- `select_set {paths}` — replace the Studio selection. Every path is
--- validated *before* the selection is touched: any failure raises NOT_FOUND
--- naming the bad path and leaves the current selection untouched.
function Selection.ops.select_set(args: Args, _ctx)
	local rawPaths = args.paths

	if type(rawPaths) ~= "table" then
		error(Error.new(Error.INVALID_ARGUMENT, "paths must be an array of instance paths"), 0)
	end

	local count = #rawPaths

	if count > Selection.LIMITS.maxPaths then
		error(Error.new(Error.INVALID_ARGUMENT, `paths exceeds the {Selection.LIMITS.maxPaths}-entry limit`), 0)
	end

	local instances: { Instance } = table.create(count)

	for index, rawPath in ipairs(rawPaths :: { any }) do
		if type(rawPath) ~= "string" then
			error(Error.new(Error.INVALID_ARGUMENT, `paths[{index}] must be a string`), 0)
		end

		local instance, _, failedSegment = Paths.tryResolve(rawPath)

		if instance == nil then
			error(
				Error.new(
					Error.NOT_FOUND,
					`paths[{index}] ('{rawPath}') does not resolve: segment '{failedSegment}' not found — selection unchanged`
				),
				0
			)
		end

		table.insert(instances, instance)
	end

	local ok, err = pcall(function()
		StudioSelection:Set(instances)
	end)

	if not ok then
		error(Error.new(Error.PLUGIN_ERROR, `Selection:Set failed: {tostring(err)}`), 0)
	end

	return { count = #instances }
end

return Selection
