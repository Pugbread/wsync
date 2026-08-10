local WSync = script:FindFirstAncestor("WSync")
local App = WSync.App
local Components = App.Components
local Util = Components.Util

local Fusion = require(WSync.Packages.Fusion)

local Config = require(WSync.Config)
local GlobalUtil = require(WSync.Util)

local Theme = require(App.Theme)
local default = require(Util.default)
local filterHost = require(Util.filterHost)
local filterPort = require(Util.filterPort)
local getTextSize = require(Util.getTextSize)

local Widget = require(Components.Plugin.Widget)
local ScrollingContainer = require(Components.ScrollingContainer)
local OptionSelector = require(Components.OptionSelector)
local Collapsible = require(Components.Collapsible)
local TextButton = require(Components.TextButton)
local Container = require(Components.Container)
local Checkbox = require(Components.Checkbox)
local Dropdown = require(Components.Dropdown)
local Padding = require(Components.Padding)
local Input = require(Components.Input)
local List = require(Components.List)
local Text = require(Components.Text)
local Box = require(Components.Box)

local New = Fusion.New
local Value = Fusion.Value
local Hydrate = Fusion.Hydrate
local Cleanup = Fusion.Cleanup
local Observer = Fusion.Observer
local Computed = Fusion.Computed
local Children = Fusion.Children
local OnChange = Fusion.OnChange
local ForValues = Fusion.ForValues
local ForPairs = Fusion.ForPairs
local Spring = Fusion.Spring
local cleanup = Fusion.cleanup
local peek = Fusion.peek

-- Connectivity only (Design §7.0 — the Studio-first ruling): there is one
-- syncing method with no sync settings, so this widget shrank to Host, Port,
-- AutoDiscover, Https and LogLevel. The retired keys still exist in
-- Config.Setting (their values are fixed by SyncScope.resolveRetired), they
-- just have no rows anymore.
local SETTINGS_DATA: { { Title: string, Settings: { SettingData } } } = {
	{
		Title = "Connection",
		Settings = {
			{
				Setting = "Host",
				Name = "Server Host",
				Description = "Manual override — the first address discovery probes, so a remote daemon works too",
				Filter = filterHost,
				CustomWidth = 0.33,
			},
			{
				Setting = "Port",
				Name = "Server Port",
				Description = "Manual override — the port probed together with Server Host",
				Filter = filterPort,
				CustomWidth = 70,
			},
			{
				Setting = "AutoDiscover",
				Name = "Auto Discover",
				Description = "Also scan localhost 7978–7990 for a matching daemon; disable to probe Host/Port only",
			},
			{
				Setting = "Https",
				Name = "Use HTTPS",
				Description = "Use HTTPS (TLS) instead of plain HTTP protocol when connecting to the server",
			},
		},
	},
	{
		Title = "Diagnostics",
		Settings = {
			{
				Setting = "LogLevel",
				Name = "Log Level",
				Description = "The level of logging you want to see in the output",
				Options = { "Off", "Error", "Warn", "Info", "Debug", "Trace" },
			},
		},
	},
}

local LEVELS = { "Global", "Game", "Place" }

type SettingData = {
	Setting: Config.Setting,
	Name: string,
	Description: string,
	Options: { string }?,
	Filter: ((value: string) -> string)?,
	CustomWidth: number?,
}

type EntryProps = {
	Data: SettingData,
	Level: Fusion.Value<Config.Level>,
	Binding: Fusion.Value<any>,
}

