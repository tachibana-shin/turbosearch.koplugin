//! Inspects one real book from the library: what format, what metadata, and how
//! much text came out.
//!
//! ```text
//! cargo run --release --bin inspect -- <path.epub>
//! ```

use std::path::Path;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let arg = std::env::args().nth(1).unwrap_or_default();
    let path = Path::new(&arg);
    let fmt = turbosearch::format::detect(path);
    println!("file       : {}", path.display());
    println!(
        "extension  : {:?}",
        path.extension().and_then(|e| e.to_str())
    );
    println!("detected   : {fmt:?}");

    let t0 = std::time::Instant::now();
    let content = turbosearch::format::epub::open(path)?;
    println!("opened in  : {:.1?}", t0.elapsed());
    println!("sections   : {}", content.sections.len());
    println!("title      : {}", content.metadata.title);
    println!("authors    : {:?}", content.metadata.authors);
    println!("language   : {}", content.metadata.language);
    println!(
        "series     : {} #{}",
        content.metadata.series, content.metadata.series_index
    );
    println!("identifiers: {}", content.metadata.identifiers.join(", "));

    let text: usize = content.sections.iter().map(|s| s.text.len()).sum();
    println!("text bytes : {text} ({:.1} KB)", text as f64 / 1024.0);
    let preview = String::from_utf8_lossy(&content.sections[0].text);
    let start = preview
        .char_indices()
        .nth(120)
        .map(|(i, _)| i)
        .unwrap_or(preview.len());
    println!("preview    : {:?}", &preview[..start.min(preview.len())]);
    Ok(())
}
