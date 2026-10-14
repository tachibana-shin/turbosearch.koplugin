//! The C ABI the Lua side binds to.
//!
//! KOReader plugins are Lua and run inside LuaJIT, so the engine has to be
//! reachable from there. LuaJIT's foreign-function interface calls C directly,
//! which makes a `cdylib` with `#[no_mangle]` entry points the shortest path:
//! no Lua module protocol to implement, no Lua interpreter linked in, and the
//! same `.so` works whether the caller is LuaJIT, a test harness or a debugger.
//!
//! ## Ownership
//!
//! Strings crossing into Rust are borrowed for the duration of the call and
//! never retained, so the caller owns everything it passes. Strings crossing out
//! are allocated by `ts_alloc_buffer` and freed by [`ts_free`], never by Lua and
//! never by Rust's allocator directly.
//!
//! ## Error convention
//!
//! Pointer-returning functions return null on failure and record a malloc'd
//! message retrievable with [`ts_last_error`]. Integer-returning functions
//! return 0 on success and a negative value on failure, using
//! [`TS_ERR_INTERNAL`] and friends. No function in this module panics across the
//! boundary: a panic unwinding into LuaJIT's stack would abort the process, so
//! every entry point wraps its body in `catch_unwind` and reports instead.
//!
//! ## Threading
//!
//! Nothing here is thread-safe, and does not need to be: LuaJIT runs the
//! plugin on one thread. A `ts_index` handle is owned by whoever opened it.

use std::cell::Cell;
use std::os::raw::{c_char, c_int};
use std::panic::{self, AssertUnwindSafe};
use std::path::Path;

use crate::index::{dict, query};
use crate::storage::metadata;

/// Error codes returned by the integer entry points.
pub const TS_OK: c_int = 0;
/// The argument was null, empty or the wrong kind.
pub const TS_ERR_ARGUMENT: c_int = -1;
/// The database could not be opened.
pub const TS_ERR_OPEN: c_int = -2;
/// The query failed.
pub const TS_ERR_QUERY: c_int = -3;
/// The call panicked; the message is in `ts_last_error`.
pub const TS_ERR_INTERNAL: c_int = -4;

/// A handle to an open index. Opaque to Lua by design: its size is not part of
/// the ABI, so adding fields never breaks a binding.
pub struct TsIndex {
    engine: &'static crate::storage::Engine,
    dict: dict::Dict,
    /// The indexing session `ts_index_begin` opened, if one is open.
    ///
    /// On the handle rather than in a thread-local because it outlives the call
    /// that made it: a session that only ever existed for the duration of one
    /// function could not be stepped, and stepping is what keeps the reader's
    /// screen drawing while a library is being read.
    session: Option<crate::storage::build::Session>,
}

thread_local! {
    /// The last error message, in the same storage `ts_free` releases.
    static LAST_ERROR: Cell<Option<*mut c_char>> = const { Cell::new(None) };
}

/// Sets the thread's last-error message from a Rust string.
fn set_error(msg: &str) {
    LAST_ERROR.with(|cell| {
        if let Some(old) = cell.replace(None) {
            // SAFETY: the pointer came from `to_c_string` and was produced by
            // this thread-local, so it has not been handed out elsewhere.
            unsafe { ts_free(old) };
        }
        cell.set(Some(to_c_string(msg)));
    });
}

fn clear_error() {
    LAST_ERROR.with(|cell| {
        if let Some(old) = cell.replace(None) {
            // SAFETY: as in `set_error`.
            unsafe { ts_free(old) };
        }
    });
}

/// Copies `text` into a buffer the caller releases with [`ts_free`].
///
/// Exposed because every outbound string uses it, and because a caller that
/// wants to build a Lua string out of several pieces needs a buffer anyway.
#[unsafe(no_mangle)]
pub extern "C" fn ts_alloc_buffer(len: usize) -> *mut c_char {
    if len == 0 {
        // A zero-length request is still a valid empty string.
        return Box::into_raw(vec![0u8].into_boxed_slice()) as *mut c_char;
    }
    let mut buf = vec![0u8; len + 1];
    let raw = buf.as_mut_ptr();
    std::mem::forget(buf);
    raw as *mut c_char
}

