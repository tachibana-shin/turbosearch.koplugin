//! Compares redb and SQLite on the workload the index actually performs.
//!
//! ```text
//! cargo run --release --bin dbbench -- [rows]
//! ```
//!
//! The question this answers is whether switching the backend would make the
//! index smaller or faster, and the honest way to answer it is to run the same
//! data through both. The workload matters more than the raw numbers: rows of
//! roughly 1 KiB (the measured per-chunk size), written in many separate
//! transactions, which is what an indexing session does.
//!
//! Both backends are compared on three things that each pull in a different
//! direction:
//! - **file size**, because the index budget is a hard constraint;
//! - **write time**, because a full build is minutes rather than seconds;
//! - **read time**, because that is what a query pays per term.
//!
//! SQLite is only in the dev-dependencies: this is a decision tool, not a
//! dependency the shipped engine takes on.

use std::path::{Path, PathBuf};
use std::time::Instant;

use redb::ReadableDatabase;

/// Bytes per row, matching the measured average chunk size.
const ROW_BYTES: usize = 1140;

/// Transactions to spread the insert over, matching the chunk-per-term pattern.
const TXN_EVERY: usize = 100;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let rows: usize = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(50_000);

    let dir: std::path::PathBuf = std::env::var("BENCH_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir().join("dbbench"));
    let _ = std::fs::create_dir_all(&dir);
    println!(
        "workload: {rows} rows x {ROW_BYTES} bytes = {:.1} MB",
        rows * ROW_BYTES / 1_000_000
    );

    // One deterministic payload, so both backends see identical bytes.
    let payload = make_payload(ROW_BYTES);
    assert_eq!(payload.len(), ROW_BYTES);

    bench_sqlite(&dir.join("s.db"), rows, &payload, 16384)?;
    bench_redb(&dir.join("r.redb"), rows, &payload)?;

    if std::env::var("CHURN").is_ok() {
        println!("\n-- one transaction per row (the real indexing pattern) --");
        let rows = rows.min(20_000);
        bench_churn_sqlite(&dir.join("cs.db"), rows, &payload, 16384)?;
        bench_churn_redb(&dir.join("cr.redb"), rows, &payload)?;
    }
    Ok(())
}

/// A block that compresses the way a real chunk does, so both backends see a
/// value that is not trivially compressible to nothing.
/// `(term, chunk)` for row `n`, matching the real term-to-chunk ratio.
fn key_for(n: usize) -> (String, u16) {
    // Injective, with about two chunks per term. The real ratio is 1.14 chunks
    // per term, but what matters for the B-tree shape is that keys are distinct
    // and spread over tens of thousands of terms; the exact ratio moves very
    // little. Getting this wrong produced duplicate keys and the very first
    // insert failed.
    (format!("term{}", n / 2), (n % 2) as u16)
}

fn make_payload(len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    let mut state: u32 = 0x1234_5678;
    while out.len() < len {
        // xorshift: cheap, deterministic, and not compressible by zstd.
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        out.extend_from_slice(&state.to_le_bytes());
    }
    out.truncate(len);
    out
}

/// One transaction per row, which is the write pattern of a real indexing
/// session and the case where fragmentation dominates.
fn bench_churn_sqlite(
    path: &Path,
    rows: usize,
    payload: &[u8],
    page_size: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    let _ = std::fs::remove_file(path);
    let conn = rusqlite::Connection::open(path)?;
    conn.pragma_update(None, "page_size", page_size as i64)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.execute_batch(
        "CREATE TABLE postings (term TEXT, chunk INTEGER, body BLOB, PRIMARY KEY (term, chunk)) WITHOUT ROWID",
    )?;

    let t = Instant::now();
    let mut n = 0usize;
    while n < rows {
        conn.execute("BEGIN IMMEDIATE", [])?;
        let (term, chunk) = key_for(n);
        conn.execute(
            "INSERT INTO postings (term, chunk, body) VALUES (?1, ?2, ?3)",
            rusqlite::params![term, chunk as i64, payload],
        )?;
        conn.execute("COMMIT", [])?;
        n += 1;
    }
    let write_s = t.elapsed().as_secs_f64();
    conn.pragma_update(None, "wal_checkpoint", "TRUNCATE")?;
    let size = std::fs::metadata(path)?.len() as usize + wal_size(path);
    let _ = conn.close();
    println!(
        "sqlite churn  p={page_size:<6} {:>7.2} MB in {:>6.2} s  ({:.1} MB/s)   [1 txn per row]",
        size as f64 / 1e6,
        write_s,
        rows as f64 * payload.len() as f64 / 1e6 / write_s
    );
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(wal_path(path));
    Ok(())
}

fn bench_churn_redb(
    path: &Path,
    rows: usize,
    payload: &[u8],
) -> Result<(), Box<dyn std::error::Error>> {
    let _ = std::fs::remove_file(path);
    let mut db = redb::Builder::new()
        .set_cache_size(8 * 1024 * 1024)
        .create(path)?;
    let t = Instant::now();
    for n in 0..rows {
        let (term, chunk) = key_for(n);
        let txn = db.begin_write()?;
        {
            let mut table = txn.open_table(postings_table())?;
            table.insert((term.as_str(), chunk), payload)?;
        }
        txn.commit()?;
    }
    let write_s = t.elapsed().as_secs_f64();
    let pre = std::fs::metadata(path)?.len() as usize;
    let _ = db.compact()?;
    let size = std::fs::metadata(path)?.len() as usize;
    println!(
        "redb   churn          {:>7.2} MB in {:>6.2} s  ({:.1} MB/s)   [1 txn per row]",
        size as f64 / 1e6,
        write_s,
        rows as f64 * payload.len() as f64 / 1e6 / write_s
    );
    println!("       compaction: {pre} -> {size} bytes");
    drop(db);
    let _ = std::fs::remove_file(path);
    Ok(())
}

