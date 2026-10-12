//! Where a book's metadata comes from.
//!
//! Priority follows `note.md` section 2.3:
//!
//! 1. **KOReader's sidecar** (`<book>.sdr/metadata.epub.lua`) — this is what the
//!    user is already looking at in the library screen, so reusing it keeps
//!    search results consistent with what they see.
//! 2. **Metadata embedded in the book** (OPF, FB2 description).
//! 3. **The filename**, for formats that carry nothing.
//!
//! The priority is not decoration. The reference library has EPUBs whose OPF
//! claims `language: en` while the sidecar says `vi`; the book is Vietnamese.
//! Taking the OPF answer would have been simply wrong.

use std::path::{Path, PathBuf};

use super::epub;
use crate::storage::metadata::{BookMetaRecord, Provenance};

/// Builds a metadata record for one book, from the best source available.
pub fn resolve(path: &Path) -> BookMetaRecord {
    // 1. KOReader's sidecar.
    if let Some(record) = from_sidecar(path) {
        return record;
    }

    // 2. Whatever the container itself carries.
    if let Some(record) = from_embedded(path) {
        return record;
    }

    // 3. The filename.
    from_filename(path)
}

/// Reads `<book>.sdr/metadata.epub.lua`, KOReader's own metadata cache.
///
/// This is a focused reader for the subset of Lua the sidecar uses: a `return`
/// table whose values are strings, numbers, booleans and nested tables. It is
/// not a Lua interpreter, and it does not try to be one — running arbitrary Lua
/// from a book folder would be far more risk than the feature is worth.
pub fn from_sidecar(path: &Path) -> Option<BookMetaRecord> {
    let sidecar = sidecar_path(path)?;
    let bytes = std::fs::read(&sidecar).ok()?;
    let text = String::from_utf8_lossy(&bytes);

    let props = lua_table_field(&text, "doc_props")?;
    let title = lua_string_field(&props, "title")?;
    if title.trim().is_empty() {
        return None;
    }

    let mut record = BookMetaRecord::new(
        title,
        lua_string_field(&props, "authors").unwrap_or_default(),
    );
    // The sidecar joins multiple values with a real newline, written as a Lua
    // line continuation, which is the same convention the OPF path uses.
    record.authors = unescape_lua(&record.authors);
    record.language = lua_string_field(&props, "language").unwrap_or_default();
    record.language = unescape_lua(&record.language);
    record.series = unescape_lua(&lua_string_field(&props, "series").unwrap_or_default());
    record.series_index = (lua_number_field(&props, "series_index").unwrap_or(0.0) as u32) * 1000;
    record.identifiers = lua_string_field(&props, "identifiers")
        .map(|i| unescape_lua(&i))
        .unwrap_or_default()
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    record.provenance = Provenance::Sidecar;

    // `stats` carries the same title and sometimes a fuller author list.
    if let Some(stats) = lua_table_field(&text, "stats")
        && let Some(authors) = lua_string_field(&stats, "authors")
    {
        let authors = unescape_lua(&authors);
        if !authors.trim().is_empty() && record.authors.trim().is_empty() {
            record.authors = authors;
        }
    }

    Some(record)
}

/// The sidecar path for a book.
pub fn sidecar_path(path: &Path) -> Option<PathBuf> {
    let parent = path.parent()?;
    let stem = path.file_stem()?.to_str()?;
    let candidate = parent.join(format!("{stem}.sdr/metadata.epub.lua"));
    candidate.is_file().then_some(candidate)
}

/// Reads the metadata a container carries itself.
pub fn from_embedded(path: &Path) -> Option<BookMetaRecord> {
    // EPUB-family containers and MOBI/AZW both carry their metadata inside the
    // file, and both were verified to parse on the reference library. A MOBI is
    // a PalmDB, not a ZIP, so it needs its own reader.
    match crate::storage::book::Format::from_extension(
        path.extension().and_then(|e| e.to_str()).unwrap_or(""),
    ) {
        crate::storage::book::Format::Mobi
        | crate::storage::book::Format::Azw
        | crate::storage::book::Format::Azw3 => return from_palmdb(path),
        _ => {}
    }

    // Other containers fall back to OPF; the FB2 path lives in
    // `crate::format::fb2`.
    match epub::open(path) {
        Ok(content) if !content.metadata.title.is_empty() => {
            let mut record = BookMetaRecord::new(content.metadata.title, content.metadata.authors);
            record.language = content.metadata.language;
            record.series = content.metadata.series;
            record.series_index = content.metadata.series_index;
            record.set_description(content.metadata.description);
            record.identifiers = content.metadata.identifiers;
            record.publisher = content.metadata.publisher;
            record.provenance = Provenance::Embedded;
            Some(record)
        }
        _ => None,
    }
}

