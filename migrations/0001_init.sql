-- The turbosearch schema.
--
-- Single source of truth for the database layout. `src/sql/schema.rs` reads this
-- file at build time to create the compile-time database that sqlx's checked
-- macros verify against, so a column typo fails `cargo build` rather than the
-- first query that touches it.
--
-- The statements are idempotent (`IF NOT EXISTS`) because the same file is both
-- the migration for a fresh database and the reference the macros check.
--
-- Note that `books.path` is a UNIQUE INDEX, not part of the primary key: two
-- rows for one file is how a deleted book comes back.

CREATE TABLE IF NOT EXISTS meta (
    key   BLOB    NOT NULL PRIMARY KEY,
    value BLOB    NOT NULL
);

CREATE TABLE IF NOT EXISTS books (
    id            INTEGER NOT NULL PRIMARY KEY,
    path          TEXT    NOT NULL,
    scope         TEXT    NOT NULL DEFAULT '',
    size          INTEGER NOT NULL DEFAULT 0,
    mtime_ms      INTEGER NOT NULL DEFAULT 0,
    inode         INTEGER NOT NULL DEFAULT 0,
    format        INTEGER NOT NULL DEFAULT 0,
    entry_count   INTEGER NOT NULL DEFAULT 0,
    token_count   INTEGER NOT NULL DEFAULT 0,
    indexed_at_ms INTEGER NOT NULL DEFAULT 0,
    flags         INTEGER NOT NULL DEFAULT 0
);

CREATE UNIQUE INDEX IF NOT EXISTS books_path ON books (path);

CREATE TABLE IF NOT EXISTS metadata (
    book_id              INTEGER NOT NULL PRIMARY KEY,
    title                TEXT    NOT NULL DEFAULT '',
    authors             TEXT    NOT NULL DEFAULT '',
    language            TEXT    NOT NULL DEFAULT '',
    publisher           TEXT    NOT NULL DEFAULT '',
    series              TEXT    NOT NULL DEFAULT '',
    series_index        INTEGER NOT NULL DEFAULT 0,
    description         TEXT    NOT NULL DEFAULT '',
    identifiers         TEXT    NOT NULL DEFAULT '',
    provenance          INTEGER NOT NULL,
    description_truncated INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS metaidx (
    key   BLOB    NOT NULL PRIMARY KEY,
    value BLOB    NOT NULL
);

CREATE TABLE IF NOT EXISTS config (
    key   BLOB    NOT NULL PRIMARY KEY,
    value BLOB    NOT NULL
);

-- The only table the query path reads hard. `term` is the leading key so a
-- prefix query is an index range scan, which is what keeps a term's postings
-- cheap to read.
CREATE TABLE IF NOT EXISTS postings (
    term  TEXT    NOT NULL,
    chunk INTEGER NOT NULL,
    body  BLOB    NOT NULL,
    PRIMARY KEY (term, chunk)
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS dict (
    key   BLOB    NOT NULL PRIMARY KEY,
    value BLOB    NOT NULL
);

CREATE TABLE IF NOT EXISTS dict_delta (
    term   TEXT    NOT NULL PRIMARY KEY,
    df     INTEGER NOT NULL,
    tf     INTEGER NOT NULL,
    chunks INTEGER NOT NULL
);

-- One row per (book, section): the checkpoint list that turns a position back
-- into a byte offset. `value` is the packed checkpoint array.
CREATE TABLE IF NOT EXISTS entrymap (
    book_id   INTEGER NOT NULL,
    entry_idx INTEGER NOT NULL,
    name      TEXT    NOT NULL,
    value     BLOB    NOT NULL,
    PRIMARY KEY (book_id, entry_idx)
) WITHOUT ROWID;