local function Entry(props: EntryProps): Frame
	local valueComponent
	local setting = props.Data.Setting

	local absoluteSize = Value(Vector2.zero)
	local placeholder = Value(Config:get(setting))

	local connections = {
		Config:onChanged(setting, function(value)
			placeholder:set(value)
		end),
		Observer(props.Level):onChange(function()
			placeholder:set(Config:get(setting))
		end),
	}

	if props.Data.Filter then
		local userInput = false

		table.insert(
			connections,
			Observer(props.Binding):onChange(function()
				local value = peek(props.Binding)

				if not userInput and value == Config:getDefault(setting) then
					props.Binding:set("")
				end

				userInput = false
			end)
		)

		if peek(props.Binding) == Config:getDefault(setting) then
			props.Binding:set("")
		end

		local focused = Value(false)

		valueComponent = Box {
			Size = props.Data.CustomWidth
					and math.floor(props.Data.CustomWidth) == props.Data.CustomWidth
					and UDim2.fromOffset(props.Data.CustomWidth, Theme.CompSizeY.Medium)
				or UDim2.new(props.Data.CustomWidth or 0.2, 0, 0, Theme.CompSizeY.Medium),
			BorderColor = Spring(
				Computed(function(use)
					if use(focused) then
						return use(Theme.Colors.Brand)
					else
						return use(Theme.Colors.Border)
					end
				end),
				Theme.SpringFastSpeed,
				Theme.SpringDamping
			),
			[Children] = {
				Input {
					Size = UDim2.fromScale(1, 1),
					Text = props.Binding,
					PlaceholderText = placeholder,
					Started = function()
						focused:set(true)
					end,
					Changed = function(text)
						userInput = true
						props.Binding:set(props.Data.Filter(text))
					end,
					Finished = function(text)
						focused:set(false)
						text = text ~= "" and text or Config:getDefault(setting)

						props.Binding:set(text)
						Config:set(setting, text, peek(props.Level))
					end,
					[Children] = {
						Padding {
							Horizontal = Theme.Padding,
						},
					},
				},
			},
		}
	elseif props.Data.Options then
		valueComponent = Dropdown {
			Options = props.Data.Options,
			Value = props.Binding,
			Placeholder = placeholder,
			Selected = function(value)
				Config:set(setting, value, peek(props.Level))
			end,
		}
	else
		valueComponent = Checkbox {
			Value = props.Binding,
			Placeholder = placeholder,
			Changed = function(value)
				Config:set(setting, value, peek(props.Level))
			end,
		}
	end

	return Container {
		ZIndex = props.Data.Options and 2 or 1,
		Size = UDim2.fromScale(1, 0),
		[Children] = {
			New "Frame" {
				Size = UDim2.new(1, 0, 0, Theme.BorderThickness),
				BackgroundColor3 = Theme.Colors.Border,
				BorderSizePixel = 0,
			},
			Container {
				Size = UDim2.fromScale(1, 1),
				[Children] = {
					Padding {},
					Text {
						Text = props.Data.Name,
						Font = Theme.Fonts.Bold,
					},
					Container {
						Size = UDim2.fromScale(1, 0),
						[Children] = {
							List {
								FillDirection = Enum.FillDirection.Horizontal,
								VerticalAlignment = Enum.VerticalAlignment.Center,
								HorizontalFlex = Enum.UIFlexAlignment.Fill,
							},
							Container {
								[Children] = {
									Text {
										RichText = true,
										TextWrapped = true,
										AutomaticSize = Enum.AutomaticSize.None,
										Text = props.Data.Description,
										TextSize = Theme.TextSize.Small,
										Color = Theme.Colors.TextDimmed,
										Position = UDim2.fromOffset(0, 22),
										Size = Computed(function(use)
											local absoluteSize = use(absoluteSize)

											local size = getTextSize(
												props.Data.Description,
												Theme.TextSize.Small,
												Theme.Fonts.Enums.Regular,
												Vector2.new(absoluteSize.X, math.huge)
											)

											return UDim2.new(1, 0, 0, size.Y)
										end),
										[OnChange "AbsoluteSize"] = function(size)
											absoluteSize:set(size)
										end,
									},
								},
							},
							Hydrate(valueComponent) {
								[Cleanup] = connections,
								[Children] = {
									New "UIFlexItem" {},
								},
							},
						},
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
	local level = Value("Global")
	local bindings = {}

	return Widget {
		Name = "WSync - Settings",
		OverrideEnabled = true,
		MinimumSize = Vector2.new(350, 400),
		Closed = props.Closed,
		[Children] = {
			Padding {
				Right = 4,
			},
			ScrollingContainer {
				ScrollBar = true,
				[Children] = {
					List {},
					Padding {
						Padding = Theme.WidgetPadding,
					},
					OptionSelector {
						Options = LEVELS,
						Selected = function(option)
							level:set(option)

							for setting, binding in bindings do
								binding:set(default(Config:get(setting, option, true), Config:getDefault(setting)))
							end
						end,
					},
					ForPairs(SETTINGS_DATA, function(_, index, data)
						return index,
							Collapsible {
								Title = `{index}. {data.Title}`,
								-- Few enough settings now to show everything
								Expanded = true,
								Elements = ForValues(data.Settings, function(_, data)
									local setting = data.Setting
									local binding =
										Value(default(Config:get(setting, peek(level)), Config:getDefault(setting)))

									bindings[setting] = binding

									return Entry {
										Data = data,
										Level = level,
										Binding = binding,
									}
								end, cleanup),
							}
					end, cleanup),
					Container {
						Size = UDim2.fromScale(1, 0),
						LayoutOrder = GlobalUtil.len(SETTINGS_DATA) + 1,
						ZIndex = 0,
						[Children] = {
							TextButton {
								AnchorPoint = Vector2.new(1, 0),
								Position = UDim2.fromScale(1, 0),
								Text = "Restore Defaults",
								Activated = function()
									for setting, binding in bindings do
										binding:set(Config:getDefault(setting))
									end

									Config:restoreDefaults(peek(level))
								end,
							},
						},
					},
				},
			},
		},
	}
end
