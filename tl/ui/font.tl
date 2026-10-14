-- Fonts: a face is a name and a size, and what comes back is opaque — the
-- plugin only ever hands it to something that measures.
local record FontModule
   --- Resolves a face. The reader caches these, so asking per row costs nothing.
   getFace: function(FontModule, string, integer): any
end

return {
   getFace = function(_self: FontModule, _name: string, _size: integer): any
      return {}
   end,
} as FontModule
