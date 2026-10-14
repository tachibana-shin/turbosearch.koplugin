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
use turbosearch::storage::metadata::{BookMetaRecord, Provenance};
use turbosearch::storage::{Engine, RunError, book::FileEntry, config::Config};
use turbosearch::tokenize::for_each_token_spanned;

/// Reads an environment variable, falling back to the settings file beside the
/// index.
///
/// The plugin writes `<index>.env`, and a variable the process was started without
/// is answered from there — so a setting changed on the reader reaches the next run
/// without anybody exporting it first. A variable that *is* set in the environment
/// wins: a one-off run from a shell should not be overridden by what the settings
/// screen says.
///
/// The file is `KEY = value` per line, the same shape the plugin shows, and a
/// missing or unreadable file means "no settings", exactly like an unset variable.
fn env_setting(db_path: &str, key: &str) -> Option<String> {
    if let Ok(v) = std::env::var(key) {
        return Some(v);
    }
    let file = std::path::Path::new(db_path).with_extension("env");
    let text = std::fs::read_to_string(&file).ok()?;
    env_from_text(&text, key)
}

/// Reads one setting out of the text of a settings file.
///
/// A blank value is *unset*, not set-to-nothing, and the difference is not
/// academic: `FRESH` is answered by its presence, so the settings screen writing
/// `FRESH=` for a field nobody filled in would have discarded the index on the
/// next run. The screen writes every key it offers, blank when there is no
/// choice to record, which means a blank line has to read as "nothing decided
/// here" — exactly as an absent file does.
fn env_from_text(text: &str, key: &str) -> Option<String> {
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // Out of the `if` rather than nested inside it: a line either has a key
        // and a value or it is not a setting, and saying so once reads as the
        // loop's own condition.
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        if k.trim() == key {
            let value = v.trim();
            if value.is_empty() {
                // The line says nothing, so it does not answer and does not
                // shadow a later line that does. Only a blank at the end of the
                // file — which is where a settings screen leaves a field nobody
                // filled in — reads as unset.
                continue;
            }
            return Some(value.to_string());
        }
    }
    None
}

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
    // Where the settings file sits: the index, with `env` in place of `idx`.
    let db_path: String = db.to_string_lossy().into_owned();

    let max_books: Option<usize> = env_setting(&db_path, "MAX_BOOKS").and_then(|v| v.parse().ok());
    let want_search = env_setting(&db_path, "SEARCH");
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
    if env_setting(&db_path, "FRESH").is_some() {
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
        match env_setting(&db_path, "CONTENT_MODE").as_deref() {
            Some("terms") => turbosearch::storage::config::ContentMode::TermsOnly,
            Some("files") => turbosearch::storage::config::ContentMode::FileList,
            Some("full") => turbosearch::storage::config::ContentMode::Full,
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
    // Sub-phase timings inside "decode + index + write". Without them the only
    // visible breakdown is decode-or-write, which is not enough to know which of
    // the two to attack — and the answer turned out to be neither.
    let mut t_decode = std::time::Duration::ZERO;
    let mut t_loop = std::time::Duration::ZERO;
    let mut t_entrymap = std::time::Duration::ZERO;
    let t_flush;
    let t_enc;
    let t_wr;
    let mut sections_total = 0usize;
    // `(book_id, metadata)` for every book, metadata-only formats included.
    let mut records: Vec<(u16, turbosearch::storage::metadata::BookMetaRecord)> = Vec::new();

    let mut dict = dict::Dict::new();
    {
        let mut w = writer::Writer::new(&engine)?;
        for entry in plan(&engine, &root, max_books) {
            // Sniff the container: the extension only decided whether the file was
            // worth opening at all.
            let format = sniff::detect(&entry.path);
            if !format.is_text() {
                skipped += 1;
                continue;
            }

            let book_id = w.documents() as u16;
            let _t = std::time::Instant::now();
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
            let decoded = match load_sections(&entry.path, format) {
                Ok(d) => d,
                Err(e) => {
                    failed += 1;
                    if failed <= 5 {
                        eprintln!("  decode {}: {e}", entry.path.display());
                    }
                    continue;
                }
            };
            t_decode += _t.elapsed();
            sections_total += decoded.sections.len();

            // One position counter for the whole book. Sections share it and are
            // told apart by `entry_idx`, which is what the chunk codec requires:
            // `pos_gap` is a delta, so restarting per section would make positions
            // non-monotonic within a document and corrupt the column.
            let _t_loop = std::time::Instant::now();
            let mut position = 0u32;
            let mut section_tokens = 0u64;
            // Every section's entry map, held until the book is done. Writing
            // them one at a time is one transaction per section, and a
            // 12 000-section library pays for twelve thousand of them; one per
            // book is both cheaper and leaves the file far less fragmented,
            // which is what the final compaction has to clean up.
            let mut maps: Vec<(u16, EntryMap)> = Vec::with_capacity(decoded.sections.len());
            for (entry_idx, text) in decoded.sections.iter().enumerate() {
                // Sections are decoded bytes; tokenizing needs text.
                let text = String::from_utf8_lossy(text).into_owned();
                let mut b = entrymap::Builder::new(entry_name(&entry.path, entry_idx));
                // Position and byte offset are offered together, which is what makes
                // the checkpoint sparse rather than one per token.
                if content_mode.has_positions() {
                    // The count comes from the same pass rather than a second
                    // `tokenize::tokenize` call: that re-tokenized every section
                    // and allocated a `Vec` of owned terms, which on the measured
                    // library is ~1,2 GB of churn to print one number.
                    position = for_each_token_spanned(&text, position, |term, p, start, _end| {
                        b.offer(p, start as u32);
                        let _ = w.add(term, book_id as u32, p, entry_idx as u16);
                        section_tokens += 1;
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
                        section_tokens += 1;
                    });
                    for (term, (p, entry)) in seen {
                        let _ = w.add(&term, book_id as u32, p, entry);
                    }
                    position = next;
                }
                maps.push((entry_idx as u16, b.finish()));

                text_bytes += text.len() as u64;
            }
            tokens += section_tokens;
            t_loop += _t_loop.elapsed();
            let _t_em = std::time::Instant::now();
            for (entry_idx, map) in &maps {
                writer::put_entry_map(&engine, book_id, *entry_idx, map)?;
            }
            t_entrymap += _t_em.elapsed();

            w.end_document()?;
            indexed += 1;
            *counts.entry(format!("{format:?}")).or_insert(0) += 1;

            // Metadata for every book, content or not: a PDF the decoder cannot
            // read still has a title worth searching for, and it must land under
            // the same book id the postings of readable books use.
            //
            // Sidecar first, exactly as `meta::resolve` orders it, but the
            // embedded half comes from the book just decoded instead of opening
            // the file a second time.
            let record = turbosearch::format::meta::from_sidecar(&entry.path)
                .or_else(|| decoded.embedded.clone())
                .unwrap_or_else(|| turbosearch::format::meta::from_filename(&entry.path));
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
                    entry_count: decoded.sections.len() as u16,
                    token_count: tokens as u32,
                    indexed_at_ms: now_ms(),
                    flags: 0,
                },
            )?;
        }
        w.close(&mut dict)?;
        t_flush = w.time_flush();
        t_enc = w.time_encode();
        t_wr = w.time_write();
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
    //
    // Only when the session actually wrote something. `VACUUM` rewrites the
    // whole database, so running it on a rescan that found nothing to do costs a
    // full 162 MB write to reclaim nothing — and a rescan with no changes is the
    // common case on a device. Measured on a 25-book directory: a no-change
    // rescan went from rewriting the file to writing nothing at all, and from
    // 28 s to 160 ms.
    let reclaimed = if std::env::var("NO_COMPACT").is_ok() {
        eprintln!("compaction skipped (NO_COMPACT): keeping every page the build churned");
        false
    } else if indexed == 0 && meta_rows == 0 {
        eprintln!("compaction skipped: nothing was indexed");
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
    println!("  decode                : {:>8.1?}", t_decode);
    println!("  chunk encode          : {:>8.1?}", t_enc);
    println!("  postings write        : {:>8.1?}", t_wr);
    println!(
        "  tokenize + index      : {:>8.1?}  (flush: {:>8.1?})",
        t_loop, t_flush
    );
    println!("  entry maps            : {:>8.1?}", t_entrymap);
    println!(
        "  postings flush + dict : {:>8.1?}",
        t_meta.saturating_sub(t_books)
    );
    println!(
        "  metadata + metaidx    : {:>8.1?}",
        t_meta.saturating_sub(t_books)
    );
    println!(
        "  compaction            : {:>8.1?}",
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

    if let Some(q) = env_setting(&db_path, "META_SEARCH") {
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
    let decoded = load_sections(Path::new(path), format).ok()?;
    let raw = decoded.sections.get(idx)?;
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
/// The files this pass has to index, diffed against what is already indexed.
///
/// The engine here is the **real** one, not a scratch copy. `scan_scope` diffs a
/// candidate against `BOOKS` on `(size, mtime, inode)` and reports it `unchanged`
/// when all three match, which is what makes a rescan cost a `stat` per file
/// rather than a full re-index. Diffing against a throwaway database made every
/// file look new on every run, so a rescan re-tokenized the whole library and
/// the file grew each time — measured at 15,3 s and a doubled database for a
/// rescan with no changes.
///
/// The settings come from the **database**, not from `Config::default()`. A
/// settings screen cannot be worth opening if what it writes is ignored on the
/// next run, and the config row is the one place both the plugin and this binary
/// can see. `Default` is still the fallback for an index nobody has configured.
fn plan(engine: &Engine, root: &Path, max: Option<usize>) -> Vec<FileEntry> {
    let cfg = match engine.config() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("config: {e}, using defaults");
            Config::default()
        }
    };
    let mut out = match scan_scope(engine, &cfg.filter(), root) {
        Ok(p) => {
            eprintln!(
                "scan: {} new, {} updated, {} unchanged, {} missing",
                p.new.len(),
                p.updated.len(),
                p.unchanged.len(),
                p.missing.len()
            );
            // `updated` is indexed alongside `new`: a file whose fingerprint
            // changed is a file whose content may have changed, and skipping it
            // because only `new` was consulted is how a book silently stops
            // matching after it is edited.
            let mut out = p.new;
            out.extend(p.updated);
            out
        }
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

/// Loads a book's sections in reading order, as decoded text.
/// The decoded book: its sections, and the metadata that was already open.
///
/// The metadata comes back with the sections rather than being resolved again
/// afterwards, because resolving it afterwards re-opens the file. That second
/// open cost 12,5 s per 100 books on the measured library, which is a whole
/// extra decode pass spent reading an OPF that was already in memory.
struct Decoded {
    sections: Vec<Vec<u8>>,
    /// Embedded metadata, when the container carried any.
    embedded: Option<BookMetaRecord>,
}

fn load_sections(path: &Path, format: Format) -> Result<Decoded, RunError> {
    match format {
        Format::Epub | Format::Kepub => {
            let book = epub::open(path).map_err(|e| bad(&e.to_string()))?;
            // The OPF is parsed by `open` anyway; turning it into the record here
            // is what `format::meta::from_embedded` does after re-opening.
            let embedded = (!book.metadata.title.is_empty()).then(|| {
                let mut record =
                    BookMetaRecord::new(book.metadata.title.clone(), book.metadata.authors.clone());
                record.language = book.metadata.language.clone();
                record.series = book.metadata.series.clone();
                record.series_index = book.metadata.series_index;
                record.set_description(book.metadata.description.clone());
                record.identifiers = book.metadata.identifiers.clone();
                record.publisher = book.metadata.publisher.clone();
                record.provenance = Provenance::Embedded;
                record
            });
            Ok(Decoded {
                sections: book.sections.into_iter().map(|s| s.text).collect(),
                embedded,
            })
        }
        Format::Txt | Format::Markdown => {
            let bytes = std::fs::read(path).map_err(|e| bad(&e.to_string()))?;
            Ok(Decoded {
                sections: plain::single_section(&bytes)
                    .map_err(|e| bad(&e.to_string()))?
                    .into_iter()
                    .map(|s| s.text)
                    .collect(),
                embedded: None,
            })
        }
        Format::Mobi | Format::Azw | Format::Azw3 => {
            let bytes = std::fs::read(path).map_err(|e| bad(&e.to_string()))?;
            let book = mobi::parse(&bytes).map_err(|e| bad(&e.to_string()))?;
            let embedded = (!book.metadata.title.is_empty()).then(|| {
                let mut record =
                    BookMetaRecord::new(book.metadata.title.clone(), book.metadata.authors.clone());
                record.language = book.metadata.language.clone();
                record.publisher = book.metadata.publisher.clone();
                record.identifiers = if book.metadata.isbn.is_empty() {
                    Vec::new()
                } else {
                    vec![format!("isbn:{}", book.metadata.isbn)]
                };
                record.set_description(book.metadata.description.clone());
                record.provenance = Provenance::Embedded;
                record
            });
            Ok(Decoded {
                sections: vec![book.text],
                embedded,
            })
        }
        _ => Err(bad("unsupported text format")),
    }
}

fn bad(what: &str) -> RunError {
    RunError::Message(what.to_string())
}

#[cfg(test)]
mod env_settings {
    use super::env_from_text;

    /// The one that matters: the settings screen writes every key it offers,
    /// blank when there is no choice to record, and `FRESH` is answered by its
    /// presence — so a blank line read as a value would discard the index on the
    /// next run. Blank has to mean "nothing decided here".
    #[test]
    fn a_blank_value_is_unset() {
        let file = "CONTENT_MODE=\nMAX_BOOKS=\nSEARCH=\nMETA_SEARCH=\nFRESH=\n";
        assert_eq!(env_from_text(file, "FRESH"), None);
        assert_eq!(env_from_text(file, "CONTENT_MODE"), None);
        assert_eq!(env_from_text(file, "MAX_BOOKS"), None);
    }

    #[test]
    fn a_value_is_returned_trimmed() {
        assert_eq!(
            env_from_text("CONTENT_MODE = terms \n", "CONTENT_MODE"),
            Some("terms".to_string())
        );
    }

    #[test]
    fn a_blank_value_does_not_hide_a_later_one() {
        // Two lines for one key is something only a hand-edit produces, and the
        // hand meant the second one.
        assert_eq!(
            env_from_text("FRESH=\nFRESH=1\n", "FRESH"),
            Some("1".to_string())
        );
    }

    #[test]
    fn comments_and_broken_lines_are_not_settings() {
        let file = "# a comment\n\nnot a setting\nFRESH=1\n";
        assert_eq!(env_from_text(file, "FRESH"), Some("1".to_string()));
        assert_eq!(env_from_text(file, "# a comment"), None);
    }

    #[test]
    fn an_absent_key_is_unset() {
        assert_eq!(env_from_text("CONTENT_MODE=full\n", "MAX_BOOKS"), None);
        assert_eq!(env_from_text("", "FRESH"), None);
    }
}
