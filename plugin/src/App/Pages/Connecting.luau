local WSync = script:FindFirstAncestor("WSync")
local App = WSync.App
local Components = App.Components

local Fusion = require(WSync.Packages.Fusion)

local Theme = require(App.Theme)

local TextButton = require(Components.TextButton)
local Spinner = require(Components.Spinner)
local Text = require(Components.Text)
local List = require(Components.List)
local Box = require(Components.Box)

local Children = Fusion.Children

type Props = {
	App: { [string]: any },
	-- Optional variant (Design §7.3 as amended): the connect-time divergence
	-- wait reuses this page with a status line and a Cancel that submits the
	-- daemon decision instead of dropping the connection.
	Message: string?,
	Cancel: (() -> ())?,
}

return function(props: Props): { Instance }
	local children = {
		List {
			HorizontalAlignment = Enum.HorizontalAlignment.Right,
		},
		Box {
			Size = UDim2.new(1, 0, 0, Theme.CompSizeY.Large * 1.5),
			[Children] = {
				Spinner {
					AnchorPoint = Vector2.new(0.5, 0.5),
					Position = UDim2.fromScale(0.5, 0.5),
				},
			},
		},
	}

	if props.Message then
		table.insert(
			children,
			Text {
				Text = props.Message,
				TextWrapped = true,
				TextSize = Theme.TextSize.Small,
				Color = Theme.Colors.TextDimmed,
			}
		)
	end

	table.insert(
		children,
		TextButton {
			Text = "Cancel",
			Activated = function()
				if props.Cancel then
					-- Divergence wait: cancel the pending decision while the
					-- connection stays alive — resolution flows back through
					-- the decision channel
					props.Cancel()
				else
					props.App:disconnect(true)
					props.App:home()
				end
			end,
		}
	)

	return children
end
