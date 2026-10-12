//! Runs the sniffer over a real library and reports what it finds.
//!
//! ```text
//! cargo run --release --bin sniffcheck -- <root>
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use turbosearch::format::sniff::{detect, is_text_format};
use turbosearch::storage::book::Format;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = PathBuf::from(std::env::args().nth(1).unwrap_or_else(|| ".".to_string()));
    let mut files = Vec::new();
    walk(&root, &mut files);
    files.sort();
    println!("scanning {} files under {}", files.len(), root.display());

    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut mismatched: Vec<(String, Format, Format)> = Vec::new();
    let mut text = 0usize;

    for path in &files {
        let sniffed = detect(path);
        let claimed = path
            .extension()
            .and_then(|e| e.to_str())
            .map(Format::from_extension)
            .unwrap_or(Format::Unknown);
        if is_text_format(sniffed) {
            text += 1;
        }
        if sniffed != claimed {
            mismatched.push((
                path.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
                claimed,
                sniffed,
            ));
        }
        *counts.entry(format!("{sniffed:?}")).or_insert(0) += 1;
    }

    println!("\n--- detected formats ---");
    for (fmt, n) in &counts {
        println!("  {fmt:<12} {n}");
    }
    println!("text-bearing: {text} of {}", files.len());

    if !mismatched.is_empty() {
        println!(
            "\n--- extension disagrees with contents ({} files) ---",
            mismatched.len()
        );
        for (name, claimed, sniffed) in mismatched.iter().take(15) {
            println!("  {name}\n      claimed {claimed:?} -> actually {sniffed:?}");
        }
        if mismatched.len() > 15 {
            println!("  ... and {} more", mismatched.len() - 15);
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
        } else if p.is_file() {
            out.push(p);
        }
    }
}
