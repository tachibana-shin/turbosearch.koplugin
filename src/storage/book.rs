//! The `BOOKS` table and the scan that keeps it in sync with the filesystem.
//!
//! Change detection is `(path, size, mtime)` only. `inode` is recorded for
//! diagnostics because `/mnt/us` on a Kindle is vfat, where inode numbers are
//! not stable; see `note.md` section 0, row 7.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use super::pack::{Reader, Writer};
use super::{Config, Engine, RunResult};

/// Container formats the scanner can recognise, from `note.md` section 3.0.1.
///
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum Format {
    #[default]
    Unknown = 0,
    Epub = 1,
    Kepub = 2,
    Mobi = 3,
    Azw = 4,
    Azw3 = 5,
    Txt = 6,
    Markdown = 7,
    Html = 8,
    Xhtml = 9,
    Fb2 = 10,
    Rtf = 11,
    Pdf = 12,
    Djvu = 13,
    Cbz = 14,
    Kfx = 15,
    /// Unparseable for content, but metadata was captured.
    MetadataOnly = 254,
}

impl Format {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Format::Unknown),
            1 => Some(Format::Epub),
            2 => Some(Format::Kepub),
            3 => Some(Format::Mobi),
            4 => Some(Format::Azw),
            5 => Some(Format::Azw3),
            6 => Some(Format::Txt),
            7 => Some(Format::Markdown),
            8 => Some(Format::Html),
            9 => Some(Format::Xhtml),
            10 => Some(Format::Fb2),
            11 => Some(Format::Rtf),
            12 => Some(Format::Pdf),
            13 => Some(Format::Djvu),
            14 => Some(Format::Cbz),
            15 => Some(Format::Kfx),
            254 => Some(Format::MetadataOnly),
            _ => None,
        }
    }

    /// Extension used when guessing a format before the magic sniff runs.
    pub fn from_extension(ext: &str) -> Format {
        match ext.to_ascii_lowercase().as_str() {
            "epub" => Format::Epub,
            "kepub" => Format::Kepub,
            "mobi" => Format::Mobi,
            "azw" => Format::Azw,
            "azw3" => Format::Azw3,
            "txt" | "text" => Format::Txt,
            "md" | "markdown" => Format::Markdown,
            "html" | "htm" | "shtml" => Format::Html,
            "xhtml" | "xht" => Format::Xhtml,
            "fb2" | "fb2.zip" => Format::Fb2,
            "rtf" => Format::Rtf,
            "pdf" => Format::Pdf,
            "djvu" | "djv" => Format::Djvu,
            "cbz" | "cbr" | "cb7" => Format::Cbz,
            "kfx" => Format::Kfx,
            _ => Format::Unknown,
        }
    }

    /// True when this format carries text worth indexing.
    pub fn is_text(&self) -> bool {
        matches!(
            self,
            Format::Epub
                | Format::Kepub
                | Format::Mobi
                | Format::Azw
                | Format::Azw3
                | Format::Txt
                | Format::Markdown
                | Format::Html
                | Format::Xhtml
                | Format::Fb2
                | Format::Rtf
        )
    }
}

/// `BOOKS` flag bits, from `note.md` section 2.
pub mod flags {
    /// Positions were capped for this book.
    pub const PARTIAL: u8 = 1 << 0;
    /// Entry order fell back to entry-name order instead of spine order.
    pub const UNSORTED_SPINE: u8 = 1 << 1;
    /// Book is indexed but has no content postings.
    pub const METADATA_ONLY: u8 = 1 << 2;
}

/// One row of `BOOKS`, keyed by `book_id`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Book {
    pub path: String,
    /// The scan root that discovered this book.
    ///
    /// This is what makes a scoped search cheap. When the user browses into a
    /// subdirectory and types a query, we index only that directory and then
    /// filter results to it, instead of sweeping every configured `source_dirs`.
    pub scope: String,
    pub size: u64,
    /// Milliseconds since the Unix epoch.
    pub mtime_ms: i64,
    /// Recorded for diagnostics only; never used for change detection.
    pub inode: u64,
    pub format: Format,
    pub entry_count: u16,
    pub token_count: u32,
    pub indexed_at_ms: i64,
    pub flags: u8,
}

impl Book {
    pub fn encode(&self) -> Vec<u8> {
        // `scope` is appended last so that a row written by an older build fails
        // loudly on decode instead of silently shifting every later field.
        Writer::with_capacity(64)
            .str(&self.path)
            .u64(self.size)
            .i64(self.mtime_ms)
            .u64(self.inode)
            .u8(self.format as u8)
            .u16(self.entry_count)
            .u32(self.token_count)
            .i64(self.indexed_at_ms)
            .u8(self.flags)
            .str(&self.scope)
            .finish()
    }

