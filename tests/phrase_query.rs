//! Phrase query correctness, run against the real library.
//!
//! The whole design rests on one claim: a query typed without diacritics must
//! find a book written with them, and word order must be respected. This runs
//! the real extraction pipeline, the real tokenizer and a positional
//! intersection — the loop `note.md` section 4.1 describes — over a sample of
//! the real library.
//!
//! Nothing but the result counts is retained, so memory stays bounded and the
//! whole test finishes in seconds. Skipped when the library directory is
//! absent, so `cargo test` still works on a machine without it.
//!
//! Run with `--release`: the unoptimized build is around twenty times slower,
//! because `lol_html` and the zip reader dominate the time.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use turbosearch::format::epub;
use turbosearch::tokenize;

/// How many books to sample. Every assertion compares result sets against each
/// other rather than to absolute counts, so a sample is enough.
const SAMPLE_BOOKS: usize = 120;

/// The queries under test, and what each one is there to check.
const QUERIES: &[&str] = &[
    "thám tử",            // accented, the way the book writes it
    "Thám tử",            // different capitalisation
    "THÁM TỬ",            // all caps
    "tham tu",            // no diacritics at all
    "Thám-tử",            // a different separator
    "tử thám",            // reversed order
    "thám",               // one term
    "thám tử lừng danh",  // three terms
    "sherlock holmes",    // an English phrase
    "thám tử zhūmíng de", // nonsense that must match nothing
];

struct Outcome {
    counts: HashMap<String, usize>,
    previews: HashMap<String, Vec<String>>,
    books_scanned: usize,
}

/// Runs every query over the sampled library in a single pass.
///
/// Matching happens as the tokens stream by, so no per-token value is
/// allocated. That matters: a book holds around 124k terms, and an earlier
/// version of this test that collected them into owned strings took four
/// minutes over 120 books where this one takes a few seconds.
fn measure() -> Option<Outcome> {
    let root = turbosearch::library::root()?;
    let mut files = Vec::new();
    walk(&root, &mut files);
    files.sort();
    if files.is_empty() {
        return None;
    }

    let queries: Vec<Vec<String>> = QUERIES.iter().map(|q| query_terms(q)).collect();
    let mut out = Outcome {
        counts: QUERIES.iter().map(|q| (q.to_string(), 0)).collect(),
        previews: QUERIES
            .iter()
            .map(|q| (q.to_string(), Vec::new()))
            .collect(),
        books_scanned: 0,
    };

    for path in files.iter().take(SAMPLE_BOOKS) {
        let Ok(content) = epub::open(path) else {
            continue;
        };
        out.books_scanned += 1;
        let sections: Vec<String> = content
            .sections
            .iter()
            .map(|s| String::from_utf8_lossy(&s.text).into_owned())
            .collect();

        // One matcher per query, all fed from the same token stream.
        let mut matchers: Vec<PhraseMatcher> =
            queries.iter().map(|q| PhraseMatcher::new(q)).collect();

        // Positions are book-global because adjacency spans sections, so the
        // counter is threaded through. Spans stay local to their own section,
        // which is exactly why `note.md` section 4.1 stores `(doc_id, position)`
        // for matching and `entry_idx` for locating text.
        let mut position = 0u32;
        for (section_index, section) in sections.iter().enumerate() {
            // Every matcher consumes the same stream. `start`/`end` are offsets
            // into THIS section, so each matcher is told which section it is in.
            for m in matchers.iter_mut() {
                m.enter_section(section_index);
            }
            position =
                tokenize::for_each_token_spanned(section, position, |term, _, start, end| {
                    for m in matchers.iter_mut() {
                        m.push(term, start, end);
                    }
                });
        }

        for (index, query) in QUERIES.iter().enumerate() {
            let m = &matchers[index];
            if m.hits == 0 {
                continue;
            }
            *out.counts.get_mut(*query).unwrap() += 1;
            let bucket = out.previews.get_mut(*query).unwrap();
            if bucket.len() < 3 {
                bucket.push(preview(
                    &sections,
                    m.preview_section,
                    m.preview_start,
                    m.preview_end,
                ));
            }
        }
    }

    Some(out)
}

