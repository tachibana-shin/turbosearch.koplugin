//! Integration tests for the storage layer.
//!
//! These use real temp files rather than an in-memory stand-in, because the
//! point is to exercise redb's transaction and durability behaviour.

use std::fs;
use std::path::PathBuf;

use turbosearch::storage::{
    Engine, MAX_BOOKS,
    book::{
        Book, FileEntry, FileFilter, Format, ScanMode, ScanPlan, ScopeFilter, path_in_scope,
        scan_full, scan_scope, scan_with_mode,
    },
    config::{
        Config, DEFAULT_EXCLUDE, DEFAULT_EXCLUDE_DIRS, DEFAULT_INCLUDE, IndexDepth, PrefixMode,
    },
    meta::{CODEC_VERSION, Meta, NORMALIZER_VERSION, SCHEMA_VERSION, Status, TOKENIZER_VERSION},
};

/// Body long enough to pass the default 5 KiB minimum file size.
const BODY: &str = "The quick brown fox jumps over the lazy dog. \\
Lorem ipsum dolor sit amet, consectetur adipiscing elit, sed do eiusmod tempor. ";

/// A bookshelf of files under a temp directory.
struct Shelf {
    dir: tempfile::TempDir,
    engine: Engine,
}

impl Shelf {
    fn new(tag: &str) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(format!("{tag}.redb"));
        let engine = Engine::open(&path).expect("open engine");
        Shelf { dir, engine }
    }

    /// Writes a file with the given body, padded past the default minimum size
    /// so it survives the size filter.
    fn write_file(&self, name: &str, content: &str) {
        let p = self.dir.path().join(name);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).expect("mkdir");
        }
        // Real books are far larger than the 5 KiB floor; fixtures are short, so
        // pad them rather than relying on every test author remembering to.
        let mut body = String::from(content);
        while body.len() < 8 * 1024 {
            body.push_str(content);
        }
        fs::write(p, body).expect("write file");
    }

    /// The default filter, with the size floor dropped.
    ///
    /// Most fixtures are short strings, and the dedicated size-bound tests below
    /// exercise the real 5 KiB floor separately.
    fn filter(&self) -> FileFilter {
        let mut f = Config::default().filter();
        f.min_size = 0;
        f
    }

    /// Runs a scan over the shelf root.
    fn scan(&self) -> turbosearch::storage::RunResult<ScanPlan> {
        scan_with_mode(
            &self.engine,
            &self.filter(),
            &[self.dir.path().to_path_buf()],
            ScanMode::Scope,
        )
    }
}

