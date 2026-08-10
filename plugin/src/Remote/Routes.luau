--!strict
-- Parent-route codec for the clipboard ops (copy.json / paste.json). A route
-- addresses an instance as `{name, class, ordinal}` segments from the
-- DataModel down: `ordinal` is the 1-based position among siblings sharing
-- both name and class, so duplicate same-named siblings and names containing
-- `/` round-trip exactly where the `/`-separated path grammar (Remote/Paths)
-- cannot. `clipboard_copy` records each root's parent route inside the
-- payload envelope; `clipboard_paste_commit` resolves it in the destination
-- place ("default routing restores each root's original parent route when it
-- exists" — pinned).
--
-- Tree access is injected (`Adapter`) so the ordinal logic runs against
-- plain-table trees under a standalone `luau` binary; the handler passes
-- thin Instance wrappers. No requires, no Roblox APIs.

local Routes = {}

Routes.MAX_SEGMENTS = 256

export type Segment = {
	name: string,
	class: string,
	ordinal: number,
}

export type Adapter = {
	-- The parent node, or nil at (or above) the DataModel root.
	parentOf: (node: any) -> any?,
	nameOf: (node: any) -> string,
	classOf: (node: any) -> string,
	childrenOf: (node: any) -> { any },
	-- True for the DataModel root itself (routes are rooted just below it).
	isRoot: (node: any) -> boolean,
}

--- Encode the route of `node` from the root down. Returns nil when the node
--- is unparented, sits above the root, or is deeper than MAX_SEGMENTS.
function Routes.routeOf(node: any, adapter: Adapter): { Segment }?
	local route: { Segment } = {}
	local current = node

	while current ~= nil and not adapter.isRoot(current) do
		local parent = adapter.parentOf(current)

		if parent == nil then
			return nil
		end

		local name = adapter.nameOf(current)
		local class = adapter.classOf(current)
		local ordinal = 0

		for _, sibling in adapter.childrenOf(parent) do
			if adapter.nameOf(sibling) == name and adapter.classOf(sibling) == class then
				ordinal += 1

				if sibling == current then
					break
				end
			end
		end

		if ordinal < 1 then
			-- The node is not among its parent's children (torn mid-walk)
			return nil
		end

		if #route >= Routes.MAX_SEGMENTS then
			return nil
		end

		table.insert(route, 1, {
			name = name,
			class = class,
			ordinal = ordinal,
		})

		current = parent
	end

	if current == nil then
		return nil
	end

	return route
end

--- Whether `value` is a structurally valid route (decoded from the payload
--- envelope, so every field is untrusted).
function Routes.validate(value: any): boolean
	if type(value) ~= "table" or #value < 1 or #value > Routes.MAX_SEGMENTS then
		return false
	end

	for _, segment in value :: { any } do
		if type(segment) ~= "table" then
			return false
		end

		if type(segment.name) ~= "string" or segment.name == "" then
			return false
		end

		if type(segment.class) ~= "string" or segment.class == "" then
			return false
		end

		local ordinal = segment.ordinal

		if type(ordinal) ~= "number" or ordinal < 1 or ordinal % 1 ~= 0 then
			return false
		end
	end

	return true
end

--- Resolve a validated route against `root` (the DataModel node). Returns
--- the addressed node, or nil when any segment has no matching child.
function Routes.resolve(route: { Segment }, root: any, adapter: Adapter): any?
	local current = root

	for _, segment in route do
		local ordinal = 0
		local matched: any? = nil

		for _, child in adapter.childrenOf(current) do
			if adapter.nameOf(child) == segment.name and adapter.classOf(child) == segment.class then
				ordinal += 1

				if ordinal == segment.ordinal then
					matched = child
					break
				end
			end
		end

		if matched == nil then
			return nil
		end

		current = matched
	end

	return current
end

return Routes
