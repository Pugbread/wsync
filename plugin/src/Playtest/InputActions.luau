--!strict
-- Validation and normalization for `playtest_input` action lists
-- (playtest.json; pinned wave-6 contract): ≤200 actions per request, action
-- types `key`, `key_press`, `mouse_move`, `mouse_delta`, `mouse_button`,
-- `click`, `text` and `wait`, and a planned-duration budget of
-- min(timeoutMs, 30 s) so a request can never park the runtime longer than
-- its own timeout.
--
-- Everything structural is rejected here, before a single event is injected
-- ("reject before applying" — the Capture/Options discipline). What cannot be
-- validated purely is deferred to the runtime: KeyCode / UserInputType names
-- are checked against the real Enums there, and a bad name fails the request
-- before any action of it has run.
--
-- Violations raise inline `__remoteError` INVALID_ARGUMENT markers (the
-- Codec/Selector/Options pattern — keep the shape in sync with Remote/Error).
-- No requires, no Roblox APIs: loadable under a standalone `luau` binary.

local InputActions = {}

InputActions.LIMITS = {
	maxActions = 200,
	maxTimeoutMs = 30000,
	defaultTimeoutMs = 15000,
	-- Per-action and total planned wall-clock cap (seconds)
	maxActionSeconds = 30,
	maxTextBytes = 8 * 1024,
	maxNameBytes = 64,
	defaultHoldSeconds = 0.05,
}

export type Action = {
	kind: "key" | "key_press" | "mouse_move" | "mouse_delta" | "mouse_button" | "click" | "text" | "wait",
	key: string?,
	button: string?,
	x: number?,
	y: number?,
	text: string?,
	down: boolean?,
	repeatKey: boolean?,
	repeatCount: number?,
	-- The planned blocking time of this action (hold time for key_press and
	-- click, the wait time for wait, 0 otherwise)
	seconds: number,
}

export type Plan = {
	actions: { Action },
	totalSeconds: number,
	budgetSeconds: number,
}

local function invalid(message: string): never
	error({ __remoteError = true, code = "INVALID_ARGUMENT", message = message }, 0)
end

local function isFinite(value: any): boolean
	return type(value) == "number" and value == value and value > -math.huge and value < math.huge
end

local function validateCoordinate(value: any, index: number, field: string): number
	if value == nil then
		return 0
	end

	if not isFinite(value) then
		invalid(`action {index}: {field} must be a finite number`)
	end

	return value :: number
end

local function validateSeconds(value: any, defaultSeconds: number, index: number, field: string): number
	if value == nil then
		return defaultSeconds
	end

	if not isFinite(value) or (value :: number) < 0 then
		invalid(`action {index}: {field} must be a finite non-negative number`)
	end

	if (value :: number) > InputActions.LIMITS.maxActionSeconds then
		invalid(`action {index}: {field} exceeds the {InputActions.LIMITS.maxActionSeconds}s per-action limit`)
	end

	return value :: number
end

--- The enum-member name for `key`/`button` fields: structurally validated
--- here, resolved against the real Enum by the runtime.
local function validateName(value: any, index: number, field: string): string
	if type(value) ~= "string" or value == "" then
		invalid(`action {index}: {field} must be a non-empty string`)
	end

	if #(value :: string) > InputActions.LIMITS.maxNameBytes then
		invalid(`action {index}: {field} exceeds {InputActions.LIMITS.maxNameBytes} bytes`)
	end

	return value :: string
end

--- Validate the request timeout (milliseconds; ≤30 000 pinned). Returns the
--- effective timeout in milliseconds.
function InputActions.validateTimeout(timeoutMs: any): number
	if timeoutMs == nil then
		return InputActions.LIMITS.defaultTimeoutMs
	end

	if
		not isFinite(timeoutMs)
		or timeoutMs % 1 ~= 0
		or timeoutMs < 100
		or timeoutMs > InputActions.LIMITS.maxTimeoutMs
	then
		invalid(`timeoutMs must be an integer of 100-{InputActions.LIMITS.maxTimeoutMs}`)
	end

	return timeoutMs :: number
end