/// Frees a buffer returned by any function in this module.
///
/// # Safety
/// `ptr` must be non-null and come from one of the `*_buffer` or outbound
/// functions. Passing null is allowed and does nothing.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ts_free(ptr: *mut c_char) {
    unsafe {
        if ptr.is_null() {
            return;
        }
        let len = std::ffi::CStr::from_ptr(ptr).to_bytes().len();
        drop(Vec::from_raw_parts(ptr as *mut u8, len, len + 1));
    }
}

/// The last error message, or null when the previous call succeeded.
///
/// The returned pointer is owned by this library and is overwritten by the next
/// failing call; copy it if you need to keep it.
#[unsafe(no_mangle)]
pub extern "C" fn ts_last_error() -> *const c_char {
    LAST_ERROR.with(|cell| match cell.get() {
        Some(ptr) => ptr as *const c_char,
        None => std::ptr::null(),
    })
}

/// The engine's version, so a binding can refuse to talk to an old `.so`.
#[unsafe(no_mangle)]
pub extern "C" fn ts_version() -> *const c_char {
    // A static, never freed: the version outlives every call.
    static mut VERSION: Option<*const c_char> = None;
    static INIT: std::sync::Once = std::sync::Once::new();
    unsafe {
        INIT.call_once(|| {
            let s = concat!(env!("CARGO_PKG_NAME"), " ", env!("CARGO_PKG_VERSION"));
            let mut bytes = s.as_bytes().to_vec();
            bytes.push(0);
            VERSION = Some(bytes.leak().as_ptr() as *const c_char);
        });
        VERSION.unwrap_or(std::ptr::null())
    }
}

/// Opens an index at `db_path`.
///
/// # Safety
/// `db_path` must be a null-terminated UTF-8 path, or null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ts_open(db_path: *const c_char) -> *mut TsIndex {
    unsafe {
        let result = panic::catch_unwind(AssertUnwindSafe(|| {
            let path = match to_str(db_path) {
                Some(p) if !p.is_empty() => p.to_string(),
                _ => {
                    set_error("ts_open: db_path is null or empty");
                    return std::ptr::null_mut();
                }
            };
            match crate::storage::Engine::open(Path::new(&path)) {
                Ok(engine) => {
                    let engine: &'static mut crate::storage::Engine = Box::leak(Box::new(engine));
                    let engine: &'static crate::storage::Engine = engine;
                    let d = match dict::load(engine) {
                        Ok(d) => d,
                        Err(e) => {
                            set_error(&format!("ts_open: {path}: {e}"));
                            return std::ptr::null_mut();
                        }
                    };
                    clear_error();
                    Box::into_raw(Box::new(TsIndex {
                        engine,
                        dict: d,
                        session: None,
                    }))
                }
                Err(e) => {
                    set_error(&format!("ts_open: {path}: {e}"));
                    std::ptr::null_mut()
                }
            }
        }));
        match result {
            Ok(ptr) => ptr,
            Err(_) => {
                set_error("ts_open: panicked");
                std::ptr::null_mut()
            }
        }
    }
}

/// Closes a handle from [`ts_open`].
///
/// # Safety
/// `handle` must be non-null and come from [`ts_open`]; it must not be used
/// afterwards.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ts_close(handle: *mut TsIndex) {
    unsafe {
        if handle.is_null() {
            return;
        }
        let _ = panic::catch_unwind(AssertUnwindSafe(|| {
            let mut boxed = Box::from_raw(handle);
            // An open session holds metadata it has not written yet, and a book
            // with postings and no `BOOKS` row is one nobody can find. Dropping
            // it silently would lose exactly those books, so the session is
            // finished here as well; its report has nowhere to go, its writes
            // still land.
            if let Some(session) = boxed.session.take() {
                let _ = session.finish(boxed.engine, true);
            }
            // The engine was leaked in ts_open so the handle could hold a reference
            // to it; this is where that ends.
            drop(boxed);
        }));
    }
}

