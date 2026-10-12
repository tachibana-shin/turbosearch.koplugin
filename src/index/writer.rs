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

use std::collections::BTreeMap;

use crate::index::{Chunk, EntryMap, Posting, dict, posting};
use crate::storage::{Engine, RunResult, entrymap_table, postings_table};
use redb::ReadableTable;

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

/// What one term accumulated during the session.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct TermCounts {
    /// Documents that contributed at least one posting.
    df: u32,
    /// Total postings.
    tf: u32,
    /// Chunks written to `POSTINGS` this session.
    chunks_written: u32,
    /// Positions dropped because the per-document cap was reached.
    dropped: u32,
}

/// Accumulates an indexing session and writes it into the database.
pub struct Writer<'a> {
    engine: &'a Engine,
    /// term -> postings not yet in a chunk.
    pending: BTreeMap<Vec<u8>, Vec<Posting>>,
    /// Total postings across `pending`, the number the pool cap bounds.
    pending_postings: usize,
    /// The cap itself, held as a field so tests can drive a session smaller
    /// than a full library without feeding it millions of postings.
    max_pending: usize,
    /// term -> next chunk index to write.
    next_chunk: BTreeMap<Vec<u8>, u32>,
    /// term -> session counts.
    counts: BTreeMap<Vec<u8>, TermCounts>,
    /// term -> (current document, positions counted in it), for df and the cap.
    runs: BTreeMap<Vec<u8>, (u32, u32)>,
    /// Documents fed to this writer.
    docs: u32,
}

