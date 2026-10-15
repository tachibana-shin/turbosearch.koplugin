//! Turning files into indexed books.
//!
//! The command-line indexer and the reader both build an index, and the only
//! thing that should differ between them is who asked. The loop that decodes a
//! book, tokenizes it and writes its postings lives here so that a search from
//! inside a book indexes exactly the way a build from a shell does: a second
//! copy of this loop would be a second set of bugs, and the copy nobody runs
//! the tests against is the one that would drift.
//!
//! The work is split in two on purpose. [`Plan`] is a scan — cheap, one `stat`
//! per file, and it says what needs doing. [`Session`] does the reading and the
//! writing, in as many [`Session::step`] calls as the caller cares to make,
//! because a reader who asks a question is owed a screen that still draws while
//! the answer is being built.

use std::collections::{BTreeMap, VecDeque};
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::format::{epub, mobi, plain, sniff};
use crate::index::{EntryMap, dict, entrymap, query, writer};
use crate::storage::book::{Book, FileEntry, Format, ScanPlan, scan_full, scan_scope};
use crate::storage::config::{Config, ContentMode};
use crate::storage::metadata::{BookMetaRecord, Provenance, rebuild_index};
use crate::storage::{Engine, RunError, RunResult};
use crate::tokenize::for_each_token_spanned;

/// What a scan decided, before a single file is opened.
#[derive(Debug, Default)]
pub struct Plan {
    /// New and updated files, in the order they will be indexed.
    pub entries: Vec<FileEntry>,
    /// Files never indexed before.
    pub new: usize,
    /// Files whose fingerprint changed, so their text may have too.
    pub updated: usize,
    /// Files already indexed and untouched: one `stat` each and nothing else.
    pub unchanged: usize,
    /// Books under a root this pass could see that are no longer on disk.
    ///
    /// Carried rather than counted because they are acted on: the row has to go
    /// or the reader is offered a file that is not there.
    pub missing: Vec<(u16, String)>,
    /// The stored config could not be read, so the defaults were used.
    pub config_error: Option<String>,
    /// The scan itself failed; the plan is empty.
    pub scan_error: Option<String>,
}

/// The files one directory needs indexing.
///
/// This is the fast path a search takes: one folder, a few seconds at most on
/// first sight and a `stat` per file afterwards. A root that is missing or
/// unreadable is not an error — the reader may have unplugged the card — it is
/// an empty plan.
pub fn plan_scope(engine: &Engine, dir: &Path, max: Option<usize>) -> Plan {
    let (config, config_error) = load_config(engine);
    finish_plan(scan_scope(engine, &config.filter(), dir), max, config_error)
}

/// Every file the configured roots need indexing.
///
/// The slow path, and the one worth warning about: on a library that has never
/// been indexed this is minutes rather than seconds.
pub fn plan_full(engine: &Engine, max: Option<usize>) -> Plan {
    let (config, config_error) = load_config(engine);
    finish_plan(scan_full(engine, &config), max, config_error)
}

/// Reads the config from the database, falling back to the defaults.
///
/// From the database and not from `Config::default()`: the config row is the
/// one place a settings screen and this binary can both see, and a tool that
/// silently discards the settings it was just given is worse than one that
/// refuses.
fn load_config(engine: &Engine) -> (Config, Option<String>) {
    match engine.config() {
        Ok(config) => (config, None),
        Err(e) => (
            Config::default(),
            Some(format!("config: {e}, using defaults")),
        ),
    }
}

fn finish_plan(
    scan: RunResult<ScanPlan>,
    max: Option<usize>,
    config_error: Option<String>,
) -> Plan {
    let scan = match scan {
        Ok(s) => s,
        Err(e) => {
            return Plan {
                config_error,
                scan_error: Some(format!("scan: {e}")),
                ..Plan::default()
            };
        }
    };
    let ScanPlan {
        new,
        updated,
        unchanged,
        missing,
        mode: _,
        scanned_roots: _,
    } = scan;

    let plan = Plan {
        new: new.len(),
        updated: updated.len(),
        unchanged: unchanged.len(),
        missing,
        config_error,
        scan_error: None,
        entries: Vec::new(),
    };
    let mut entries = new;
    entries.extend(updated);
    // Path order rather than the scan's modification order, so an id is handed
    // to the same book whichever of two runs gets there first.
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    if let Some(n) = max {
        entries.truncate(n);
    }
    Plan { entries, ..plan }
}

