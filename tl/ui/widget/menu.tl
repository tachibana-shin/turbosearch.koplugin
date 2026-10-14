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
   --- The face size the right-hand column is drawn at. Set rather than read, so
   --- that a caller measuring that column in pixels measures it in the face it
   --- will actually be painted in.
   items_mandatory_font_size: integer
   --- The bar under the list. `{any}` because the bar's rows are the caller's
   --- shape to declare, the same reason `item_table` is.
   buttons: {any}
end

local record MenuModule
   new: function(any, MenuArgs): MenuArgs
end

return {
   new = function(_self: any, args: MenuArgs): MenuArgs return args end,
} as MenuModule
