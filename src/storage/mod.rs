//! The storage layer: scanning, configuration, metadata, and the engine facade.
//!
//! The engine itself lives in [`crate::sql`] and is re-exported here, so callers
//! keep writing `storage::Engine`. What used to live here — redb table
//! definitions and the transaction API — is gone with the backend.

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

/// The nine table names the engine opens.
///
/// Declared here rather than at each call site because `sql::schema::MIGRATION`
/// is the file that actually creates them, and this list is what a reader
/// checks it against.
pub const TABLES: [&str; 9] = [
    "meta",
    "books",
    "metadata",
    "metaidx",
    "config",
    "postings",
    "dict",
    "dict_delta",
    "entrymap",
];

/// The error and result alias live with the engine now, because SQLite's error
/// type is what every accessor actually returns:
///
/// ```ignore
/// pub use crate::sql::engine::{Engine, RunError, RunResult};
/// ```
///
/// They are still exported from `storage`, so callers that wrote
/// `storage::RunResult` keep compiling.
pub use crate::sql::engine::{Engine, RunError, RunResult};