fn book(path: &str, size: u64, mtime: i64) -> Book {
    Book {
        path: path.to_string(),
        size,
        mtime_ms: mtime,
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// META
// ---------------------------------------------------------------------------

#[test]
fn meta_absent_on_fresh_database() {
    let s = Shelf::new("meta-absent");
    assert_eq!(s.engine.meta().unwrap(), None);
}

#[test]
fn meta_roundtrips_through_database() {
    let s = Shelf::new("meta-roundtrip");
    let m = Meta {
        config_hash: 0x1234_5678,
        book_id_hwm: 17,
        scan_cursor: Some("documents/A Bá Tá [99].txt".into()),
        db_bytes_written: 99_000,
        status: Status::Indexing,
        ..Meta::new()
    };
    s.engine.write_meta(&m).unwrap();

    let back = s.engine.meta().unwrap().expect("meta row present");
    assert_eq!(back, m);
    assert_eq!(back.book_id_hwm, 17);
}

#[test]
fn meta_rewrites_in_place() {
    let s = Shelf::new("meta-rewrite");
    s.engine.write_meta(&Meta::new()).unwrap();
    s.engine
        .write_meta(&Meta {
            book_id_hwm: 5,
            ..Meta::new()
        })
        .unwrap();

    let rows = s.engine.meta().unwrap();
    assert_eq!(rows.map(|m| m.book_id_hwm), Some(5));
}

#[test]
fn meta_version_gates_are_independent() {
    // Only one mismatch at a time must invalidate; otherwise a codec change
    // would silently produce wrong postings on an old file.
    let cases: Vec<fn(&mut Meta)> = vec![
        |m| m.schema_version += 1,
        |m| m.tokenizer_version += 1,
        |m| m.normalizer_version += 1,
        |m| m.codec_version += 1,
    ];
    for mutate in cases {
        let mut m = Meta::new();
        mutate(&mut m);
        assert!(
            !m.is_compatible(),
            "a version bump must invalidate the file"
        );
    }
    assert!(Meta::new().is_compatible());
}

#[test]
fn meta_version_constants_are_monotonic() {
    // Guards against someone reordering or duplicating the constants.
    // These are pinned values: bumping one without rebuilding the file is a
    // correctness bug, so the test records what the build actually shipped.
    assert_eq!(
        (
            SCHEMA_VERSION,
            TOKENIZER_VERSION,
            NORMALIZER_VERSION,
            CODEC_VERSION
        ),
        (1, 1, 1, 1),
        "version constants changed; update note.md section 2 alongside"
    );
    assert_eq!(MAX_BOOKS, u16::MAX as u32 + 1, "book_id must stay a u16");
}

// ---------------------------------------------------------------------------
// BOOKS / PATHS
// ---------------------------------------------------------------------------

#[test]
fn put_book_is_visible_to_both_tables() {
    let s = Shelf::new("put-book");
    let b = Book {
        scope: "documents".into(),
        path: "documents/One.epub".into(),
        size: 4096,
        mtime_ms: 1_700_000_000_000,
        inode: 12345,
        format: Format::Epub,
        entry_count: 3,
        token_count: 90_000,
        flags: 0,
        indexed_at_ms: 1_700_000_100_000,
    };
    s.engine.put_book(1, &b).unwrap();

    assert_eq!(s.engine.book(1).unwrap(), Some(b.clone()));
    assert_eq!(
        s.engine.book_id_for_path("documents/One.epub").unwrap(),
        Some(1)
    );
    assert!(
        s.engine
            .book_id_for_path("documents/Missing.epub")
            .unwrap()
            .is_none()
    );
    assert_eq!(s.engine.book(999).unwrap(), None);
}

#[test]
fn put_book_overwrites_same_id() {
    let s = Shelf::new("put-overwrite");
    let mut b = book("a.epub", 10, 1);
    s.engine.put_book(1, &b).unwrap();
    b.size = 20;
    b.token_count = 7;
    s.engine.put_book(1, &b).unwrap();

    assert_eq!(s.engine.book(1).unwrap(), Some(b));
    assert_eq!(
        s.engine.all_books().unwrap().len(),
        1,
        "id must not duplicate"
    );
    assert_eq!(s.engine.read_paths().unwrap().len(), 1);
}

#[test]
fn remove_book_clears_both_tables() {
    let s = Shelf::new("remove-book");
    s.engine.put_book(1, &book("a.epub", 10, 1)).unwrap();

    assert!(s.engine.remove_book(1).unwrap(), "row existed");
    assert_eq!(s.engine.book(1).unwrap(), None);
    assert_eq!(s.engine.book_id_for_path("a.epub").unwrap(), None);
}

#[test]
fn remove_book_is_idempotent() {
    let s = Shelf::new("remove-twice");
    s.engine.put_book(1, &book("a.epub", 10, 1)).unwrap();
    assert!(s.engine.remove_book(1).unwrap(), "first remove succeeds");
    assert!(
        !s.engine.remove_book(1).unwrap(),
        "second remove is a no-op"
    );
}

#[test]
fn all_books_is_ordered_by_id() {
    let s = Shelf::new("all-books");
    for id in [7u16, 3, 11, 1] {
        s.engine
            .put_book(id, &book(&format!("{id}.epub"), 10, id as i64))
            .unwrap();
    }
    let ids: Vec<u16> = s
        .engine
        .all_books()
        .unwrap()
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    assert_eq!(ids, vec![1, 3, 7, 11]);
}

#[test]
fn read_paths_maps_every_path() {
    let s = Shelf::new("read-paths");
    for (id, name) in [(1u16, "a.epub"), (2, "b.epub"), (3, "c.epub")] {
        s.engine.put_book(id, &book(name, 1, 1)).unwrap();
    }
    let map = s.engine.read_paths().unwrap();
    assert_eq!(map.len(), 3);
    assert_eq!(map.get("b.epub"), Some(&2));
}

#[test]
fn book_encoding_survives_odd_paths() {
    let s = Shelf::new("odd-paths");
    // Filenames from a real library: punctuation, brackets, spaces, CJK, accents.
    let paths = [
        "documents/Tokyo Ravens - Chuong 13_ Dem nguoc - Kouhei Azano (241).epub",
        "documents/Yến Tiệc Của Sherlockian Và Bão Tố Mùa Xuân - Mai Mochizuki & Ninh Nhân Văn (dịch) (9).epub",
        "documents/中文测试/こんにちは.txt",
        "documents/emoji \u{1F4DA} title.md",
        "documents/quote\"and'single.txt",
        "documents/",
    ];
    for (i, p) in paths.iter().enumerate() {
        let b = book(p, (i + 1) as u64, i as i64);
        s.engine.put_book(i as u16, &b).unwrap();
        assert_eq!(s.engine.book(i as u16).unwrap(), Some(b), "path {p}");
    }
}

#[test]
fn book_flags_roundtrip_all_bits() {
    let s = Shelf::new("flags");
    for (i, bits) in [(0u8, 0u8), (1, 1), (2, 2), (3, 4), (4, 7)] {
        let b = Book {
            flags: bits,
            ..book(&format!("f{i}.epub"), 1, 1)
        };
        s.engine.put_book(i as u16, &b).unwrap();
        assert_eq!(
            s.engine.book(i as u16).unwrap().unwrap().flags,
            bits,
            "flags {bits}"
        );
    }
}

#[test]
fn every_format_roundtrips() {
    let s = Shelf::new("formats");
    for (i, f) in [
        Format::Unknown,
        Format::Epub,
        Format::Kepub,
        Format::Mobi,
        Format::Azw,
        Format::Azw3,
        Format::Txt,
        Format::Markdown,
        Format::Html,
        Format::Xhtml,
        Format::Fb2,
        Format::MetadataOnly,
    ]
    .into_iter()
    .enumerate()
    {
        let b = Book {
            format: f,
            ..book(&format!("x{i}"), 1, 1)
        };
        s.engine.put_book(i as u16, &b).unwrap();
        assert_eq!(s.engine.book(i as u16).unwrap().unwrap().format, f);
    }
}

#[test]
fn format_from_extension_covers_defaults() {
    for ext in DEFAULT_INCLUDE {
        let f = Format::from_extension(ext);
        assert_ne!(f, Format::Unknown, "extension {ext} must map to a format");
    }
    assert_eq!(
        Format::from_extension("EPUB"),
        Format::Epub,
        "case-insensitive"
    );
    assert_eq!(Format::from_extension("nope"), Format::Unknown);
}

// ---------------------------------------------------------------------------
// Id allocation
// ---------------------------------------------------------------------------

#[test]
fn allocate_ids_is_monotone_and_never_reused() {
    let s = Shelf::new("alloc");
    let first: Vec<u16> = s.engine.allocate_ids(3).unwrap().unwrap();
    let second: Vec<u16> = s.engine.allocate_ids(2).unwrap().unwrap();
    assert_eq!(first, vec![0, 1, 2]);
    assert_eq!(second, vec![3, 4]);

    // Even after a book is deleted, its id is gone for good.
    s.engine.remove_book(2).unwrap();
    let third: Vec<u16> = s.engine.allocate_ids(1).unwrap().unwrap();
    assert_eq!(third, vec![5], "ids are never reused");
}

#[test]
fn allocate_zero_is_noop() {
    let s = Shelf::new("alloc-zero");
    assert!(s.engine.allocate_ids(0).unwrap().unwrap().is_empty());
    // Allocating nothing must not even create the META row.
    assert_eq!(s.engine.meta().unwrap(), None);
}

#[test]
fn allocate_persists_high_water_mark() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hwm.redb");
    {
        let e = Engine::open(&path).unwrap();
        e.allocate_ids(10).unwrap().unwrap();
    }
    // Reopen: the mark must survive a restart.
    let e = Engine::open(&path).unwrap();
    let next = e.allocate_ids(1).unwrap().unwrap();
    assert_eq!(
        next,
        vec![10],
        "high-water mark must persist across restarts"
    );
}

#[test]
fn allocate_reports_when_id_space_exhausted() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("exhausted.redb");
    let e = Engine::open(&path).unwrap();
    // The mark is its own `meta` row, seeded here to one below the top, which is
    // the state a full library is in.
    e.seed_next_id(MAX_BOOKS - 1).unwrap();
    // The last usable id is 65_535; allocating it must succeed.
    let last = e.allocate_ids(1).unwrap().expect("one id left");
    assert_eq!(last, vec![u16::MAX]);
    // Any further allocation must report exhaustion, not wrap around.
    assert_eq!(e.allocate_ids(1).unwrap(), None, "space is exhausted");
    assert_eq!(e.allocate_ids(1000).unwrap(), None, "no wrap-around");
}

