-- One registered action, with the fields the loader reads back.
local record Action
   category: string
   event: string
   title: string
   general: boolean
   arg: any
end

local record DispatcherModule
   registerAction: function(any, string, Action)
end

return {
   registerAction = function(_self: any, _name: string, _action: Action) end,
} as DispatcherModule
