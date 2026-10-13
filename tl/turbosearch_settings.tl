-- Settings, and everything the engine can be asked about itself.
--
-- Kept apart from the plugin's own file because it talks to an index rather than
-- to a book, and because it is the half a reader arrives at wanting an answer
-- rather than a search.
--
-- A popup, not a menu: every other plugin on this reader shows its settings as
-- one, and with reason — the settings are something to glance at, act on and be
-- rid of, and a full-screen list is the wrong shape for five lines of
-- information. The report sits in the title, where a popup puts what it is about,
--- and each button carries an icon from the reader's own set, so a reader scanning
--- for the one they want finds it by shape before reading any text.

local ButtonDialog = require("ui/widget/buttondialog")
local ConfirmBox = require("ui/widget/confirmbox")
local InfoMessage = require("ui/widget/infomessage")
local UIManager = require("ui/uimanager")
local WidgetContainer = require("ui/widget/container/widgetcontainer")
local ffiUtil = require("ffi/util")
local _ = require("gettext") as function(string): string
local T = ffiUtil.template

-- The part of the plugin this screen reaches for: the engine it loads through, and
--- the settings file it keeps.
--- What the index says about itself, as the binding reports it.
local record IndexInfo
   bytes: number
   terms: number
   mode: string
end

local record EngineBinding
   open_index: function(string): any, string
   index_info: function(any): IndexInfo
   close: function(any)
end

local record SettingsFile
   data: {any: any}
   readSetting: function(SettingsFile, string, any): any
   saveSetting: function(SettingsFile, string, any)
   flush: function(SettingsFile)
end

local record SettingsHost
   engine: function(any): EngineBinding
   loadSettings: function(any): SettingsFile
end

-- The warning travels with the report, so the screen can decide whether it
-- deserves a row of its own.
local record IndexReport
   index: string
   bytes: number
   terms: number
   content: string
   mode: string
   warn: string
end

-- Icons, from KOReader's own set.
---
-- Named rather than drawn because an icon that ships with the reader renders in
-- whatever style the reader is using, and one that does not shows as a placeholder.
local ICON_SEARCH: string = "appbar.search"
local ICON_FOLDER: string = "appbar.filebrowser"
local ICON_CLOSE: string = "close"

-- Sizes a byte count in a unit a reader can act on.
local function format_bytes(bytes: number): string
   if bytes < 1024 then
      return T(_("%1 B"), bytes)
   end
   if bytes < 1024 * 1024 then
      return T(_("%1 kB"), math.floor(bytes / 1024))
   end
   return string.format("%.1f MB", bytes / (1024 * 1024))
end

-- Describes the search mode the way the query box's hint does.
local function mode_short(mode: string): string
   if mode == "Metadata" then
      return _("Title, author, series")
   end
   if mode == "Pattern" then
      return _("Pattern")
   end
   return _("Book text")
end

-- Describes how much content the index holds.
local function content_short(mode: string): string
   if mode == "FileList" then
      return _("File list only")
   end
   if mode == "TermsOnly" then
      return _("Terms, no positions")
   end
   return _("Full text")
end

-- The search modes, as a reader chooses between them.
local record ModeOption
   mode: string
   text: string
   help: string
end

local record Settings
   --- Inherited from the reader's widget base through the metatable set below, and
   --- declared here because a record's type cannot see its metatable.
   new: function(any, any): Settings
   main: SettingsHost

   report: function(Settings): IndexReport, string
   show: function(Settings)
   choose_mode: function(Settings)
end

setmetatable(Settings, { __index = WidgetContainer })

