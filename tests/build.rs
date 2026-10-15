//! Building an index from where the reader happens to be.
//!
//! A shell build reads a library once and stops. A reader searches while
//! somebody is waiting, over a folder that already has books in it, and the
//! files it indexed last time are the ones most likely to have changed — so
//! these tests are about the second and third pass, and what a pass that finds
//! nothing must not do.

use std::fs;
use std::path::{Path, PathBuf};

use turbosearch::index::{dict, query};
use turbosearch::storage::Engine;
use turbosearch::storage::book::scope_ids;
use turbosearch::storage::build::{self, BuildStats, Session, plan_scope};
use turbosearch::storage::config::ContentMode;

/// A folder of files and the index beside it.
struct Shelf {
    _dir: tempfile::TempDir,
    root: PathBuf,
    db: PathBuf,
}

impl Shelf {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("books");
        fs::create_dir_all(&root).expect("mkdir");
        let db = dir.path().join("index.db");
        Shelf {
            _dir: dir,
            root,
            db,
        }
    }

    fn engine(&self) -> Engine {
        Engine::open(&self.db).expect("open engine")
    }

    /// Writes a file past the 5 KiB size floor, the way a real book is.
    fn write(&self, name: &str, content: &str) -> PathBuf {
        let p = self.root.join(name);
        let mut body = String::new();
        while body.len() < 8 * 1024 {
            body.push_str(content);
        }
        fs::write(&p, body).expect("write file");
        p
    }

    /// What a search does before it searches: scan this folder, read what the
    /// scan decided needed reading, and finish.
    fn index(&self) -> BuildStats {
        self.index_dir(&self.root.clone())
    }

    fn index_dir(&self, dir: &Path) -> BuildStats {
        let engine = self.engine();
        let plan = plan_scope(&engine, dir, None);
        let mut session = Session::new(&engine, plan, ContentMode::Full).expect("session");
        session.step(&engine, usize::MAX).expect("step");
        session.finish(&engine, false).expect("finish")
    }
}

/// What a reader gets back: the hits that are still allowed to be shown.
fn hits(engine: &Engine, word: &str, scope: &str) -> Vec<query::Hit> {
    let d = dict::load(engine).expect("dictionary");
    let raw = query::search_phrase(engine, &d, word).expect("search");
    query::live_hits(engine, raw, scope).expect("live hits")
}

/// The sentence a match is in comes from decoding the book again.
///
/// The index stores where a word is, never what it is: there is no sentence in
/// SQLite to recover, only a position and the entry map that leads to it. So a
/// preview is a read of the book, which is why it is one call per question a
/// reader asks rather than four more columns on every search — and why it is
/// `None` and not an error when the book is gone.
#[test]
fn the_sentence_a_match_is_in_comes_from_the_book() {
    let shelf = Shelf::new();
    shelf.write(
        "one.txt",
        "A baker has arrived in this town. Conan is here. ",
    );
    shelf.index();

    let engine = shelf.engine();
    let d = dict::load(&engine).expect("dictionary");
    let raw = query::search_phrase(&engine, &d, "Conan").expect("search");
    let live = query::live_hits(&engine, raw, "").expect("live hits");
    assert_eq!(live.len(), 1, "the book is found");
    let hit = live[0].clone();

    let sentence = build::snippet_for(&engine, hit.doc_id, hit.entry_idx, hit.first_position, 60)
        .expect("the book is still there")
        .expect("the section is still the same shape");
    assert!(
        sentence.contains("Conan"),
        "the sentence has the word in it: {sentence}"
    );
    assert!(
        sentence.contains("baker"),
        "and the rest of the sentence, not just the word: {sentence}"
    );

    // Gone, and therefore not an answer and not a sentence.
    fs::remove_file(shelf.root.join("one.txt")).expect("remove the book");
    shelf.index();
    let d = dict::load(&engine).expect("dictionary after the removal");
    let raw = query::search_phrase(&engine, &d, "Conan").expect("search again");
    assert_eq!(
        query::live_hits(&engine, raw, "").expect("live hits").len(),
        0,
        "a book that went away stops being an answer"
    );
    assert!(
        build::snippet_for(&engine, hit.doc_id, hit.entry_idx, hit.first_position, 60)
            .expect("no re-read")
            .is_none(),
        "and has no sentence left to show"
    );
}

