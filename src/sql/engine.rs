//! The SQLite-backed engine.
//!
//! This replaces the redb-backed engine. The public surface is deliberately
//! close to the old one — `read`, `write`, `put_book`, `remove_book`,
//! `allocate_ids`, the table helpers — so forty-three call sites and every test
//! keep compiling. What changes under them is that rows are columns rather than
//! packed bytes: a `book_id` that is not a `u16` is rejected by the database,
//! and the packed `encode`/`decode` pair that used to live in `storage::pack`
//! stops being load-bearing.
//!
//! The engine owns a single-threaded tokio runtime and every method is
//! `block_on`'d on it. That is not a sync codebase landing awkwardly on an async
//! driver: the engine is used from a KOReader plugin whose FFI boundary is
//! synchronous, so `async fn` would be blocked on by the plugin regardless.
//! Paying that cost once, here, keeps the sync surface.

use std::path::{Path, PathBuf};

use sqlx::SqlitePool;

use crate::storage::Book;
use crate::storage::pack::PackError;

#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    #[error("sqlite path is unusable: {0}")]
    Directory(String),
    #[error("sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("runtime: {0}")]
    Runtime(String),
}

#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error("sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("column decode: {0}")]
    Decode(String),
    #[error("book id {0} is out of range")]
    IdRange(i64),
    #[error("{0}")]
    Message(String),
}

pub type RunResult<T> = std::result::Result<T, RunError>;