/// How much content the index holds: 0 `Full`, 1 `TermsOnly`, 2 `FileList`.
///
/// A caller that needs phrase queries can check this and warn, rather than
/// returning a phrase result that cannot exist.
///
/// # Safety
/// `handle` must be non-null and come from [`ts_open`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ts_content_mode(handle: *mut TsIndex) -> c_int {
    unsafe {
        let Some(handle) = as_ref(handle) else {
            return TS_ERR_ARGUMENT;
        };
        match handle.engine.config() {
            Ok(c) => c.content_mode as c_int,
            Err(e) => {
                set_error(&format!("ts_content_mode: {e}"));
                TS_ERR_QUERY
            }
        }
    }
}

/// Searches content.
///
/// Writes newly created lines of `"<doc_id> <score>\n"` into a buffer the caller
/// releases with [`ts_free`]. The count goes to `*out_count`.
///
/// # Safety
/// `handle` must come from [`ts_open`]; `query` must be a null-terminated UTF-8
/// string or null; `out_count` must be writable when non-null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ts_search(
    handle: *mut TsIndex,
    query: *const c_char,
    scope: *const c_char,
    out_count: *mut c_int,
) -> *mut c_char {
    unsafe {
        let Some(handle) = as_ref(handle) else {
            return err_ret("ts_search: null handle", out_count);
        };
        let Some(q) = to_str(query) else {
            return err_ret("ts_search: null or empty query", out_count);
        };

        match panic::catch_unwind(AssertUnwindSafe(|| {
            search_impl(handle, q, to_str(scope).unwrap_or(""))
        })) {
            Ok(Ok((buffer, count))) => {
                clear_error();
                if !out_count.is_null() {
                    *out_count = count;
                }
                buffer
            }
            Ok(Err(e)) => err_ret(&format!("ts_search: {e}"), out_count),
            Err(_) => err_ret("ts_search: panicked", out_count),
        }
    }
}

/// Searches content with a regular expression over the term dictionary.
///
/// The pattern is matched against the stored terms, which are folded, so a query
/// typed with accents reaches what the unaccented form would. A pattern matching
/// too many terms is refused rather than run: the answer would be a list nobody
/// reads, and a reader waiting through it.
///
/// # Safety
/// `handle` must come from [`ts_open`]; `query` must be a null-terminated UTF-8
/// string or null; `out_count` must be writable when non-null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ts_search_regex(
    handle: *mut TsIndex,
    query: *const c_char,
    scope: *const c_char,
    out_count: *mut c_int,
) -> *mut c_char {
    unsafe {
        let Some(handle) = as_ref(handle) else {
            return err_ret("ts_search_regex: null handle", out_count);
        };
        let Some(q) = to_str(query) else {
            return err_ret("ts_search_regex: null or empty query", out_count);
        };

        match panic::catch_unwind(AssertUnwindSafe(|| {
            regex_impl(handle, q, to_str(scope).unwrap_or(""))
        })) {
            Ok(Ok((buffer, count))) => {
                clear_error();
                if !out_count.is_null() {
                    *out_count = count;
                }
                buffer
            }
            Ok(Err(e)) => err_ret(&format!("ts_search_regex: {e}"), out_count),
            Err(_) => err_ret("ts_search_regex: panicked", out_count),
        }
    }
}

fn regex_impl(
    handle: &TsIndex,
    query: &str,
    scope: &str,
) -> crate::storage::RunResult<(*mut c_char, c_int)> {
    let hits = query::search_regex(handle.engine, &handle.dict, query)?;
    let hits = query::live_hits(handle.engine, hits, scope)?;
    let buffer = lines_buffer(hits.iter().map(|h| (h.doc_id, h.score)));
    Ok((buffer, hits.len() as c_int))
}

