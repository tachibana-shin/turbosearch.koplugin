-- A checkbox with a label beside it.
---
-- Inserted into a dialog with `addWidget`, which is how the reader itself composes
--- a find-and-replace box: the widget goes between the input and the buttons.
local record CheckButton
   text: string
   checked: boolean
   callback: function()
   parent: any
end

local record CheckButtonModule
   new: function(any, CheckButton): CheckButton
end

return {
   new = function(_self: any, args: CheckButton): CheckButton return args end,
} as CheckButtonModule
