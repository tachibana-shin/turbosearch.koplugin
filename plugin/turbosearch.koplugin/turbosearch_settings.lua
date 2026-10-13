-- Settings, and everything the engine can be asked about itself.
--
-- Kept apart from main.lua because it is the half of the plugin that is read
-- most and changed most, and because it is the only part that talks to more than
-- one index at a time: a reader switching an index needs the full report of
-- where it points before the switch takes effect, and a file browser is not the
-- place to discover that the old index is still the one being read.
--
-- The tabs follow the question a reader actually arrives with. "What have I got"
-- is answered by the index it points at, its size and its terms; "what can it
-- do" is the content mode, which is decided by whoever built the index and not
-- changeable here; "what am I looking for" is the search mode, which is the one
-- thing this screen does change.

local ConfirmBox = require("ui/widget/confirmbox")
local DataStorage = require("datastorage")
local InfoMessage = require("ui/widget/infomessage")
local Menu = require("ui/widget/menu")
local UIManager = require("ui/uimanager")
local WidgetContainer = require("ui/widget/container/widgetcontainer")
local ffiUtil = require("ffi/util")
local _ = require("gettext")
local T = ffiUtil.template

local Settings = WidgetContainer:extend{
    name = "turbosearch_settings",
}

--- Sizes a byte count in a unit a reader can act on.
---
--- Bytes with no unit are what a directory listing gives and what nobody can
--- act on: the difference between the three content modes is a factor of a
--- hundred, so the unit is what makes the number a decision.
local function format_bytes(bytes)
    if bytes < 1024 then
        return T(_("%1 B"), bytes)
    end
    if bytes < 1024 * 1024 then
        return T(_("%1 kB"), math.floor(bytes / 1024))
    end
    return string.format("%.1f MB", bytes / (1024 * 1024))
end

--- Describes the search mode the way the query box's hint does.
---
--- The three are separate index operations, not one operation with a flag, so
--- the label has to say which rather than describe a level of "strength".
local function mode_label(mode)
    if mode == "Metadata" then
        return _("Title, author and series only")
    end
    if mode == "Pattern" then
        return _("Regular expression over the terms")
    end
    return _("Book text (phrases in quotes)")
end

--- Describes how much content the index holds, and what it costs.
local function content_label(mode, search_mode)
    if mode == "FileList" then
        return _("File list only — nothing to search inside")
    end
    if mode == "TermsOnly" then
        return _("Terms, no positions — no phrase search")
    end
    return _("Full text — phrases allowed")
end

--- Opens the index briefly and reports what it holds.
---
--- The handle is closed before anything is shown, for the same reason the search
--- path closes it: a settings screen that holds a database open while the reader
--- reads it is a database open while the reader is doing something else.
function Settings:report(settings)
    local plugin = self.main
    local binding, bind_err = plugin:engine()
    if not binding then
        return nil, bind_err or _("The engine could not be loaded.")
    end

    local index_path = settings:readSetting("index_path")
    local handle, err
    handle, err = binding.open_index(index_path)
    if not handle then
        return nil, T(_("Could not open the index at %1.\n\n%2"),
            index_path, tostring(err))
    end

    local info = binding.index_info(handle)
    binding.close(handle)

    -- A content mode that cannot answer the search mode chosen is worth saying
    -- rather than leaving the reader to find out by typing a query.
    local warn = nil
    if info.mode ~= "Full" then
        if settings:readSetting("search_mode") == "Pattern" then
            warn = _("The index only stores terms without positions, so a pattern search \
will find the documents but cannot rank or highlight them.")
        else
            warn = _("The index only stores terms without positions, so phrases in \
quotes will not match. Change the search mode to leave quotes loose, or rebuild the index.")
        end
    end
    if info.mode == "FileList" then
        warn = _("This index is only a list of files: it has no text to search at all. \
Point the plugin at an index built with CONTENT_MODE=terms or full.")
    end

    return {
        index = index_path,
        bytes = info.bytes,
        terms = info.terms,
        content = content_label(info.mode, settings:readSetting("search_mode")),
        mode = mode_label(settings:readSetting("search_mode")),
        warn = warn,
    }, ""
end

--- Shows the settings screen.
function Settings:show()
    local plugin = self.main
    local settings = plugin:loadSettings()

    local report, err = self:report(settings)
    if not report then
        UIManager:show(ConfirmBox:new{
            title = _("No index"),
            text = err,
            ok_text = _("Close"),
        })
        return
    end

    local items = {
        { text = _("Search"), sub_text = report.mode },
        { text = _("Index"), sub_text = report.index },
        { text = _("Content"), sub_text = report.content },
        { text = _("Size"), sub_text = format_bytes(report.bytes) },
        { text = _("Terms"), sub_text = string.format("%d", report.terms) },
    }
    if report.warn then
        items[#items + 1] = {
            text = _("Not available"),
            sub_text = report.warn,
            dim = true,
        }
    end

    local menu
    menu = Menu:new{
        title = _("Book search"),
        item_table = items,
        covers_full_width = true,
        onMenuSelect = function(_, item)
            -- The first row is the one thing this screen changes; the rest are
            -- read from the index and cannot be, so they are marked read-only.
            if item.text == _("Search") then
                UIManager:close(menu)
                self:chooseMode(settings)
            elseif item.text == _("Index") then
                UIManager:show(InfoMessage:new{ text = report.index })
            else
                UIManager:show(InfoMessage:new{
                    text = item.sub_text or item.text,
                })
            end
        end,
    }
    UIManager:show(menu)
end

--- Chooses what the query means.
---
--- Secured as a menu rather than a dialog because it is three choices and no
--- typing: a dialog with three buttons is the same control with more
--- interaction in front of it.
function Settings:chooseMode(settings)
    local options = {
        { text = _("Book text"), sub_text = _("phrases in quotes match exactly"), mode = "Text" },
        { text = _("Title, author, series"), sub_text = _("skip the book contents"), mode = "Metadata" },
        { text = _("Regular expression"), sub_text = _("over the terms in the index"), mode = "Pattern" },
    }

    local menu
    menu = Menu:new{
        title = _("What should the query mean?"),
        item_table = options,
        covers_full_width = true,
        onMenuSelect = function(_, item)
            settings:saveSetting("search_mode", item.mode)
            settings:flush()
            UIManager:close(menu)
            self:show()
        end,
    }
    UIManager:show(menu)
end

--- Opens the index directory, for a reader who wants to copy one across.
---
--- The generator is the indexer binary in this repository, not something the
--- plugin can do: building an index reads every book and writes hundreds of
--- megabytes, which is not a job for a menu callback on an e-reader. So the
--- plugin says what to run rather than pretending it can.
function Settings:openIndexFolder(settings)
    local index_path = settings:readSetting("index_path")
    local dir = index_path:match("(.*[/\\])")
    UIManager:show(InfoMessage:new{
        text = dir or index_path,
    })
end

return Settings