// ---------------------------------------------------------------------------
// Scan
// ---------------------------------------------------------------------------

#[test]
fn scan_finds_new_files() {
    let s = Shelf::new("scan-new");
    s.write_file("a.epub", BODY);
    s.write_file("b.epub", BODY);
    s.write_file("sub/c.mobi", BODY);

    let plan = s.scan().unwrap();
    assert_eq!(plan.new.len(), 3);
    assert_eq!(plan.total(), 3);
    assert!(plan.needs_work());
}

#[test]
fn scan_is_idempotent_when_nothing_changes() {
    let s = Shelf::new("scan-stable");
    s.write_file("a.epub", BODY);

    // First scan proposes; the caller then indexes and records the book.
    let plan1 = s.scan().unwrap();
    assert_eq!(plan1.new.len(), 1);
    let entry = plan1.new[0].clone();
    s.engine
        .put_book(
            1,
            &book(&entry.path.to_string_lossy(), entry.size, entry.mtime_ms),
        )
        .unwrap();

    let plan2 = s.scan().unwrap();
    assert_eq!(plan2.new.len(), 0, "nothing new on the second pass");
    assert_eq!(
        plan2.unchanged.len(),
        1,
        "the file is now known and unchanged"
    );
    assert!(!plan2.needs_work());
}

