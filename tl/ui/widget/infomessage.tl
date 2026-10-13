-- A transient message over the top of the screen.
local record InfoMessageArgs
   text: string
end

local record InfoMessageModule
   new: function(any, InfoMessageArgs): InfoMessageArgs
end

return {
   new = function(_self: any, args: InfoMessageArgs): InfoMessageArgs
      return args
   end,
} as InfoMessageModule
