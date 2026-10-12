//! Tokenizes a real book and reports what actually comes out.
//!
//! ```text
//! cargo run --release --bin tokcheck -- <path.epub>
//! ```

use std::path::Path;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let arg = std::env::args().nth(1).unwrap_or_default();
    let path = Path::new(&arg);
    let content = turbosearch::format::epub::open(path)?;

    let mut total: u64 = 0;
    let mut position = 0u32;
    let mut bad = 0u64;
    let mut first: Vec<(String, u32)> = Vec::new();

    for section in &content.sections {
        let text = String::from_utf8_lossy(&section.text);
        position = turbosearch::tokenize::for_each_token(&text, position, |term, pos| {
            total += 1;
            if first.len() < 8 {
                first.push((String::from_utf8_lossy(term).into_owned(), pos));
            }
            // A term must never contain whitespace, a combining mark, an
            // uppercase letter, or be empty.
            if term.is_empty()
                || term.iter().any(|b| *b <= b' ')
                || term.iter().any(|&b| b.is_ascii_uppercase())
                || String::from_utf8_lossy(term)
                    .chars()
                    .any(|c| c.is_whitespace())
            {
                bad += 1;
            }
        });
    }

    println!("file       : {}", path.display());
    println!("title      : {}", content.metadata.title);
    println!("sections   : {}", content.sections.len());
    println!("tokens     : {total}");
    println!("positions  : {position}");
    println!("bad tokens : {bad}");
    println!("first 8    :");
    for (t, p) in &first {
        println!("  [{p}] {t:?}");
    }

    // The real test: search with a folded, unaccented query and build a
    // preview from what the book actually says.
    let query = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "nguyen".to_string());
    let wanted = turbosearch::tokenize::normalize(&query);
    println!();
    println!("query       : {query:?} -> index key {wanted:?}");

    let mut hits = 0u64;
    'outer: for section in &content.sections {
        let text = String::from_utf8_lossy(&section.text);
        let mut spans: Vec<(Vec<u8>, u32, usize, usize)> = Vec::new();
        turbosearch::tokenize::for_each_token_spanned(&text, 0, |term, pos, s, e| {
            spans.push((term.to_vec(), pos, s, e));
        });
        for (i, (term, _, s, e)) in spans.iter().enumerate() {
            if term != wanted.as_bytes() {
                continue;
            }
            hits += 1;
            if hits > 3 {
                break 'outer;
            }
            // Preview: the words on either side, as written in the book.
            let mut ws = *s;
            let mut we = *e;
            for _ in 0..7 {
                if let Some(prev) = spans[..i].iter().rev().find(|(_, _, ps, _)| ps < &ws) {
                    ws = prev.2;
                } else {
                    break;
                }
            }
            for _ in 0..7 {
                if let Some(next) = spans[i + 1..].iter().find(|(_, _, ns, _)| ns > &we) {
                    we = next.3;
                } else {
                    break;
                }
            }
            let preview = &text[ws..we];
            println!("  preview {hits}: {preview:?}");
        }
    }
    println!("total hits for {query:?}: {hits}");
    Ok(())
}
