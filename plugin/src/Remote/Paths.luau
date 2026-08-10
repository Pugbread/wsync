--!strict
-- Instance path codec for the remote-op surface (coordinator ruling, Design
-- §5.4): paths are `/`-separated Studio *names* rooted at the DataModel
-- (`Workspace/Camera`), and the empty string means `game` itself.
--
-- v1 limitation (documented, no escaping): a name segment may contain any
-- character except `/`. An instance whose Name contains `/` cannot be
-- addressed, and the path this module produces for it is ambiguous — which is
-- why the mutating ops refuse to *create* one (WriteRules.checkNameWrite); a
-- slashed name can still arrive from Studio itself or `eval`, and only `eval`
-- can reach it afterwards. Duplicate sibling names resolve to the first
-- `FindFirstChild` match. Escaping and disambiguation are protocol-v1.1
-- candidates.
--
-- Resolution is strictly read-only: the first segment is looked up with
-- `FindFirstChild` like every other one — never `GetService`, which would
-- *create* a missing service and mutate the DataModel.

local Error = require(script.Parent.Error)

local Paths = {}

-- Guard against degenerate inputs before walking the tree
local MAX_PATH_BYTES = 16 * 1024
local MAX_PATH_SEGMENTS = 256

--- Split a path into name segments. Empty segments (leading/trailing or
--- doubled `/`) collapse; `""` and `"/"` produce zero segments (the
--- DataModel). Raises INVALID_ARGUMENT for non-string or oversized paths.
function Paths.split(path: any): { string }
	if type(path) ~= "string" then
		error(Error.new(Error.INVALID_ARGUMENT, "path must be a string"), 0)
	end

	if #path > MAX_PATH_BYTES then
		error(Error.new(Error.INVALID_ARGUMENT, `path exceeds {MAX_PATH_BYTES} bytes`), 0)
	end

	local segments = {}

	for segment in path:gmatch("[^/]+") do
		table.insert(segments, segment)
	end

	if #segments > MAX_PATH_SEGMENTS then
		error(Error.new(Error.INVALID_ARGUMENT, `path exceeds {MAX_PATH_SEGMENTS} segments`), 0)
	end

	return segments
end

--- Resolve a path without raising on a missing instance. Returns the
--- instance, or `nil` plus the index and name of the first missing segment.
--- Still raises INVALID_ARGUMENT for malformed path values.
function Paths.tryResolve(path: any): (Instance?, number?, string?)
	local segments = Paths.split(path)
	local current: Instance = game

	for index, segment in segments do
		local child = current:FindFirstChild(segment)

		if child == nil then
			return nil, index, segment
		end

		current = child
	end

	return current, nil, nil
end

--- Resolve a path or raise NOT_FOUND naming the first missing segment.
function Paths.resolve(path: any): Instance
	local instance, failedIndex, failedSegment = Paths.tryResolve(path)

	if instance == nil then
		local parentPath = table.concat(Paths.split(path), "/", 1, (failedIndex :: number) - 1)
		local parentLabel = if parentPath == "" then "the DataModel" else `'{parentPath}'`

		error(
			Error.new(
				Error.NOT_FOUND,
				`no instance at '{tostring(path)}': segment {failedIndex} ('{failedSegment}') not found under {parentLabel}`
			),
			0
		)
	end

	return instance
end

--- Build the path of an instance. Returns the path and whether the instance
--- is rooted at the DataModel (`false` for instances outside `game`, whose
--- path is best-effort and not resolvable back).
function Paths.pathOf(instance: Instance): (string, boolean)
	if instance == game then
		return "", true
	end

	local parts = {}
	local current: Instance? = instance

	while current ~= nil and current ~= game do
		table.insert(parts, 1, current.Name)
		current = current.Parent
	end

	return table.concat(parts, "/"), current == game
end

return Paths