    pub fn decode(bytes: &[u8]) -> super::pack::Result<Self> {
        let mut r = Reader::new(bytes);
        Ok(Book {
            path: r.str()?,
            size: r.u64()?,
            mtime_ms: r.i64()?,
            inode: r.u64()?,
            format: Format::from_u8(r.u8()?).unwrap_or(Format::Unknown),
            entry_count: r.u16()?,
            token_count: r.u32()?,
            indexed_at_ms: r.i64()?,
            flags: r.u8()?,
            scope: r.str()?,
        })
    }
}

/// Milliseconds since the Unix epoch, the unit `BOOKS` stores.
///
/// Falls back to 0 when the platform reports a pre-epoch time, which vfat can
/// do for files with a bogus timestamp.
pub fn mtime_ms(m: &fs::Metadata) -> i64 {
    m.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn inode(m: &fs::Metadata) -> u64 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        m.ino()
    }
    #[cfg(not(unix))]
    {
        let _ = m;
        0
    }
}

/// Reads the `(size, mtime_ms, inode)` triple for one regular file.
pub fn stat_triple(path: &Path) -> Option<(u64, i64, u64)> {
    let m = fs::metadata(path).ok()?;
    if !m.is_file() {
        return None;
    }
    Some((m.len(), mtime_ms(&m), inode(&m)))
}

/// A candidate file found on disk.
#[derive(Debug, Clone)]
pub struct FileEntry {
    pub path: PathBuf,
    /// Scan root that produced this entry; assigned by `scan_with_mode`.
    pub scope: Option<String>,
    pub size: u64,
    pub mtime_ms: i64,
    pub inode: u64,
    pub format: Format,
}

/// What one scan decided about every candidate file.
#[derive(Debug, Clone, Default)]
pub struct ScanPlan {
    /// Which kind of pass produced this plan.
    pub mode: ScanMode,
    /// Roots this pass visited, readable or not, in request order.
    pub scanned_roots: Vec<String>,
    /// New files that need a `book_id` assigned and content indexed.
    pub new: Vec<FileEntry>,
    /// Files whose metadata changed, needing a new id and a reindex.
    pub updated: Vec<FileEntry>,
    /// Files already indexed and unchanged; zero I/O beyond the `stat`.
    pub unchanged: Vec<PathBuf>,
    /// `book_id`s that are in the database but gone from disk. Their postings
    /// become orphans and are dropped at read time.
    pub missing: Vec<(u16, String)>,
}

impl ScanPlan {
    pub fn needs_work(&self) -> bool {
        !self.new.is_empty() || !self.updated.is_empty() || !self.missing.is_empty()
    }

    pub fn total(&self) -> usize {
        self.new.len() + self.updated.len() + self.unchanged.len() + self.missing.len()
    }
}

/// Filter that decides whether a path is a candidate at all.
///
/// The rule set lives in `Config` (`note.md` section 3.5); this is the resolver
/// that applies it.
#[derive(Debug, Clone)]
pub struct FileFilter {
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub exclude_dir_globs: Vec<String>,
    pub min_size: u64,
    pub max_size: u64,
}

impl FileFilter {
    /// True when the entry should be handed to the format sniffer.
    pub fn accepts(&self, path: &Path) -> bool {
        let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
            return false;
        };
        let ext = ext.to_ascii_lowercase();
        if self.exclude.contains(&ext) {
            // Exclusion wins, per `note.md` section 3.5.1.
            return false;
        }
        if !self.include.contains(&ext) {
            return false;
        }
        let Ok(meta) = fs::metadata(path) else {
            return false;
        };
        if !meta.is_file() {
            return false;
        }
        let len = meta.len();
        len >= self.min_size && len <= self.max_size
    }

    /// True when a directory should be skipped entirely, children included.
    pub fn skips_dir(&self, dir_name: &str) -> bool {
        // Hidden entries are never scanned; `.sdr` is covered here too.
        if dir_name.starts_with('.') {
            return true;
        }
        self.exclude_dir_globs.iter().any(|glob| {
            match glob.strip_suffix('*') {
                // "*.sdr" -> any directory whose name ends with .sdr
                Some("*") => dir_name.ends_with(&glob[..glob.len() - 1]),
                Some(prefix) => dir_name.starts_with(prefix),
                None => glob == dir_name,
            }
        })
    }
}

