//! Container detection from file contents.
//!
//! The extension is only a cheap pre-filter: `Config.include_extensions` decides
//! whether a file is *worth reading* at all, and everything else is decided by
//! the bytes. This matters because real libraries name files carelessly — a
//! `.txt` that is really an EPUB, or `.azw`/`.mobi`/`.epub` used interchangeably
//! for the same book.
//!
//! Detection is deliberately ordered cheapest-first and never falls back to
//! "guess from the extension", so a mislabelled file is detected as what it is
//! rather than what it claims to be.

use std::fs::File;
use std::io::Read;
use std::path::Path;

use crate::storage::book::Format;

/// Bytes needed to classify every container below.
///
/// EPUB's signature is the `mimetype` ZIP entry stored uncompressed at the very
/// start of the file, which fits comfortably in this window.
const SNIFF_BYTES: usize = 1024;

/// Offset of the PalmDB type/creator magic inside a MOBI header.
const PDB_MAGIC_OFFSET: usize = 60;
const PDB_MAGIC: &[u8] = b"BOOKMOBI";

/// Detects which container `path` holds.
///
/// Returns [`Format::Unknown`] when the contents match nothing, which the caller
/// treats as "skip this file" rather than an error.
pub fn detect(path: &Path) -> Format {
    let mut head = [0u8; SNIFF_BYTES];
    let read = match File::open(path).and_then(|mut f| f.read(&mut head)) {
        Ok(n) => n,
        Err(_) => return Format::Unknown,
    };
    if read == 0 {
        return Format::Unknown;
    }
    let head = &head[..read];

    if is_zip(head) {
        return detect_zip_payload(head, path);
    }
    detect_bytes(head)
}

/// Classifies a leading byte slice, for testing and for callers that already
/// hold the buffer.
///
/// Cannot detect containers that need the ZIP central directory, so those come
/// back as [`Format::Unknown`]; use [`detect`] when a path is available.
pub fn detect_bytes(head: &[u8]) -> Format {
    if head.len() < 4 {
        return Format::Unknown;
    }

    if is_zip(head) {
        return detect_zip_payload(head, Path::new("/dev/null"));
    }
    if head.get(PDB_MAGIC_OFFSET..PDB_MAGIC_OFFSET + PDB_MAGIC.len()) == Some(PDB_MAGIC) {
        return Format::Mobi;
    }
    if head.starts_with(b"%PDF") {
        return Format::Pdf;
    }
    if head.starts_with(b"AT&TFORM") {
        return Format::Djvu;
    }
    if head.starts_with(b"{\\rtf") {
        return Format::Rtf;
    }
    if head.starts_with(b"\x1a\x45\xdf\xa3") {
        // Matroska: a KFX book is one of these wrapping an Ion object stream.
        return Format::Kfx;
    }

    let text = leading_text(head);
    if text.starts_with("<?xml") {
        return detect_xml(text);
    }
    if text.starts_with("<!DOCTYPE html") || text.starts_with("<!doctype html") {
        return Format::Html;
    }
    if text.starts_with("<html") || text.starts_with("<HTML") {
        return Format::Html;
    }
    if text.starts_with('<') {
        // A bare tag with no doctype: still almost certainly markup.
        return Format::Html;
    }
    if looks_like_text(head) {
        return Format::Txt;
    }
    Format::Unknown
}

/// EPUB's ZIP stores `mimetype` as the first, uncompressed entry, so the
/// signature is visible without parsing the archive. Not every writer honours
/// that, so the fallback opens the central directory, which lists every entry
/// name without decompressing anything.
fn detect_zip_payload(head: &[u8], path: &Path) -> Format {
    if find(head, b"mimetypeapplication/epub+zip").is_some() {
        return Format::Epub;
    }
    if let Ok(mut archive) = zip::ZipArchive::new(std::fs::File::open(path).unwrap_or_else(|_| {
        // `head` was already read from this path, so it opens.
        std::fs::File::open("/dev/null").expect("/dev/null opens")
    })) {
        for i in 0..archive.len() {
            if let Ok(entry) = archive.by_index_raw(i) {
                let name = entry.name();
                if name == "mimetype" {
                    return Format::Epub;
                }
                if name == "META-INF/container.xml" {
                    return Format::Epub;
                }
            }
        }
    }
    Format::Unknown
}

