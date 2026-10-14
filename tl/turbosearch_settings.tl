-- Settings: every configuration the engine acts on, and every environment
-- variable it reads.
--
-- One screen, one list, no wizard. The settings used to be a report screen and
-- then a configure screen behind a button, which was two hops to reach a text
-- field, and the two screens existed because half of what they showed was not
-- configurable at all. Now everything reachable is a row on the one screen, and
-- what cannot be changed is said so rather than hidden.
--
-- Two panes, because a setting has two homes:
--
--   `config` is a record in the database the engine reads back on open. File
--   selection and query behaviour live there, so a change is in force the next
--   time anything opens the index.
--
--   `env` is what the indexer was started with. A settings screen cannot reach
--   into a running process, so it records what the *next* run should use, in
--   `<index>.env` beside the index. A variable already set in the environment
--   wins over that file, so a one-off run from a shell still behaves — which is
--   why the pane is named for the run rather than for the value.
--
-- Every row is one item in a list rather than a bespoke widget per setting,
-- because there are a dozen of them in four kinds — text, list, number, choice —
-- and one editing path is easier to follow than four.

local ConfirmBox = require("ui/widget/confirmbox")
local Device = require("device")
local Font = require("ui/font")
local InfoMessage = require("ui/widget/infomessage")
local InputDialog = require("ui/widget/inputdialog")
local Menu = require("ui/widget/menu")
local RenderText = require("ui/rendertext")
local UIManager = require("ui/uimanager")
local WidgetContainer = require("ui/widget/container/widgetcontainer")
local ffiUtil = require("ffi/util")
local _ = require("gettext") as function(string): string
local T = ffiUtil.template

--- Icons, as text rather than images — see tl/plugin.tl for why.
local ICON_SETTINGS: string = ""
local ICON_CLOSE: string = ""

--- One settings row.
local record Setting
   --- The key as the engine spells it, also the line the file holds.
   key: string
   --- What it is, shown as the row's text.
   label: string
   --- What is in force, shown as the right-hand column.
   value: string
   --- How a new value is entered.
   edit: string
   choices: {string}
end

--- The box a single value is typed into, and the calls made through it.
---
--- The settings menu is a plain `any` because its rows are the caller's shape to
--- declare; this one is worth naming, because the value typed into it is read
--- back through two calls and a rename of either should be an error here.
local record ValueBox
   getInputText: function(ValueBox): string
   onShowKeyboard: function(ValueBox)
end

--- One row of the list, and the setting it was built from.
local record Row
   text: string
   mandatory: string
   setting: Setting
end

--- The plugin, as far as this screen is concerned.
local record SettingsHost
   --- The stored config as text, or nil plus a reason.
   engine_config_text: function(SettingsHost): string, string
   --- Applies `key = value` lines, leaving the rest alone.
   engine_apply_config: function(SettingsHost, string): boolean, string
   --- Reopens the query box, which is where the round trip ends.
   onTurbosearchSearch: function(SettingsHost)
end

--- The settings screen.
local record Settings
   new: function(any, any): Settings

   main: SettingsHost
   rows: {Setting}
   env_rows: {Setting}
   pane: string
   --- The index being configured, and the env file beside it.
   index_path: string
   env_file: string
   return_to_search: boolean

   reload: function(Settings)
   show: function(Settings)
   edit: function(Settings, Setting, string)
   save: function(Settings, string)
end

--- The marker a choice row carries when it is already in force.
---
--- A local rather than the translator called inline, because Teal reads a bare
--- `_(...)` as indexing a value that is not there.
local function mark_now(_choice: string): string
   return _("now")
end

--- Where the env file is: the index path, with `env` in place of its extension.
---
--- A method rather than a free function, because the path is the plugin's setting
--- and arrives with the screen. A caller that never set one has nowhere to put a
--- file and no run to read it, so the answer is empty rather than a made-up path.
function Settings:env_file_path(): string
   if self.index_path == nil then
      return ""
   end
   --- The settings file sits beside the index, so it is the index with `env` in
   --- place of its extension. A path with no name behind the extension leaves
   --- nothing to substitute, and the `.env` that followed was a *relative* path
   --- — written into whatever directory the reader happened to be in, which is
   --- how this project's own build-time `.env` came to be replaced by a
   --- settings record. Empty is the only honest answer there is nowhere else.
   local stem: string = (self.index_path:gsub("%.[^%.]+$", ""))
   if stem == "" then
      return ""
   end
   return stem .. ".env"
end

--- The face the right-hand column is drawn in, and the size it is drawn at.
---
--- Set on the menu as well as measured in, so the two cannot drift apart: a
--- column measured in one face and painted in another has only guessed, and a
--- guess about width is the one thing a layout cannot afford.
local MANDATORY_FONT_SIZE: integer = 16