fn bench_sqlite(
    path: &Path,
    rows: usize,
    payload: &[u8],
    page_size: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    let _ = std::fs::remove_file(path);

    let t = Instant::now();
    let conn = rusqlite::Connection::open(path)?;
    // The knobs SQLite gives that redb does not: page size, and how much
    // durability each transaction buys.
    conn.pragma_update(None, "page_size", page_size as i64)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "cache_size", -8000)?;
    conn.execute_batch("CREATE TABLE postings (term TEXT, chunk INTEGER, body BLOB, PRIMARY KEY (term, chunk)) WITHOUT ROWID")?;

    let mut written = 0usize;
    let mut n = 0usize;
    while n < rows {
        conn.execute("BEGIN IMMEDIATE", [])?;
        for _ in 0..TXN_EVERY.min(rows - n) {
            let term = format!("term{}", n / 8000);
            conn.execute(
                "INSERT INTO postings (term, chunk, body) VALUES (?1, ?2, ?3)",
                rusqlite::params![term, (n % 8000) as i64, payload],
            )?;
            n += 1;
            written += payload.len();
        }
        conn.execute("COMMIT", [])?;
    }
    let write_s = t.elapsed().as_secs_f64();

    let t = Instant::now();
    let probe_term = format!("term{}", (rows / 2) / 2);
    let hits: i64 = conn.query_row(
        "SELECT COUNT(*) FROM postings WHERE term = ?1",
        rusqlite::params![&probe_term],
        |r| r.get(0),
    )?;
    let read_s = t.elapsed().as_secs_f64();

    // The WAL holds everything not yet checkpointed, so without this the size
    // reflects buffering rather than what the query path later reads.
    conn.pragma_update(None, "wal_checkpoint", "TRUNCATE")?;
    let size = std::fs::metadata(path)?.len() as usize + wal_size(path);
    let _ = conn.close();
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(wal_path(path));

    println!(
        "sqlite page={page_size:<6} {:>7.2} MB in {:>6.2} s ({:>4.1} MB/s), read {hits} rows in {read_s:.4} s",
        size as f64 / 1e6,
        write_s,
        written as f64 / 1e6 / write_s
    );
    Ok(())
}

fn bench_redb(path: &Path, rows: usize, payload: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
    let _ = std::fs::remove_file(path);

    let mut db = redb::Builder::new()
        .set_cache_size(8 * 1024 * 1024)
        .create(path)?;
    let t = Instant::now();

    let mut n = 0usize;
    let mut written = 0usize;
    while n < rows {
        let txn = db.begin_write()?;
        {
            let mut table = txn.open_table(postings_table())?;
            for _ in 0..TXN_EVERY.min(rows - n) {
                let (term, chunk) = key_for(n);
                table.insert((term.as_str(), chunk), payload)?;
                n += 1;
                written += payload.len();
            }
        }
        txn.commit()?;
    }
    let write_s = t.elapsed().as_secs_f64();

    let t = Instant::now();
    let txn = db.begin_read()?;
    let table = txn.open_table(postings_table())?;
    // A range scan over the key prefix, which is what `term_postings` does.
    // Scanning the whole table would measure the bug that was already fixed,
    // and the comparison would be meaningless.
    let probe = format!("term{}", (rows / 2) / 2);
    let mut hits = 0usize;
    let start = (probe.as_str(), u16::MIN);
    let end = (probe.as_str(), u16::MAX);
    for _row in table.range(start..=end)? {
        hits += 1;
    }
    let read_s = t.elapsed().as_secs_f64();
    // Drop the read snapshot before compacting: holding one pins the old pages
    // and the compact is refused, which would silently measure the wrong file.
    drop(table);
    drop(txn);
    let pre_compact = std::fs::metadata(path)?.len() as usize;
    let _ = db.compact()?;
    let size = std::fs::metadata(path)?.len() as usize;
    drop(db);

    println!(
        "redb   (compacted)   {:>7.2} MB in {:>6.2} s ({:>4.1} MB/s), read {hits} rows in {read_s:.4} s",
        size as f64 / 1e6,
        write_s,
        written as f64 / 1e6 / write_s
    );
    println!(
        "       (pre-compact)  {:>7.2} MB, so compaction {} it",
        pre_compact as f64 / 1e6,
        if pre_compact > size {
            "shrinks"
        } else {
            "leaves"
        }
    );
    let _ = std::fs::remove_file(path);
    Ok(())
}

fn postings_table() -> redb::TableDefinition<'static, (&'static str, u16), &'static [u8]> {
    redb::TableDefinition::new("postings")
}

fn wal_path(p: &Path) -> PathBuf {
    let mut s = p.as_os_str().to_os_string();
    s.push("-wal");
    PathBuf::from(s)
}

fn wal_size(p: &Path) -> usize {
    std::fs::metadata(wal_path(p))
        .map(|m| m.len() as usize)
        .unwrap_or(0)
}
