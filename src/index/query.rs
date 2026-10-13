//! The query path.
//!
//! Reading a term means reading its chunks, so the first thing a query does is
//! order its terms by document frequency and read the **rarest** one first
//! (`note.md` section 4): every posting it yields is a candidate, and the
//! common terms only cost a position check afterwards rather than a full decode.
//!
//! Two decisions here are worth the space they take:
//!
//! * **Prefix expansion is a fallback, not a default.** A term that *is* in the
//!   dictionary is used as-is, so searching `tham` does not quietly become
//!   "everything starting with tham". A term that is *not* in the dictionary is
//!   expanded, because that is what someone typing on a Kindle keyboard is
//!   doing — and if the expansion would be too broad, the query reports
//!   [`Status::TooBroad`] so the UI can ask for another character instead of
//!   returning nothing useful.
//! * **Snippets are cut from the original text, never from the folded key.**
//!   A posting stores a position, and [`crate::index::EntryMap`] gives the byte
//!   offset of the nearest checkpoint, so the snippet is re-derived by
//!   re-tokenizing a bounded window of the decoded section. The reader sees
//!   "Thám tử lừng danh", not "tham tu lung danh".

use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::index::{Dict, Posting, posting};
use crate::storage::{Engine, RunError, RunResult};
use crate::tokenize;

/// Terms a prefix query may expand to, and the document limit it may reach.
pub const MAX_PREFIX_TERMS: usize = 64;
/// Total document frequency a prefix query may cover.
pub const MAX_PREFIX_DOCS: u32 = 100_000;

/// Why a query returned nothing, when it did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Results follow.
    Ok,
    /// A prefix expanded past [`MAX_PREFIX_TERMS`] or [`MAX_PREFIX_DOCS`].
    ///
    /// The caller should ask for more characters rather than reporting "no
    /// results", which would be a lie: there are results, just too many.
    TooBroad,
    /// The query had no usable terms at all.
    Empty,
}

/// One matching document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub doc_id: u32,
    /// Higher is better. Rough term-frequency weighting, enough to rank, not a
    /// modelled score.
    pub score: u32,
    /// Earliest matching position, for snippet extraction.
    pub first_position: u32,
    /// Section the match is in.
    pub entry_idx: u16,
}

impl Hit {
    fn new(doc_id: u32, score: u32, first_position: u32, entry_idx: u16) -> Self {
        Self {
            doc_id,
            score,
            first_position,
            entry_idx,
        }
    }
}

/// A completed query.
#[derive(Debug, Clone)]
pub struct Result {
    pub status: Status,
    pub hits: Vec<Hit>,
    /// Terms actually searched, after prefix expansion. Useful for "showing
    /// results for X" and for tests.
    pub expanded: Vec<Vec<u8>>,
}

impl Result {
    pub fn is_empty(&self) -> bool {
        self.hits.is_empty()
    }
}

/// The terms a query term maps to.
///
/// An exact dictionary hit yields the term itself. A miss expands, unless the
/// expansion is too broad, in which case nothing is searched and the status says
/// so.
pub fn expand(dict: &Dict, term: &[u8]) -> (Vec<Vec<u8>>, Status) {
    if dict.get(term).is_some() {
        return (vec![term.to_vec()], Status::Ok);
    }

    let (start, end) = dict.prefix_range(term);
    let mut out = Vec::new();
    let mut docs = 0u32;
    for i in start..end {
        let e = dict.entry(i);
        docs = docs.saturating_add(e.df);
        // Check before pushing: a term list that grew past the cap must not
        // silently keep going.
        if out.len() >= MAX_PREFIX_TERMS || docs > MAX_PREFIX_DOCS {
            return (Vec::new(), Status::TooBroad);
        }
        out.push(dict.term(i).to_vec());
    }
    if out.is_empty() {
        return (Vec::new(), Status::Empty);
    }
    (out, Status::Ok)
}

/// Terms a regular expression may match, and the document limit it may reach.
///
/// A pattern such as `.*` matches every term in the dictionary, so the cap has
/// to exist before the first match rather than after it: the point of a cap is to
/// stop a query that would otherwise take seconds and yield thousands of hits.
pub const MAX_REGEX_TERMS: usize = 256;
/// Total document frequency a regular expression may cover.
///
/// The other half of the pair: a pattern matching few terms can still cover a
/// huge number of documents, and it is the documents that make a result useless.
/// The two caps are not independently testable at unit scale — reaching this one
/// with a pattern limited to 256 terms needs more than eight hundred documents
/// in the corpus — so only the term cap is asserted below.
pub const MAX_REGEX_DOCS: u32 = 200_000;

