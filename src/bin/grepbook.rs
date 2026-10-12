//! Finds which books in the library contain a phrase, by running the real
//! extraction and tokenization pipeline and doing a positional intersection.
//!
//! This is the query engine's inner loop, executed against real data before the
//! engine itself exists.
//!
//! ```text
//! cargo run --release --bin grepbook -- <phrase> [root]
//! ```

use std::path::{Path, PathBuf};

fn main() {
    let phrase = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "thám tử".to_string());
    let root = std::env::args().nth(2).unwrap_or_else(|| {
        // The reference library, resolved through `library` so the path is
        // found on any machine that has one, rather than one developer's.
        turbosearch::library::root()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .to_string_lossy()
            .into_owned()
    });

    let mut files = Vec::new();
    walk(Path::new(&root), &mut files);
    files.sort();
    println!("query: {phrase:?} over {} books", files.len());

    // Tokenize the query the same way the documents are tokenized.
    let mut qterms: Vec<String> = Vec::new();
    turbosearch::tokenize::for_each_token(&phrase, 0, |t, _| {
        qterms.push(String::from_utf8_lossy(t).into_owned())
    });
    println!("query terms: {qterms:?}");
    if qterms.is_empty() {
        println!("(no terms; nothing to search)");
        return;
    }

    let mut hits = 0;
    let mut shown = 0;
    for path in &files {
        let Ok(content) = turbosearch::format::epub::open(path) else {
            continue;
        };

        // A book's token stream is continuous across sections, so the position
        // counter is threaded through — that is what phrase adjacency depends
        // on. Spans, however, are offsets *within their own section*, so each
        // token carries the section it came from. Mixing those two is exactly
        // the trap the POSTINGS layout in `note.md` section 4.1 avoids by
        // storing `(doc_id, position)` for matching and `entry_idx` for
        // locating the text.
        let mut position = 0u32;
        let mut tokens: Vec<(String, u32, u32, usize, usize)> = Vec::new();
        for (section_index, section) in content.sections.iter().enumerate() {
            let text = String::from_utf8_lossy(&section.text).into_owned();
            let mut spans: Vec<(String, u32, usize, usize)> = Vec::new();
            turbosearch::tokenize::for_each_token_spanned(&text, position, |t, pos, s, e| {
                spans.push((String::from_utf8_lossy(t).into_owned(), pos, s, e));
            });
            if let Some(last) = spans.last() {
                position = last.1 + 1;
            }
            for (term, pos, s, e) in spans {
                tokens.push((term, pos, section_index as u32, s, e));
            }
        }

        // Locate the first term, then look for the rest in order at a gap of
        // one position. This is the two-pointer intersection of `note.md`
        // section 4.1 step 3, in its simplest form.
        let first = &qterms[0];
        let mut found = Vec::new();
        for (i, tok) in tokens.iter().enumerate() {
            if tok.0 != *first {
                continue;
            }
            let mut ok = true;
            for (k, want) in qterms.iter().enumerate().skip(1) {
                match tokens.get(i + k) {
                    Some(t) if t.0 == *want && t.1 == tok.1 + k as u32 => {}
                    _ => {
                        ok = false;
                        break;
                    }
                }
            }
            if ok {
                found.push(i);
            }
        }

        if found.is_empty() {
            continue;
        }
        hits += 1;
        if shown < 3 {
            shown += 1;
            let i = found[0];
            // Preview from the ORIGINAL text around the first hit.
            let section_index = tokens[i].2 as usize;
            let from = tokens[i].3.saturating_sub(30);
            let last = i + qterms.len() - 1;
            let to = (tokens[last].4 + 40).min(content.sections[section_index].text.len());
            println!(
                "  {} hit(s) — {:?}",
                found.len(),
                path.file_name().unwrap().to_string_lossy()
            );
            println!(
                "      preview: {:?}",
                preview(&content, section_index, from, to)
            );
        }
    }
    println!("books matching: {hits}");
}

/// Slices the original text of one section, clamping to that section's bounds.
fn preview(
    content: &turbosearch::format::epub::BookContent,
    section_index: usize,
    from: usize,
    to: usize,
) -> String {
    let Some(section) = content.sections.get(section_index) else {
        return String::new();
    };
    let a = from.min(section.text.len());
    let b = to.clamp(a, section.text.len());
    String::from_utf8_lossy(&section.text[a..b])
        .replace('\n', " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
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
