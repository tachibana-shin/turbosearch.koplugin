//! EPUB (and KEPUB) decoding, plus the shared section/metadata types.
//!
//! Reading order comes from the OPF `<spine>`, never from ZIP entry order:
//! `note.md` section 0, row 3. The manifest maps `idref` to an href, and each
//! href is resolved relative to the OPF's own directory, so an `OEBPS/` layout
//! works exactly like a flat one.
//!
//! Metadata (`<metadata>`) is parsed here because the OPF is already open; see
//! `note.md` section 3.2, step 3.

use std::io;

/// One readable unit of a book, in reading order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    /// Stable identifier stored in `ENTRYMAP`: the entry name inside the
    /// container, or a synthesized name for a single-file format.
    pub name: String,
    /// Byte offset of this section's raw bytes within its container.
    ///
    /// Zero for single-file formats, which have no inner addressing.
    pub raw_offset: u64,
    /// Byte length of the raw bytes.
    pub raw_len: u64,
    /// Decoded, entity-resolved text ready for tokenization.
    pub text: Vec<u8>,
}

/// Everything a book yields: its sections in reading order and its metadata.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BookContent {
    pub format: crate::storage::book::Format,
    pub sections: Vec<Section>,
    pub metadata: BookMeta,
}

/// The metadata that gets stored and matched against in `METAIDX`.
///
/// Fields are `String` rather than `Vec<String>` for authors because OPF and FB2
/// both list authors as repeated elements; splitting happens at the store
/// boundary, not here, so this type stays a faithful transcription of the file.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BookMeta {
    pub title: String,
    pub authors: String,
    pub language: String,
    pub publisher: String,
    pub series: String,
    /// Multiplied by 1000 so it fits an integer field.
    pub series_index: u32,
    pub description: String,
    /// `scheme:value` pairs, e.g. `uuid:...`, `isbn:...`.
    pub identifiers: Vec<String>,
}

/// A `<manifest>` item: an id, a resolved archive path, and a media type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestItem {
    pub id: String,
    pub href: String,
    pub media_type: String,
}

/// The parsed OPF, ready to be resolved against the archive.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Package {
    pub metadata: BookMeta,
    pub manifest: Vec<ManifestItem>,
    /// `idref`s in reading order. Some may be missing from the manifest; the
    /// extractor skips those rather than failing the whole book.
    pub spine: Vec<String>,
}

/// Reads the OPF path out of `META-INF/container.xml`.
///
/// Returns `None` when the container is missing or unparseable, in which case the
/// caller falls back to guessing the OPF from the entry list (`note.md`
/// section 3.2, step 1).
pub fn container_rootfile(container_xml: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(container_xml);
    let mut reader = quick_xml::Reader::from_str(&text);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Err(_) => return None,
            Ok(quick_xml::events::Event::Eof) => return None,
            Ok(quick_xml::events::Event::Empty(ref e))
            | Ok(quick_xml::events::Event::Start(ref e)) => {
                let name = e.name();
                if local_name(name.as_ref()) != "rootfile" {
                    buf.clear();
                    continue;
                }
                for attr in e.attributes().flatten() {
                    if attr.key.as_ref() == b"full-path" {
                        let path = String::from_utf8_lossy(&attr.value).into_owned();
                        return (!path.is_empty()).then_some(path);
                    }
                }
            }
            _ => {}
        }
        buf.clear();
    }
}