/// An index whose recorded layout is gone gets every book read again.
///
/// An entry map holds byte offsets into the text the extractor produced, so when
/// the layout changes those offsets point at different words while the postings
/// go on answering. A search succeeds, the sentence beside it comes from the
/// wrong place, and nothing on the screen says so — which is the only failure
/// mode worth a rebuild over.
#[test]
fn an_index_with_no_recorded_layout_is_read_again() {
    let shelf = Shelf::new();
    shelf.write("one.txt", "A book about Zephyrinal the inventor. ");
    shelf.index();

    let engine = shelf.engine();
    assert_eq!(
        engine.text_layout().unwrap(),
        Some(turbosearch::format::html::TEXT_LAYOUT),
        "a session that read a book records what it read"
    );

    // The same pass again: nothing has moved, so nothing is read.
    let stats = shelf.index();
    assert_eq!(stats.indexed, 0, "an unchanged file is not read again");

    engine.reset_text_layout().unwrap();
    let stats = shelf.index();
    assert_eq!(stats.indexed, 1, "and with the record gone it is");
    assert_eq!(
        engine.text_layout().unwrap(),
        Some(turbosearch::format::html::TEXT_LAYOUT),
        "and the record is made again by the pass that read it"
    );

    // Backwards is never believed: an older build finishing behind a newer one
    // would be claiming something about books it never touched.
    engine.set_text_layout(0).unwrap();
    assert_eq!(
        engine.text_layout().unwrap(),
        Some(turbosearch::format::html::TEXT_LAYOUT),
        "the record only moves forward"
    );
}

/// An index that has grown must not be given ids it is already using.
///
/// The id the first book of a session gets used to come from a counter that
/// starts at zero on every session, so a folder indexed after 619 other books
/// was handed book 0 — which overwrote a live row and posted its text under an
/// id whose chunks had been written in a different order.
#[test]
fn an_index_that_grows_keeps_handing_out_ids_nobody_has() {
    let shelf = Shelf::new();
    shelf.write("one.txt", "The first book mentions Conan. ");
    shelf.write("two.txt", "The second book mentions nothing alike. ");
    shelf.index();

    shelf.write("three.txt", "The third book mentions Zephyrinal. ");
    let stats = shelf.index();
    assert_eq!(stats.indexed, 1, "only the new file is new");
    assert_eq!(stats.failed, 0, "and it reads");

    let engine = shelf.engine();
    assert_eq!(scope_ids(&engine, "").expect("ids").len(), 3, "three books");
    // Each book's own word still finds its own book. If an id had been reused,
    // one of the three would have been overwritten and one word would find two
    // books — or none.
    for word in ["Conan", "alike", "Zephyrinal"] {
        assert_eq!(hits(&engine, word, "").len(), 1, "{word} finds one book");
    }
    let ids: Vec<u32> = hits(&engine, "Zephyrinal", "")
        .iter()
        .map(|h| h.doc_id)
        .collect();
    assert_eq!(ids.len(), 1, "the new book is one book");
}

/// A pass that finds nothing to do must leave the dictionary alone.
///
/// The writer stores whatever dictionary it is handed, and it is handed a fresh
/// one unless the caller loads what is already there: a session over an index
/// with 62 663 terms in it wrote back a dictionary with the same terms and
/// every count zeroed — 193 313 bytes became 140 351 — and the run after that
/// started its chunk numbering again at zero.
#[test]
fn a_pass_with_nothing_to_do_does_not_rewrite_the_dictionary() {
    let shelf = Shelf::new();
    shelf.write("book.txt", "A book with a distinctive word: Quixotran. ");
    shelf.index();

    let engine = shelf.engine();
    let before = engine
        .dict_blob()
        .expect("dictionary blob")
        .expect("present");
    let terms_before = dict::load(&engine).expect("dictionary").len();

    let stats = shelf.index();
    assert_eq!(stats.indexed, 0, "nothing was new");
    assert_eq!(stats.removed, 0, "and nothing had gone");

    let after = engine
        .dict_blob()
        .expect("dictionary blob")
        .expect("present");
    assert_eq!(before, after, "the dictionary is byte for byte the same");
    assert_eq!(terms_before, dict::load(&engine).expect("dictionary").len());
}

