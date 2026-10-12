//! Decodes every real MOBI file in a library and reports the result.
//!
//! ```text
//! cargo run --release --bin mobicheck -- <root>
//! ```

use std::path::{Path, PathBuf};

use turbosearch::format::mobi;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = PathBuf::from(std::env::args().nth(1).unwrap_or_else(|| ".".to_string()));
    let mut files = Vec::new();
    walk(&root, &mut files);
    files.sort();

    for path in &files {
        println!(
            "=== {}",
            path.file_name().unwrap_or_default().to_string_lossy()
        );
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) => {
                println!("  read error: {e}");
                continue;
            }
        };
        println!("  file size: {} bytes", bytes.len());
        match mobi::parse(&bytes) {
            Ok(m) => {
                println!("  compression: {}", m.compression);
                println!("  text bytes : {}", m.text.len());
                println!("  title      : {:?}", m.metadata.title);
                println!("  authors    : {:?}", m.metadata.authors);
                println!("  language   : {:?}", m.metadata.language);
                println!("  isbn       : {:?}", m.metadata.isbn);
                let preview = String::from_utf8_lossy(&m.text[..m.text.len().min(400)]);
                let preview: String = preview.chars().take(400).collect();
                println!("  preview    : {:?}", preview.replace('\n', " ").trim());
            }
            Err(e) => println!("  PARSE ERROR: {e}"),
        }
    }
    Ok(())
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
        } else if p.extension().and_then(|e| e.to_str()) == Some("mobi") {
            out.push(p);
        }
    }
}
