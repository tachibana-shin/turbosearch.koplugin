-- turbosearch: typed Lua bindings for the turbosearch engine.
--
-- The engine is a C shared library loaded through LuaJIT's FFI. Every entry
-- point is declared here in one place, so the rest of the plugin works with
-- records and strings rather than raw pointers. See `note.md` section 5 for
-- why this is the delivery method: a KOReader plugin is Lua, and LuaJIT calls
-- C directly.
--
-- Build with `tl build` (see tlconfig.lua). The generated `.lua` is what
-- KOReader loads; this file is the source and is not shipped.
--
-- The `ffi` module is a LuaJIT builtin, so `tl check` on a plain Lua
-- interpreter reports `module not found: 'ffi'`. That is the interpreter missing
-- LuaJIT, not a defect in this file. Run `tl check` under `luajit` instead.

local ffi = require("ffi")

------------------------------------------------------------
-- The C ABI
------------------------------------------------------------

-- The handle is opaque: its size is not part of the interface, so the engine
-- can gain fields without breaking this binding.
ffi.cdef([[
typedef struct ts_index ts_index;

ts_index *ts_open(const char *db_path);
void ts_close(ts_index *handle);

const char *ts_version(void);
int ts_content_mode(const ts_index *handle);

char *ts_search(ts_index *handle, const char *query, int *out_count);
char *ts_search_meta(ts_index *handle, const char *query, int *out_count);
char *ts_search_regex(ts_index *handle, const char *query, int *out_count);
char *ts_book_title(ts_index *handle, int book_id);
char *ts_book_path(ts_index *handle, int book_id);

char *ts_alloc_buffer(size_t len);
void ts_free(char *ptr);
const char *ts_last_error(void);

long long ts_index_bytes(const ts_index *handle);
long long ts_term_count(const ts_index *handle);
]])

------------------------------------------------------------
-- Types
------------------------------------------------------------

--- One search result.
local record Hit
   doc_id: integer
   score: integer
end

--- How much content an index holds. Phrase queries only work on Full.
local enum Mode
   "Full"
   "TermsOnly"
   "FileList"
end

--- How a query is interpreted.
---
--- `Text` is a phrase when the query is quoted and a set of terms otherwise,
--- `Metadata` searches only title, author and series, and `Pattern` treats the
--- query as a regular expression over the terms in the index.
local enum SearchMode
   "Text"
   "Metadata"
   "Pattern"
end

--- Options for a search. All fields are optional.
local record SearchOptions
   -- Keep at most this many results, the way a reader wants one screen.
   limit: integer
   -- Restrict to a directory, for a search started inside a folder. Empty
   -- means the whole library.
   scope: string
end

--- An open index.
local record Handle
   ptr: any
end

--- True once `load_lib` has run.
local loaded: boolean = false
local C = ffi.C

------------------------------------------------------------
-- Error codes, mirroring src/ffi.rs
------------------------------------------------------------

local TS_OK: integer = 0
local TS_ERR_ARGUMENT: integer = -1
local TS_ERR_OPEN: integer = -2
local TS_ERR_QUERY: integer = -3
local TS_ERR_INTERNAL: integer = -4

local MODE_FULL: integer = 0
local MODE_TERMS_ONLY: integer = -1
local MODE_FILE_LIST: integer = 2

------------------------------------------------------------
-- Loading
------------------------------------------------------------

--- Loads the engine from an explicit path. Raises on failure.
---
---  is rebound here rather than left as : the C namespace that LuaJIT
--- searches for a symbol by default is the executable and its dependencies, not
--- a library loaded at runtime, so  is undefined even after a
--- successful . Rebinding to the loaded handle is what makes the rest
--- of this file resolve.
---
--- Deliberately does not swallow the error: `ffi.load` already produces a
--- message naming the library and the reason, and a plugin that cannot load
--- the engine has nothing useful left to do. The alternative shape — returning
--- `(boolean, string)` — needs `pcall`, whose polymorphic multi-value return
--- Teal cannot type, which would cost more than it gives.
local function load_lib(path: string)
   C = ffi.load(path)
   loaded = true
