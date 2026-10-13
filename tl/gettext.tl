-- gettext: the module is a callable tag, so it is described as one.
---
--- Typed as a function rather than a record because that is what `_("...")` is,
--- and a module with a `__call` metamethod cannot be expressed as a type. The
--- cast is at the require, which is the one place the shape is known, and it
--- leaves the generated Lua as a plain `require("gettext")`.
return {
   mark = function(msgid: string): string return msgid end,
   pgettext = function(_context: string, msgid: string): string return msgid end,
} as any
