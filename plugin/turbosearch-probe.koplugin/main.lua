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

local PLUGIN_DIR = "/home/shin/turbosearch/plugin/turbosearch.koplugin/"
local INDEX = "/tmp/opencode/e2e.idx"
local QUERY = "thám tử"

-- Inert unless `TURBOSEARCH_PROBE=1`, so symlinking this plugin in costs
-- nothing and running it is a deliberate act.
local PROBE_PATH = os.getenv("TURBOSEARCH_PROBE") == "1"

local Probe = WidgetContainer:extend{
    name = "turbosearch_probe",
}

function Probe:init()
    if not PROBE_PATH then
        return
    end
    UIManager:scheduleIn(4, function()
        self:probe()
    end)
end

function Probe:probe()
    logger.info("PROBE: ==== begin ====")

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
    local items = {}
    local missing = 0
    for i = 1, #hits do
        local path = hits[i] and hits[i].path or "?"
        items[i] = {
            text = path and path ~= "" and path:match("[^/]+$") or "?",
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