end

--- True once a library has been loaded.
local function is_loaded(): boolean
   return loaded
end

--- The version string of the loaded engine, or nil.
local function version(): string
   if not loaded then
      return nil
   end
   local s: any = C.ts_version()
   if s == ffi.NULL then
      return nil
   end
   return ffi.string(s)
end

------------------------------------------------------------
-- Handle lifecycle
------------------------------------------------------------

--- The last error the engine reported, as a Lua string.
local function last_error(): string
   if not loaded then
      return "engine not loaded"
   end
   local s: any = C.ts_last_error()
   if s == ffi.NULL then
      return ""
   end
   return ffi.string(s)
end

--- Opens an index at `path`.
local function open_index(path: string): Handle
   if not loaded then
      error("engine not loaded; call load_lib() first")
   end
   local ptr: any = C.ts_open(path)
   if ptr == ffi.NULL then
      error(last_error())
   end
   return { ptr = ptr }
end

--- Closes a handle. Idempotent, so a teardown hook may run twice.
local function close(handle: Handle)
   if handle.ptr == nil then
      return
   end
   C.ts_close(handle.ptr)
   handle.ptr = nil
end

--- How much content this index holds.
local function content_mode(handle: Handle): Mode
   local m = C.ts_content_mode(handle.ptr)
   if m == MODE_FULL then
      return "Full"
   end
   if m == MODE_TERMS_ONLY then
      return "TermsOnly"
   end
   return "FileList"
end

------------------------------------------------------------
-- Search
------------------------------------------------------------

