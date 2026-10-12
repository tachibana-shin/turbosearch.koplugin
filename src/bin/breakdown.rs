//! Breaks an index down by codec, to see where its bytes are going.
//!
//! ```text
//! cargo run --release --bin breakdown -- <db_path>
//! ```
//!
//! The chunk block stores four length-prefixed sections, so the split is
//! measurable without decoding anything: each section's declared length is what
//! it costs on disk.

use std::time::Instant;

use turbosearch::index::posting::CHUNK_POSTINGS;
use turbosearch::storage::Engine;
use redb::ReadableTable;

const SECTIONS: usize = 4;

/// Column names, in the order `Chunk::encode` writes them.
const NAMES: [&str; SECTIONS] = ["elias_fano(docs)", "pfor(freqs)", "rle(entries)", "pfor(pos_gaps)"];

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args().nth(1).expect("db path");
    let engine = Engine::open(&path)?;

    let started = Instant::now();
    let mut totals = [0u64; SECTIONS];
    let mut header = 0u64;
    let mut rows = 0u64;
    let mut postings = 0u64;
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
            let bytes = v.value();
            rows += 1;
            terms.insert(k.value().0.to_vec());

            // The chunk header is: tag(4) + first_doc(4) + three u32 counters(12)
            // then length-prefixed sections.
            let mut pos = 20usize;
            for slot in &mut totals {
                let Some(len) = bytes.get(pos..pos + 4) else { break };
                let len = u32::from_le_bytes([len[0], len[1], len[2], len[3]]) as usize;
                pos += 4 + len;
                header += 4;
                *slot += len as u64;
            }
            // Count the postings the block claims, which is the honest
            // denominator: bytes per posting.
            //
            // Layout is tag(4), first_doc(4), then the three u32 counters.
            if bytes.len() >= 12 {
                postings += u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]) as u64;
            }
            header += 20;
        }
        Ok(())
    })?;

    let elapsed = started.elapsed();
    let all = totals.iter().sum::<u64>() + header;
    println!("{rows} chunks over {} terms in {elapsed:.1?}", terms.len());
    println!("chunk postings claimed: {postings} (chunk size {CHUNK_POSTINGS})");
    println!("block bytes: {all} ({:.2} MB)", all as f64 / 1e6);
    for (name, n) in NAMES.iter().zip(totals) {
        let pct = n as f64 * 100.0 / all as f64;
        let per = if postings > 0 { n as f64 / postings as f64 } else { 0.0 };
        println!("  {name:<18} {n:>12} ({pct:5.1}%)  {per:.2} B/posting");
    }
    println!("  {:<18} {header:>12} ({:5.1}%)", "headers", header as f64 * 100.0 / all as f64);
    println!();
    println!("  {:<18} {:>12} ({:5.1}%)  {:.2} B/posting", "total", all, 100.0, all as f64 / postings as f64);
    report_table_sizes(&engine);
    Ok(())
}

/// Value bytes and row counts per table, so page overhead shows up as the gap
/// between these and the file size.
fn report_table_sizes(engine: &Engine) {
    println!();
    println!("per table (value bytes + key bytes):");
    let mut grand = 0usize;

    // One concrete read per table. A generic version would need each table's
    // key and value types to agree, which they do not, and the whole point is
    // to see the real numbers rather than a shape.
    macro_rules! row {
        ($name:literal, $def:expr, $key:expr) => {{
            let t = engine
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
                t.0,
                t.0 as f64 / 1e6,
                t.1,
                if t.1 > 0 { t.0 as f64 / t.1 as f64 } else { 0.0 }
            );
            grand += t.0;
        }};
    }

    row!("postings", turbosearch::storage::postings_table(), |k: &(&[u8], u16)| k.0.len() + 2);
    row!("entrymap", turbosearch::storage::entrymap_table(), |_: &(u16, u16)| 4);
    row!("dict", turbosearch::storage::dict_table(), |_: &&[u8]| 3);
    row!("dict_delta", turbosearch::storage::delta_table(), |_: &&[u8]| 0);
    row!("books", turbosearch::storage::book::books_table(), |_: &u16| 2);
    row!("metadata", turbosearch::storage::metadata::metadata_table(), |_: &u16| 2);
    row!("metaidx", turbosearch::storage::metadata::metaidx_table(), |_: &&str| 3);
    row!("config", turbosearch::storage::config::config_table(), |_: &&str| 6);
    row!("meta", turbosearch::storage::meta_table(), |_: &&str| 4);

    println!("  {:<12} {grand:>12} bytes ({:.2} MB)", "total", grand as f64 / 1e6);
}
