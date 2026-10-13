-- One button in a dialog, and the dialog itself.
local record DialogButton
   text: string
   callback: function()
   is_enter_default: boolean
   --- A name from the reader's own icon set, which renders in the reader's style
   --- rather than as a placeholder.
   icon: string
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
   --- Puts a widget between the input and the buttons, which is where the
   --- reader's own find box puts its "case sensitive" checkbox.
   addWidget: function(InputDialogArgs, any)
end

local record InputDialogModule
   new: function(any, InputDialogArgs): InputDialogArgs
end

return {
   new = function(_self: any, args: InputDialogArgs): InputDialogArgs
      return args
   end,
} as InputDialogModule