#[test]
fn scan_detects_a_rewritten_file() {
    let s = Shelf::new("scan-changed");
    let p = s.dir.path().join("a.epub");
    fs::write(&p, BODY.as_bytes()).unwrap();
    let plan = s.scan().unwrap();
    let e = &plan.new[0];
    s.engine
        .put_book(1, &book(&e.path.to_string_lossy(), e.size, e.mtime_ms))
        .unwrap();

    // Same path, different size and mtime.
    std::thread::sleep(std::time::Duration::from_millis(20));
    fs::write(
        &p,
        "rewritten, and comfortably past the 5 KiB floor"
            .repeat(200)
            .as_bytes(),
    )
    .unwrap();

    let plan = s.scan().unwrap();
    assert_eq!(plan.updated.len(), 1, "size+mtime change must be detected");
    assert_eq!(plan.new.len(), 0);
}

#[test]
fn scan_mtime_granularity_is_handled() {
    // vfat stores mtime with 2-second granularity, so a same-size rewrite inside
    // the same 2-second window can look unchanged. That is an accepted risk
    // (`note.md` section 0, row 7), so assert only that we never crash on it.
    let s = Shelf::new("scan-granularity");
    let p = s.dir.path().join("a.epub");
    fs::write(&p, "aaaa").unwrap();
    let plan = s.scan().unwrap();
    let e = &plan.new[0];
    s.engine
        .put_book(1, &book(&e.path.to_string_lossy(), e.size, e.mtime_ms))
        .unwrap();
    fs::write(&p, "bbbb").unwrap();

    let plan = s.scan().unwrap();
    // Size is unchanged, so at worst it lands in `unchanged`; either way the
    // scan must complete and report a consistent total.
    assert_eq!(plan.total(), 1);
}

#[test]
fn scan_finds_deleted_books() {
    let s = Shelf::new("scan-deleted");
    s.write_file("a.epub", BODY);
    s.write_file("b.epub", BODY);
    let plan = s.scan().unwrap();
    for (i, e) in plan.new.iter().enumerate() {
        s.engine
            .put_book(
                i as u16,
                &book(&e.path.to_string_lossy(), e.size, e.mtime_ms),
            )
            .unwrap();
    }
    fs::remove_file(s.dir.path().join("a.epub")).unwrap();

    let plan = s.scan().unwrap();
    assert_eq!(
        plan.missing.len(),
        1,
        "the removed file is reported missing"
    );
    assert_eq!(plan.unchanged.len(), 1);
}

#[test]
fn scan_orders_work_by_mtime_descending() {
    let s = Shelf::new("scan-order");
    for name in ["old.epub", "mid.epub", "new.epub"] {
        s.write_file(name, "x");
        std::thread::sleep(std::time::Duration::from_millis(15));
    }
    // Force distinct mtimes.
    let now = std::time::SystemTime::now();
    for (i, name) in ["old.epub", "mid.epub", "new.epub"].iter().enumerate() {
        let t = now - std::time::Duration::from_secs(3600 * (3 - i as u64));
        let f = fs::File::options()
            .write(true)
            .open(s.dir.path().join(name))
            .unwrap();
        f.set_times(fs::FileTimes::new().set_accessed(t).set_modified(t))
            .unwrap();
    }

    let plan = s.scan().unwrap();
    let times: Vec<i64> = plan.new.iter().map(|e| e.mtime_ms).collect();
    let sorted = {
        let mut t = times.clone();
        t.sort_by(|a, b| b.cmp(a));
        t
    };
    assert_eq!(times, sorted, "newest first, per note.md section 1.2");
}

