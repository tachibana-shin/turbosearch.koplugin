-- The FileManager, which is what opens a book.
local record FileManagerModule
   --- Called as a plain function with the caller's `ui` passed in \u2014 the idiom the
   --- other plugins use \u2014 because for a document it reaches ReaderUI:showReader,
   --- which does not depend on the caller being the FileManager itself.
   openFile: function(any, string)
   --- The browser, when one is open. `nil` inside a book, which is what makes
   --- "where is the reader" two questions rather than one: an open book says
   --- where it is, and a browser says where it is looking.
   instance: FileManagerInstance
end

local record FileManagerInstance
   file_chooser: FileChooser
end

local record FileChooser
   --- The folder on screen, without a trailing slash.
   path: string
end

return {
   openFile = function(_ui: any, _file: string) end,
   instance = nil as FileManagerInstance,
} as FileManagerModule
