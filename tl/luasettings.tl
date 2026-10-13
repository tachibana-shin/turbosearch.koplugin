-- LuaSettings: the settings file, and the calls made on it.
---
--- `open` is a plain function despite being called with a colon, so it takes the
--- module as its first argument. Its result is `any` because the file's shape is
--- the caller's to declare: a module cannot lend its types to the module that
--- requires it, so the caller casts into the shape it wants.
local record LuaSettingsModule
   open: function(any, string): any
end

return {
   open = function(_self: any, _path: string): any return nil end,
} as LuaSettingsModule