#[test]
fn size_bounds_are_enforced() {
    let s = Shelf::new("scan-size");
    // Written directly, bypassing the padding helper, so the sizes are exact.
    fs::write(s.dir.path().join("tiny.epub"), "x").unwrap();
    fs::write(s.dir.path().join("big.epub"), "z".repeat(40 * 1024)).unwrap();
    fs::write(s.dir.path().join("huge.epub"), "z".repeat(200 * 1024)).unwrap();

    let plan = scan_with_mode(
        &s.engine,
        &Config::default().filter(),
        &[s.dir.path().to_path_buf()],
        ScanMode::Scope,
    )
    .unwrap();
    let names: Vec<String> = plan
        .new
        .iter()
        .map(|e| e.path.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert!(
        !names.contains(&"tiny.epub".to_string()),
        "below min size is skipped"
    );
    assert!(
        names.contains(&"big.epub".to_string()),
        "in-range files are scanned"
    );

    // Now set a tight maximum and confirm the large file drops out.
    let cfg = Config {
        max_file_size: 50 * 1024,
        ..Config::default()
    };
    let plan = scan_with_mode(
        &s.engine,
        &cfg.filter(),
        &[s.dir.path().to_path_buf()],
        ScanMode::Scope,
    )
    .unwrap();
    let names: Vec<String> = plan
        .new
        .iter()
        .map(|e| e.path.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert!(names.contains(&"big.epub".to_string()));
    assert!(
        !names.contains(&"huge.epub".to_string()),
        "above max size is skipped"
    );
}

#[test]
fn scan_skips_sidecar_and_hidden_directories() {
    let s = Shelf::new("scan-dirs");
    s.write_file("keep.epub", BODY);
    // Sidecar dirs, hidden dirs, and junk extensions all live next to books.
    s.write_file("keep.sdr/metadata.epub.lua", "return {}");
    s.write_file("keep.sdr/cover.jpg", "\0\0");
    s.write_file(".hidden/inside.epub", BODY);
    s.write_file("__MACOSX/junk.epub", BODY);
    s.write_file("notes.log", "x");
    s.write_file("cover.png", "\0");
    s.write_file("partial.part", "x");

    let plan = s.scan().unwrap();
    let names: Vec<String> = plan
        .new
        .iter()
        .map(|e| e.path.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, vec!["keep.epub".to_string()], "only the real book");
}

#[test]
fn scan_tolerates_a_missing_root() {
    let s = Shelf::new("scan-missing-root");
    let plan = scan_with_mode(
        &s.engine,
        &s.filter(),
        &[s.dir.path().join("does-not-exist")],
        ScanMode::Scope,
    )
    .unwrap();
    assert!(plan.new.is_empty());
}

#[test]
fn scan_tolerates_permission_denied_subdirectory() {
    // Simulate an unreadable directory by pointing at a file as a root.
    let s = Shelf::new("scan-unreadable");
    s.write_file("a.epub", BODY);
    let file_root = s.dir.path().join("a.epub");
    let plan = s.scan().unwrap();
    let e = &plan.new[0];
    s.engine
        .put_book(1, &book(&e.path.to_string_lossy(), e.size, e.mtime_ms))
        .unwrap();

    // The scan over a bogus root must not panic or error.
    let plan2 = scan_with_mode(&s.engine, &s.filter(), &[file_root], ScanMode::Scope).unwrap();
    assert_eq!(plan2.new.len(), 0, "a file root yields no candidates");

    // Crucially, the indexed book must NOT be reported missing: an unreadable
    // root says nothing about its contents.
    assert!(
        plan2.missing.is_empty(),
        "an unreadable root must not orphan books"
    );
}

#[test]
fn scan_reports_missing_only_when_a_root_is_readable() {
    let s = Shelf::new("scan-missing-root-guard");
    s.write_file("a.epub", BODY);
    let plan = s.scan().unwrap();
    let e = &plan.new[0];
    s.engine
        .put_book(1, &book(&e.path.to_string_lossy(), e.size, e.mtime_ms))
        .unwrap();

    // A readable root with the file deleted: the book is genuinely gone.
    fs::remove_file(s.dir.path().join("a.epub")).unwrap();
    let plan = s.scan().unwrap();
    assert_eq!(plan.missing.len(), 1, "deleted file is reported");
    assert_eq!(plan.missing[0].0, 1);
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

#[test]
fn config_defaults_are_sane() {
    let c = Config::default();
    assert_eq!(c.source_dirs, vec!["/mnt/us/documents".to_string()]);
    assert!(c.index_depth.is_full(), "full positional is the default");
    assert_eq!(c.prefix_mode, PrefixMode::Auto);
    assert!(c.index_only_when_charging);
    assert!(!c.meta_only_unparseable);
    assert!(!c.include_extensions.is_empty());
    assert!(!c.exclude_extensions.is_empty());
}

#[test]
fn config_roundtrips_through_database() {
    let s = Shelf::new("config-roundtrip");
    let c = Config {
        source_dirs: vec!["/mnt/us/books".into(), "/mnt/us/more".into()],
        include_extensions: {
            let mut e = Config::default().include_extensions;
            e.push("azw4".into());
            e
        },
        min_file_size: 1024,
        max_file_size: 999,
        index_depth: IndexDepth::BALANCED,
        prefix_mode: PrefixMode::Prefix,
        ..Config::default()
    };
    s.engine.set_config(&c).unwrap();
    assert_eq!(s.engine.config().unwrap(), c);
}

#[test]
fn config_absent_returns_defaults() {
    let s = Shelf::new("config-default");
    assert_eq!(s.engine.config().unwrap(), Config::default());
}

#[test]
fn config_defaults_are_not_empty_or_duplicated() {
    for list in [DEFAULT_INCLUDE, DEFAULT_EXCLUDE, DEFAULT_EXCLUDE_DIRS] {
        assert!(!list.is_empty());
        let mut sorted = list.to_vec();
        sorted.sort_unstable();
        let before = sorted.len();
        sorted.dedup();
        assert_eq!(sorted.len(), before, "list {list:?} has duplicates");
    }
}

#[test]
fn include_and_exclude_do_not_overlap() {
    // A nautomatic format must not be both scanned and skipped.
    for ext in DEFAULT_INCLUDE {
        assert!(
            !DEFAULT_EXCLUDE.contains(ext),
            "extension {ext} is in both lists"
        );
    }
}

#[test]
fn exclusion_beats_inclusion() {
    let mut c = Config::default();
    c.include_extensions.push("azw4".into());
    c.exclude_extensions.push("azw4".into());
    assert!(!c.accepts_extension("azw4"));
    assert!(c.accepts_extension("epub"));
}

#[test]
fn extension_matching_is_case_insensitive() {
    let c = Config::default();
    assert!(c.accepts_extension("EPUB"));
    assert!(c.accepts_extension("Mobi"));
}

#[test]
fn junk_extensions_are_rejected() {
    let c = Config::default();
    for ext in [
        "log", "lua", "json", "css", "png", "pdf", "djvu", "cbz", "kfx", "tmp",
    ] {
        assert!(!c.accepts_extension(ext), "{ext} must be excluded");
    }
}

#[test]
// Assertions on consts are deliberate: they pin the contract so an
// accidental edit fails loudly instead of silently changing behaviour.
#[allow(clippy::assertions_on_constants)]
fn index_depth_presets_are_ordered_and_default_to_full() {
    assert!(IndexDepth::FULL.0 > IndexDepth::BALANCED.0);
    assert!(IndexDepth::BALANCED.0 > IndexDepth::LIGHT.0);
    assert!(
        IndexDepth::default().is_full(),
        "full positional is the default"
    );
    assert_eq!(IndexDepth::FULL.0, u32::MAX, "full means no cap at all");
}

#[test]
fn prefix_mode_roundtrips_all_variants() {
    let s = Shelf::new("prefix-modes");
    for (i, mode) in [PrefixMode::Auto, PrefixMode::Exact, PrefixMode::Prefix]
        .into_iter()
        .enumerate()
    {
        let c = Config {
            prefix_mode: mode,
            ..Config::default()
        };
        s.engine.set_config(&c).unwrap();
        assert_eq!(s.engine.config().unwrap().prefix_mode, mode, "case {i}");
    }
}

#[test]
fn content_hash_ignores_file_selection_settings() {
    let a = Config::default();
    let mut b = Config::default();
    // These only need a rescan, not a rebuild.
    b.include_extensions.push("azw4".into());
    b.exclude_extensions.push("nope".into());
    b.exclude_dir_globs.clear();
    b.source_dirs.clear();
    b.min_file_size = 1;
    b.max_file_size = 10;
    assert_eq!(
        a.content_hash(),
        b.content_hash(),
        "file-selection settings must not force a rebuild"
    );
}

#[test]
fn content_hash_tracks_token_stream_settings() {
    let a = Config::default();
    {
        let mut b = Config {
            index_depth: IndexDepth::BALANCED,
            ..Config::default()
        };
        b.min_file_size = a.min_file_size;
        assert_ne!(a.content_hash(), b.content_hash(), "depth changes tokens");
    }
}

// ---------------------------------------------------------------------------
// Durability
// ---------------------------------------------------------------------------

#[test]
fn data_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("persist.redb");
    {
        let e = Engine::open(&path).unwrap();
        e.put_book(1, &book("a.epub", 123, 456)).unwrap();
        e.set_config(&Config::default()).unwrap();
    }
    let e = Engine::open(&path).unwrap();
    assert_eq!(e.book(1).unwrap().unwrap().path, "a.epub");
    assert_eq!(e.config().unwrap(), Config::default());
}

#[test]
fn failed_write_rolls_back() {
    let s = Shelf::new("rollback");
    // A real rollback: write a valid row, then a statement that fails, inside one
    // transaction. The valid row must not survive, which is the property redb's
    // closure-based API provided and SQLite provides natively.
    let rolled = s.engine.block_on(async {
        let mut txn = s.engine.pool().begin().await?;
        sqlx::query("INSERT INTO meta (key, value) VALUES ('probe', ?1)")
            .bind(&b"kept"[..])
            .execute(&mut *txn)
            .await?;
        // A PRIMARY KEY violation: 'probe' is already the key of the row written
        // above, inside this same transaction. (An earlier version of this test
        // inserted 'meta', which collided with nothing, and passed vacuously.)
        let bad = sqlx::query("INSERT INTO meta (key, value) VALUES ('probe', ?1)")
            .bind(&b"never"[..])
            .execute(&mut *txn)
            .await;
        assert!(bad.is_err(), "the duplicate key must fail the statement");
        let _ = txn.rollback().await;
        Ok::<_, sqlx::Error>(())
    });
    assert!(rolled.is_ok());
    assert_eq!(
        s.engine.config().unwrap(),
        Config::default(),
        "no partial write"
    );
    // The probe row must not have survived the rollback either.
    let probe: Option<i64> = s
        .engine
        .block_on(async {
            sqlx::query_scalar("SELECT COUNT(*) FROM meta WHERE key = 'probe'")
                .fetch_one(s.engine.pool())
                .await
        })
        .ok();
    assert_eq!(probe, Some(0), "the transaction must have rolled back");
}

#[test]
fn file_size_grows_and_compaction_keeps_data() {
    let s = Shelf::new("size-compact");
    for i in 0..40u16 {
        s.engine
            .put_book(i, &book(&format!("b{i}.epub"), i as u64 + 1, i as i64))
            .unwrap();
    }
    assert!(s.engine.file_size() > 0);
    let _ = s.engine.compact();
    assert_eq!(
        s.engine.all_books().unwrap().len(),
        40,
        "compaction loses nothing"
    );
}

#[test]
fn open_creates_a_missing_parent_directory() {
    // `Engine::open` creates the parent, because a scratch index is often the
    // first thing written to a directory. The error case it does have to cover
    // is an unwritable path, which is asserted below.
    let dir = tempfile::tempdir().unwrap();
    let deep = dir.path().join("a").join("b").join("c");
    let e = Engine::open(deep.join("db.sqlite"));
    assert!(e.is_ok(), "a missing parent is created, not an error");

    // A path under a *file* cannot be created, and must fail cleanly rather than
    // panic on an unwritable path.
    let file = dir.path().join("not-a-dir");
    std::fs::write(&file, b"x").unwrap();
    let r = Engine::open(file.join("db.sqlite"));
    assert!(r.is_err(), "a path under a file must be an error");
}

// ---------------------------------------------------------------------------
// Cross-cutting guards against spec violations
// ---------------------------------------------------------------------------

#[test]
fn table_names_match_the_spec() {
    // These strings are the on-disk schema; renaming one orphans user data.
    use turbosearch::storage::*;
    let names = [
        TABLE_META,
        TABLE_BOOKS,
        TABLE_PATHS,
        TABLE_CONFIG,
        TABLE_METADATA,
        TABLE_METAIDX,
        TABLE_DICT,
        TABLE_DICT_DELTA,
        TABLE_POSTINGS,
        TABLE_ENTRYMAP,
    ];
    assert_eq!(names[0], "meta");
    assert_eq!(names[1], "books");
    assert_eq!(names[2], "paths");
    assert_eq!(names[3], "config");
    let mut all = names.to_vec();
    let before = all.len();
    all.sort_unstable();
    all.dedup();
    assert_eq!(all.len(), before, "table names must be unique");
}

#[test]
// Assertions on consts are deliberate: they pin the contract so an
// accidental edit fails loudly instead of silently changing behaviour.
#[allow(clippy::assertions_on_constants)]
fn engine_cache_size_stays_small() {
    // The floor matters as much as the ceiling: a cache too small to hold the
    // term dictionary turns every lookup into a page read.
    let s = Shelf::new("cache");
    // A negative `cache_size` is KiB, and -8000 is what `open` sets.
    let cache = s.engine.block_on(async {
        sqlx::query_scalar::<_, i64>("PRAGMA cache_size")
            .fetch_one(s.engine.pool())
            .await
            .unwrap_or(0)
    });
    assert_eq!(cache, -8000, "the cache bound must be the one set at open");
    // 8000 KiB = 7,8 MB, which is what the constant was sized for.
    assert!(8000 <= 16 * 1024, "too big for a PW3");
}

#[test]
fn scan_plan_counts_every_bucket() {
    let mut p = ScanPlan::default();
    assert_eq!(p.total(), 0);
    assert!(!p.needs_work());
    p.unchanged.push(std::path::PathBuf::from("a"));
    assert_eq!(p.total(), 1);
    assert!(!p.needs_work(), "unchanged files need no work");
    p.new.push(FileEntry {
        path: PathBuf::from("b"),
        scope: None,
        size: 0,
        mtime_ms: 0,
        inode: 0,
        format: Format::Epub,
    });
    assert!(p.needs_work());
}

// ---------------------------------------------------------------------------
// Scoped vs full scan
//
// The user browses into a subdirectory and types a query before any full scan
// has run. That path must index only the directory being browsed, and a later
// full scan must not undo it.
// ---------------------------------------------------------------------------

#[test]
fn scope_scan_indexes_only_the_requested_directory() {
    let s = Shelf::new("scope-only");
    s.write_file("Tokyo Ravens/01.epub", BODY);
    s.write_file("Tokyo Ravens/02.epub", BODY);
    s.write_file("KonoSuba/01.epub", BODY);
    s.write_file("loose.epub", BODY);

    let scope = s.dir.path().join("Tokyo Ravens");
    let plan = scan_scope(&s.engine, &s.filter(), &scope).unwrap();

    // Only the two books under the requested directory are proposed.
    assert_eq!(plan.new.len(), 2, "other directories must not be touched");
    assert!(plan.new.iter().all(|e| e.path.starts_with(&scope)));
    assert_eq!(plan.mode, ScanMode::Scope);
    // Nothing outside the scope may be reported missing.
    assert!(plan.missing.is_empty());
    assert_eq!(plan.scanned_roots, vec![scope.display().to_string()]);
}

#[test]
fn full_scan_covers_every_configured_root() {
    let s = Shelf::new("full-covers");
    s.write_file("A/1.epub", BODY);
    s.write_file("B/2.epub", BODY);

    let cfg = Config {
        source_dirs: vec![
            s.dir.path().join("A").display().to_string(),
            s.dir.path().join("B").display().to_string(),
        ],
        ..Config::default()
    };
    let plan = scan_full(&s.engine, &cfg).unwrap();

    assert_eq!(plan.new.len(), 2, "both roots are swept");
    assert_eq!(plan.mode, ScanMode::Full);
    assert_eq!(plan.scanned_roots.len(), 2);
}

#[test]
fn full_scan_after_scope_scan_does_not_delete_the_scoped_books() {
    // The dangerous ordering: index a folder that is NOT a configured root, then
    // run a full scan over a different root. The first batch must survive.
    let s = Shelf::new("full-after-scope");
    s.write_file("Books/A/1.epub", BODY);
    s.write_file("Books/A/2.epub", BODY);
    s.write_file("Books/B/1.epub", BODY);

    // Index only A.
    let scope = s.dir.path().join("Books/A");
    let plan = scan_scope(&s.engine, &s.filter(), &scope).unwrap();
    for (i, e) in plan.new.iter().enumerate() {
        s.engine
            .put_book(
                i as u16,
                &book(&e.path.to_string_lossy(), e.size, e.mtime_ms),
            )
            .unwrap();
    }

    // Now sweep a DIFFERENT root. B's book is new; A's books are untouched.
    let cfg = Config {
        source_dirs: vec![s.dir.path().join("Books/B").display().to_string()],
        ..Config::default()
    };
    let plan = scan_full(&s.engine, &cfg).unwrap();

    assert_eq!(plan.new.len(), 1, "B is discovered");
    assert!(plan.new[0].path.ends_with("B/1.epub"));
    assert!(
        plan.missing.is_empty(),
        "books outside the swept root must not be orphaned: {:?}",
        plan.missing
    );
    assert_eq!(s.engine.all_books().unwrap().len(), 2, "A's books survive");
}

#[test]
fn deleting_a_book_inside_the_scanned_root_is_reported() {
    let s = Shelf::new("scope-delete");
    s.write_file("scope/a.epub", BODY);
    s.write_file("scope/b.epub", BODY);
    s.write_file("outside/c.epub", BODY);

    let scope = s.dir.path().join("scope");
    let plan = scan_scope(&s.engine, &s.filter(), &scope).unwrap();
    for (i, e) in plan.new.iter().enumerate() {
        s.engine
            .put_book(
                i as u16,
                &book(&e.path.to_string_lossy(), e.size, e.mtime_ms),
            )
            .unwrap();
    }
    fs::remove_file(s.dir.path().join("scope/a.epub")).unwrap();

    let plan = scan_scope(&s.engine, &s.filter(), &scope).unwrap();
    assert_eq!(plan.missing.len(), 1, "the file inside the scope is gone");
    assert_eq!(plan.unchanged.len(), 1, "the sibling is untouched");
}

#[test]
fn scope_search_restricts_results_to_the_current_directory() {
    let f = ScopeFilter::new("/mnt/us/documents/Tokyo Ravens");
    assert!(f.accepts("/mnt/us/documents/Tokyo Ravens/01.epub"));
    assert!(f.accepts("/mnt/us/documents/Tokyo Ravens/nested/09.epub"));
    // A sibling directory that merely shares a prefix must not leak in.
    assert!(!f.accepts("/mnt/us/documents/Tokyo Ravens Extra/01.epub"));
    assert!(!f.accepts("/mnt/us/documents/KonoSuba/01.epub"));

    assert!(ScopeFilter::all().accepts("/anything/at/all.epub"));
}

#[test]
fn path_in_scope_handles_boundaries() {
    assert!(path_in_scope("/a/b/c.epub", "/a/b"));
    assert!(path_in_scope("/a/b", "/a/b"), "the scope itself counts");
    assert!(!path_in_scope("/a/bc/d.epub", "/a/b"));
    assert!(!path_in_scope("/a", "/a/b"));
    assert!(
        path_in_scope("/anything", ""),
        "an empty scope means no restriction"
    );
}

#[test]
fn scope_search_is_cheap_relative_to_a_full_scan() {
    // The reason scoping exists: a subdirectory holds a small fraction of the
    // library, so the first search indexes a handful of books instead of all.
    let s = Shelf::new("scope-cost");
    for i in 0..40 {
        s.write_file(&format!("Big/f{i}.epub"), BODY);
    }
    s.write_file("Big/Small/target.epub", BODY);

    let small = s.dir.path().join("Big/Small");
    let plan = scan_scope(&s.engine, &s.filter(), &small).unwrap();

    // 1 book indexed instead of 41.
    assert_eq!(plan.new.len(), 1);
    assert_eq!(
        plan.new.len() * 100
            / scan_with_mode(
                &s.engine,
                &s.filter(),
                &[s.dir.path().to_path_buf()],
                ScanMode::Full
            )
            .unwrap()
            .new
            .len(),
        2,
        "the scoped pass touches ~2% of the library"
    );
}
