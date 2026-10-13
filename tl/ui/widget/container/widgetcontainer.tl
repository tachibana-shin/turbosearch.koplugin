-- The class base. `extend` is dynamic by nature — a plugin hands it a table of
-- methods it is about to define — so the checker cannot see what comes back, and
-- the records a plugin declares for its own instances are what get checked.
local record ClassBase
   extend: function(any, any): any
   new: function(any, any): any
end

return {
   extend = function(_self: any, subclass: any): any return subclass end,
   new = function(_self: any, args: any): any return args end,
} as ClassBase