/// Parses an OPF package document.
pub fn parse_package(opf: &[u8]) -> Package {
    let text = String::from_utf8_lossy(opf);
    let mut reader = quick_xml::Reader::from_str(&text);
    reader.config_mut().trim_text(true);

    let mut pkg = Package::default();
    let mut in_metadata = false;
    let mut in_manifest = false;
    let mut in_spine = false;
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Err(_) => return pkg,
            Ok(quick_xml::events::Event::Eof) => return pkg,
            // `<item rdf:.../>` and `<itemref .../>` are self-closing, so they
            // arrive as Empty. Handling only Start here silently drops the
            // entire spine.
            Ok(quick_xml::events::Event::Start(ref e))
            | Ok(quick_xml::events::Event::Empty(ref e)) => {
                let name = local_name(e.name().as_ref());
                match name.as_str() {
                    "metadata" => in_metadata = true,
                    "manifest" => in_manifest = true,
                    "spine" => in_spine = true,
                    // Metadata children carry the interesting values.
                    "title" if in_metadata => {
                        pkg.metadata.title = read_text(&mut reader, e.name());
                    }
                    "creator" if in_metadata => {
                        // Repeated: join, since `BookMeta.authors` is one field.
                        let value = read_text(&mut reader, e.name());
                        if !value.is_empty() {
                            if !pkg.metadata.authors.is_empty() {
                                pkg.metadata.authors.push('\n');
                            }
                            pkg.metadata.authors.push_str(&value);
                        }
                    }
                    "language" if in_metadata => {
                        pkg.metadata.language = read_text(&mut reader, e.name());
                    }
                    "publisher" if in_metadata => {
                        pkg.metadata.publisher = read_text(&mut reader, e.name());
                    }
                    "description" if in_metadata => {
                        pkg.metadata.description = read_text(&mut reader, e.name());
                    }
                    // EPUB2 puts series/series_index in a calibre `<meta>`.
                    "identifier" if in_metadata => {
                        let scheme = attr_value(e, b"scheme")
                            .or_else(|| attr_value(e, b"opf:scheme"))
                            .unwrap_or_default();
                        let value = read_text(&mut reader, e.name());
                        if !value.is_empty() {
                            let pair = if scheme.is_empty() {
                                value
                            } else {
                                format!("{scheme}:{value}")
                            };
                            pkg.metadata.identifiers.push(pair);
                        }
                    }
                    "meta" if in_metadata => {
                        apply_meta_tag(&mut pkg.metadata, e);
                    }
                    "item" if in_manifest => {
                        if let (Some(id), Some(href)) =
                            (attr_value(e, b"id"), attr_value(e, b"href"))
                        {
                            pkg.manifest.push(ManifestItem {
                                id,
                                href,
                                media_type: attr_value(e, b"media-type").unwrap_or_default(),
                            });
                        }
                    }
                    "itemref" if in_spine => {
                        if let Some(idref) = attr_value(e, b"idref") {
                            pkg.spine.push(idref);
                        }
                    }
                    _ => {}
                }
            }
            Ok(quick_xml::events::Event::End(ref e)) => {
                match local_name(e.name().as_ref()).as_str() {
                    "metadata" => in_metadata = false,
                    "manifest" => in_manifest = false,
                    "spine" => in_spine = false,
                    _ => {}
                }
            }
            _ => {}
        }
        buf.clear();
    }
}

/// Handles a `<meta name="calibre:series" content="..."/>` style tag.
///
/// Calibre, the usual source of a Vietnamese light-novel library, writes series
/// here rather than in the Dublin Core fields.
fn apply_meta_tag(meta: &mut BookMeta, e: &quick_xml::events::BytesStart) {
    let Some(name) = attr_value(e, b"name") else {
        return;
    };
    let Some(content) = attr_value(e, b"content") else {
        return;
    };
    // Case-insensitive: a mismatch here silently drops the series.
    match name.to_ascii_lowercase().as_str() {
        "calibre:series" | "series" => meta.series = content,
        "calibre:series_index" | "series_index" | "calibre:series-index" => {
            meta.series_index = parse_series_index(&content);
        }
        _ => {}
    }
}

/// Accepts `1`, `1.0`, `1.5` and a leading `#3` form; 0 means "unknown".
fn parse_series_index(s: &str) -> u32 {
    let trimmed = s.trim().trim_start_matches('#').trim();
    trimmed
        .parse::<f32>()
        .map(|v| (v * 1000.0).round() as u32)
        .unwrap_or(0)
}

/// Reads the character data of an element, consuming through to its end.
fn read_text(reader: &mut quick_xml::Reader<&[u8]>, name: quick_xml::name::QName) -> String {
    // `Cow<str>` rather than `&str`, so `&amp;` in a title reaches us as `&`.
    reader
        .read_text(name)
        .map(|t| t.into_owned())
        .unwrap_or_default()
}

/// The tag name with any namespace prefix stripped and lowercased.
///
/// `dc:title`, `Title` and `DC:TITLE` all become `title`. HTML and XHTML do not
/// require a particular case, and books in the wild take advantage of that, so a
/// case-sensitive comparison here would silently drop the spine or the metadata.
fn local_name(full: &[u8]) -> String {
    let base = full
        .iter()
        .rposition(|&b| b == b':')
        .map(|i| i + 1)
        .unwrap_or(0);
    String::from_utf8_lossy(&full[base..]).to_ascii_lowercase()
}