/// Why a scan was started, which determines what it is allowed to conclude.
///
/// A search triggered from the file browser indexes only the directory being
/// browsed, so it must not touch books outside it. A full scan covers every
/// configured `source_dirs` root and is the only pass allowed to report
/// deletions broadly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScanMode {
    /// Index only the requested roots. Only books under those roots may be
    /// reported missing. This is the fast path used when the user types a query
    /// before any full scan has run.
    #[default]
    Scope,
    /// Sweep every configured source directory.
    Full,
}

/// Walks the source roots and diffs them against `BOOKS` / `PATHS`.
///
/// `roots` is always the set of directories this pass actually visited; `mode`
/// only widens or narrows what the pass is allowed to conclude about books it
/// did not visit.
pub fn scan_with_mode(
    engine: &Engine,
    filter: &FileFilter,
    roots: &[PathBuf],
    mode: ScanMode,
) -> RunResult<ScanPlan> {
    // Load every stored book once so the walk performs no per-file query.
    let known: HashMap<String, (u16, Book)> = engine
        .read_paths()?
        .into_iter()
        .filter_map(|(path, id)| engine.book(id).ok().flatten().map(|b| (path, (id, b))))
        .collect();

    let mut plan = ScanPlan {
        mode,
        ..ScanPlan::default()
    };
    let mut found: HashSet<String> = HashSet::new();

    // Roots this pass actually managed to read. A book may only be reported
    // missing if it lives under one of these, because only then do we have
    // current information about it.
    let mut covered_prefixes: Vec<String> = Vec::new();

    for root in roots {
        let mut visited = 0usize;
        let scope = root.to_string_lossy().into_owned();
        walk_root(root, filter, &mut |mut entry| {
            visited += 1;
            let key = entry.path.to_string_lossy().into_owned();
            found.insert(key.clone());
            // Remember which root found this book so a later scoped search can
            // restrict itself without inspecting every path.
            entry.scope = Some(scope.clone());
            match known.get(&key) {
                None => plan.new.push(entry),
                Some((_, stored)) => {
                    if stored.size == entry.size && stored.mtime_ms == entry.mtime_ms {
                        plan.unchanged.push(entry.path);
                    } else {
                        plan.updated.push(entry);
                    }
                }
            }
            Ok(())
        })?;

        // Record the root either way: it is the unit of coverage for bookkeeping
        // and for the UI's "what has been indexed" display.
        plan.scanned_roots.push(scope.clone());
        if visited > 0 || std::fs::read_dir(root).is_ok() {
            // The root was readable. An empty readable root still gives us the
            // information "there is nothing here", so deletions below it stand.
            covered_prefixes.push(scope);
        }
    }

    // A book is only missing when it lives under a root we can actually see.
    // Without this, a scoped index of one folder followed by a full scan of a
    // different folder would delete every book in the first.
    for (path, (id, _)) in &known {
        if found.contains(path) {
            continue;
        }
        let under_covered = covered_prefixes
            .iter()
            .any(|prefix| path == prefix || path.starts_with(&format!("{prefix}/")));
        if under_covered {
            plan.missing.push((*id, path.clone()));
        }
    }

    // Most recently modified first, per `note.md` section 1.2.
    let by_mtime_desc = |a: &FileEntry, b: &FileEntry| b.mtime_ms.cmp(&a.mtime_ms);
    plan.new.sort_by(by_mtime_desc);
    plan.updated.sort_by(by_mtime_desc);

    Ok(plan)
}

/// True when `path` lies inside `scope`.
///
/// Both sides are compared as path prefixes rather than byte prefixes so that
/// `/docs` does not match `/documents-extra`. An exact match counts, so a scope
/// pointed straight at a book file still matches that file.
pub fn path_in_scope(path: &str, scope: &str) -> bool {
    if scope.is_empty() {
        return true;
    }
    path == scope || path.starts_with(&format!("{scope}/"))
}

/// A scope restriction for a query.
///
/// `None` means "search everything that has been indexed", which is the natural
/// state once a full scan has run. `Some(dir)` restricts the result to books
/// discovered under that directory.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ScopeFilter(pub Option<String>);

impl ScopeFilter {
    /// Restricts results to `dir`.
    pub fn new(dir: impl Into<String>) -> Self {
        ScopeFilter(Some(dir.into()))
    }

    /// No restriction.
    pub fn all() -> Self {
        ScopeFilter(None)
    }

    /// True when a book at `path` is inside this scope.
    pub fn accepts(&self, path: &str) -> bool {
        match &self.0 {
            None => true,
            Some(dir) => path_in_scope(path, dir),
        }
    }
}

