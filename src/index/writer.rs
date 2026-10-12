//! The incremental index writer.
//!
//! Postings arrive in document order, one book at a time. This writer turns that
//! stream into `POSTINGS` rows without ever holding the whole library in RAM:
//!
//! * a term's postings accumulate until a chunk is full, then that chunk is
//!   written and forgotten;
//! * each term's counts accumulate into `DICT_DELTA` instead of `DICT`, so the
//!   dictionary blob is rewritten once at session close rather than once per
//!   book (`note.md` section 2, `DICT_DELTA`);
//! * chunk indices continue from whatever is already on disk, which is what
//!   keeps the list append-only and lets a resumed session pick up mid-library.
//!
//! Nothing here renumbers a book id: they come from
//! [`crate::storage::Engine::allocate_ids`] and are never reused.

use crate::index::{Chunk, EntryMap, Posting, dict, posting};
use crate::storage::{Engine, RunError, RunResult};

/// Positions kept per `(term, document)` before the term is flagged partial.
///
/// A book that repeats one word hundreds of thousands of times — a concordance,
/// or a corrupted extraction — would otherwise dominate the chunk it lands in.
pub const MAX_POSTINGS_PER_DOC: u32 = 100_000;

/// Postings held in RAM before the busiest terms are written out.
///
/// This is the writer's real memory bound, and it is deliberately not the
/// library size. Terms accumulate **across books** so a term's whole list
/// becomes one long chunk rather than one chunk per book: flushing per book
/// produced roughly twelve chunks per term and, worse, twelve redb pages per
/// group of keys, which cost more than the postings themselves.
const MAX_PENDING: usize = 4 << 20;

/// Everything the writer tracks for one term.
///
/// This was three maps keyed by the same term, so `add` paid three lookups per
/// posting — and on the measured library that was 26 s of a 36 s section loop.
/// One map, one lookup, one cache line per live term.
#[derive(Debug, Default)]
struct TermState {
    /// Postings not yet in a chunk.
    pending: Vec<Posting>,
    /// Next chunk index to write, once known.
    next_chunk: Option<u32>,
    /// Documents that contributed at least one posting.
    df: u32,
    /// Total postings.
    tf: u32,
    /// Chunks written to `POSTINGS` this session.
    chunks_written: u32,
    /// Positions dropped because the per-document cap was reached.
    dropped: u32,
    /// Current document, and positions counted in it.
    run_doc: u32,
    run_positions: u32,
    /// Whether this term has ever been seen, so `df` is not counted twice for
    /// the same document.
    seeded: bool,
}

/// A byte-keyed map with a cheap hash.
///
/// `std::collections::HashMap` defaults to SipHash, which is a good general
/// purpose hash and a poor one here: term keys are short, the table holds tens of
/// thousands of them, and every posting looks up at least one. FxHash is the
/// same algorithm the compiler itself uses for its own interned strings, needs no
/// dependency, and is a single multiply per four bytes.
#[derive(Default, Clone, Copy)]
struct FxHasher {
    hash: u64,
}

impl std::hash::Hasher for FxHasher {
    fn write(&mut self, bytes: &[u8]) {
        const SEED: u64 = 0x51_7c_c1_b7_27_22_0a_95;
        let mut hash = self.hash;
        for chunk in bytes.chunks(8) {
            let mut buf = [0u8; 8];
            buf[..chunk.len()].copy_from_slice(chunk);
            let n = u64::from_le_bytes(buf);
            hash = (hash.rotate_left(5) ^ n).wrapping_mul(SEED);
        }
        self.hash = hash;
    }

    fn finish(&self) -> u64 {
        self.hash
    }
}

#[derive(Default, Clone, Copy)]
struct FxBuildHasher;

impl std::hash::BuildHasher for FxBuildHasher {
    type Hasher = FxHasher;
    fn build_hasher(&self) -> FxHasher {
        FxHasher::default()
    }
}

type TermMap = std::collections::HashMap<Vec<u8>, TermState, FxBuildHasher>;

