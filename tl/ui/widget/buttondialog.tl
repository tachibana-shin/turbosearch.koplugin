local record ButtonDialogArgs
   title: string
   --- Rows of buttons, the way a popup lays them out.
   buttons: {any}
   width_factor: number
   dismissable: boolean
end

local record ButtonDialogModule
   new: function(any, ButtonDialogArgs): ButtonDialogArgs
end

return {
   new = function(_self: any, args: ButtonDialogArgs): ButtonDialogArgs
      return args
   end,
} as ButtonDialogModule
