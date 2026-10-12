//! Reports indexer progress while a full build runs.
//!
//! ```text
//! cargo run --release --bin wprogress -- <root> [db_path]
//! ```
//!
//! A full build takes minutes on a large library, and the indexer is silent for
//! all of them: nothing says whether it is on book 50 or book 500, or whether it
//! is decoding, tokenizing, or writing. This wrapper runs the same pipeline and
//! prints a line per book, so a slow pass can be attributed to a source rather
//! than being a total mystery.

use std::path::{Path, PathBuf};
use std::time::Instant;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let root = PathBuf::from(args.next().unwrap_or_else(|| {
        turbosearch::library::root()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .to_string_lossy()
            .into_owned()
    }));
    let db = PathBuf::from(args.next().unwrap_or_else(|| "library.idx".into()));
    let only = std::env::var("ONLY").unwrap_or_default();

    if std::fs::remove_file(&db).is_err() {
        // Nothing to remove: fine, the open reports anything else.
    }
    let engine = turbosearch::storage::Engine::open(&db)?;
    let started = Instant::now();
    let mut words = 0u64;
    let mut secs = 0usize;
    let mut done = 0usize;
    let mut failed = 0usize;
    let mut slowest: Vec<(std::time::Duration, String)> = Vec::new();

    {
        use turbosearch::format::sniff;
        use turbosearch::index::{dict, entrymap, writer};
        use turbosearch::storage::book::Book;
        use turbosearch::tokenize::for_each_token_spanned;

        let mut dict = dict::Dict::new();
        let mut w = writer::Writer::new(&engine)?;
        let mut records: Vec<(u16, turbosearch::storage::metadata::BookMetaRecord)> = Vec::new();

        let mut files: Vec<PathBuf> = std::fs::read_dir(&root)?
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_file())
            .collect();
        files.sort();
        if !only.is_empty() {
            files.retain(|p| p.to_string_lossy().contains(&only));
        }
        println!("{} books; writing to {}", files.len(), db.display());

        for (i, path) in files.iter().enumerate() {
            let t = Instant::now();
            let format = sniff::detect(path);
            if !format.is_text() {
                continue;
            }
            let Some(sections) = load(path, format) else {
                failed += 1;
                continue;
            };
            let book_id = w.documents() as u16;
            let mut book_position = 0u32;

            for (entry_idx, text) in sections.iter().enumerate() {
                let text = String::from_utf8_lossy(text).into_owned();
                let mut b = entrymap::Builder::new(format!("{}#{entry_idx}", path.display()));
                book_position =
                    for_each_token_spanned(&text, book_position, |term, p, start, _e| {
                        b.offer(p, start as u32);
                        let _ = w.add(term, book_id as u32, p, entry_idx as u16);
                    });
                writer::put_entry_map(&engine, book_id, entry_idx as u16, &b.finish())?;
                secs += 1;
            }
            w.end_document()?;

            let record = turbosearch::format::meta::resolve(path);
            engine.put_book(
                book_id,
                &Book {
                    path: path.to_string_lossy().into_owned(),
                    scope: String::new(),
                    size: sections.iter().map(|t| t.len() as u64).sum(),
                    mtime_ms: 0,
                    inode: 0,
                    format,
                    entry_count: sections.len() as u16,
                    token_count: 0,
                    indexed_at_ms: 0,
                    flags: 0,
                },
            )?;
            records.push((book_id, record));

            let took = t.elapsed();
            words += sections.iter().map(|t| t.len() as u64).sum::<u64>();
            done += 1;
            if slowest.len() < 8 || slowest.last().map(|s| s.0) < Some(took) {
                slowest.push((
                    took,
                    path.file_name().unwrap().to_string_lossy().into_owned(),
                ));
                slowest.sort_by_key(|s| std::cmp::Reverse(s.0));
                slowest.truncate(8);
            }
            if done.is_multiple_of(25) || i + 1 == files.len() {
                let size = engine.file_size();
                println!(
                    "[{}/{}] {} sec, {} books, {} sections, {} bytes text, db {:.1} MB",
                    i + 1,
                    files.len(),
                    started.elapsed().as_secs(),
                    done,
                    secs,
                    words,
                    size as f64 / 1e6
                );
            }
        }
        for (id, r) in &records {
            engine.put_metadata(*id, r)?;
        }
        w.close(&mut dict)?;
    }
    engine.compact()?;

    println!("\ndone: {done} books, {secs} sections, {words} bytes text, {failed} failed");
    println!(
        "total {:.1} s, db {:.1} MB",
        started.elapsed().as_secs_f64(),
        std::fs::metadata(&db)?.len() as f64 / 1e6
    );
    if !slowest.is_empty() {
        println!("\nslowest books:");
        for (t, name) in &slowest {
            println!(
                "  {:.2} s  {}",
                t.as_secs_f64(),
                &name[..name.len().min(70)]
            );
        }
    }
    Ok(())
}

/// Loads a book's sections in reading order, as decoded text.
fn load(path: &Path, format: turbosearch::storage::book::Format) -> Option<Vec<Vec<u8>>> {
    use turbosearch::storage::book::Format;
    match format {
        Format::Epub | Format::Kepub => Some(
            turbosearch::format::epub::open(path)
                .ok()?
                .sections
                .into_iter()
                .map(|s| s.text)
                .collect(),
        ),
        Format::Txt | Format::Markdown => {
            let bytes = std::fs::read(path).ok()?;
            Some(
                turbosearch::format::plain::single_section(&bytes)
                    .ok()?
                    .into_iter()
                    .map(|s| s.text)
                    .collect(),
            )
        }
        Format::Mobi | Format::Azw | Format::Azw3 => Some(vec![
            turbosearch::format::mobi::parse(&std::fs::read(path).ok()?)
                .ok()?
                .text,
        ]),
        _ => None,
    }
}
