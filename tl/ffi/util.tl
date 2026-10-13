-- ffiUtil: the helpers KOReader plugins reach for through the FFI.
local record FfiUtil
   --- Substitutes %1, %2 ... with its following arguments.
   template: function(string, any...): string
end

return {
   template = function(_text: string): string return "" end,
} as FfiUtil
