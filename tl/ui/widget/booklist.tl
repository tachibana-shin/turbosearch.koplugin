--- KOReader's own book list: a menu whose rows carry a path.
---
--- It is what the file browser's book tabs and the bookmarks browser use, and
--- what it adds over a plain menu is everything a list of books needs — the
--- cover, the read status, the sidecar settings, all keyed off `item.path`.
---
--- CoverBrowser, where it is installed, hooks the book list to replace the item
--- widget with one that draws a cover, and keys that cover off the same path.
--- So a results list built on a menu and a book list built on a book list look
--- the same to a reader whose reader has CoverBrowser, and only one of them has
--- to know anything about covers.
---
--- `new` is `Menu:new` — the class extends the menu and overrides the item widget
--- and the book-info cache — so what it takes is what a menu takes.
local record BookListArgs
   title: string
   subtitle: string
   item_table: {any}
   covers_full_width: boolean
   --- The keyboard shortcuts down the left edge. Off, because these letters are
   --- not the reader's file browser and a list of fourteen books labelled
   --- Q W E R T U looks like a quiz rather than a shelf. `Device:hasKeyboard()`
   --- is the menu's own default, and on the emulated readers here it is true.
   is_enable_shortcut: boolean
   onMenuSelect: function(any, any)
   onMenuHold: function(any, any)
   close_callback: function()
   --- The bar under the list. `{any}` because the bar's rows are the caller's
   --- shape to declare, the same reason `item_table` is.
   buttons: {any}
end

--- What is known about one book, from its path.
---
--- A record with every field optional rather than a table of strings, because
--- "not known yet" and "known to be nothing" are different answers and only one
--- of them should re-open a sidecar file.
local record BookInfo
   been_opened: boolean
   status: string
   rating: integer
   percent_finished: number
   pages: integer
   has_annotations: boolean
end

local record BookListModule
   new: function(any, BookListArgs): BookListArgs
   --- Reads a book's status and sidecar, caching it on the way.
   getBookInfo: function(string): BookInfo
   --- Whether the book has ever been opened, which is what a "new" marker is.
   hasBookBeenOpened: function(string): boolean
end

return {
   new = function(_self: any, args: BookListArgs): BookListArgs return args end,
   getBookInfo = function(_file: string): BookInfo return {} as BookInfo end,
   hasBookBeenOpened = function(_file: string): boolean return false end,
} as BookListModule
