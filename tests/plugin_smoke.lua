-- Exercises the plugin against stubbed KOReader modules.
--
-- The plugin is the part of this project with no Rust test in front of it, and
-- its failure modes are silent: a menu item with an empty text, a handle left
-- open, a path that was never fetched. Rather than wait for those to show up in
-- a reader, this runs the real main.lua with the KOReader modules replaced, and
-- asserts on the menu the plugin would have shown.
--
-- Run with: script/lua-test

local PLUGIN_DIR = "build/turbosearch.koplugin/"
local PLUGIN = PLUGIN_DIR .. "main.lua"
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

-- The plugin's own modules sit beside main.lua and are required by name, so the
-- directory has to be on the path before it is loaded.
package.path = PLUGIN_DIR .. "?.lua;" .. package.path
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
            elseif args.buttons then
                captured.popup = args
            else
                captured.info = args
            end
            -- The plugin calls through the widget rather than only reading fields,
            -- so a stub that is a plain table is not enough for the calls the
            -- dialog path makes.
            args.getInputText = function(self) return self.input or "" end
            args.onShowKeyboard = function() end
            --- Widgets added by addWidget are collected, so a test can read the
            --- checkboxes the plugin put on the screen.
            args.added = {}
            args.addWidget = function(self, w) table.insert(self.added, w) end
            args.onCloseWidget = function() end
            args.free = function() end
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
    package.preload["ui/widget/checkbutton"] = loader(widgets)
    package.preload["ui/widget/inputdialog"] = loader(widgets)
    package.preload["ui/widget/menu"] = loader(widgets)
    package.preload["ui/widget/confirmbox"] = loader(widgets)
    package.preload["ui/widget/buttondialog"] = loader(widgets)
    -- Measuring, not drawing: the plugin sizes the right-hand column from the
    -- screen it is on rather than from a count of characters, and this is the
    -- seam where that shows.
    package.preload["device"] = loader({
        screen = {
            getWidth = function(self) return 1072 end,
            getHeight = function(self) return 1448 end,
            scaleBySize = function(self, value) return value end,
        },
    })
    package.preload["ui/font"] = loader({
        getFace = function(self, name, size) return { name = name, size = size } end,
    })
    -- About eight pixels a character in `infont` at this size: enough for the
    -- test to tell "fits" from "does not" without a font engine behind it.
    local function fits(text, width)
        return #text <= math.floor(width / 8)
    end

    package.preload["ui/rendertext"] = loader({
        --- Hands back the part of the value that fits, unchanged — the question
        --- a caller asks before it decides that anything needs a mark.
        getSubTextByWidth = function(self, text, face, width, kerning, bold)
            if fits(text, width) then
                return text
            end
            return text:sub(1, math.floor(width / 8))
        end,

        --- Records what was asked to be shortened, and returns it whole: the
        --- test wants to know which values were treated as too wide, and the
        --- reader's own ellipsis is the reader's to draw.
        truncateTextByWidth = function(self, text, face, width, kerning, bold)
            captured.column_marked = captured.column_marked or {}
            captured.column_marked[#captured.column_marked + 1] = text
            -- Keep the longest one seen: every row passes through here, and it
            -- is the long value — forty extensions, a whole path — that a
            -- caller with a character budget would have sliced before this.
            if not captured.column_input or #text > #captured.column_input then
                captured.column_input = text
            end
            captured.column_budget = width
            captured.column_face = face
            return text
        end,
    })
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

-- The plugin's own modules sit beside main.lua and are required by name, so the
-- directory has to be on the path before it is loaded.
package.path = PLUGIN_DIR .. "?.lua;" .. package.path

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

--- The options the popup carries. Each is a checkbox under the input, and the
--- reader reads them back when Search is pressed.
local boxes = captured.dialog and captured.dialog.added or {}
check("the popup shows an option per index operation", #boxes == 2,
    #boxes .. " boxes")
local texts = {}
for _, box in ipairs(boxes) do texts[box.text] = true end
check("metadata is one of the options",
    texts["Metadata only — skip the book contents"] == true, texts)
check("a regular expression is one of the options",
    texts["Regular expression — over the terms"] == true, texts)

--- The buttons: settings on the left, search on the right, each with an icon.
local row = captured.dialog and captured.dialog.buttons
    and captured.dialog.buttons[2] or {}
check("settings and search are two buttons in one row", #row == 2, #row)
--- Each button starts with a nerdfont glyph in the Private Use Area, which the
--- reader draws through its fallback font — a character, not a decoded image.
check("settings is on the left", row[1] and row[1].text:find("Settings"),
    row[1] and row[1].text)
check("search is on the right", row[2] and row[2].text:find("Search"),
    row[2] and row[2].text)
check("each button carries a nerdfont glyph",
    row[1] and #row[1].text > #("Settings") and row[2] and #row[2].text > #("Search"),
    row[1] and row[1].text)

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
-- The format on the right is what makes a list of Vietnamese titles readable:
-- without it every row starts with the same three letters.
check("each row carries its format on the right",
    first and first.mandatory ~= nil and first.mandatory ~= "",
    first and first.mandatory)
check("the format is the file's suffix, capitalised",
    first and first.mandatory == first.path:match("%.([^%.]+)$"):upper(),
    first and first.mandatory)
check("the subtitle names the query",
    captured.menu and captured.menu.subtitle:find("thám tử", 1, true) ~= nil)

io.write("\nsearch modes\n")

-- Book text, and then the other two by switching the setting the settings screen
-- writes. All three reach the same menu, which is the point: a mode is a choice
-- about what the query means, not a different result screen.
local modes = { "Text", "Metadata", "Pattern" }
for _, mode in ipairs(modes) do
    -- The phrase is what Text is for, and the Japanese name is what the other
    -- two are for: it must appear in one under content search and in both under
    -- metadata or pattern, which is what makes the modes a real choice.
    local query = mode == "Text" and "thám tử" or "higashino"
    instance:loadSettings():saveSetting("search_mode", mode)
    captured.menu = nil
    instance:search(query)
    local n = captured.menu and captured.menu.item_table and #captured.menu.item_table or 0
    check(string.format("mode %s returns rows", mode), n > 0,
        captured.info and captured.info.text or captured.menu and n or "nothing")
    check(string.format("mode %s names the query", mode),
        captured.menu and captured.menu.subtitle
            and captured.menu.subtitle:find(query, 1, true) ~= nil,
        captured.menu and captured.menu.subtitle)
end
instance:loadSettings():saveSetting("search_mode", "Text")

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

-- Search again first: the empty-state query above left the menu holding a row
-- with no path, which is the correct thing for that screen and the wrong row to
-- try to open.
captured.menu = nil
instance:search("thám tử")
local path = captured.menu and captured.menu.item_table[1]
    and captured.menu.item_table[1].path
check("a row to open exists", path and path ~= "", path)
captured.opened = nil
instance:openBook(path)
check("selecting a row opens that file", captured.opened == path, captured.opened)

io.write("\nsettings screen\n")

-- The stub host answers with the same lines the engine would, so the screen
-- renders real rows rather than empty ones.
local function stub_host(overrides)
   local text = table.concat({
      "source_dirs=/mnt/us/documents",
      -- The long ones are the point: a real index carries forty extensions and
      -- a dozen folders to skip, and those are the values a column sized by a
      -- character budget would have had to throw away.
      "include_extensions=epub,kepub,mobi,azw,azw3,txt,text,md,markdown,fb2,html,xhtml",
      "exclude_extensions=log,lua,json,opf,ncx,css,js,db,bak,tmp,old,swp,sdr,toc,cover,png,jpg,gif,webp,bmp,ico,svg,mp3,ttf,zip,gz,pdf,djvu,cbz,cbr",
      "exclude_dir_globs=*.sdr,.calibrece,.thumbnails,@eaDir,__MACOSX",
      "min_file_size=5120",
      "max_file_size=67108864",
      "index_depth=32",
      "prefix_mode=auto",
      "index_only_when_charging=true",
      "meta_only_unparseable=false",
      "content_mode=full",
   }, "\n")
   return {
      engine_config_text = function(self) return text, "" end,
      engine_apply_config = function(self, t) return true, "" end,
      onTurbosearchSearch = function(self) end,
   }
end

local Settings = dofile("/home/shin/turbosearch/build/turbosearch.koplugin/turbosearch_settings.lua")
--- The index path the env file is derived from comes from the host, the way the
--- plugin hands it over, rather than being invented here.
local host = stub_host()
host.index_path = function(self) return "/tmp/opencode/turbosearch-test.idx" end
--- The path arrives with the screen, the way the plugin hands it over.
local screen = Settings.new(Settings, {
   main = host, pane = "config", index_path = "/tmp/opencode/turbosearch-test.idx",
})
captured.menu = nil
screen:reload()
local items = captured.menu and captured.menu.item_table or {}
check("the settings open as a screen with one row per setting", #items == 11,
    #items .. " rows")
check("a row shows what is in force, in the right-hand column",
    items[7] and items[7].mandatory == "32" and items[7].text ~= nil,
    items[7] and items[7].mandatory)
check("a choice setting is a choice, not a text field",
    items[8] and items[8].setting.edit == "choice",
    items[8] and items[8].setting.edit)

-- The column is sized by the screen it is on, in the face it will be painted
-- in, and the plugin hands the whole value over to be shortened rather than
-- slicing it first: a character budget in the caller is how forty-eight
-- characters came to be wider than the row and take the screen down.
check("the column is measured against the screen, not a character count",
    captured.column_budget and captured.column_budget > 0
        and captured.column_budget < 1072,
    captured.column_budget)
check("the value reaches the measurement whole, to be shortened there",
    captured.column_input and #captured.column_input > 48,
    captured.column_input and (#captured.column_input .. " chars"))
check("the column is measured in the face it is drawn in",
    captured.column_face and captured.column_face.name == "infont"
        and captured.column_face.size == 16,
    captured.column_face and captured.column_face.name)

-- The reader appends its ellipsis whether or not it cut anything, so calling it
-- blind marks every row — `5120…`, `auto…`, `full…` — and a mark that is always
-- there says nothing. Only a value that genuinely does not fit may end in one.
local marked = captured.column_marked or {}
local short_marked = {}
for _, value in ipairs(marked) do
    if #value <= 8 then short_marked[#short_marked + 1] = value end
end
check("a value that fits is shown whole, with no mark",
    #short_marked == 0, table.concat(short_marked, ", "))
check("a value that does not fit is the one that gets marked",
    #marked > 0 and #marked < 11, #marked .. " of 11 rows marked")

-- The env pane writes a file the next indexer run will read, and a variable
-- already in the environment still wins over it.
local env_file = "/tmp/opencode/turbosearch-test.env"
screen.pane = "env"
screen:save("env")
local written = io.open(env_file, "r")
local body = written and written:read("a") or ""
if written then written:close() end
check("the env pane writes a file with one line per setting",
    body:find("CONTENT_MODE", 1, true) ~= nil and body:find("MAX_BOOKS", 1, true) ~= nil,
    body)
check("the env file says the real environment still wins",
    body:find("wins", 1, true) ~= nil, body)

-- The settings file sits beside the index, so a path with nothing behind its
-- extension has no such place. The version that answered `.env` anyway wrote a
-- *relative* path — into whatever directory the test ran from, which is how this
-- project's own build-time `.env` came to hold a settings record instead of the
-- sqlx `DATABASE_URL`.
screen.index_path = ".idx"
check("a path with nothing behind its extension has no settings file",
    screen:env_file_path() == "", screen:env_file_path())
screen.index_path = ""
check("an empty index path has no settings file either",
    screen:env_file_path() == "", screen:env_file_path())
screen.index_path = "/tmp/opencode/turbosearch-test.idx"
check("a real index path does have one",
    screen:env_file_path() == "/tmp/opencode/turbosearch-test.env",
    screen:env_file_path())

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
