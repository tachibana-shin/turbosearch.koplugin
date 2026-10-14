-- turbosearch: full-text search across the book library.
--
-- The engine is a C shared library loaded through LuaJIT's FFI, described in
-- tl/turbosearch.tl and compiled beside this file. See `note.md` section 5 for
-- why: a KOReader plugin is Lua, LuaJIT calls C directly, and the C ABI is where
-- a wrong type costs a use-after-free rather than a Lua error.
--
-- This file is the plugin's own logic. It is Teal for the same reason, which is
-- the reason the stubs in tl/ exist: every widget a plugin touches is described
-- once, in one place, so a field the reader's version does not have — `dimmed`,
-- `UIManager:setInputDialog`, `ui/widget/alertdialog` — is a build error rather
-- than a screen that silently does not do what it looks like it does.

local DataStorage = require("datastorage")
local Dispatcher = require("dispatcher")
local FileManager = require("apps/filemanager/filemanager")
local InfoMessage = require("ui/widget/infomessage")
local CheckButton = require("ui/widget/checkbutton")
local InputDialog = require("ui/widget/inputdialog")
local LuaSettings = require("luasettings")
local Menu = require("ui/widget/menu")
local UIManager = require("ui/uimanager")
local WidgetContainer = require("ui/widget/container/widgetcontainer")
local ffiUtil = require("ffi/util")
local _ = require("gettext") as function(string): string
local T = ffiUtil.template

--- The part of the binding the plugin reaches for.
---
--- Declared here rather than imported, because a module cannot lend its types to
--- the module that requires it. The cast in `engine` joins the two, so a method
--- the binding drops or renames stops this file compiling instead of quietly
--- stopping the search from working.
local record EngineBinding
   is_loaded: function(): boolean
   load_lib: function(string): any
   open_index: function(string): any, string
   close: function(any)
   search_as: function(any, any, string, any): {Hit}, string
   book_title: function(any, integer): string
   book_path: function(any, integer): string
   index_info: function(any): any
end

--- The Teal binding, which is what the engine is reached through.
local binding = require("turbosearch")

--- The settings screen, and the engine loader it reaches the binding through.
local Settings = require("turbosearch_settings")

--- LuaJIT's own table, which is what says which architecture is running.
global record jit
   arch: string
end

--- The directory this plugin was loaded from.
---
--- Resolved from the running source rather than hard-coded: the plugin is
--- normally a symlink into ~/.config/koreader, and debug.getinfo reports the
--- path the file was actually loaded through, which is where the engine and the
--- compiled binding also sit.
---
--- The leading `@` is part of the debug representation rather than the path, and
--- dropping it keeps the resolved directory from becoming a name such as
--- "@plugin/turbosearch.koplugin/" that no require can match.
local function plugin_dir(): string
   local source: string = debug.getinfo(1, "S").source
   if source:sub(1, 1) == "@" then
      source = source:sub(2)
   end
   return (source:match("(.*/)") or "./")
end

--- Icons, as text rather than images.
---
--- KOReader loads nerdfonts as a *fallback* font, so a glyph is a character at a
--- Private Use Area codepoint that the font supplies an outline for. This is how the
--- reader composes its own icon rows — readertypography.lua inserts the raw
--- character into a string — and it costs nothing to draw: no image is decoded and
--- no icon is looked up, while the glyph takes the size, colour and face of the
--- text it sits in.
---
--- The `icon = "appbar.settings"` form used before is a different mechanism
--- entirely: it names an SVG under resources/icons, which the reader then decodes
--- and scales. It works, but it is a second way to say the same thing, and a name
--- that does not exist renders as a placeholder rather than an error.
---
--- Codepoints, from the font the reader actually loads:
---   from fontTools import TTFont
---   TTFont('/usr/lib/koreader/fonts/nerdfonts/symbols.ttf').getBestCmap()
local ICON_SEARCH: string = ""
local ICON_SETTINGS: string = ""
local ICON_CLOSE: string = ""

