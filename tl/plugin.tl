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
local ConfirmBox = require("ui/widget/confirmbox")
local InputDialog = require("ui/widget/inputdialog")
local LuaSettings = require("luasettings")
local BookList = require("ui/widget/booklist")
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
   --- The sentence a hit is in. An empty string means there is none.
   snippet: function(any, integer, integer, integer): string, string
   book_title: function(any, integer): string
   book_path: function(any, integer): string
   index_info: function(any): any
   --- Scans a directory and says how many files still need reading. An empty
   --- directory means every configured root.
   index_begin: function(any, string): integer, string
   --- Reads at most `budget` of what the session has left, and says what is
   --- still to do.
   index_step: function(any, integer): integer, string
   --- Ends a session, reporting what it did as `key=value` lines.
   index_end: function(any): string, string
   --- The whole config, as `key = value` lines.
   config_text: function(any): string, string
   --- Applies settings lines, leaving the rest alone.
   apply_config: function(any, string): boolean, string
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
--- nf-md-refresh.1, checked against the font's own cmap: rebuilding is the
--- same question as searching, asked earlier.
local ICON_INDEX: string = ""

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
   entry_idx: integer
   position: integer
end

--- One row of a results list: a book, where it is, and where in it the match
--- is.
local record ResultRow
   text: string
   mandatory: string
   path: string
   --- Which book the row belongs to, and where in that book the match is. Both
   --- are here for the sentence: a row nobody looks closer at never pays for
   --- them, and the sentence is the reason the row was built.
   doc_id: integer
   entry_idx: integer
   position: integer
   --- The sentence, once it has been read. Held here so a reader who looks at
   --- the same row twice reads the book once.
   preview: string
   --- Whether it has been looked for yet. A string cannot say "not yet", and
   --- "not yet" and "there is none" are two different answers to show.
   looked: boolean
   --- A metadata hit has no sentence behind it — it matched the title page, not
   --- the text — and asking a book for the sentence around position 0 is asking
   --- it for its opening line, which is not what was searched for.
   can_preview: boolean
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
   --- The open book, when the plugin was opened from inside one. `nil` in the
   --- browser, which is the whole difference between the two cases below.
   document: OpenDocument
end

local record OpenDocument
   file: string
end

local record Turbosearch
   name: string
   fullname: string
   ui: UiModule
   result_menu: any
   --- The settings screen, while it is open. See `onTurbosearchSettings`.
   settings_screen: any
   --- The option checkboxes the popup is showing, if any. Held here rather than in
   --- a local so the callback that reads them can reach them.
   option_boxes: {OptionBox}

   init: function(Turbosearch)
   loadSettings: function(Turbosearch): SettingsFile
   engine: function(Turbosearch): EngineBinding
   readSearchOptions: function(Turbosearch): {string: any}
   --- The settings screen calls these three on the plugin.
   engine_config_text: function(Turbosearch): string, string
   engine_apply_config: function(Turbosearch, string): boolean, string
   settings_index_path: function(Turbosearch): string
   current_directory: function(Turbosearch): string
   build_index: function(Turbosearch, EngineBinding, any, string, boolean, function(boolean, string))
   step_index: function(Turbosearch, EngineBinding, any, integer, function(boolean, string))
   index_now: function(Turbosearch)
   close_index_message: function(Turbosearch)
   --- The engine, for a read that happens after the search closed its own
   --- handle: a sentence is read when the reader asks, which is later.
   reopen_engine: function(Turbosearch): EngineBinding
   --- What a tap on a row means: the sentence, and the book behind it.
   show_row: function(Turbosearch, ResultRow)
   --- Reads the sentence out of the book — the only expensive thing a search
   --- does once it has already answered.
   read_sentence: function(Turbosearch, ResultRow): string
   index_summary: function(Turbosearch, string): string
   index_message: any
   --- The query's results, shown once the index is up to date.
   results: function(Turbosearch, EngineBinding, any, string, {string: any})

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