/// What one or more [`Session::step`] calls have done.
#[derive(Debug, Default)]
pub struct BuildStats {
    /// Books registered, content or metadata only.
    pub indexed: usize,
    /// Files the extension filter would not open at all.
    pub skipped: usize,
    /// Files whose container could not be decoded.
    pub failed: usize,
    /// The first few decode failures, which is all anybody will read.
    pub failures: Vec<String>,
    /// Books removed because their file is gone.
    pub removed: usize,
    pub sections: usize,
    pub text_bytes: u64,
    pub tokens: u64,
    /// Metadata rows written when the session ended.
    pub meta_rows: usize,
    /// Size of the rebuilt metadata index.
    pub meta_index_bytes: usize,
    /// Format counts, so a caller can say what it actually read.
    pub formats: BTreeMap<String, usize>,
    /// Whether the file was compacted at the end.
    pub reclaimed: bool,
    pub decode: Duration,
    pub tokenize: Duration,
    pub entrymap: Duration,
    pub chunk_encode: Duration,
    pub postings_write: Duration,
    pub writer_flush: Duration,
    pub finish: Duration,
}

impl BuildStats {
    /// True when this session left the database different from how it found it.
    ///
    /// Compaction rewrites the whole file, so running it when nothing was
    /// written costs a full write to reclaim nothing — and a rescan that found
    /// nothing to do is the common case on a device.
    pub fn wrote_anything(&self) -> bool {
        self.indexed > 0 || self.meta_rows > 0 || self.removed > 0
    }
}

/// A build in progress.
///
/// Postings are written as books are read, one durable flush per step. The
/// metadata — which has to exist for a book to be findable by title — is held
/// until [`Session::finish`], so a caller that stops halfway leaves postings
/// nobody can reach rather than a book that is listed but has no text.
pub struct Session {
    /// What the scan decided still needs reading.
    todo: VecDeque<FileEntry>,
    /// Books whose file has gone, removed when the session ends.
    missing: Vec<(u16, String)>,
    /// The dictionary this session is adding to, and the one it stores back.
    ///
    /// Loaded rather than started empty. `Writer::close` applies the session's
    /// counts to whatever is in here and then *replaces* the stored dictionary
    /// with it, so an empty one is not "no changes" — it is a dictionary that no
    /// longer knows any of the tens of thousands of terms already indexed.
    dict: dict::Dict,
    records: Vec<(u16, BookMetaRecord)>,
    content_mode: ContentMode,
    stats: BuildStats,
}

impl Session {
    /// Opens a session over what `plan` found.
    pub fn new(engine: &Engine, plan: Plan, content_mode: ContentMode) -> RunResult<Self> {
        // The id mark is seeded from the rows themselves, because an index built
        // before anything allocated one has books and no mark: the next
        // allocation would start at zero and hand out an id that is already in
        // use, which overwrites a live book rather than adding one.
        engine.seed_next_id(engine.next_free_book_id()? as u32)?;
        Ok(Self {
            todo: plan.entries.into(),
            missing: plan.missing,
            dict: dict::load(engine)?,
            records: Vec::new(),
            content_mode,
            stats: BuildStats::default(),
        })
    }

    /// Files still waiting to be read.
    pub fn pending(&self) -> usize {
        self.todo.len()
    }

    /// Books whose file has gone, waiting to be dropped.
    ///
    /// Counted separately from [`Session::pending`] because a session with
    /// nothing to read is still worth finishing when there is something to drop:
    /// a folder where the last book was deleted has no work at all by one count
    /// and one removal by the other.
    pub fn gone(&self) -> usize {
        self.missing.len()
    }

