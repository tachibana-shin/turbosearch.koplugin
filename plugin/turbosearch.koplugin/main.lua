-- turbosearch: full-text search across the book library.
--
-- The index is built offline by the `indexer` binary in this repository; this
-- plugin only reads it. That split is deliberate: indexing a library takes tens
-- of seconds and writes hundreds of megabytes on a Kindle, which is not
-- something to do from a menu callback. Run the indexer, then search here.
--
-- The FFI binding lives in Teal (tl/turbosearch.tl) and compiles to
-- turbosearch.lua beside this file, because the C ABI is where a wrong type
-- costs a use-after-free rather than a Lua error. This file stays plain Lua:
-- KOReader's own modules are untyped, so typing them would mean maintaining
-- stubs for somebody else's API to no benefit.
--
-- Nothing here blocks the UI thread. A query on a full index answers in tens of
-- milliseconds, and the engine handle is opened, used and closed inside one
-- search, so a stale handle never outlives the menu it fed.

local DataStorage = require("datastorage")
local Dispatcher = require("dispatcher")
local FileManager = require("apps/filemanager/filemanager")
local InfoMessage = require("ui/widget/infomessage")
local InputDialog = require("ui/widget/inputdialog")
local LuaSettings = require("luasettings")
local Menu = require("ui/widget/menu")
local Settings = require("turbosearch_settings")
local UIManager = require("ui/uimanager")
local WidgetContainer = require("ui/widget/container/widgetcontainer")
local ffiUtil = require("ffi/util")
local _ = require("gettext")
local T = ffiUtil.template

local LIB_NAME = "turbosearch"

-- Maximum rows this menu will build. Beyond this the page takes longer to
-- render than a reader is willing to sit still for, and someone looking for a
-- book is refining the query rather than paging through hundreds of hits.
local MAX_RESULTS = 200

local DEFAULT_SETTINGS = {
    index_path = DataStorage:getDataDir() .. "/search/turbosearch.db",
    last_query = "",
    -- How the query is read: book text, book details, or a pattern.
    search_mode = "Text",
}

local Turbosearch = WidgetContainer:extend{
    name = "turbosearch",
    fullname = _("Book search"),
}

-- The directory this plugin was loaded from.
--
-- Resolved from the running source rather than hard-coded: the plugin is
-- normally a symlink into ~/.config/koreader, and debug.getinfo reports the
-- path the file was actually loaded through, which is also where the engine
-- and the generated binding sit.
--
-- The leading `@` is part of the debug representation rather than the path,
-- and dropping it keeps the resolved directory from becoming a name such as
-- "@plugin/turbosearch.koplugin/" that no require can match.
local function plugin_dir()
    local source = debug.getinfo(1, "S").source
    if source:sub(1, 1) == "@" then
        source = source:sub(2)
    end
    return (source:match("(.*/)") or "./")
end

-- The engine library for the architecture LuaJIT is running on.
--
-- Both are built beside the plugin — the host library for the desktop reader and
-- the armv7 one for the device — and which one loads is decided from the
-- interpreter rather than from whichever file was left there last. An x86-64
-- library on a Kindle fails at ffi.load with a message about the ELF class, so
-- choosing by name is what keeps that from being a search rather than a load
-- error.
local function engine_name()
    if jit.arch == "arm" then
        return "libturbosearch-armv7.so"
    end
    return "libturbosearch.so"
end

--- Loads the engine and the binding, once, and remembers both.
---
--- A missing index is expected rather than a bug — the reader cannot build the
--- index from here — so its message says what to run instead.
local function load_engine()
    if Turbosearch.binding then
        return Turbosearch.binding
    end
    package.path = plugin_dir() .. "?.lua;" .. package.path
    local ok, binding = pcall(require, LIB_NAME)
    if not ok then
        return nil, T(_("Could not load the search engine: %1"), tostring(binding))
    end
    local library = plugin_dir() .. engine_name()
    local loaded, load_err = pcall(binding.load_lib, library)
    if not loaded then
        return nil, T(_("Could not load the search engine from %1.\n\n%2"),
            library, tostring(load_err))
    end
    Turbosearch.binding = binding
    return binding
