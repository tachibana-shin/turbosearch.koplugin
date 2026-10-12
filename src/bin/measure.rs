//! Full-corpus extraction and index size measurement.
//!
//! Answers one question with real numbers: how big is the extracted text, and how
//! big is the index built on top of it, for each position cap.
//!
//! Runs over the whole library pointed at by the first argument so the result
//! needs no extrapolation.
//!
//! ```text
//! cargo run --release --bin measure -- <root> [zstd]
//! ```

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use turbosearch::codec::{elias_fano, front, pfor};

/// Postings for one term: (doc_id, position) pairs in ascending order.
type Postings = Vec<(u32, u32)>;

struct Corpus {
    postings: HashMap<Vec<u8>, Postings>,
    texts: Vec<String>,
    books: u32,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = PathBuf::from(
        std::env::args()
            .nth(1)
            .unwrap_or_else(|| "/home/shin/.config/koreader/rakuyomi/downloads/calibre".to_string()),
    );
    let zstd_levels: Vec<i32> = std::env::args()
        .nth(2)
        .map(|s| s.split(',').filter_map(|x| x.parse().ok()).collect())
        .unwrap_or_else(|| vec![3, 19]);

    let mut files = Vec::new();
    walk(&root, &mut files);
    files.sort();
    println!("library root : {}", root.display());
    println!("candidate files: {}", files.len());

    let mut corpus = Corpus {
        postings: HashMap::new(),
        texts: Vec::new(),
        books: 0,
    };
    let mut skipped = 0u32;
    let start = std::time::Instant::now();

    for (doc, path) in files.iter().enumerate() {
        match extract_epub_text(path) {
            Ok(Some(text)) => {
                let tokens = index_book(&mut corpus.postings, doc as u32, &text);
                let _ = tokens;
                corpus.texts.push(text);
                corpus.books += 1;
            }
            _ => skipped += 1,
        }
        if doc % 100 == 99 {
            let bytes: u64 = corpus.texts.iter().map(|t| t.len() as u64).sum();
            println!(
                "  ... {}/{} books, {} text so far, {:.1}s elapsed",
                doc + 1,
                files.len(),
                fmt_bytes(bytes),
                start.elapsed().as_secs_f64()
            );
        }
    }

    let text_bytes: u64 = corpus.texts.iter().map(|t| t.len() as u64).sum();
    let tokens: u64 = corpus.postings.values().map(|v| v.len() as u64).sum();
    let lib_bytes: u64 = files
        .iter()
        .filter_map(|p| std::fs::metadata(p).ok())
        .map(|m| m.len())
        .sum();

    println!();
    println!("=== EXTRACTION (whole library, no extrapolation) ===");
    println!("books indexed   : {}", corpus.books);
    println!("books skipped   : {skipped}");
    println!("raw text bytes  : {} ({})", text_bytes, fmt_bytes(text_bytes));
    println!("avg text / book : {} KB", text_bytes / corpus.books.max(1) as u64 / 1024);
    println!("tokens          : {} ({} M)", tokens, tokens / 1_000_000);

    let terms: Vec<&[u8]> = {
        let map: BTreeMap<&[u8], ()> = corpus
            .postings
            .keys()
            .map(|k| k.as_slice())
            .map(|k| (k, ()))
            .collect();
        map.into_keys().collect()
    };
    let refs: Vec<&[u8]> = terms.clone();
    let dict_fc = front::encode(&refs).len();
    println!("distinct terms  : {} ({} K)", terms.len(), terms.len() / 1000);
    println!("dictionary      : {} (front-coded)", fmt_bytes(dict_fc as u64));

    println!();
    println!("=== ZSTD BASELINE (what if we just stored the text) ===");
    let mut all_text: Vec<u8> = Vec::with_capacity(text_bytes as usize + corpus.texts.len());
    for t in &corpus.texts {
        all_text.extend_from_slice(t.as_bytes());
        all_text.push(b'\n');
    }
    for level in &zstd_levels {
        let t0 = std::time::Instant::now();
        let n = zstd_len(&all_text, *level);
        println!(
            "zstd-{:<2}          : {} ({:.1}% of raw text, {:.1}s)",
            level,
            fmt_bytes(n as u64),
            100.0 * n as f64 / text_bytes as f64,
            t0.elapsed().as_secs_f64()
        );
    }

