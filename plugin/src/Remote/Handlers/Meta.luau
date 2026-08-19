--!strict
-- Meta ops: `ping`, `version`, `capabilities` (ping.json, version.json,
-- capabilities.json).
--
-- `capabilities` is the feature-negotiation surface (Design §16: partial op
-- surfaces must degrade cleanly): it reports every cataloged op with an
-- explicit boolean — implemented ops true, everything else false — plus the
-- plugin-side limits so agents can bound their requests without guessing.

local RunService = game:GetService("RunService")

local WSync = script:FindFirstAncestor("WSync")

local Host = require(WSync.Playtest.Host)

local Codec = require(script.Parent.Parent.Codec)
local LogBuffer = require(script.Parent.Parent.LogBuffer)
local Selector = require(script.Parent.Parent.Selector)

local Inspect = require(script.Parent.Inspect)
local Query = require(script.Parent.Query)
local Selection = require(script.Parent.Selection)
local Logs = require(script.Parent.Logs)
local Bulk = require(script.Parent.Bulk)
local Capture = require(script.Parent.Capture)
local Transmit = require(script.Parent.Transmit)
local Clipboard = require(script.Parent.Clipboard)
local PlaytestOps = require(script.Parent.Playtest)

type Args = { [string]: any }
type Context = { protocol: number, pluginVersion: string, ops: { [string]: any } }

-- The full remote-op catalog (Design §5.4). Everything the dispatcher does
-- not implement is reported `false` so daemons and agents never probe blind.
local CATALOG = {
	"get",
	"set",
	"ls",
	"tree",
	"props",
	"query",
	"find",
	"find_by_attr",
	"new",
	"rm",
	"mv",
	"class_info",
	"enum_list",
	"enums",
	"set_attr",
	"rm_attr",
	"attr_ls",
	"add_tag",
	"rm_tag",
	"tag_ls",
	"call",
	"select_get",
	"select_set",
	"eval",
	"logs",
	"save",
	"undo",
	"redo",
	"waypoint",
	"version",
	"ping",
	"capabilities",
	"transaction_begin",
	"transaction_finish",
	"read_subtree",
	"source_read",
	"clipboard_copy",
	"clipboard_read",
	"clipboard_paste_begin",
	"clipboard_paste_chunk",
	"clipboard_paste_commit",
	"capture_prepare",
	"capture_read",
	"capture_close",
	"transmit_prepare",
	"transmit_read",
	"transmit_close",
	"playtest_start",
	"playtest_run_start",
	"playtest_status",
	"playtest_run_poll",
	"playtest_contexts",
	"playtest_wait",
	"playtest_exec",
	"playtest_logs",
	"playtest_ui",
	"playtest_input",
	"playtest_request",
	"playtest_capture",
	"playtest_screenshot",
	"playtest_video",
	"playtest_stop",
	"playtest_run_cancel",
	"open_in_editor",
}

local Meta = {}

Meta.ops = {} :: { [string]: (args: Args, ctx: Context) -> any }

function Meta.ops.ping(_args: Args, _ctx: Context)
	return { pong = true }
end

function Meta.ops.version(_args: Args, ctx: Context)
	local studioVersion: string? = nil

	local ok, version = pcall(function()
		return RunService:GetRobloxVersion()
	end)

	if ok then
		studioVersion = tostring(version)
	end

	return {
		pluginVersion = ctx.pluginVersion,
		protocol = ctx.protocol,
		studioVersion = studioVersion,
	}
end

