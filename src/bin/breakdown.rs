//! Breaks an index down by table, to see where its bytes are going.
//!
//! ```text
//! cargo run --release --bin breakdown -- <db_path>
//! ```
//!
//! The whole `POSTINGS` table is stored as zstd-framed chunks, so there is no
//! per-column length to read off disk: the on-disk bytes are one opaque frame per
//! row. This tool therefore reports what a reader actually pays for — value bytes
//! per table, rows per table, and what that implies for redb's page overhead —
//! and decodes the postings only to count them, because that denominator is what
//! every "bytes per posting" figure needs.
//!
//! The number this exists to expose is the gap between value bytes and file size.
//! redb's page size is fixed and its `Builder::set_page_size` is `#[cfg(test)]`,
//! so a value that does not fit a page costs a page regardless of how small it
//! is. When that gap dominates, no codec improvement will help.

use std::path::PathBuf;
use std::time::Instant;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = PathBuf::from(std::env::args().nth(1).expect("db path"));
    let engine = turbosearch::storage::Engine::open(&path)?;

    let started = Instant::now();
    let mut postings = 0u64;
    let mut frame_bytes = 0u64;
    let mut undecodable = 0u64;
    let mut chunk_rows = 0u64;
    let mut terms = std::collections::BTreeSet::new();

    // Walk the dictionary rather than the postings table: the engine has no
    // "iterate every term" accessor, and the dictionary is exactly the set of
    // terms that own postings.
    let dict = turbosearch::index::dict::load(&engine)?;
    for i in 0..dict.len() {
        let term = dict.term(i).to_vec();
        terms.insert(term.clone());
        for (_chunk, body) in engine.postings_term(&term)? {
            chunk_rows += 1;
            frame_bytes += body.len() as u64;
            match turbosearch::index::Chunk::decode(&body) {
                Some(c) => postings += c.postings.len() as u64,
                None => undecodable += 1,
            }
        }
    }

    println!(
        "postings table: {chunk_rows} chunks over {} terms, decoded in {:.1?}",
        terms.len(),
        started.elapsed()
    );
    println!(
        "  {postings} postings, {frame_bytes} bytes of frames = {:.2} B/posting",
        frame_bytes as f64 / postings.max(1) as f64
    );
    if undecodable > 0 {
        println!("  {undecodable} chunks failed to decode (invisible to search)");
    }

    report_tables(&engine, &path);
    Ok(())
}

fn report_tables(engine: &turbosearch::storage::Engine, db: &PathBuf) {
    println!("\nper table (value bytes + key bytes):");
    let mut grand = 0usize;

    // One query per table. `octet_length` is SQLite's own measurement of stored
    // bytes, and the payload column differs per table (`postings.body`,
    // `entrymap` carries both `name` and `value`), so each entry names its own.
    //
    // The SQL is a literal per table rather than a formatted string, because
    // sqlx's `raw_sql` only accepts `&str` — deliberately, since a formatted
    // statement is where injection would come from. Nine literals is cheaper
    // than a lint suppression.
    macro_rules! row {
        ($name:literal, $sql:literal) => {{
            let (rows, payload): (i64, i64) = engine.block_on(async {
                sqlx::query_as::<_, (i64, i64)>($sql)
                    .fetch_one(engine.pool())
                    .await
                    .unwrap_or((0, 0))
            });
            let bytes = payload as usize + rows as usize;
            println!(
                "  {:<12} {:>12} bytes ({:6.2} MB) in {:>7} rows, {:.0} B/row",
                $name,
                bytes,
                bytes as f64 / 1e6,
                rows,
                if rows > 0 {
                    bytes as f64 / rows as f64
                } else {
                    0.0
                }
            );
            grand += bytes;
        }};
    }

    row!(
        "postings",
        "SELECT COUNT(*), COALESCE(SUM(octet_length(body)),0) FROM postings"
    );
    row!(
        "entrymap",
        "SELECT COUNT(*), COALESCE(SUM(octet_length(name)+octet_length(value)),0) FROM entrymap"
    );
    row!(
        "dict",
        "SELECT COUNT(*), COALESCE(SUM(octet_length(value)),0) FROM dict"
    );
    row!(
        "dict_delta",
        "SELECT COUNT(*), COALESCE(SUM(octet_length(term)),0) FROM dict_delta"
    );
    row!(
        "books",
        "SELECT COUNT(*), COALESCE(SUM(octet_length(path)),0) FROM books"
    );
    row!(
        "metadata",
        "SELECT COUNT(*), COALESCE(SUM(octet_length(title)+octet_length(authors)),0) FROM metadata"
    );
    row!(
        "metaidx",
        "SELECT COUNT(*), COALESCE(SUM(octet_length(value)),0) FROM metaidx"
    );
    row!(
        "config",
        "SELECT COUNT(*), COALESCE(SUM(octet_length(value)),0) FROM config"
    );
    row!(
        "meta",
        "SELECT COUNT(*), COALESCE(SUM(octet_length(value)),0) FROM meta"
    );

    let file = std::fs::metadata(db).map(|m| m.len()).unwrap_or(0) as usize;
    println!(
        "  {:<12} {grand:>12} bytes ({:.2} MB)",
        "total",
        grand as f64 / 1e6
    );
    println!(
        "  {:<12} {file:>12} bytes ({:.2} MB)",
        "file size",
        file as f64 / 1e6
    );
    if file > 0 {
        // The point of the whole tool: how much of the file is data.
        println!(
            "  {:<12} {:>11.1}% of the file; {:.1}% is page overhead",
            "value bytes",
            grand as f64 * 100.0 / file as f64,
            (file.saturating_sub(grand)) as f64 * 100.0 / file as f64
        );
    }
}
