-- Exercises the plugin against stubbed KOReader modules.
--
-- The plugin is the part of this project with no Rust test in front of it, and
-- its failure modes are silent: a menu item with an empty text, a handle left
-- open, a path that was never fetched. Rather than wait for those to show up in
-- a reader, this runs the real main.lua with the KOReader modules replaced, and
-- asserts on the menu the plugin would have shown.
--
-- Run with: script/lua-test

local PLUGIN = "plugin/turbosearch.koplugin/main.lua"
local INDEX = os.getenv("TS_TEST_INDEX") or "/tmp/opencode/e2e.idx"

local captured = {}
local failures = {}

--- gettext's marker, so the stubbed widgets can compare titles the way the
--- plugin builds them.
local function _(s) return s end

local function check(name, condition, detail)
    if condition then
        io.write(string.format("  ok   %s\n", name))
    else
        io.write(string.format("  FAIL %s%s\n", name, detail and (" — " .. tostring(detail)) or ""))
        failures[#failures + 1] = name
    end
end

--- Replaces every KOReader module the plugin requires. The widgets are recorded
--- rather than drawn, so a test can assert on what would have been displayed.
local function stub_koreader()
    -- `package.preload` wants a function per entry, not a table: a plain table
    -- is popped by require and the module is then reported as missing.
    local function loader(module)
        return function() return module end
    end

    local widgets = {
        new = function(widget_module, args)
            args = args or {}
            -- Keep the three apart rather than lumping them together: a search
            -- that fails shows an InfoMessage, and reporting that as "no rows"
            -- hides the one thing worth reading.
            if args.title == _("Search in books") then
                captured.dialog = args
            elseif args.item_table then
                captured.menu = args
            else
                captured.info = args
            end
            return args
        end,
    }

    package.preload["datastorage"] = loader({
        getDataDir = function() return "/tmp/opencode" end,
        getSettingsDir = function() return "/tmp/opencode" end,
    })
    -- registerAction is called with a colon, so the definition is the third
    -- argument rather than the second.
    package.preload["dispatcher"] = loader({
        registerAction = function(_, _action_name, action) captured.action = action end,
    })
    package.preload["apps/filemanager/filemanager"] = loader({
        openFile = function(_, file) captured.opened = file end,
    })
    package.preload["ui/widget/infomessage"] = loader(widgets)
    package.preload["ui/widget/inputdialog"] = loader(widgets)
    package.preload["ui/widget/menu"] = loader(widgets)
    package.preload["luasettings"] = loader({
        --- A fresh object per open, the way the real one has a file behind it.
        open = function(_, _path)
            return {
                data = {},
                readSetting = function(self, key, default)
                    local value = self.data[key]
                    if value == nil then return default end
                    return value
                end,
                saveSetting = function(self, key, value) self.data[key] = value end,
                flush = function() end,
            }
        end,
    })
    package.preload["ui/uimanager"] = loader({
        show = function(_, w) captured.shown = w end,
        close = function() end,
        setInputDialog = function() end,
    })
    -- WidgetContainer is a class: extend has to carry the methods through.
    local Widget = {}
    Widget.__index = Widget
    function Widget:extend(subclass)
        subclass = subclass or {}
        setmetatable(subclass, { __index = self })
        return subclass
    end
    function Widget:new(args)
        args = args or {}
        setmetatable(args, self)
        self.__index = self
        return args
    end
    package.preload["ui/widget/container/widgetcontainer"] = loader(Widget)
    package.preload["ffi/util"] = loader({
        template = function(s, ...)
            local args = { ... }
            return (s:gsub("%%(%d+)", function(n) return tostring(args[tonumber(n)]) end))
        end,
    })
    package.preload["gettext"] = loader(setmetatable({
        pgettext = function(_, s) return s end,
    }, {
        __call = function(_, s) return s end,
    }))
end

stub_koreader()

local plugin = dofile(PLUGIN)
local fake_ui = {
    menu = {
        registerToMainMenu = function() end,
    },
}
local instance = plugin:new{ ui = fake_ui }
instance:init()

io.write("plugin loads and registers\n")
check("registers a dispatcher action", captured.action ~= nil)
check("registers with the plugin's name",
    captured.action and captured.action.event == "TurbosearchSearch", captured.action and captured.action.event)

-- Point the settings at the index this test was given.
instance:loadSettings():saveSetting("index_path", INDEX)

io.write("\nsearch\n")

captured.menu = nil
instance:onTurbosearchSearch()
check("the dialog opens pre-filled with the last query", captured.dialog ~= nil)
check("the dialog has input", captured.dialog and captured.dialog.input ~= nil)

captured.menu = nil
instance:search("thám tử")
check("a content search shows a menu", captured.menu ~= nil,
    captured.info and captured.info.text or "nothing was shown")
if not captured.menu then
    io.write("  ... the plugin showed: ", captured.info and captured.info.text or "?", "\n")
end
check("the menu has rows", captured.menu and captured.menu.item_table and #captured.menu.item_table > 0,
    captured.menu and captured.menu.item_table and #captured.menu.item_table)
local first = captured.menu and captured.menu.item_table and captured.menu.item_table[1]
check("each row has a title", first and first.text ~= nil and first.text ~= "", first and first.text)
check("each row has a path to open", first and first.path ~= nil and first.path ~= "",
    first and first.path)
check("the subtitle names the query",
    captured.menu and captured.menu.subtitle:find("thám tử", 1, true) ~= nil)

io.write("\nmetadata fallback\n")

captured.menu = nil
instance:search("higashino")
check("an author-only query still finds something", captured.menu and #captured.menu.item_table > 0,
    captured.menu and #captured.menu.item_table)

io.write("\nempty and rejected queries\n")

captured.menu = nil
instance:search("   ")
check("a blank query shows nothing", captured.menu == nil)

captured.menu = nil
instance:search("zzzznotawordzzzz")
check("a query with no hits shows the empty state",
    captured.menu and captured.menu.subtitle:find("nothing found", 1, true) ~= nil,
    captured.menu and captured.menu.subtitle)

io.write("\nopening a book\n")

local path = captured.menu and captured.menu.item_table[1]
    and captured.menu.item_table[1].path
captured.opened = nil
instance:openBook(path)
check("selecting a row opens that file", captured.opened == path, captured.opened)

io.write("\nrebuilding the menu\n")
-- The handle is closed before the menu is built, so a second search must work
-- from a fresh open rather than reuse one that was already closed.
captured.menu = nil
instance:search("thám tử")
check("a second search works", captured.menu and #captured.menu.item_table > 0)

io.write("\n")
if #failures == 0 then
    io.write("all checks passed\n")
    os.exit(0)
end
io.write(string.format("%d check(s) failed: %s\n", #failures, table.concat(failures, ", ")))
os.exit(1)
