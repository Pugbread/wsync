local RunService = game:GetService("RunService")
local TweenService = game:GetService("TweenService")

local plugin = script:FindFirstAncestorWhichIsA("Plugin")

local WSync = script:FindFirstAncestor("WSync")
local Packages = WSync.Packages
local Helpers = WSync.Helpers

local Components = script.Components
local Widgets = script.Widgets
local Pages = script.Pages

local Fusion = require(Packages.Fusion)
local Signal = require(Packages.Signal)

local manifest = require(WSync.manifest)
local Config = require(WSync.Config)
local Types = require(WSync.Types)
local Util = require(WSync.Util)
local Log = require(WSync.Log)
local Core = require(WSync.Core)
local Discovery = require(WSync.Discovery)
local Policy = require(WSync.Discovery.Policy)
local describeChanges = require(Helpers.describeChanges)

local Assets = require(script.Assets)
local Loader = require(script.Loader)
local Theme = require(script.Theme)

local Toolbar = require(Components.Plugin.Toolbar)
local ToolbarButton = require(Components.Plugin.ToolbarButton)
local Widget = require(Components.Plugin.Widget)
local Container = require(Components.Container)
local Padding = require(Components.Padding)
local Text = require(Components.Text)
local List = require(Components.List)

local NotConnected = require(Pages.NotConnected)
local Connecting = require(Pages.Connecting)
local Connected = require(Pages.Connected)
local Unavailable = require(Pages.Unavailable)
local Prompt = require(Pages.Prompt)
local Error = require(Pages.Error)

local Settings = require(Widgets.Settings)
local Help = require(Widgets.Help)
local Diff = require(Widgets.Diff)

local New = Fusion.New
local Value = Fusion.Value
local Spring = Fusion.Spring
local OnEvent = Fusion.OnEvent
local OnChange = Fusion.OnChange
local Observer = Fusion.Observer
local Computed = Fusion.Computed
local Children = Fusion.Children
local cleanup = Fusion.cleanup
local peek = Fusion.peek

local RECONNECT_INTERVAL = 5

local App = {}
App.__index = App

function App.new()
	Loader.load()

	local self = setmetatable({}, App)

	self.core = nil
	self.lastUpdate = os.clock()
	self.onCanceled = Signal.new()

	-- The address discovery resolved for the current/last connection —
	-- displayed while connected and handed to Core; the Settings values are
	-- only ever read as scan candidates (Design §9.4 as amended)
	self.host = Config:get("Host")
	self.port = Config:get("Port")
	-- Bumped on every connect/disconnect; stale async flows (pending
	-- connects) check it and abort
	self.connectGeneration = 0
	-- Debounces the status card's action button (a double-click must not
	-- start a scan and immediately cancel it)
	self.lastFlowAction = 0

	self.lastSync = Value(os.time())
	self.lastSyncKind = Value("Unknown")
	self.transportStatus = Value(nil)
	-- The passive disk-review line on the Connected page (Design §7.0, code
	-- scope): the indicator string, or nil when nothing awaits review
	self.diskReview = Value(nil)
	-- The connection state machine driving the Not Connected status card
	-- (Design §9.2 as amended; pure transitions in Discovery.Policy)
	self.flow = Value(Policy.initialState())

	self.rootSize = Value(Vector2.new(300, 200))
	self.icon = Value(Assets.WSync.Logo)
	self.pages = Value({})

	self.settingsWidget = Value(nil)
	self.helpWidget = Value(nil)
	self.diffWidget = Value(nil)

	local isOpen = Value(false)

	local toolbarButton = ToolbarButton {
		Toolbar = Toolbar {
			Name = "WSync",
		},
		Name = "WSync",
		ToolTip = "Open WSync UI",
		Image = self.icon,
		[OnEvent "Click"] = function()
			isOpen:set(not peek(isOpen))
		end,
	}

	Widget {
		Name = "WSync",
		MinimumSize = peek(self.rootSize),
		Enabled = isOpen,
		[OnChange "Enabled"] = function(isEnabled)
			isOpen:set(isEnabled)
		end,
		[OnChange "AbsoluteSize"] = function(size)
			self.rootSize:set(size)
		end,
		[Children] = self.pages,
	}

	plugin.Unloading:Connect(Observer(isOpen):onChange(function()
		toolbarButton:SetActive(peek(isOpen))
	end))

	toolbarButton:SetActive(peek(isOpen))

	if RunService:IsEdit() then
		self:home()

		if Config:get("AutoConnect") then
			self:connect()
		end
	else
		self:setPage(Unavailable())
	end

	return self