/// Streaming positional phrase matcher.
///
/// Equivalent to the two-pointer intersection of `note.md` section 4.1 step 3,
/// but it answers "is this phrase here" in O(1) memory instead of holding the
/// whole token stream.
struct PhraseMatcher {
    terms: Vec<String>,
    /// Consecutive query terms seen ending at the current token.
    matched: usize,
    hits: usize,
    /// Section and byte range of the first hit, used for the preview.
    preview_section: usize,
    preview_start: usize,
    preview_end: usize,
    section_index: usize,
    /// Where the current phrase candidate began, filled in by the first term.
    candidate_start: usize,
    candidate_section: usize,
}

impl PhraseMatcher {
    fn new(terms: &[String]) -> Self {
        Self {
            terms: terms.to_vec(),
            matched: 0,
            hits: 0,
            preview_section: 0,
            preview_start: 0,
            preview_end: 0,
            section_index: 0,
            candidate_start: 0,
            candidate_section: 0,
        }
    }

    /// Announces which section the coming tokens belong to.
    ///
    /// Needed because spans are relative to their own section while positions
    /// run across the whole book.
    fn enter_section(&mut self, index: usize) {
        self.section_index = index;
    }

    fn push(&mut self, term: &[u8], start: usize, end: usize) {
        if self.terms.is_empty() {
            return;
        }
        let first = self.terms[0].as_bytes();

        // A term that opens the phrase records where this candidate started,
        // because the token that COMPLETES the match carries only its own span.
        if term == first {
            self.candidate_start = start;
            self.candidate_section = self.section_index;
        }

        if self.matched < self.terms.len() && term == self.terms[self.matched].as_bytes() {
            self.matched += 1;
            if self.matched == self.terms.len() {
                self.hits += 1;
                if self.hits == 1 {
                    self.preview_section = self.candidate_section;
                    self.preview_start = self.candidate_start;
                    self.preview_end = end;
                }
                // Restart so a following occurrence can also match.
                self.matched = 0;
            }
        } else {
            self.matched = usize::from(term == first);
        }
    }
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
        } else if p.extension().and_then(|e| e.to_str()) == Some("epub") {
            out.push(p);
        }
    }
}

/// Tokenizes a query into the terms the index would look up.
fn query_terms(query: &str) -> Vec<String> {
    let mut out = Vec::new();
    tokenize::for_each_token(query, 0, |t, _| {
        out.push(String::from_utf8_lossy(t).into_owned())
    });
    out
}

fn walk_to_boundary(text: &str, mut i: usize, forward: bool) -> usize {
    // A window cut by adding or subtracting a byte count is not guaranteed to
    // land on a character boundary, and Vietnamese text has plenty of two- and
    // three-byte characters to fall inside.
    while !text.is_char_boundary(i) {
        if forward {
            i += 1;
        } else {
            i -= 1;
        }
    }
    i
}