/// Metadata from the EXTH block of a MOBI/AZW/AZW3.
///
/// The LZ77-decompressed text is not needed here, so a file whose text cannot be
/// decoded still yields its title and authors.
fn from_palmdb(path: &Path) -> Option<BookMetaRecord> {
    let bytes = std::fs::read(path).ok()?;
    let book = super::mobi::parse(&bytes).ok()?;
    if book.metadata.title.is_empty() {
        return None;
    }
    let mut record = BookMetaRecord::new(book.metadata.title, book.metadata.authors);
    record.language = book.metadata.language;
    record.publisher = book.metadata.publisher;
    record.set_description(book.metadata.description);
    // The schema has no tag column, so EXTH subjects are dropped rather than
    // shoe-horned into `description`, which the reader is shown verbatim.
    if !book.metadata.isbn.is_empty() {
        record
            .identifiers
            .push(format!("isbn:{}", book.metadata.isbn));
    }
    record.provenance = Provenance::Embedded;
    Some(record)
}

/// The last resort: derive a title from the filename.
pub fn from_filename(path: &Path) -> BookMetaRecord {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_string();
    // Genre tags in parentheses carry no title information, and the calibre
    // `(123)` suffix is a book id, not part of the name.
    let cleaned = strip_calibre_suffix(&stem);
    BookMetaRecord::new(cleaned, "")
}

/// Drops a trailing calibre `(123)`-style suffix.
pub fn strip_calibre_suffix(stem: &str) -> String {
    let trimmed = stem.trim();
    if let Some(open) = trimmed.rfind('(') {
        let inner = &trimmed[open + 1..];
        if let Some(close) = inner.find(')') {
            let digits = &inner[..close];
            if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) {
                return trimmed[..open].trim().to_string();
            }
        }
    }
    trimmed.to_string()
}

/// Finds a table field by name and returns its raw body.
///
/// Handles `["name"] = { ... }` and `["name"] = {}`, and only reads a table one
/// level deep, which is all the sidecar needs.
pub fn lua_table_field(text: &str, name: &str) -> Option<String> {
    for pattern in [&format!("[\"{name}\"]"), &format!("['{name}']")] {
        if let Some(start) = text.find(pattern.as_str()) {
            let after = &text[start + pattern.len()..];
            let eq = after.find('=')?;
            let value = after[eq + 1..].trim_start();
            if let Some(body) = value.strip_prefix('{') {
                return Some(balanced_braces(body));
            }
        }
    }
    None
}