--- The stored config as text, for the settings screen.
---
--- Goes through the same handle a search uses, because the engine is open once and
--- the settings are a second question asked of it.
function Turbosearch:engine_config_text(): string, string
   local engine = self:engine()
   if not engine then
      return nil, engine_error
   end
   local handle, err = engine.open_index(
      self:loadSettings():readSetting("index_path", "") as string)
   if not handle then
      return nil, tostring(err)
   end
   --- The handle is a borrow, so the config is read through it and the handle
   --- closed, rather than the other way round.
   local ok: boolean
   local text: string
   local reason: string
   ok, text, reason = pcall(engine.config_text, handle)
   engine.close(handle)
   if not ok then
      return nil, tostring(text)
   end
   if text == nil then
      return nil, tostring(reason)
   end
   return text, ""
end

--- Applies settings lines, for the settings screen.
function Turbosearch:engine_apply_config(text: string): boolean, string
   local engine = self:engine()
   if not engine then
      return false, engine_error
   end
   local handle, err = engine.open_index(
      self:loadSettings():readSetting("index_path", "") as string)
   if not handle then
      return false, tostring(err)
   end
   local ok: boolean
   local applied: boolean
   local reason: string
   ok, applied, reason = pcall(engine.apply_config, handle, text)
   engine.close(handle)
   if not ok then
      return false, tostring(applied)
   end
   if not applied then
      return false, tostring(reason)
   end
   return true, ""
end

--- The index the settings are about.
function Turbosearch:settings_index_path(): string
   return self:loadSettings():readSetting("index_path", "") as string
end

--- Opens the settings popup.
---
--- A separate entry point rather than a branch of the search one, so a reader who
--- wants to know what an index holds does not have to dismiss a query box first.
function Turbosearch:onTurbosearchSettings(return_to_search: boolean)
   local screen = Settings:new{
      main = self,
      return_to_search = return_to_search,
      pane = "config",
      index_path = self:loadSettings():readSetting("index_path", "") as string,
   }
   --- Kept on the plugin rather than left local, because a settings screen is a
   --- round trip: the reader comes back to it from wherever the search went, and
   --- a screen nothing holds is a screen nothing can put back.
   self.settings_screen = screen
   --- `reload` rather than `show`, because the first thing a settings screen owes
   --- a reader is what is in force, not what it was last time.
   screen:reload()
end

--- The folder the reader is standing in.
---
--- A book being open puts the reader in that book's folder — which is what a
--- reader who searches from inside a chapter means — and failing that, the
--- folder being browsed. Empty only when neither exists, and an empty answer is
--- then not a fallback to "everything" chosen quietly: there is no folder to
--- speak of, so there is nothing narrower to search.
function Turbosearch:current_directory(): string
   local ui: UiModule = self.ui
   local doc = ui and ui.document
   if doc and doc.file and doc.file ~= "" then
      local dir: string = doc.file:match("^(.*)/[^/]+$")
      if dir and dir ~= "" then
         return dir
      end
   end

   local fm = FileManager.instance
   if fm and fm.file_chooser and fm.file_chooser.path then
      return fm.file_chooser.path
   end
   return ""
end

