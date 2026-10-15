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
--- The words the index is expected to answer. Overridable for the same reason
--- `TS_PROBE_INDEX` is: the library is whatever was built for it, and a probe
--- that asks for one fixed phrase stops probing the plugin the moment the books
--- under it change.
local QUERY = os.getenv("TS_PROBE_QUERY") or "Yukino"
local META_QUERY = os.getenv("TS_PROBE_META_QUERY") or "Oregairu"

--- One screenshot, wherever it is taken from.
---
--- A path rather than a return value, because the shots outlive the frames they
--- catch: each one is written and then the reader paints over it.
local function shot(name)
    local dir = os.getenv("TURBOSEARCH_SHOT_DIR") or "/tmp/opencode"
    local path = dir .. "/" .. name .. ".png"
    local ok, err = require("device").screen:shot(path)
    logger.info("PROBE: screenshot", name, tostring(ok), tostring(err))
end

-- Inert unless `TURBOSEARCH_PROBE` is set, so symlinking this plugin in costs
-- nothing and running it is a deliberate act.
--
-- Four modes:
--   TURBOSEARCH_PROBE=show    open the plugin's real search dialog and leave it
--                              on screen, unaddressable by automation because
--                              KOReader has no input device on this machine
--   TURBOSEARCH_PROBE=settings open the plugin's settings screen
--   TURBOSEARCH_PROBE=results  show the results list and then the sentence
--                              behind its first row, both left on screen
--   TURBOSEARCH_PROBE=index    build the index where the reader is standing,
--                              one batch per turn of the event loop, and search
--   TURBOSEARCH_PROBE=1        run the full verification and close everything
local MODE = os.getenv("TURBOSEARCH_PROBE")
local PROBE_PATH = MODE == "1"
local SHOW_DIALOG = MODE == "show"
local SHOW_SETTINGS = MODE == "settings"
local RUN_INDEX = MODE == "index"
local RUN_RESULTS = MODE == "results"

--- Points the plugin at this run's index, and hands the reader's own back.
---
--- Not a courtesy. The reader's index path is the reader's, and leaving it
--- aimed at a scratch file in /tmp takes the reader's index away on the next
--- reboot — silently, in a file the reader never opens — which is how a probe
--- ends up costing somebody the thing it was written to test.
---
--- The function it returns puts it back, so every way out of a probe can reach
--- it: a probe has more early returns than it has steps.
local function aim_at(instance)
    local settings = instance:loadSettings()
    local was = settings:readSetting("index_path", "")
    settings:saveSetting("index_path", INDEX)
    settings:flush()
    logger.info("PROBE: indexing into", settings:readSetting("index_path", ""))
    --- Anonymous, because `return function name()` is not a thing Lua has: the
    --- named form is a statement and not an expression, and this is an expression.
    return function()
        settings:saveSetting("index_path", was)
        settings:flush()
        logger.info("PROBE: index path put back to", was ~= "" and was or "(unset)")
    end
end

local Probe = WidgetContainer:extend{
    name = "turbosearch_probe",
}

function Probe:init()
    if not PROBE_PATH and not SHOW_DIALOG and not SHOW_SETTINGS and not RUN_INDEX
            and not RUN_RESULTS then
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
        if RUN_INDEX then
            self:probeIndex()
            return
        end
        if RUN_RESULTS then
            self:probeResults()
            return
        end
        self:probe()
    end)
end