--- The engine library, always under one name.
---
--- A release is built per platform and each contains a `libturbosearch.so` for that
--- platform, so there is one file in the plugin directory and nothing to choose
--- between. LuaJIT loads it, and if the wrong one is there it fails at `ffi.load`
-- with a message about the ELF class, which is the honest outcome — a reader that
--- got the wrong archive finds out at once.
---
--- LuaJIT was *not* asked to tell the two ARM ABIs apart, which an earlier draft had
--- it doing: it reports `arm` for soft- and hard-float alike, so the choice would
--- have been a guess at the reader's firmware. Which binary to ship is the job of
--- whoever picks the platform, not of the code that loads it.
local ENGINE_LIBRARY: string = "libturbosearch.so"

-- Maximum rows this menu will build. Beyond this the page takes longer to render
-- than a reader is willing to sit still for, and someone looking for a book is
-- refining the query rather than paging through hundreds of hits.
local MAX_RESULTS: integer = 200

local DEFAULT_SETTINGS: {string: any} = {
   index_path = DataStorage:getDataDir() .. "/search/turbosearch.db",
   last_query = "",
   -- How the query is read: book text, book details, or a pattern.
   search_mode = "Text",
}

--- A settings file, and the calls made on it.
---
--- Declared here rather than taken from the stub, for the reason the binding's
--- interface is: a module cannot lend its types, so the caller declares the shape
--- it wants and casts into it.
local record SettingsFile
   data: {any: any}
   readSetting: function(SettingsFile, string, any): any
   saveSetting: function(SettingsFile, string, any)
   flush: function(SettingsFile)
end

--- One option checkbox, and the state it holds.
---
--- Declared here rather than taken from the stub, for the reason `QueryDialog` is:
--- a module cannot lend its types to the module that requires it, and a list of
--- `any` cannot be indexed — which is what this needs to do when the Search button
--- reads the boxes back.
local record OptionBox
   text: string
   checked: boolean
   parent: any
end

--- The query box, and the two calls made through it.
---
--- Declared here for the reason the binding's interface is, so the cast at the
--- construction joins the stub's type to it.
local record QueryDialog
   title: string
   input: string
   input_hint: string
   getInputText: function(QueryDialog): string
   onShowKeyboard: function(QueryDialog)
   addWidget: function(QueryDialog, any)
end

--- The settings file, opened once.
local settings_object: SettingsFile = nil as SettingsFile

--- One search result: the document it came from, and how well it matched.
local record Hit
   doc_id: integer
   score: integer
end

--- One row of a results list: a book's title and where it is.
local record ResultRow
   text: string
   mandatory: string
   path: string
end

--- The plugin, in the shape KOReader loads it.
---
--- `extend` hands back a table the checker cannot see into, so the record is what
--- types `self` inside each method below. Its signatures are the contract the
--- definitions are checked against: a method that stops returning what its callers
--- take stops compiling.
--- The menu a plugin registers itself into.
local record MenuRegistration
   registerToMainMenu: function(any, {string: any})
end

--- The application the plugin was loaded into: the FileManager or the Reader.
local record UiModule
   menu: MenuRegistration
end

local record Turbosearch
   name: string
   fullname: string
   ui: UiModule
   result_menu: any
   --- The option checkboxes the popup is showing, if any. Held here rather than in
   --- a local so the callback that reads them can reach them.
   option_boxes: {OptionBox}

   init: function(Turbosearch)
   loadSettings: function(Turbosearch): SettingsFile
   engine: function(Turbosearch): EngineBinding
   readSearchOptions: function(Turbosearch): {string: any}
   onDispatcherRegisterActions: function(Turbosearch)
   addToMainMenu: function(Turbosearch, {string: any})
   onTurbosearchSearch: function(Turbosearch)
   onTurbosearchSettings: function(Turbosearch, boolean)
   search: function(Turbosearch, string, {string: any})
   query: function(Turbosearch, EngineBinding, any, string, {string: any}): {Hit}, string
   count_text: function(Turbosearch, integer): string
   showEmpty: function(Turbosearch, string)
   openBook: function(Turbosearch, string)