end

function App:setIcon(icon: ("Ok" | "Warn" | "Error")?)
	if icon == "Ok" then
		self.icon:set(Assets.WSync.LogoOk)
	elseif icon == "Warn" then
		self.icon:set(Assets.WSync.LogoWarn)
	elseif icon == "Error" then
		self.icon:set(Assets.WSync.LogoError)
	else
		self.icon:set(Assets.WSync.Logo)
	end
end

function App:setPage(page)
	local pages = peek(self.pages)

	for id, child in pages do
		if child._finished then
			page[id] = nil
			continue
		end

		task.spawn(function()
			child._destructor(child._value)
			child._finished = true
		end)
	end

	local size = Value(UDim2.fromScale(1, 1))

	-- Extremely hacky way to smoothly transition between pages
	page = Computed(function()
		return New "Frame" {
			AnchorPoint = Vector2.new(0.5, 0.5),
			Position = UDim2.fromScale(0.5, 0.5),
			Size = Spring(size, 10),
			BackgroundColor3 = Theme.Colors.Background,
			BorderSizePixel = 0,
			[Children] = {
				List {
					VerticalAlignment = Enum.VerticalAlignment.Center,
					HorizontalAlignment = Enum.HorizontalAlignment.Center,
					Spacing = 8,
				},
				Padding {
					Padding = Theme.WidgetPadding,
				},
				Container {
					Size = UDim2.fromScale(1, 0),
					[Children] = {
						-- Text wordmark instead of Argon's banner image
						-- (TODO(phase-3-polish): dedicated WSync banner asset).
						Text {
							Size = UDim2.fromOffset(155, 35),
							AutomaticSize = Enum.AutomaticSize.None,
							Text = "WSync",
							Font = Theme.Fonts.Bold,
							Color = Theme.Colors.Brand,
							Scaled = true,
							TextSize = 32,
							TextXAlignment = Enum.TextXAlignment.Left,
						},
						Text {
							AnchorPoint = Vector2.new(1, 1),
							Position = UDim2.new(1, -3, 1, 0),
							Text = `v{manifest.package.version}`,
							Color = Theme.Colors.TextDimmed,
							TextSize = Theme.TextSize.Small,
						},
					},
				},
				Container {
					Size = UDim2.fromScale(1, 0),
					[Children] = page,
				},
			},
		}
	end, function(page)
		if not page then
			return
		end

		local TWEEN_INFO = TweenInfo.new(0.2, Enum.EasingStyle.Sine, Enum.EasingDirection.Out)
		TweenService:Create(page, TWEEN_INFO, { BackgroundTransparency = 1 }):Play()

		for _, descendant in page:GetDescendants() do
			local properties = {}

			if descendant:IsA("GuiObject") then
				if descendant:IsA("GuiButton") then
					descendant.Active = false
				end

				properties.BackgroundTransparency = 1

				if descendant:IsA("TextLabel") or descendant:IsA("TextButton") or descendant:IsA("TextBox") then
					properties.TextTransparency = 1
				elseif descendant:IsA("ImageLabel") or descendant:IsA("ImageButton") then
					properties.ImageTransparency = 1
				end
			end

			if descendant:IsA("UIStroke") then
				properties.Transparency = 1
			end

			if next(properties) then
				TweenService:Create(descendant, TWEEN_INFO, properties):Play()
			end
		end

		page.ZIndex = 2

		size:set(UDim2.fromScale(1.2, 1))

		task.wait(0.2)
		cleanup(page)
	end)

	pages[Util.generateGUID()] = page
	self.pages:set(pages)