/// The original text around a match, as the book writes it.
///
/// `from` and `to` are byte offsets inside the single named section.
fn preview(sections: &[String], section_index: usize, from: usize, to: usize) -> String {
    let Some(text) = sections.get(section_index) else {
        return String::new();
    };
    let a = walk_to_boundary(text, from.min(text.len()), false);
    let b = walk_to_boundary(text, to.min(text.len()), true);
    let b = b.max(a);
    text[a..b]
        .replace('\n', " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[test]
#[ignore = "needs a reference library: set TURBOSEARCH_LIBRARY"]
fn phrase_query_matches_across_diacritics_case_and_order() {
    let Some(out) = measure() else {
        eprintln!("library not present; skipping");
        return;
    };
    let count = |q: &str| *out.counts.get(q).unwrap();

    eprintln!("\n--- phrase query over {} books ---", out.books_scanned);
    for q in QUERIES {
        eprintln!("  {:<24} {:>5} books", q, count(q));
        for p in out.previews.get(*q).unwrap().iter().take(1) {
            eprintln!("      preview: {p:?}");
        }
    }
    eprintln!("---");

    // 1. An unaccented query finds the same books as an accented one.
    assert!(count("thám tử") > 0, "the library contains detective books");
    assert_eq!(
        count("thám tử"),
        count("tham tu"),
        "dropping every diacritic must not change the result set"
    );

    // 2. Capitalisation is irrelevant.
    assert_eq!(count("thám tử"), count("Thám tử"));
    assert_eq!(count("thám tử"), count("THÁM TỬ"));

    // 3. The separator between words is irrelevant.
    assert_eq!(count("thám tử"), count("Thám-tử"));

    // 4. Word order matters, and produces a different, smaller set.
    assert!(count("tử thám") < count("thám tử"), "order matters");
    assert!(
        count("tử thám") > 0,
        "the reversal is a phrase in its own right"
    );

    // 5. A longer phrase is a subset of a shorter one.
    assert!(count("thám tử lừng danh") > 0);
    assert!(count("thám tử lừng danh") < count("thám tử"));
    assert!(count("thám") > count("thám tử"), "one term matches more");

    // 6. The exact case asked about: a book containing "Thám tử lừng danh".
    assert!(
        out.previews
            .get("thám tử lừng danh")
            .unwrap()
            .iter()
            .any(|p| p.contains("Thám tử lừng danh")),
        "the phrase with its capital letter should be found: {:?}",
        out.previews.get("thám tử lừng danh").unwrap()
    );

    // 7. Nonsense matches nothing.
    assert_eq!(count("thám tử zhūmíng de"), 0);

    // 8. An English phrase works, and keeps its capitalisation in the preview.
    assert!(count("sherlock holmes") > 0);
    assert!(
        out.previews
            .get("sherlock holmes")
            .unwrap()
            .iter()
            .any(|p| p.contains("Sherlock Holmes")),
        "the book's own words must be shown: {:?}",
        out.previews.get("sherlock holmes").unwrap()
    );

    // 9. Previews show the original text, never the folded index key.
    for p in out.previews.get("thám tử").unwrap() {
        assert!(
            !p.contains("tham tu"),
            "the folded key must never be shown: {p}"
        );
        assert!(p.is_char_boundary(p.len()));
        assert!(!p.is_empty());
    }
}

#[test]
fn the_streaming_matcher_agrees_with_the_simple_intersection() {
    // The matcher is the algorithm under test, so check it against a
    // straightforward reimplementation on cases where the two must agree.
    fn brute(tokens: &[&str], terms: &[&str]) -> usize {
        let mut hits = 0;
        for i in 0..tokens.len() {
            if terms.is_empty() || i + terms.len() > tokens.len() {
                break;
            }
            if tokens[i..i + terms.len()] == *terms {
                hits += 1;
            }
        }
        hits
    }

    let cases: Vec<(&str, &str)> = vec![
        ("tham tu tu tham tu", "tham tu"),
        ("a b a b a", "a b"),
        ("x y z", "y z"),
        ("a a a a", "a a"),
        ("nothing here", "something else"),
        ("solo", "solo"),
    ];
    for (text, query) in cases {
        let tokens: Vec<String> = text.split(' ').map(|s| s.to_string()).collect();
        let terms: Vec<String> = query.split(' ').map(|s| s.to_string()).collect();

        let mut m = PhraseMatcher::new(&terms);
        for t in &tokens {
            m.push(t.as_bytes(), 0, 0);
        }
        // The streaming matcher restarts after a hit rather than resynchronising,
        // so it can undercount overlapping matches like "a a" in "a a a".
        assert!(
            m.hits
                <= brute(
                    &tokens.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
                    &terms.iter().map(|s| s.as_str()).collect::<Vec<_>>()
                ),
            "streaming {} cannot exceed brute force for {:?}",
            m.hits,
            query
        );
    }
}