end

--- The class base, applied by hand.
---
--- `WidgetContainer:extend` builds a *new* table, and the methods defined below
--- would land on this one instead of on it. What extend does is set a metatable
--- and make the base reachable through it, which is this one line: everything an
--- instance asks for that the plugin does not define falls through to the reader's
--- own widget machinery, `new` included, which is how KOReader instantiates the
--- module it loaded.
Turbosearch.name = "turbosearch"
Turbosearch.fullname = _("Book search")

--- The class base, applied by hand.
---
--- `WidgetContainer:extend` builds a *new* table, and the methods defined below
--- would land on this one instead of on it. What extend does is set a metatable
--- and make the base reachable through it, which is this one line: everything an
--- instance asks for that the plugin does not define falls through to the reader's
--- own widget machinery, `new` included, which is how KOReader instantiates the
--- module it loaded.
setmetatable(Turbosearch, { __index = WidgetContainer })

function Turbosearch:init()
   self:loadSettings()
   self:onDispatcherRegisterActions()
   self.ui.menu:registerToMainMenu(self)
end

function Turbosearch:loadSettings(): SettingsFile
   if not settings_object then
      local object = LuaSettings:open(
         DataStorage:getSettingsDir() .. "/turbosearch.lua") as SettingsFile
      for key, value in pairs(DEFAULT_SETTINGS) do
         if object.data[key] == nil then
            object.data[key] = value
         end
      end
      settings_object = object
   end
   return settings_object
end

--- Why the engine could not be loaded, for a reader to read.
local engine_error: string = ""

--- The binding, loading the engine the first time it is asked for.
---
--- A missing engine is expected rather than a bug — a reader cannot build one from
--- here — so the failure is reported as a message rather than a raise. One return
--- value keeps the caller's shape the same either way: the binding, or nil.
function Turbosearch:engine(): EngineBinding
   if not binding.is_loaded() then
      local library: string = plugin_dir() .. ENGINE_LIBRARY
      local ok: boolean
      local load_err: any
      ok, load_err = pcall(binding.load_lib, library)
      if not ok then
         engine_error = T(_("Could not load the search engine from %1.\n\n%2"),
            library, tostring(load_err))
         return nil
      end
   end
   engine_error = ""
   return binding as EngineBinding
end

function Turbosearch:onDispatcherRegisterActions()
   Dispatcher:registerAction(self.name .. "_search", {
      category = "none",
      event = "TurbosearchSearch",
      title = _("Search books"),
      general = true,
   })
end

function Turbosearch:addToMainMenu(menu_items: {string: any})
   menu_items.turbosearch = {
      text = _("Book search"),
      sorting_hint = "more_tools",
      callback = function()
         self:onTurbosearchSearch()
      end,
   }
   --- From the main menu there is nothing to come back to, so the round trip is
   --- only from the query box.
   menu_items.turbosearch_settings = {
      text = _("Book search settings"),
      sorting_hint = "more_tools",
      callback = function()
         self:onTurbosearchSettings(false)
      end,
   }
end

--- Opens the settings popup.
---
--- A separate entry point rather than a branch of the search one, so a reader who
--- wants to know what an index holds does not have to dismiss a query box first.
function Turbosearch:onTurbosearchSettings(return_to_search: boolean)
   local screen = Settings:new{ main = self, return_to_search = return_to_search }
   screen:show()
end

