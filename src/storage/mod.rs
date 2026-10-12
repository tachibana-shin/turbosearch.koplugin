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