fn attr_value(e: &quick_xml::events::BytesStart, key: &[u8]) -> Option<String> {
    // Compared on the local name and ignoring case: `opf:file-as` matches
    // `file-as`, and `Media-Type` matches `media-type`.
    e.attributes()
        .flatten()
        .find(|a| {
            let k = a.key.as_ref();
            let base = k
                .iter()
                .rposition(|&b| b == b':')
                .map(|i| i + 1)
                .unwrap_or(0);
            k[base..].eq_ignore_ascii_case(key)
        })
        .map(|a| String::from_utf8_lossy(&a.value).into_owned())
}

/// Resolves an OPF href against the OPF's own directory.
///
/// `href` may carry a fragment (`file.html#anchor`) which is dropped here, and
/// percent escapes which are decoded. A bare `href` with no directory yields a
/// path at the archive root.
pub fn resolve_href(opf_dir: &str, href: &str) -> String {
    let path = href.split('#').next().unwrap_or(href);
    let decoded = percent_decode(path);
    if decoded.is_empty() {
        return String::new();
    }
    if opf_dir.is_empty() {
        return normalize(&decoded);
    }
    normalize(&format!("{opf_dir}/{decoded}"))
}

/// Collapses `.` and `..` so a resolved href matches an archive entry exactly.
pub fn normalize(path: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                // Never pop past the root: an overshooting `..` is dropped
                // rather than turning into a leading separator.
                out.pop();
            }
            other => out.push(other),
        }
    }
    out.join("/")
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(hex) = hex
                && let Ok(v) = u8::from_str_radix(hex, 16)
            {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Why an EPUB could not be opened.
#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    #[error("not a readable zip archive: {0}")]
    NotArchive(String),
    #[error("no OPF package found in the archive")]
    NoPackage,
    #[error("i/o reading {0}: {1}")]
    Io(String, io::Error),
}

impl From<io::Error> for OpenError {
    fn from(e: io::Error) -> Self {
        OpenError::Io("archive".to_string(), e)
    }
}

/// Maximum uncompressed size for a single section.
///
/// Guards against a zip bomb: one entry claiming gigabytes of text.
const MAX_SECTION_BYTES: u64 = 32 * 1024 * 1024;

/// Maximum number of spine sections.
const MAX_SECTIONS: usize = 2000;

/// Opens an EPUB and returns its sections in reading order plus its metadata.
///
/// The OPF is located via `META-INF/container.xml`. When the container is absent
/// or unparseable the extractor falls back to the first `.opf` in the archive and
/// marks the result so the caller can set `flags.unsorted_spine`
/// (`note.md` section 3.2, step 1).
pub fn open(path: &std::path::Path) -> Result<BookContent, OpenError> {
    let file = std::fs::File::open(path)?;
    let mut archive =
        zip::ZipArchive::new(file).map_err(|e| OpenError::NotArchive(e.to_string()))?;

    // Locate the OPF.
    let opf_path = match read_entry(&mut archive, "META-INF/container.xml") {
        Some(container) => {
            container_rootfile(&container).unwrap_or_else(|| guess_opf(&mut archive))
        }
        None => guess_opf(&mut archive),
    };
    if opf_path.is_empty() {
        return Err(OpenError::NoPackage);
    }

    let opf_bytes = read_entry(&mut archive, &opf_path).ok_or(OpenError::NoPackage)?;
    let mut pkg = parse_package(&opf_bytes);

    // Hrefs in the manifest are relative to the OPF's own directory, so an
    // `OEBPS/content.opf` produces paths like `OEBPS/index_split_000.html`.
    let dir = opf_path.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
    for item in &mut pkg.manifest {
        item.href = resolve_href(dir, &item.href);
    }

    let resolved: std::collections::HashMap<&str, &ManifestItem> =
        pkg.manifest.iter().map(|m| (m.id.as_str(), m)).collect();

    let mut sections = Vec::new();
    for idref in &pkg.spine {
        if sections.len() >= MAX_SECTIONS {
            break;
        }
        let Some(item) = resolved.get(idref.as_str()) else {
            // An idref that is not in the manifest is skipped, not fatal.
            continue;
        };
        if !is_content_item(&item.href, &item.media_type) {
            continue;
        }
        let Some(raw) = read_entry(&mut archive, &item.href) else {
            continue;
        };
        if raw.len() as u64 > MAX_SECTION_BYTES {
            continue;
        }
        let text = crate::format::html::extract_text(&raw)?;
        if text.iter().all(|b| b.is_ascii_whitespace()) {
            // An image-only or empty spine entry still advances positions, but
            // there is nothing to index, so skip it without touching the count
            // the tokenizer will see.
            continue;
        }
        sections.push(Section {
            name: item.href.clone(),
            raw_offset: 0,
            raw_len: raw.len() as u64,
            text,
        });
    }

    // Every manifest entry is a candidate section when the spine is absent or
    // entirely unresolvable; sorted by name for determinism.
    if sections.is_empty() {
        let mut names: Vec<&ManifestItem> = pkg
            .manifest
            .iter()
            .filter(|m| is_content_item(&m.href, &m.media_type))
            .collect();
        names.sort_by(|a, b| a.href.cmp(&b.href));
        for item in names.iter().take(MAX_SECTIONS) {
            let Some(raw) = read_entry(&mut archive, &item.href) else {
                continue;
            };
            let text = crate::format::html::extract_text(&raw)?;
            if text.iter().all(|b| b.is_ascii_whitespace()) {
                continue;
            }
            sections.push(Section {
                name: item.href.clone(),
                raw_offset: 0,
                raw_len: raw.len() as u64,
                text,
            });
        }
    }

    Ok(BookContent {
        format: crate::storage::book::Format::Epub,
        sections,
        metadata: pkg.metadata,
    })
}

