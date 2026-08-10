local plugin = script:FindFirstAncestorWhichIsA("Plugin")

local WSync = script:FindFirstAncestor("WSync")
local App = WSync.App
local Components = App.Components
local Util = Components.Util

local Fusion = require(WSync.Packages.Fusion)

local stripProps = require(Util.stripProps)

local Hydrate = Fusion.Hydrate

local COMPONENT_ONLY_PROPS = {
	"Name",
}

type Props = {
	Name: string,
	[any]: any,
}

return function(props: Props): PluginToolbar
	return Hydrate(plugin:CreateToolbar(props.Name))(stripProps(props, COMPONENT_ONLY_PROPS))
end
