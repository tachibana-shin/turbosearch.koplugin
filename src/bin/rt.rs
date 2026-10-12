//! Times metadata resolution alone, over N books.
use std::time::Instant;
fn main() {
    let root = std::path::PathBuf::from(std::env::args().nth(1).unwrap());
    let n: usize = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(50);
    let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(&root)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .collect();
    files.sort();
    let t = Instant::now();
    let mut sidecar = 0;
    let mut embedded = 0;
    let mut filename = 0;
    for p in files.iter().take(n) {
        let r = turbosearch::format::meta::resolve(p);
        match r.provenance {
            turbosearch::storage::metadata::Provenance::Sidecar => sidecar += 1,
            turbosearch::storage::metadata::Provenance::Embedded => embedded += 1,
            _ => filename += 1,
        }
    }
    println!(
        "resolve {n} books in {:.2?}  (sidecar {sidecar}, embedded {embedded}, filename {filename})",
        t.elapsed()
    );
}
