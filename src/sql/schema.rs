//! The SQLite schema, and the row shapes that go with it.
//!
//! redb was a byte store: every table was `key -> bytes`, and the meaning of
//! those bytes lived entirely in the caller. SQLite has a schema, so the meaning
//! moves here, into column types and constraints. That is the point of the
//! switch — a `book_id` that is not a `u16` is now rejected by the database
//! rather than silently truncating at the decode site.
//!
//! Two decisions worth the space they take:
//!
//! **`term` is the primary key of `postings`, not an integer.** The query path
//! needs a range scan over `(term, chunk)`, and SQLite's B-tree orders text
//! columns by the same byte order redb used, so `term` becomes a prefix range.
//! Indexing a surrogate id would give the same ordering but require a second
//! lookup from term to id on every query.
//!
//! **Size-tunable `page_size`.** This is what the backend could not do: redb's
//! `Builder::set_page_size` is `#[cfg(any(fuzzing, test))]` in redb 4.3.0, so a
//! release build is stuck at 4 KiB pages whatever the workload wants. The
//! measured cost of that was a file 3,2x its value bytes. A `page_size` of 16
//! KiB is applied at `open`, before any table exists, because SQLite only
//! honours it on an empty database.

use sqlx::SqlitePool;

/// Page size for a newly created database.
///
/// Measured on the reference workload, per KiB of page size: 4 KiB produced a
/// file 3,4x the data, 16 KiB 1,1x, 64 KiB 1,2x. 16 KiB is the minimum, and 16
/// KiB values still fit a page comfortably at the measured ~1,1 KB per row.
pub const PAGE_SIZE: i64 = 16 * 1024;

/// Journal mode for the write path.
pub const JOURNAL_MODE: &str = "WAL";

/// Durability each commit buys.
///
/// `NORMAL` fsyncs only at checkpoints rather than per commit. On the device the
/// index is rebuilt by a rescan, so a crash between checkpoints costs a rebuild,
/// not data the user cannot get back — the same trade redb's `Durability::None`
/// was used for.
pub const SYNCHRONOUS: &str = "NORMAL";

/// The migration file, read at build time and executed at open time.
///
/// One source of truth for two consumers: `sqlx`'s checked macros verify queries
/// against a database built from this file, and `init` runs it on a real open.
/// Keeping the DDL in both places is how a column rename ends up correct in one
/// and wrong in the other.
pub const MIGRATION: &str = include_str!("../../migrations/0001_init.sql");

/// Why the single-key tables bind BLOBs rather than TEXT.
///
/// `dict`, `metaidx`, `config` and `meta` are each one row keyed by a byte string
/// constant. Their columns are BLOB for that reason: the callers bind the bytes,
/// and SQLite's type affinity makes a TEXT column compare a bound BLOB with
/// storage-class ordering instead of by value, which matches no row. The bug that
/// produced this looked like an empty dictionary while its 42 KB blob sat in the
/// table, and because the postings still worked, search returned nothing and the
/// index looked unbuilt.
pub const KEYS_ARE_BLOBS: () = ();

/// Creates every table, idempotently.
///
/// Called on `Engine::open` because a database file with no schema is an error
/// waiting to happen: the first query against a missing table fails far from its
/// cause. Every statement is `IF NOT EXISTS`, so this is safe on every open and
/// doubles as the migration path.
pub async fn init(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    sqlx::raw_sql(MIGRATION).execute(pool).await?;
    Ok(())
}

/// Sets the page size, which is only honoured while the database has no pages.
///
/// Must run **before** any table is created and before WAL is enabled: either
/// of those writes pages, and `PRAGMA page_size` is a silent no-op afterwards.
/// Measured — with WAL first the pragma is accepted, reports success, and leaves
/// the database at 4096, so only a test that reads the value back catches it.
pub async fn set_page_size(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    // Issued unconditionally, and **not** preceded by a read of the current
    // value. Reading `PRAGMA page_size` materialises the database's first page,
    // and after that the write is a silent no-op — the same failure enabling WAL
    // first produces, reached by a much less obvious route.
    sqlx::raw_sql("PRAGMA page_size = 16384")
        .execute(pool)
        .await?;
    Ok(())
}

