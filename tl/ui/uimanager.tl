local record UIManagerModule
   show: function(any, any)
   close: function(any, any)
   --- Runs `action` after `seconds` without any input, which is the only way
   --- anything automated reaches a reader with no input device.
   scheduleIn: function(any, number, function())
   schedule: function(any, number, function())
   --- Runs `action` after the next repaint, which is how a job that has more to
   --- do stays alive without a screen that never draws: the event loop gets to
   --- run between one batch and the next.
   tickAfterNext: function(any, function())
end

return {
   show = function(_self: any, _widget: any) end,
   close = function(_self: any, _widget: any) end,
   scheduleIn = function(_self: any, _seconds: number, _action: function()) end,
   schedule = function(_self: any, _seconds: number, _action: function()) end,
   tickAfterNext = function(_self: any, _action: function()) end,
} as UIManagerModule