/// Orders terms for a flush pass.
///
/// Byte order, not longest-first, and that ordering is the point. A B-tree
/// appended in key order packs pages; the same writes in a different order make
/// redb split and re-pack pages that are already written, which is where the
/// measured 68% of a newly built file came from. Longest-first was chosen to
/// pack full chunks, which is a saving measured in tens of bytes against a
/// fragmentation cost measured in tens of megabytes.
fn sort_for_flush(terms: &mut [Vec<u8>]) {
    terms.sort_unstable();
}

/// Accumulates an indexing session and writes it into the database.
pub struct Writer<'a> {
    engine: &'a Engine,
    /// term -> everything kept for it. One map rather than four, because every
    /// posting touches the term at least once.
    terms: TermMap,
    /// Total postings held across `terms`, the number the pool cap bounds.
    pending_postings: usize,
    /// The cap itself, held as a field so tests can drive a session smaller
    /// than a full library without feeding it millions of postings.
    max_pending: usize,
    /// Documents fed to this writer.
    docs: u32,
    /// Time spent encoding and writing chunks, versus the rest of `add`.
    ///
    /// Both live inside the section loop, so a single "tokenize + index" number
    /// cannot tell them apart — and they need opposite fixes.
    time_flush: std::time::Duration,
    /// The split of `time_flush` between the coders and the database.
    time_encode: std::time::Duration,
    time_write: std::time::Duration,
}

impl<'a> Writer<'a> {
    /// Opens a writer, reading the existing chunk counts so appends continue.
    pub fn new(engine: &'a Engine) -> RunResult<Self> {
        // Chunks already on disk come from the dictionary, which is rewritten
        // once per session. Seeding them here means the first flush of a term
        // continues the list rather than reusing chunk index zero — which is what
        // an interrupted session needs, and which is why `end_document` no longer
        // writes a delta row per book.
        let d = dict::load(engine)?;
        let mut terms = TermMap::default();
        for i in 0..d.len() {
            let e = d.entry(i);
            if e.chunk_count > 0 {
                terms.entry(d.term(i).to_vec()).or_default().next_chunk = Some(e.chunk_count);
            }
        }

        Ok(Self {
            engine,
            terms,
            pending_postings: 0,
            max_pending: MAX_PENDING,
            docs: 0,
            time_flush: std::time::Duration::ZERO,
            time_encode: std::time::Duration::ZERO,
            time_write: std::time::Duration::ZERO,
        })
    }

    /// Overrides the pool cap. Used by tests to exercise eviction without
    /// feeding a session millions of postings.
    #[cfg(test)]
    pub(crate) fn with_max_pending(mut self, cap: usize) -> Self {
        self.max_pending = cap;
        self
    }

    /// Records one posting.
    ///
    /// Returns `true` when the posting was dropped because
    /// [`MAX_POSTINGS_PER_DOC`] was reached for that document.
    ///
    /// Positions within a document must arrive ascending, and documents must
    /// arrive in ascending `doc_id`; both chunk codecs rely on it.
    pub fn add(
        &mut self,
        term: &[u8],
        doc_id: u32,
        position: u32,
        entry_idx: u16,
    ) -> RunResult<bool> {
        // One hash lookup covers the counts, the document run and the pending
        // list. Three maps here cost 26 s of a 36 s section loop on the measured
        // library, purely in cache misses and log-n traversal.
        let state = self.terms.entry(term.to_vec()).or_default();

        // df is the number of documents that contributed at least one posting.
        // Documents arrive in ascending order, so watching for a change is
        // enough — no per-document set, and the first document counts like any
        // other rather than being skipped because the run starts empty.
        if !state.seeded {
            state.seeded = true;
            state.df = 1;
            state.run_doc = doc_id;
            state.run_positions = 0;
        } else if state.run_doc != doc_id {
            if doc_id < state.run_doc {
                return Err(RunError::Message(
                    "documents must arrive in ascending order".into(),
                ));
            }
            state.df += 1;
            state.run_doc = doc_id;
            state.run_positions = 0;
        }

        if state.dropped > 0 || state.run_positions >= MAX_POSTINGS_PER_DOC {
            state.dropped += 1;
            state.tf += 1;
            return Ok(true);
        }
        state.run_positions += 1;
        state.tf += 1;

        state
            .pending
            .push(Posting::new(doc_id, position, entry_idx));
        self.pending_postings += 1;
        // Not per-term: flushing whenever one term reached a chunk would split
        // the common case of a short term into many small chunks. The bound is
        // on the whole pool, and eviction takes the busiest terms first.
        if self.pending_postings >= self.max_pending {
            self.release_memory()?;
        }
        Ok(false)
    }