--- Shows the query box, pre-filled with the last query so a follow-up search is
--- one edit away.
function Turbosearch:onTurbosearchSearch()
   local settings = self:loadSettings()

   -- The query box, the options, and the three buttons — one screen, in the order
   --- a reader works through them: what am I looking for, what kind of a search is
   --- it, build, go.
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
               text = ICON_INDEX .. "  " .. _("Index"),
               callback = function()
                  UIManager:close(dialog)
                  self:index_now()
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

   -- Off by default: the folder the reader is standing in is what a search from
   --- inside it means, and a reader who wants every book asked instead can say so
   --- without it being the answer they did not pick.
   local global_box = CheckButton:new{
      text = _("Search the whole library"),
      checked = settings:readSetting("opt_global", self:current_directory() == "") as boolean,
      parent = dialog,
   }
   dialog:addWidget(global_box)

   Turbosearch.option_boxes = { meta_box, pattern_box, global_box } as {OptionBox}
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
   local whole: boolean = settings:readSetting("opt_global", self:current_directory() == "") as boolean
   if boxes[3] then
      whole = boxes[3].checked
   end
   settings:saveSetting("opt_metadata", metadata)
   settings:saveSetting("opt_pattern", pattern)
   settings:saveSetting("opt_global", whole)
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

   -- Where the question is being asked. An empty scope is the whole library, and
   --- it is the answer in two cases for different reasons: every book was asked
   --- for, or there is no folder here to ask about — a reader on the main menu is
   --- not standing in anything, and searching a directory nobody picked would be
   --- an answer to a question they did not ask.
   local here: string = self:current_directory()
   if here == "" then
      whole = true
   end
   return {
      mode = mode,
      metadata = metadata,
      pattern = pattern,
      whole = whole,
      scope = whole and "" or here,
   }
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
   -- A search answers from what has been indexed, so what the reader is standing
   --- in is built first. Indexing is not a setting and not another screen: a
   --- reader who types a word is asking a question, and the folder they are in is
   --- the answer they mean.
   local chosen: {string: any} = opts or {} as {string: any}
   local scope: string = (chosen.scope or "") as string
   local whole: boolean = (chosen.whole == true)
   self:build_index(engine, handle, scope, whole, function(built: boolean, why: string)
      if not built then
         engine.close(handle)
         UIManager:show(InfoMessage:new{
            text = T(_("Could not index %1.\n\n%2"),
               scope ~= "" and scope or _("the library"), why),
         })
         return
      end
      self:results(engine, handle, query, chosen)
   end)
end

--- Runs the query and shows what it found, now that the index can answer it.
---
--- The handle is closed by whichever branch of it ends the trip, so no handle has
--- to outlive the search that opened it.
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
   --- `chosen` and not `nil`: the scope is in there, and a query that drops it
   --- answers out of the folder the reader is standing in — the one thing the
   --- box they just set was there to stop.
   local hits: {Hit}
   local query_err: string
   hits, query_err = engine.search_as(handle, mode, query, chosen)
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