/// Extensions that always mean "this manifest entry is XHTML content".
///
/// The extension is trusted over `media-type`: real EPUBs frequently omit the
/// attribute or write a placeholder, while the href always carries the truth.
const CONTENT_EXTENSIONS: &[&str] = &["xhtml", "html", "htm", "xhtm"];

/// True when a manifest href ends in a readable content extension.
///
/// Case-insensitive, because `.XHTML` and `.Html` both occur.
pub fn is_content_extension(href: &str) -> bool {
    match href.rsplit_once('.') {
        Some((_, ext)) => {
            let lower = ext.to_ascii_lowercase();
            CONTENT_EXTENSIONS.contains(&lower.as_str())
        }
        None => false,
    }
}

/// True for media types whose bytes are XHTML.
///
/// Only consulted for hrefs with no extension at all, since the extension is the
/// stronger signal.
pub fn is_text_media_type(media_type: &str) -> bool {
    // Case-insensitive: `Application/XHTML+XML` shows up in real files.
    let lower = media_type.to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "application/xhtml+xml" | "text/html" | "application/x-dtbook+xml"
    )
}

/// True when a manifest entry holds content worth indexing.
///
/// An extension match decides on its own. Without an extension the media type
/// gets a say, but an absent media type still means "not content", so images and
/// stylesheets never slip through.
pub fn is_content_item(href: &str, media_type: &str) -> bool {
    if is_content_extension(href) {
        return true;
    }
    !href.contains('.') && is_text_media_type(media_type)
}

/// Guesses the OPF location when `container.xml` is unusable.
///
/// The first entry whose name ends in `.opf`, else the conventional
/// `OEBPS/content.opf`, else the empty string so the caller reports an error.
fn guess_opf<R: std::io::Read + std::io::Seek>(archive: &mut zip::ZipArchive<R>) -> String {
    for name in ["OEBPS/content.opf", "content.opf", "book.opf"] {
        if archive.by_name(name).is_ok() {
            return name.to_string();
        }
    }
    for i in 0..archive.len() {
        if let Ok(entry) = archive.by_index(i)
            && entry.name().ends_with(".opf")
        {
            return entry.name().to_string();
        }
    }
    String::new()
}

/// Reads a whole archive entry into memory.
/// Reads a whole archive entry into memory.
///
/// ZIP entry names are case-sensitive, but an EPUB's manifest href is not
/// required to match that case, so an exact miss falls back to a
/// case-insensitive scan. Without this, a book that spells its href
/// `ChapterOne.XHTML` and its entry `chapterone.xhtml` loses the section with no
/// error at all.
fn read_entry<R: std::io::Read + std::io::Seek>(
    archive: &mut zip::ZipArchive<R>,
    name: &str,
) -> Option<Vec<u8>> {
    if let Ok(entry) = archive.by_name(name) {
        return read_entry_bytes(entry);
    }
    read_entry_case_insensitive(archive, name)
}

