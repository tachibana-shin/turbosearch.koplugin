//! End-to-end index + search over a real library.
//!
//! This is the tool that answers the questions the whole design rests on: how
//! big does the database get, how long does indexing take, and do the results
//! look right on books nobody hand-picked?
//!
//! ```text
//! cargo run --release --bin indexer -- <root> [db_path]
//! SEARCH="thám tử" cargo run --release --bin indexer -- <root> [db_path]
//! MAX_BOOKS=20 cargo run --release --bin indexer -- <root> [db_path]
//! ```
//!
//! Without `SEARCH` it only measures. Each book becomes one `doc_id`, and each
//! section of that book becomes one `ENTRYMAP` row, which is what lets a hit
//! resolve back to the exact file it came from.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use turbosearch::format::{epub, mobi, plain, sniff};
use turbosearch::index::{EntryMap, dict, entrymap, query, writer};
use turbosearch::storage::book::{Format, scan_scope};
use turbosearch::storage::{Engine, RunError, book::FileEntry, config::Config};
use turbosearch::tokenize::{self, for_each_token_spanned};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let root = PathBuf::from(args.next().unwrap_or_else(|| {
        turbosearch::library::root()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .to_string_lossy()
            .into_owned()
    }));
    let db = PathBuf::from(args.next().unwrap_or_else(|| {
        // A scratch database under the process temp directory: a hardcoded
        // path would resolve on the machine that developed this and nowhere
        // else.
        turbosearch::library::scratch_dir()
            .join("turbosearch.idx")
            .to_string_lossy()
            .into_owned()
    }));
    let max_books: Option<usize> = std::env::var("MAX_BOOKS").ok().and_then(|v| v.parse().ok());
    let want_search = std::env::var("SEARCH").ok();
    // `--mode full|terms|files` persists the choice into CONFIG.
    let set_mode: Option<String> = {
        let argv: Vec<String> = std::env::args().collect();
        argv.iter()
            .position(|a| a == "--mode")
            .and_then(|i| argv.get(i + 1))
            .cloned()
    };

    // The database is only wiped when asked, because wiping it also wipes the
    // CONFIG row the content mode lives in — and a tool that silently discards
    // the setting it was just given is worse than one that refuses.
    if std::env::var("FRESH").is_ok() {
        std::fs::remove_file(&db).ok();
        // The WAL and shared-memory sidecars survive the main file and make the
        // next open read a stale journal, so they go too.
        for suffix in ["-wal", "-shm"] {
            std::fs::remove_file(format!("{}{}", db.display(), suffix)).ok();
        }
    }
    let engine = Engine::open(&db).map_err(|e| format!("open {db:?}: {e}"))?;

    // The stored config decides how much content to index. `--mode` writes it
    // back, so the setting survives to the next run; `CONTENT_MODE` instead
    // overrides for this run only, which is how the three depths were measured
    // against the same library without editing a config row each time.
    let stored = engine.config().map_err(|e| format!("read config: {e}"))?;
    let content_mode = if let Some(m) = &set_mode {
        let parsed = turbosearch::storage::config::ContentMode::from_name(m)
            .ok_or_else(|| format!("unknown content mode {m:?}: expected full, terms or files"))?;
        let mut cfg = stored.clone();
        cfg.content_mode = parsed;
        engine
            .set_config(&cfg)
            .map_err(|e| format!("write config: {e}"))?;
        parsed
    } else {
        match std::env::var("CONTENT_MODE").as_deref() {
            Ok("terms") => turbosearch::storage::config::ContentMode::TermsOnly,
            Ok("files") => turbosearch::storage::config::ContentMode::FileList,
            Ok("full") => turbosearch::storage::config::ContentMode::Full,
            _ => stored.content_mode,
        }
    };
    if set_mode.is_some() {
        eprintln!(
            "content mode: {} (written to CONFIG, replacing {})",
            content_mode.name(),
            stored.content_mode.name()
        );
    } else if content_mode != stored.content_mode {
        eprintln!(
            "content mode: {} (CONTENT_MODE overrides the stored {})",
            content_mode.name(),
            stored.content_mode.name()
        );
    } else {
        eprintln!("content mode: {} (from config)", content_mode.name());
    }
    eprintln!(
        "  content: {}, positions: {}",
        content_mode.has_content(),
        content_mode.has_positions()
    );

    let started = Instant::now();
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut indexed = 0usize;
    let mut skipped = 0usize;
    let mut failed = 0usize;
    let mut text_bytes = 0u64;
    let mut tokens = 0u64;
    let mut sections_total = 0usize;
    // `(book_id, metadata)` for every book, metadata-only formats included.
    let mut records: Vec<(u16, turbosearch::storage::metadata::BookMetaRecord)> = Vec::new();

    let mut dict = dict::Dict::new();
    {
        let mut w = writer::Writer::new(&engine)?;
        for entry in plan(&root, max_books) {
            // Sniff the container: the extension only decided whether the file was
            // worth opening at all.
            let format = sniff::detect(&entry.path);
            if !format.is_text() {
                skipped += 1;
                continue;
            }

            let book_id = w.documents() as u16;
            // `FileList` never opens a book: that is the entire point of the
            // mode. Indexing 600 books costs a `read_dir` and a metadata read,
            // not 300 MB of decoded HTML.
            if !content_mode.has_content() {
                let record = turbosearch::format::meta::resolve(&entry.path);
                engine.put_book_batch(
                    book_id,
                    &turbosearch::storage::book::Book {
                        path: entry.path.to_string_lossy().into_owned(),
                        scope: entry.scope.clone().unwrap_or_default(),
                        size: entry.size,
                        mtime_ms: entry.mtime_ms,
                        inode: entry.inode,
                        format,
                        entry_count: 0,
                        token_count: 0,
                        indexed_at_ms: now_ms(),
                        flags: 0,
                    },
                )?;
                engine.put_metadata_batch(book_id, &record)?;
                records.push((book_id, record));
                w.end_document()?;
                indexed += 1;
                *counts
                    .entry(format!("{format:?} metadata-only"))
                    .or_insert(0) += 1;
                continue;
            }
            let Ok(sections) = load_sections(&entry.path, format) else {
                failed += 1;
                if failed <= 5 {
                    eprintln!("  decode {}: unreadable", entry.path.display());
                }
                continue;
            };
            sections_total += sections.len();

            // One position counter for the whole book. Sections share it and are
            // told apart by `entry_idx`, which is what the chunk codec requires:
            // `pos_gap` is a delta, so restarting per section would make positions
            // non-monotonic within a document and corrupt the column.
            let mut position = 0u32;
            // Every section's entry map, held until the book is done. Writing
            // them one at a time is one transaction per section, and a
            // 12 000-section library pays for twelve thousand of them; one per
            // book is both cheaper and leaves the file far less fragmented,
            // which is what the final compaction has to clean up.
            let mut maps: Vec<(u16, EntryMap)> = Vec::with_capacity(sections.len());
            for (entry_idx, text) in sections.iter().enumerate() {
                // Sections are decoded bytes; tokenizing needs text.
                let text = String::from_utf8_lossy(text).into_owned();
                let mut b = entrymap::Builder::new(entry_name(&entry.path, entry_idx));
                // Position and byte offset are offered together, which is what makes
                // the checkpoint sparse rather than one per token.
                if content_mode.has_positions() {
                    position = for_each_token_spanned(&text, position, |term, p, start, _end| {
                        b.offer(p, start as u32);
                        let _ = w.add(term, book_id as u32, p, entry_idx as u16);
                    });
                } else {
                    // `TermsOnly`: one posting per term and book, which is what
                    // makes the mode small — the same term repeated two hundred
                    // times in one book still contributes exactly one posting.
                    //
                    // The position counter keeps running across sections, exactly
                    // as in `Full`: `pos_gap` is a delta within a document, and
                    // resetting it per section makes positions repeat, which
                    // underflows the column and makes the term's whole posting
                    // list unreadable. Measured as a content search that returned
                    // zero hits in this mode and the right answer in `Full`.
                    let mut seen: std::collections::BTreeMap<Vec<u8>, (u32, u16)> =
                        std::collections::BTreeMap::new();
                    let mut next = position;
                    for_each_token_spanned(&text, position, |term, p, _start, _end| {
                        seen.insert(term.to_vec(), (p, entry_idx as u16));
                        next = next.max(p + 1);
                    });
                    for (term, (p, entry)) in seen {
                        let _ = w.add(&term, book_id as u32, p, entry);
                    }
                    position = next;
                }
                maps.push((entry_idx as u16, b.finish()));

                text_bytes += text.len() as u64;
                tokens += tokenize::tokenize(&text).len() as u64;
            }
            for (entry_idx, map) in &maps {
                writer::put_entry_map(&engine, book_id, *entry_idx, map)?;
            }

            w.end_document()?;
            indexed += 1;
            *counts.entry(format!("{format:?}")).or_insert(0) += 1;

            // Metadata is indexed for every book, content or not: a PDF the
            // decoder cannot read still has a title worth searching for, and it
            // must land under the same book id the postings of readable books use.
            let record = turbosearch::format::meta::resolve(&entry.path);
            records.push((book_id, record));

            // Registering the book matters for more than listing: `METAIDX` is
            // rebuilt from `METADATA` rows *that have a live `BOOKS` row*, so a
            // book omitted here would silently vanish from every metadata search.
            engine.put_book_batch(
                book_id,
                &turbosearch::storage::book::Book {
                    path: entry.path.to_string_lossy().into_owned(),
                    scope: entry.scope.clone().unwrap_or_default(),
                    size: entry.size,
                    mtime_ms: entry.mtime_ms,
                    inode: entry.inode,
                    format,
                    entry_count: sections.len() as u16,
                    token_count: tokens as u32,
                    indexed_at_ms: now_ms(),
                    flags: 0,
                },
            )?;
        }
        w.close(&mut dict)?;
    }
    let t_books = started.elapsed();

    let mut meta_rows = 0usize;
    for (book_id, record) in &records {
        engine.put_metadata_batch(*book_id, record)?;
        meta_rows += 1;
    }
    // Everything above with `*_batch` is parked in memory until one durable
    // commit. `rebuild_index` below reads those rows, and the non-durable commit
    // is visible to it inside this process, but the file on disk still needs the
    // flush before compaction rewrites it.
    engine.flush()?;
    let index = turbosearch::storage::metadata::rebuild_index(&engine)?;
    engine.put_meta_index(&index)?;
    println!(
        "metadata rows: {meta_rows}, index {} bytes",
        index.blob_len()
    );
    // Compact once the writer is gone, so the file does not carry every page the
    // session churned through. A fresh index that skips this measured 69% free
    // space, which is three times the data it holds.
    let t_meta = started.elapsed();
    // Compaction is the single biggest write in the build: it rewrites the
    // whole database to reclaim free pages. With rows written in key order the
    // overhead is already down to 37,9%, so this trades a one-time write
    // against a permanently smaller file, and the choice belongs to the caller
    // rather than to the code — which is why it can be turned off.
    let reclaimed = if std::env::var("NO_COMPACT").is_ok() {
        eprintln!("compaction skipped (NO_COMPACT): keeping every page the build churned");
        false
    } else {
        engine.compact()?
    };
    let size_after_compact = engine.file_size();
    let t_compact = started.elapsed();

    println!(
        "indexed {indexed} books ({sections_total} sections), skipped {skipped}, failed {failed}"
    );
    let formats = counts
        .iter()
        .map(|(f, n)| format!("{f}={n}"))
        .collect::<Vec<_>>()
        .join(" ");
    println!("formats: {formats}");
    println!("text {text_bytes} bytes, {tokens} tokens");
    println!("phases:");
    println!("  decode + index + write : {:>8.1?}", t_books);
    println!(
        "  metadata + metaidx     : {:>8.1?}",
        t_meta.saturating_sub(t_books)
    );
    println!(
        "  compaction             : {:>8.1?}",
        t_compact.saturating_sub(t_meta)
    );
    println!("  total                  : {:>8.1?}", started.elapsed());
    let size = size_after_compact;
    println!(
        "compaction reclaimed free pages: {}",
        if reclaimed { "yes" } else { "none to reclaim" }
    );
    println!("db {db:?} = {} bytes ({:.1} MB)", size, size as f64 / 1e6);
    if text_bytes > 0 {
        println!(
            "index size / text size = {:.1}%",
            size as f64 * 100.0 / text_bytes as f64
        );
    }

    // Diagnostics, all through one handle: redb files are single-writer, so a
    // second `Engine::open` on the same path fails while this one lives. The
    // engine's own work is finished, so releasing it here is safe.
    drop(engine);
    let diag = Engine::open(&db)?;

    if let Ok(q) = std::env::var("META_SEARCH") {
        let rows = turbosearch::storage::metadata::search(&diag, &q)?;
        println!("metadata {q:?} -> {} results", rows.len());
        for (id, title) in rows.iter().take(10) {
            println!("  {id}: {title}");
        }
    }

    if std::env::var("DEBUG_DICT").is_ok() {
        let d = dict::load(&diag)?;
        println!(
            "dictionary {} terms; terms flagged partial: {}",
            d.len(),
            d.entries().iter().filter(|x| x.is_partial()).count()
        );
        for probe in ["tham", "conan", "th\u{e1}m"] {
            let postings = query::term_postings(&diag, probe.as_bytes())?;
            println!(
                "  probe {probe:?}: in dict {}, {} postings",
                d.get(probe.as_bytes()).is_some(),
                postings.len()
            );
        }
    }

    if let Some(q) = want_search {
        let dict = dict::load(&diag)?;

        let t = Instant::now();
        let hits = query::search_phrase(&diag, &dict, &q)?;
        println!(
            "\nphrase {q:?} -> {} hits in {:.1?}",
            hits.len(),
            t.elapsed()
        );
        show(&diag, &hits);

        let t = Instant::now();
        let all = query::search(&diag, &dict, &q, None)?;
        println!(
            "any-term {q:?} -> {} hits in {:.1?} (status {:?})",
            all.hits.len(),
            t.elapsed(),
            all.status
        );
        show(&diag, &all.hits);
    }
    Ok(())
}
/// Prints the top hits with a snippet cut from the book's own text.
///
/// Re-loading the section text here is the honest measurement: it is what a
/// reader's UI does, and it captures the cost of turning a posting back into
/// words.
fn show(engine: &Engine, hits: &[query::Hit]) {
    // Sections are keyed by an opaque name, so a book id is all a real caller
    // would have from `BOOKS`; here the name carries the path.
    let maps = all_entry_names(engine);
    for (i, h) in hits.iter().take(8).enumerate() {
        let snippets = hit_snippets(engine, h, &maps);
        println!("  {}. doc {} score {}", i + 1, h.doc_id, h.score);
        for (name, text) in snippets.iter().take(2) {
            println!("      {name}");
            println!("      {text}");
        }
    }
}