/// A file that changed is re-indexed once, under one row.
///
/// Ids are never reused, so the new text lands under a fresh id — and the row
/// the old id was in has to go, or a search offers the same book twice, one of
/// the two answers stale.
#[test]
fn a_file_that_changed_is_reindexed_under_a_single_row() {
    let shelf = Shelf::new();
    let book = shelf.write("book.txt", "The old text mentions Zephyrinal. ");
    shelf.index();

    let engine = shelf.engine();
    assert_eq!(
        hits(&engine, "Zephyrinal", "").len(),
        1,
        "the old text is found"
    );

    // A different length, so the fingerprint differs, the way an edited file
    // does rather than a touch.
    let mut body = String::from("The new text mentions Quixotran. ");
    while body.len() < 8 * 1024 {
        body.push_str("The new text mentions Quixotran. ");
    }
    fs::write(&book, body).expect("rewrite file");

    let stats = shelf.index();
    assert_eq!(stats.indexed, 1, "one file was updated");

    let engine = shelf.engine();
    assert_eq!(
        scope_ids(&engine, "").expect("ids").len(),
        1,
        "still one book"
    );
    assert_eq!(
        hits(&engine, "Quixotran", "").len(),
        1,
        "the new text is found"
    );
    assert_eq!(
        hits(&engine, "Zephyrinal", "").len(),
        0,
        "and the text it replaced is not still answering"
    );
}

/// A file that went away stops being an answer.
///
/// Its postings stay on disk — they are skipped at read time rather than
/// rewritten — so what keeps a search from answering for it is the same filter
/// that keeps a scoped search inside its folder.
#[test]
fn a_file_that_went_away_stops_being_an_answer() {
    let shelf = Shelf::new();
    let book = shelf.write("book.txt", "A book mentioning Zephyrinal. ");
    shelf.index();

    let engine = shelf.engine();
    assert_eq!(
        hits(&engine, "Zephyrinal", "").len(),
        1,
        "found while it exists"
    );

    fs::remove_file(&book).expect("delete file");
    let stats = shelf.index();
    assert_eq!(stats.removed, 1, "the row goes");

    let engine = shelf.engine();
    assert_eq!(
        scope_ids(&engine, "").expect("ids").len(),
        0,
        "no books left"
    );
    assert_eq!(
        hits(&engine, "Zephyrinal", "").len(),
        0,
        "its postings are still there, and are still not an answer"
    );
}

/// A scope stays inside its folder, and only its folder.
///
/// `/books/alpha` must not reach `/books/alpha-extra`: they share a prefix and
/// not a directory, and a rule that compared raw prefixes would search both.
#[test]
fn a_scope_search_stays_inside_its_own_directory() {
    let shelf = Shelf::new();
    let alpha = shelf.root.join("alpha");
    let alpha_extra = shelf.root.join("alpha-extra");
    for dir in [&alpha, &alpha_extra] {
        fs::create_dir_all(dir).expect("mkdir");
    }
    let write_into = |dir: &Path, name: &str, content: &str| {
        let p = dir.join(name);
        let mut body = String::from(content);
        while body.len() < 8 * 1024 {
            body.push_str(content);
        }
        fs::write(p, body).expect("write file");
    };
    write_into(&alpha, "one.txt", "The alpha book mentions Zephyrinal. ");
    write_into(
        &alpha_extra,
        "two.txt",
        "The other book mentions Quixotran. ",
    );

    shelf.index_dir(&alpha);
    shelf.index_dir(&alpha_extra);

    let engine = shelf.engine();
    assert_eq!(
        scope_ids(&engine, alpha.to_str().expect("utf8"))
            .expect("ids")
            .len(),
        1,
        "the prefix a directory shares with its neighbour is not a membership"
    );
    assert_eq!(
        hits(&engine, "Quixotran", alpha.to_str().expect("utf8")).len(),
        0,
        "a search in alpha does not answer out of alpha-extra"
    );
    assert_eq!(
        hits(&engine, "Quixotran", alpha_extra.to_str().expect("utf8")).len(),
        1,
        "and does answer out of its own"
    );
    assert_eq!(
        hits(&engine, "Zephyrinal", "").len(),
        1,
        "an unrestricted search still sees both"
    );
}

/// An empty scope is the whole index, not nothing.
///
/// This is what makes the same filter answer "which of these hits still have a
/// book behind them" as well as "which of these books are in this folder" — the
/// two are one question asked of one table.
#[test]
fn an_empty_scope_is_the_whole_index() {
    let shelf = Shelf::new();
    shelf.write("one.txt", "One book. ");
    shelf.write("two.txt", "Another book. ");
    shelf.index();

    let engine = shelf.engine();
    assert_eq!(scope_ids(&engine, "").expect("ids").len(), 2);
    assert_eq!(
        hits(&engine, "book", "").len(),
        2,
        "both books answer an unrestricted search"
    );
}