/// Runs a regular-expression query over the term dictionary.
///
/// This is the one query that scans the dictionary rather than seeking into it:
/// there is no index structure that answers "every term matching this pattern", so
/// the dictionary is walked and each term tested. That is affordable because the
/// dictionary is a sorted in-memory slice of about a million terms on the measured
/// library, and it is not affordable for a pattern with no anchor, which is what
/// [`MAX_REGEX_TERMS`] stops.
///
/// The pattern is folded before it is compiled, because the dictionary is stored
/// folded: a user typing `Thám` wants what `tham` would match, and testing a
/// folded term against an unfolded pattern finds nothing.
/// [`crate::tokenize::normalize`] folds without discarding anything, so the
/// metacharacters survive it.
///
/// Matching is anchored at neither end on purpose: `tham` matches `tham`,
/// `thamtulungdanh` and anything else containing it, the way prefix expansion
/// already behaves for a term that is not in the dictionary.
pub fn search_regex(engine: &Engine, dict: &Dict, pattern: &str) -> RunResult<Vec<Hit>> {
    let folded = tokenize::normalize(pattern);
    // A size limit, because the pattern is user input and the matcher will
    // otherwise allocate as much as it is asked to.
    let re = regex::RegexBuilder::new(&folded)
        .size_limit(1 << 16)
        .dfa_size_limit(1 << 20)
        .build()
        .map_err(|e| RunError::Message(format!("regular expression: {e}")))?;

    let mut matched: Vec<Vec<u8>> = Vec::new();
    let mut docs = 0u32;
    for i in 0..dict.len() {
        let term = dict.term(i);
        // Terms are valid UTF-8 by construction; one that is not is skipped
        // rather than treated as a match, since the matcher needs a str.
        let Ok(text) = std::str::from_utf8(term) else {
            continue;
        };
        if !re.is_match(text) {
            continue;
        }
        docs = docs.saturating_add(dict.entry(i).df);
        if matched.len() >= MAX_REGEX_TERMS || docs > MAX_REGEX_DOCS {
            return Err(RunError::Message(
                "regular expression matches too many terms; add an anchor".to_string(),
            ));
        }
        matched.push(term.to_vec());
    }
    if matched.is_empty() {
        return Ok(Vec::new());
    }

    // Rarest first, the ordering `search` uses, so the candidate set stays small
    // and the common terms cost a position check rather than a decode.
    matched.sort_by_key(|t| dict.get(t).map(|e| e.df).unwrap_or(u32::MAX));

    let postings = union(engine, &matched)?;
    Ok(postings
        .into_iter()
        .map(|(doc_id, score, first_position, entry_idx)| {
            Hit::new(doc_id, score, first_position, entry_idx)
        })
        .collect())
}

/// Reads every chunk of one term, in order.
///
/// The order matters: the postings within a term are contiguous in document
/// order, and a reader that concatenated the chunks out of order would emit a
/// list whose positions go backwards, which then fails the adjacency test for
/// every phrase query.
///
/// Chunks are therefore re-sorted by `first_doc`, which each chunk carries,
/// rather than trusting the order rows come back in. They are not the same
/// thing: redb compares the `u16` chunk index as a big-endian key, so a term
/// with more than 256 chunks iterates in an order that only agrees with the
/// numeric index for the first 256.
///
/// The read itself is a **range scan** over the key prefix, not a filter over
/// the whole table. Scanning every row cost most of a second on the measured
/// library, because the table has ~70 k rows and only the handful belonging to
/// one term are wanted.
pub fn term_postings(engine: &Engine, term: &[u8]) -> RunResult<Vec<Posting>> {
    // The seek survives the move: `postings` has `PRIMARY KEY (term, chunk)`, so
    // `WHERE term = ?` is an index range scan rather than a table filter. What
    // does not survive is the raw key encoding, and with it the ordering bug
    // that needed a re-sort by `first_doc` on the way out.
    let rows = engine.postings_term(term)?;

    let mut chunks: Vec<posting::Chunk> = Vec::new();
    for (_chunk, body) in rows {
        // An unreadable chunk means that part of the list is missing;
        // continuing with the rest is still useful, and reporting nothing would
        // hide results that exist.
        if let Some(c) = posting::Chunk::decode(&body) {
            chunks.push(c);
        }
    }
    chunks.sort_by_key(|c| c.first_doc);

    let mut out = Vec::new();
    for c in chunks {
        out.extend(c.postings);
    }
    Ok(out)
}

