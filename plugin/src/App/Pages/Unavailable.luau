local WSync = script:FindFirstAncestor("WSync")
local App = WSync.App
local Components = App.Components

local Fusion = require(WSync.Packages.Fusion)

local Theme = require(App.Theme)

local Padding = require(Components.Padding)
local Text = require(Components.Text)
local Box = require(Components.Box)

local Children = Fusion.Children

type Props = {
	App: { [string]: any },
	Message: string,
}

return function(): Frame
	return Box {
		Size = UDim2.new(1, 0, 0, Theme.CompSizeY.Large * 2),
		[Children] = {
			Padding {},
			Text {
				Text = "Unavailable during playtest! Return to the edit mode to continue using WSync",
				TextWrapped = true,
				Font = Theme.Fonts.Mono,
				TextSize = Theme.TextSize.Medium,
				Color = Theme.Colors.TextDimmed,
			},
		},
	}
end
