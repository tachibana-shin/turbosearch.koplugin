//! Table definitions and the [`Engine`] facade.
//!

use std::path::{Path, PathBuf};

use redb::{Database, ReadableDatabase, ReadableTable};

use crate::storage::pack::PackError;

pub mod book;
pub mod config;
pub mod meta;
pub mod metadata;
pub mod pack;

pub use book::{
    Book, Coverage, FileFilter, Format, ScanMode, ScanPlan, ScopeFilter, path_in_scope, scan_full,
    scan_scope, scan_with_mode,
};
pub use config::{Config, IndexDepth, PrefixMode};
pub use meta::{Meta, Status};
pub use metadata::{
    BookMetaRecord, METAIDX_KEY, MetaIndex, Provenance, rebuild_index as rebuild_meta_index,
    search as search_metadata,
};

pub use pack::{Reader, Writer as PackWriter};

/// Book count ceiling from `note.md` section 2: `book_id` is `u16`.
pub const MAX_BOOKS: u32 = u16::MAX as u32 + 1;

pub const TABLE_META: &str = "meta";
pub const TABLE_BOOKS: &str = "books";
pub const TABLE_PATHS: &str = "paths";
pub const TABLE_CONFIG: &str = "config";

/// Index tables, declared here for schema stability but used
pub const TABLE_METADATA: &str = "metadata";
pub const TABLE_METAIDX: &str = "metaidx";
pub const TABLE_DICT: &str = "dict";
pub const TABLE_DICT_DELTA: &str = "dict_delta";
pub const TABLE_POSTINGS: &str = "postings";
pub const TABLE_ENTRYMAP: &str = "entrymap";

