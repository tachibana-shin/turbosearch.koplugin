-- The plugin's metadata, which is what the reader shows for it.
---
-- Teal for the same reason everything else here is: this file is generated rather
--- than edited, and a plugin directory that contains only build output is one that
--- cannot drift from the sources it came from.
---
-- The plugin is a search tool, so its entry in the menu is where the reader's own
--- browser would put a search.

local _ = require("gettext") as function(string): string

local record Meta
   fullname: string
   description: string
end

return {
   fullname = _("Book search"),
   description = _("Full-text search across the whole book library."),
} as Meta
