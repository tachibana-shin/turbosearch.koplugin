-- Settings, and everything the engine can be asked about itself.
--
-- Kept apart from main.lua because it talks to an index rather than to a book, and
-- because it is the half a reader arrives at wanting an answer rather than a
-- search.
--
-- A popup, not a menu: every other plugin on this reader shows its settings as
-- one, and with reason — the settings are something to glance at, act on and be
-- rid of, and a full-screen list is the wrong shape for five lines of
-- information. The report sits in the title where a popup puts what it is about,
-- and each button carries an icon, so a reader scanning for the one they want
-- finds it by shape before reading any text.

local ButtonDialog = require("ui/widget/buttondialog")
local ConfirmBox = require("ui/widget/confirmbox")
local InfoMessage = require("ui/widget/infomessage")
local UIManager = require("ui/uimanager")
local WidgetContainer = require("ui/widget/container/widgetcontainer")
local ffiUtil = require("ffi/util")
local _ = require("gettext")
local T = ffiUtil.template

local Settings = WidgetContainer:extend{
    name = "turbosearch_settings",
}

--- Icons, from KOReader's own set.
---
--- Named rather than inlined because the set is the reader's: an icon that ships
--- with KOReader renders in whatever style the reader is using, and one that does
--- not shows as a placeholder.
local ICON_SEARCH = "appbar.search"
local ICON_SETTINGS = "appbar.settings"
local ICON_FOLDER = "appbar.filebrowser"
local ICON_CLOSE = "close"

--- Sizes a byte count in a unit a reader can act on.
---
--- Bytes with no unit are what a directory listing gives and what nobody can act
--- on: the difference between the three content modes is a factor of a hundred, so
--- the unit is what turns the number into a decision.
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
--- The three are separate index operations, not one operation with a strength, so
--- the label says which rather than implying the others are weaker.
local function mode_short(mode)
    if mode == "Metadata" then
        return _("Title, author, series")
    end
    if mode == "Pattern" then
        return _("Pattern")
    end
    return _("Book text")
end

--- Describes how much content the index holds.
local function content_short(mode)
    if mode == "FileList" then
        return _("File list only")
    end
    if mode == "TermsOnly" then
        return _("Terms, no positions")
    end
    return _("Full text")
end

--- Opens the index briefly and reports what it holds.
---
--- The handle is closed before anything is shown, for the same reason the search
--- path closes it: a settings popup holding a database open while the reader reads
--- it is a database open while the reader is doing something else.
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

    -- A content mode that cannot answer the mode chosen is worth saying rather
    -- than leaving a reader to find out by typing a query.
    local warn = nil
    if info.mode == "FileList" then
        warn = _("This index is only a list of files, so it has no text to search. \
Build one with CONTENT_MODE=terms or full.")
    elseif info.mode == "TermsOnly" then
        warn = _("This index has no positions, so phrases in quotes will not match. \
Switch to a loose query, or rebuild the index.")
    end

    return {
        index = index_path,
        bytes = info.bytes,
        terms = info.terms,
        content = content_short(info.mode),
        mode = mode_short(settings:readSetting("search_mode")),
    }, ""
end

--- Opens the settings popup.
function Settings:show()
    local plugin = self.main
    local settings = plugin:loadSettings()

    local report, err = self:report(settings)
    if not report then
        UIManager:show(ConfirmBox:new{
            title = _("Book search"),
            text = err,
            icon = "texture-box",
            ok_text = _("Close"),
        })
        return
    end

    -- The report goes in the title, where a popup puts what it is about. Each
    -- line is a fact to read and none is something to tap, so none of them is a
    -- button.
    local title = _("Book search") .. "\n\n"
        .. _("Index") .. "  " .. report.index .. "\n"
        .. _("Size") .. "  " .. format_bytes(report.bytes) .. "\n"
        .. _("Terms") .. "  " .. string.format("%d", report.terms) .. "\n"
        .. _("Content") .. "  " .. report.content .. "\n"
        .. _("Query means") .. "  " .. report.mode

    local dialog
    dialog = ButtonDialog:new{
        title = title,
        -- One row of the two things a reader came to change, and a close under
        -- them, so the popup is two taps tall and does not scroll.
        buttons = {
            {
                {
                    text = _("Query means"),
                    icon = ICON_SEARCH,
                    callback = function()
                        UIManager:close(dialog)
                        self:chooseMode(settings)
                    end,
                },
                {
                    text = _("Index"),
                    icon = ICON_FOLDER,
                    callback = function()
                        UIManager:close(dialog)
                        UIManager:show(InfoMessage:new{ text = report.index })
                    end,
                },
            },
            {
                {
                    text = _("Close"),
                    icon = ICON_CLOSE,
                    callback = function()
                        UIManager:close(dialog)
                    end,
                },
            },
        },
    }
    UIManager:show(dialog)
end

--- Chooses what the query means.
---
--- A popup with a row per option rather than a dialog with a keyboard: three
--- choices and no typing, so the control is three buttons and the current one is
--- marked. Tapping one applies it and comes back here, which keeps the reader
--- looking at the same popup they already understood.
function Settings:chooseMode(settings)
    local current = settings:readSetting("search_mode")

    local function option(mode, text, help)
        return {
            text = text .. "  " .. help,
            text_font_bold = current == mode,
            callback = function()
                settings:saveSetting("search_mode", mode)
                settings:flush()
                self:show()
            end,
        }
    end

    local dialog
    dialog = ButtonDialog:new{
        title = _("What should the query mean?") .. "\n\n"
            .. _("Now") .. ": " .. mode_short(current),
        buttons = {
            {
                option("Text", _("Book text"),
                    _("phrases in quotes match exactly")),
            },
            {
                option("Metadata", _("Title, author, series"),
                    _("skip the book contents")),
            },
            {
                option("Pattern", _("Regular expression"),
                    _("over the terms in the index")),
            },
            {
                {
                    text = _("Close"),
                    callback = function()
                        UIManager:close(dialog)
                    end,
                },
            },
        },
    }
    UIManager:show(dialog)
end

return Settings