--- Shows the query box, pre-filled with the last query so a follow-up search is
--- one edit away.
function Turbosearch:onTurbosearchSearch()
   local settings = self:loadSettings()

   -- The query box, the options, and the two buttons — one screen, in the order a
   --- reader works through them: what am I looking for, what kind of a search is
   --- it, go.
   local dialog: QueryDialog
   dialog = InputDialog:new{
      title = _("Search in books"),
      input = settings:readSetting("last_query", "") as string,
      input_hint = _("a word, or \"two words\" for an exact phrase"),
      buttons = {
         {
            {
               text = ICON_CLOSE .. "  " .. _("Cancel"),
               callback = function()
                  UIManager:close(dialog)
               end,
            },
         },
         {
            {
               text = ICON_SETTINGS .. "  " .. _("Settings"),
               callback = function()
                  UIManager:close(dialog)
                  --- Settings is a round trip. Leaving it closes the query box and
                  --- opens it again, so a reader who came here to change the index
                  --- — or who tapped the button by mistake — is back at the keyword
                  --- rather than at the book list.
                  self:onTurbosearchSettings(true)
               end,
            },
            {
               text = ICON_SEARCH .. "  " .. _("Search"),
               is_enter_default = true,
               callback = function()
                  local query: string = dialog:getInputText()
                  -- The options are read before the dialog closes, because closing
                  --- it drops the widgets the state lives on.
                  local opts: {string: any} = self:readSearchOptions()
                  UIManager:close(dialog)
                  self:search(query, opts)
               end,
            },
         },
      },
   } as QueryDialog

   -- Each option is a checkbox below the input, and each one is a separate index
   --- operation rather than a strength setting, so none of them is a radio group
   --- and more than one can be on. The last one wins in the dispatch, which is
   --- stated on the label rather than left to be discovered.
   local meta_box = CheckButton:new{
      text = _("Metadata only — skip the book contents"),
      checked = settings:readSetting("opt_metadata", false) as boolean,
      parent = dialog,
   }
   dialog:addWidget(meta_box)

   local pattern_box = CheckButton:new{
      text = _("Regular expression — over the terms"),
      checked = settings:readSetting("opt_pattern", false) as boolean,
      parent = dialog,
   }
   dialog:addWidget(pattern_box)

   Turbosearch.option_boxes = { meta_box, pattern_box } as {OptionBox}
   UIManager:show(dialog)
   dialog:onShowKeyboard()
end

--- Reads the option checkboxes, and remembers them for the next search.
---
--- Remembered rather than reset, because a reader who looks for an author once
--- looks for another author next time, and the box being where they left it is the
--- difference between one tap and three.
function Turbosearch:readSearchOptions(): {string: any}
   local settings = self:loadSettings()
   --- The boxes, or an empty list — which is what a call from the settings screen
   --- rather than the popup looks like.
   local boxes: {OptionBox} = Turbosearch.option_boxes or ({} as {OptionBox})
   local metadata: boolean = settings:readSetting("opt_metadata", false) as boolean
   local pattern: boolean = settings:readSetting("opt_pattern", false) as boolean
   if boxes[1] then
      metadata = boxes[1].checked
   end
   if boxes[2] then
      pattern = boxes[2].checked
   end
   settings:saveSetting("opt_metadata", metadata)
   settings:saveSetting("opt_pattern", pattern)
   settings:flush()

   -- Which mode the two boxes amount to. Metadata first, because it is the narrower
   --- question: a pattern over an index that was never searched for text is a way of
   --- finding nothing slowly.
   local mode: string = "Text"
   if metadata then
      mode = "Metadata"
   elseif pattern then
      mode = "Pattern"
   end
   return { mode = mode, metadata = metadata, pattern = pattern }
end