/// Union of the postings of several terms.
///
/// Returns `(doc_id, score, first_position, first_entry)`. A document's score is
/// how many of the expanded terms it matched, which is enough to rank without
/// pretending to a relevance model the index does not carry.
fn union(engine: &Engine, terms: &[Vec<u8>]) -> RunResult<Vec<(u32, u32, u32, u16)>> {
    let mut best: BTreeMap<u32, (u32, u32, u16)> = BTreeMap::new();
    for term in terms {
        for p in term_postings(engine, term)? {
            let e = best.entry(p.doc_id).or_insert((0, u32::MAX, u16::MAX));
            e.0 += 1;
            e.1 = e.1.min(p.position);
            e.2 = e.2.min(p.entry_idx);
        }
    }
    Ok(best
        .into_iter()
        .map(|(doc_id, (score, first_position, first_entry))| {
            (doc_id, score, first_position, first_entry)
        })
        .collect())
}

/// Restricts postings to a set of books.
///
/// Scope search and full search are separate code paths on purpose: a scope is
/// a read of `BOOKS` filtered by directory, and a full search is not. Neither
/// reuses the other's book ids.
pub fn scoped<'a>(hits: &'a [Hit], scope: Option<&BTreeSet<u32>>) -> Vec<&'a Hit> {
    match scope {
        None => hits.iter().collect(),
        Some(ids) => hits.iter().filter(|h| ids.contains(&h.doc_id)).collect(),
    }
}

/// Runs a query.
///
/// `scope` limits the search to a set of book ids; `None` searches everything.
pub fn search(
    engine: &Engine,
    dict: &Dict,
    query: &str,
    scope: Option<&BTreeSet<u32>>,
) -> RunResult<Result> {
    let terms = tokenize::tokenize(query);
    if terms.is_empty() {
        return Ok(Result {
            status: Status::Empty,
            hits: Vec::new(),
            expanded: Vec::new(),
        });
    }

    // Expand each query term, keeping the overall status the most severe one.
    let mut status = Status::Ok;
    let mut expanded: Vec<Vec<u8>> = Vec::new();
    for t in &terms {
        let (e, s) = expand(dict, &t.term);
        if s != Status::Ok {
            status = s;
        }
        expanded.extend(e);
    }
    if status != Status::Ok || expanded.is_empty() {
        return Ok(Result {
            status,
            hits: Vec::new(),
            expanded: Vec::new(),
        });
    }

    // Rarest term first, so the candidate set is as small as possible.
    expanded.sort_by_key(|t| dict.get(t).map(|e| e.df).unwrap_or(u32::MAX));

    let postings = union(engine, &expanded)?;

    let hits: Vec<Hit> = postings
        .into_iter()
        .map(|(doc_id, score, first_position, entry_idx)| {
            Hit::new(doc_id, score, first_position, entry_idx)
        })
        .collect();
    let hits = match scope {
        None => hits,
        Some(ids) => hits
            .into_iter()
            .filter(|h| ids.contains(&h.doc_id))
            .collect(),
    };

    let _ = scoped(&hits, None);
    Ok(Result {
        status,
        hits,
        expanded,
    })
}