/// Error type for closures that touch several tables at once.
///
/// `redb`'s own errors all convert into this, so a scan closure can call any
/// table accessor with a single `?`.
#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error("redb: {0}")]
    Redb(#[from] redb::Error),
    #[error("redb table: {0}")]
    Table(#[from] redb::TableError),
    #[error("redb storage: {0}")]
    Storage(#[from] redb::StorageError),
    #[error("redb transaction: {0}")]
    Transaction(#[from] redb::TransactionError),
    #[error("redb commit: {0}")]
    Commit(#[from] redb::CommitError),
    #[error("redb compaction: {0}")]
    Compaction(#[from] redb::CompactionError),
    #[error("packed data: {0}")]
    Pack(#[from] PackError),
    #[error("redb durability: {0}")]
    Durability(#[from] redb::SetDurabilityError),
    #[error("{0}")]
    Message(String),
}

/// Alias used throughout the storage layer.
pub type RunResult<T> = std::result::Result<T, RunError>;

/// Treats a table that has never been written as empty.
///
/// redb reports `TableDoesNotExist` when a read opens a table no transaction has
/// created yet. That is the normal state of a freshly opened database and of a
/// database where the user never enabled indexing, so every read path funnels
/// through here instead of duplicating the check.
pub fn or_empty<T>(r: std::result::Result<T, redb::TableError>) -> RunResult<Option<T>> {
    match r {
        Ok(v) => Ok(Some(v)),
        Err(redb::TableError::TableDoesNotExist(..)) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Owned the database file and provides transaction helpers.
pub struct Engine {
    db: Database,
    path: PathBuf,
}

impl Engine {
    /// Opens (creating if needed) the database at `path`.
    pub fn open(path: impl AsRef<Path>) -> std::result::Result<Self, redb::DatabaseError> {
        let path = path.as_ref().to_path_buf();
        let db = redb::Builder::new()
            .set_cache_size(Self::CACHE_SIZE)
            .create(&path)?;
        Ok(Self { db, path })
    }

    /// Memory the page cache may use.
    ///
    /// redb defaults to 1 GiB, which on a 512 MB Kindle would thrash. Note that
    /// the indexer only touches a few hundred pages of term dictionary at a time
    /// and the query path touches a handful of postings blocks, so 8 MiB is ample.
    pub const CACHE_SIZE: usize = 8 * 1024 * 1024;

    pub fn db(&self) -> &Database {
        &self.db
    }

    /// Path of the underlying file, for diagnostics.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Runs a closure inside a read transaction.
    pub fn read<R>(&self, f: impl FnOnce(&redb::ReadTransaction) -> RunResult<R>) -> RunResult<R> {
        let txn = self.db.begin_read()?;
        f(&txn)
    }

    /// Runs a closure inside a write transaction, committing it on success and
    /// rolling back if the closure returns an error.
    pub fn write<R>(
        &self,
        f: impl FnOnce(&mut redb::WriteTransaction) -> RunResult<R>,
    ) -> RunResult<R> {
        let mut txn = self.db.begin_write()?;
        let out = f(&mut txn)?;
        txn.commit()?;
        Ok(out)
    }

    /// Runs a closure inside a **non-durable** write transaction, and flushes it
    /// only when asked.
    ///
    /// This exists because a bulk index build writes one row per section, and
    /// each `write` above costs an `fsync`. Measured on a real disk: a full
    /// library took 34 minutes and stalled after 105 MB, against about nine
    /// minutes for the same run writing to tmpfs, where the same fsync is a
    /// no-op. On the device the storage is a real eMMC, so this is the number
    /// that matters.
    ///
    /// `Durability::None` defers the flush until a later commit, so a session
    /// can write as many transactions as it likes and pay for one sync at the
    /// end. The caller must finish with [`Engine::flush`], or the work is lost.
    pub fn write_batch<R>(
        &self,
        f: impl FnOnce(&mut redb::WriteTransaction) -> RunResult<R>,
    ) -> RunResult<R> {
        let mut txn = self.db.begin_write()?;
        txn.set_durability(redb::Durability::None)?;
        let out = f(&mut txn)?;
        txn.commit()?;
        Ok(out)
    }

    /// Persists every non-durable commit, returning whether anything was written.
    pub fn flush(&self) -> RunResult<bool> {
        let txn = self.db.begin_write()?;
        // An empty durable commit is what forces the deferred writes out.
        txn.commit()?;
        Ok(true)
    }

    /// Reads the `META` row, or `None` when the file was never initialised.
    pub fn meta(&self) -> RunResult<Option<Meta>> {
        let txn = self.db.begin_read()?;
        let Some(table) = or_empty(txn.open_table(meta_table()))? else {
            return Ok(None);
        };
        Ok(table
            .get(Meta::KEY)?
            .map(|v| Meta::decode(v.value()))
            .transpose()?)
    }

    /// Writes the `META` row, creating the table on first use.
    pub fn write_meta(&self, meta: &Meta) -> RunResult<()> {
        let bytes = meta.encode();
        self.write(|txn| {
            txn.open_table(meta_table())?
                .insert(Meta::KEY, bytes.as_slice())?;
            Ok(())
        })
    }

    /// Inserts or replaces one `BOOKS` row and its `PATHS` back-reference.
    ///
    /// Both writes share one transaction so the two tables can never disagree.
    pub fn put_book(&self, id: u16, b: &Book) -> RunResult<()> {
        let bytes = b.encode();
        let path = b.path.clone();
        self.write(|txn| {
            txn.open_table(book::books_table())?
                .insert(id, bytes.as_slice())?;
            txn.open_table(book::paths_table())?
                .insert(path.as_str(), id)?;
            Ok(())
        })
    }

    /// Drops a `BOOKS` row and its `PATHS` entry, orphaning its postings.
    ///
    /// Deletion is O(1) precisely because we do not touch the postings here:
    /// `note.md` section 0, row 5. The postings become orphans that the read path
    /// drops.
    ///
    /// `METADATA` IS removed, because unlike postings it is not cheaply filterable
    /// at read time: `METAIDX` is rebuilt from the `METADATA` rows, so a row left
    /// behind resurrects the book in every metadata search forever. Real bug,
    /// caught by `tests/metadata_search.rs::removing_a_book_drops_it_from_the_index`.
    pub fn remove_book(&self, id: u16) -> RunResult<Option<Book>> {
        let existing = self.book(id)?;
        if let Some(b) = &existing {
            let path = b.path.clone();
            self.write(|txn| {
                txn.open_table(book::books_table())?.remove(id)?;
                txn.open_table(book::paths_table())?.remove(path.as_str())?;
                txn.open_table(metadata::metadata_table())?.remove(id)?;
                Ok(())
            })?;
            // The book's snippets must go too, or a cached posting could still
            // resolve to text the reader is no longer allowed to see.
            crate::index::writer::remove_entry_maps(self, id)?;
        }
        Ok(existing)
    }

    /// Allocates `n` fresh `book_id`s, advancing the high-water mark.
    ///
    /// Ids are never reused, which is what makes deleting a book O(1): the old
    /// postings simply become orphans that the read path drops.
    ///
    /// Returns `None` when the id space is exhausted, which the caller should
    /// surface as "rebuild required". An empty request returns `Some(vec![])`.
    pub fn allocate_ids(&self, n: u32) -> RunResult<Option<Vec<u16>>> {
        if n == 0 {
            return Ok(Some(Vec::new()));
        }
        self.write(|txn| {
            let mut table = txn.open_table(meta_table())?;
            let mut cur = match table.get(Meta::KEY)? {
                Some(v) => Meta::decode(v.value())?,
                None => Meta::new(),
            };
            let last = match cur.book_id_hwm.checked_add(n) {
                Some(l) if l <= cur.book_id_limit => l,
                _ => return Ok(None),
            };
            let ids: Vec<u16> = (cur.book_id_hwm..last).map(|i| i as u16).collect();
            cur.book_id_hwm = last;
            let bytes = cur.encode();
            table.insert(Meta::KEY, bytes.as_slice())?;
            Ok(Some(ids))
        })
    }

    /// Compacts the file, reclaiming space from obsolete pages.
    ///
    /// Returns `true` if pages were actually freed. Call this once the orphan
    /// postings exceed ~25% of the file, per `note.md` section 3.4 L6.
    pub fn compact(&mut self) -> RunResult<bool> {
        Ok(self.db.compact()?)
    }

    /// Approximate on-disk size in bytes, including free pages.
    pub fn file_size(&self) -> u64 {
        std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0)
    }
}

/// The `META` table: exactly one row, keyed by [`Meta::KEY`].
pub fn meta_table() -> redb::TableDefinition<'static, &'static str, &'static [u8]> {
    redb::TableDefinition::new(TABLE_META)
}

/// The `DICT` table: exactly one row, keyed by [`crate::index::dict::DICT_KEY`].
pub fn dict_table() -> redb::TableDefinition<'static, &'static [u8], &'static [u8]> {
    redb::TableDefinition::new(TABLE_DICT)
}

/// The `DICT_DELTA` table: one row per term touched in the running session.
pub fn delta_table() -> redb::TableDefinition<'static, &'static [u8], &'static [u8]> {
    redb::TableDefinition::new(TABLE_DICT_DELTA)
}

/// The `POSTINGS` table: rows keyed by `(term, chunk_idx)`.
pub fn postings_table() -> redb::TableDefinition<'static, (&'static [u8], u16), &'static [u8]> {
    redb::TableDefinition::new(TABLE_POSTINGS)
}

/// The `ENTRYMAP` table: rows keyed by `(book_id, entry_idx)`.
pub fn entrymap_table() -> redb::TableDefinition<'static, (u16, u16), &'static [u8]> {
    redb::TableDefinition::new(TABLE_ENTRYMAP)
}
