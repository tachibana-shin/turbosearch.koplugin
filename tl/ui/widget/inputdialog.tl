-- One button in a dialog, and the dialog itself.
local record DialogButton
   text: string
   callback: function()
   is_enter_default: boolean
end

local record InputDialogArgs
   title: string
   input: string
   input_hint: string
   description: string
   buttons: {{DialogButton}}
   --- Present so the object handed back can be called through: the real one has
   --- more, and a caller reaching past this gets an error naming the field.
   getInputText: function(InputDialogArgs): string
   onShowKeyboard: function(InputDialogArgs)
end

local record InputDialogModule
   new: function(any, InputDialogArgs): InputDialogArgs
end

return {
   new = function(_self: any, args: InputDialogArgs): InputDialogArgs
      return args
   end,
} as InputDialogModule
