local ChangeHistoryService = game:GetService("ChangeHistoryService")

return function(action: string, name: string?)
	name = name and ` "{name}"` or ""
	ChangeHistoryService:SetWaypoint(`WSync: {action}{name} [{DateTime.now():FormatLocalTime("LTS", "en-us")}]`)
end
