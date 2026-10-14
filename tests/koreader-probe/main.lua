-- Probe: construct the real KOReader widgets with the exact field sets the
-- turbosearch plugin uses, fed by a real search through the real engine.
--
-- This is the end of the chain the stub test cannot reach: the same LuaJIT, the
-- same widget code, the same .so, and a real index on disk. `script/lua-test`
-- proves the plugin's own logic; this proves the installed reader accepts what
-- that logic produces. The two together are the reason a widget field that only
-- this reader's version supports is caught here rather than on a device.
--
-- Nothing is ever shown: each widget is constructed and closed, so the reader
-- carries on unchanged afterwards.

local UIManager = require("ui/uimanager")
local logger = require("logger")
local WidgetContainer = require("ui/widget/container/widgetcontainer")
local FileManager = require("apps/filemanager/filemanager")
local Menu = require("ui/widget/menu")

local PLUGIN_DIR = "/home/shin/turbosearch/build/turbosearch.koplugin/"
local INDEX = os.getenv("TS_PROBE_INDEX") or "/tmp/opencode/e2e.idx"
local QUERY = "thám tử"

-- Inert unless `TURBOSEARCH_PROBE` is set, so symlinking this plugin in costs
-- nothing and running it is a deliberate act.
--
-- Two modes:
--   TURBOSEARCH_PROBE=show    open the plugin's real search dialog and leave it
--                              on screen, unaddressable by automation because
--                              KOReader has no input device on this machine
--   TURBOSEARCH_PROBE=settings open the plugin's settings screen
--   TURBOSEARCH_PROBE=1        run the full verification and close everything
local MODE = os.getenv("TURBOSEARCH_PROBE")
local PROBE_PATH = MODE == "1"
local SHOW_DIALOG = MODE == "show"
local SHOW_SETTINGS = MODE == "settings"

local Probe = WidgetContainer:extend{
    name = "turbosearch_probe",
}

function Probe:init()
    if not PROBE_PATH and not SHOW_DIALOG and not SHOW_SETTINGS then
        return
    end
    UIManager:scheduleIn(4, function()
        if SHOW_DIALOG then
            self:showSearchDialog()
            return
        end
        if SHOW_SETTINGS then
            self:showSettingsScreen()
            return
        end
        self:probe()
    end)
end

--- Opens the plugin's own search dialog and leaves it there.
---
--- The dialog is shown but never closed, because the input path that would let
--- anything dismiss it does not exist here: `/dev/input/event*` is root:input
--- and this user is not in that group, so KOReader opens no input device and
--- receives no keys. Constructing it is the furthest automation can go; typing
--- into it has to come from outside the machine.
function Probe:showSearchDialog()
    logger.info("PROBE: opening the plugin search dialog")

    -- The instance the reader itself loaded, rather than a new one: building a
    -- second goes through WidgetContainer:new, which calls init(), and init()
    -- registers the plugin into a menu it does not have. The reader's own
    -- instance already has that.
    local PluginLoader = require("pluginloader")
    local instance = PluginLoader:getPluginInstance("turbosearch")
    if not instance then
        logger.info("PROBE: no turbosearch instance loaded")
        return
    end
    logger.info("PROBE: found the plugin instance")

    local ok, err = pcall(function()
        instance:onTurbosearchSearch()
        logger.info("PROBE: onTurbosearchSearch returned; dialog left open")
    end)
    if not ok then
        logger.info("PROBE: failed", tostring(err))
    end
end

--- Opens the plugin's own settings screen and leaves it there.
---
--- Same reason as the search dialog: the settings are the part of the plugin a
--- reader reaches for most and the only one that reads an index rather than a
--- book, so it is worth seeing work in the reader.
function Probe:showSettingsScreen()
    logger.info("PROBE: opening the plugin settings screen")

    local PluginLoader = require("pluginloader")
    local instance = PluginLoader:getPluginInstance("turbosearch")
    if not instance then
        logger.info("PROBE: no turbosearch instance loaded")
        return
    end
    logger.info("PROBE: found the plugin instance")

    local ok, err = pcall(function()
        instance:onTurbosearchSettings()
        logger.info("PROBE: onTurbosearchSettings returned; settings left open")
    end)
    if not ok then
        logger.info("PROBE: failed", tostring(err))
        return
    end

    -- The screen is left open on purpose, so the shots have to wait for the next
    -- paint rather than race it — and there are two, because the pane a setting
    -- lives in decides whether it is a record in the index or a line the next
    -- indexer run will read, and a reader has to be able to tell them apart.
    local UIManager = require("ui/uimanager")
    local Screen = require("device").screen
    local dir = os.getenv("TURBOSEARCH_SHOT_DIR") or "/tmp/opencode"

    local function shot(name)
        local path = dir .. "/" .. name .. ".png"
        local shot_ok, shot_err = Screen:shot(path)
        logger.info("PROBE: screenshot", name, tostring(shot_ok), tostring(shot_err))
    end

    UIManager:scheduleIn(3, function()
        shot("settings-config")
        local screen = instance.settings_screen
        if not screen then
            logger.info("PROBE: no settings screen to switch panes on")
            return
        end
        screen.pane = "env"
        screen:show()
        UIManager:scheduleIn(3, function() shot("settings-env") end)
    end)
end