--- How much of a row the value may take, as a share of the screen.
---
--- Two fifths leaves the label the rest, which fits "Folders to index" at the
--- font the menu draws it in. The share is of the *screen* rather than a number
--- of characters because a character is eight pixels on one device and twelve
--- on another, and only the pixels decide: forty-eight characters sounded small
--- and was in fact the whole width of the row, which is what squeezed the label
--- to zero and took the screen down with it.
local MANDATORY_SHARE: number = 0.4

--- The value as it fits: measured in pixels, shortened, and given an ellipsis.
---
--- Measured rather than sliced, so `exclude_extensions` — forty names long —
--- occupies exactly as much of the row as a short value would, and what is left
--- of it is what a reader can still see without opening the row. What is cut off
--- is not lost: tapping the row opens the whole value, which is where a setting
--- is read properly anyway.
local function column_of(value: string): string
   if value == "" then
      return ""
   end
   local face = Font:getFace("infont", MANDATORY_FONT_SIZE)
   local budget = math.floor(Device.screen:getWidth() * MANDATORY_SHARE)

   --- `truncateTextByWidth` appends its ellipsis whether or not it cut anything,
   --- so calling it alone marks every short value — `auto`, `full`, `5120` — as
   --- though something were being held back. A mark that is always there carries
   --- no information at all, which is the opposite of what it is for; measuring
   --- first is what makes it mean "there is more here than you can see".
   if RenderText:getSubTextByWidth(value, face, budget, true, false) == value then
      return value
   end
   return RenderText:truncateTextByWidth(value, face, budget, true, false)
end

--- Reads the `key = value` pairs out of `text`.
local function pairs_of(text: string): {string: string}
   local out: {string: string} = {}
   for line in (text .. "\n"):gmatch("(.-)\n") do
      local key, value = line:match("^([^=]+)=(.*)$")
      if key then
         out[key:match("^%s*(.-)%s*$")] = value:match("^%s*(.-)%s*$")
      end
   end
   return out
end

--- The config rows: everything the engine acts on, in the order a reader asks.
local function config_rows(text: string): {Setting}
   local by_key = pairs_of(text)

   local function row(key: string, label: string, choices: {string}): Setting
      return {
         key = key,
         label = label,
         value = by_key[key] or "",
         edit = choices and #choices > 0 and "choice" or "text",
         choices = choices or ({} as {string}),
      }
   end

   return {
      row("source_dirs", _("Folders to index"), {} as {string}),
      row("include_extensions", _("File types to index"), {} as {string}),
      row("exclude_extensions", _("File types to skip"), {} as {string}),
      row("exclude_dir_globs", _("Folders to skip"), {} as {string}),
      row("min_file_size", _("Smallest file (bytes)"), {} as {string}),
      row("max_file_size", _("Largest file (bytes)"), {} as {string}),
      row("index_depth", _("Positions kept per term"),
         { "full", "balanced", "light" } as {string}),
      row("prefix_mode", _("Expand a prefix query"),
         { "auto", "exact", "prefix" } as {string}),
      row("index_only_when_charging", _("Only index while charging"),
         { "true", "false" } as {string}),
      row("meta_only_unparseable", _("Index metadata for unreadable files"),
         { "true", "false" } as {string}),
      row("content_mode", _("What is stored"), {} as {string}),
   }
end

--- The env rows: what the next indexer run will use.
local function env_rows(path: string): {Setting}
   local handle = io.open(path, "r")
   local text: string = ""
   if handle then
      text = handle:read("a") or ""
      handle:close()
   end
   local by_key = pairs_of(text)

   local function row(key: string, label: string): Setting
      return {
         key = key,
         label = label,
         value = by_key[key] or "",
         edit = "text",
         choices = {} as {string},
      }
   end

   return {
      row("CONTENT_MODE", _("What to store: full, terms or files")),
      row("MAX_BOOKS", _("Index at most this many books")),
      row("SEARCH", _("Phrase to try once indexing is done")),
      row("META_SEARCH", _("Metadata to try once indexing is done")),
      row("FRESH", _("Discard the index and build it again")),
   }
end