fn detect_xml(text: &str) -> Format {
    // FB2 declares its namespace on the root element, which is close enough to
    // the declaration to appear in the sniff window.
    if text.contains("FictionBook") {
        return Format::Fb2;
    }
    if text.contains("<html") || text.contains("<!DOCTYPE") {
        return Format::Html;
    }
    Format::Xhtml
}

fn is_zip(head: &[u8]) -> bool {
    head.starts_with(b"PK\x03\x04")
        || head.starts_with(b"PK\x05\x06")
        || head.starts_with(b"PK\x07\x08")
}

/// True when the bytes look like prose rather than binary noise.
///
/// The threshold is intentionally loose: a false positive here costs one wasted
/// parse, while a false negative drops a real book.
fn looks_like_text(head: &[u8]) -> bool {
    let sample = &head[..head.len().min(512)];
    if sample.is_empty() {
        return false;
    }
    let printable = sample
        .iter()
        .filter(|&&b| {
            b == b'\n' || b == b'\r' || b == b'\t' || (0x20..=0x7E).contains(&b) || b >= 0x80
        })
        .count();
    printable * 100 >= sample.len() * 90
}

/// The leading slice interpreted as text, for tag detection.
fn leading_text(head: &[u8]) -> &str {
    // Skip a UTF-8 BOM if present.
    let body = head.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(head);
    std::str::from_utf8(body).unwrap_or("")
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// True when this format carries text worth indexing.
///
/// Everything else still gets a `BOOKS` row and possibly a `METADATA` row, so it
/// shows up in metadata search — it just contributes no content postings.
pub fn is_text_format(format: Format) -> bool {
    matches!(
        format,
        Format::Epub
            | Format::Kepub
            | Format::Mobi
            | Format::Txt
            | Format::Markdown
            | Format::Html
            | Format::Xhtml
            | Format::Fb2
            | Format::Rtf
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::tempdir;

    fn write_fixture(name: &str, bytes: &[u8]) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempdir().unwrap();
        let path = dir.path().join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(bytes).unwrap();
        (dir, path)
    }

    /// Builds a real EPUB-shaped ZIP header: an uncompressed `mimetype` entry
    /// first, exactly as the spec requires.
    fn epub_fixture() -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"PK\x03\x04\x14\x00\x00\x00\x08\x00");
        out.extend_from_slice(b"mimetypeapplication/epub+zip");
        out.extend_from_slice(b"PK\x05\x06rest-of-archive");
        out
    }

    #[test]
    fn detects_epub_from_the_stored_mimetype_entry() {
        let (_d, p) = write_fixture("a.epub", &epub_fixture());
        assert_eq!(detect(&p), Format::Epub);
    }

    #[test]
    fn detects_mobi_from_the_pdb_magic() {
        let mut bytes = vec![0u8; 128];
        bytes[..8].copy_from_slice(b"BOOKNAME");
        bytes[60..68].copy_from_slice(b"BOOKMOBI");
        let (_d, p) = write_fixture("a.mobi", &bytes);
        assert_eq!(detect(&p), Format::Mobi);
    }

    #[test]
    fn pdb_magic_must_be_at_the_exact_offset() {
        // Present but misplaced: not a MOBI.
        let mut bytes = vec![0u8; 128];
        bytes[30..38].copy_from_slice(b"BOOKMOBI");
        let (_d, p) = write_fixture("b.mobi", &bytes);
        assert_eq!(detect(&p), Format::Unknown);
    }

    #[test]
    fn detects_pdf_djvu_rtf_and_kfx() {
        let cases: [(&[u8], Format); 4] = [
            (b"%PDF-1.7\n% junk", Format::Pdf),
            (b"AT&TFORM\x00\x00\x00\x00DJVU", Format::Djvu),
            (b"{\\rtf1\\ansi\\deff0}", Format::Rtf),
            (b"\x1aE\xdf\xa3kfx-bytes", Format::Kfx),
        ];
        for (bytes, expected) in cases {
            let (_d, p) = write_fixture("x.bin", bytes);
            assert_eq!(detect(&p), expected, "{bytes:?}");
        }
    }

    #[test]
    fn detects_fb2_from_the_xml_declaration() {
        let src = "<?xml version=\"1.0\" encoding=\"UTF-8\"?><FictionBook xmlns=\"http://www.gribuser.ru/xml/fictionbook/2.0\"><body/></FictionBook>";
        let (_d, p) = write_fixture("a.fb2", src.as_bytes());
        assert_eq!(detect(&p), Format::Fb2);
    }

    #[test]
    fn detects_html_with_and_without_doctype() {
        let sources = [
            "<!DOCTYPE html><html><body>x</body></html>",
            "<html><body>x</body></html>",
            "<HTML><BODY>x</BODY></HTML>",
            "<div>just a fragment</div>",
        ];
        for src in sources {
            let (_d, p) = write_fixture("a.html", src.as_bytes());
            assert_eq!(detect(&p), Format::Html, "{src}");
        }
    }

    #[test]
    fn detects_plain_text() {
        let (_d, p) = write_fixture("a.txt", b"Just an ordinary novel, chapter one.\n");
        assert_eq!(detect(&p), Format::Txt);
    }

    #[test]
    fn binary_noise_is_unknown_not_text() {
        let mut bytes = vec![0u8; 512];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = (i * 7 + 3) as u8;
        }
        let (_d, p) = write_fixture("noise.bin", &bytes);
        assert_eq!(detect(&p), Format::Unknown);
    }

    #[test]
    fn missing_file_is_unknown_not_a_panic() {
        let dir = tempdir().unwrap();
        assert_eq!(detect(&dir.path().join("nope.epub")), Format::Unknown);
    }

    #[test]
    fn empty_file_is_unknown() {
        let (_d, p) = write_fixture("empty.epub", b"");
        assert_eq!(detect(&p), Format::Unknown);
    }

    #[test]
    fn a_tiny_file_does_not_panic() {
        for n in 0..8usize {
            let bytes = vec![b'x'; n];
            let (_d, p) = write_fixture("tiny", &bytes);
            let _ = detect(&p);
        }
    }

    #[test]
    fn a_utf8_bom_is_skipped_before_looking_for_tags() {
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(b"<html><body>x</body></html>");
        let (_d, p) = write_fixture("bom.html", &bytes);
        assert_eq!(detect(&p), Format::Html);
    }

    #[test]
    fn a_mislabelled_book_is_detected_by_contents() {
        // The whole point of sniffing: the extension lies, the bytes do not.
        let (_d, p) = write_fixture("actually-an-epub.txt", &epub_fixture());
        assert_eq!(detect(&p), Format::Epub, "contents must win over the name");
    }

    #[test]
    fn an_epub_whose_mimetype_is_reordered_still_detects() {
        // Real ZIP with the mimetype entry placed last, which several writers
        // do. Detection must open the central directory rather than trusting
        // the entry order.
        let dir = tempdir().unwrap();
        let path = dir.path().join("c.epub");
        {
            let file = std::fs::File::create(&path).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            let opts: zip::write::FileOptions<()> = Default::default();
            zip.start_file("0-mo-dau.xhtml", opts).unwrap();
            std::io::Write::write_all(&mut zip, b"<html><body>x</body></html>").unwrap();
            zip.start_file("mimetype", opts).unwrap();
            std::io::Write::write_all(&mut zip, b"application/epub+zip").unwrap();
            zip.finish().unwrap();
        }
        assert_eq!(detect(&path), Format::Epub);
    }

    #[test]
    fn text_formats_are_the_ones_with_content_postings() {
        assert!(is_text_format(Format::Epub));
        assert!(is_text_format(Format::Fb2));
        assert!(is_text_format(Format::Mobi));
        assert!(is_text_format(Format::Txt));
        // These get metadata rows only.
        assert!(!is_text_format(Format::Pdf));
        assert!(!is_text_format(Format::Djvu));
        assert!(!is_text_format(Format::Unknown));
    }
}
