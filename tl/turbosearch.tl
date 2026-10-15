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

/* `scope` restricts a search to one directory; an empty one is everything
   that has been indexed. */
char *ts_search(ts_index *handle, const char *query, const char *scope, int *out_count);
char *ts_search_meta(ts_index *handle, const char *query, const char *scope, int *out_count);
char *ts_search_regex(ts_index *handle, const char *query, const char *scope, int *out_count);

/* The sentence a hit is in, cut from the book. Null with no error means there is
   none to show; null with one means the book could not be read. */
char *ts_snippet(ts_index *handle, int doc_id, int entry_idx, int position, int radius);

/* Building, in as many steps as the caller wants to give back control for. */
int ts_index_begin(ts_index *handle, const char *dir);
int ts_index_step(ts_index *handle, int budget);
char *ts_index_end(ts_index *handle);
char *ts_book_title(ts_index *handle, int book_id);
char *ts_book_path(ts_index *handle, int book_id);

char *ts_alloc_buffer(size_t len);
void ts_free(char *ptr);
const char *ts_last_error(void);

long long ts_index_bytes(const ts_index *handle);
long long ts_term_count(const ts_index *handle);

/* The stored settings, and applying a partial change to them. */
char *ts_config_text(ts_index *handle);
int ts_set_config_text(ts_index *handle, const char *text);
]])

------------------------------------------------------------
-- Types
------------------------------------------------------------

--- One search result.
local record Hit
   doc_id: integer
   score: integer
   --- Which section of the book the match is in.
   entry_idx: integer
   --- Where in that section it starts, counted in tokens from its first one.
   ---
   --- These two are what a preview of the sentence needs, and they come back
   --- with the result rather than being looked up again: a second search to find
   --- where the first one matched would be a second decode, and a decode is what
   --- this is trying to spend once.
   position: integer
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
local function load_lib(path: string): any
   C = ffi.load(path)
   loaded = true
   --- The handle is handed back so a caller can tell a loaded engine from a
   --- refused one; pcall on a function that returns nothing has one value to
   --- give, which is what made the caller unable to tell the two apart.
   return C
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
local function open_index(path: string): Handle, string
   if not loaded then
      return nil, "engine not loaded; call load_lib() first"
   end
   local ptr: any = C.ts_open(path)
   if ptr == ffi.NULL then
      return nil, last_error()
   end
   return { ptr = ptr }, ""
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
local function search_pattern(handle: Handle, query: string, opts?: SearchOptions): {Hit}, string
   local o: SearchOptions = opts or {} as SearchOptions
   local indirect: any = ffi.new("int[1]")
   -- A scope of "" is the whole index, which is what the engine answers from
   -- when nothing narrows it. Passing it down rather than filtering here is what
   -- keeps the two paths — "only this folder" and "everywhere" — the same query.
   local buf: any = C.ts_search_regex(handle.ptr, query, o.scope or "", indirect)
   if buf == ffi.NULL then
      return {} as {Hit}, last_error()
   end
   local lines: {string} = drain(buf)
   local result: {Hit} = {}
   for _, line in ipairs(lines) do
      local id, score, entry, position = line:match("^(%d+)%s+(%d+)%s+(%d+)%s+(%d+)$")
      if id then
         result[#result + 1] = {
            doc_id = tonumber(id) as integer,
            score = tonumber(score) as integer,
            entry_idx = tonumber(entry) as integer,
            position = tonumber(position) as integer,
         }
      end
   end
   if o.limit and o.limit > 0 and #result > o.limit then
      return table.move(result, 1, o.limit, 1, {} as {Hit})
   end
   return result
end

local function search(handle: Handle, query: string, opts?: SearchOptions): {Hit}, string
   local o: SearchOptions = opts or {} as SearchOptions
   local indirect: any = ffi.new("int[1]")
   -- A scope of "" is the whole index, which is what the engine answers from
   -- when nothing narrows it. Passing it down rather than filtering here is what
   -- keeps the two paths — "only this folder" and "everywhere" — the same query.
   local buf: any = C.ts_search(handle.ptr, query, o.scope or "", indirect)
   if buf == ffi.NULL then
      return {} as {Hit}, last_error()
   end
   local lines: {string} = drain(buf)

   local result: {Hit} = {}
   for _, line in ipairs(lines) do
      local id, score, entry, position = line:match("^(%d+)%s+(%d+)%s+(%d+)%s+(%d+)$")
      if id then
         result[#result + 1] = {
            doc_id = tonumber(id) as integer,
            score = tonumber(score) as integer,
            entry_idx = tonumber(entry) as integer,
            position = tonumber(position) as integer,
         }
      end
   end
   if o.limit and o.limit > 0 and #result > o.limit then
      return table.move(result, 1, o.limit, 1, {} as {Hit})
   end
   return result, ""
end