--- Shows the results list, and then the sentence behind its first row.
---
--- Both are left on screen because the list is what a reader looks at and the
--- sentence is what they were looking for, and neither is worth much described
--- in a log. The list is the plugin's own, built through the same `results`
--- the Search button calls, so what is on screen here is what a reader gets.
function Probe:probeResults()
    logger.info("PROBE: ==== results ====")

    local PluginLoader = require("pluginloader")
    local instance = PluginLoader:getPluginInstance("turbosearch")
    if not instance then
        logger.info("PROBE: no turbosearch instance loaded")
        return
    end
    logger.info("PROBE: found the plugin instance")

    local restore = aim_at(instance)
    local forced = os.getenv("TURBOSEARCH_HERE")
    if forced then
        instance.ui = { document = { file = forced } }
    end
    local here = instance:current_directory()
    local opts = instance:readSearchOptions()
    logger.info("PROBE: standing in", here == "" and "(every book)" or here)

    local engine = instance:engine()
    if not engine then
        logger.info("PROBE: engine not loaded")
        restore()
        return
    end
    local settings = instance:loadSettings()
    local handle = engine.open_index(settings:readSetting("index_path", ""))
    if not handle then
        logger.info("PROBE: open_index failed")
        restore()
        return
    end
    local hits, why = engine.search(handle, QUERY, opts)
    logger.info("PROBE: search", QUERY, #hits, tostring(why))

    -- The plugin's own path, so the list on screen is the list a reader gets.
    local Menu = require("ui/widget/menu")
    local ok, err = pcall(function()
        instance:results(engine, handle, QUERY, opts)
    end)
    logger.info("PROBE: results", ok, tostring(err))
    local menu = instance.result_menu
    if not menu then
        restore()
        return
    end
    logger.info("PROBE: rows", #menu.item_table)
    for i = 1, math.min(#menu.item_table, 6) do
        local row = menu.item_table[i]
        logger.info("PROBE: row", i, row.text, "|", row.path,
            "| previewable:", row.can_preview)
    end

    UIManager:scheduleIn(4, function()
        shot("results-list")
        -- The same menu, so the reader's own select path runs rather than a
        --- copy of it.
        local row = menu.item_table[1]
        local shown, preview_err = pcall(function() instance:show_row(row) end)
        logger.info("PROBE: preview", shown, tostring(preview_err),
            row.looked and "(read the book)" or "(nothing to read)")
        if row.preview == "" then
            logger.info("PROBE: preview was empty — the book opened instead")
        else
            logger.info("PROBE: the sentence", row.preview)
        end
        UIManager:scheduleIn(3, function()
            shot("results-sentence")
            restore()
            logger.info("PROBE: ==== end ====")
        end)
    end)
end

--- Builds the index where the reader is standing, and searches it.
---
--- The path the stub test cannot take: a batch per turn of the event loop, a
--- repaint per batch, and the two screens a reader really sees while it runs.
--- Everything here is what a tap on Index does, with a log where a reader
--- would be watching.
function Probe:probeIndex()
    logger.info("PROBE: ==== indexing ====")

    local PluginLoader = require("pluginloader")
    local instance = PluginLoader:getPluginInstance("turbosearch")
    if not instance then
        logger.info("PROBE: no turbosearch instance loaded")
        return
    end
    logger.info("PROBE: found the plugin instance")

    -- The plugin's own settings object, and not a second one over the same file:
    -- the plugin caches what it opened, so an index path written through a
    -- different object is a different world: the reader goes on indexing wherever
    -- it was already pointed, says so cheerfully, and the index this probe meant
    -- to build is never created.
    local restore = aim_at(instance)

    -- Where the reader is, which is the whole question the Index button answers.
    -- Inside a book it is the book's folder; on the main menu it is the folder
    -- the browser is showing; with neither there is nothing to narrow to, and
    -- that is the whole library.
    local forced = os.getenv("TURBOSEARCH_HERE")
    if forced then
        instance.ui = { document = { file = forced } }
    end
    local here = instance:current_directory()
    logger.info("PROBE: standing in", here == "" and "(every book)" or here)

    -- Counted rather than stubbed: the progress screen is built once and painted
    -- once, and a second one per batch would be the thing to catch.
    local shown = 0
    local real_show = UIManager.show
    UIManager.show = function(manager, widget)
        shown = shown + 1
        return real_show(manager, widget)
    end
    local ok, err = pcall(function() instance:index_now() end)
    UIManager.show = real_show
    logger.info("PROBE: index_now", ok, tostring(err), "screens shown:", shown)

    local message = instance.index_message
    if message then
        logger.info("PROBE: progress still up", tostring(message.text))
    end
    shot("index-progress")

    UIManager:scheduleIn(2, function()
        shot("index-summary")

        -- And now the question the index was built for, answered from it: once
        -- inside the folder, and once pointed somewhere that is not one, which
        -- is what "stays inside its own directory" looks like from here.
        local engine = instance:engine()
        if not engine then
            logger.info("PROBE: engine not loaded")
            restore()
            return
        end
        local h, open_err = engine.open_index(INDEX)
        if not h then
            logger.info("PROBE: open_index failed", tostring(open_err))
            restore()
            return
        end
        local rows, why = engine.search(h, QUERY, { scope = here })
        logger.info("PROBE: scoped search", QUERY, "in",
            here == "" and "every book" or here, #rows, tostring(why))
        if #rows > 0 then
            logger.info("PROBE: scoped row", engine.book_title(h, rows[1].doc_id),
                "|", engine.book_path(h, rows[1].doc_id))
        end
        local outside = engine.search(h, QUERY,
            { scope = here == "" and "/nowhere" or here .. "-not-a-folder" })
        logger.info("PROBE: search pointed outside its own folder", #outside)
        engine.close(h)
        restore()
        logger.info("PROBE: ==== end ====")
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
    local instance = PluginLoader:getPluginInstance("turbosearch")
    if not instance then
        logger.info("PROBE: no turbosearch instance loaded")
        return
    end
    logger.info("PROBE: found the plugin instance")

    -- The plugin's own settings object, and not a second one over the same file:
    -- the plugin caches what it opened, so an index path written through a
    -- different object is a different world: the reader goes on indexing wherever
    -- it was already pointed, says so cheerfully, and the index this probe meant
    -- to build is never created.
    local settings = instance:loadSettings()
    local restore = aim_at(instance)
    self.settings_for_report = settings

    package.path = PLUGIN_DIR .. "?.lua;" .. package.path
    local ts = require("turbosearch")

    local ok, err = pcall(ts.load_lib, PLUGIN_DIR .. "libturbosearch.so")
    logger.info("PROBE: load_lib", ok, tostring(err))
    if not ok then
        restore()
        return
    end
    logger.info("PROBE: engine", ts.version() or "?", "loaded:", ts.is_loaded())

    local handle
    handle, err = ts.open_index(INDEX)
    logger.info("PROBE: open_index", handle ~= nil, tostring(err))
    if not handle then
        restore()
        return
    end
    logger.info("PROBE: content mode", ts.content_mode(handle))

    local ok, hits = pcall(ts.search, handle, QUERY)
    logger.info("PROBE: search", ok, type(hits), ok and #hits or tostring(hits))
    if not ok or #hits == 0 then
        ts.close(handle)
        return
    end

    local ok_meta, meta = pcall(ts.search_meta, handle, META_QUERY)
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
            subtitle = QUERY .. " — found in book text, " .. #hits .. " book(s)",
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
    restore()
end

return Probe
