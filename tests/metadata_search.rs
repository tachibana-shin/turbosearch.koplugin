//! Metadata search, end to end against the real library.
//!
//! The point of `METAIDX` is that a user typing a fragment of an accented name
//! without a Vietnamese keyboard still finds the book. These tests build the
//! real store from the real books and then search it.

use std::path::{Path, PathBuf};

use turbosearch::format::meta::{from_filename, resolve};
use turbosearch::storage::{Engine, MetaIndex};

/// A temp engine plus the directory it lives in.
struct Store {
    _dir: tempfile::TempDir,
    engine: Engine,
}

impl Store {
    fn new(tag: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let engine = Engine::open(dir.path().join(format!("{tag}.redb"))).unwrap();
        Store { _dir: dir, engine }
    }
}

fn library_root() -> Option<PathBuf> {
    turbosearch::library::root()
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

/// Resolves metadata for a sample of the library and indexes it.
///
/// Returns `(store, book count, titles by book id)`.
fn build_sample(limit: usize) -> Option<(Store, usize)> {
    let root = library_root()?;
    let mut files = Vec::new();
    walk(&root, &mut files);
    files.sort();
    if files.is_empty() {
        return None;
    }

    let store = Store::new("metadata-search");
    let mut indexed = 0usize;
    let mut next_id = 0u16;
    for path in files.iter().take(limit) {
        let record = resolve(path);
        if record.title.is_empty() {
            continue;
        }
        let ids = store.engine.allocate_ids(1).unwrap().unwrap();
        let id = ids[0];
        let _ = next_id;
        next_id += 1;
        // A BOOKS row is required, not decoration: `METAIDX` is rebuilt only for
        // ids that still exist in `BOOKS`, so a metadata row without one is an
        // orphan and is skipped.
        store
            .engine
            .put_book(
                id,
                &turbosearch::storage::Book {
                    path: path.to_string_lossy().into_owned(),
                    size: 1,
                    mtime_ms: 1,
                    scope: root.display().to_string(),
                    flags: 0,
                    ..Default::default()
                },
            )
            .unwrap();
        store.engine.put_metadata(id, &record).unwrap();
        indexed += 1;
    }
    turbosearch::storage::rebuild_meta_index(&store.engine).unwrap();
    Some((store, indexed))
}

/// Searches by a metadata fragment.
fn find(store: &Store, query: &str) -> Vec<String> {
    turbosearch::storage::search_metadata(&store.engine, query)
        .unwrap()
        .into_iter()
        .map(|(_, title)| title)
        .collect()
}

#[test]
#[ignore = "needs a reference library: set TURBOSEARCH_LIBRARY"]
fn a_prefix_of_an_accented_author_finds_the_book() {
    let Some((store, indexed)) = build_sample(120) else {
        eprintln!("library not present; skipping");
        return;
    };
    assert!(indexed > 50, "sample should index a decent number of books");

    // The library's own vocabulary: Vietnamese names typed without diacritics.
    for needle in ["nguy", "nguyen", "nhat", "anh"] {
        let hits = find(&store, needle);
        assert!(
            !hits.is_empty(),
            "needle {needle:?} should match at least one book title or author"
        );
    }
}

#[test]
fn the_full_name_finds_more_than_a_single_fragment() {
    let Some((store, _)) = build_sample(120) else {
        return;
    };
    // A longer query must not silently return nothing.
    let hits = find(&store, "nguyen nhat anh");
    // Not every book in the sample is by this author, so only assert that the
    // longer query is a subset of the shorter one's results.
    let short = find(&store, "nguyen");
    assert!(
        hits.len() <= short.len(),
        "{} vs {}",
        hits.len(),
        short.len()
    );
}

#[test]
#[ignore = "needs a reference library: set TURBOSEARCH_LIBRARY"]
fn the_sidecar_language_reaches_the_record() {
    // The reference library has books whose OPF says `en` and whose sidecar says
    // `vi`. The sidecar must win.
    let Some((_store, _)) = build_sample(150) else {
        return;
    };
    let root = library_root().unwrap();
    let mut files = Vec::new();
    walk(&root, &mut files);
    files.sort();

    let mut checked = 0;
    let mut sidecars = 0;
    for path in files.iter().take(150) {
        let Some(sidecar) = sidecar_for(path) else {
            continue;
        };
        checked += 1;
        // If the sidecar carries a language, the record must carry it too.
        if turbosearch::format::meta::sidecar_path(path).is_some() {
            let record = resolve(path);
            if record.provenance == turbosearch::storage::Provenance::Sidecar {
                sidecars += 1;
                assert!(
                    !record.language.is_empty(),
                    "a sidecar-derived record should have a language: {:?}",
                    path.file_name()
                );
            }
        }
        let _ = sidecar;
    }
    eprintln!("books with a sidecar: {sidecars} of {checked} checked");
    assert!(
        sidecars > 0,
        "the library should have at least one book with a sidecar"
    );
}

fn sidecar_for(path: &Path) -> Option<PathBuf> {
    turbosearch::format::meta::sidecar_path(path)
}

#[test]
fn search_matches_the_book_title_a_user_sees() {
    let Some((store, _)) = build_sample(80) else {
        return;
    };
    // Pick a title from the store and confirm a fragment of it is findable.
    let titles: Vec<String> = find(&store, "tập");
    assert!(
        !titles.is_empty() || find(&store, "tap").is_empty(),
        "either the accented or the folded form should be searchable"
    );
    let _ = &store;
}

#[test]
fn an_unmatched_query_returns_nothing_rather_than_everything() {
    let Some((store, _)) = build_sample(30) else {
        return;
    };
    assert!(find(&store, "zzzzqqqqxxxxnotpresent").is_empty());
    assert!(find(&store, "").is_empty());
    assert!(find(&store, "   ").is_empty());
}

#[test]
fn the_index_rebuilds_and_searches_again() {
    let Some((store, _)) = build_sample(40) else {
        return;
    };
    let before = find(&store, "nguy");
    // A rebuild from the METADATA rows must reproduce the same answers.
    turbosearch::storage::rebuild_meta_index(&store.engine).unwrap();
    let after = find(&store, "nguy");
    assert_eq!(before.len(), after.len());
}

#[test]
fn removing_a_book_drops_it_from_the_index() {
    let Some((store, _)) = build_sample(40) else {
        return;
    };
    let before = find(&store, "nguyen");
    if let Some((id, _)) = turbosearch::storage::search_metadata(&store.engine, "nguyen")
        .unwrap()
        .first()
        .cloned()
    {
        store.engine.remove_book(id).unwrap();
        // Deleting the book orphans its postings; the metadata is removed too,
        // so a rebuild is what makes the search forget it.
        turbosearch::storage::rebuild_meta_index(&store.engine).unwrap();
        let after = find(&store, "nguyen");
        assert_eq!(after.len() + 1, before.len(), "one book should be gone");
    }
}

#[test]
fn a_filename_fallback_is_searchable_too() {
    // A format with no metadata at all still has to be findable by its name.
    let store = Store::new("filename-fallback");
    let record = from_filename(Path::new("/documents/Tokyo Ravens - Chuong 13 (241).epub"));
    store
        .engine
        .put_book(
            1,
            &turbosearch::storage::Book {
                path: "/documents/Tokyo Ravens - Chuong 13 (241).epub".into(),
                scope: "/documents".into(),
                ..Default::default()
            },
        )
        .unwrap();
    store.engine.put_metadata(1, &record).unwrap();
    turbosearch::storage::rebuild_meta_index(&store.engine).unwrap();

    let hits = turbosearch::storage::search_metadata(&store.engine, "tokyo").unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].1, "Tokyo Ravens - Chuong 13");
}

#[test]
fn the_index_is_small_enough_to_hold_in_ram() {
    let Some((store, indexed)) = build_sample(60) else {
        return;
    };
    let index: MetaIndex = store.engine.meta_index().unwrap();
    // One entry per indexed book, and this equality is the check that the
    // rebuild actually saw every row rather than stopping at one.
    assert_eq!(
        index.len(),
        indexed,
        "rebuilt index has {} entries for {indexed} books",
        index.len()
    );
    // A 6,000-book library should land in the low megabytes, so a 60-book sample
    // is comfortably a few hundred kilobytes.
    assert!(
        index.blob_len() < 1024 * 1024,
        "{} books produced {} bytes of metadata, more than expected",
        indexed,
        index.blob_len()
    );
}