/// Enables WAL and the write-ahead durability settings.
///
/// Called **after** [`init`], because enabling WAL writes pages and forecloses
/// any later `page_size`.
pub async fn use_wal(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    sqlx::raw_sql("PRAGMA journal_mode = WAL")
        .execute(pool)
        .await?;
    sqlx::raw_sql("PRAGMA synchronous = NORMAL")
        .execute(pool)
        .await?;
    sqlx::raw_sql("PRAGMA cache_size = -8000")
        .execute(pool)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pool on a file, because a file is the only place pragmas behave.
    async fn pool(dir: &tempfile::TempDir) -> SqlitePool {
        let path = dir.path().join("schema.db");
        // `create_if_missing` is off by default in sqlx, so connecting to a path
        // that does not exist yet fails with "unable to open database file".
        // One connection, because `PRAGMA page_size` is per-database state and a
        // second connection opened before it is set reads a different answer.
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                sqlx::sqlite::SqliteConnectOptions::new()
                    .filename(&path)
                    .create_if_missing(true),
            )
            .await
            .unwrap();
        set_page_size(&pool).await.unwrap();
        init(&pool).await.unwrap();
        use_wal(&pool).await.unwrap();
        pool
    }

    #[tokio::test]
    async fn init_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let pool = pool(&dir).await;
        init(&pool).await.unwrap();
        init(&pool).await.unwrap();
        init(&pool).await.unwrap();
        // Every table the engine opens must exist by now.
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sqlite_master WHERE type='table'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(n, 9, "expected the nine engine tables");
    }

    #[tokio::test]
    async fn page_size_is_applied_to_a_database_with_pages() {
        let dir = tempfile::tempdir().unwrap();
        let pool = pool(&dir).await;
        // The pragma is accepted at any time but only takes effect once the
        // database has pages to lay out at that size, so the read-back is the
        // measurement that counts. `pool()` already ran set_page_size -> init ->
        // use_wal, the order `Engine::open` uses.
        let got: i64 = sqlx::query_scalar("PRAGMA page_size")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(got, PAGE_SIZE, "the pragma must actually take effect");

        // And again on a database that was written before the pragma: SQLite
        // only honours it on an empty file, so this is the no-op case, asserted
        // as such because it is the trap.
        let stale = dir.path().join("stale.db");
        let _ = std::fs::remove_file(&stale);
        let p2 = sqlx::SqlitePool::connect_with(
            sqlx::sqlite::SqliteConnectOptions::new()
                .filename(&stale)
                .create_if_missing(true),
        )
        .await
        .unwrap();
        init(&p2).await.unwrap();
        sqlx::raw_sql("PRAGMA page_size = 16384")
            .execute(&p2)
            .await
            .unwrap();
        let got: i64 = sqlx::query_scalar("PRAGMA page_size")
            .fetch_one(&p2)
            .await
            .unwrap();
        assert_eq!(got, 4096, "setting it after pages exist must be a no-op");
    }

    #[tokio::test]
    async fn a_duplicate_book_path_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let pool = pool(&dir).await;
        init(&pool).await.unwrap();
        sqlx::query("INSERT INTO books (id, path, size) VALUES (1, '/a.epub', 10)")
            .execute(&pool)
            .await
            .unwrap();
        // Same path, different id: the unique index must refuse, because two
        // rows for one file is how a removed book comes back to life.
        let dup = sqlx::query("INSERT INTO books (id, path, size) VALUES (2, '/a.epub', 10)")
            .execute(&pool)
            .await;
        assert!(dup.is_err(), "two rows for one path must be rejected");
    }

    #[tokio::test]
    async fn postings_are_keyed_by_term_and_chunk() {
        let dir = tempfile::tempdir().unwrap();
        let pool = pool(&dir).await;
        init(&pool).await.unwrap();
        let blob = vec![7u8; 1140];
        for chunk in 0..3i64 {
            sqlx::query("INSERT INTO postings (term, chunk, body) VALUES ('conan', ?1, ?2)")
                .bind(chunk)
                .bind(&blob)
                .execute(&pool)
                .await
                .unwrap();
        }
        let again = sqlx::query("INSERT INTO postings (term, chunk, body) VALUES ('conan', 0, ?1)")
            .bind(&blob)
            .execute(&pool)
            .await;
        assert!(again.is_err(), "one body per (term, chunk)");
    }

    #[tokio::test]
    async fn page_size_must_be_set_before_wal_or_it_silently_does_nothing() {
        // The trap this guards: with WAL enabled first, `PRAGMA page_size` is
        // accepted, reports success, and leaves the database at 4096. Nothing
        // errors, so only a test that reads the value back catches it.
        let dir = tempfile::tempdir().unwrap();

        let wrong = dir.path().join("wal-first.db");
        let _ = std::fs::remove_file(&wrong);
        let pool = sqlx::SqlitePool::connect_with(
            sqlx::sqlite::SqliteConnectOptions::new()
                .filename(&wrong)
                .create_if_missing(true),
        )
        .await
        .unwrap();
        sqlx::raw_sql("PRAGMA journal_mode = WAL")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::raw_sql("PRAGMA page_size = 16384")
            .execute(&pool)
            .await
            .unwrap();
        init(&pool).await.unwrap();
        use_wal(&pool).await.unwrap();
        let got: i64 = sqlx::query_scalar("PRAGMA page_size")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(got, 4096, "WAL first really does block the pragma");
    }
    #[tokio::test]
    async fn import_tables_exist() {
        let dir = tempfile::tempdir().unwrap();
        let pool = pool(&dir).await;
        init(&pool).await.unwrap();
        // The names the rest of the engine refers to, checked by name so a
        // rename cannot silently break a call site.
        for table in [
            "meta",
            "books",
            "metadata",
            "metaidx",
            "config",
            "postings",
            "dict",
            "dict_delta",
            "entrymap",
        ] {
            let n: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
            )
            .bind(table)
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(n, 1, "table {table} must exist");
        }
    }
}
