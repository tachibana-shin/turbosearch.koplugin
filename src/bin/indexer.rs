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

use std::path::{Path, PathBuf};
use std::time::Instant;

use turbosearch::format::sniff;
use turbosearch::index::{EntryMap, dict, query, writer};
use turbosearch::storage::{Engine, build};

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

    // The scan and the build are two calls rather than one. A scan is one
    // `stat` per file and a build is minutes, so a caller that wants to say what
    // is about to happen — or warn that it will take a while — has somewhere to
    // do it.
    let plan = build::plan_scope(&engine, &root, max_books);
    if let Some(why) = &plan.config_error {
        eprintln!("{why}");
    }
    if let Some(why) = &plan.scan_error {
        eprintln!("{why}");
    }
    eprintln!(
        "scan: {} new, {} updated, {} unchanged, {} missing",
        plan.new,
        plan.updated,
        plan.unchanged,
        plan.missing.len()
    );

    // Compaction is the single biggest write in the build: it rewrites the whole
    // database to reclaim free pages. With rows written in key order the
    // overhead is already down to 37,9%, so this trades a one-time write against
    // a permanently smaller file, and the choice belongs to the caller rather
    // than to the code — which is why it can be turned off.
    let compact = if std::env::var("NO_COMPACT").is_ok() {
        eprintln!("compaction skipped (NO_COMPACT): keeping every page the build churned");
        false
    } else {
        true
    };

    let mut session = build::Session::new(&engine, plan, content_mode)?;
    // One step for the whole run: this caller has no screen to keep drawing, so
    // there is nothing to hand control back to between batches.
    session.step(&engine, usize::MAX)?;
    let stats = session.finish(&engine, compact)?;
    if !stats.reclaimed {
        eprintln!("compaction skipped: nothing was indexed");
    }
    for failure in &stats.failures {
        eprintln!("  {failure}");
    }
    println!(
        "metadata rows: {}, index {} bytes",
        stats.meta_rows, stats.meta_index_bytes
    );
    println!(
        "indexed {} books ({} sections), removed {}, skipped {}, failed {}",
        stats.indexed, stats.sections, stats.removed, stats.skipped, stats.failed
    );
    let formats = stats
        .formats
        .iter()
        .map(|(f, n)| format!("{f}={n}"))
        .collect::<Vec<_>>()
        .join(" ");
    println!("formats: {formats}");
    println!("text {} bytes, {} tokens", stats.text_bytes, stats.tokens);
    println!("phases:");
    println!("  decode                : {:>8.1?}", stats.decode);
    println!("  tokenize + index      : {:>8.1?}", stats.tokenize);
    println!("  entry maps            : {:>8.1?}", stats.entrymap);
    println!("  chunk encode          : {:>8.1?}", stats.chunk_encode);
    println!("  postings write        : {:>8.1?}", stats.postings_write);
    println!("  postings flush + dict : {:>8.1?}", stats.writer_flush);
    println!("  metadata + metaidx    : {:>8.1?}", stats.finish);
    println!("  total                 : {:>8.1?}", started.elapsed());
    let size = engine.file_size();
    println!(
        "compaction reclaimed free pages: {}",
        if stats.reclaimed {
            "yes"
        } else {
            "none to reclaim"
        }
    );
    println!("db {db:?} = {size} bytes ({:.1} MB)", size as f64 / 1e6);
    if stats.text_bytes > 0 {
        println!(
            "index size / text size = {:.1}%",
            size as f64 * 100.0 / stats.text_bytes as f64
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
    let decoded = build::load_sections(Path::new(path), format).ok()?;
    let raw = decoded.sections.get(idx)?;
    Some(String::from_utf8_lossy(raw).into_owned())
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