--- Writes the env rows back.
---
--- The whole pane rather than the one row that changed, so a write cannot leave
--- the file and the screen holding different things.
local function write_env(path: string, rows: {Setting})
   if path == "" then
      --- No index to sit beside means nowhere to put the record, and anywhere
      --- else would be a settings file no run would ever read.
      return
   end
   local lines: {string} = {
      "# What the next indexer run reads. A variable set in the environment wins",
      "# over this file, so a one-off run from a shell still behaves.",
   }
   for _, setting in ipairs(rows) do
      lines[#lines + 1] = setting.key .. "=" .. setting.value
   end
   local handle = io.open(path, "w")
   if handle then
      handle:write(table.concat(lines, "\n") .. "\n")
      handle:close()
   end
end

setmetatable(Settings, { __index = WidgetContainer })

--- Reloads what is in force, then shows it.
---
--- Reloads rather than caching, because the engine may be holding a different
--- record than the file: a settings screen that shows what it last wrote is one
--- that can quietly lie about what is in force.
function Settings:reload()
   local host: SettingsHost = self.main
   local text: string
   local err: string
   text, err = host:engine_config_text()
   if not text then
      UIManager:show(ConfirmBox:new{
         title = _("Book search"),
         text = T(_("There is no index to read settings from.\n\n%1"), tostring(err)),
         icon = "texture-box",
         ok_text = _("Close"),
      } as any)
      return
   end
   self.rows = config_rows(text)
   self.env_file = self:env_file_path()
   self.env_rows = env_rows(self.env_file)
   self:show()
end

--- Shows the pane that is currently selected.
function Settings:show()
   local items: {any} = {}
   local title: string
   local what: string = self.pane

   if what == "env" then
      title = T(_("Next run — %1 settings"), _("indexer"))
      for _, setting in ipairs(self.env_rows) do
         items[#items + 1] = {
            text = setting.label,
            mandatory = column_of(setting.value),
            setting = setting,
         }
      end
   else
      title = _("Stored — what the engine acts on")
      for _, setting in ipairs(self.rows) do
         items[#items + 1] = {
            text = setting.label,
            mandatory = column_of(setting.value),
            setting = setting,
         }
      end
   end

   local pane = self.pane
   local dialog: any
   dialog = Menu:new{
      title = ICON_SETTINGS .. "  " .. _("Book search settings"),
      --- The pane's name, on one line: a menu measures its subtitle before it
      --- lays it out, and a line it cannot measure is a refused screen.
      subtitle = title,
      item_table = items,
      covers_full_width = true,
      --- The right column is drawn in this face, and it is set rather than left
      --- to the menu's own default so that the measurement above and the widget
      --- that finally paints it agree.
      items_mandatory_font_size = MANDATORY_FONT_SIZE,
      onMenuSelect = function(_menu, item: Row)
         self:edit(item.setting, pane)
      end,
      --- One button, because the only thing left to do on a settings screen is
      --- leave — and leaving is where the round trip ends.
      buttons = {
         {
            {
               text = ICON_CLOSE .. "  " .. _("Close"),
               callback = function()
                  UIManager:close(dialog)
                  if self.return_to_search then
                     (self.main as SettingsHost):onTurbosearchSearch()
                  end
               end,
            },
         },
      },
   } as any

   UIManager:show(dialog)
end

--- Edits one setting.
---
--- A choice is a list to pick from; anything else is a text field. The pane is
--- remembered so the save lands in the record the value was read from, which is
--- the difference between writing to the database and writing to the file.
function Settings:edit(setting: Setting, pane: string)
   if setting.edit == "choice" then
      local dialog: any
      local items: {any} = {}
      for _, choice in ipairs(setting.choices) do
         items[#items + 1] = {
            text = choice,
            mandatory = choice == setting.value and mark_now(choice) or "",
            callback = function()
               setting.value = choice
               UIManager:close(dialog)
               self:save(pane)
            end,
         }
      end
      dialog = Menu:new{
         title = setting.label,
         subtitle = T(_("Now") .. ": %1", setting.value),
         item_table = items,
         covers_full_width = true,
      } as any
      UIManager:show(dialog)
      return
   end

   local input: ValueBox
   input = InputDialog:new{
      title = setting.label,
      input = setting.value,
      description = setting.key,
      buttons = {
         {
            {
               text = _("Cancel"),
               callback = function()
                  UIManager:close(input)
               end,
            },
            {
               text = _("Save"),
               is_enter_default = true,
               callback = function()
                  setting.value = input:getInputText()
                  UIManager:close(input)
                  self:save(pane)
               end,
            },
         },
      },
   } as ValueBox
   UIManager:show(input)
   input:onShowKeyboard()
end

--- Writes the pane back, then shows it again with what is now in force.
function Settings:save(pane: string)
   if pane == "env" then
      write_env(self.env_file, self.env_rows)
      self.env_rows = env_rows(self.env_file)
      self:show()
      return
   end

   --- The config pane is applied as a partial change: the rows the engine does not
   --- let a reader set are left exactly as they were.
   local host: SettingsHost = self.main
   local lines: {string} = {}
   for _, setting in ipairs(self.rows) do
      lines[#lines + 1] = setting.key .. "=" .. setting.value
   end
   local ok: boolean
   local err: string
   ok, err = host:engine_apply_config(table.concat(lines, "\n"))
   if not ok then
      UIManager:show(InfoMessage:new{ text = tostring(err) } as any)
      return
   end
   self:reload()
end

return Settings