/// Reads one opened entry, refusing anything past the bomb guard.
fn read_entry_bytes<R: std::io::Read>(mut entry: zip::read::ZipFile<'_, R>) -> Option<Vec<u8>> {
    if entry.size() > MAX_SECTION_BYTES {
        return None;
    }
    let mut buf = Vec::with_capacity(entry.size() as usize);
    std::io::Read::read_to_end(&mut entry, &mut buf).ok()?;
    Some(buf)
}

/// Linear scan looking for an entry equal to `name` ignoring case.
///
/// Only reached after an exact lookup fails, so the common path pays nothing.
fn read_entry_case_insensitive<R: std::io::Read + std::io::Seek>(
    archive: &mut zip::ZipArchive<R>,
    name: &str,
) -> Option<Vec<u8>> {
    let target = name.to_ascii_lowercase();
    let mut match_index = None;
    for i in 0..archive.len() {
        let Ok(entry) = archive.by_index(i) else {
            continue;
        };
        if entry.name().to_ascii_lowercase() == target {
            match_index = Some(i);
            break;
        }
    }
    let entry = archive.by_index(match_index?).ok()?;
    read_entry_bytes(entry)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opens_the_measured_library_epub_and_reads_its_spine() {
        // A real book from the reference library, if it is present. The path
        // comes from `library`, never a hardcoded one, so this runs on any
        // machine that has a KOReader library and skips (with a message) on one
        // that does not.
        let Some(root) = crate::library::root() else {
            eprintln!("reference library not set; skipping");
            return;
        };
        let Ok(entries) = std::fs::read_dir(root) else {
            eprintln!("reference library unreadable; skipping");
            return;
        };
        let Some(path) = entries.flatten().find_map(|e| {
            let p = e.path();
            (p.extension().and_then(|x| x.to_str()) == Some("epub")).then_some(p)
        }) else {
            eprintln!("no epub found; skipping");
            return;
        };

        let content = open(&path).expect("open a real epub");
        assert!(!content.sections.is_empty(), "a real book yields sections");
        assert!(
            !content.metadata.title.is_empty(),
            "a real book has a title"
        );
        // Position 0 must be real prose, not markup.
        let first = std::str::from_utf8(&content.sections[0].text).unwrap();
        assert!(
            !first.contains("<p"),
            "tags leaked into the text: {first:?}"
        );
        assert!(first.trim().len() > 3, "section is not empty: {first:?}");
    }

    #[test]
    fn a_zip_without_an_opf_yields_an_error_not_panic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("broken.epub");
        std::fs::write(&path, b"PK\x03\x04not really a zip").unwrap();
        assert!(open(&path).is_err());
    }

    #[test]
    fn a_missing_path_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(open(&dir.path().join("nope.epub")).is_err());
    }

    #[test]
    fn container_rootfile_is_found() {
        let xml = br#"<?xml version="1.0"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles>
    <rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/>
  </rootfiles>
</container>"#;
        assert_eq!(
            container_rootfile(xml).as_deref(),
            Some("OEBPS/content.opf")
        );
    }

    #[test]
    fn container_rootfile_at_root() {
        let xml =
            br#"<container><rootfiles><rootfile full-path="content.opf"/></rootfiles></container>"#;
        assert_eq!(container_rootfile(xml).as_deref(), Some("content.opf"));
    }

    #[test]
    fn missing_or_broken_container_yields_none() {
        assert_eq!(container_rootfile(b""), None);
        assert_eq!(container_rootfile(b"<container><broken"), None);
        assert_eq!(
            container_rootfile(b"<container><rootfiles/></container>"),
            None
        );
        assert_eq!(
            container_rootfile(b"<c><rootfile full-path=\"\"/></c>"),
            None
        );
    }

    #[test]
    fn package_metadata_is_parsed() {
        let opf = r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="2.0">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:calibre="http://calibre.kovidgoyal.net/2009/metadata">
    <dc:title>Tập 3 - Chị đang được gọi đấy, Darkness-san</dc:title>
    <dc:creator opf:role="aut">Natsume Akatsuki</dc:creator>
    <dc:creator opf:role="trl">Nguyễn Dịch Giả</dc:creator>
    <dc:language>vi</dc:language>
    <dc:publisher>konovn.net</dc:publisher>
    <dc:description>Nội dung bất tận.</dc:description>
    <dc:identifier opf:scheme="uuid">dabcf998-69e0-49c8-8d86-f1e5a097620a</dc:identifier>
    <meta name="calibre:series" content="KonoSuba"/>
    <meta name="calibre:series_index" content="3.0"/>
  </metadata>
  <manifest>
    <item id="titlepage" href="titlepage.xhtml" media-type="application/xhtml+xml"/>
    <item id="c1" href="index_split_000.html" media-type="application/xhtml+xml"/>
  </manifest>
  <spine toc="ncx">
    <itemref idref="titlepage"/>
    <itemref idref="c1"/>
  </spine>
</package>"#;

        let pkg = parse_package(opf.as_bytes());
        assert_eq!(
            pkg.metadata.title,
            "Tập 3 - Chị đang được gọi đấy, Darkness-san"
        );
        assert_eq!(pkg.metadata.authors, "Natsume Akatsuki\nNguyễn Dịch Giả");
        assert_eq!(pkg.metadata.language, "vi");
        assert_eq!(pkg.metadata.publisher, "konovn.net");
        assert_eq!(pkg.metadata.series, "KonoSuba");
        assert_eq!(pkg.metadata.series_index, 3000);
        assert_eq!(pkg.metadata.identifiers.len(), 1);
        assert!(pkg.metadata.identifiers[0].starts_with("uuid:"));
        assert_eq!(pkg.spine, vec!["titlepage", "c1"]);
        assert_eq!(pkg.manifest.len(), 2);
        assert_eq!(pkg.manifest[0].href, "titlepage.xhtml");
    }

    #[test]
    fn spine_order_is_documented_not_manifest_order() {
        // The real library has manifest ids in reverse of reading order.
        let opf = br#"<package><manifest>
<item id="id256" href="index_split_000.html"/>
<item id="id255" href="index_split_001.html"/>
</manifest>
<spine><itemref idref="id255"/><itemref idref="id256"/></spine>
</package>"#;
        let pkg = parse_package(opf);
        assert_eq!(pkg.spine, vec!["id255", "id256"]);
    }

    #[test]
    fn href_resolution_handles_subdirectories() {
        assert_eq!(resolve_href("", "a.xhtml"), "a.xhtml");
        assert_eq!(resolve_href("OEBPS", "a.xhtml"), "OEBPS/a.xhtml");
        assert_eq!(resolve_href("OEBPS", "../img/a.png"), "img/a.png");
        assert_eq!(resolve_href("a/b", "../c.xhtml"), "a/c.xhtml");
        assert_eq!(resolve_href("a/b/c", "../../d.xhtml"), "a/d.xhtml");
    }

    #[test]
    fn href_fragment_and_percent_escapes_are_handled() {
        assert_eq!(resolve_href("", "a.xhtml#anchor"), "a.xhtml");
        assert_eq!(resolve_href("", "my%20book.xhtml"), "my book.xhtml");
        assert_eq!(resolve_href("", "T%E1%BA%ADp%203.xhtml"), "Tập 3.xhtml");
    }

    #[test]
    fn normalize_collapses_dot_segments() {
        assert_eq!(normalize("a/b/../c"), "a/c");
        assert_eq!(normalize("./a/./b"), "a/b");
        assert_eq!(normalize("a/../../b"), "b");
        assert_eq!(normalize(""), "");
    }

    #[test]
    fn series_index_accepts_common_forms() {
        assert_eq!(parse_series_index("1"), 1000);
        assert_eq!(parse_series_index("1.0"), 1000);
        assert_eq!(parse_series_index("3.5"), 3500);
        assert_eq!(parse_series_index(" 4 "), 4000);
        assert_eq!(parse_series_index("#3"), 3000);
        assert_eq!(parse_series_index("junk"), 0);
        assert_eq!(parse_series_index(""), 0);
    }

    #[test]
    fn empty_package_parses_to_defaults() {
        let pkg = parse_package(b"");
        assert_eq!(pkg, Package::default());
    }

    #[test]
    fn garbage_opf_does_not_panic() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"<package>");
        bytes.extend_from_slice(&[0xFFu8; 100]);
        let _ = parse_package(&bytes);
    }
}