--- Searches metadata: title, author, series and description.
---
--- The engine writes `"<doc_id>\t<title>"` lines, so the parser differs from
--- `search`; the shared `drain` is what keeps the buffer handling in one place.
local function search_meta(handle: Handle, query: string, opts?: SearchOptions): {Hit}, string
   local o: SearchOptions = opts or {} as SearchOptions
   local indirect: any = ffi.new("int[1]")
   -- A scope of "" is the whole index, which is what the engine answers from
   -- when nothing narrows it. Passing it down rather than filtering here is what
   -- keeps the two paths — "only this folder" and "everywhere" — the same query.
   local buf: any = C.ts_search_meta(handle.ptr, query, o.scope or "", indirect)
   if buf == ffi.NULL then
      return {} as {Hit}, last_error()
   end
   local lines: {string} = drain(buf)

   local result: {Hit} = {}
   for _, line in ipairs(lines) do
      local id = line:match("^(%d+)")
      if id then
         -- No section and no position: a metadata hit is a match on the cover and
         --- the title page, not in the text, so there is no sentence to show and
         --- a row like this opens the book rather than previewing it.
         result[#result + 1] = {
            doc_id = tonumber(id) as integer,
            score = 0 as integer,
            entry_idx = 0 as integer,
            position = 0 as integer,
         }
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
local function search_as(handle: Handle, mode: SearchMode, query: string, opts?: SearchOptions): {Hit}, string
   if mode == "Metadata" then
      return search_meta(handle, query, opts)
   end
   if mode == "Pattern" then
      return search_pattern(handle, query, opts)
   end
   return search(handle, query, opts)
end

--- Indexes what `dir` still needs, and says how many files that is.
---
--- The scan that answers costs one `stat` per file and writes nothing, which is
--- what lets a reader be told what the next few minutes will cost before a
--- single book is opened. An empty `dir` is every configured root — the whole
--- library, and the slow path.
---
--- Returns the count, or -1 plus the engine's reason.
--- The sentence a hit is in, taken out of the book the hit is in.
---
--- Returns an empty string when there is nothing to show, which is not a
--- failure: a book whose sections moved since it was indexed has no sentence to
--- point at, and the caller is expected to show the book instead of an excuse.
--- It costs a decode of one book, so it is asked for when the reader asks and
--- never ahead of time.
local function snippet(handle: Handle, doc_id: integer, entry_idx: integer,
                     position: integer, radius?: integer): string, string
   local take: integer = radius or 80
   local buf: any = C.ts_snippet(handle.ptr, doc_id, entry_idx, position, take)
   if buf == ffi.NULL then
      return "", last_error()
   end
   --- Read before freeing, for the same reason as every other buffer here.
   local text: string = ffi.string(buf)
   C.ts_free(buf)
   return text, ""
end

local function index_begin(handle: Handle, dir: string): integer, string
   local left: integer = C.ts_index_begin(handle.ptr, dir)
   if left < 0 then
      return -1, last_error()
   end
   return left, ""
end

--- Reads and indexes at most `budget` of the files a session has left, and says
--- what is still to do.
---
--- Stepped rather than done in one call because a screen cannot be redrawn from
--- inside a call that never returns: the reader hands control back between
--- batches, and the engine gets to be interrupted by somebody closing the book.
---
--- Returns the number still left, or -1 plus the engine's reason.
local function index_step(handle: Handle, budget: integer): integer, string
   local left: integer = C.ts_index_step(handle.ptr, budget)
   if left < 0 then
      return -1, last_error()
   end
   return left, ""
end

--- Ends a session: the metadata it held is written, and the handle's dictionary
--- is reloaded so the next search sees what was just indexed.
---
--- Returns what the session did as `key=value` lines, or nil plus the reason.
local function index_end(handle: Handle): string, string
   local buf: any = C.ts_index_end(handle.ptr)
   if buf == ffi.NULL then
      return nil, last_error()
   end
   local text: string = ffi.string(buf)
   C.ts_free(buf)
   return text, ""
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
   snippet = snippet,
   index_begin = index_begin,
   index_step = index_step,
   index_end = index_end,
   --- The whole config, as `key = value` lines.
   ---
   --- One handle, one read, closed by the caller — the same borrow a search uses.
   --- Returns the text, or nil plus the engine's reason.
   config_text = function(handle: Handle): string, string
      local buf: any = C.ts_config_text(handle.ptr)
      if buf == ffi.NULL then
         return nil, last_error()
      end
      --- Read before freeing, which is the whole point of the helper: the engine's
      --- buffer is freed by the caller, so the order is a use-after-free otherwise,
      --- and it would read as a misspelled setting rather than a crash.
      local text: string = ffi.string(buf)
      C.ts_free(buf)
      return text, ""
   end,
   --- Applies settings lines, leaving the rest alone.
   ---
   --- True, or false plus the reason: the engine refuses a line it cannot apply,
   --- and the caller is the one that can say what that means.
   apply_config = function(handle: Handle, text: string): boolean, string
      local code: integer = C.ts_set_config_text(handle.ptr, text)
      if code == 0 then
         return true, ""
      end
      return false, last_error()
   end,
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