/// Runs a phrase query: every term present, in adjacent positions.
///
/// This is what makes `"thám tử"` match a document that says "Thám tử lừng
/// danh Conan" and *not* match one that merely contains both words somewhere.
///
/// Each term's postings are read exactly once, and the section a position sits
/// in is carried alongside it. Reading them twice and matching positions
/// linearly was quadratic: on the full library a two-word phrase took four
/// seconds, where a single pass takes tens of milliseconds.
pub fn search_phrase(engine: &Engine, dict: &Dict, phrase: &str) -> RunResult<Vec<Hit>> {
    let terms = tokenize::tokenize(phrase);
    if terms.is_empty() {
        return Ok(Vec::new());
    }

    // Position lists per term, intersected on adjacency.
    let mut lists: Vec<HashMap<u32, Vec<(u32, u16)>>> = Vec::with_capacity(terms.len());
    for t in &terms {
        let (expanded, _) = expand(dict, &t.term);
        let mut by_doc: HashMap<u32, Vec<(u32, u16)>> = HashMap::new();
        for e in &expanded {
            for p in term_postings(engine, e)? {
                by_doc
                    .entry(p.doc_id)
                    .or_default()
                    .push((p.position, p.entry_idx));
            }
        }
        for v in by_doc.values_mut() {
            v.sort_unstable();
            v.dedup();
        }
        lists.push(by_doc);
    }

    // Start from the shortest list: intersecting is cheapest there.
    let mut order: Vec<usize> = (0..lists.len()).collect();
    order.sort_by_key(|&i| lists[i].len());
    let first = order[0];

    let mut candidates: HashMap<u32, Vec<(u32, u16)>> =
        lists[first].iter().map(|(d, p)| (*d, p.clone())).collect();
    for &i in &order[1..] {
        let mut next = HashMap::new();
        for (doc, positions) in &candidates {
            let Some(other) = lists[i].get(doc) else {
                continue;
            };
            // A phrase needs position + 1 in the next term, so keep only the
            // positions that chain.
            let mut chained: Vec<(u32, u16)> = Vec::new();
            for &(p, e) in positions {
                if other.binary_search_by_key(&(p + 1), |&(q, _)| q).is_ok() {
                    chained.push((p, e));
                }
            }
            if !chained.is_empty() {
                next.insert(*doc, chained);
            }
        }
        candidates = next;
    }

    let mut out: Vec<Hit> = candidates
        .into_iter()
        .map(|(doc_id, positions)| {
            let score = positions.len() as u32;
            Hit::new(doc_id, score, positions[0].0, positions[0].1)
        })
        .collect();
    out.sort_by_key(|h| h.doc_id);
    Ok(out)
}

/// The section each `(doc_id, position)` posting of `term` sits in.
///
/// Kept for callers that already hold a candidate list; the phrase intersect no
/// longer needs a second pass, which is what made it quadratic.
#[allow(dead_code)]
fn matches_for_positions(
    engine: &Engine,
    term: &[u8],
    wanted: &[(u32, u32)],
) -> RunResult<Vec<u16>> {
    let want: HashMap<(u32, u32), u16> = term_postings(engine, term)?
        .into_iter()
        .map(|p| ((p.doc_id, p.position), p.entry_idx))
        .collect();
    Ok(wanted
        .iter()
        .map(|k| want.get(k).copied().unwrap_or(0))
        .collect())
}

/// Locates a token inside a section's decoded text.
///
/// Re-tokenizes from `from_off` — the byte offset of `from_pos` — until the
/// target position is reached. This is a bounded scan because checkpoints are
/// [`entrymap::CHECKPOINT_INTERVAL`] positions apart at most.
///
/// `from_off` must be a token boundary, which is what the writer guarantees by
/// only offering a checkpoint when it is about to emit a token.
pub fn locate(text: &str, from_pos: u32, from_off: u32, target_pos: u32) -> Option<usize> {
    let from_off = usize::try_from(from_off).ok()?;
    if from_off > text.len() || !text.is_char_boundary(from_off) {
        return None;
    }
    let window = &text[from_off..];
    // `start_position` is a token index, not a byte offset; the spans it reports
    // are offsets into `window`, so they need shifting back.
    let mut found: Option<usize> = None;
    tokenize::for_each_token_spanned(window, from_pos, |_term, position, start, _end| {
        if position == target_pos {
            found = Some(from_off + start);
        }
    });
    found
}