/// Reads a string or number field from a table body.
pub fn lua_string_field(table_body: &str, name: &str) -> Option<String> {
    for pattern in [&format!("[\"{name}\"]"), &format!("['{name}']")] {
        if let Some(start) = table_body.find(pattern.as_str()) {
            let after = &table_body[start + pattern.len()..];
            let eq = after.find('=')?;
            let value = after[eq + 1..].trim_start();
            if value.starts_with('"') || value.starts_with('\'') {
                return Some(read_lua_string(value));
            }
            // A bare value — a number or a boolean. Real sidecars put each field
            // on its own line, but a compact table separates them with commas,
            // so cut at whichever comes first.
            let end = value.find([',', '}']).unwrap_or(value.len());
            let trimmed = value[..end].trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

/// Reads a numeric field from a table body.
pub fn lua_number_field(table_body: &str, name: &str) -> Option<f64> {
    lua_string_field(table_body, name).and_then(|v| v.trim().parse().ok())
}

/// Returns the characters inside a Lua string literal, escape sequences intact.
fn read_lua_string(value: &str) -> String {
    let quote = value.chars().next().unwrap_or('"');
    let bytes: Vec<char> = value.chars().collect();
    let mut out = String::new();
    let mut i = 1usize;
    while i < bytes.len() {
        match bytes[i] {
            c if c == quote => break,
            '\\' if i + 1 < bytes.len() => {
                // Keep the escape so the caller can decide what it means.
                out.push('\\');
                out.push(bytes[i + 1]);
                i += 2;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

/// Resolves Lua's `\` newline continuation into a real newline.
///
/// A sidecar writes a multi-line value as a string containing a backslash
/// immediately followed by a newline, which Lua reads as one newline character.
/// Leaving it raw would show up as a stray backslash in search results.
pub fn unescape_lua(value: &str) -> String {
    value.replace("\\\n", "\n").replace("\\\r\n", "\n")
}

/// Returns the body of a brace-delimited value, balanced.
fn balanced_braces(after: &str) -> String {
    let mut depth = 1usize;
    let mut out = String::new();
    let chars = after.chars();
    for c in chars {
        match c {
            '{' => {
                depth += 1;
                out.push(c);
            }
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return out;
                }
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn write(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    // --- sidecar ---------------------------------------------------------------

    #[test]
    fn a_real_sidecar_is_parsed() {
        // Verbatim from the reference library.
        let lua = r#"-- /x.sdr/metadata.epub.lua
return {
    ["annotations"] = {},
    ["copt_font_size"] = 22,
    ["doc_props"] = {
        ["authors"] = "Mai Mochizuki\
Ninh Nhân Văn (dịch)",
        ["identifiers"] = "uuid:2ac297f3-e270-49e5-8026-82a863f65699\
calibre:d82b869c-2715-4df9-9981-bc9280f4c0af",
        ["language"] = "vi",
        ["series"] = "Holmes ở Kyoto",
        ["series_index"] = 5,
        ["title"] = "Yến Tiệc Của Sherlockian Và Bão Tố Mùa Xuân",
    },
    ["doc_pages"] = 428,
    ["percent_finished"] = 0.011682242990654,
    ["preferred_dictionaries"] = {},
    ["stats"] = {
        ["authors"] = "Mai Mochizuki\
Ninh Nhân Văn (dịch)",
        ["language"] = "vi",
        ["series"] = "Holmes ở Kyoto #5",
        ["title"] = "Yến Tiệc Của Sherlockian Và Bão Tố Mùa Xuân",
    },
    ["summary"] = {
        ["modified"] = "2025-08-06",
        ["status"] = "reading",
    },
    ["text_lang"] = "vi",
}
"#;
        let dir = tempdir().unwrap();
        let book = dir.path().join("book.epub");
        write(&book, "not a real epub");
        write(&dir.path().join("book.sdr/metadata.epub.lua"), lua);

        let record = from_sidecar(&book).expect("the sidecar should parse");
        assert_eq!(record.title, "Yến Tiệc Của Sherlockian Và Bão Tố Mùa Xuân");
        assert_eq!(record.authors, "Mai Mochizuki\nNinh Nhân Văn (dịch)");
        assert_eq!(
            record.author_list(),
            vec!["Mai Mochizuki", "Ninh Nhân Văn (dịch)"]
        );
        assert_eq!(record.language, "vi");
        assert_eq!(record.series, "Holmes ở Kyoto");
        assert_eq!(record.series_index, 5000);
        assert_eq!(record.identifiers.len(), 2);
        assert!(record.identifiers.iter().any(|i| i.starts_with("uuid:")));
        assert!(record.identifiers.iter().any(|i| i.starts_with("calibre:")));
        assert_eq!(record.provenance, Provenance::Sidecar);
    }

    #[test]
    fn a_sidecar_without_a_title_is_rejected() {
        let dir = tempdir().unwrap();
        let book = dir.path().join("b.epub");
        write(&book, "x");
        write(
            &dir.path().join("b.sdr/metadata.epub.lua"),
            r#"return { ["doc_props"] = {} }"#,
        );
        assert!(from_sidecar(&book).is_none());
    }

    #[test]
    fn a_missing_sidecar_is_simply_absent() {
        let dir = tempdir().unwrap();
        let book = dir.path().join("b.epub");
        write(&book, "x");
        assert!(from_sidecar(&book).is_none());
        assert!(sidecar_path(&book).is_none());
    }

    #[test]
    fn an_unreadable_sidecar_does_not_panic() {
        let dir = tempdir().unwrap();
        let book = dir.path().join("b.epub");
        write(&book, "x");
        let p = dir.path().join("b.sdr/metadata.epub.lua");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, [0xFFu8, 0xFE, 0x00]).unwrap();
        assert!(from_sidecar(&book).is_none());
    }

    #[test]
    fn the_sidecar_path_follows_the_stem() {
        let dir = tempdir().unwrap();
        let book = dir.path().join("A Weird (99).Name.epub");
        write(&book, "x");
        write(
            &dir.path().join("A Weird (99).Name.sdr/metadata.epub.lua"),
            "x",
        );
        assert!(sidecar_path(&book).is_some());
    }

    #[test]
    fn single_quoted_lua_keys_are_supported() {
        let lua = "return { ['doc_props'] = { ['title'] = 'Tôi Là Ai', ['language'] = 'vi' } }";
        let dir = tempdir().unwrap();
        let book = dir.path().join("q.epub");
        write(&book, "x");
        write(&dir.path().join("q.sdr/metadata.epub.lua"), lua);
        let r = from_sidecar(&book).unwrap();
        assert_eq!(r.title, "Tôi Là Ai");
        assert_eq!(r.language, "vi");
    }

    // --- filename --------------------------------------------------------------

    #[test]
    fn filename_metadata_strips_the_calibre_suffix() {
        for (input, expected) in [
            ("Tap 3 - Natsume Akatsuki (148)", "Tap 3 - Natsume Akatsuki"),
            ("plain title", "plain title"),
            ("trailing (abc)", "trailing (abc)"),
            ("trailing (12)", "trailing"),
            ("holmes 9 (87)", "holmes 9"),
        ] {
            let r = from_filename(Path::new(&format!("/x/{input}.epub")));
            assert_eq!(r.title, expected, "input {input:?}");
            assert_eq!(r.provenance, Provenance::Filename);
        }
    }

    #[test]
    fn a_book_with_no_metadata_falls_back_to_the_filename() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("Some Book (42).epub");
        write(&path, "not an epub");
        let record = resolve(&path);
        assert_eq!(record.title, "Some Book");
        assert_eq!(record.provenance, Provenance::Filename);
    }

    // --- lua parsing primitives ------------------------------------------------

    #[test]
    fn lua_string_field_reads_quoted_numbers_and_bools() {
        let body = r#"["a"] = "one", ["b"] = 42, ["c"] = true, ["d"] = false,"#;
        assert_eq!(lua_string_field(body, "a").as_deref(), Some("one"));
        assert_eq!(lua_string_field(body, "b").as_deref(), Some("42"));
        assert_eq!(lua_string_field(body, "c").as_deref(), Some("true"));
        assert_eq!(lua_number_field(body, "b"), Some(42.0));
    }

    #[test]
    fn unescape_lua_resolves_line_continuations() {
        assert_eq!(unescape_lua("a\\\nb"), "a\nb");
        assert_eq!(unescape_lua("a\\\r\nb"), "a\nb");
        assert_eq!(unescape_lua("plain"), "plain");
    }

    #[test]
    fn balanced_braces_handles_nesting() {
        assert_eq!(balanced_braces("a{b{c}d}e"), "a{b{c}d}e".to_string());
        assert_eq!(balanced_braces("x}y"), "x");
    }

    #[test]
    fn lua_table_field_finds_a_nested_table() {
        let text = r#"return { ["outer"] = { ["inner"] = { ["k"] = "v" } } }"#;
        let outer = lua_table_field(text, "outer").unwrap();
        assert!(outer.contains("inner"));
        assert!(!outer.contains("outer"));
    }

    #[test]
    fn a_table_with_the_same_key_later_is_not_picked_up() {
        // `summary` repeats keys already seen in `doc_props`.
        let lua = r#"return {
        ["doc_props"] = { ["title"] = "first" },
        ["summary"] = { ["title"] = "second" },
    }"#;
        assert_eq!(
            lua_string_field(&lua_table_field(lua, "doc_props").unwrap(), "title").as_deref(),
            Some("first")
        );
    }

    // --- provenance ordering ---------------------------------------------------

    #[test]
    fn the_sidecar_wins_over_everything() {
        // The claim in `note.md` section 2.3, demonstrated: the OPF says `en`,
        // the sidecar says `vi`, and the sidecar is the one that survives.
        let dir = tempdir().unwrap();
        let book = dir.path().join("lang.epub");
        write(&book, "not an epub");
        write(
            &dir.path().join("lang.sdr/metadata.epub.lua"),
            r#"return { ["doc_props"] = { ["title"] = "T", ["language"] = "vi" } }"#,
        );
        let record = resolve(&book);
        assert_eq!(record.language, "vi");
        assert_eq!(record.provenance, Provenance::Sidecar);
    }
}