-- Opens the index briefly and reports what it holds.
---
-- The handle is closed before anything is shown, for the same reason the search
-- path closes it: a settings popup holding a database open while the reader reads
-- it is a database open while the reader is doing something else.
function Settings:report(): IndexReport, string
   local engine = (self.main as SettingsHost):engine()
   if not engine then
      return nil as IndexReport, _("The engine could not be loaded.")
   end

   local settings: SettingsFile = (self.main as SettingsHost):loadSettings()
   local index_path: string = settings:readSetting("index_path", "") as string
   --- The binding reports a failed open as a message rather than a raise, so a
   --- missing index is a row in the reader rather than a dead one.
   local handle: any
   local open_err: string
   handle, open_err = engine.open_index(index_path)
   if not handle then
      return nil as IndexReport, T(_("Could not open the index at %1.\n\n%2"),
         index_path, open_err)
   end

   local info = engine.index_info(handle)
   engine.close(handle)

   -- A content mode that cannot answer the mode chosen is worth saying rather
   -- than leaving a reader to find out by typing a query.
   local warn: string = ""
   if info.mode == "FileList" then
      warn = _("This index is only a list of files, so it has no text to search. \
Build one with CONTENT_MODE=terms or full.")
   elseif info.mode == "TermsOnly" then
      warn = _("This index has no positions, so phrases in quotes will not match. \
Switch to a loose query, or rebuild the index.")
   end

   return {
      index = index_path,
      bytes = math.floor(info.bytes),
      terms = info.terms,
      content = content_short(info.mode),
      mode = mode_short(settings:readSetting("search_mode", "Text") as string),
      -- The warning is carried on the report rather than shown here, because the
      -- screen decides whether a warning deserves its own row.
      warn = warn,
   } as IndexReport, ""
end

-- Opens the settings popup.
function Settings:show()
   local report, err = self:report()
   if not report then
      UIManager:show(ConfirmBox:new{
         title = _("Book search"),
         text = err,
         icon = "texture-box",
         ok_text = _("Close"),
      } as any)
      return
   end

   -- The report goes in the title, where a popup puts what it is about. Each line
   -- is a fact to read and none is something to tap, so none of them is a button.
   local title: string = _("Book search") .. "\n\n"
      .. _("Index") .. "  " .. report.index .. "\n"
      .. _("Size") .. "  " .. format_bytes(report.bytes) .. "\n"
      .. _("Terms") .. "  " .. string.format("%d", math.floor(report.terms)) .. "\n"
      .. _("Content") .. "  " .. report.content .. "\n"
      .. _("Query means") .. "  " .. report.mode

   local dialog: any
   dialog = ButtonDialog:new{
      title = title,
      -- One row of the two things a reader came to change, and a close under
      -- them, so the popup is two taps tall and does not scroll.
      buttons = {
         {
            {
               text = _("Query means"),
               icon = ICON_SEARCH,
               callback = function()
                  UIManager:close(dialog)
                  self:choose_mode()
               end,
            },
            {
               text = _("Index"),
               icon = ICON_FOLDER,
               callback = function()
                  UIManager:close(dialog)
                  UIManager:show(InfoMessage:new{ text = report.index } as any)
               end,
            },
         },
         {
            {
               text = _("Close"),
               icon = ICON_CLOSE,
               callback = function()
                  UIManager:close(dialog)
               end,
            },
         },
      },
   } as any
   UIManager:show(dialog)
end

-- Chooses what the query means.
---
-- A popup with a row per option rather than a dialog with a keyboard: three
-- choices and no typing, so the control is three buttons and the current one is
-- marked by weight. Tapping one applies it and comes back here, which keeps the
-- reader looking at the same popup they already understood.
function Settings:choose_mode()
   local settings: SettingsFile = (self.main as SettingsHost):loadSettings()
   local current: string = settings:readSetting("search_mode", "Text") as string

   local dialog: any

   --- The rows are built before the popup is, because the current option is marked
   --- by weight and that depends on a setting read at run time.
   local rows: {any} = {}
   local options: {ModeOption} = {
      { mode = "Text", text = _("Book text"),
        help = _("phrases in quotes match exactly") },
      { mode = "Metadata", text = _("Title, author, series"),
        help = _("skip the book contents") },
      { mode = "Pattern", text = _("Regular expression"),
        help = _("over the terms in the index") },
   }

   for _, option in ipairs(options) do
      rows[#rows + 1] = {
         {
            text = option.text .. "  " .. option.help,
            text_font_bold = option.mode == current,
            callback = function()
               settings:saveSetting("search_mode", option.mode)
               settings:flush()
               self:show()
            end,
         },
      }
   end
   rows[#rows + 1] = {
      {
         text = _("Close"),
         callback = function()
            UIManager:close(dialog)
         end,
      },
   }

   dialog = ButtonDialog:new{
      title = _("What should the query mean?") .. "\n\n"
         .. _("Now") .. ": " .. mode_short(current),
      buttons = rows,
   } as any
   UIManager:show(dialog)
end

return Settings