impl<'a> Writer<'a> {
    /// Opens a writer, reading the existing chunk counts so appends continue.
    pub fn new(engine: &'a Engine) -> RunResult<Self> {
        let mut next_chunk = BTreeMap::new();

        // Chunks already on disk come from two places: the dictionary, which is
        // rewritten once per session, and the delta from a session that was
        // interrupted before its merge. Both are needed for the next index to be
        // append-only rather than a rewrite.
        let d = dict::load(engine)?;
        for i in 0..d.len() {
            let e = d.entry(i);
            if e.chunk_count > 0 {
                next_chunk.insert(d.term(i).to_vec(), e.chunk_count);
            }
        }
        for (term, _df, _tf, chunks) in dict::load_deltas(engine)? {
            let n = next_chunk.entry(term).or_insert(0);
            *n = n.saturating_add(chunks);
        }

        Ok(Self {
            engine,
            pending: BTreeMap::new(),
            pending_postings: 0,
            max_pending: MAX_PENDING,
            next_chunk,
            counts: BTreeMap::new(),
            runs: BTreeMap::new(),
            docs: 0,
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
        let c = self.counts.entry(term.to_vec()).or_default();

        // df counts a document once. Documents arrive in ascending order, so
        // watching for a change of doc is enough — no per-document set needed.
        let run = self.runs.entry(term.to_vec()).or_insert((0, 0));
        if run.0 != doc_id {
            if doc_id > run.0 {
                c.df += 1;
            }
            *run = (doc_id, 0);
        }

        if c.dropped > 0 || run.1 >= MAX_POSTINGS_PER_DOC {
            c.dropped += 1;
            c.tf += 1;
            return Ok(true);
        }
        run.1 += 1;
        c.tf += 1;

        let v = self.pending.entry(term.to_vec()).or_default();
        v.push(Posting::new(doc_id, position, entry_idx));
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
        let mut order: Vec<Vec<u8>> = self.pending.keys().cloned().collect();
        order.sort_unstable_by_key(|t| {
            std::cmp::Reverse(self.pending.get(t.as_slice()).map(Vec::len).unwrap_or(0))
        });

        for term in &order {
            if self.pending_postings <= target {
                break;
            }
            self.flush_term(term)?;
        }
        Ok(())
    }

    /// Marks the end of a document.
    ///
    /// Deliberately does *not* flush: writing a chunk per book is what inflated
    /// both the chunk count and the redb pages. A crash between books loses the
    /// unflushed tail of this one document, which the next scan re-indexes.
    pub fn end_document(&mut self) -> RunResult<()> {
        self.docs += 1;
        dict::store_deltas(self.engine, &self.delta_rows())
    }

    /// Writes every term with pending postings.
    pub fn flush(&mut self) -> RunResult<()> {
        let terms: Vec<Vec<u8>> = self.pending.keys().cloned().collect();
        for t in terms {
            self.flush_term(&t)?;
        }
        Ok(())
    }

    /// Writes one term's pending postings as chunks.
    fn flush_term(&mut self, term: &[u8]) -> RunResult<()> {
        let Some(postings) = self.pending.remove(term) else {
            return Ok(());
        };
        self.pending_postings -= postings.len();

        let mut next = self.next_chunk.get(term).copied().unwrap_or(0);
        let mut writes: Vec<(u16, Vec<u8>)> =
            Vec::with_capacity(postings.len() / posting::CHUNK_POSTINGS + 1);

        for group in postings.chunks(posting::CHUNK_POSTINGS) {
            if group.is_empty() {
                continue;
            }
            let chunk = Chunk {
                first_doc: group[0].doc_id,
                postings: group.to_vec(),
            };
            writes.push((next as u16, chunk.encode()));
            next += 1;
        }

        if writes.is_empty() {
            return Ok(());
        }
        self.engine.write(|txn| {
            let mut t = txn.open_table(postings_table())?;
            for (idx, blob) in &writes {
                t.insert((term, *idx), blob.as_slice())?;
            }
            Ok(())
        })?;

        self.next_chunk.insert(term.to_vec(), next);
        self.counts.entry(term.to_vec()).or_default().chunks_written += writes.len() as u32;
        Ok(())
    }

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
        for (term, c) in &self.counts {
            dict.add(term, c.df, c.tf, c.chunks_written);
        }
        // A term that hit the per-document cap loses positions, so it must be
        // reported as partial or the query layer will trust an incomplete list.
        for (term, c) in &self.counts {
            if c.dropped > 0
                && let Some(e) = dict.entry_for_mut(term)
            {
                e.flags |= dict::FLAG_PARTIAL;
            }
        }
        dict::store(self.engine, dict)?;
        dict::clear_deltas(self.engine)
    }

    /// The delta rows this session produced, for `DICT_DELTA`.
    pub fn delta_rows(&self) -> Vec<(Vec<u8>, u32, u32, u32)> {
        self.counts
            .iter()
            .map(|(t, c)| (t.clone(), c.df, c.tf, c.chunks_written))
            .collect()
    }

    /// Documents fed to this writer.
    pub fn documents(&self) -> u32 {
        self.docs
    }

    /// Positions dropped by the per-document cap.
    pub fn dropped(&self) -> u32 {
        self.counts.values().map(|c| c.dropped).sum()
    }
}

/// Writes one `ENTRYMAP` row.
pub fn put_entry_map(
    engine: &Engine,
    book_id: u16,
    entry_idx: u16,
    map: &EntryMap,
) -> RunResult<()> {
    let blob = map.encode();
    engine.write(|txn| {
        let mut t = txn.open_table(entrymap_table())?;
        t.insert((book_id, entry_idx), blob.as_slice())?;
        Ok(())
    })
}

/// Reads one `ENTRYMAP` row.
pub fn get_entry_map(engine: &Engine, book_id: u16, entry_idx: u16) -> RunResult<Option<EntryMap>> {
    engine.read(|txn| match txn.open_table(entrymap_table()) {
        Ok(t) => match t.get((book_id, entry_idx))? {
            Some(v) => Ok(Some(EntryMap::decode(v.value())?)),
            None => Ok(None),
        },
        Err(_) => Ok(None),
    })
}

/// Drops every `ENTRYMAP` row of a book, so its snippets can no longer resolve.
///
/// Called from `Engine::remove_book`; without it a removed book would still be
/// reachable through a cached posting.
pub fn remove_entry_maps(engine: &Engine, book_id: u16) -> RunResult<()> {
    let rows: Vec<u16> = engine.read(|txn| {
        let t = match txn.open_table(entrymap_table()) {
            Ok(t) => t,
            Err(_) => return Ok(Vec::new()),
        };
        let mut out = Vec::new();
        for row in t.iter()? {
            let (k, _) = row?;
            if k.value().0 == book_id {
                out.push(k.value().1);
            }
        }
        Ok(out)
    })?;

    if rows.is_empty() {
        return Ok(());
    }
    engine.write(|txn| {
        let mut t = txn.open_table(entrymap_table())?;
        for eid in rows {
            t.remove((book_id, eid))?;
        }
        Ok(())
    })
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
            .read(|txn| {
                // A database that has never written postings has no table yet;
                // that is a count of zero, not an error.
                let t = match crate::storage::or_empty(txn.open_table(postings_table()))? {
                    Some(t) => t,
                    None => return Ok(0),
                };
                let mut n = 0u32;
                for row in t.iter()? {
                    let (k, _) = row?;
                    if k.value().0 == term {
                        n += 1;
                    }
                }
                Ok(n)
            })
            .unwrap()
    }