end

function App:home()
	self.lastUpdate = os.clock()

	self:setPage(NotConnected {
		App = self,
	})

	self:setIcon()
end

function App:settings()
	local widget = peek(self.settingsWidget)

	if widget then
		widget:Destroy()
		widget = nil
	else
		widget = Settings {
			Closed = function()
				peek(self.settingsWidget):Destroy()
				self.settingsWidget:set(nil)
			end,
		}
	end

	self.settingsWidget:set(widget)
end

function App:help()
	local widget = peek(self.helpWidget)

	if widget then
		widget:Destroy()
		widget = nil
	else
		widget = Help {
			Closed = function()
				peek(self.helpWidget):Destroy()
				self.helpWidget:set(nil)
			end,
		}
	end

	self.helpWidget:set(widget)
end

function App:diff(changes: Types.Changes?, tree: any?)
	local widget = peek(self.diffWidget)

	if widget then
		widget:Destroy()
		widget = nil
	elseif changes and tree then
		widget = Diff {
			Changes = changes,
			Tree = tree,
			Closed = function()
				peek(self.diffWidget):Destroy()
				self.diffWidget:set(nil)
			end,
		}
	end

	self.diffWidget:set(widget)
end

--- Advance the connection state machine (pure transitions in
--- Discovery.Policy) and run whatever effect the transition asks for. Every
--- async flow re-enters here through completion events tagged with the
--- generation it started under; Policy drops the stale ones.
function App:dispatch(event)
	local current = peek(self.flow)
	local state, effect = Policy.step(current, event)

	if state == current then
		-- Ignored event (wrong state or stale generation)
		return
	end

	self.flow:set(state)

	if effect == "scan" then
		self:__runScan(state)
	elseif effect == "connect" then
		self:__beginConnect(state)
	elseif effect == "create" then
		self:__runCreate(state)
	elseif effect == "wait" then
		self:__runWait(state)
	end
end

