-- The Not Connected page (Design §9.2 as amended; user-ruled Ro-Sync model):
-- a status-driven connection card — place name, colored status dot, status
-- line and ONE dynamic action button — instead of Argon's host:port inputs.
-- Portless auto-discovery is the only primary flow; the Settings widget's
-- `Host`/`Port` remain the manual override and participate in discovery as
-- the first scan candidate. All wording, dot colors and button labels come
-- from `Discovery.Policy.describe`, driven by `App.flow` (the connection
-- state machine).

local UserInputService = game:GetService("UserInputService")

local WSync = script:FindFirstAncestor("WSync")
local App = WSync.App
local Components = App.Components

local Fusion = require(WSync.Packages.Fusion)

local Config = require(WSync.Config)
local Place = require(WSync.Place)
local Policy = require(WSync.Discovery.Policy)
local Assets = require(App.Assets)
local Theme = require(App.Theme)

local TextButton = require(Components.TextButton)
local IconButton = require(Components.IconButton)
local Container = require(Components.Container)
local Padding = require(Components.Padding)
local Corner = require(Components.Corner)
local List = require(Components.List)
local Text = require(Components.Text)
local Box = require(Components.Box)

local New = Fusion.New
local Value = Fusion.Value
local Cleanup = Fusion.Cleanup
local Computed = Fusion.Computed
local Children = Fusion.Children
local Spring = Fusion.Spring
local cleanup = Fusion.cleanup
local peek = Fusion.peek

-- The create flow's dot color (Design §9.2: amber for create-ready and the
-- daemon wait); green and red reuse the diff palette
local AMBER = Color3.fromRGB(235, 180, 75)

type Props = {
	App: { [string]: any },
}

return function(props: Props): { Instance }
	-- Poked by Settings changes so the idle/scanning lines stay truthful when
	-- the user edits Host/Port/AutoDiscover while the card is visible
	local configPulse = Value(0)

	local function repulse()
		configPulse:set(peek(configPulse) + 1)
	end

	local connections = {
		Config:onChanged("AutoDiscover", repulse),
		Config:onChanged("Host", repulse),
		Config:onChanged("Port", repulse),
		-- Escape backs out of the confirm step (best effort: Studio routes
		-- keyboard input here only while the main view has focus)
		UserInputService.InputBegan:Connect(function(input)
			if input.KeyCode == Enum.KeyCode.Escape and peek(props.App.flow).name == "create-confirm" then
				props.App:flowBack()
			end
		end),
	}

	local descriptor = Computed(function(use)
		use(configPulse)

		-- `Place.Name` is the resolved published name; raw `game.Name` in
		-- edit mode is Studio's internal place-file name ("Place7")
		return Policy.describe(use(props.App.flow), {
			placeName = Place.Name,
			autoDiscover = Config:get("AutoDiscover") == true,
			host = tostring(Config:get("Host")),
			port = tonumber(Config:get("Port")) or Config:getDefault("Port"),
		})
	end)

	local dotColor = Computed(function(use)
		local dot = use(descriptor).dot

		if dot == "green" then
			return Theme.Colors.Diff.Add
		elseif dot == "amber" then
			return AMBER
		elseif dot == "red" then
			return Theme.Colors.Diff.Remove
		else
			return use(Theme.Colors.TextDimmed)
		end
	end)

	local statusText = Computed(function(use)
		return use(descriptor).status
	end)

	-- The single dynamic action button: label and emphasis follow the state;
	-- the instance is only rebuilt when the button appears or disappears
	-- (nothing to click while the broker request runs)
	local actionLabel = Computed(function(use)
		return use(descriptor).action or ""
	end)

	local actionSolid = Computed(function(use)
		return use(descriptor).action ~= "Cancel"
	end)

	local hasAction = Computed(function(use)
		return use(descriptor).action ~= nil
	end)

	local actionButton = Computed(function(use)
		if not use(hasAction) then
			return nil
		end

		return TextButton {
			Solid = actionSolid,
			LayoutOrder = 3,
			Text = actionLabel,
			Activated = function()
				props.App:flowAction()
			end,
		}
	end, cleanup)

	-- The confirm step's secondary Back button (step 2 of the two-step
	-- Create Project confirm; Escape does the same)
	local showBack = Computed(function(use)
		return use(descriptor).back
	end)

	local backButton = Computed(function(use)
		if not use(showBack) then
			return nil
		end

		return TextButton {
			LayoutOrder = 2,
			Text = "Back",
			Activated = function()
				props.App:flowBack()
			end,
		}
	end, cleanup)

	return {
		List {},
		-- The connection card
		Box {
			LayoutOrder = 1,
			Size = UDim2.fromScale(1, 0),
			[Cleanup] = connections,
			[Children] = {
				List {
					Spacing = 6,
				},
				Padding {
					Padding = 10,
				},
				Text {
					LayoutOrder = 1,
					Size = UDim2.new(1, 0, 0, 22),
					AutomaticSize = Enum.AutomaticSize.None,
					Text = if Place.Name ~= "" then Place.Name else "Untitled place",
					Font = Theme.Fonts.Bold,
					TextSize = Theme.TextSize.Medium,
					TextTruncate = Enum.TextTruncate.AtEnd,
				},
				Container {
					LayoutOrder = 2,
					Size = UDim2.fromScale(1, 0),
					[Children] = {
						New "Frame" {
							Size = UDim2.fromOffset(8, 8),
							Position = UDim2.fromOffset(0, 6),
							BackgroundColor3 = Spring(dotColor, Theme.SpringFastSpeed, Theme.SpringDamping),
							BorderSizePixel = 0,
							[Children] = {
								Corner {
									Radius = 4,
								},
							},
						},
						Text {
							Position = UDim2.fromOffset(16, 0),
							Size = UDim2.new(1, -16, 0, 0),
							AutomaticSize = Enum.AutomaticSize.Y,
							Text = statusText,
							TextWrapped = true,
							TextSize = Theme.TextSize.Small,
							Color = Theme.Colors.TextDimmed,
						},
					},
				},
			},
		},
		Container {
			LayoutOrder = 2,
			Size = UDim2.fromScale(1, 0),
			[Children] = {
				List {
					FillDirection = Enum.FillDirection.Horizontal,
					HorizontalAlignment = Enum.HorizontalAlignment.Right,
				},
				IconButton {
					Icon = Assets.Icons.Help,
					BackgroundActive = Computed(function(use)
						return use(props.App.helpWidget) ~= nil
					end),
					Activated = function()
						props.App:help()
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
				backButton,
				actionButton,
			},
		},
	}
end