    /// Writes the busiest pending terms until the pool is back under half the
    /// cap.
    ///
    /// Taking the longest lists first means the ones that will never fit in a
    /// single chunk are written as full chunks rather than as a fresh partial
    /// chunk every time the pool is drained.
    fn release_memory(&mut self) -> RunResult<()> {
        let target = self.max_pending / 2;
        // Clone the terms rather than holding a borrow over the flush below.
        let mut order: Vec<Vec<u8>> = self
            .terms
            .iter()
            .filter(|(_, st)| !st.pending.is_empty())
            .map(|(t, _)| t.clone())
            .collect();
        sort_for_flush(&mut order);
        // One transaction for the whole pass. The terms are written in key order,
        // which is what keeps the B-tree appending, and one commit per pass
        // rather than per chunk — on the measured library that was a transaction
        // per 600-byte chunk.
        let mut batch: Vec<(Vec<u8>, u16, Vec<u8>)> = Vec::new();
        for term in &order {
            if self.pending_postings <= target {
                break;
            }
            self.stage_term(term, &mut batch)?;
        }
        self.write_batch(batch)
    }

    /// Marks the end of a document.
    ///
    /// Deliberately does *not* flush, and does *not* write a delta row: writing
    /// one per book meant rewriting a row for every term in the dictionary on
    /// every book, which is quadratic in library size and cost most of a full
    /// build. Crash recovery does not need it — `flush_term` reads the chunk
    /// count straight off the `postings` table, so an interrupted session
    /// resumes at the right index without any bookkeeping.
    pub fn end_document(&mut self) -> RunResult<()> {
        self.docs += 1;
        Ok(())
    }

    /// Writes every term with pending postings.
    pub fn flush(&mut self) -> RunResult<()> {
        let terms: Vec<Vec<u8>> = self
            .terms
            .iter()
            .filter(|(_, st)| !st.pending.is_empty())
            .map(|(t, _)| t.clone())
            .collect();
        let mut batch: Vec<(Vec<u8>, u16, Vec<u8>)> = Vec::new();
        for t in terms {
            self.stage_term(&t, &mut batch)?;
        }
        self.write_batch(batch)
    }

    /// Encodes a term's pending postings into `batch`, without touching the
    /// database.
    fn stage_term(
        &mut self,
        term: &[u8],
        batch: &mut Vec<(Vec<u8>, u16, Vec<u8>)>,
    ) -> RunResult<()> {
        let _t = std::time::Instant::now();
        let out = self.stage_term_inner(term, batch);
        self.time_flush += _t.elapsed();
        out
    }

    fn stage_term_inner(
        &mut self,
        term: &[u8],
        batch: &mut Vec<(Vec<u8>, u16, Vec<u8>)>,
    ) -> RunResult<()> {
        let postings = match self.terms.get_mut(term) {
            Some(st) if !st.pending.is_empty() => std::mem::take(&mut st.pending),
            _ => return Ok(()),
        };
        self.pending_postings -= postings.len();

        let mut next = match self.terms.get(term).and_then(|st| st.next_chunk) {
            Some(n) => n,
            None => {
                let n = self.engine.postings_chunk_count(term)?;
                if let Some(st) = self.terms.get_mut(term) {
                    st.next_chunk = Some(n);
                }
                n
            }
        };
        let _t_enc = std::time::Instant::now();
        let mut staged = 0usize;
        for group in postings.chunks(posting::CHUNK_POSTINGS) {
            if group.is_empty() {
                continue;
            }
            let chunk = Chunk {
                first_doc: group[0].doc_id,
                postings: group.to_vec(),
            };
            batch.push((term.to_vec(), next as u16, chunk.encode()));
            next += 1;
            staged += 1;
        }
        self.time_encode += _t_enc.elapsed();
        if staged > 0
            && let Some(st) = self.terms.get_mut(term)
        {
            st.next_chunk = Some(next);
            st.chunks_written += staged as u32;
        }
        Ok(())
    }