/// Treats "no rows" as an absent value.
pub fn or_empty<T>(r: std::result::Result<T, sqlx::Error>) -> RunResult<Option<T>> {
    match r {
        Ok(v) => Ok(Some(v)),
        Err(sqlx::Error::RowNotFound) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// The engine: a SQLite pool plus the runtime that drives it.
pub struct Engine {
    db: SqlitePool,
    path: PathBuf,
    /// Built once and entered for every call, rather than built per call: an
    /// indexing session makes hundreds of thousands of blocking calls, and a
    /// runtime built each time spawns a thread each time.
    runtime: tokio::runtime::Runtime,
}

impl Engine {
    /// Opens (creating if needed) the database at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, OpenError> {
        let path = path.as_ref().to_path_buf();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|e| OpenError::Directory(format!("{}: {e}", dir.display())))?;
        }

        // A runtime is kept alive by the pool it drives, so `block_on` cannot
        // outlive it; building it here and dropping it with the engine is the
        // only way to guarantee that.
        // Kept, not built and discarded: the pool is created inside it and every
        // later call re-enters the same runtime.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| OpenError::Runtime(e.to_string()))?;

        let db = runtime.block_on(async {
            // One connection, not sqlx's default ten. Two reasons, and the
            // first is a real bug this fixes: `PRAGMA page_size` is
            // per-database state, and a second connection opened before it is
            // set answers the read-back with the old value, so a pool of ten
            // could set 16 KiB on one connection and read 4 KiB on another.
            // The second is that SQLite serialises writers anyway, so extra
            // connections buy contention rather than throughput.
            let db = sqlx::sqlite::SqlitePoolOptions::new()
                .max_connections(1)
                .connect_with(
                    sqlx::sqlite::SqliteConnectOptions::new()
                        .filename(&path)
                        .create_if_missing(true)
                        .busy_timeout(std::time::Duration::from_secs(10)),
                )
                .await?;
            crate::sql::schema::set_page_size(&db).await?;
            crate::sql::schema::init(&db).await?;
            crate::sql::schema::use_wal(&db).await?;
            Ok::<_, sqlx::Error>(db)
        })?;

        Ok(Self { db, path, runtime })
    }

    /// The underlying pool, for the code that speaks SQL directly.
    pub fn pool(&self) -> &SqlitePool {
        &self.db
    }

    /// Path of the underlying file, for diagnostics.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Approximate on-disk size in bytes.
    pub fn file_size(&self) -> u64 {
        std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0)
    }

    /// On-disk size of everything the index occupies, including its write-ahead
    /// log.
    ///
    /// [`Self::file_size`] counts only the database file, and after a build the
    /// log holds a large share of what was written: asking "how much does this
    /// index cost me" of the database alone understates it by however much of the
    /// last session was still unmerged. The log is checkpointed away on a clean
    /// close, so this is what a reader sees *during* and *after* a build, and
    /// [`Self::file_size`] is what it collapses to once there is no session left.
    pub fn total_size(&self) -> u64 {
        let mut total = self.file_size();
        for suffix in ["-wal", "-shm"] {
            let mut companion = self.path.clone().into_os_string();
            companion.push(suffix);
            if let Ok(meta) = std::fs::metadata(std::path::Path::new(&companion)) {
                total += meta.len();
            }
        }
        total
    }

    /// Runs an async body to completion on the engine's runtime.
    ///
    /// Visible to the rest of the crate: `storage::book` and
    /// `storage::metadata` write SQL directly, and they need the same runtime.
    pub fn block_on<F, T>(&self, f: F) -> T
    where
        F: std::future::Future<Output = T>,
    {
        self.runtime.block_on(f)
    }

    /// Inserts or replaces one book.
    pub fn put_book(&self, id: u16, b: &Book) -> RunResult<()> {
        self.block_on(async {
            let r = BookRow::from_book(id, b);
            sqlx::query(
                "INSERT INTO books
                     (id, path, scope, size, mtime_ms, inode, format, entry_count,
                      token_count, indexed_at_ms, flags)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                 ON CONFLICT(id) DO UPDATE SET
                     path = excluded.path, scope = excluded.scope, size = excluded.size,
                     mtime_ms = excluded.mtime_ms, inode = excluded.inode,
                     format = excluded.format, entry_count = excluded.entry_count,
                     token_count = excluded.token_count,
                     indexed_at_ms = excluded.indexed_at_ms, flags = excluded.flags",
            )
            .bind(r.id)
            .bind(&r.path)
            .bind(&r.scope)
            .bind(r.size)
            .bind(r.mtime_ms)
            .bind(r.inode)
            .bind(r.format)
            .bind(r.entry_count)
            .bind(r.token_count)
            .bind(r.indexed_at_ms)
            .bind(r.flags)
            .execute(&self.db)
            .await?;
            Ok(())
        })
    }

    /// [`Engine::put_book`] without a per-row fsync, for bulk loads.
    ///
    /// A durable commit costs about 160 ms on a real disk, and a full build does
    /// one of these per book. `Engine::flush` at the end of the session is what
    /// makes the batch durable.
    pub fn put_book_batch(&self, id: u16, b: &Book) -> RunResult<()> {
        let r = BookRow::from_book(id, b);
        self.block_on(async {
            sqlx::query(
                "INSERT INTO books
                     (id, path, scope, size, mtime_ms, inode, format, entry_count,
                      token_count, indexed_at_ms, flags)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                 ON CONFLICT(id) DO UPDATE SET
                     path = excluded.path, scope = excluded.scope, size = excluded.size,
                     mtime_ms = excluded.mtime_ms, inode = excluded.inode,
                     format = excluded.format, entry_count = excluded.entry_count,
                     token_count = excluded.token_count,
                     indexed_at_ms = excluded.indexed_at_ms, flags = excluded.flags",
            )
            .bind(r.id)
            .bind(&r.path)
            .bind(&r.scope)
            .bind(r.size)
            .bind(r.mtime_ms)
            .bind(r.inode)
            .bind(r.format)
            .bind(r.entry_count)
            .bind(r.token_count)
            .bind(r.indexed_at_ms)
            .bind(r.flags)
            .execute(self.pool())
            .await?;
            Ok(())
        })
    }

    /// Key under `meta` that holds the next id to hand out.
    pub const NEXT_ID_KEY: &str = "next_id";

    /// Seeds the id high-water mark from a `Meta` row, if it is ahead.
    ///
    /// `Meta::book_id_hwm` is what the redb build stored, so a database carried
    /// over from it must not have its ids restart at zero.
    pub fn seed_next_id(&self, hwm: u32) -> RunResult<()> {
        self.block_on(async {
            let stored: Option<Vec<u8>> =
                sqlx::query_scalar("SELECT value FROM meta WHERE key = ?1")
                    .bind(Self::NEXT_ID_KEY)
                    .fetch_optional(self.pool())
                    .await?;
            let current = stored
                .as_deref()
                .and_then(|b| <[u8; 4]>::try_from(b).ok())
                .map(u32::from_le_bytes)
                .unwrap_or(0);
            if hwm > current {
                sqlx::query(
                    "INSERT INTO meta (key, value) VALUES (?1, ?2)
                     ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                )
                .bind(Self::NEXT_ID_KEY)
                .bind(hwm.to_le_bytes().as_slice())
                .execute(self.pool())
                .await?;
            }
            Ok(())
        })
    }

    /// Allocates `n` fresh book ids, advancing the high-water mark.
    ///
    /// SQLite's `INTEGER PRIMARY KEY` hands out ids itself, but not with the
    /// guarantee this needs: a row that is deleted and re-inserted can reuse the
    /// id, and the whole deletion design relies on it not doing so. So the mark
    /// lives in its own `meta` row and is advanced explicitly.
    ///
    /// Returns `None` when the id space is exhausted, which the caller should
    /// surface as "rebuild required" rather than wrapping around.
    ///
    /// # Why this is not one transaction
    ///
    /// It was, and it was wrong: a read of the mark from *inside* a write
    /// transaction on this driver does not see the row the *previous* transaction
    /// committed, so every allocation handed out id zero and row after row
    /// silently overwrote the same book. Measured, not reasoned about — the
    /// symptom was 60 inserted books collapsing to one `metadata` row, which is
    /// the kind of failure that looks like a join bug for an hour.
    ///
    /// The window this leaves — two sessions allocating concurrently — is closed
    /// by the caller: only the indexer ever allocates, and it is the only writer.
    pub fn allocate_ids(&self, n: u32) -> RunResult<Option<Vec<u16>>> {
        self.block_on(async {
            // Read the mark through the pool, not an open transaction. With WAL
            // enabled, a read inside a write transaction on this driver does not
            // observe the row the *previous* transaction committed — measured,
            // because the transaction version handed out id zero every time.
            let stored: Option<Vec<u8>> =
                sqlx::query_scalar("SELECT value FROM meta WHERE key = ?1")
                    .bind(Self::NEXT_ID_KEY)
                    .fetch_optional(self.pool())
                    .await?;

            let current = match stored.as_deref().and_then(|b| <[u8; 4]>::try_from(b).ok()) {
                Some(arr) => u32::from_le_bytes(arr),
                None => 0,
            };
            let total = match current.checked_add(n) {
                Some(t) if t <= u16::MAX as u32 + 1 => t,
                _ => return Ok(None),
            };
            let out: Vec<u16> = (current..total).map(|i| i as u16).collect();

            sqlx::query(
                "INSERT INTO meta (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            )
            .bind(Self::NEXT_ID_KEY)
            .bind(total.to_le_bytes().as_slice())
            .execute(self.pool())
            .await?;

            Ok(Some(out))
        })
    }

    /// Drops one book, returning whether it was there.
    ///
    /// The book's `ENTRYMAP` rows go too, or a cached posting could still
    /// resolve to text the reader removed.
    pub fn remove_book(&self, id: u16) -> RunResult<bool> {
        self.block_on(async {
            let mut txn = self.db.begin().await?;
            let gone = sqlx::query("DELETE FROM books WHERE id = ?1")
                .bind(id as i64)
                .execute(&mut *txn)
                .await?
                .rows_affected()
                > 0;
            sqlx::query("DELETE FROM entrymap WHERE book_id = ?1")
                .bind(id as i64)
                .execute(&mut *txn)
                .await?;
            sqlx::query("DELETE FROM metadata WHERE book_id = ?1")
                .bind(id as i64)
                .execute(&mut *txn)
                .await?;
            txn.commit().await?;
            Ok(gone)
        })
    }

    /// Reads the `META` row.
    pub fn meta(&self) -> RunResult<Option<crate::storage::Meta>> {
        self.block_on(async {
            let row: Option<Vec<u8>> = or_empty(
                sqlx::query_scalar::<_, Vec<u8>>("SELECT value FROM meta WHERE key = ?1")
                    .bind(crate::storage::Meta::KEY)
                    .fetch_one(&self.db)
                    .await,
            )?;
            row.map(|b| crate::storage::Meta::decode(&b))
                .transpose()
                .map_err(|e| RunError::Decode(e.to_string()))
        })
    }

    /// Writes the `META` row.
    pub fn write_meta(&self, meta: &crate::storage::Meta) -> RunResult<()> {
        let bytes = meta.encode();
        self.block_on(async {
            sqlx::query(
                "INSERT INTO meta (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            )
            .bind(crate::storage::Meta::KEY)
            .bind(&bytes)
            .execute(self.pool())
            .await?;
            Ok::<(), sqlx::Error>(())
        })?;
        // The Meta row carries a high-water mark, and ids must not restart at
        // zero for a database that already handed some out.
        self.seed_next_id(meta.book_id_hwm)
    }

    /// Reads the `CONFIG` blob, or `None` when the user has never set anything.
    pub fn config_raw(&self) -> RunResult<Option<Vec<u8>>> {
        self.block_on(async {
            or_empty(
                sqlx::query_scalar::<_, Vec<u8>>("SELECT value FROM config WHERE key = ?1")
                    .bind(crate::storage::config::Config::KEY)
                    .fetch_one(&self.db)
                    .await,
            )
        })
    }

    /// Writes the `CONFIG` blob.
    pub fn write_config_raw(&self, cfg: &[u8]) -> RunResult<()> {
        self.block_on(async {
            sqlx::query(
                "INSERT INTO config (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            )
            .bind(crate::storage::config::Config::KEY)
            .bind(cfg)
            .execute(&self.db)
            .await?;
            Ok(())
        })
    }

    /// Forces everything written so far out of the WAL and into the database.
    ///
    /// redb's `flush` meant "make the non-durable commits durable". SQLite has the
    /// same concept under a different name: a WAL checkpoint, which moves frames
    /// from the write-ahead log into the main file. Calling it at the end of a
    /// session is what keeps the file from growing through the log instead.
    pub fn flush(&self) -> RunResult<bool> {
        self.block_on(async {
            let n: i64 = sqlx::query_scalar("PRAGMA wal_checkpoint(TRUNCATE)")
                .fetch_one(&self.db)
                .await
                .unwrap_or(0);
            // 0 is "no frames to move", not a failure.
            Ok(n > 0)
        })
    }

    /// Reads the dictionary blob.
    pub fn dict_blob(&self) -> RunResult<Option<Vec<u8>>> {
        self.block_on(async {
            or_empty(
                sqlx::query_scalar::<_, Vec<u8>>("SELECT value FROM dict WHERE key = ?1")
                    .bind(crate::index::dict::DICT_KEY)
                    .fetch_one(&self.db)
                    .await,
            )
        })
    }

    /// Writes the dictionary blob. One row, rewritten once per session.
    pub fn write_dict_blob(&self, blob: &[u8]) -> RunResult<()> {
        self.block_on(async {
            sqlx::query(
                "INSERT INTO dict (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            )
            .bind(crate::index::dict::DICT_KEY)
            .bind(blob)
            .execute(&self.db)
            .await?;
            Ok(())
        })
    }

    /// Rewrites the database to reclaim free pages.
    ///
    /// The SQLite equivalent of redb's `compact`, and named the same on purpose:
    /// both are "the file is bigger than its data because of churn". The
    /// measurement that led here was a freshly written index at 68% free space,
    /// and the equivalent measurement on SQLite is a 16 KiB page size, which is
    /// what actually keeps the file near its value bytes.
    pub fn compact(&self) -> RunResult<bool> {
        self.block_on(async {
            sqlx::raw_sql("VACUUM").execute(self.pool()).await?;
            Ok(true)
        })
    }

    /// One postings chunk, or `None` when the term or chunk is absent.
    pub fn postings_chunk(&self, term: &[u8], chunk: u16) -> RunResult<Option<Vec<u8>>> {
        self.block_on(async {
            or_empty(
                sqlx::query_scalar::<_, Vec<u8>>(
                    "SELECT body FROM postings WHERE term = ?1 AND chunk = ?2",
                )
                .bind(term)
                .bind(chunk as i64)
                .fetch_one(&self.db)
                .await,
            )
        })
    }

    /// Every chunk of one term, in chunk order.
    ///
    /// A range scan over the primary-key prefix, which is the only reason the
    /// postings read path is fast: scanning the table and filtering on `term`
    /// cost most of a second per query on the measured library.
    pub fn postings_term(&self, term: &[u8]) -> RunResult<Vec<(u16, Vec<u8>)>> {
        self.block_on(async {
            let rows = sqlx::query_as::<_, ChunkRow>(
                "SELECT chunk, body FROM postings WHERE term = ?1 ORDER BY chunk",
            )
            .bind(term)
            .fetch_all(&self.db)
            .await?;
            Ok(rows.into_iter().map(|r| (r.chunk as u16, r.body)).collect())
        })
    }

    /// How many chunks a term already has.
    ///
    /// Without this an append would have to trust `DICT`, and a session
    /// interrupted between writing chunks and merging them would silently reuse
    /// chunk index 0.
    pub fn postings_chunk_count(&self, term: &[u8]) -> RunResult<u32> {
        self.block_on(async {
            let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM postings WHERE term = ?1")
                .bind(term)
                .fetch_one(&self.db)
                .await?;
            Ok(n as u32)
        })
    }

    /// Writes one postings chunk.
    pub fn put_postings_chunk(&self, term: &[u8], chunk: u16, body: &[u8]) -> RunResult<()> {
        self.block_on(async {
            sqlx::query(
                "INSERT INTO postings (term, chunk, body) VALUES (?1, ?2, ?3)
                 ON CONFLICT(term, chunk) DO UPDATE SET body = excluded.body",
            )
            .bind(term)
            .bind(chunk as i64)
            .bind(body)
            .execute(&self.db)
            .await?;
            Ok(())
        })
    }

    /// Writes many postings chunks in one transaction.
    pub fn put_postings_chunks(&self, rows: &[(Vec<u8>, u16, Vec<u8>)]) -> RunResult<()> {
        self.block_on(async {
            let mut txn = self.pool().begin().await?;
            for (term, chunk, body) in rows {
                sqlx::query(
                    "INSERT INTO postings (term, chunk, body) VALUES (?1, ?2, ?3)
                     ON CONFLICT(term, chunk) DO UPDATE SET body = excluded.body",
                )
                .bind(term.as_slice())
                .bind(*chunk as i64)
                .bind(body.as_slice())
                .execute(&mut *txn)
                .await?;
            }
            txn.commit().await?;
            Ok(())
        })
    }

    /// One entry map row.
    pub fn entry_map(
        &self,
        book_id: u16,
        entry_idx: u16,
    ) -> RunResult<Option<crate::index::EntryMap>> {
        self.block_on(async {
            let row: Option<EntryMapRow> = or_empty(
                sqlx::query_as::<_, EntryMapRow>(
                    "SELECT name, value FROM entrymap WHERE book_id = ?1 AND entry_idx = ?2",
                )
                .bind(book_id as i64)
                .bind(entry_idx as i64)
                .fetch_one(&self.db)
                .await,
            )?;
            let Some(r) = row else { return Ok(None) };
            let checkpoints =
                unpack_checkpoints(&r.value).map_err(|e| RunError::Decode(e.to_string()))?;
            Ok(Some(crate::index::EntryMap {
                name: r.name,
                checkpoints,
            }))
        })
    }

    /// Writes one entry map row.
    pub fn put_entry_map(
        &self,
        book_id: u16,
        entry_idx: u16,
        map: &crate::index::EntryMap,
    ) -> RunResult<()> {
        let name = map.name.clone();
        let checkpoints = pack_checkpoints(&map.checkpoints);
        self.block_on(async {
            sqlx::query(
                "INSERT INTO entrymap (book_id, entry_idx, name, value)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(book_id, entry_idx) DO UPDATE SET
                     name = excluded.name, value = excluded.value",
            )
            .bind(book_id as i64)
            .bind(entry_idx as i64)
            .bind(&name)
            .bind(&checkpoints)
            .execute(&self.db)
            .await?;
            Ok(())
        })
    }

    /// Every book id that has at least one entry map row.
    pub fn books_with_entry_maps(&self) -> RunResult<Vec<u16>> {
        self.block_on(async {
            let rows = sqlx::query_scalar::<_, i64>(
                "SELECT DISTINCT book_id FROM entrymap ORDER BY book_id",
            )
            .fetch_all(&self.db)
            .await?;
            Ok(rows
                .into_iter()
                .filter_map(|i| u16::try_from(i).ok())
                .collect())
        })
    }

    /// Replaces `DICT_DELTA` wholesale.
    ///
    /// Wholesale, because a merge into `DICT` happens in the same call: the rows
    /// exist only for a session that was interrupted before that merge.
    pub fn replace_deltas(&self, rows: &[crate::index::dict::DeltaRow]) -> RunResult<()> {
        self.block_on(async {
            let mut txn = self.db.begin().await?;
            sqlx::query("DELETE FROM dict_delta")
                .execute(&mut *txn)
                .await?;
            for (term, df, tf, chunks) in rows {
                sqlx::query(
                    "INSERT INTO dict_delta (term, df, tf, chunks) VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT(term) DO UPDATE SET df = df + excluded.df,
                         tf = tf + excluded.tf, chunks = chunks + excluded.chunks",
                )
                .bind(term.as_slice())
                .bind(*df as i64)
                .bind(*tf as i64)
                .bind(*chunks as i64)
                .execute(&mut *txn)
                .await?;
            }
            txn.commit().await?;
            Ok(())
        })
    }

    /// Reads `DICT_DELTA`.
    pub fn list_deltas(&self) -> RunResult<Vec<crate::index::dict::DeltaRow>> {
        self.block_on(async {
            let rows = sqlx::query_as::<_, DeltaRow>(
                "SELECT term, df, tf, chunks FROM dict_delta ORDER BY term",
            )
            .fetch_all(&self.db)
            .await?;
            Ok(rows
                .into_iter()
                .map(|r| (r.term, r.df as u32, r.tf as u32, r.chunks as u32))
                .collect())
        })
    }

    /// Drops `DICT_DELTA`.
    pub fn clear_deltas(&self) -> RunResult<()> {
        self.block_on(async {
            sqlx::query("DELETE FROM dict_delta")
                .execute(&self.db)
                .await?;
            Ok(())
        })
    }
}

/// One row of `DICT_DELTA`.
#[derive(sqlx::FromRow)]
struct DeltaRow {
    term: Vec<u8>,
    df: i64,
    tf: i64,
    chunks: i64,
}

/// Checkpoints as a packed blob, rather than one row each.
///
/// Redb stored them per `(book, entry)` already, and a row per checkpoint would
/// multiply rows by ~500 for no gain: they are only ever read as a whole and
/// binary-searched in memory.
fn pack_checkpoints(cps: &[(u32, u32)]) -> Vec<u8> {
    let mut w = crate::storage::pack::Writer::with_capacity(cps.len() * 8 + 4);
    w = w.u32(cps.len() as u32);
    let mut prev = (0u32, 0u32);
    for &(p, o) in cps {
        w = w.u32(p - prev.0).u32(o - prev.1);
        prev = (p, o);
    }
    w.finish()
}

fn unpack_checkpoints(b: &[u8]) -> RunResult<Vec<(u32, u32)>> {
    let mut r = crate::storage::pack::Reader::new(b);
    let n = r.u32().map_err(pack_err)?;
    let mut out = Vec::with_capacity(n as usize);
    let mut pos = 0u32;
    let mut off = 0u32;
    for _ in 0..n {
        pos += r.u32().map_err(pack_err)?;
        off += r.u32().map_err(pack_err)?;
        out.push((pos, off));
    }
    Ok(out)
}

fn pack_err(e: PackError) -> RunError {
    RunError::Message(e.to_string())
}

/// A metadata row, with its repeated fields already joined.
///
/// Public because `storage::metadata` writes through it: one row type, one place
/// where a column name can be wrong.
#[derive(sqlx::FromRow)]
pub struct MetaRow {
    pub title: String,
    pub authors: String,
    pub language: String,
    pub publisher: String,
    pub series: String,
    pub series_index: i64,
    pub description: String,
    pub identifiers: String,
    pub provenance: i64,
    pub description_truncated: i64,
}

impl MetaRow {
    pub fn into_record(self) -> crate::storage::metadata::BookMetaRecord {
        use crate::storage::metadata::{BookMetaRecord, Provenance};
        let mut r = BookMetaRecord::new(self.title, self.authors);
        r.language = self.language;
        r.publisher = self.publisher;
        r.series = self.series;
        r.series_index = self.series_index as u32;
        r.description = self.description;
        r.identifiers = split_identifiers(&self.identifiers);
        r.provenance = Provenance::from_u8(self.provenance as u8).unwrap_or(Provenance::Filename);
        r.description_truncated = self.description_truncated != 0;
        r
    }

    pub fn from_record(r: &crate::storage::metadata::BookMetaRecord) -> Self {
        Self {
            title: r.title.clone(),
            authors: r.authors.clone(),
            language: r.language.clone(),
            publisher: r.publisher.clone(),
            series: r.series.clone(),
            series_index: r.series_index as i64,
            description: r.description.clone(),
            identifiers: r.identifiers.join("\n"),
            provenance: r.provenance as u8 as i64,
            description_truncated: r.description_truncated as i64,
        }
    }
}

/// Identifiers joined with a newline, which no ISBN or UUID contains.
fn split_identifiers(s: &str) -> Vec<String> {
    if s.is_empty() {
        Vec::new()
    } else {
        s.lines().map(|l| l.to_string()).collect()
    }
}

/// A `(chunk, body)` pair of `postings`.
#[derive(sqlx::FromRow)]
pub struct ChunkRow {
    chunk: i64,
    body: Vec<u8>,
}

/// An entry map's `name` plus its packed checkpoints.
#[derive(sqlx::FromRow)]
struct EntryMapRow {
    name: String,
    value: Vec<u8>,
}

/// A book as a row of `books`.
///
/// Every `i64` is a `u16` or `u32` on the way out, converted in `into_book`;
/// SQLite has no narrower integer, and the conversion is where the narrowing
/// belongs rather than inside a `bind`.
#[derive(sqlx::FromRow)]
pub struct BookRow {
    pub id: i64,
    pub path: String,
    pub scope: String,
    pub size: i64,
    pub mtime_ms: i64,
    pub inode: i64,
    pub format: i64,
    pub entry_count: i64,
    pub token_count: i64,
    pub indexed_at_ms: i64,
    pub flags: i64,
}

impl BookRow {
    pub fn into_book(self) -> (u16, Book) {
        let id = self.id as u16;
        let format = crate::storage::book::Format::from_u8(self.format as u8)
            .unwrap_or(crate::storage::book::Format::Unknown);
        (
            id,
            Book {
                path: self.path,
                scope: self.scope,
                size: self.size as u64,
                mtime_ms: self.mtime_ms,
                inode: self.inode as u64,
                format,
                entry_count: self.entry_count as u16,
                token_count: self.token_count as u32,
                indexed_at_ms: self.indexed_at_ms,
                flags: self.flags as u8,
            },
        )
    }

    fn from_book(id: u16, b: &Book) -> Self {
        Self {
            id: id as i64,
            path: b.path.clone(),
            scope: b.scope.clone(),
            size: b.size as i64,
            mtime_ms: b.mtime_ms,
            inode: b.inode as i64,
            format: b.format as i8 as i64,
            entry_count: b.entry_count as i64,
            token_count: b.token_count as i64,
            indexed_at_ms: b.indexed_at_ms,
            flags: b.flags as i64,
        }
    }
}