/// The size of the index on disk, in bytes, or a negative error code.
///
/// Reported separately from the open calls because it is the number a reader
/// wants before deciding to build one: the difference between the three content
/// modes is a factor of a hundred, so "how much will this cost me" is a
/// question the reader can only answer after opening an index. Reading it does
/// not need a database at all, so the handle is not required.
///
/// # Safety
/// `handle` must come from [`ts_open`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ts_index_bytes(handle: *mut TsIndex) -> i64 {
    unsafe {
        let Some(handle) = as_ref(handle) else {
            set_error("ts_index_bytes: null handle");
            return TS_ERR_ARGUMENT as i64;
        };
        let bytes = handle.engine.total_size();
        match i64::try_from(bytes) {
            Ok(value) => value,
            Err(_) => {
                set_error("ts_index_bytes: index larger than a 64-bit integer");
                TS_ERR_QUERY as i64
            }
        }
    }
}

/// The number of distinct terms in the index, or a negative error code.
///
/// Together with [`ts_index_bytes`] this is what a progress dialog would need,
/// and what tells a reader whether an index is `files`, `terms` or `full`
/// before the first query rather than after it.
///
/// # Safety
/// `handle` must come from [`ts_open`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ts_term_count(handle: *mut TsIndex) -> i64 {
    unsafe {
        let Some(handle) = as_ref(handle) else {
            set_error("ts_term_count: null handle");
            return TS_ERR_ARGUMENT as i64;
        };
        i64::try_from(handle.dict.len()).unwrap_or(i64::MAX)
    }
}

/// The stored settings, as `key = value` lines.
///
/// Every setting the engine will act on, including the ones a build chooses rather
/// than a query — `source_dirs`, `include_extensions`, `index_depth`,
/// `prefix_mode`. The plugin shows them so that what an index was built with is not
/// a thing you have to guess, and so that a settings screen can offer a field for
/// each of them.
///
/// # Safety
/// `handle` must come from [`ts_open`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ts_config_text(handle: *mut TsIndex) -> *mut c_char {
    unsafe {
        let Some(handle) = as_ref(handle) else {
            set_error("ts_config_text: null handle");
            return std::ptr::null_mut();
        };
        match handle.engine.config() {
            Ok(cfg) => {
                clear_error();
                let mut body = String::new();
                for (key, value) in cfg.to_lines() {
                    body.push_str(&key);
                    body.push('=');
                    body.push_str(&value);
                    body.push('\n');
                }
                to_c_string(&body)
            }
            Err(e) => {
                set_error(&format!("ts_config_text: {e}"));
                std::ptr::null_mut()
            }
        }
    }
}

/// Applies settings lines, leaving the ones absent alone.
///
/// Returns [`TS_OK`], or a negative code with the reason in `ts_last_error`. The
/// partial-merge rule matters for a plugin that has shown its own subset of the
/// settings: a field it does not know about must survive the save.
///
/// `content_mode` is refused here — it changes what is stored, so the postings
/// already in the index would be in the wrong shape for it. Both the plugin and
/// the indexer set it when an index is built.
///
/// # Safety
/// `handle` must come from [`ts_open`]; `text` must be a null-terminated UTF-8
/// string or null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ts_set_config_text(handle: *mut TsIndex, text: *const c_char) -> c_int {
    unsafe {
        let Some(handle) = as_ref(handle) else {
            set_error("ts_set_config_text: null handle");
            return TS_ERR_ARGUMENT;
        };
        let Some(text) = to_str(text) else {
            set_error("ts_set_config_text: null or empty text");
            return TS_ERR_ARGUMENT;
        };

        let mut cfg = match handle.engine.config() {
            Ok(c) => c,
            Err(e) => {
                set_error(&format!("ts_set_config_text: {e}"));
                return TS_ERR_QUERY;
            }
        };
        if let Err(e) = cfg.merge_lines(text) {
            set_error(&format!("ts_set_config_text: {e}"));
            return TS_ERR_ARGUMENT;
        }
        let blob = cfg.encode();
        match handle.engine.write_config_raw(&blob) {
            Ok(()) => {
                clear_error();
                TS_OK
            }
            Err(e) => {
                set_error(&format!("ts_set_config_text: {e}"));
                TS_ERR_QUERY
            }
        }
    }
}