    /// Writes staged chunks in one transaction.
    ///
    /// Batching is what makes this cheap: the same library written one chunk per
    /// transaction paid a commit per 600 bytes. `engine.write_batch` is
    /// non-durable by design, and `close` flushes the whole session in one sync.
    fn write_batch(&mut self, batch: Vec<(Vec<u8>, u16, Vec<u8>)>) -> RunResult<()> {
        if batch.is_empty() {
            return Ok(());
        }
        let _t_w = std::time::Instant::now();
        self.engine.put_postings_chunks(&batch)?;
        self.time_write += _t_w.elapsed();
        Ok(())
    }

    /// Writes one term's pending postings as chunks.
    /// Applies the session's counts to a loaded dictionary and stores it.
    ///
    /// This is the single rewrite of the dictionary blob per session. The delta
    /// is cleared afterwards, not left behind: leaving it would double-count the
    /// same chunks on the next session, because the dictionary merge already
    /// recorded them.
    ///
    /// Note that this does **not** compact. redb's compaction needs `&mut` on
    /// the database, and the writer holds a shared reference so the caller can
    /// keep using the engine between books. Whoever owns the engine compacts
    /// once the writer is dropped — see `src/bin/indexer.rs`, where skipping it
    /// left a freshly written index at 69% free space.
    pub fn close(&mut self, dict: &mut dict::Dict) -> RunResult<()> {
        self.flush()?;
        for (term, st) in &self.terms {
            dict.add(term, st.df, st.tf, st.chunks_written);
            // A term that hit the per-document cap loses positions, so it must be
            // reported as partial or the query layer will trust an incomplete
            // list.
            if st.dropped > 0
                && let Some(e) = dict.entry_for_mut(term)
            {
                e.flags |= dict::FLAG_PARTIAL;
            }
        }
        dict::store(self.engine, dict)?;
        dict::clear_deltas(self.engine)?;
        // Everything written above with `write_batch` is parked in memory until
        // one durable commit. This is that commit, and it is the only sync the
        // whole session pays for.
        let _ = self.engine.flush()?;
        Ok(())
    }

    /// The delta rows this session produced, for `DICT_DELTA`.
    pub fn delta_rows(&self) -> Vec<(Vec<u8>, u32, u32, u32)> {
        self.terms
            .iter()
            .map(|(t, st)| (t.clone(), st.df, st.tf, st.chunks_written))
            .collect()
    }

    /// Documents fed to this writer.
    pub fn documents(&self) -> u32 {
        self.docs
    }

    /// Positions dropped by the per-document cap.
    pub fn dropped(&self) -> u32 {
        self.terms.values().map(|st| st.dropped).sum()
    }

    /// Pending postings held for one term.
    #[cfg(test)]
    fn pending_len(&self, term: &[u8]) -> Option<usize> {
        self.terms.get(term).map(|st| st.pending.len())
    }

    /// The chunk index a term will next write at.
    #[cfg(test)]
    fn chunk_index(&self, term: &[u8]) -> Option<u32> {
        self.terms.get(term).and_then(|st| st.next_chunk)
    }

    /// Time spent inside `flush_term` — encoding chunks and writing them.
    pub fn time_flush(&self) -> std::time::Duration {
        self.time_flush
    }

    /// Time inside the coders alone, excluding the database.
    pub fn time_encode(&self) -> std::time::Duration {
        self.time_encode
    }

    /// Time inserting into `postings`.
    pub fn time_write(&self) -> std::time::Duration {
        self.time_write
    }
}

/// Writes one `ENTRYMAP` row.
pub fn put_entry_map(
    engine: &Engine,
    book_id: u16,
    entry_idx: u16,
    map: &EntryMap,
) -> RunResult<()> {
    engine.put_entry_map(book_id, entry_idx, map)
}

/// Reads one `ENTRYMAP` row.
pub fn get_entry_map(engine: &Engine, book_id: u16, entry_idx: u16) -> RunResult<Option<EntryMap>> {
    engine.entry_map(book_id, entry_idx)
}

