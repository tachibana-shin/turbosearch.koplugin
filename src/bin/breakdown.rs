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

use redb::ReadableTable;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = PathBuf::from(std::env::args().nth(1).expect("db path"));
    let engine = turbosearch::storage::Engine::open(&path)?;

    let started = Instant::now();
    let mut postings = 0u64;
    let mut frame_bytes = 0u64;
    let mut undecodable = 0u64;
    let mut chunk_rows = 0u64;
    let mut terms = std::collections::BTreeSet::new();

    engine.read(|txn| {
        let t = match turbosearch::storage::or_empty(
            txn.open_table(turbosearch::storage::postings_table()),
        )? {
            Some(t) => t,
            None => return Ok(()),
        };
        for row in t.iter()? {
            let (k, v) = row?;
            chunk_rows += 1;
            terms.insert(k.value().0.to_vec());
            frame_bytes += v.value().len() as u64;
            match turbosearch::index::Chunk::decode(v.value()) {
                Some(c) => postings += c.postings.len() as u64,
                None => undecodable += 1,
            }
        }
        Ok(())
    })?;

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

    macro_rules! row {
        ($name:literal, $def:expr, $key:expr) => {{
            let (bytes, rows) = engine
                .read(|txn| {
                    let t = match turbosearch::storage::or_empty(txn.open_table($def))? {
                        Some(t) => t,
                        None => return Ok((0usize, 0usize)),
                    };
                    let mut bytes = 0usize;
                    let mut rows = 0usize;
                    for row in t.iter()? {
                        let (k, v) = row?;
                        bytes += v.value().len() + $key(&k.value());
                        rows += 1;
                    }
                    Ok((bytes, rows))
                })
                .unwrap_or((0, 0));
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
        turbosearch::storage::postings_table(),
        |k: &(&[u8], u16)| k.0.len() + 2
    );
    row!(
        "entrymap",
        turbosearch::storage::entrymap_table(),
        |_: &(u16, u16)| 4
    );
    row!("dict", turbosearch::storage::dict_table(), |_: &&[u8]| 3);
    row!(
        "dict_delta",
        turbosearch::storage::delta_table(),
        |_: &&[u8]| 0
    );
    row!(
        "books",
        turbosearch::storage::book::books_table(),
        |_: &u16| 2
    );
    row!(
        "metadata",
        turbosearch::storage::metadata::metadata_table(),
        |_: &u16| 2
    );
    row!(
        "metaidx",
        turbosearch::storage::metadata::metaidx_table(),
        |_: &&str| 3
    );
    row!(
        "config",
        turbosearch::storage::config::config_table(),
        |_: &&str| 6
    );
    row!("meta", turbosearch::storage::meta_table(), |_: &&str| 4);

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