end

-- Opens the index. Returns nil plus a message ready to show.
--
-- The engine names the failing path in its own error text, so this only has
-- to say where it looked and what was wrong with it.
--
-- The settings object is passed in rather than read from the class: it is a
-- LuaSettings, whose keys live in `data`, so reaching into it as a table
-- would silently hand the engine an empty path.
local function open_index(binding, settings)
    local index_path = settings:readSetting("index_path")
    local handle, err = binding.open_index(index_path)
    if handle then
        return handle
    end
    return nil, T(_("Could not open the index at %1.\n\n%2"),
        index_path, tostring(err))
end

function Turbosearch:init()
    self:loadSettings()
    self:onDispatcherRegisterActions()
    self.ui.menu:registerToMainMenu(self)
end

function Turbosearch:loadSettings()
    if not Turbosearch.settings then
        local object = LuaSettings:open(DataStorage:getSettingsDir() .. "/turbosearch.lua")
        for key, value in pairs(DEFAULT_SETTINGS) do
            if object.data[key] == nil then
                object.data[key] = value
            end
        end
        Turbosearch.settings = object
    end
    return Turbosearch.settings
end

function Turbosearch:onDispatcherRegisterActions()
    Dispatcher:registerAction(self.name .. "_search", {
        category = "none",
        event = "TurbosearchSearch",
        title = _("Search books"),
        general = true,
    })
end

function Turbosearch:addToMainMenu(menu_items)
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

--- Opens the settings screen.
---
--- A separate entry point rather than a branch of the search one, so a reader
--- who wants to know what an index holds does not have to dismiss a query box to
--- find out.
function Turbosearch:onTurbosearchSettings()
    local settings = Settings:new{ main = self }
    settings:show()
end

--- Shows the query box, pre-filled with the last query so a follow-up search
--- is one edit away.
--- The engine, loading it if this is the first thing to ask.
---
--- Exposed because the settings screen reads an index before any search has run,
--- and reaching into `self.binding` for it made a reader who opened settings
--- first see "the engine is not loaded" — a dead end they had to leave by
--- searching and coming back. Loading on demand is the same call the search path
--- makes, cached, so it costs the first time only.
function Turbosearch:engine()
    return load_engine()
end

function Turbosearch:onTurbosearchSearch()
    local settings = self:loadSettings()
    local dialog
    dialog = InputDialog:new{
        title = _("Search in books"),
        input = settings:readSetting("last_query", ""),
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
                        local query = dialog:getInputText()
                        UIManager:close(dialog)
                        self:search(query)
                    end,
                },
            },
        },
    }
    UIManager:show(dialog)
    -- Raising the keyboard is what puts the cursor in the field. `UIManager:
    -- setInputDialog` used to be the way to hand the dialog to the reader as the
    -- active input, but no version here has that method and nothing calls it, so
    -- the call raised as soon as the dialog was shown and aborted the handler --
    -- the reader logged an error instead of focusing the field.
    dialog:onShowKeyboard()
end

