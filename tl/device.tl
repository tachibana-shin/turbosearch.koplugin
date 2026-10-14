-- The device, and the screen a widget is laid out on.
--
-- Only what a layout decision needs: how big the surface is. A column that has
-- to sit beside another one cannot be sized in characters, because a character
-- is a different width on every device and only the pixels decide.
local record ScreenRecord
   getWidth: function(ScreenRecord): integer
   getHeight: function(ScreenRecord): integer
   --- Scales a design pixel to this device's, which is how KOReader's own
   --- constants are written so that one number means one thing everywhere.
   scaleBySize: function(ScreenRecord, integer): integer
end

local record DeviceRecord
   screen: ScreenRecord
end

return {
   screen = {
      getWidth = function(_self: ScreenRecord): integer return 1072 end,
      getHeight = function(_self: ScreenRecord): integer return 1448 end,
      scaleBySize = function(_self: ScreenRecord, value: integer): integer return value end,
   },
} as DeviceRecord
