//! Measures what the exact positions actually buy, and what coarsening them costs.
//!
//! `note.md` section 3.4 found positions are 84% of the index. This quantifies
//! the alternative: keep positions only to a block, which shrinks them sharply
//! but turns a phrase query into candidate generation that needs verification.
//!
//! ```text
//! cargo run --release --bin postest -- <books_dir> [books_to_sample]
//! ```

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use turbosearch::codec::{elias_fano, pfor};

fn main() {
    let dir = PathBuf::from(std::env::args().nth(1).unwrap_or_else(|| ".".to_string()));
    let limit: usize = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(60);

    let mut files = Vec::new();
    walk(&dir, &mut files);
    files.sort();
    if files.is_empty() {
        eprintln!("no EPUB files found under {}", dir.display());
        return;
    }

    let mut postings: HashMap<Vec<u8>, Vec<(u32, u32)>> = HashMap::new();
    let mut books = 0usize;

    for (doc, path) in files.iter().take(limit).enumerate() {
        let Ok(content) = turbosearch::format::epub::open(path) else {
            continue;
        };
        books += 1;
        let mut position = 0u32;
        for section in &content.sections {
            let text = String::from_utf8_lossy(&section.text);
            position = turbosearch::tokenize::for_each_token(&text, position, |term, pos| {
                postings
                    .entry(term.to_vec())
                    .or_default()
                    .push((doc as u32, pos));
            });
        }
    }

    let total: u64 = postings.values().map(|v| v.len() as u64).sum();
    println!("books={books} terms={} postings={total}", postings.len());
    println!();

    // --- size under each position granularity -------------------------------
    let doc_only = measure_parts(&postings, 0).1;
    println!(
        "{:<34} {:>9} bytes  {:>6.1} MB  {:>5.1} bits/posting",
        "doc ids + freq only (no positions)",
        doc_only,
        doc_only as f64 / 1e6,
        doc_only as f64 * 8.0 / total as f64
    );
    for bits in [0u32, 6, 8, 10] {
        let size = measure(&postings, bits);
        let label = if bits == 0 {
            "exact".to_string()
        } else {
            format!("pos >> {bits} ({} -token blocks)", 1usize << bits)
        };
        println!(
            "{label:<34} {size:>9} bytes  {:>6.1} MB  {:>5.1} bits/posting",
            size as f64 / 1e6,
            size as f64 * 8.0 / total as f64
        );
    }

    // --- what coarsening does to a real phrase query ------------------------
    // "Thám tử" is a query a user of this library would actually type.
    println!();
    for bits in [0u32, 6] {
        let exact = phrase_hits(&postings, b"tham", b"tu", 0);
        let coarse = phrase_hits(&postings, b"tham", b"tu", bits);
        println!(
            "phrase \"tham tu\" with pos>>{bits}: {exact} exact matches, {coarse} candidates to verify"
        );
        if bits > 0 {
            println!(
                "   -> {} false positives per true match",
                coarse.saturating_sub(exact) as f64 / exact.max(1) as f64
            );
        }
    }
}

/// Counts postings of `b` that sit `block - 1` blocks after a posting of `a`
/// in the same document. With `bits == 0` that is the exact phrase match.
fn phrase_hits(
    postings: &HashMap<Vec<u8>, Vec<(u32, u32)>>,
    a: &[u8],
    b: &[u8],
    bits: u32,
) -> usize {
    let Some(pa) = postings.get(a) else { return 0 };
    let Some(pb) = postings.get(b) else { return 0 };

    // Coarse positions are a sorted key, so a hash map of (doc, block) works
    // and prefix-scan is not needed.
    let mut by_key: HashMap<(u32, u32), usize> = HashMap::new();
    for (d, p) in pb {
        let key = coarsen(*p, bits);
        *by_key.entry((*d, key)).or_insert(0) += 1;
    }

    let mut hits = 0usize;
    for (d, p) in pa {
        let key = coarsen(*p, bits);
        // `a` is followed by `b` where `b` occupies the next block.
        if by_key.contains_key(&(*d, key + 1)) {
            hits += 1;
        }
    }
    hits
}

fn coarsen(pos: u32, bits: u32) -> u32 {
    if bits == 0 { pos } else { pos >> bits }
}

/// Encodes every term's postings, slicing into blocks of 128 like the real
/// writer does, and returns the total byte count.
fn measure(postings: &HashMap<Vec<u8>, Vec<(u32, u32)>>, bits: u32) -> usize {
    measure_parts(postings, bits).0
}

/// Returns (full size, doc-only size) so the value of positions is visible.
fn measure_parts(postings: &HashMap<Vec<u8>, Vec<(u32, u32)>>, bits: u32) -> (usize, usize) {
    let mut total = 0usize;
    let mut doc_only = 0usize;
    let mut terms: Vec<&Vec<u8>> = postings.keys().collect();
    terms.sort();

    let mut docs: Vec<u32> = Vec::new();
    let mut freqs: Vec<u32> = Vec::new();
    let mut gaps: Vec<u32> = Vec::new();

    for term in terms {
        let list = &postings[term];
        for window in list.chunks(128) {
            docs.clear();
            freqs.clear();
            gaps.clear();

            // Group by document. Within a document the gaps are monotone, which
            // is what makes them compressible.
            let mut i = 0usize;
            while i < window.len() {
                let doc = window[i].0;
                let mut j = i;
                while j < window.len() && window[j].0 == doc {
                    j += 1;
                }
                docs.push(doc);
                freqs.push((j - i) as u32);
                let mut prev = 0u32;
                for (offset, item) in window[i..j].iter().enumerate() {
                    let value = coarsen(item.1, bits);
                    gaps.push(if offset == 0 { value } else { value - prev });
                    prev = value;
                }
                i = j;
            }

            let d = elias_fano::encode(&docs).len();
            let f = pfor::encode_auto(&freqs).len();
            let g = pfor::encode_auto(&gaps).len();
            total += d + f + g;
            doc_only += d + f;
        }
    }
    (total, doc_only)
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            let n = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if n.ends_with(".sdr") || n.starts_with('.') {
                continue;
            }
            walk(&p, out);
        } else if p.extension().and_then(|e| e.to_str()) == Some("epub") {
            out.push(p);
        }
    }
}
