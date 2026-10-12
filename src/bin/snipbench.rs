//! Measures what a result preview actually costs, per `note.md` section 4.5.
//!
//! ```text
//! cargo run --release --bin snipbench -- <path.epub>
//! ```

use std::path::Path;
use std::time::Instant;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let arg = std::env::args().nth(1).unwrap_or_default();
    let path = Path::new(&arg);
    let runs = 20;

    // 1. Cold: open the archive and extract ONE entry, which is what a snippet
    //    needs. Nothing else.
    let mut cold = Vec::new();
    for _ in 0..runs {
        let t0 = Instant::now();
        let file = std::fs::File::open(path)?;
        let mut zip = zip::ZipArchive::new(file)?;
        // The first XHTML entry, as the real path would look it up by name.
        let name = (0..zip.len())
            .find_map(|i| {
                let e = zip.by_index(i).ok()?;
                let n = e.name().to_string();
                (n.ends_with(".xhtml") || n.ends_with(".html") || n.ends_with(".htm")).then_some(n)
            })
            .expect("an EPUB contains at least one XHTML entry");
        let mut entry = zip.by_name(&name)?;
        let mut raw = Vec::new();
        std::io::Read::read_to_end(&mut entry, &mut raw)?;
        let text = turbosearch::format::html::extract_text(&raw)?;
        cold.push((t0.elapsed(), text.len()));
    }
    let avg_cold: f64 = cold.iter().map(|(d, _)| d.as_secs_f64()).sum::<f64>() / runs as f64 * 1000.0;
    let avg_len: usize = cold.iter().map(|(_, l)| *l).sum::<usize>() / runs;

    // 2. Tokenize one section, which the preview needs to locate the word.
    let section = turbosearch::format::epub::open(path)?;
    let longest = section
        .sections
        .iter()
        .max_by_key(|s| s.text.len())
        .expect("a book has sections");
    let text = String::from_utf8_lossy(&longest.text);

    let t0 = Instant::now();
    let mut spans = Vec::new();
    turbosearch::tokenize::for_each_token_spanned(&text, 0, |t, p, s, e| {
        spans.push((t.to_vec(), p, s, e));
    });
    let tok_ms = t0.elapsed().as_secs_f64() * 1000.0;

    // 3. Slice a 200-byte window around a middle token: the display step.
    let mid = spans.len() / 2;
    let (.., s, e) = spans[mid];
    let t0 = Instant::now();
    let mut window = String::new();
    for _ in 0..1000 {
        let a = s.saturating_sub(100);
        let b = (e + 100).min(text.len());
        window.push_str(&text[a..b]);
    }
    let slice_ms = t0.elapsed().as_secs_f64() * 1000.0 / 1000.0;

    // 4. Warm: re-use the same archive. This is the LRU-of-one case.
    let file = std::fs::File::open(path)?;
    let mut zip = zip::ZipArchive::new(file)?;
    let t0 = Instant::now();
    let mut total_warm = std::time::Duration::ZERO;
    for i in 0..runs {
        // Each "result" is a different section, so the entry lookup is the cost.
        let idx = i % zip.len();
        let t1 = Instant::now();
        let mut entry = zip.by_index(idx)?;
        let mut raw = Vec::new();
        std::io::Read::read_to_end(&mut entry, &mut raw)?;
        total_warm += t1.elapsed();
        let _ = raw.len();
    }
    let warm_ms = t0.elapsed().as_secs_f64() * 1000.0 / runs as f64;

    println!("file            : {}", path.display());
    println!("sections        : {}", section.sections.len());
    println!();
    println!("cold: open zip + read 1 entry + strip HTML : {avg_cold:.1} ms  (entry {avg_len} B)");
    println!("tokenize 1 section ({tokens} tokens)      : {tok_ms:.1} ms", tokens = spans.len());
    println!("slice a 200-byte window                    : {slice_ms:.3} ms");
    println!("warm: read one more entry from an open zip: {warm_ms:.1} ms");
    println!();
    println!("=> one preview ~ {:.1} ms cold, ~{:.2} ms warm", avg_cold + tok_ms, warm_ms + slice_ms);
    Ok(())
}