--- Start discovery — the only connect entry point (AutoConnect on place
--- open, the status card's Connect button, AutoReconnect). A no-op unless
--- the machine is idle.
function App:connect()
	self:dispatch({ kind = "start" })
end

--- The status card's single dynamic action button.
function App:flowAction()
	local now = os.clock()

	if now - self.lastFlowAction < 0.25 then
		return
	end

	self.lastFlowAction = now

	local name = peek(self.flow).name

	if name == "idle" then
		self:dispatch({ kind = "start" })
	elseif name == "scanning" or name == "waiting-daemon" then
		self:dispatch({ kind = "cancel" })
	elseif name == "found" then
		-- A connect is already underway; tear it down (disconnect resets the
		-- machine back to idle)
		self:disconnect(true)
	elseif name == "create-ready" then
		self:dispatch({ kind = "create" })
	elseif name == "create-confirm" then
		self:dispatch({ kind = "confirm" })
	elseif name == "no-broker" or name == "error" then
		self:dispatch({ kind = "retry" })
	end
end

--- The confirm step's Back button (Escape does the same).
function App:flowBack()
	self:dispatch({ kind = "back" })
end

--- Probe the candidate list (Settings `Host:Port` first, then the local
--- range); no match falls through to the broker probe and the Create Project
--- offer (explicit two-step confirm, never automatic).
function App:__runScan(state)
	local generation = state.generation

	local function continuing()
		local flow = peek(self.flow)
		return flow.generation == generation and flow.name == "scanning"
	end

	task.spawn(function()
		local match = Discovery.scan(continuing):expect()

		if not continuing() then
			return
		end

		if match then
			-- Connect-stage line (Design §7.0): the scan result, so a stalled
			-- connect is diagnosable from the Output window
			Log.stage(
				`Connect: discovery matched '{match.hello.project or match.hello.name}'`
					.. ` on {match.host}:{match.port}`
			)

			self:dispatch({
				kind = "scan-found",
				generation = generation,
				host = match.host,
				port = match.port,
				project = match.hello.project or match.hello.name,
			})

			return
		end

		local broker = Discovery.findBroker(continuing):expect()

		if not continuing() then
			return
		end

		if broker then
			local known = Discovery.brokerKnowsGame(broker.hello)

			Log.stage(
				"Connect: discovery found no matching daemon (project broker present"
					.. if known then ", knows this game)" else ")"
			)
			self:dispatch({ kind = "scan-none-broker", generation = generation, broker = broker, known = known })
		else
			Log.stage("Connect: discovery found no matching daemon and no project broker")
			self:dispatch({ kind = "scan-none-no-broker", generation = generation })
		end
	end)
end

--- Hand the resolved address to Core — discovery is over at this point and
--- the existing connect flow (transport selection included) takes it from
--- here unchanged.
function App:__beginConnect(state)
	self.host = state.host
	self.port = state.port

	self.connectGeneration += 1
	self.transportStatus:set(nil)
	self.diskReview:set(nil)

	self:__connectTo(state.host, state.port, self.connectGeneration)
end

--- POST the open place's metadata to the desktop broker (pinned wave-5
--- contract). Reached exclusively through the confirm step.
function App:__runCreate(state)
	local generation = state.generation
	local broker = state.broker

	task.spawn(function()
		local ok, result = pcall(function()
			return Discovery.createProject(broker):expect()
		end)

		if ok then
			self:dispatch({ kind = "create-ok", generation = generation })
		else
			-- Broker unreachable (or refusing) mid-flow: error state with an
			-- explicit retry
			local message = if typeof(result) == "string" then result else nil

			self:dispatch({ kind = "create-error", generation = generation, message = message })
		end
	end)
end

--- Project created — wait for its daemon (bounded retries, cancellable from
--- the page; the re-scan applies the same strict-GameId policy, which the
--- fresh project satisfies since the broker stamps its gameId).
function App:__runWait(state)
	local generation = state.generation

	local function continuing()
		local flow = peek(self.flow)
		return flow.generation == generation and flow.name == "waiting-daemon"
	end

	task.spawn(function()
		local match = Discovery.waitForDaemon(function(attempt, total)
			if continuing() then
				self:dispatch({ kind = "wait-progress", generation = generation, attempt = attempt, total = total })
			end
		end, continuing):expect()

		if not continuing() then
			return
		end

		if match then
			self:dispatch({
				kind = "wait-found",
				generation = generation,
				host = match.host,
				port = match.port,
				project = match.hello.project or match.hello.name,
			})
		else
			self:dispatch({ kind = "wait-exhausted", generation = generation })
		end
	end)
end

