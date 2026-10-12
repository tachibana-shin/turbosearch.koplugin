//! Measures query latency against an existing index.
//!
//! ```text
//! cargo run --release --bin latency -- <db> <phrase>
//! ```
//!
//! Runs the query repeatedly and reports the distribution, because a single
//! timing on a warm cache says nothing about the worst case on a Kindle.

use std::time::Instant;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args().nth(1).expect("db path");
    let phrase = std::env::args().nth(2).unwrap_or_else(|| "thám tử".into());

    let engine = turbosearch::storage::Engine::open(&path)?;
    let dict = turbosearch::index::dict::load(&engine)?;
    eprintln!("dictionary {} terms", dict.len());

    // Show what the tokenizer actually produced for the phrase, and what the
    // dictionary holds that is nearby. A mismatch here explains every "0 hits".
    for token in turbosearch::tokenize::tokenize(&phrase) {
        let t = &token.term;
        eprintln!(
            "  token {:?} -> in dict {}, prefix range {:?}",
            String::from_utf8_lossy(t),
            dict.get(t).is_some(),
            dict.prefix_range(t)
        );
    }
    let near: Vec<String> = (0..dict.len())
        .filter(|&i| dict.term(i).starts_with(b"th"))
        .take(8)
        .map(|i| String::from_utf8_lossy(dict.term(i)).into_owned())
        .collect();
    eprintln!("  first terms starting with 'th': {near:?}");
    let first: Vec<String> = (0..5.min(dict.len()))
        .map(|i| String::from_utf8_lossy(dict.term(i)).into_owned())
        .collect();
    eprintln!("  first 5 terms: {first:?}");

    let mut phrase_times = Vec::new();
    let mut any_times = Vec::new();
    let mut hits = 0usize;
    let mut any_hits = 0usize;

    for _ in 0..20 {
        let t = Instant::now();
        let r = turbosearch::index::query::search_phrase(&engine, &dict, &phrase)?;
        phrase_times.push(t.elapsed().as_micros());
        hits = r.len();
    }
    for _ in 0..20 {
        let t = Instant::now();
        let r = turbosearch::index::query::search(&engine, &dict, &phrase, None)?;
        any_times.push(t.elapsed().as_micros());
        any_hits = r.hits.len();
    }
    // A single-word query is the interactive case: the reader types one thing.
    let t = Instant::now();
    let w = turbosearch::index::query::search(&engine, &dict, "conan", None)?;
    let single = t.elapsed().as_micros();
    let single_hits = w.hits.len();

    phrase_times.sort();
    any_times.sort();
    println!("phrase {phrase:?}: {hits} hits");
    println!(
        "  median {:>7} us   p95 {:>7} us   max {:>7} us",
        phrase_times[phrase_times.len() / 2],
        phrase_times[phrase_times.len() * 95 / 100],
        phrase_times[phrase_times.len() - 1]
    );
    println!("any-term {phrase:?}: {any_hits} hits");
    println!(
        "  median {:>7} us   p95 {:>7} us   max {:>7} us",
        any_times[any_times.len() / 2],
        any_times[any_times.len() * 95 / 100],
        any_times[any_times.len() - 1]
    );
    println!("single word \"conan\": {single_hits} hits in {single} us");
    Ok(())
}