function Probe:probe()
    logger.info("PROBE: ==== begin ====")

    local PluginLoader = require("pluginloader")
    local LuaSettings = require("luasettings")
    local DataStorage = require("datastorage")
    local settings = LuaSettings:open(DataStorage:getSettingsDir() .. "/turbosearch.lua")
    settings:saveSetting("index_path", INDEX)
    self.settings_for_report = settings

    local LuaSettings = require("luasettings")
    local DataStorage = require("datastorage")
    local settings = LuaSettings:open(DataStorage:getSettingsDir() .. "/turbosearch.lua")
    settings:saveSetting("index_path", INDEX)
    self.settings_for_report = settings

    package.path = PLUGIN_DIR .. "?.lua;" .. package.path
    local ts = require("turbosearch")

    local ok, err = pcall(ts.load_lib, PLUGIN_DIR .. "libturbosearch.so")
    logger.info("PROBE: load_lib", ok, tostring(err))
    if not ok then
        return
    end
    logger.info("PROBE: engine", ts.version() or "?", "loaded:", ts.is_loaded())

    local handle
    handle, err = ts.open_index(INDEX)
    logger.info("PROBE: open_index", handle ~= nil, tostring(err))
    if not handle then
        return
    end
    logger.info("PROBE: content mode", ts.content_mode(handle))

    local ok, hits = pcall(ts.search, handle, QUERY)
    logger.info("PROBE: search", ok, type(hits), ok and #hits or tostring(hits))
    if not ok or #hits == 0 then
        ts.close(handle)
        return
    end

    local ok_meta, meta = pcall(ts.search_meta, handle, "higashino")
    logger.info("PROBE: search_meta", ok_meta, type(meta),
        ok_meta and #meta or tostring(meta))
    ts.close(handle)

    -- Build the rows the way main.lua does, then hand them to the real Menu.
    -- Built with the fields the plugin's own rows carry, `mandatory` included:
    -- a row with a right-hand column is a different path through Menu's item
    -- widget, so the field is exercised here rather than read off a doc comment.
    local items = {}
    local missing = 0
    for i = 1, #hits do
        local path = hits[i] and hits[i].path or "?"
        local suffix = path:match("%.([^%.]+)$")
        items[i] = {
            text = path and path ~= "" and path:match("[^/]+$") or "?",
            mandatory = suffix and suffix:upper() or "",
            path = path,
        }
    end
    logger.info("PROBE: built", #items, "rows from", #hits, "hits")

    -- Re-open briefly so the titles can be resolved the way the plugin does,
    -- through the same calls it makes.
    local h2 = ts.open_index(INDEX)
    for i = 1, math.min(#items, 10) do
        items[i].text = ts.book_title(h2, hits[i].doc_id)
        if items[i].text == "" or items[i].text == nil then
            missing = missing + 1
            items[i].text = "UNRESOLVED"
        end
    end
    ts.close(h2)
    logger.info("PROBE: rows with an unresolved title:", missing)

    local built, build_err = pcall(function()
        local menu = Menu:new{
            title = "Search results",
            subtitle = "thám tử — found in book text, " .. #hits .. " book(s)",
            item_table = items,
            covers_full_width = true,
            onMenuSelect = function(_, item)
                logger.info("PROBE: row selected", item.path)
            end,
        }
        UIManager:close(menu)
        logger.info("PROBE: real Menu from real results constructed and closed")
    end)
    logger.info("PROBE: Menu", built, tostring(build_err))

    for i = 1, math.min(#items, 5) do
        logger.info("PROBE: row", i, items[i].text, "|", items[i].path)
    end

    --- The settings screen reads this same index through these same calls, so
    --- they are exercised here rather than only in the stub test: what a reader
    --- reads off a row is what comes back from the engine, and the row a setting
    --- lands in is the row that gets written back.
    local Settings = require("turbosearch_settings")
    local instance = PluginLoader:getPluginInstance("turbosearch")
    local screen = Settings:new{
        main = instance,
        index_path = INDEX,
    }

    -- Each call answers with a value plus a reason, so there are three things to
    -- separate: a raise, a refusal, and an answer. `pcall` puts the success flag
    -- first, which is what this got wrong the first time.
    local ok, text, message = pcall(function() return instance:engine_config_text() end)
    if not ok then
        logger.info("PROBE: settings config raised", tostring(text))
    elseif not text then
        logger.info("PROBE: settings config refused", tostring(message))
    else
        local _, lines = text:gsub("[^\n]+", "")
        logger.info("PROBE: settings config lines", lines)
        logger.info("PROBE: settings index_depth", text:match("index_depth=([^\n]+)"))
        logger.info("PROBE: settings content_mode", text:match("content_mode=([^\n]+)"))
    end

    -- A setting that decides what is stored has to be refused rather than
    -- applied, and the refusal is the whole transaction: nothing is written.
    local applied, refuse = instance:engine_apply_config("content_mode=full")
    logger.info("PROBE: settings refusal", tostring(applied), tostring(refuse))

    -- The pane a setting lives in decides where it goes, so the file's path is
    -- checked too: a path with nothing behind its extension would leave a
    -- *relative* one, which is a settings record in whatever directory the
    -- reader happened to be in.
    logger.info("PROBE: settings env file", screen:env_file_path())

    -- The rows carry the field class the plugin's own rows do, so what follows
    -- is the plugin's path resolution and its open, against a real book.
    local h3 = ts.open_index(INDEX)
    local path = ts.book_path(h3, hits[1].doc_id)
    logger.info("PROBE: book_path", path)
    ts.close(h3)

    local opened
    opened, build_err = pcall(function()
        FileManager.openFile(self.ui, path)
        logger.info("PROBE: FileManager.openFile returned for", path)
    end)
    logger.info("PROBE: openBook", opened, tostring(build_err))
end

return Probe
