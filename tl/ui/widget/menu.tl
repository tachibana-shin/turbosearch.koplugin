-- A menu's constructor takes whatever rows the caller built: their shape is the
--- caller's record to declare, and the menu only has to know they are rows.
local record MenuArgs
   title: string
   subtitle: string
   item_table: {any}
   covers_full_width: boolean
   onMenuSelect: function(any, any)
   onMenuHold: function(any, any)
   close_callback: function()
end

local record MenuModule
   new: function(any, MenuArgs): MenuArgs
end

return {
   new = function(_self: any, args: MenuArgs): MenuArgs return args end,
} as MenuModule