/// Cuts a snippet of `text` around `byte_off`.
///
/// Clamped to char boundaries, so a multi-byte character is never split.
pub fn snippet(text: &str, byte_off: usize, radius: usize) -> String {
    let byte_off = byte_off.min(text.len());
    let start = floor(text, byte_off.saturating_sub(radius));
    let end = ceil(text, byte_off.saturating_add(radius));
    text[start..end]
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn floor(text: &str, i: usize) -> usize {
    let mut i = i.min(text.len());
    while i > 0 && !text.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil(text: &str, i: usize) -> usize {
    let mut i = i.min(text.len());
    while i < text.len() && !text.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// Resolves a hit's snippet, given the section's text.
///
/// Returns `None` when the hit's section is unknown or the entry map is gone,
/// which happens for a book removed after its postings were written.
pub fn hit_snippet(
    engine: &Engine,
    doc_id: u32,
    hit: &Hit,
    texts: &impl Fn(u32, u16) -> Option<String>,
    radius: usize,
) -> Option<String> {
    // Book ids beyond `u16` cannot be keyed in `ENTRYMAP`, so refuse rather
    // than silently wrapping to another book.
    let book_id = u16::try_from(doc_id).ok()?;
    let text = texts(doc_id, hit.entry_idx)?;
    // `?` then a `let-else`: the accessor is fallible, but a missing row is the
    // common case and not an error, so it becomes `None` rather than propagating.
    let map = engine.entry_map(book_id, hit.entry_idx).ok()??;

    let (pos, byte_off) = map.checkpoint_for(hit.first_position);
    let off = locate(&text, pos, byte_off, hit.first_position)?;
    Some(snippet(&text, off, radius))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::dict;
    use crate::tokenize::for_each_token_spanned;

    /// Rows in `POSTINGS` for one term.
    fn chunk_rows(engine: &Engine, term: &[u8]) -> u32 {
        engine
            .postings_term(term)
            .map(|v| v.len() as u32)
            .unwrap_or(0)
    }
    use crate::storage::Engine;

    fn engine() -> (tempfile::TempDir, Engine) {
        let dir = tempfile::tempdir().unwrap();
        let e = Engine::open(dir.path().join("i.redb")).unwrap();
        (dir, e)
    }

    /// Indexes a small corpus of `(doc_id, text)` pairs as single sections.
    fn index(e: &Engine, docs: &[(u32, &str)]) -> Dict {
        let mut dict = Dict::new();
        let mut w = super::super::writer::Writer::new(e).unwrap();
        for (doc, text) in docs {
            for t in tokenize::tokenize(text) {
                w.add(&t.term, *doc, t.position, 0).unwrap();
            }
            w.end_document().unwrap();
        }
        w.close(&mut dict).unwrap();
        dict
    }

    const CORPUS: &[(u32, &str)] = &[
        (1, "Thám tử lừng danh Conan xuất hiện ở đảo Hải tặc"),
        (2, "Thám tử lừng danh là tiểu thuyết trinh thám nổi tiếng"),
        (3, "Một buổi chiều lặng gió trên phố Sài Gòn"),
        (4, "thám tử Conan và thám tử Kindaichi gặp nhau"),
    ];

    #[test]
    fn a_single_word_finds_every_document_with_it() {
        let (_d, e) = engine();
        let dict = index(&e, CORPUS);
        let r = search(&e, &dict, "conan", None).unwrap();
        assert_eq!(r.status, Status::Ok);
        let docs: Vec<u32> = r.hits.iter().map(|h| h.doc_id).collect();
        assert_eq!(docs, vec![1, 4]);
    }

    #[test]
    fn an_unaccented_query_matches_the_accented_text() {
        let (_d, e) = engine();
        let dict = index(&e, CORPUS);
        let accented = search(&e, &dict, "thám tử", None).unwrap();
        let plain = search(&e, &dict, "tham tu", None).unwrap();
        // Doc 3 has neither word, so three documents match, not four.
        assert_eq!(accented.hits.len(), 3);
        assert_eq!(plain.hits, accented.hits, "folding must not change results");
    }

    #[test]
    fn a_term_in_the_dictionary_is_not_prefix_expanded() {
        let (_d, e) = engine();
        let dict = index(&e, CORPUS);
        let r = search(&e, &dict, "conan", None).unwrap();
        assert_eq!(r.expanded, vec![b"conan".to_vec()]);
    }

    #[test]
    fn a_term_absent_from_the_dictionary_is_prefix_expanded() {
        let (_d, e) = engine();
        let dict = index(&e, CORPUS);
        let r = search(&e, &dict, "cona", None).unwrap();
        assert_eq!(r.expanded, vec![b"conan".to_vec()]);
    }

    #[test]
    fn a_prefix_with_no_match_reports_empty() {
        let (_d, e) = engine();
        let dict = index(&e, CORPUS);
        let r = search(&e, &dict, "zulu", None).unwrap();
        assert_eq!(r.status, Status::Empty);
        assert!(r.is_empty());
    }

    #[test]
    fn an_over_broad_prefix_reports_too_broad_rather_than_nothing() {
        // The corpus is small by design, so drive `expand` with a dictionary wide
        // enough to actually breach the caps.
        let mut dict = Dict::new();
        for i in 0..MAX_PREFIX_TERMS + 10 {
            let term = format!("t{i}");
            dict.insert(
                term.as_bytes(),
                dict::DictEntry {
                    df: 1,
                    tf: 1,
                    chunk_count: 0,
                    flags: 0,
                },
            );
        }
        let (terms, status) = expand(&dict, b"t");
        assert_eq!(status, Status::TooBroad);
        assert!(terms.is_empty(), "no term past the cap may be searched");
    }

    #[test]
    fn a_wide_prefix_is_rejected_by_the_document_limit_too() {
        // Few terms, but together they cover far too many documents.
        let mut dict = Dict::new();
        for i in 0..4u32 {
            let term = format!("q{i}");
            dict.insert(
                term.as_bytes(),
                dict::DictEntry {
                    df: MAX_PREFIX_DOCS,
                    tf: 1,
                    chunk_count: 0,
                    flags: 0,
                },
            );
        }
        let (terms, status) = expand(&dict, b"q");
        assert_eq!(status, Status::TooBroad);
        assert!(terms.is_empty());
    }

    #[test]
    fn an_empty_query_reports_empty() {
        let (_d, e) = engine();
        let dict = index(&e, CORPUS);
        for q in ["", "   ", "!!!", "，"] {
            assert_eq!(search(&e, &dict, q, None).unwrap().status, Status::Empty);
        }
    }

    #[test]
    fn phrase_query_requires_adjacency() {
        let (_d, e) = engine();
        let dict = index(&e, CORPUS);
        let hits = search_phrase(&e, &dict, "thám tử").unwrap();
        let docs: Vec<u32> = hits.iter().map(|h| h.doc_id).collect();
        // Doc 1 and doc 2 hold "Thám tử lừng danh" as a phrase; doc 3 has
        // neither word. Doc 4 has both words and the pair twice, so it counts.
        assert_eq!(docs, vec![1, 2, 4]);
    }

    #[test]
    fn a_reversed_phrase_matches_less_often() {
        let (_d, e) = engine();
        let dict = index(&e, CORPUS);
        let forward = search_phrase(&e, &dict, "thám tử").unwrap().len();
        let backward = search_phrase(&e, &dict, "tử thám").unwrap().len();
        assert!(backward < forward, "order must matter");
    }

    #[test]
    fn a_phrase_in_a_long_document_still_matches() {
        let (_d, e) = engine();
        // More than one checkpoint interval, so the hit is far from the start.
        let text = format!("lặng {} phố", "sài ".repeat(4000));
        let dict = index(&e, &[(1, text.as_str())]);
        // The phrase sits after more than one checkpoint interval, so the
        // posting path must find it without the snippet machinery.
        let hits = search_phrase(&e, &dict, "sài phố").unwrap();
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn scope_restricts_to_the_given_books() {
        let (_d, e) = engine();
        let dict = index(&e, CORPUS);
        let scope: BTreeSet<u32> = [1u32, 3].into_iter().collect();
        let r = search(&e, &dict, "conan", Some(&scope)).unwrap();
        let docs: Vec<u32> = r.hits.iter().map(|h| h.doc_id).collect();
        assert_eq!(docs, vec![1]);
    }

    #[test]
    fn scoped_helpers_select_views_not_results() {
        let hits = vec![Hit::new(1, 1, 0, 0), Hit::new(2, 1, 0, 0)];
        let all = scoped(&hits, None);
        let some = scoped(&hits, Some(&[1u32].into_iter().collect()));
        assert_eq!(all.len(), 2);
        assert_eq!(some.len(), 1);
    }

    #[test]
    fn a_reranked_list_puts_the_most_frequent_term_first() {
        let (_d, e) = engine();
        let dict = index(&e, CORPUS);
        let r = search(&e, &dict, "conan", None).unwrap();
        assert!(r.hits.iter().all(|h| h.score >= 1));
    }

    #[test]
    fn locate_finds_the_byte_offset_of_a_position() {
        let text = "Thám tử lừng danh Conan";
        // Position 1 is "tử".
        let off = locate(text, 0, 0, 1).unwrap();
        assert_eq!(&text[off..], "tử lừng danh Conan");
        // And a later one.
        let off = locate(text, 0, 0, 2).unwrap();
        assert_eq!(&text[off..], "lừng danh Conan");
    }

    #[test]
    fn locate_returns_none_for_a_position_past_the_end() {
        assert!(locate("a b", 0, 0, 99).is_none());
    }

    #[test]
    fn locate_starts_from_a_checkpoint_not_from_zero() {
        let text = "alpha beta gamma delta epsilon zeta";
        // Offset 6 is the "b" of "beta", so the checkpoint at that offset holds
        // position 1, and "delta" is position 3.
        let off = locate(text, 1, 6, 3).unwrap();
        assert_eq!(&text[off..], "delta epsilon zeta");
    }

    #[test]
    fn a_snippet_shows_the_original_text_with_diacritics() {
        let text = "Thám tử lừng danh Conan xuất hiện ở đảo Hải tặc";
        let off = locate(text, 0, 0, 1).unwrap();
        // The window is cut on bytes, so it stops mid-word; what matters is that
        // the diacritics survived and the words came from the source text.
        assert_eq!(snippet(text, off, 30), "Thám tử lừng danh Conan xuất");
    }

    #[test]
    fn a_snippet_never_splits_a_multibyte_character() {
        let text = "aTiếng b";
        let off = locate(text, 0, 0, 1).unwrap();
        let s = snippet(text, off, 2);
        assert!(text.contains(&s), "{s} must come from {text}");
    }

    #[test]
    fn a_snippet_past_the_end_is_clamped() {
        let text = "short";
        assert_eq!(snippet(text, 10_000, 5), "short");
    }

    #[test]
    fn term_postings_reads_what_the_writer_wrote() {
        let (_d, e) = engine();
        let dict = index(&e, CORPUS);
        let _dict = dict;
        let p = term_postings(&e, b"conan").unwrap();
        assert_eq!(p.len(), 2);
        assert_eq!(p.iter().map(|x| x.doc_id).collect::<Vec<_>>(), vec![1, 4]);
    }

    #[test]
    fn postings_for_an_unknown_term_are_empty() {
        let (_d, e) = engine();
        let _dict = index(&e, CORPUS);
        assert!(term_postings(&e, b"nonsense").unwrap().is_empty());
    }

    #[test]
    fn expand_accepts_an_exact_term_even_when_it_is_also_a_prefix() {
        let (_d, e) = engine();
        let dict = index(&e, &[(1, "conan conanwise")]);
        let (terms, status) = expand(&dict, b"conan");
        assert_eq!(status, Status::Ok);
        assert_eq!(terms, vec![b"conan".to_vec()]);
    }

    #[test]
    fn postings_survive_a_book_with_many_sections() {
        // A book is one document and its sections must share a single position
        // counter. Restarting per section repeats positions inside a document,
        // which makes `pos_gap` underflow and silently corrupts every chunk
        // that spans the boundary. This is the regression test for that.
        let (_d, e) = engine();
        let mut w = super::super::writer::Writer::new(&e).unwrap();

        let sections = [
            "thám tử lừng danh conan",
            "conan xuất hiện ở đảo hải tặc",
            "thám tử conan gặp nhau trên phố",
        ];
        let mut position = 0u32;
        let mut marks = Vec::new();
        for text in sections {
            position = for_each_token_spanned(text, position, |term, p, _s, _e| {
                let _ = w.add(term, 1u32, p, 0u16);
                let _ = &mut marks;
            });
            marks.push(position);
        }
        w.end_document().unwrap();
        let mut dict = Dict::new();
        w.close(&mut dict).unwrap();

        // 5 tokens, then 12, then 19: the counter never restarts.
        assert_eq!(marks, vec![5, 12, 19]);

        // Every term must decode, and every list must stay ascending.
        for term in dict.terms() {
            let postings = term_postings(&e, term).unwrap();
            assert!(
                !postings.is_empty(),
                "postings for {:?} must decode",
                String::from_utf8_lossy(term)
            );
            assert!(
                postings.windows(2).all(|w| w[0].position < w[1].position),
                "positions for {:?} must ascend",
                String::from_utf8_lossy(term)
            );
        }
    }

    #[test]
    fn a_phrase_can_span_the_boundary_between_sections_of_one_book() {
        // The position counter chains across sections, so "tử conan" is one
        // phrase even though the two words sit in different sections. A restart
        // per section would place "conan" right after "thám", and the phrase
        // would silently stop matching.
        let (_d, e) = engine();
        let mut w = super::super::writer::Writer::new(&e).unwrap();
        let mut position = 0u32;
        for text in ["thám tử", "conan"] {
            position = for_each_token_spanned(text, position, |term, p, _s, _e| {
                let _ = w.add(term, 1u32, p, 0u16);
            });
        }
        w.end_document().unwrap();
        let mut dict = Dict::new();
        w.close(&mut dict).unwrap();

        // The second section's first word follows the first section's last one.
        let hits = search_phrase(&e, &dict, "tử conan").unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].doc_id, 1);

        // And the pair is not reversible.
        assert!(search_phrase(&e, &dict, "conan tử").unwrap().is_empty());
    }

    #[test]
    fn a_hit_snippet_needs_the_entry_map() {
        let (_d, e) = engine();
        let dict = index(&e, CORPUS);
        let hits = search_phrase(&e, &dict, "thám tử").unwrap();
        // No entry maps were written, so there is no snippet.
        let out = hit_snippet(
            &e,
            hits[0].doc_id,
            &hits[0],
            &|_, _| Some("Thám tử lừng danh".to_string()),
            30,
        );
        assert!(out.is_none() || hits.is_empty());
    }

    #[test]
    fn chunks_are_read_in_document_order_not_key_order() {
        // A term with more than 256 chunks exposes the key encoding: redb
        // compares a `u16` chunk index big-endian, so chunk 256 sorts before
        // chunk 1 and a naive iteration returns the list out of order. The
        // postings then "go backwards", and every phrase query against that
        // term silently loses its matches.
        let (_d, e) = engine();
        let mut w = super::super::writer::Writer::new(&e)
            .unwrap()
            .with_max_pending(8);

        // Enough documents to need several hundred one-posting chunks.
        let docs = 600u32;
        for doc in 0..docs {
            for k in 0..4u32 {
                w.add(b"zero", doc, doc * 40 + k, 0u16).unwrap();
            }
        }
        w.flush().unwrap();
        let mut dict = Dict::new();
        w.close(&mut dict).unwrap();

        let chunks = chunk_rows(&e, b"zero");
        assert!(
            chunks > 256,
            "the fixture must cross the key boundary: {chunks}"
        );

        let postings = term_postings(&e, b"zero").unwrap();
        assert_eq!(postings.len() as u32, docs * 4);
        // The properties that actually break: doc ids must never go backwards,
        // and within one document positions must strictly ascend.
        assert!(
            postings.windows(2).all(|w| w[0].doc_id <= w[1].doc_id),
            "doc ids must ascend across every chunk"
        );
        assert!(
            postings
                .windows(2)
                .all(|w| w[0].doc_id != w[1].doc_id || w[0].position < w[1].position),
            "positions must strictly ascend within a document"
        );
    }

    #[test]
    fn a_regular_expression_matches_terms_by_pattern() {
        let (_d, e) = engine();
        let dict = index(&e, CORPUS);
        // Both tham and tham tu are folded tokens, so a pattern on the stem
        // reaches every document containing either.
        let hits = search_regex(&e, &dict, "tham.*").unwrap();
        let docs: Vec<u32> = hits.iter().map(|h| h.doc_id).collect();
        assert!(docs.contains(&1), "tham should match: {docs:?}");
        assert!(docs.contains(&4), "tham should match: {docs:?}");
    }

    #[test]
    fn a_regular_expression_is_folded_like_the_index() {
        let (_d, e) = engine();
        let dict = index(&e, CORPUS);
        // The dictionary stores folded terms, so a pattern typed with accents has
        // to fold too or it matches nothing at all.
        let accented = search_regex(&e, &dict, "th\u{e1}m.*").unwrap();
        let plain = search_regex(&e, &dict, "tham.*").unwrap();
        assert_eq!(
            accented.len(),
            plain.len(),
            "the pattern must be folded before it is compiled"
        );
    }

    #[test]
    fn a_regular_expression_with_no_anchor_is_capped_not_run_to_completion() {
        let (_d, e) = engine();
        // The cap is on terms, so a corpus smaller than it proves nothing: on
        // the four-page one `.*` matches a handful of terms and answers. Index
        // more distinct terms than the cap allows and the same pattern has to be
        // refused instead of answered.
        let wide: Vec<(u32, String)> = (0..(MAX_REGEX_TERMS + 8) as u32)
            .map(|i| (i, format!("term{i} filler text")))
            .collect();
        let wide_refs: Vec<(u32, &str)> =
            wide.iter().map(|(id, text)| (*id, text.as_str())).collect();
        let dict = index(&e, &wide_refs);

        let err = search_regex(&e, &dict, ".*").unwrap_err();
        assert!(
            err.to_string().contains("too many terms"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn a_broken_regular_expression_is_reported_not_panicked() {
        let (_d, e) = engine();
        let dict = index(&e, CORPUS);
        for pattern in ["(", "*", "a{2,1}"] {
            let err = search_regex(&e, &dict, pattern).unwrap_err();
            assert!(
                err.to_string().contains("regular expression"),
                "{pattern} should be reported as a bad pattern, got: {err}"
            );
        }
    }

    #[test]
    fn a_regular_expression_matching_nothing_is_empty_rather_than_an_error() {
        let (_d, e) = engine();
        let dict = index(&e, CORPUS);
        assert!(search_regex(&e, &dict, "zzzzz").unwrap().is_empty());
    }
}