--- Validate and normalize one action list against the pinned limits. Raises
--- INVALID_ARGUMENT on the first violation; returns the fully-planned action
--- sequence with its total blocking time already proven to fit the budget.
function InputActions.validate(actions: any, timeoutMs: any): Plan
	local effectiveTimeout = InputActions.validateTimeout(timeoutMs)

	if type(actions) ~= "table" then
		invalid("actions must be an array of action objects")
	end

	local list = actions :: { any }
	local count = #list

	if count < 1 then
		invalid("actions must contain at least one action")
	end

	if count > InputActions.LIMITS.maxActions then
		invalid(`actions exceed the {InputActions.LIMITS.maxActions}-action limit`)
	end

	local budgetSeconds = math.min(effectiveTimeout / 1000, InputActions.LIMITS.maxActionSeconds)
	local planned: { Action } = table.create(count)
	local totalSeconds = 0

	for index = 1, count do
		local action = list[index]

		if type(action) ~= "table" then
			invalid(`action {index} must be an object`)
		end

		local kind = action.kind or action.type
		local normalized: Action

		if kind == "key" then
			normalized = {
				kind = "key",
				key = validateName(action.key, index, "key"),
				down = action.down ~= false,
				repeatKey = action.repeatKey == true,
				seconds = 0,
			}
		elseif kind == "key_press" then
			normalized = {
				kind = "key_press",
				key = validateName(action.key, index, "key"),
				seconds = validateSeconds(action.duration, InputActions.LIMITS.defaultHoldSeconds, index, "duration"),
			}
		elseif kind == "mouse_move" or kind == "mouse_delta" then
			normalized = {
				kind = kind,
				x = validateCoordinate(action.x, index, "x"),
				y = validateCoordinate(action.y, index, "y"),
				seconds = 0,
			}
		elseif kind == "mouse_button" then
			local repeatCount = 0

			if action.repeatCount ~= nil then
				if not isFinite(action.repeatCount) or action.repeatCount % 1 ~= 0 or action.repeatCount < 0 then
					invalid(`action {index}: repeatCount must be a non-negative integer`)
				end

				repeatCount = action.repeatCount
			end

			normalized = {
				kind = "mouse_button",
				button = if action.button == nil then "MouseButton1" else validateName(action.button, index, "button"),
				x = validateCoordinate(action.x, index, "x"),
				y = validateCoordinate(action.y, index, "y"),
				down = action.down ~= false,
				repeatCount = repeatCount,
				seconds = 0,
			}
		elseif kind == "click" then
			normalized = {
				kind = "click",
				button = if action.button == nil then "MouseButton1" else validateName(action.button, index, "button"),
				x = validateCoordinate(action.x, index, "x"),
				y = validateCoordinate(action.y, index, "y"),
				seconds = validateSeconds(action.duration, InputActions.LIMITS.defaultHoldSeconds, index, "duration"),
			}
		elseif kind == "text" then
			if type(action.text) ~= "string" or action.text == "" then
				invalid(`action {index}: text must be a non-empty string`)
			end

			if #(action.text :: string) > InputActions.LIMITS.maxTextBytes then
				invalid(`action {index}: text exceeds {InputActions.LIMITS.maxTextBytes} bytes`)
			end

			normalized = {
				kind = "text",
				text = action.text,
				seconds = 0,
			}
		elseif kind == "wait" then
			normalized = {
				kind = "wait",
				seconds = validateSeconds(
					if action.seconds ~= nil then action.seconds else action.duration,
					0,
					index,
					"seconds"
				),
			}
		else
			invalid(
				`action {index}: unknown action type '{tostring(kind)}' — expected key, key_press, mouse_move, `
					.. "mouse_delta, mouse_button, click, text, or wait"
			)
			error("unreachable")
		end

		totalSeconds += normalized.seconds
		planned[index] = normalized
	end

	if totalSeconds > budgetSeconds then
		invalid(
			`the planned action durations total {string.format("%.2f", totalSeconds)}s, `
				.. `exceeding the {string.format("%.2f", budgetSeconds)}s budget of this request`
		)
	end

	return {
		actions = planned,
		totalSeconds = totalSeconds,
		budgetSeconds = budgetSeconds,
	}
end

return InputActions