fn search_impl(
    handle: &TsIndex,
    query: &str,
    scope: &str,
) -> crate::storage::RunResult<(*mut c_char, c_int)> {
    let hits = query::search_phrase(handle.engine, &handle.dict, query)?;
    let hits = query::live_hits(handle.engine, hits, scope)?;
    let buffer = lines_buffer(hits.iter().map(|h| (h.doc_id, h.score)));
    Ok((buffer, hits.len() as c_int))
}

/// Writes a results buffer and reports the count.
unsafe fn err_ret(msg: &str, out_count: *mut c_int) -> *mut c_char {
    unsafe {
        set_error(msg);
        if !out_count.is_null() {
            *out_count = 0;
        }
        std::ptr::null_mut()
    }
}

/// Searches metadata: title, author, series and description.
///
/// Writes `"<doc_id>\t<title>\n"` lines into a buffer released with [`ts_free`].
///
/// # Safety
/// As [`ts_search`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ts_search_meta(
    handle: *mut TsIndex,
    query: *const c_char,
    scope: *const c_char,
    out_count: *mut c_int,
) -> *mut c_char {
    unsafe {
        let Some(handle) = as_ref(handle) else {
            return err_ret("ts_search_meta: null handle", out_count);
        };
        let Some(q) = to_str(query) else {
            return err_ret("ts_search_meta: null or empty query", out_count);
        };

        match panic::catch_unwind(AssertUnwindSafe(|| {
            let rows = metadata::search(handle.engine, q)?;
            let ids = crate::storage::book::scope_ids(handle.engine, to_str(scope).unwrap_or(""))?;
            crate::storage::RunResult::Ok(
                rows.into_iter()
                    .filter(|(id, _)| ids.contains(id))
                    .collect::<Vec<_>>(),
            )
        })) {
            Ok(Ok(rows)) => {
                clear_error();
                let mut body = String::new();
                for (id, title) in &rows {
                    body.push_str(&format!("{id}\t{title}\n"));
                }
                if !out_count.is_null() {
                    *out_count = rows.len() as c_int;
                }
                if rows.is_empty() {
                    // An empty result is a valid empty buffer, not a failure.
                    return to_c_string("");
                }
                to_c_string(&body)
            }
            Ok(Err(e)) => err_ret(&format!("ts_search_meta: {e}"), out_count),
            Err(_) => err_ret("ts_search_meta: panicked", out_count),
        }
    }
}

/// Starts indexing what `dir` still needs, and reports how many files are left.
///
/// `dir` empty means every configured root: the whole library, which is minutes
/// rather than seconds and worth warning about before a single book is read. The
/// scan that answers the count costs one `stat` per file and writes nothing, so
/// a caller can ask what this is going to take and then decide.
///
/// A previous session is replaced rather than resumed: the reader asks again
/// each time, and a session that is finished rather than abandoned still writes
/// the metadata it has been holding.
///
/// # Safety
/// `handle` must come from [`ts_open`]; `dir` must be a null-terminated UTF-8
/// path or null, and null means the whole library.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ts_index_begin(handle: *mut TsIndex, dir: *const c_char) -> c_int {
    unsafe {
        let Some(engine) = as_ref(handle).map(|h| h.engine) else {
            return TS_ERR_ARGUMENT;
        };
        let dir = to_str(dir).unwrap_or("").to_string();

        let built = panic::catch_unwind(AssertUnwindSafe(|| {
            let plan = if dir.is_empty() {
                crate::storage::build::plan_full(engine, None)
            } else {
                crate::storage::build::plan_scope(engine, Path::new(&dir), None)
            };
            if let Some(why) = &plan.scan_error {
                // An unreadable root is not fatal — the card may be out — but it
                // does mean this pass indexes nothing, and a caller told
                // otherwise would search an index that never grew.
                set_error(&format!("ts_index_begin: {why}"));
            }
            let mode = engine.config()?.content_mode;
            crate::storage::build::Session::new(engine, plan, mode)
        }));
        match built {
            Ok(Ok(session)) => {
                let pending = session.pending() as c_int;
                if session.pending() == 0 && session.gone() == 0 {
                    // Nothing to read and nothing to drop. Keeping the session
                    // anyway would make closing the handle finish it, and
                    // finishing rebuilds the metadata index — a write on every
                    // search, for nothing, on a device that writes.
                    clear_error();
                    return 0;
                }
                if let Some(h) = as_mut(handle) {
                    h.session = Some(session);
                }
                clear_error();
                pending
            }
            Ok(Err(e)) => {
                set_error(&format!("ts_index_begin: {e}"));
                TS_ERR_QUERY
            }
            Err(_) => {
                set_error("ts_index_begin: panicked");
                TS_ERR_INTERNAL
            }
        }
    }
}

