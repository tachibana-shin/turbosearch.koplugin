-- Measuring and shortening text, in pixels rather than in characters.
local record RenderTextModule
   --- The part of `text` that is at most `width` pixels wide, unchanged.
   ---
   --- Compared against the whole value by a caller that wants to know whether
   --- anything would be cut before deciding to say so.
   getSubTextByWidth: function(RenderTextModule, string, any, integer, boolean, boolean): string
   --- `text` shortened to at most `width` pixels, with an ellipsis on the end.
   ---
   --- The ellipsis is appended whether or not anything was cut, so this is not
   --- the thing to reach for first.
   ---
   --- The four arguments after the text are the face to measure in, that width,
   --- whether to kern while measuring, and whether to bold — the same shape the
   --- reader passes, because a caller that measures in one face and draws in
   --- another has only guessed.
   truncateTextByWidth: function(RenderTextModule, string, any, integer, boolean, boolean): string
end

return {
   getSubTextByWidth = function(_self: RenderTextModule, text: string, _face: any,
                                _width: integer, _kerning: boolean, _bold: boolean): string
      return text
   end,
   truncateTextByWidth = function(_self: RenderTextModule, text: string, _face: any,
                                  _width: integer, _kerning: boolean, _bold: boolean): string
      return text
   end,
} as RenderTextModule