    println!();
    println!("=== INDEX SIZE (whole library, no extrapolation) ===");
    println!("{:>8} {:>12} {:>14} {:>10} {:>12} {:>10}",
        "cap/doc", "positions", "index bytes", "index MB", "% of text", "% of epub");
    for cap in [usize::MAX, 32usize, 8, 2] {
        let t0 = std::time::Instant::now();
        let r = measure_index(&corpus, &terms, cap);
        let label = if cap == usize::MAX { "full".to_string() } else { cap.to_string() };
        println!(
            "{:>8} {:>12} {:>14} {:>10.1} {:>11.1}% {:>9.1}%  {:.1}s",
            label,
            r.positions,
            r.bytes,
            r.bytes as f64 / 1e6,
            100.0 * r.bytes as f64 / text_bytes as f64,
            100.0 * r.bytes as f64 / lib_bytes as f64,
            t0.elapsed().as_secs_f64(),
        );
    }

    println!();
    println!("=== VERDICT ===");
    println!("library (.epub on disk) : {}", fmt_bytes(lib_bytes));
    println!("extracted raw text      : {}", fmt_bytes(text_bytes));
    println!("term dictionary         : {}", fmt_bytes(dict_fc as u64));
    Ok(())
}

fn fmt_bytes(n: u64) -> String {
    if n >= 1_000_000_000 {
        format!("{:.2} GB", n as f64 / 1e9)
    } else if n >= 1_000_000 {
        format!("{:.1} MB", n as f64 / 1e6)
    } else {
        format!("{} KB", n / 1024)
    }
}

struct IndexMeasure {
    positions: u64,
    bytes: usize,
}

fn measure_index(corpus: &Corpus, terms: &[&[u8]], cap: usize) -> IndexMeasure {
    let mut doc_bytes = 0usize;
    let mut freq_bytes = 0usize;
    let mut pos_bytes = 0usize;
    let mut entry_bytes = 0usize;
    let mut positions = 0u64;

    let mut docs: Vec<u32> = Vec::new();
    let mut freqs: Vec<u32> = Vec::new();
    let mut gaps: Vec<u32> = Vec::new();
    let mut entries: Vec<u32> = Vec::new();

    for term in terms {
        let list = &corpus.postings[*term];

        // Apply the per-(term, doc) cap.
        let mut seen: HashMap<u32, u32> = HashMap::new();
        let mut kept: Vec<(u32, u32)> = Vec::new();
        for &(d, p) in list {
            let n = seen.entry(d).or_insert(0);
            if (*n as usize) < cap {
                kept.push((d, p));
                *n += 1;
            }
        }
        if kept.is_empty() {
            continue;
        }
        positions += kept.len() as u64;

        for window in kept.chunks(128) {
            docs.clear();
            freqs.clear();
            gaps.clear();
            entries.clear();

            let mut i = 0usize;
            while i < window.len() {
                let doc = window[i].0;
                let mut j = i;
                while j < window.len() && window[j].0 == doc {
                    j += 1;
                }
                let positions_in_doc: Vec<u32> = window[i..j].iter().map(|&(_, p)| p).collect();

                docs.push(doc);
                freqs.push(positions_in_doc.len() as u32);

                // Entry index advances slowly through a document, which is what
                // makes this column compressible.
                let first = positions_in_doc[0] / 4000;
                let last = *positions_in_doc.last().unwrap() / 4000;
                entries.push(first);
                if last != first {
                    entries.push(last);
                }
                for k in 1..positions_in_doc.len() {
                    gaps.push(positions_in_doc[k] - positions_in_doc[k - 1]);
                }
                i = j;
            }

            doc_bytes += elias_fano::encode(&docs).len();
            freq_bytes += pfor::encode_auto(&freqs).len();
            pos_bytes += pfor::encode_auto(&gaps).len();
            entry_bytes += pfor::encode_auto(&entries).len();
        }
    }

    IndexMeasure {
        positions,
        bytes: doc_bytes + freq_bytes + pos_bytes + entry_bytes,
    }
}

fn zstd_len(data: &[u8], level: i32) -> usize {
    zstd::encode_all(data, level).map(|c| c.len()).unwrap_or(0)
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
            if buf.len() > 32 * 1024 * 1024 {
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

/// Minimal HTML text stripper over raw bytes so multi-byte UTF-8 survives.
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

/// Tokenizes one book and returns its token count.
fn index_book(postings: &mut HashMap<Vec<u8>, Postings>, doc: u32, text: &str) -> u64 {
    let mut pos: u32 = 0;
    let mut run: Vec<u8> = Vec::new();

    for ch in text.chars() {
        if ch.is_alphanumeric() {
            for lower in ch.to_lowercase() {
                run.extend_from_slice(lower.encode_utf8(&mut [0u8; 4]).as_bytes());
            }
        } else if !run.is_empty() {
            postings.entry(std::mem::take(&mut run)).or_default().push((doc, pos));
            pos += 1;
        }
    }
    if !run.is_empty() {
        postings.entry(run).or_default().push((doc, pos));
    }
    pos as u64
}
