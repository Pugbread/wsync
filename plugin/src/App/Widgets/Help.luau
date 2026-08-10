local WSync = script:FindFirstAncestor("WSync")
local App = WSync.App
local Components = App.Components

local Fusion = require(WSync.Packages.Fusion)

local Theme = require(App.Theme)

local Widget = require(Components.Plugin.Widget)
local ScrollingContainer = require(Components.ScrollingContainer)
local Container = require(Components.Container)
local Padding = require(Components.Padding)
local Input = require(Components.Input)
local List = require(Components.List)
local Text = require(Components.Text)
local Box = require(Components.Box)

local Children = Fusion.Children

local function Entry(body: { any })
	return Box {
		Size = UDim2.fromScale(1, 0),
		[Children] = {
			Padding {},
			List {
				Spacing = 6,
			},
			body,
		},
	}
end

local function Header(text: string)
	return Text {
		Font = Theme.Fonts.Bold,
		Text = text,
	}
end

local function Paragraph(text: string)
	return Text {
		Text = text,
		RichText = true,
		TextWrapped = true,
		TextSize = Theme.TextSize.Small,
		Color = Theme.Colors.TextDimmed,
	}
end

local function Link(text: string, url: string)
	return Container {
		Size = UDim2.fromScale(1, 0),
		[Children] = {
			List {
				HorizontalAlignment = Enum.HorizontalAlignment.Center,
				Spacing = 8,
			},
			Text {
				Font = Theme.Fonts.Bold,
				Text = text,
			},
			Box {
				AutomaticSize = Enum.AutomaticSize.XY,
				[Children] = {
					Padding {},
					Input {
						AutomaticSize = Enum.AutomaticSize.XY,
						Text = url,
						TextEditable = false,
						TextSize = Theme.TextSize.Large,
						Color = Theme.Colors.Brand,
					},
				},
			},
		},
	}
end

type Props = {
	Closed: (() -> ())?,
}

return function(props: Props): DockWidgetPluginGui
	return Widget {
		Name = "WSync - Help",
		OverrideEnabled = true,
		MinimumSize = Vector2.new(400, 350),
		Closed = props.Closed,
		[Children] = {
			Padding {
				Right = 4,
			},
			ScrollingContainer {
				ScrollBar = true,
				[Children] = {
					List {
						Spacing = 22,
					},
					Padding {
						Padding = Theme.WidgetPadding,
					},
					Link("Learn more with the built-in docs:", "wsync doc"),
					Entry {
						Header "1. Setting up the project",
						Paragraph "First you need to create a new WSync project. Run the <b>wsync init</b> command in your terminal, or use the WSync desktop app to add a project.",
					},
					Entry {
						Header "2. Starting the daemon",
						Paragraph "Serve the project with the <b>wsync serve</b> command, or toggle <b>Serve</b> on the project card in the WSync desktop app.",
					},
					Entry {
						Header "3. Connecting",
						Paragraph "With <b>Auto Discover</b> enabled, WSync scans localhost for a daemon whose project matches this place and connects automatically.",
						Paragraph "You can always connect manually: make sure host and port match the daemon, then press <b>Connect</b>.",
					},
					Entry {
						Header "4. Syncing",
						Paragraph "Save your files to see changes in Roblox Studio. <b>Two-Way Sync</b> is on by default, so changes made in Studio are written back to the file system as well.",
					},
				},
			},
		},
	}
end