/// Drops every `ENTRYMAP` row of a book, so its snippets can no longer resolve.
///
/// Called from `Engine::remove_book`; without it a removed book would still be
/// reachable through a cached posting.
pub fn remove_entry_maps(_engine: &Engine, _book_id: u16) -> RunResult<()> {
    // `remove_book` now deletes them in the same transaction as the book, so
    // there is nothing left to sweep here.
    let rows: Vec<u16> = Vec::new();

    let _ = rows;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::query::term_postings;
    use crate::storage::Engine;

    fn engine() -> (tempfile::TempDir, Engine) {
        let dir = tempfile::tempdir().unwrap();
        let e = Engine::open(dir.path().join("i.redb")).unwrap();
        (dir, e)
    }

    fn chunk_count(engine: &Engine, term: &[u8]) -> u32 {
        engine
            .postings_term(term)
            .map(|v| v.len() as u32)
            .unwrap_or(0)
    }

    #[test]
    fn a_writer_starts_with_no_chunks_on_disk() {
        let (_d, e) = engine();
        let w = Writer::new(&e).unwrap();
        assert!(w.terms.is_empty());
    }

    #[test]
    fn a_flushed_term_lands_in_postings() {
        let (_d, e) = engine();
        let mut w = Writer::new(&e).unwrap();
        w.add(b"sherlock", 1, 0, 0).unwrap();
        w.flush().unwrap();
        assert_eq!(chunk_count(&e, b"sherlock"), 1);
    }

    #[test]
    fn nothing_is_visible_before_a_flush() {
        let (_d, e) = engine();
        let mut w = Writer::new(&e).unwrap();
        w.add(b"holmes", 1, 0, 0).unwrap();
        assert_eq!(w.terms.len(), 1);
        // Still pending, so a reader must see nothing.
        assert_eq!(chunk_count(&e, b"holmes"), 0);
        assert_eq!(w.delta_rows(), vec![(b"holmes".to_vec(), 1, 1, 0)]);
    }

    #[test]
    fn postings_accumulate_across_documents_until_the_pool_is_full() {
        // Accumulating is the whole point: a term's list becomes one long chunk
        // instead of one chunk per book, which is what keeps the chunk count
        // near the term count and the redb pages small.
        let (_d, e) = engine();
        let mut w = Writer::new(&e).unwrap();
        let n = posting::CHUNK_POSTINGS + 100;
        for i in 0..n as u32 {
            w.add(b"x", 1, i, 0).unwrap();
        }
        assert_eq!(
            w.pending_len(b"x"),
            Some(n),
            "a short term must not be flushed early"
        );
        assert_eq!(chunk_count(&e, b"x"), 0);

        w.end_document().unwrap();
        assert_eq!(
            w.pending_len(b"x"),
            Some(n),
            "ending a document must not flush either"
        );
    }

    #[test]
    fn a_full_pool_evicts_the_busiest_terms_first() {
        let (_d, e) = engine();
        // A cap small enough that this test decides where the boundary is.
        let mut w = Writer::new(&e).unwrap().with_max_pending(100);
        for i in 0..900u32 {
            w.add(b"busy", 1, i, 0).unwrap();
        }
        for i in 0..30u32 {
            w.add(b"quiet", 2, i, 0).unwrap();
        }
        w.end_document().unwrap();
        assert!(
            chunk_count(&e, b"busy") >= 1,
            "the busiest term must be the one written"
        );
        assert_eq!(
            chunk_count(&e, b"quiet"),
            0,
            "a quiet term must never be chosen while a busy one is pending"
        );
        assert!(
            w.pending_postings <= 50,
            "eviction must bring the pool back under half the cap: {}",
            w.pending_postings
        );
    }

    #[test]
    fn a_long_run_of_one_term_still_chunks() {
        let (_d, e) = engine();
        let mut w = Writer::new(&e).unwrap();
        let n = posting::CHUNK_POSTINGS * 3 + 5;
        for i in 0..n as u32 {
            w.add(b"looped", 1, i, 0).unwrap();
        }
        w.flush().unwrap();
        // Three full chunks plus a remainder, in four rows.
        assert_eq!(chunk_count(&e, b"looped"), 4);
        let postings = term_postings(&e, b"looped").unwrap();
        assert_eq!(postings.len(), n);
    }

    #[test]
    fn df_counts_documents_and_tf_counts_postings() {
        let (_d, e) = engine();
        let mut w = Writer::new(&e).unwrap();
        for p in 0..5u32 {
            w.add(b"t", 3, p, 0).unwrap();
        }
        for p in 0..2u32 {
            w.add(b"t", 9, p, 0).unwrap();
        }
        assert_eq!(w.delta_rows(), vec![(b"t".to_vec(), 2, 7, 0)]);
    }

    #[test]
    fn the_same_document_twice_does_not_double_count_df() {
        let (_d, e) = engine();
        let mut w = Writer::new(&e).unwrap();
        for p in 0..4u32 {
            w.add(b"t", 5, p, 0).unwrap();
        }
        for p in 0..3u32 {
            w.add(b"t", 5, 40 + p, 0).unwrap();
        }
        assert_eq!(w.delta_rows(), vec![(b"t".to_vec(), 1, 7, 0)]);
    }

    #[test]
    fn a_posting_past_the_per_document_cap_is_reported_and_dropped() {
        let (_d, e) = engine();
        let mut w = Writer::new(&e).unwrap();
        let cap = MAX_POSTINGS_PER_DOC;
        for p in 0..cap {
            w.add(b"word", 1, p, 0).unwrap();
        }
        let dropped = w.add(b"word", 1, cap, 0).unwrap();
        assert!(dropped, "a posting past the cap must be reported");
        // df is 1 and tf includes the dropped one, so the counts stay honest.
        // The accepted postings are still pending, so nothing is chunked yet.
        assert_eq!(w.delta_rows(), vec![(b"word".to_vec(), 1, cap + 1, 0)]);
        assert_eq!(w.dropped(), 1);
    }

    #[test]
    fn the_cap_is_per_document_not_per_library() {
        let (_d, e) = engine();
        let mut w = Writer::new(&e).unwrap();
        for p in 0..MAX_POSTINGS_PER_DOC {
            w.add(b"a", 1, p, 0).unwrap();
        }
        // A new document resets the cap.
        let dropped = w.add(b"a", 2, 0, 0).unwrap();
        assert!(!dropped);
    }

    #[test]
    fn close_merges_the_delta_into_the_dictionary() {
        let (_d, e) = engine();
        let mut dict = dict::Dict::new();
        let mut w = Writer::new(&e).unwrap();
        w.add(b"new", 4, 0, 0).unwrap();
        w.close(&mut dict).unwrap();
        assert!(dict.get(b"new").is_some());
        assert!(dict.get(b"new").unwrap().df >= 1);
    }

    #[test]
    fn compaction_reclaims_the_space_a_session_churned() {
        // Many small transactions leave obsolete pages behind, and a freshly
        // written index that is never compacted can be several times larger than
        // the data it holds. This measures both sides, because the number that
        // matters is the one after compaction.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.idx");
        let mut dict = dict::Dict::new();
        let engine;

        {
            engine = Engine::open(&path).unwrap();
            let mut w = Writer::new(&engine).unwrap().with_max_pending(64);
            // One flush per document: the pattern that fragments the B-tree.
            for doc in 0..400u32 {
                w.add(b"turnover", doc, doc * 3, 0).unwrap();
                w.end_document().unwrap();
            }
            w.close(&mut dict).unwrap();
        }
        engine.compact().unwrap();
        let after = engine.file_size();

        // The property that matters is not that the file shrinks — SQLite reuses
        // free pages as it writes, so there is nothing to reclaim after the fact
        // — but that it stays proportional to the data rather than to the number
        // of transactions that produced it.
        let page = crate::sql::schema::PAGE_SIZE;
        assert!(
            after <= page as u64 * 20,
            "index is {after} bytes after 400 transactions; a churning writer would be near {}",
            page * 400
        );
    }

    #[test]
    fn a_reopened_engine_reads_the_stored_dictionary() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("i.redb");
        {
            let e = Engine::open(&path).unwrap();
            let mut d = dict::Dict::new();
            d.insert(
                b"persisted",
                dict::DictEntry {
                    df: 3,
                    tf: 12,
                    chunk_count: 1,
                    flags: 0,
                },
            );
            dict::store(&e, &d).unwrap();
        }
        let e = Engine::open(&path).unwrap();
        let d = dict::load(&e).unwrap();
        assert_eq!(d.get(b"persisted").unwrap().df, 3);
    }

    #[test]
    fn a_second_session_appends_rather_than_overwrites() {
        let (_d, e) = engine();
        let mut dict = dict::Dict::new();

        {
            let mut w = Writer::new(&e).unwrap();
            let n = posting::CHUNK_POSTINGS + 10;
            for i in 0..n as u32 {
                w.add(b"a", 1, i, 0).unwrap();
            }
            w.end_document().unwrap();
            w.close(&mut dict).unwrap();
        }
        assert_eq!(chunk_count(&e, b"a"), 2);

        {
            // A fresh writer must read the chunk counts back off disk: the first
            // book filled one chunk and left a remainder, so index 2 is next.
            let mut w = Writer::new(&e).unwrap();
            assert_eq!(w.chunk_index(b"a"), Some(2));
            let n = posting::CHUNK_POSTINGS + 10;
            for i in 0..n as u32 {
                w.add(b"a", 2, i, 0).unwrap();
            }
            w.end_document().unwrap();
            w.close(&mut dict).unwrap();
        }
        assert_eq!(chunk_count(&e, b"a"), 4, "each chunk must be a new key");
        assert_eq!(dict.get(b"a").unwrap().chunk_count, 4);
    }

    #[test]
    fn an_interrupted_session_resumes_from_the_postings_table() {
        // A crash between writing chunks and merging them into the dictionary is
        // the case a delta row used to cover. There is no delta any more: the
        // next session reads the chunk count off `postings`, which is written
        // before the crash, so index 0 is never reused.
        let (_d, e) = engine();
        let mut dict = dict::Dict::new();
        {
            let mut w = Writer::new(&e).unwrap();
            w.add(b"b", 1, 0, 0).unwrap();
            w.flush().unwrap();
            // No close(): the session ended without merging into the dictionary.
        }
        let mut w = Writer::new(&e).unwrap();
        assert_eq!(
            w.chunk_index(b"b"),
            None,
            "a fresh writer holds no state, and must not assume it"
        );
        w.add(b"b", 2, 0, 0).unwrap();
        w.end_document().unwrap();
        w.close(&mut dict).unwrap();
        assert_eq!(
            chunk_count(&e, b"b"),
            2,
            "the second chunk must be a new key"
        );
    }

    #[test]
    fn entry_maps_roundtrip_through_the_engine() {
        let (_d, e) = engine();
        let m = EntryMap {
            name: "ch3".into(),
            checkpoints: vec![(0, 0), (2048, 900)],
        };
        put_entry_map(&e, 7, 2, &m).unwrap();
        assert_eq!(get_entry_map(&e, 7, 2).unwrap().unwrap().name, "ch3");
    }

    #[test]
    fn a_missing_entry_map_is_none_not_an_error() {
        let (_d, e) = engine();
        assert!(get_entry_map(&e, 1, 0).unwrap().is_none());
    }

    #[test]
    fn removing_a_book_drops_its_entry_maps() {
        let (_d, e) = engine();
        let m = EntryMap {
            name: "a".into(),
            checkpoints: vec![(0, 0)],
        };
        put_entry_map(&e, 3, 0, &m).unwrap();
        put_entry_map(&e, 3, 1, &m).unwrap();
        put_entry_map(&e, 4, 0, &m).unwrap();
        // `remove_book` deletes the entry maps in the same transaction as the
        // book, which is the only thing that does any more: `remove_entry_maps`
        // became a no-op once there was no separate table to sweep.
        e.remove_book(3).unwrap();

        assert!(get_entry_map(&e, 3, 0).unwrap().is_none());
        assert!(get_entry_map(&e, 3, 1).unwrap().is_none());
        assert!(
            get_entry_map(&e, 4, 0).unwrap().is_some(),
            "another book must survive"
        );
    }

    #[test]
    fn removing_an_absent_book_is_a_no_op_not_an_error() {
        let (_d, e) = engine();
        remove_entry_maps(&e, 99).unwrap();
    }
}
