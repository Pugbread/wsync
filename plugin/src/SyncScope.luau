--!strict
-- Sync scope (Design §7.0 — the Studio-first ruling). The daemon's hello
-- (`GET /hello` JSON and the WS server hello) carries `scope: "code"|"full"`
-- (absent = full):
--
--   * `code` — the one syncing method: exactly `Folder`, `Script`,
--     `LocalScript`, `ModuleScript` participate in sync, in every direction;
--     folders count even when empty. Everything else is Studio-authoritative:
--     never written to disk, never deleted from Studio (the allowlist is the
--     structural KeepUnknowns guarantee). Connect is promptless — the daemon
--     auto-applies Studio → disk after the compare commit.
--   * `full` — the legacy power-user projection; the pre-ruling resolver flow
--     and settings semantics stay as they were.
--
-- This module is the single owner of the allowlist, the current session's
-- scope, and the fixed-value resolution for the settings §7.0 retires. It is
-- deliberately require-free (the Protocol/Policy pattern) so a standalone
-- `luau` binary can load it for behavior tests; Core stamps the scope after
-- the hello and resets it on disconnect.

local SyncScope = {}

export type Scope = "code" | "full"

--- The code-scope class allowlist (pinned): the only classes that sync when
--- the daemon declares `scope: "code"`. `Folder` is unconditional — empty
--- folders sync too, which is exactly what the old `isScriptRelated` gate got
--- wrong for this mode.
---
--- The `StarterPlayerScripts` / `StarterCharacterScripts` containers are here
--- for the same reason `Folder` is: they are the path a script lives behind,
--- not content of their own. Leaving them out dropped every diff entry under
--- `StarterPlayer` from the upload, so the daemon saw that subtree as already
--- in sync and never wrote those scripts to disk. They are structural only —
--- Studio always provides them, so they are created as plain directories and
--- never deleted from Studio.
SyncScope.CODE_CLASSES = {
	Folder = true,
	Script = true,
	LocalScript = true,
	ModuleScript = true,
	StarterPlayerScripts = true,
	StarterCharacterScripts = true,
} :: { [string]: boolean }

local current: Scope = "full"

--- Map a hello `scope` field onto a scope: only an explicit `"code"` opts in;
--- absent or anything else means full (pinned contract).
function SyncScope.normalize(value: any): Scope
	return if value == "code" then "code" else "full"
end

--- Stamp the session scope (Core, right after the transport hello resolves).
function SyncScope.set(value: any)
	current = SyncScope.normalize(value)
end

--- Back to the full-scope default (Core stop / fresh connect attempt).
function SyncScope.reset()
	current = "full"
end

function SyncScope.get(): Scope
	return current
end

function SyncScope.isCode(): boolean
	return current == "code"
end

--- Is this class inside the code-scope allowlist?
function SyncScope.isCodeClass(className: string?): boolean
	return className ~= nil and SyncScope.CODE_CLASSES[className] == true
end

--------------------------------------------------------------------------------
-- Retired settings (Design §7.0: "no settings")
--------------------------------------------------------------------------------

--- Fixed-value resolution for the nine settings §7.0 retires. Returns
--- `(true, value)` when `setting` is retired — the value applies regardless
--- of any stored override — or `(false, nil)` for live settings. Two return
--- values because `false` is a legitimate fixed value (SyncbackProperties).
function SyncScope.resolveRetired(setting: string, scope: Scope): (boolean, any)
	if setting == "TwoWaySync" then
		-- Two-way sync is the product's point; never off.
		return true, true
	elseif setting == "InitialSyncPriority" then
		-- Only the full-scope legacy resolver path reads it; fixed "Server".
		return true, "Server"
	elseif setting == "SyncbackProperties" then
		return true, false
	elseif setting == "LiveHydrate" then
		return true, true
	elseif setting == "OverridePackages" then
		return true, true
	elseif setting == "OnlyCodeMode" then
		-- Superseded by the hello scope — derived, never stored.
		return true, scope == "code"
	elseif setting == "DisplayPrompts" then
		-- A code-scope connect never prompts (discovery's strict GameId match
		-- replaced the mismatch prompt); the full-scope legacy flow keeps the
		-- pre-ruling default.
		return true, if scope == "code" then "Never" else "Always"
	elseif setting == "ChangesThreshold" then
		-- Ignored on code scope: incoming syncs auto-apply (the daemon-side
		-- conflict engine is the guard). Full scope keeps the default gate.
		return true, if scope == "code" then math.huge else 5
	elseif setting == "KeepUnknowns" then
		-- Effectively true on code scope — non-code instances are never
		-- deleted, guaranteed structurally by the allowlist. Full scope keeps
		-- the pre-ruling default so its divergence diff still enumerates
		-- unknowns.
		return true, scope == "code"
	end

	return false, nil
end

--------------------------------------------------------------------------------
-- Diff-entry filtering (the plugin-side half of the compare upload contract)
--------------------------------------------------------------------------------

type MappedEntries = {
	entries: { any },
	pendingRemovals: { [string]: any },
	skipped: number,
}

--- Filter a mapped `/compare` upload (Divergence Protocol `MappedEntries`)
--- down to the code-scope allowlist: only the four classes enter the upload,
--- and pending Studio-only removals whose entries were dropped are unregistered
--- with them (a Part never becomes a transfer target). The daemon drops
--- out-of-scope entries too — this is the plugin-side half of that contract.
--- Returns the filtered set and the dropped-entry count.
function SyncScope.filterMappedEntries(mapped: MappedEntries): (MappedEntries, number)
	local entries = {}
	local pendingRemovals = {}
	local dropped = 0

	for _, entry in mapped.entries do
		if SyncScope.isCodeClass((entry :: any).class) then
			table.insert(entries, entry)

			local ref = (entry :: any).ref
			local pending = if ref ~= nil then mapped.pendingRemovals[ref] else nil

			if pending ~= nil then
				pendingRemovals[ref] = pending
			end
		else
			dropped += 1
		end
	end

	return {
		entries = entries,
		pendingRemovals = pendingRemovals,
		skipped = mapped.skipped,
	}, dropped
end

return SyncScope