/// Every `ENTRYMAP` row the engine holds, keyed by `(book, entry)`.
fn all_entry_names(engine: &Engine) -> Vec<(u16, u16, EntryMap)> {
    let mut out = Vec::new();
    for book in engine.books_with_entry_maps().unwrap_or_default() {
        // The engine has no "list every entry of a book" accessor, and adding
        // one for a diagnostic would be API for its own sake, so walk the
        // section count the book itself recorded.
        let count = engine
            .book(book)
            .ok()
            .flatten()
            .map(|b| b.entry_count)
            .unwrap_or(0);
        for entry in 0..count {
            if let Ok(Some(m)) = writer::get_entry_map(engine, book, entry) {
                out.push((book, entry, m));
            }
        }
    }
    out
}

fn hit_snippets(
    _engine: &Engine,
    hit: &query::Hit,
    maps: &[(u16, u16, EntryMap)],
) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (book, entry, map) in maps {
        if *book != hit.doc_id as u16 {
            continue;
        }
        let Some(text) = load_section_text(map) else {
            continue;
        };
        let (pos, byte_off) = map.checkpoint_for(hit.first_position);
        if let Some(off) = query::locate(&text, pos, byte_off, hit.first_position) {
            out.push((map.name.clone(), query::snippet(&text, off, 120)));
        }
        let _ = entry;
    }
    out
}

