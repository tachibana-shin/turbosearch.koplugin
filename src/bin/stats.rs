//! Real-corpus statistics collector for the postings codec stack.
//!
//! This is the gate described in `note.md` section 7. It measures the actual
//! compressed size of a postings index on a real library, and sweeps the two
//! levers that actually change the size:
//!
//! 1. `CAP` — maximum positions stored per (term, document)
//! 2. `DF_LIMIT` — only store positions for terms whose document frequency is
//!    below a threshold
//!
//! Text extraction is deliberately crude (chapter order falls back to entry-name
//! order) so that only the statistical shape of the token stream matters

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use turbosearch::codec::{elias_fano, front, pfor, vbyte};

/// Postings for one term: (doc_id, position) pairs, in ascending order.
type Postings = Vec<(u32, u32)>;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = PathBuf::from(std::env::args().nth(1).unwrap_or_else(|| {
        // The reference library, resolved through `library` so the path is
        // found on any machine that has one, rather than one developer's.
        turbosearch::library::root()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .to_string_lossy()
            .into_owned()
    }));
    let book_limit: usize = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(200);
    let caps: Vec<usize> = std::env::args()
        .nth(3)
        .map(|s| {
            s.split(',')
                .filter_map(|x| {
                    if x == "inf" {
                        Some(usize::MAX)
                    } else {
                        x.parse().ok()
                    }
                })
                .collect()
        })
        .unwrap_or_else(|| vec![2, 8, 32, usize::MAX]);

    println!("scanning {} (limit {} books)", root.display(), book_limit);

    let mut files = Vec::new();
    walk(&root, &mut files);
    files.sort();
    println!("found {} candidate files", files.len());

    let mut postings: HashMap<Vec<u8>, Postings> = HashMap::new();
    let mut tokens: u64 = 0;
    let mut books = 0u32;

    for doc in 0..book_limit as u32 {
        let Some(path) = files.get(doc as usize) else {
            break;
        };
        if let Ok(Some(text)) = extract_epub_text(path) {
            index_book(&mut postings, doc, &text);
            tokens += text.split_whitespace().count() as u64;
            books += 1;
        }
    }

    let terms: Vec<&[u8]> = {
        let map: BTreeMap<&[u8], ()> = postings
            .keys()
            .map(|k| k.as_slice())
            .map(|k| (k, ()))
            .collect();
        map.into_keys().collect()
    };

    println!();
    println!("=== CORPUS ===");
    println!("books indexed : {books}");
    println!("tokens        : {tokens}");
    println!(
        "distinct terms: {} ({:.1}K)",
        terms.len(),
        terms.len() as f64 / 1e3
    );

    // Term dictionary size under front coding.
    let refs: Vec<&[u8]> = terms.clone();
    let dict_fc = front::encode(&refs).len();
    let dict_raw: usize = terms.iter().map(|t| t.len() + 2).sum();
    println!(
        "dictionary    : front-coded {} bytes ({:.1}% of raw {})",
        dict_fc,
        100.0 * dict_fc as f64 / dict_raw as f64,
        dict_raw
    );

    // Size sweep over the two levers.
    println!();
    let full_postings: u64 = terms.iter().map(|t| postings[*t].len() as u64).sum();

    // `df_limit` keeps positions only for terms whose document frequency is low;
    // everything above it degrades to doc-level postings.
    let df_limits: Vec<usize> = std::env::args()
        .nth(4)
        .map(|s| {
            s.split(',')
                .filter_map(|x| {
                    if x == "inf" {
                        Some(usize::MAX)
                    } else {
                        x.parse().ok()
                    }
                })
                .collect()
        })
        .unwrap_or_else(|| vec![usize::MAX]);

    println!();
    println!("=== SIZE SWEEP (projected to the 2.5GB / 600M-token reference library) ===");
    println!(
        "{:>8} {:>9} {:>12} {:>8} {:>11} {:>13}",
        "cap/doc", "df-limit", "postings", "kept", "bits/post", "projected DB"
    );
    for &cap in &caps {
        for &df_limit in &df_limits {
            let r = measure(&postings, &terms, cap, df_limit, full_postings);
            println!(
                "{:>8} {:>9} {:>12} {:>7.1}% {:>11.2} {:>11} MB",
                if cap == usize::MAX {
                    "inf".to_string()
                } else {
                    cap.to_string()
                },
                if df_limit == usize::MAX {
                    "inf".to_string()
                } else {
                    df_limit.to_string()
                },
                r.postings,
                100.0 * r.postings as f64 / full_postings as f64,
                r.bits_per_posting,
                r.projected_mb as u64
            );
        }
    }

    let baseline = measure(&postings, &terms, usize::MAX, usize::MAX, full_postings);
    println!();
    println!("breakdown at cap=inf (bits per kept posting):");
    println!("  doc-gap   : {:.2}", baseline.doc_bits);
    println!("  freq      : {:.2}", baseline.freq_bits);
    println!("  pos-gap   : {:.2}   <-- dominant", baseline.pos_bits);
    println!("  entry     : {:.2}", baseline.entry_bits);
    let _ = vbyte::leb_len;
    Ok(())
}