    #[test]
    fn a_writer_starts_with_no_chunks_on_disk() {
        let (_d, e) = engine();
        let w = Writer::new(&e).unwrap();
        assert!(w.pending.is_empty());
        assert!(w.next_chunk.is_empty());
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
        assert_eq!(w.pending.len(), 1);
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
            w.pending.get(b"x".as_slice()).map(Vec::len),
            Some(n),
            "a short term must not be flushed early"
        );
        assert_eq!(chunk_count(&e, b"x"), 0);

        w.end_document().unwrap();
        assert_eq!(
            w.pending.get(b"x".as_slice()).map(Vec::len),
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
        let mut engine;
        let uncompacted;

        {
            engine = Engine::open(&path).unwrap();
            let mut w = Writer::new(&engine).unwrap().with_max_pending(64);
            // One flush per document: the pattern that fragments the B-tree.
            for doc in 0..400u32 {
                w.add(b"turnover", doc, doc * 3, 0).unwrap();
                w.end_document().unwrap();
            }
            w.close(&mut dict).unwrap();
            uncompacted = engine.file_size();
        }
        let compacted = engine.compact().unwrap();
        let after = engine.file_size();

        assert!(compacted, "the session must have left pages to reclaim");
        assert!(
            after < uncompacted,
            "compaction must shrink the file: {uncompacted} -> {after}"
        );
        // The data itself is a few kilobytes.
        assert!(after < 64 * 1024, "compacted index is {after} bytes");
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
            assert_eq!(w.next_chunk.get(b"a".as_slice()).copied(), Some(2));
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
    fn an_interrupted_session_resumes_from_the_delta() {
        // A crash between writing chunks and merging them is the case the delta
        // exists for: the next session must not reuse chunk index 0.
        let (_d, e) = engine();
        let mut dict = dict::Dict::new();
        {
            let mut w = Writer::new(&e).unwrap();
            w.add(b"b", 1, 0, 0).unwrap();
            w.flush().unwrap();
            // No close(): the session ended without merging into the dictionary.
            dict::store_deltas(&e, &w.delta_rows()).unwrap();
        }
        let mut w = Writer::new(&e).unwrap();
        assert_eq!(w.next_chunk.get(b"b".as_slice()).copied(), Some(1));
        w.add(b"b", 2, 0, 0).unwrap();
        w.end_document().unwrap();
        w.close(&mut dict).unwrap();
        assert_eq!(chunk_count(&e, b"b"), 2);
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
        remove_entry_maps(&e, 3).unwrap();

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