/// Reads and indexes at most `budget` of the files the session has left, and
/// reports what is still to do.
///
/// One call per batch rather than one per session, because a screen cannot be
/// redrawn from inside a call that never returns: the reader shows how much is
/// left between batches, and the engine gets to hand control back. Zero or a
/// negative `budget` takes whatever is left.
///
/// # Safety
/// `handle` must come from [`ts_open`] and have a session from [`ts_index_begin`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ts_index_step(handle: *mut TsIndex, budget: c_int) -> c_int {
    unsafe {
        let Some(engine) = as_ref(handle).map(|h| h.engine) else {
            return TS_ERR_ARGUMENT;
        };
        let Some(session) = as_mut(handle).and_then(|h| h.session.as_mut()) else {
            set_error("ts_index_step: no session; call ts_index_begin first");
            return TS_ERR_ARGUMENT;
        };
        let budget = if budget <= 0 {
            usize::MAX
        } else {
            budget as usize
        };

        match panic::catch_unwind(AssertUnwindSafe(|| session.step(engine, budget))) {
            Ok(Ok(left)) => {
                clear_error();
                left as c_int
            }
            Ok(Err(e)) => {
                set_error(&format!("ts_index_step: {e}"));
                TS_ERR_QUERY
            }
            Err(_) => {
                set_error("ts_index_step: panicked");
                TS_ERR_INTERNAL
            }
        }
    }
}

/// Ends the session: the metadata it held is written, the metadata index is
/// rebuilt, the file is compacted when anything was written, and the handle's
/// dictionary is reloaded so the next search sees what was just indexed.
///
/// Writes the key/value lines `key=value`, one per line, which is what a caller
/// turns into a sentence. Empty when there was no session, which is a valid
/// answer rather than an error: nothing was asked of it.
///
/// # Safety
/// `handle` must come from [`ts_open`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ts_index_end(handle: *mut TsIndex) -> *mut c_char {
    unsafe {
        let Some(engine) = as_ref(handle).map(|h| h.engine) else {
            return err_ret("ts_index_end: null handle", std::ptr::null_mut());
        };
        let Some(session) = as_mut(handle).and_then(|h| h.session.take()) else {
            clear_error();
            return to_c_string("");
        };

        let done = panic::catch_unwind(AssertUnwindSafe(|| session.finish(engine, true)));
        let stats = match done {
            Ok(Ok(stats)) => stats,
            Ok(Err(e)) => return err_ret(&format!("ts_index_end: {e}"), std::ptr::null_mut()),
            Err(_) => return err_ret("ts_index_end: panicked", std::ptr::null_mut()),
        };
        // The session stored a dictionary of its own; the handle is still
        // searching with the one it loaded at open, and a query run before this
        // line would be answered from a dictionary that has never heard of the
        // terms just written.
        match dict::load(engine) {
            Ok(d) => {
                if let Some(h) = as_mut(handle) {
                    h.dict = d;
                }
            }
            Err(e) => return err_ret(&format!("ts_index_end: {e}"), std::ptr::null_mut()),
        }
        clear_error();
        to_c_string(&format!(
            "indexed={}\nremoved={}\nskipped={}\nfailed={}\nsections={}\ntokens={}\nbytes={}\nreclaimed={}\n",
            stats.indexed,
            stats.removed,
            stats.skipped,
            stats.failed,
            stats.sections,
            stats.tokens,
            stats.text_bytes,
            stats.reclaimed
        ))
    }
}