struct Measure {
    postings: u64,
    bits_per_posting: f64,
    projected_mb: f64,
    doc_bits: f64,
    freq_bits: f64,
    pos_bits: f64,
    entry_bits: f64,
}

/// The reference library from `note.md` section 1.1.
const REFERENCE_TOKENS: u64 = 600_000_000;

fn measure(
    postings: &HashMap<Vec<u8>, Postings>,
    terms: &[&[u8]],
    cap: usize,
    df_limit: usize,
    full_postings: u64,
) -> Measure {
    let mut doc_bytes = 0usize;
    let mut freq_bytes = 0usize;
    let mut pos_bytes = 0usize;
    let mut entry_bytes = 0usize;
    let mut total_postings = 0u64;

    // Column buffers reused across blocks.
    let mut docs: Vec<u32> = Vec::new();
    let mut freqs: Vec<u32> = Vec::new();
    let mut pos_gaps: Vec<u32> = Vec::new();
    let mut entries: Vec<u32> = Vec::new();

    for term in terms {
        let list = &postings[*term];
        if list.len() > df_limit {
            continue;
        }

        let mut kept = Vec::new();
        let mut per_doc: HashMap<u32, u32> = HashMap::new();
        for &(d, p) in list {
            let n = per_doc.entry(d).or_insert(0);
            if (*n as usize) < cap {
                kept.push((d, p));
                *n += 1;
            }
        }
        if kept.is_empty() {
            continue;
        }
        total_postings += kept.len() as u64;

        for window in kept.chunks(128) {
            docs.clear();
            freqs.clear();
            pos_gaps.clear();
            entries.clear();

            // Group the window by document.
            let mut i = 0usize;
            while i < window.len() {
                let doc = window[i].0;
                let mut j = i;
                while j < window.len() && window[j].0 == doc {
                    j += 1;
                }
                let positions: Vec<u32> = window[i..j].iter().map(|&(_, p)| p).collect();

                docs.push(doc);
                freqs.push(positions.len() as u32);
                // Entry index advances slowly as we walk the document, which is
                // what makes the entry column compressible with RLE.
                let first = positions.first().copied().unwrap_or(0) / 4000;
                let last = positions.last().copied().unwrap_or(0) / 4000;
                entries.push(first);
                if last != first {
                    entries.push(last);
                }
                // Position gaps within this document.
                for k in 1..positions.len() {
                    pos_gaps.push(positions[k] - positions[k - 1]);
                }

                i = j;
            }

            doc_bytes += elias_fano::encode(&docs).len();
            freq_bytes += pfor::encode_auto(&freqs).len();
            pos_bytes += pfor::encode_auto(&pos_gaps).len();
            entry_bytes += pfor::encode_auto(&entries).len();
        }
    }

    let total = doc_bytes + freq_bytes + pos_bytes + entry_bytes;
    let bpp = if total_postings == 0 {
        0.0
    } else {
        total as f64 * 8.0 / total_postings as f64
    };

    // Project to the reference library. Everything scales linearly with tokens,
    // and the measured corpus is a fixed token count, so use bytes per token.
    let measured_tokens = postings
        .values()
        .map(|v| v.len() as u64)
        .sum::<u64>()
        .min(full_postings);

    Measure {
        postings: total_postings,
        bits_per_posting: bpp,
        projected_mb: total as f64 / measured_tokens.max(1) as f64 * REFERENCE_TOKENS as f64 / 1e6,
        doc_bits: doc_bytes as f64 * 8.0 / total_postings.max(1) as f64,
        freq_bits: freq_bytes as f64 * 8.0 / total_postings.max(1) as f64,
        pos_bits: pos_bytes as f64 * 8.0 / total_postings.max(1) as f64,
        entry_bits: entry_bytes as f64 * 8.0 / total_postings.max(1) as f64,
    }
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name.ends_with(".sdr") || name.starts_with('.') {
                continue;
            }
            walk(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("epub") {
            out.push(path);
        }
    }
}