function Meta.ops.capabilities(_args: Args, ctx: Context)
	local features: { [string]: boolean } = {}

	for _, op in CATALOG do
		features[op] = ctx.ops[op] ~= nil
	end

	-- Implemented ops missing from the catalog would otherwise be invisible
	for op in ctx.ops do
		if features[op] == nil then
			features[op] = true
		end
	end

	-- Session identity for CLI workflow preconditions (`expectedMode` /
	-- `expectedPlaceId`): both mode fields derive from the same
	-- RunService-based detection (Playtest/Host) so they can never disagree.
	-- Only the edit copy serves the op surface, so this reports `edit`/`Edit`
	-- in practice — runtime playtest copies never answer these ops.
	local hostType = Host.detect()

	return {
		protocol = ctx.protocol,
		plugin = ctx.pluginVersion,
		mode = if hostType == "Edit" then "edit" else string.lower(hostType),
		hostDataModelType = hostType,
		placeId = game.PlaceId,
		features = features,
		limits = {
			codec = {
				maxDepth = Codec.LIMITS.maxDepth,
				maxNodes = Codec.LIMITS.maxNodes,
				maxTableEntries = Codec.LIMITS.maxTableEntries,
				maxStringBytes = Codec.LIMITS.maxStringBytes,
			},
			query = {
				maxLimit = Query.LIMITS.maxLimit,
				defaultLimit = Query.LIMITS.defaultLimit,
				maxSelectorBytes = Selector.MAX_SELECTOR_BYTES,
				maxSelectorSegments = Selector.MAX_SEGMENTS,
				maxProjectedProps = Query.LIMITS.maxProjectedProps,
			},
			find = {
				defaultLimit = Query.LIMITS.findDefaultLimit,
				maxLimit = Query.LIMITS.maxLimit,
			},
			tree = {
				defaultDepth = Inspect.LIMITS.treeDefaultDepth,
				maxDepth = Inspect.LIMITS.treeMaxDepth,
				maxNodes = Inspect.LIMITS.treeMaxNodes,
			},
			ls = {
				maxChildren = Inspect.LIMITS.maxChildren,
			},
			logs = {
				capacity = LogBuffer.CAPACITY,
				maxMessageBytes = LogBuffer.MAX_MESSAGE_BYTES,
				defaultLimit = Logs.LIMITS.defaultLimit,
				maxLimit = Logs.LIMITS.maxLimit,
			},
			select = {
				maxPaths = Selection.LIMITS.maxPaths,
			},
			bulk = {
				sourceReadMaxLen = Bulk.LIMITS.sourceReadMaxLen,
				sourceTtlSeconds = Bulk.LIMITS.sourceTtlSeconds,
				subtreeMaxNodes = Bulk.LIMITS.subtreeMaxNodes,
			},
			capture = {
				maxAxis = Capture.LIMITS.maxAxis,
				maxPixels = Capture.LIMITS.maxPixels,
				maxSessions = Capture.LIMITS.maxSessions,
				readMaxLen = Capture.LIMITS.readMaxLen,
				ttlSeconds = Capture.LIMITS.ttlSeconds,
				minPadding = Capture.LIMITS.minPadding,
				maxPadding = Capture.LIMITS.maxPadding,
				minFov = Capture.LIMITS.minFov,
				maxFov = Capture.LIMITS.maxFov,
				defaultModelSize = Capture.LIMITS.defaultModelSize,
			},
			-- Transmit items are ordinary capture sessions, so `readMaxLen`
			-- and `ttlSeconds` repeat the capture bounds on purpose: the
			-- daemon pulls them with `capture_read`/`capture_close`
			-- (`transmit_read`/`transmit_close` report false in `features`).
			transmit = {
				maxItems = Transmit.LIMITS.maxItems,
				maxAxis = Transmit.LIMITS.maxAxis,
				maxPixels = Transmit.LIMITS.maxPixels,
				readMaxLen = Transmit.LIMITS.readMaxLen,
				ttlSeconds = Transmit.LIMITS.ttlSeconds,
			},
			clipboard = {
				maxRoots = Clipboard.LIMITS.maxRoots,
				maxBytes = Clipboard.LIMITS.maxBytes,
				readMaxLen = Clipboard.LIMITS.readMaxLen,
				chunkMaxLen = Clipboard.LIMITS.chunkMaxLen,
				ttlSeconds = Clipboard.LIMITS.ttlSeconds,
				maxPasteSessions = Clipboard.LIMITS.maxPasteSessions,
			},
			playtest = {
				maxScriptBytes = PlaytestOps.LIMITS.maxScriptBytes,
				eventBytes = PlaytestOps.LIMITS.eventBytes,
				resultBytes = PlaytestOps.LIMITS.resultBytes,
				execDefaultTimeoutMs = PlaytestOps.LIMITS.execDefaultTimeoutMs,
				execMaxTimeoutMs = PlaytestOps.LIMITS.execMaxTimeoutMs,
				inputMaxActions = PlaytestOps.LIMITS.inputMaxActions,
				inputMaxTimeoutMs = PlaytestOps.LIMITS.inputMaxTimeoutMs,
				runMaxTimeoutSec = PlaytestOps.LIMITS.runMaxTimeoutSec,
				pollMaxRecords = PlaytestOps.LIMITS.pollMaxRecords,
				waitMaxTimeoutMs = PlaytestOps.LIMITS.waitMaxTimeoutMs,
			},
		},
		-- Path grammar (coordinator ruling): `/`-separated Studio names, no
		-- escaping in v1, empty string = the DataModel.
		paths = {
			separator = "/",
			escaped = false,
		},
	}
end

return Meta