function App:__connectTo(host: string, port: number, generation: number)
	local project = Value({})
	local canceled = false
	local loading = true

	self.core = Core.new(host, port)

	task.spawn(function()
		task.wait(0.2)

		if generation == self.connectGeneration and not self:isConnected() and loading then
			self:setPage(Connecting {
				App = self,
			})
		end
	end)

	self.onCanceled:Once(function()
		canceled = true
	end)

	self.core:onPrompt(function(message, changes, tree)
		loading = false

		local signal = Signal.new()
		local result = false

		self:setIcon("Warn")

		-- Prompt user to accept or reject incoming changes
		if changes then
			local options = { "Accept", "Diff", "Cancel" }
			message = describeChanges(changes)

			self:setPage(Prompt {
				App = self,
				Message = message,
				Options = options,
				Signal = signal,
			})

			local choice = signal:Wait()

			while choice == "Diff" do
				self:diff(changes, tree)
				choice = signal:Wait()
			end

			self:diff()

			result = choice == "Accept"
		-- Prompt user whether to continue when server ID differs
		else
			local options = { "Continue", "Cancel" }

			self:setPage(Prompt {
				App = self,
				Message = message,
				Options = options,
				Signal = signal,
			})

			result = signal:Wait() == "Continue"
		end

		if self.core.status == Core.Status.Connected then
			self:setPage(Connected {
				App = self,
				Project = project,
			})

			self:setIcon("Ok")
		end

		return result
	end)

	self.core:onReady(function(projectDetails)
		loading = false
		project:set(projectDetails)

		self.lastSync:set(os.time())

		self.lastSyncKind:set(Config:get("InitialSyncPriority"))

		self:setPage(Connected {
			App = self,
			Project = project,
		})

		self:setIcon("Ok")
	end)

	-- Connect-time divergence routed to the desktop app (Design §7.3 as
	-- amended): a small waiting state on the Connecting page whose Cancel
	-- submits the cancel decision while the connection stays alive. The
	-- resolved event needs no page action — `run` continues into onReady.
	self.core:onDivergence(function(event)
		if event.kind ~= "waiting" then
			return
		end

		loading = false

		self:setIcon("Warn")

		self:setPage(Connecting {
			App = self,
			Message = "Waiting for decision in the WSync app…",
			Cancel = event.cancel,
		})
	end)

	-- Passive disk-review indicator (Design §7.0, code scope): one amber line
	-- on the Connected page; nothing blocks on it and the review itself lives
	-- in the desktop app
	self.core:onDiskReview(function(message)
		self.diskReview:set(message)
	end)

	-- Transport lifecycle (WebSocket reconnect backoff with visible countdown,
	-- Design §9.4) — surfaced on the Connected page in place of the address
	self.core:onTransportStatus(function(status)
		if not self:isConnected() then
			return
		end

		if status.kind == "reconnecting" then
			self.transportStatus:set(`Reconnecting in {status.secondsLeft}s (attempt {status.attempt})…`)
			self:setIcon("Warn")
		elseif status.kind == "connecting" then
			self.transportStatus:set("Reconnecting…")
		elseif status.kind == "disconnected" then
			self.transportStatus:set("Connection lost")
			self:setIcon("Warn")
		elseif status.kind == "connected" then
			self.transportStatus:set(nil)
			self:setIcon("Ok")
		end
	end)

	self.core:onSync(function(kind, data)
		self.lastSync:set(os.time())
		self.lastSyncKind:set(
			(kind == "SyncChanges" or kind == "SyncDetails") and "Server"
				or kind == "SyncbackChanges" and "Client"
				or "None"
		)

		if kind == "SyncDetails" then
			project:set(data)
		end
	end)

	self.core:run():catch(function(err)
		loading = false

		if generation ~= self.connectGeneration then
			return
		end

		if Core.wasExitGraceful(err) or canceled then
			self:disconnect()
			self:home()
			return
		end

		self:setPage(Error {
			App = self,
			Message = err.message or tostring(err),
		})

		self:setIcon("Error")

		self:disconnect()

		-- A non-retryable server shutdown halts the retry loop by design
		-- (Design §9.4)
		if Config:get("AutoReconnect") and not Core.wasFatal(err) then
			task.wait(RECONNECT_INTERVAL)

			if self.lastUpdate + RECONNECT_INTERVAL <= os.clock() then
				self:connect()
			end
		end
	end)
end

function App:disconnect(cancel: boolean?)
	self.lastUpdate = os.clock()
	self.connectGeneration += 1
	self.transportStatus:set(nil)
	self.diskReview:set(nil)

	-- Drop whatever the connection flow was doing (stale scans and waits
	-- abort through the generation bump)
	self:dispatch({ kind = "reset" })

	if self.core then
		if cancel then
			self.onCanceled:Fire()
		end

		self.core:stop()
		self.core = nil
	end
end

function App:isConnected()
	return self.core and self.core.status == Core.Status.Connected
end

return App