/// Loads the text a map's name refers back to.
fn load_section_text(map: &EntryMap) -> Option<String> {
    // The entry name is "<path>#<entry_idx>", which is enough to recover the
    // section for a diagnostic run.
    let (path, idx) = map.name.rsplit_once('#')?;
    let idx: usize = idx.parse().ok()?;
    let format = sniff::detect(Path::new(path));
    let sections = load_sections(Path::new(path), format).ok()?;
    let raw = sections.get(idx)?;
    Some(String::from_utf8_lossy(raw).into_owned())
}

/// Name recorded in `ENTRYMAP`, carrying the source path so snippets can be
/// traced back to a file.
/// Wall-clock milliseconds, for `Book.indexed_at_ms`.
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn entry_name(path: &Path, idx: usize) -> String {
    format!("{}#{idx}", path.display())
}

/// Walks the library through the real scan path, so the extension filters and
/// directory exclusions in `Config` are exercised rather than assumed.
fn plan(root: &Path, max: Option<usize>) -> Vec<FileEntry> {
    let cfg = Config::default();
    let engine_dummy = tempdir_engine();
    let mut out = match scan_scope(&engine_dummy, &cfg.filter(), root) {
        Ok(p) => p.new,
        Err(e) => {
            eprintln!("scan {root:?}: {e}");
            Vec::new()
        }
    };
    out.sort_by(|a, b| a.path.cmp(&b.path));
    if let Some(n) = max {
        out.truncate(n);
    }
    out
}