/// Whether a full scan has already covered the library, or only a scope has.
///
/// Drives the UI's choice between "indexing now, one second" and "indexing now,
/// several minutes", and lets the caller warn before starting the slow path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Coverage {
    /// Nothing has been indexed yet: a scoped index is the only usable option.
    Empty,
    /// Only some directories have been indexed; the next search can still be
    /// scoped to wherever the user is browsing.
    Partial,
    /// A full scan has run, so an unrestricted search answers from the whole
    /// library.
    Full,
}

/// Indexes only `dir`, ignoring the configured `source_dirs`.
///
/// This is what the query path calls when the user types a search before a full
/// scan has ever run: it indexes the directory being browsed and nothing else,
/// so the first search answers in seconds instead of minutes.
pub fn scan_scope(
    engine: &Engine,
    filter: &FileFilter,
    dir: impl AsRef<Path>,
) -> RunResult<ScanPlan> {
    scan_with_mode(
        engine,
        filter,
        &[dir.as_ref().to_path_buf()],
        ScanMode::Scope,
    )
}

/// Sweeps every configured source directory.
pub fn scan_full(engine: &Engine, config: &Config) -> RunResult<ScanPlan> {
    scan_with_mode(engine, &config.filter(), &config.roots(), ScanMode::Full)
}

fn walk_root(
    root: &Path,
    filter: &FileFilter,
    visit: &mut dyn FnMut(FileEntry) -> RunResult<()>,
) -> RunResult<()> {
    let Ok(entries) = fs::read_dir(root) else {
        // A missing or unreadable root is not fatal: the user may have unplugged
        // the SD card that this root points at.
        return Ok(());
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if filter.skips_dir(name) {
                continue;
            }
            walk_root(&path, filter, visit)?;
        } else if file_type.is_file()
            && filter.accepts(&path)
            && let Some((size, mtime, ino)) = stat_triple(&path)
        {
            let format = path
                .extension()
                .and_then(|e| e.to_str())
                .map(Format::from_extension)
                .unwrap_or(Format::Unknown);
            visit(FileEntry {
                path,
                scope: None,
                size,
                mtime_ms: mtime,
                inode: ino,
                format,
            })?;
        }
    }
    Ok(())
}

/// PATH lookups and whole-table reads.
///
/// `PATHS` is no longer a separate table: `books` has a unique index on `path`,
/// which is the same information with one fewer place to fall out of sync. The
/// `OR IGNORE` on the write path is what keeps a duplicate path from failing a
/// whole book batch — a library that contains the same file twice, which happens
/// with symlinked or rebranched calibre libraries, used to abort the scan.
impl Engine {
    /// Loads `path -> book_id` for every book.
    pub fn read_paths(&self) -> RunResult<HashMap<String, u16>> {
        self.block_on(async {
            let rows = sqlx::query_as::<_, PathRow>("SELECT path, id FROM books ORDER BY path")
                .fetch_all(self.pool())
                .await?;
            Ok(rows.into_iter().map(|r| (r.path, r.id as u16)).collect())
        })
    }

    /// Looks up one `book_id` by path.
    pub fn book_id_for_path(&self, path: &str) -> RunResult<Option<u16>> {
        self.block_on(async {
            let id: Option<i64> = crate::sql::engine::or_empty(
                sqlx::query_scalar::<_, i64>("SELECT id FROM books WHERE path = ?1")
                    .bind(path)
                    .fetch_one(self.pool())
                    .await,
            )?;
            Ok(id.and_then(|i| u16::try_from(i).ok()))
        })
    }

    /// Reads one `BOOKS` row.
    pub fn book(&self, id: u16) -> RunResult<Option<Book>> {
        self.block_on(async {
            let row: Option<crate::sql::engine::BookRow> = crate::sql::engine::or_empty(
                sqlx::query_as::<_, crate::sql::engine::BookRow>(
                    "SELECT id, path, scope, size, mtime_ms, inode, format,
                            entry_count, token_count, indexed_at_ms, flags
                     FROM books WHERE id = ?1",
                )
                .bind(id as i64)
                .fetch_one(self.pool())
                .await,
            )?;
            Ok(row.map(|r| r.into_book().1))
        })
    }

    /// Reads every `BOOKS` row, in `book_id` order.
    pub fn all_books(&self) -> RunResult<Vec<(u16, Book)>> {
        self.block_on(async {
            let rows = sqlx::query_as::<_, crate::sql::engine::BookRow>(
                "SELECT id, path, scope, size, mtime_ms, inode, format,
                        entry_count, token_count, indexed_at_ms, flags
                 FROM books ORDER BY id",
            )
            .fetch_all(self.pool())
            .await?;
            Ok(rows.into_iter().map(|r| r.into_book()).collect())
        })
    }
}

/// A `(path, id)` pair.
#[derive(sqlx::FromRow)]
struct PathRow {
    path: String,
    id: i64,
}