    /// Reads and indexes at most `budget` files, and reports what is left.
    ///
    /// One writer per step rather than one for the whole session: the writer
    /// holds a reference to the engine and the dictionary is rewritten by its
    /// `close`, and both have to happen inside a call that returns to the
    /// caller. The dictionary stays loaded across steps, so the counts each
    /// `close` adds land on top of the previous one's rather than replacing it.
    pub fn step(&mut self, engine: &Engine, budget: usize) -> RunResult<usize> {
        if self.todo.is_empty() {
            return Ok(0);
        }
        let take = budget.min(self.todo.len());
        let batch: Vec<FileEntry> = self.todo.drain(..take).collect();

        let started = Instant::now();
        {
            let mut w = writer::Writer::new(engine)?;
            for entry in batch {
                self.index_one(engine, &mut w, entry)?;
            }
            w.close(&mut self.dict)?;
            self.stats.chunk_encode += w.time_encode();
            self.stats.postings_write += w.time_write();
            self.stats.writer_flush += w.time_flush();
        }
        let _ = started;
        Ok(self.todo.len())
    }

    /// Registers the metadata, rebuilds the metadata index, and compacts when
    /// anything was written.
    pub fn finish(mut self, engine: &Engine, compact: bool) -> RunResult<BuildStats> {
        let started = Instant::now();

        // Books under a root this pass could see that are no longer there. Their
        // postings become orphans and are skipped at read time; the row itself
        // has to go, or a search offers a file the reader cannot open.
        for (id, _) in &self.missing {
            if engine.remove_book(*id)? {
                self.stats.removed += 1;
            }
        }

        for (book_id, record) in &self.records {
            engine.put_metadata_batch(*book_id, record)?;
            self.stats.meta_rows += 1;
        }
        // Recorded only now, and only when something was written: the record
        // means "every book in here was read by a build of this layout", and a
        // session that read nothing did not make that true of the books it left
        // alone.
        if self.stats.indexed > 0 {
            engine.set_text_layout(crate::format::html::TEXT_LAYOUT)?;
        }
        // Everything above with `*_batch` is parked in memory until one durable
        // commit, and the metadata index is rebuilt from rows that have a live
        // `BOOKS` row — so it has to be read after that commit, not before.
        engine.flush()?;
        let index = rebuild_index(engine)?;
        engine.put_meta_index(&index)?;
        self.stats.meta_index_bytes = index.blob_len();

        // Compaction is the single biggest write in the build: it rewrites the
        // whole database to reclaim free pages. It belongs to the caller because
        // it trades a one-time write against a permanently smaller file, and on a
        // device that writes, that trade is not a detail.
        self.stats.reclaimed = compact && self.stats.wrote_anything() && engine.compact()?;
        self.stats.finish = started.elapsed();
        Ok(self.stats)
    }