/// A throwaway engine, because `scan_scope` diffs against `BOOKS` and an empty
/// database makes every candidate come back as new.
fn tempdir_engine() -> Engine {
    let dir = turbosearch::library::scratch_dir();
    // Created here rather than assumed: the same path is used to write the
    // database, so it has to exist wherever the process's temp directory is.
    if let Err(e) = std::fs::create_dir_all(&dir) {
        panic!("create {}: {e}", dir.display());
    }
    let path = dir.join("turbosearch-scanned.sqlite");
    let _ = std::fs::remove_file(&path);
    Engine::open(path).expect("open scratch engine")
}

/// Loads a book's sections in reading order, as decoded text.
fn load_sections(path: &Path, format: Format) -> Result<Vec<Vec<u8>>, RunError> {
    match format {
        Format::Epub | Format::Kepub => {
            let book = epub::open(path).map_err(|e| bad(&e.to_string()))?;
            Ok(book.sections.into_iter().map(|s| s.text).collect())
        }
        Format::Txt | Format::Markdown => {
            let bytes = std::fs::read(path).map_err(|e| bad(&e.to_string()))?;
            Ok(plain::single_section(&bytes)
                .map_err(|e| bad(&e.to_string()))?
                .into_iter()
                .map(|s| s.text)
                .collect())
        }
        Format::Mobi | Format::Azw | Format::Azw3 => {
            let bytes = std::fs::read(path).map_err(|e| bad(&e.to_string()))?;
            Ok(vec![
                mobi::parse(&bytes).map_err(|e| bad(&e.to_string()))?.text,
            ])
        }
        _ => Err(bad("unsupported text format")),
    }
}

fn bad(what: &str) -> RunError {
    RunError::Message(what.to_string())
}
