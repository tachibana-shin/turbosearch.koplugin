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
local CheckButton = require("ui/widget/checkbutton")
local InputDialog = require("ui/widget/inputdialog")
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

--- The settings dialog, and the calls made through it.
local record SettingsDialog
   getInputText: function(SettingsDialog): string
   addWidget: function(SettingsDialog, any)
   onShowKeyboard: function(SettingsDialog)
end

--- One option checkbox, and the state it holds.
local record OptionBox
   text: string
   checked: boolean
   parent: any
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

-- Everything the plugin can be told to do differently.
--
-- The options that used to be a mode picker are now checkboxes on the search popup,
--- where a reader already is, so what is left here is the configuration: which index
--- to read, and what the checkboxes start out as next time.
--
--- A text field, two checkboxes, and Save, in one popup, because the settings are a
--- handful of values and not a tree.
function Settings:configure()
   local settings: SettingsFile = (self.main as SettingsHost):loadSettings()

   --- The two options, declared before the dialog that closes over them.
   local metadata_box: OptionBox
   local pattern_box: OptionBox

   local dialog: SettingsDialog
   dialog = InputDialog:new{
      title = _("Book search settings"),
      input = settings:readSetting("index_path", "") as string,
      description = _("The index to read, built by the `indexer` tool in this repository."),
      buttons = {
         {
            {
               text = _("Save"),
               callback = function()
                  settings:saveSetting("index_path", dialog:getInputText())
                  settings:saveSetting("opt_metadata", metadata_box.checked)
                  settings:saveSetting("opt_pattern", pattern_box.checked)
                  settings:flush()
                  UIManager:close(dialog)
                  self:show()
               end,
            },
         },
      },
   } as SettingsDialog

   --- Each checkbox is an option the popup carries, so what it starts as next time
   --- is part of the configuration rather than a thing to remember.
   metadata_box = CheckButton:new{
      text = _("Metadata only"),
      checked = settings:readSetting("opt_metadata", false) as boolean,
      parent = dialog,
   } as OptionBox
   dialog:addWidget(metadata_box)

   pattern_box = CheckButton:new{
      text = _("Regular expression"),
      checked = settings:readSetting("opt_pattern", false) as boolean,
      parent = dialog,
   } as OptionBox
   dialog:addWidget(pattern_box)

   UIManager:show(dialog)
   dialog:onShowKeyboard()
end

return Settings