    /// Reads one file and writes everything it contributes.
    fn index_one(
        &mut self,
        engine: &Engine,
        w: &mut writer::Writer<'_>,
        entry: FileEntry,
    ) -> RunResult<()> {
        // Sniff the container: the extension only decided whether the file was
        // worth opening at all.
        let format = sniff::detect(&entry.path);
        if !format.is_text() {
            self.stats.skipped += 1;
            return Ok(());
        }

        // `FileList` never opens a book: that is the entire point of the mode.
        // Indexing 600 books costs a `read_dir` and a metadata read, not 300 MB
        // of decoded HTML.
        if !self.content_mode.has_content() {
            let record = crate::format::meta::resolve(&entry.path);
            let book_id = next_id(engine)?;
            self.put_book(engine, book_id, &entry, format, 0, 0)?;
            engine.put_metadata_batch(book_id, &record)?;
            self.records.push((book_id, record));
            w.end_document()?;
            self.stats.indexed += 1;
            *self
                .stats
                .formats
                .entry(format!("{format:?} metadata-only"))
                .or_insert(0) += 1;
            return Ok(());
        }

        let t = Instant::now();
        let decoded = match load_sections(&entry.path, format) {
            Ok(d) => d,
            Err(e) => {
                self.stats.failed += 1;
                if self.stats.failures.len() < 5 {
                    self.stats
                        .failures
                        .push(format!("decode {}: {e}", entry.path.display()));
                }
                return Ok(());
            }
        };
        self.stats.decode += t.elapsed();
        self.stats.sections += decoded.sections.len();

        // Ids are never reused, so a book being re-indexed gets a fresh one —
        // which is what keeps the new postings sorting after every old chunk —
        // and the row it replaces is dropped by `put_book`.
        let book_id = next_id(engine)?;

        // One position counter for the whole book. Sections share it and are
        // told apart by `entry_idx`, which is what the chunk codec requires:
        // `pos_gap` is a delta, so restarting per section would make positions
        // non-monotonic within a document and corrupt the column.
        let t = Instant::now();
        let mut position = 0u32;
        let mut section_tokens = 0u64;
        // Every section's entry map, held until the book is done. Writing them
        // one at a time is one transaction per section, and a 12 000-section
        // library pays for twelve thousand of them.
        let mut maps: Vec<(u16, EntryMap)> = Vec::with_capacity(decoded.sections.len());
        for (entry_idx, text) in decoded.sections.iter().enumerate() {
            // Sections are decoded bytes; tokenizing needs text.
            let text = String::from_utf8_lossy(text).into_owned();
            let mut b = entrymap::Builder::new(entry_name(&entry.path, entry_idx));
            if self.content_mode.has_positions() {
                // The count comes from the same pass rather than a second
                // `tokenize::tokenize` call: that re-tokenized every section and
                // allocated a `Vec` of owned terms, which on the measured library
                // is ~1,2 GB of churn to print one number.
                position = for_each_token_spanned(&text, position, |term, p, start, _end| {
                    b.offer(p, start as u32);
                    let _ = w.add(term, book_id as u32, p, entry_idx as u16);
                    section_tokens += 1;
                });
            } else {
                // `TermsOnly`: one posting per term and book, which is what makes
                // the mode small — the same term repeated two hundred times in
                // one book still contributes exactly one posting.
                //
                // The position counter keeps running across sections, exactly as
                // in `Full`: `pos_gap` is a delta within a document, and resetting
                // it per section makes positions repeat, which underflows the
                // column and makes the term's whole posting list unreadable.
                let mut seen: BTreeMap<Vec<u8>, (u32, u16)> = BTreeMap::new();
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
            self.stats.text_bytes += text.len() as u64;
        }
        self.stats.tokens += section_tokens;
        self.stats.tokenize += t.elapsed();

        let t = Instant::now();
        for (entry_idx, map) in &maps {
            writer::put_entry_map(engine, book_id, *entry_idx, map)?;
        }
        self.stats.entrymap += t.elapsed();

        w.end_document()?;
        self.stats.indexed += 1;
        *self.stats.formats.entry(format!("{format:?}")).or_insert(0) += 1;

        // Metadata for every book, content or not: a PDF the decoder cannot read
        // still has a title worth searching for, and it must land under the same
        // book id the postings of readable books use.
        //
        // Sidecar first, exactly as `meta::resolve` orders it, but the embedded
        // half comes from the book just decoded instead of opening the file a
        // second time.
        let record = crate::format::meta::from_sidecar(&entry.path)
            .or_else(|| decoded.embedded.clone())
            .unwrap_or_else(|| crate::format::meta::from_filename(&entry.path));
        self.put_book(
            engine,
            book_id,
            &entry,
            format,
            decoded.sections.len() as u16,
            self.stats.tokens as u32,
        )?;
        self.records.push((book_id, record));
        Ok(())
    }

    /// Registers a book, dropping the row an earlier pass left for the same file.
    ///
    /// An updated file is re-indexed under a fresh id, so without this the path
    /// is in the table twice — once with the new text and once stale — and a
    /// search returns the same book twice, one of the two answers wrong.
    fn put_book(
        &self,
        engine: &Engine,
        book_id: u16,
        entry: &FileEntry,
        format: Format,
        entry_count: u16,
        token_count: u32,
    ) -> RunResult<()> {
        let path = entry.path.to_string_lossy().into_owned();
        // The old row is dropped only when it is genuinely a different book, so
        // a first index of a file finds nothing to drop rather than finding
        // itself and dropping it out from under the row about to be written.
        if let Some(old) = engine.book_id_by_path(&path)?.filter(|old| *old != book_id) {
            engine.remove_book(old)?;
        }
        engine.put_book_batch(
            book_id,
            &Book {
                path,
                scope: entry.scope.clone().unwrap_or_default(),
                size: entry.size,
                mtime_ms: entry.mtime_ms,
                inode: entry.inode,
                format,
                entry_count,
                token_count,
                indexed_at_ms: now_ms(),
                flags: 0,
            },
        )?;
        Ok(())
    }
}

/// The next id to hand out.
///
/// From the allocated mark rather than from how many books the session has fed
/// the writer: the writer's counter starts at zero on every session, and an
/// index that already holds 619 books would be given book 0 — overwriting a live
/// row and posting its text under an id whose chunks were written in a different
/// order.
fn next_id(engine: &Engine) -> RunResult<u16> {
    let ids = engine.allocate_ids(1)?;
    match ids.as_deref() {
        Some([id]) => Ok(*id),
        _ => Err(RunError::Message(
            "the index is full: no book ids are left to hand out".to_string(),
        )),
    }
}

/// The decoded book: its sections, and the metadata that was already open.
///
/// The metadata comes back with the sections rather than being resolved again
/// afterwards, because resolving it afterwards re-opens the file. That second
/// open cost 12,5 s per 100 books on the measured library, which is a whole
/// extra decode pass spent reading an OPF that was already in memory.
pub struct Decoded {
    pub sections: Vec<Vec<u8>>,
    /// Embedded metadata, when the container carried any.
    pub embedded: Option<BookMetaRecord>,
}

/// Reads a book's sections in reading order, as decoded text.
/// The sentence around a match, read from the book the match is in.
///
/// The index stores where a word is, never what it is: there is no sentence
/// in SQLite to recover, only a position and the entry map that leads to it.
/// So this is the one call that pays for a decode, and paying it per question
/// rather than per result is what keeps the screen opening — ten rows would
/// be ten decodes of books the reader has not said yes to.
///
/// `None` is not a failure. A book whose sections moved since it was indexed,
/// or whose book row is gone, has no sentence to show, and a reader shown the
/// book itself is shown something that still opens.
pub fn snippet_for(
    engine: &Engine,
    doc_id: u32,
    entry_idx: u16,
    position: u32,
    radius: usize,
) -> RunResult<Option<String>> {
    let Ok(book_id) = u16::try_from(doc_id) else {
        return Ok(None);
    };
    let Some(book) = engine.book(book_id)? else {
        return Ok(None);
    };
    let decoded = load_sections(Path::new(&book.path), book.format)?;
    let Some(section) = decoded.sections.get(entry_idx as usize) else {
        return Ok(None);
    };
    let text = String::from_utf8_lossy(section).into_owned();
    // A `Hit` without the score: `hit_snippet` only reads the section and the
    // position out of it, and this is a hit's positions being asked about
    // through the FFI rather than a result of a search.
    let hit = query::Hit {
        doc_id,
        score: 0,
        first_position: position,
        entry_idx,
    };
    Ok(query::hit_snippet(
        engine,
        doc_id,
        &hit,
        &|_doc, _entry| Some(text.clone()),
        radius,
    ))
}

pub fn load_sections(path: &Path, format: Format) -> RunResult<Decoded> {
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

/// Name recorded in `ENTRYMAP`, carrying the source path so snippets can be
/// traced back to a file.
pub fn entry_name(path: &Path, idx: usize) -> String {
    format!("{}#{idx}", path.display())
}

/// Wall-clock milliseconds, for `Book.indexed_at_ms`.
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