function Turbosearch:results(engine: EngineBinding, handle: any, query: string,
                             opts: {string: any})
   local hits, how = self:query(engine, handle, query, opts)
   if not hits then
      engine.close(handle)
      UIManager:show(InfoMessage:new{ text = how })
      return
   end

   -- A metadata hit matched the cover and the title page, not the text, so it
   --- has no sentence behind it and a row that offers one would be offering its
   --- opening line.
   local can_preview: boolean = ((opts.mode or "Text") as string) ~= "Metadata"

   local items: {ResultRow} = {}
   for i = 1, math.min(#hits, MAX_RESULTS) do
      local hit = hits[i]
      local path: string = engine.book_path(handle, hit.doc_id)
      local title: string = engine.book_title(handle, hit.doc_id)
      items[i] = {
         text = title ~= "" and title or path:match("[^/]+$"),
         mandatory = (path:match("%.([^%.]+)$") or ""):upper(),
         path = path,
         doc_id = hit.doc_id,
         entry_idx = hit.entry_idx,
         position = hit.position,
         preview = "",
         looked = false,
         can_preview = can_preview,
      }
   end
   engine.close(handle)

   if #items == 0 then
      self:showEmpty(query)
      return
   end

   --- A book list and not a menu, which is the same thing wearing the reader's
   --- own clothes: reads a row's path for its cover, its status and its sidecar,
   --- exactly as the file browser's book tabs do. A results list that looks like
   --- the shelf the book came from is the difference between a search and a tool.
   self.result_menu = BookList:new{
      title = _("Search results"),
      subtitle = T(_("“%1” — %2, %3"), query, how, self:count_text(#hits)),
      item_table = items,
      covers_full_width = true,
      --- No letters down the edge: a reader is not typing into a shelf, and an
      --- alphabet beside every row is the one thing that made a results list
      --- look like a debug screen rather than a list of books.
      is_enable_shortcut = false,
      --- Every row comes from the same book list, so a row that opens is a row
      --- CoverBrowser would draw a cover beside.
      onMenuSelect = function(_menu, item: ResultRow)
         self:show_row(item)
      end,
   }
   UIManager:show(self.result_menu)
end

--- What a tap on a row means.
---
--- The sentence, when the match is in the text: the reader did not search for a
--- book, they searched for a word, and the sentence is what answers that. The
--- book is one tap further on, which is where it belongs — a reader who wanted
--- the book itself would have opened it from the shelf.
---
--- A hit with no sentence behind it — a metadata match, or a book whose sections
--- have moved since it was indexed — opens the book. There is nothing to show,
--- and a tap that opens a box to say so is a tap wasted.
function Turbosearch:show_row(row: ResultRow)
   if row.can_preview and not row.looked then
      row.preview = self:read_sentence(row)
      row.looked = true
   end

   local sentence: string = row.preview or ""
   if sentence == "" then
      self:openBook(row.path)
      return
   end

   --- The sentence in a box, with the book behind it. `TextViewer` would let a
   --- reader resize and scroll it, but a sentence is two lines at most and a tap
   --- elsewhere is a tap on the book.
   UIManager:show(ConfirmBox:new{
      text = sentence,
      --- A glyph before the sentence, because a plain box over a list looks like
      --- an error until it is read.
      icon = "text",
      ok_text = _("Open"),
      cancel_text = _("Back"),
      ok_callback = function()
         self:openBook(row.path)
      end,
   })
end

--- Reads the sentence a match is in, out of the book it is in.
---
--- A second handle: the search closed its own before it built the rows, and a
--- sentence is asked for later than that. Opening the index again costs a page
--- of SQLite, and the decode it pays for beside it is a thousand times that, so
--- sharing the handle would save nothing and would need a row to outlive its
--- search.
---
--- An empty string on the way out, and no box about it: the reader asked a
--- question the book can no longer answer, and the book is still there to open.
function Turbosearch:read_sentence(row: ResultRow): string
   local engine = self:engine()
   if not engine then
      return ""
   end
   local settings = self:loadSettings()
   local handle: any = engine.open_index(
      settings:readSetting("index_path", "") as string)
   if not handle then
      return ""
   end

   local text: string
   local snippet_err: string
   text, snippet_err = engine.snippet(handle, row.doc_id, row.entry_idx, row.position)
   engine.close(handle)
   if snippet_err ~= "" then
      return ""
   end
   return text
end

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

--- Brings the index up to date with whatever `scope` covers, then reports.
---
--- Reads whatever still needs reading and reports through `on_done(true, report)`
--- when the index can answer — including when there was nothing to build, which
--- is the usual case and costs one `stat` per file and no writes — or
--- `on_done(false, reason)` when it could not. The handle is not closed: whoever
--- opened it still owns it.
---
--- The whole library is minutes the first time and seconds afterwards, so a
--- reader who asked for it is asked before those minutes are spent. A folder is
--- not, because that is what a search from inside a folder costs anyway — the
--- question is only whether it happens once or once per query.
function Turbosearch:build_index(engine: EngineBinding, handle: any, scope: string,
                                 whole: boolean, on_done: function(boolean, string))
   local pending: integer
   local begin_err: string
   pending, begin_err = engine.index_begin(handle, scope)
   if pending < 0 then
      on_done(false, begin_err)
      return
   end
   if pending == 0 then
      -- Nothing needs reading. This is the common case for a second search, and
      --- the whole reason the scan that finds it is worth running every time.
      on_done(true, "")
      return
   end

   if whole then
      UIManager:show(ConfirmBox:new{
         text = T(_("%1 books have not been indexed yet.\n\nBuilding them takes minutes the first time, and seconds after that.\n\nIndex them now?"),
            tostring(pending)),
         ok_text = ICON_INDEX .. "  " .. _("Index"),
         cancel_text = _("Search what is indexed"),
         ok_callback = function()
            self:step_index(engine, handle, pending, on_done)
         end,
         cancel_callback = function()
            -- Not a failure. The reader was offered the minutes and chose not to
            --- spend them, and a search over what is there still answers.
            on_done(true, "")
         end,
      })
      return
   end

   self:step_index(engine, handle, pending, on_done)
end

--- Indexes a session a batch at a time, then finishes it.
---
--- Stepped rather than done in one call because a call that never returns is a
--- screen that never draws: each batch hands control back to the event loop, so a
--- tap still closes the book while a library is being read.
---
--- One message, painted once. A Kindle repaints the whole screen, so updating it
--- per batch would spend longer clearing the display than the reading does, and
--- would leave a page flickering while somebody tries to read over it.
function Turbosearch:step_index(engine: EngineBinding, handle: any, total: integer,
                                on_done: function(boolean, string))
   local BATCH: integer = 25
   self.index_message = InfoMessage:new{
      text = T(_("Indexing %1 books…"), tostring(total)),
   }
   UIManager:show(self.index_message)

   local function step()
      local left: integer
      local step_err: string
      left, step_err = engine.index_step(handle, BATCH)
      if left < 0 then
         self:close_index_message()
         on_done(false, step_err)
         return
      end
      if left > 0 then
         UIManager:tickAfterNext(step)
         return
      end

      local report: string
      local end_err: string
      report, end_err = engine.index_end(handle)
      self:close_index_message()
      if not report then
         on_done(false, end_err)
         return
      end
      on_done(true, report)
   end
   step()
end

--- Dismisses the indexing message, when one is up.
function Turbosearch:close_index_message()
   if self.index_message then
      UIManager:close(self.index_message)
      self.index_message = nil
   end
end

--- Turns a session's `key=value` report into something worth reading.
---
--- A report full of zeroes is the normal outcome of asking twice, and saying so
--- plainly is better than a sentence that reads like work was done.
function Turbosearch:index_summary(report: string): string
   if report == "" then
      return _("The index was already up to date.")
   end
   local indexed: string = "0"
   for line in report:gmatch("[^\n]+") do
      local key, value: string, string = line:match("^([%w_]+)=(.*)$")
      if key == "indexed" then
         indexed = value
      end
   end
   if indexed == "0" then
      return _("The index was already up to date.")
   end
   return T(_("Indexed %1 books."), indexed)
end

--- Builds what the reader is looking at, on its own.
---
--- The same work a search does before it answers, offered by itself: a reader who
--- knows they are about to look for something in this folder pays for it once, and
--- the query after that — and the one after that — goes straight to the index.
function Turbosearch:index_now()
   local engine = self:engine()
   if not engine then
      UIManager:show(InfoMessage:new{ text = engine_error })
      return
   end
   local settings = self:loadSettings()
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

   -- The Index button answers the same question the checkbox asks, so it reads
   --- the checkbox rather than inventing a scope of its own.
   local here: string = self:current_directory()
   local whole: boolean = settings:readSetting("opt_global", self:current_directory() == "") as boolean
   if here == "" then
      whole = true
   end
   local scope: string = whole and "" or here
   self:build_index(engine, handle, scope, whole, function(built: boolean, why: string)
      engine.close(handle)
      if not built then
         UIManager:show(InfoMessage:new{
            text = T(_("Could not index %1.\n\n%2"),
               scope ~= "" and scope or _("the library"), why),
         })
         return
      end
      UIManager:show(InfoMessage:new{ text = self:index_summary(why) })
   end)
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