--- Runs a query and shows the results, or the reason there are none.
---
--- Rows are built while the handle is open and closed before the menu is shown,
--- so no handle has to outlive the search that opened it.
function Turbosearch:search(raw_query)
    local query = raw_query:gsub("^%s+", ""):gsub("%s+$", "")
    if query == "" then
        return
    end

    local binding, err = load_engine()
    if not binding then
        UIManager:show(InfoMessage:new{ text = err })
        return
    end
    local settings = self:loadSettings()
    settings:saveSetting("last_query", query)
    settings:flush()

    local handle
    handle, err = open_index(binding, settings)
    if not handle then
        UIManager:show(InfoMessage:new{ text = err })
        return
    end

    -- The engine raises rather than returning an error, so every call is
    -- guarded: pcall keeps a bad query from taking the reader down.
    local hits, how = self:query(binding, handle, query)
    if not hits then
        binding.close(handle)
        UIManager:show(InfoMessage:new{ text = T(_("Search failed: %1"), tostring(how)) })
        return
    end

    local items = {}
    for i = 1, math.min(#hits, MAX_RESULTS) do
        local hit = hits[i]
        local path = binding.book_path(handle, hit.doc_id)
        local title = binding.book_title(handle, hit.doc_id)
        -- The format goes on the right, the way the reader's own file browser
        -- puts it there: a list of titles all starting with "An mang..." is a
        -- list of the same string, and the format is the one distinguishing
        -- thing a row can carry without another line.
        local suffix = path:match("%.([^%.]+)$")
        items[i] = {
            text = title ~= "" and title or path:match("[^/]+$"),
            mandatory = suffix and suffix:upper() or "",
            path = path,
        }
    end
    binding.close(handle)

    if #items == 0 then
        self:showEmpty(query)
        return
    end

    self.result_menu = Menu:new{
        title = _("Search results"),
        subtitle = T(_("“%1” — %2, %3"), query, how, self:countText(#hits)),
        item_table = items,
        covers_full_width = true,
        onMenuSelect = function(_, item)
            self:openBook(item.path)
        end,
    }
    UIManager:show(self.result_menu)
end

--- Runs the query, falling back to book details.
---
--- A reader looking for a book usually knows its name or author, not its
--- contents, so an empty content search is followed by a metadata search before
--- the result is reported as nothing. Returns the hits plus a phrase saying
--- where they came from, or nil plus an error message.
function Turbosearch:query(binding, handle, query)
    -- The mode is the reader's, and it is dispatched inside the binding so the
    -- caller cannot send a pattern to the phrase path and quietly get a phrase.
    local mode = self:loadSettings():readSetting("search_mode") or "Text"
    local how = _("found in book text")
    if mode == "Metadata" then
        how = _("matching title or author")
    elseif mode == "Pattern" then
        how = _("matching a pattern")
    end

    local ok, hits = pcall(binding.search_as, handle, mode, query)
    if not ok then
        return nil, tostring(hits)
    end
    return hits, how
end

function Turbosearch:countText(count)
    if count > MAX_RESULTS then
        return T(_("first %1 of %2"), MAX_RESULTS, count)
    end
    return T(_("%1 book(s)"), count)
end

--- Shows the empty state on the menu rather than in a dialog, so the reader
--- can start the next query from where the list would have been.
-- Shows the empty state on the menu rather than in a dialog, so the reader
-- starts the next query from where the list would have been.
--
-- The single row is there because an empty item_table renders a blank list and
-- no explanation: Menu would show the title and nothing else, and a reader who
-- typed a query is owed to know it ran. There is no `dimmed` to grey it out
-- with on this KOReader version, so the text says it instead.
function Turbosearch:showEmpty(query)
    self.result_menu = Menu:new{
        title = _("Search results"),
        subtitle = T(_("“%1” — nothing found in the book text"), query),
        item_table = {
            { text = _("No book contains this") },
        },
        covers_full_width = true,
    }
    UIManager:show(self.result_menu)
end

--- Opens a book, from wherever the reader was.
---
--- `self.ui` is the FileManager in the browser and the Reader inside a book,
-- and only the former can open a file. FileManager.openFile is called as a
--- plain function with self.ui passed in — the idiom the other plugins use —
--- because for a document it reaches ReaderUI:showReader, which does not
--- depend on the caller being the FileManager.
function Turbosearch:openBook(path)
    if not path or path == "" then
        return
    end
    FileManager.openFile(self.ui, path)
end

return Turbosearch