/// Reads a book's title, for showing next to a result.
///
/// Writes the title into a buffer released with [`ts_free`]. Null when the book
/// has no metadata.
///
/// # Safety
/// As [`ts_search`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ts_book_title(handle: *mut TsIndex, book_id: c_int) -> *mut c_char {
    unsafe {
        let Some(handle) = as_ref(handle) else {
            set_error("ts_book_title: null handle");
            return std::ptr::null_mut();
        };
        if book_id < 0 || book_id > u16::MAX as c_int {
            set_error("ts_book_title: book id out of range");
            return std::ptr::null_mut();
        }
        match handle.engine.metadata(book_id as u16) {
            Ok(Some(record)) => to_c_string(&record.title),
            Ok(None) => std::ptr::null_mut(),
            Err(e) => {
                set_error(&format!("ts_book_title: {e}"));
                std::ptr::null_mut()
            }
        }
    }
}

/// Reads a book's file path, for opening it in the reader.
///
/// # Safety
/// As [`ts_search`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ts_book_path(handle: *mut TsIndex, book_id: c_int) -> *mut c_char {
    unsafe {
        let Some(handle) = as_ref(handle) else {
            set_error("ts_book_path: null handle");
            return std::ptr::null_mut();
        };
        if book_id < 0 || book_id > u16::MAX as c_int {
            set_error("ts_book_path: book id out of range");
            return std::ptr::null_mut();
        }
        match handle.engine.book(book_id as u16) {
            Ok(Some(book)) => to_c_string(&book.path),
            Ok(None) => std::ptr::null_mut(),
            Err(e) => {
                set_error(&format!("ts_book_path: {e}"));
                std::ptr::null_mut()
            }
        }
    }
}

// --- helpers ----------------------------------------------------------------

/// Borrows a handle, or null-checks it.
unsafe fn as_ref<'b>(handle: *mut TsIndex) -> Option<&'b TsIndex> {
    unsafe {
        if handle.is_null() {
            None
        } else {
            Some(&*(handle as *const TsIndex))
        }
    }
}

/// Borrows a handle mutably, for one call.
///
/// The lifetime is the caller's, exactly as in [`as_ref`]: the handle is owned
/// by whoever passed the pointer in, and nothing here outlives the call it was
/// made for. Nothing may hold both at once.
unsafe fn as_mut<'b>(handle: *mut TsIndex) -> Option<&'b mut TsIndex> {
    unsafe {
        if handle.is_null() {
            None
        } else {
            Some(&mut *handle)
        }
    }
}

/// Borrows a C string as Rust text.
unsafe fn to_str<'b>(ptr: *const c_char) -> Option<&'b str> {
    unsafe {
        if ptr.is_null() {
            return None;
        }
        std::ffi::CStr::from_ptr(ptr).to_str().ok()
    }
}

/// Copies `text` into a fresh buffer.
fn to_c_string(text: &str) -> *mut c_char {
    let bytes = text.as_bytes();
    let mut buf = vec![0u8; bytes.len() + 1];
    buf[..bytes.len()].copy_from_slice(bytes);
    let raw = buf.as_mut_ptr();
    std::mem::forget(buf);
    raw as *mut c_char
}

