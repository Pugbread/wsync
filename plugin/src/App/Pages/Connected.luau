local WSync = script:FindFirstAncestor("WSync")
local App = WSync.App
local Widgets = App.Widgets
local Components = App.Components

local Fusion = require(WSync.Packages.Fusion)

local Types = require(WSync.Types)
local Assets = require(App.Assets)
local Theme = require(App.Theme)

local Project = require(Widgets.Project)
local TextButton = require(Components.TextButton)
local IconButton = require(Components.IconButton)
local Container = require(Components.Container)
local Padding = require(Components.Padding)
local Text = require(Components.Text)
local List = require(Components.List)
local Box = require(Components.Box)

local Value = Fusion.Value
local Observer = Fusion.Observer
local Computed = Fusion.Computed
local Children = Fusion.Children
local Cleanup = Fusion.Cleanup
local peek = Fusion.peek

-- The passive disk-review line (Design §7.0): amber, matching the warn accent
-- rather than the error red — informational, never blocking
local REVIEW_COLOR = Color3.fromRGB(235, 175, 70)

type Props = {
	App: { [string]: any },
	Project: Fusion.Value<Types.Project>,
}

return function(props: Props): { Instance }
	local syncedText = Value("just now")
	local projectWidget = Value(nil)

	local function updateSyncedText()
		return task.spawn(function()
			while true do
				local elapsed = os.time() - peek(props.App.lastSync)
				local interval = 1
				local text

				if elapsed < 1 then
					text = "just now"
				elseif elapsed < 60 then
					text = math.floor(elapsed) .. "s ago"
				elseif elapsed < 3600 then
					interval = 60
					text = elapsed // interval .. "m ago"
				elseif elapsed < 86400 then
					interval = 3600
					text = elapsed // interval .. "h ago"
				else
					interval = 86400
					text = elapsed // interval .. "d ago"
				end

				syncedText:set(text)
				task.wait(interval)
			end
		end)
	end

	local thread = updateSyncedText()
	local disconnect = Observer(props.App.lastSync):onChange(function()
		task.cancel(thread)
		thread = updateSyncedText()
	end)

	return {
		List {},
		Box {
			Size = UDim2.new(1, 0, 0, Theme.CompSizeY.Large * 1.5),
			[Children] = {
				Padding {},
				Text {
					Size = UDim2.fromScale(0.8, 0.6),
					Text = Computed(function(use)
						return use(props.Project).name
					end),
					Font = Theme.Fonts.Bold,
					Scaled = true,
				},
				Text {
					Position = UDim2.fromScale(0, 0.6),
					Size = UDim2.fromScale(0.8, 0.4),
					-- Address line doubles as the transport status readout
					-- (reconnect backoff countdown, Design §9.4)
					Text = Computed(function(use)
						return use(props.App.transportStatus) or `{props.App.host}:{props.App.port}`
					end),
					TextSize = Theme.TextSize.Small,
					Font = Theme.Fonts.Mono,
					Scaled = true,
				},
				Text {
					AnchorPoint = Vector2.new(1, 0.5),
					Position = UDim2.new(1, -2, 0.5, 0),
					Text = Computed(function(use)
						local lastSyncKind = use(props.App.lastSyncKind)
						local action = ""

						if lastSyncKind == "Server" then
							action = "↓"
						elseif lastSyncKind == "Client" then
							action = "↑"
						end

						return action .. "<i> Synced\n" .. use(syncedText) .. "</i>"
					end),
					RichText = true,
					Color = Theme.Colors.TextDimmed,
					TextSize = Theme.TextSize.Small,
					TextXAlignment = Enum.TextXAlignment.Right,
					LineHeight = 1.2,
				},
			},
		},
		-- Passive disk-review indicator (Design §7.0, code scope): one amber
		-- line under the sync ticker while disk changes await review in the
		-- desktop app. Invisible (and skipped by the list layout) when the
		-- review set is empty; nothing here blocks or prompts.
		Text {
			Size = UDim2.fromScale(1, 0),
			AutomaticSize = Enum.AutomaticSize.Y,
			Text = Computed(function(use)
				return use(props.App.diskReview) or ""
			end),
			Visible = Computed(function(use)
				return use(props.App.diskReview) ~= nil
			end),
			Color = REVIEW_COLOR,
			TextSize = Theme.TextSize.Small,
			TextWrapped = true,
			TextXAlignment = Enum.TextXAlignment.Right,
		},
		Container {
			Size = UDim2.fromScale(1, 0),
			[Children] = {
				List {
					FillDirection = Enum.FillDirection.Horizontal,
					HorizontalAlignment = Enum.HorizontalAlignment.Right,
				},
				TextButton {
					Text = "Disconnect",
					LayoutOrder = 2,
					Solid = true,
					Activated = function()
						props.App:disconnect()
						props.App:home()
					end,
				},
				IconButton {
					Icon = Assets.Icons.Settings,
					BackgroundActive = Computed(function(use)
						return use(props.App.settingsWidget) ~= nil
					end),
					Activated = function()
						props.App:settings()
					end,
					LayoutOrder = 1,
				},
				IconButton {
					Icon = Assets.Icons.Info,
					BackgroundActive = Computed(function(use)
						return use(projectWidget) ~= nil
					end),
					Activated = function()
						local widget = peek(projectWidget)

						if widget then
							widget:Destroy()
							widget = nil
						else
							widget = Project {
								App = props.App,
								Project = props.Project,

								Closed = function()
									peek(projectWidget):Destroy()
									projectWidget:set(nil)
								end,
							}
						end

						projectWidget:set(widget)
					end,
					[Cleanup] = function()
						local widget = peek(projectWidget)

						if widget then
							widget:Destroy()
							widget = nil
						end

						disconnect()
						task.cancel(thread)
					end,
				},
			},
		},
	}
end