--- Runs a query and shows the results, or the reason there are none.
---
--- Rows are built while the handle is open and closed before the menu is shown, so
--- no handle has to outlive the search that opened it.
function Turbosearch:search(raw_query: string, opts?: {string: any})
   local query: string = raw_query:gsub("^%s+", ""):gsub("%s+$", "")
   if query == "" then
      return
   end

   local engine = self:engine()
   if not engine then
      UIManager:show(InfoMessage:new{ text = engine_error })
      return
   end
   local settings = self:loadSettings()
   settings:saveSetting("last_query", query)
   settings:flush()

   -- The binding reports a failed open as a message rather than a raise, so a
   -- missing index is a row in the reader rather than a dead one.
   local handle: any
   local open_err: string
   handle, open_err = engine.open_index(settings:readSetting("index_path", "") as string)
   if not handle then
      UIManager:show(InfoMessage:new{
         text = T(_("Could not open the index at %1.\n\n%2"),
            settings:readSetting("index_path", ""), open_err),
      })
      return
   end

   -- The engine raises rather than returning an error, so every call is guarded:
   -- pcall keeps a bad query from taking the reader down.
   local hits, how = self:query(engine, handle, query, opts)
   if not hits then
      engine.close(handle)
      UIManager:show(InfoMessage:new{ text = how })
      return
   end

   local items: {ResultRow} = {}
   for i = 1, math.min(#hits, MAX_RESULTS) do
      local hit = hits[i]
      local path: string = engine.book_path(handle, hit.doc_id)
      local title: string = engine.book_title(handle, hit.doc_id)
      -- The format goes on the right, where the reader's own file browser puts
      -- it: a list of Vietnamese titles is a list of strings that all begin the
      -- same way, and the format is the one distinguishing thing a row can carry
      -- without a second line.
      local suffix: string = path:match("%.([^%.]+)$") or ""
      items[i] = {
         text = title ~= "" and title or path:match("[^/]+$"),
         mandatory = suffix:upper(),
         path = path,
      }
   end
   engine.close(handle)

   if #items == 0 then
      self:showEmpty(query)
      return
   end

   self.result_menu = Menu:new{
      title = _("Search results"),
      subtitle = T(_("“%1” — %2, %3"), query, how, self:count_text(#hits)),
      item_table = items,
      covers_full_width = true,
      onMenuSelect = function(_menu, item: ResultRow)
         self:openBook(item.path)
      end,
   }
   UIManager:show(self.result_menu)
end

--- Runs the query in whichever mode the reader chose.
---
--- The dispatch is inside the binding so a caller cannot send a pattern to the
--- phrase path and quietly get a phrase: the three are separate index operations,
--- not one operation with a flag. Returns the hits plus a phrase saying where they
--- came from, or nil plus an error message.
function Turbosearch:query(engine: EngineBinding, handle, query: string,
                           opts?: {string: any}): {Hit}, string
   local chosen: {string: any} = opts or {} as {string: any}
   local mode: string = (chosen.mode or "Text") as string
   local how = _("found in book text")
   if mode == "Metadata" then
      how = _("matching title or author")
   elseif mode == "Pattern" then
      how = _("matching a pattern")
   end

   --- The binding reports a failed query as a message rather than a raise, so a
   --- bad query is something a reader is shown instead of something that takes the
   --- reader down.
   local hits: {Hit}
   local query_err: string
   hits, query_err = engine.search_as(handle, mode, query, nil)
   if query_err ~= "" then
      return hits, query_err
   end
   return hits, how
end

function Turbosearch:count_text(count: integer): string
   if count > MAX_RESULTS then
      return T(_("first %1 of %2"), MAX_RESULTS, count)
   end
   return T(_("%1 book(s)"), count)
end

--- Shows the empty state on the menu rather than in a dialog, so the reader
--- starts the next query from where the list would have been.
---
--- The single row is there because an empty item_table renders a blank list and no
--- explanation: Menu would show the title and nothing else, and a reader who typed
--- a query is owed to know it ran. There is no `dimmed` to grey it out with on
--- this reader's version, so the text says it instead.
function Turbosearch:showEmpty(query: string)
   self.result_menu = Menu:new{
      title = _("Search results"),
      subtitle = T(_("“%1” — nothing found in the book text"), query),
      item_table = {
         { text = _("No book contains this"), mandatory = "", path = "" },
      },
      covers_full_width = true,
   }
   UIManager:show(self.result_menu)
end

--- Opens a book, from wherever the reader was.
---
--- `self.ui` is the FileManager in the browser and the Reader inside a book, and
--- only the former can open a file. FileManager.openFile is called as a plain
--- function with self.ui passed in — the idiom the other plugins use — because for
--- a document it reaches ReaderUI:showReader, which does not depend on the caller
--- being the FileManager.
function Turbosearch:openBook(path: string)
   if not path or path == "" then
      return
   end
   FileManager.openFile(self.ui, path)
end

return Turbosearch
