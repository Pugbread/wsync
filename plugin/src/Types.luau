-- General structs

export type Ref = buffer
export type ClassName = string

export type Properties = {
	[string]: any,
}

export type Meta = {
	keepUnknowns: boolean,
	meshSource: string?,
}

-- Snapshots

export type Snapshot = {
	id: Ref,
	meta: Meta,
	name: string,
	class: ClassName,
	properties: Properties,
	children: { Snapshot },
}

export type AddedSnapshot = {
	id: Ref,
	meta: Meta,
	parent: Ref,
	name: string,
	class: ClassName,
	properties: Properties,
	children: { Snapshot },
}

export type UpdatedSnapshot = {
	id: Ref,
	meta: Meta?,
	name: string?,
	class: ClassName?,
	properties: Properties?,
}

-- Other structs

export type WatcherEvent = {
	kind: "Added" | "Removed" | "Changed",
	instance: Instance,
	property: string?,
}

export type Changes = {
	additions: { AddedSnapshot },
	updates: { UpdatedSnapshot },
	removals: { Ref | Instance },
}

export type Project = {
	name: string,
	version: string,
	gameId: number?,
	placeIds: { number },
	rootRefs: { Ref },
}

--- `GET /hello` identity payload (Design §5.2), used by discovery and by
--- transport selection
export type DaemonHello = {
	name: string?,
	version: string?,
	protocol: number,
	-- Sync scope (Design §7.0): "code" | "full"; absent = full
	scope: string?,
	project: string?,
	canonicalProject: string?,
	gameId: number?,
	placeIds: { number }?,
	bootId: string?,
	pid: number?,
	port: number?,
	managedBy: string?,
	projectInit: boolean?,
}

--- Transport lifecycle updates (WebSocket reconnect backoff has a visible
--- countdown per Design §9.4)
export type TransportStatus = {
	kind: "connecting" | "connected" | "disconnected" | "reconnecting",
	attempt: number?,
	secondsLeft: number?,
}

--- Connect-time divergence decision state surfaced to the UI (Design §7.3 as
--- amended): `waiting` shows the "Waiting for decision in the WSync app…"
--- page (its `cancel` submits `{choice:"cancel"}` while the connection stays
--- alive); `resolved` lets the normal connect flow resume.
export type DivergenceStatus = {
	kind: "waiting" | "resolved",
	choice: string?,
	cancel: (() -> ())?,
}

-- Messages

export type Message = SyncChanges | SyncDetails | ExecuteCode | Disconnect | DivergenceEvent | DispatchRequest
export type MessageKind =
	"SyncChanges"
	| "SyncDetails"
	| "ExecuteCode"
	| "Disconnect"
	| "DivergenceEvent"
	| "DispatchRequest"

export type SyncChanges = {
	SyncChanges: Changes,
}

export type SyncDetails = {
	SyncDetails: Project,
}

export type ExecuteCode = {
	code: string,
}

export type Disconnect = {
	message: string,
	-- Set by the WebSocket transport for `shutdown{retryable = false}` —
	-- fatal disconnects must halt every retry loop (Design §9.4)
	fatal: boolean?,
}

--- Divergence-decision lifecycle surfaced from the server (Design §7.2/§7.3):
--- `choice-needed`/`choice-made` event frames on the WebSocket transport, and
--- the synthetic `choice-vanished` topic Core's backstop poll injects when
--- the pending set disappeared without a decision event. `disk-review`
--- (Design §7.0, code scope) is the passive review-count push after the
--- daemon auto-applied Studio → disk — it feeds the Connected-page indicator
--- and never blocks anything.
export type DivergenceEvent = {
	DivergenceEvent: {
		topic: "choice-needed" | "choice-made" | "choice-vanished" | "disk-review",
		choiceId: string?,
		choice: string?,
		reviewId: string?,
		total: number?,
		diskOnly: number?,
		differs: number?,
	},
}

--- A daemon `request` op that must stay ordered with the sync-frame queue
--- (`transaction_begin`/`transaction_finish` — pinned wave-4 contract): the
--- WebSocket transport enqueues the raw frame and Core dispatches it via
--- `transport:performRequest` once every earlier queued frame is applied.
export type DispatchRequest = {
	DispatchRequest: { [string]: any },
}

return nil
