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

--- The engine library for the architecture LuaJIT is running on.
---
--- Both are built beside the plugin — the host library for the desktop reader and
--- the armv7 one for the device — and which one loads is decided from the
--- interpreter rather than from whichever file was left there last. An x86-64
--- library on a Kindle fails at ffi.load with a message about the ELF class, so
--- choosing by name is what keeps that from being a search rather than a load
--- error.
local function engine_name(): string
   if jit.arch == "arm" then
      return "libturbosearch-armv7.so"
   end
   return "libturbosearch.so"
end

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

   init: function(Turbosearch)
   loadSettings: function(Turbosearch): SettingsFile
   engine: function(Turbosearch): EngineBinding
   onDispatcherRegisterActions: function(Turbosearch)
   addToMainMenu: function(Turbosearch, {string: any})
   onTurbosearchSearch: function(Turbosearch)
   onTurbosearchSettings: function(Turbosearch)
   search: function(Turbosearch, string)
   query: function(Turbosearch, EngineBinding, any, string): {Hit}, string
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
      local library: string = plugin_dir() .. engine_name()
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
   menu_items.turbosearch_settings = {
      text = _("Book search settings"),
      sorting_hint = "more_tools",
      callback = function()
         self:onTurbosearchSettings()
      end,
   }
end

--- Opens the settings popup.
---
--- A separate entry point rather than a branch of the search one, so a reader who
--- wants to know what an index holds does not have to dismiss a query box first.
function Turbosearch:onTurbosearchSettings()
   local screen = Settings:new{ main = self }
   screen:show()
end

--- Shows the query box, pre-filled with the last query so a follow-up search is
--- one edit away.
function Turbosearch:onTurbosearchSearch()
   local settings = self:loadSettings()
   local dialog: QueryDialog
   -- The stub's own type and this file's are the same object at run time and two
   -- different names to the checker; the cast is what joins them.
   dialog = InputDialog:new{
      title = _("Search in books"),
      input = settings:readSetting("last_query", "") as string,
      input_hint = _("a word, or \"two words\" for an exact phrase"),
      buttons = {
         {
            {
               text = _("Cancel"),
               callback = function()
                  UIManager:close(dialog)
               end,
            },
            {
               text = _("Settings"),
               callback = function()
                  UIManager:close(dialog)
                  self:onTurbosearchSettings()
               end,
            },
            {
               text = _("Search"),
               is_enter_default = true,
               callback = function()
                  local query: string = dialog:getInputText()
                  UIManager:close(dialog)
                  self:search(query)
               end,
            },
         },
      },
   } as QueryDialog
   UIManager:show(dialog)
   dialog:onShowKeyboard()
end

--- Runs a query and shows the results, or the reason there are none.
---
--- Rows are built while the handle is open and closed before the menu is shown, so
--- no handle has to outlive the search that opened it.
function Turbosearch:search(raw_query: string)
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
   local hits, how = self:query(engine, handle, query)
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
function Turbosearch:query(engine: EngineBinding, handle, query: string): {Hit}, string
   local mode: string = self:loadSettings():readSetting("search_mode", "Text") as string
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
