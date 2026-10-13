-- The FileManager, which is what opens a book.
local record FileManagerModule
   --- Called as a plain function with the caller's `ui` passed in \u2014 the idiom the
   --- other plugins use \u2014 because for a document it reaches ReaderUI:showReader,
   --- which does not depend on the caller being the FileManager itself.
   openFile: function(any, string)
end

return {
   openFile = function(_ui: any, _file: string) end,
} as FileManagerModule