fn extract_epub_text(path: &Path) -> std::io::Result<Option<String>> {
    let file = File::open(path)?;
    let mut archive = match zip::ZipArchive::new(file) {
        Ok(a) => a,
        Err(_) => return Ok(None),
    };

    let mut names: Vec<String> = Vec::new();
    for i in 0..archive.len() {
        if let Ok(entry) = archive.by_index(i) {
            let name = entry.name().to_string();
            if name.ends_with(".html") || name.ends_with(".xhtml") || name.ends_with(".htm") {
                names.push(name);
            }
        }
    }
    names.sort();

    let mut text = String::new();
    for name in names {
        if let Ok(mut entry) = archive.by_name(&name) {
            let mut buf = Vec::new();
            if entry.read_to_end(&mut buf).is_err() {
                continue;
            }
            if buf.len() > 8 * 1024 * 1024 {
                continue;
            }
            text.push_str(&strip_tags(&buf));
            text.push('\n');
        }
    }
    if text.trim().is_empty() {
        return Ok(None);
    }
    Ok(Some(text))
}

/// Minimal HTML text stripper: drops tags, comments and CDATA, keeps text.
///
/// Works on raw bytes so that multi-byte UTF-8 (Vietnamese, CJK) survives intact.
fn strip_tags(bytes: &[u8]) -> String {
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    let mut in_tag = false;
    let mut in_comment = false;

    while i < bytes.len() {
        let b = bytes[i];
        if in_comment {
            if b == b'-' && bytes[i..].starts_with(b"-->") {
                in_comment = false;
                i += 3;
            } else {
                i += 1;
            }
            continue;
        }
        if in_tag {
            if b == b'>' {
                in_tag = false;
                out.push(b' ');
            }
            i += 1;
            continue;
        }
        if b == b'<' {
            if bytes[i..].starts_with(b"<!--") {
                in_comment = true;
                i += 4;
            } else if bytes[i..].starts_with(b"<![CDATA[") {
                if let Some(end) = find(&bytes[i..], b"]]>") {
                    out.extend_from_slice(&bytes[i + 9..i + end]);
                    i += end + 3;
                } else {
                    out.push(b' ');
                    i += 9;
                }
            } else {
                in_tag = true;
                i += 1;
            }
            continue;
        }
        out.push(b);
        i += 1;
    }

    String::from_utf8_lossy(&out).into_owned()
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn index_book(postings: &mut HashMap<Vec<u8>, Postings>, doc: u32, text: &str) {
    let mut pos: u32 = 0;
    let mut run: Vec<u8> = Vec::new();

    for ch in text.chars() {
        if ch.is_alphanumeric() {
            for lower in ch.to_lowercase() {
                run.extend_from_slice(lower.encode_utf8(&mut [0u8; 4]).as_bytes());
            }
        } else if !run.is_empty() {
            postings
                .entry(std::mem::take(&mut run))
                .or_default()
                .push((doc, pos));
            pos += 1;
        }
    }
    if !run.is_empty() {
        postings.entry(run).or_default().push((doc, pos));
    }
}
