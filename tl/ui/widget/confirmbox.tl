local record ConfirmBoxArgs
   text: string
   title: string
   --- A name from the reader's own icon set, which renders in whatever style the
   --- reader is using rather than as a placeholder.
   icon: string
   ok_text: string
   cancel_text: string
   --- Both are run later, not during `new`: a confirm box is a question the
   --- reader answers, and work that has to start after the answer cannot be
   --- started before it is asked.
   ok_callback: function()
   cancel_callback: function()
end

local record ConfirmBoxModule
   new: function(any, ConfirmBoxArgs): ConfirmBoxArgs
end

return {
   new = function(_self: any, args: ConfirmBoxArgs): ConfirmBoxArgs
      return args
   end,
} as ConfirmBoxModule