--- Reads a results buffer into lines, then frees it.
---
--- The order matters and is the whole point of the helper: the engine's buffer
--- is freed by the caller, so reading after freeing is a use-after-free rather
--- than a crash, and it would show up as a misspelled title instead of a
--- diagnosable error.
local function drain(buf: any): {string}
   local text: string = ffi.string(buf)
   C.ts_free(buf)
   local lines: {string} = {}
   for line in text:gmatch("[^\n]+") do
      lines[#lines + 1] = line
   end
   return lines
end

--- Searches book content. Raises on failure.
---
--- Bytes is what it occupies on disk with its log, terms is how many distinct
--- terms it holds, and mode is how much content it stored. On the measured
--- library those three decide whether a reader is looking at a 0.9 MB file list
--- or a 163 MB phrase index, so the reader can be told before building one.
local record IndexInfo
   bytes: integer
   terms: integer
   mode: Mode
end


--- Reads what the index says about itself.
local function index_info(handle: Handle): IndexInfo
   return {
      -- The engine answers in 64-bit integers, which LuaJIT hands back as
      -- int64 cdata. Leaving them alone makes the numbers print with an `LL`
      -- suffix the moment anything formats them, so they cross as Lua numbers
      -- here, at the boundary, rather than at every call site.
      bytes = tonumber(C.ts_index_bytes(handle.ptr)) as integer,
      terms = tonumber(C.ts_term_count(handle.ptr)) as integer,
      mode = content_mode(handle),
   }
end

--- Searches with a regular expression over the terms in the index.
---
--- A pattern matching too many terms is refused by the engine rather than
--- answered, because the list would be long enough that nobody reads it and the
--- reader would have waited through the whole thing.
local function search_pattern(handle: Handle, query: string, opts?: SearchOptions): {Hit}
   local o: SearchOptions = opts or {} as SearchOptions
   local indirect: any = ffi.new("int[1]")
   local buf: any = C.ts_search_regex(handle.ptr, query, indirect)
   if buf == ffi.NULL then
      error(last_error())
   end
   local lines: {string} = drain(buf)
   local result: {Hit} = {}
   for _, line in ipairs(lines) do
      local id, score = line:match("^(%d+)%s+(%d+)$")
      if id and score then
         result[#result + 1] = { doc_id = tonumber(id) as integer, score = tonumber(score) as integer }
      end
   end
   if o.limit and o.limit > 0 and #result > o.limit then
      return table.move(result, 1, o.limit, 1, {} as {Hit})
   end
   return result
end

local function search(handle: Handle, query: string, opts?: SearchOptions): {Hit}
   local o: SearchOptions = opts or {} as SearchOptions
   local indirect: any = ffi.new("int[1]")
   local buf: any = C.ts_search(handle.ptr, query, indirect)
   if buf == ffi.NULL then
      error(last_error())
   end
   local lines: {string} = drain(buf)

   local result: {Hit} = {}
   for _, line in ipairs(lines) do
      local id, score = line:match("^(%d+)%s+(%d+)$")
      if id and score then
         result[#result + 1] = { doc_id = tonumber(id) as integer, score = tonumber(score) as integer }
      end
   end
   if o.limit and o.limit > 0 and #result > o.limit then
      return table.move(result, 1, o.limit, 1, {} as {Hit})
   end
   return result
end

--- Searches metadata: title, author, series and description.
---
--- The engine writes `"<doc_id>\t<title>"` lines, so the parser differs from
--- `search`; the shared `drain` is what keeps the buffer handling in one place.
local function search_meta(handle: Handle, query: string, opts?: SearchOptions): {Hit}
   local o: SearchOptions = opts or {} as SearchOptions
   local indirect: any = ffi.new("int[1]")
   local buf: any = C.ts_search_meta(handle.ptr, query, indirect)
   if buf == ffi.NULL then
      error(last_error())
   end
   local lines: {string} = drain(buf)

   local result: {Hit} = {}
   for _, line in ipairs(lines) do
      local id = line:match("^(%d+)")
      if id then
         result[#result + 1] = { doc_id = tonumber(id) as integer, score = 0 as integer }
      end
   end
   if o.limit and o.limit > 0 and #result > o.limit then
      return table.move(result, 1, o.limit, 1, {} as {Hit})
   end
   return result
end

--- A book's title, for showing next to a result.
local function book_title(handle: Handle, doc_id: integer): string
   local s: any = C.ts_book_title(handle.ptr, doc_id)
   if s == ffi.NULL then
      return ""
   end
   return ffi.string(s)
end

--- A book's file path, for opening it.
local function book_path(handle: Handle, doc_id: integer): string
   local s: any = C.ts_book_path(handle.ptr, doc_id)
   if s == ffi.NULL then
      return ""
   end
   return ffi.string(s)
end

--- Searches with whichever interpretation the mode asks for.
---
--- The dispatch lives here rather than in the caller so that a caller cannot
--- pass `Pattern` to the phrase path and quietly get a phrase: the three are
--- different index operations and they are not interchangeable.
local function search_as(handle: Handle, mode: SearchMode, query: string, opts?: SearchOptions): {Hit}
   if mode == "Metadata" then
      return search_meta(handle, query, opts)
   end
   if mode == "Pattern" then
      return search_pattern(handle, query, opts)
   end
   return search(handle, query, opts)
end

return {
   Hit = Hit,
   Mode = Mode,
   SearchOptions = SearchOptions,
   Handle = Handle,

   load_lib = load_lib,
   is_loaded = is_loaded,
   version = version,
   open_index = open_index,
   close = close,
   last_error = last_error,
   content_mode = content_mode,

   search = search,
   search_meta = search_meta,
   search_as = search_as,
   search_pattern = search_pattern,
   index_info = index_info,
   book_title = book_title,
   book_path = book_path,

   TS_OK = TS_OK,
   TS_ERR_ARGUMENT = TS_ERR_ARGUMENT,
   TS_ERR_OPEN = TS_ERR_OPEN,
   TS_ERR_QUERY = TS_ERR_QUERY,
   TS_ERR_INTERNAL = TS_ERR_INTERNAL,

   MODE_FULL = MODE_FULL,
   MODE_TERMS_ONLY = MODE_TERMS_ONLY,
   MODE_FILE_LIST = MODE_FILE_LIST,
}
