-- LuaJIT's foreign-function interface, described well enough for `tl check` to
-- be worth running.
--
-- `require("ffi")` only resolves under LuaJIT, so checking on a plain Lua
-- interpreter reports `module not found` and then fails on every use of the
-- module — a check that cannot fail for a real reason. This file is what Teal
-- binds to instead: the small slice of the interface the engine uses, laid out
-- so a rename in src/ffi.rs that this file declares shows up as a type error
-- rather than at run time in a reader.
--
-- Everything is `nil` or returns `nil` on purpose: this is a type declaration,
-- not an implementation, and the bodies never run under LuaJIT. The cast at the
-- bottom is what pins the field types, which is the whole value of the file.

local record CLibrary
   ts_version: function(): any
   ts_open: function(any): any
   ts_close: function(any)
   ts_content_mode: function(any): integer
   ts_search: function(any, string, any): any
   ts_search_meta: function(any, string, any): any
   ts_search_regex: function(any, string, any): any
   ts_book_title: function(any, integer): any
   ts_book_path: function(any, integer): any
   ts_alloc_buffer: function(integer): any
   ts_free: function(any)
   ts_last_error: function(): any
   -- Bytes of everything the index occupies on disk, log included.
   ts_index_bytes: function(any): integer
   -- The whole config, as key = value lines — every setting the engine acts on,
   -- including the ones a build chooses rather than a query.
   ts_config_text: function(any): any
   -- Applies settings lines, leaving the ones absent alone.
   ts_set_config_text: function(any, string): integer
   -- Distinct terms, which is what separates the three content modes in
   -- practice more tellingly than the size does.
   ts_term_count: function(any): integer
end

local record FfiModule
   -- The C library namespace. Rebound to the loaded engine at run time.
   C: CLibrary
   -- A null pointer. Typed as `any` so it can be compared against whatever a
   -- call happens to return.
   NULL: any
   cdef: function(string)
   -- `CLibrary` rather than `any` because the handle is what the rest of this
   -- file calls through, and letting the assignment go through a looser type
   -- would need a cast at the one place that matters.
   load: function(string): CLibrary
   string: function(any): string
   new: function(string): any
   typeof: function(string): any
end

return {
   C = nil,
   NULL = nil,
   cdef = function(_source: string) end,
   load = function(_path: string): CLibrary return nil end,
   string = function(_data: any): string return "" end,
   new = function(_ct: string): any return nil end,
   typeof = function(_ct: string): any return nil end,
} as FfiModule