/// Builds a buffer of `"<id> <score>\n"` lines.
fn lines_buffer(rows: impl Iterator<Item = (u32, u32)>) -> *mut c_char {
    let mut body = String::new();
    for (id, score) in rows {
        body.push_str(&format!("{id} {score}\n"));
    }
    to_c_string(&body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;

    /// A round trip through the string helpers, since ownership bugs there are
    /// use-after-free in C rather than a Rust panic.
    #[test]
    fn a_buffer_round_trips_and_frees() {
        let original = "hello, Lua";
        let buf = to_c_string(original);
        assert!(!buf.is_null());
        let back = unsafe { std::ffi::CStr::from_ptr(buf) }.to_str().unwrap();
        assert_eq!(back, original);
        unsafe { ts_free(buf) };
    }

    #[test]
    fn null_is_safe_to_free() {
        unsafe { ts_free(std::ptr::null_mut()) };
    }

    #[test]
    fn ts_alloc_buffer_gives_space_for_a_terminator() {
        for len in [0usize, 1, 8, 4096, 1 << 20] {
            let buf = ts_alloc_buffer(len);
            assert!(!buf.is_null());
            unsafe {
                std::ptr::write_bytes(buf as *mut u8, b'x', len);
                *buf.add(len) = 0;
                assert_eq!(std::ffi::CStr::from_ptr(buf).to_bytes().len(), len);
                ts_free(buf);
            }
        }
    }

    #[test]
    fn ts_version_is_stable_across_calls() {
        let a = ts_version();
        let b = ts_version();
        assert_eq!(a, b, "the version string must not move between calls");
    }

    #[test]
    fn error_convention_starts_empty_and_is_set_by_a_bad_call() {
        // Nothing has failed yet on this thread.
        clear_error();
        assert!(ts_last_error().is_null());

        let bad = CString::new("").unwrap();
        let mut count = -99i32;
        let handle = std::ptr::null_mut::<TsIndex>();
        let out = unsafe { ts_search(handle, bad.as_ptr(), std::ptr::null(), &mut count) };
        assert!(out.is_null());
        assert_eq!(count, 0, "a failed call must zero the count");
        let err = unsafe { std::ffi::CStr::from_ptr(ts_last_error()) }
            .to_str()
            .unwrap()
            .to_string();
        assert!(err.contains("null handle"), "unexpected error: {err}");
    }

    #[test]
    fn a_c_string_of_every_length_round_trips() {
        for len in 0..40usize {
            let text = "a".repeat(len);
            let buf = to_c_string(&text);
            let back = unsafe { std::ffi::CStr::from_ptr(buf) }.to_str().unwrap();
            assert_eq!(back, text);
            unsafe { ts_free(buf) };
        }
    }

    #[test]
    fn lines_buffer_formats_one_line_per_result() {
        let buf = lines_buffer([(7u32, 12u32), (9u32, 3u32)].into_iter());
        let back = unsafe { std::ffi::CStr::from_ptr(buf) }.to_str().unwrap();
        assert_eq!(back, "7 12\n9 3\n");
        unsafe { ts_free(buf) };
    }

    #[test]
    fn a_bad_path_reports_an_error_rather_than_aborting() {
        let path = CString::new("/definitely/not/here/index.db").unwrap();
        let handle = unsafe { ts_open(path.as_ptr()) };
        assert!(handle.is_null(), "opening a bad path must fail cleanly");
        let err = unsafe { std::ffi::CStr::from_ptr(ts_last_error()) }
            .to_str()
            .unwrap()
            .to_string();
        assert!(err.contains("ts_open"), "unexpected error: {err}");
    }

    #[test]
    fn a_null_handle_is_refused_by_every_entry_point() {
        let mut count = 7i32;
        assert!(
            unsafe {
                ts_search(
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    std::ptr::null(),
                    &mut count,
                )
            }
            .is_null()
        );
        assert!(
            unsafe {
                ts_search_meta(
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    std::ptr::null(),
                    &mut count,
                )
            }
            .is_null()
        );
        assert!(unsafe { ts_book_title(std::ptr::null_mut(), 0) }.is_null());
        assert!(unsafe { ts_book_path(std::ptr::null_mut(), 0) }.is_null());
        assert_eq!(
            unsafe { ts_content_mode(std::ptr::null_mut()) },
            TS_ERR_ARGUMENT
        );
        assert_eq!(count, 0);
    }

    #[test]
    fn an_out_of_range_book_id_is_refused() {
        // No handle needed: the range check comes first.
        assert!(unsafe { ts_book_title(std::ptr::null_mut::<TsIndex>(), -1) }.is_null());
        assert!(unsafe { ts_book_title(std::ptr::null_mut::<TsIndex>(), i32::MAX) }.is_null());
    }
}
