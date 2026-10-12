//! HTML and XHTML are case-insensitive: `<HtMl>`, `<SCRIPT>` and
//! `<!DOCTYPE HTML>` are all legal and all appear in real books.

use std::io::Write;
use tempfile::tempdir;

fn file_with(tag: &str, bytes: &[u8]) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempdir().unwrap();
    let path = dir.path().join(tag);
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(bytes).unwrap();
    (dir, path)
}

#[test]
fn html_detection_is_case_insensitive() {
    for src in [
        &b"<HtMl><body>x</body></HtMl>"[..],
        b"<HTML><body>x</body></HTML>",
        b"<html><body>x</body></html>",
        b"<HtMl><body>x</body></html>",
    ] {
        let (_d, p) = file_with("a.html", src);
        assert_eq!(
            turbosearch::format::detect(&p),
            turbosearch::storage::book::Format::Html,
            "source {:?}",
            String::from_utf8_lossy(src)
        );
    }
}

#[test]
fn doctype_detection_is_case_insensitive() {
    for src in [
        &b"<!DOCTYPE html><html/>"[..],
        b"<!doctype html><html/>",
        b"<!Doctype Html><html/>",
        b"<!DOCTYPE HTML><html/>",
        b"<!DOCTYPE HTML PUBLIC \"-//W3C//DTD XHTML 1.0//EN\"><html/>",
    ] {
        let (_d, p) = file_with("a.html", src);
        assert_eq!(turbosearch::format::detect(&p), turbosearch::storage::book::Format::Html);
    }
}

#[test]
fn script_tags_are_dropped_whatever_their_case() {
    for markup in [
        "<script>var leak = 1;</script><p>keep</p>",
        "<SCRIPT>var leak = 1;</SCRIPT><p>keep</p>",
        "<Script>var leak = 1;</Script><p>keep</p>",
        "<ScRiPt>var leak = 1;</ScRiPt><p>keep</p>",
        "<script type=\"text/javascript\">var leak = 1;</script><p>keep</p>",
    ] {
        let text = turbosearch::format::html::extract_text(markup.as_bytes()).unwrap();
        let text = String::from_utf8(text).unwrap();
        assert!(text.contains("keep"), "case {markup:?} -> {text:?}");
        assert!(!text.contains("leak"), "script body leaked for {markup:?}: {text:?}");
    }
}

#[test]
fn style_and_head_are_dropped_whatever_their_case() {
    for markup in [
        "<STYLE>.a{color:red}</STYLE><p>keep</p>",
        "<Style>.a{color:red}</Style><p>keep</p>",
        "<HEAD><TITLE>dropped</TITLE></HEAD><BODY><p>keep</p></BODY>",
        "<Head><title>dropped</title></Head><body><p>keep</p></body>",
    ] {
        let text = turbosearch::format::html::extract_text(markup.as_bytes()).unwrap();
        let text = String::from_utf8(text).unwrap();
        assert!(text.contains("keep"), "{markup:?} -> {text:?}");
        assert!(!text.contains("dropped"), "head content leaked for {markup:?}: {text:?}");
        assert!(!text.contains("color"), "style content leaked for {markup:?}: {text:?}");
    }
}

#[test]
fn all_caps_body_text_is_extracted() {
    let text = turbosearch::format::html::extract_text(b"<BODY><P>SHERLOCK HOLMES</P></BODY>").unwrap();
    let text = String::from_utf8(text).unwrap();
    assert!(text.contains("SHERLOCK"), "{text}");
    assert!(!text.contains("BODY"), "tag names leaked: {text}");
}

#[test]
fn opf_element_names_are_matched_case_insensitively() {
    // Element names in OPF are XML (case-sensitive in principle), but books in
    // the wild do not all follow that, and a mismatch silently drops the spine.
    let opf = r#"<package><Metadata><Title>Tập 3</Title>
<Creator>Natsume</Creator><Language>vi</Language></Metadata>
<Manifest><Item id="a" href="a.xhtml" media-type="application/xhtml+xml"/></Manifest>
<Spine><ItemRef idref="a"/></Spine></package>"#;

    let pkg = turbosearch::format::epub::parse_package(opf.as_bytes());
    assert_eq!(pkg.metadata.title, "Tập 3", "Title must match title");
    assert_eq!(pkg.metadata.authors, "Natsume");
    assert_eq!(pkg.metadata.language, "vi");
    assert_eq!(pkg.spine, vec!["a"], "ItemRef must match itemref");
}

#[test]
fn media_types_are_matched_case_insensitively() {
    // `Application/XHTML+XML` appears in real files.
    let opf = br#"<package><manifest>
<item id="a" href="a.xhtml" media-type="Application/XHTML+XML"/>
</manifest><spine><itemref idref="a"/></spine></package>"#;
    let pkg = turbosearch::format::epub::parse_package(opf);
    assert!(turbosearch::format::epub::is_text_media_type(&pkg.manifest[0].media_type));
}

#[test]
fn href_case_mismatch_against_the_archive_entry_still_resolves() {
    // A manifest href whose case differs from the actual ZIP entry. EPUB does not
    // mandate a case, and zip entry names are case-sensitive, so a naive lookup
    // silently drops the section.
    let dir = tempdir().unwrap();
    let path = dir.path().join("cased.epub");
    {
        let file = std::fs::File::create(&path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let opts: zip::write::FileOptions<()> = Default::default();

        zip.start_file("mimetype", opts).unwrap();
        std::io::Write::write_all(&mut zip, b"application/epub+zip").unwrap();

        zip.start_file("META-INF/container.xml", opts).unwrap();
        std::io::Write::write_all(&mut zip, br#"<container><rootfiles>
<rootfile full-path="OEBPS/content.opf"/></rootfiles></container>"#)
            .unwrap();

        zip.start_file("OEBPS/content.opf", opts).unwrap();
        std::io::Write::write_all(&mut zip, br#"<package><metadata>
<dc:title>Case Test</dc:title></metadata>
<manifest><item id="a" href="ChapterOne.XHTML" media-type="application/xhtml+xml"/></manifest>
<spine><itemref idref="a"/></spine></package>"#)
            .unwrap();

        // The entry on disk is spelled differently from the href.
        zip.start_file("OEBPS/chapterone.xhtml", opts).unwrap();
        std::io::Write::write_all(&mut zip, b"<html><body><p>Sherlock Holmes in Hanoi</p></body></html>")
            .unwrap();

        zip.finish().unwrap();
    }

    let content = turbosearch::format::epub::open(&path).expect("open a case-mismatched epub");
    assert_eq!(content.metadata.title, "Case Test");
    assert_eq!(content.sections.len(), 1, "the section must be found despite case");
    let text = String::from_utf8_lossy(&content.sections[0].text);
    assert!(text.contains("Sherlock"), "section text missing: {text}");
}

#[test]
fn xml_declaration_is_case_insensitive() {
    // `<?XML` is technically invalid XML but appears in sloppy files.
    for src in [
        &b"<?xml version=\"1.0\"?><FictionBook xmlns=\"x\"/>"[..],
        b"<?XML version=\"1.0\"?><FictionBook xmlns=\"x\"/>",
        b"<?XmL version=\"1.0\"?><FictionBook xmlns=\"x\"/>",
    ] {
        let (_d, p) = file_with("a.fb2", src);
        assert_eq!(turbosearch::format::detect(&p), turbosearch::storage::book::Format::Fb2);
    }
}

#[test]
fn container_entry_is_found_whatever_its_case() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("cased-container.epub");
    {
        let file = std::fs::File::create(&path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let opts: zip::write::FileOptions<()> = Default::default();

        zip.start_file("META-INF/Container.XML", opts).unwrap();
        std::io::Write::write_all(
            &mut zip,
            br#"<container><rootfiles><rootfile full-path="content.opf"/></rootfiles></container>"#,
        )
        .unwrap();

        zip.start_file("content.opf", opts).unwrap();
        std::io::Write::write_all(
            &mut zip,
            br#"<package><metadata><dc:title>Container Case</dc:title></metadata>
<manifest><item id="a" href="c.xhtml"/></manifest>
<spine><itemref idref="a"/></spine></package>"#,
        )
        .unwrap();

        zip.start_file("c.xhtml", opts).unwrap();
        std::io::Write::write_all(&mut zip, b"<html><body><p>found it</p></body></html>").unwrap();
        zip.finish().unwrap();
    }

    let content = turbosearch::format::epub::open(&path).expect("open");
    assert_eq!(content.metadata.title, "Container Case");
    assert_eq!(content.sections.len(), 1, "META-INF/Container.XML must be honoured");
}

#[test]
fn opf_attribute_names_are_matched_case_insensitively() {
    let opf = br#"<package><metadata>
<dc:title>Attr Case</dc:title>
<dc:identifier OPF:SCHEME="isbn">978-3-16-148410-0</dc:identifier>
<meta NAME="calibre:series" CONTENT="Attributed"/>
</metadata>
<manifest><item ID="a" HREF="c.xhtml" MEDIA-TYPE="application/xhtml+xml"/></manifest>
<spine><itemref IDREF="a"/></spine></package>"#;

    let pkg = turbosearch::format::epub::parse_package(opf);
    assert_eq!(pkg.metadata.title, "Attr Case");
    assert_eq!(pkg.metadata.series, "Attributed", "meta NAME/CONTENT must match");
    assert!(pkg.metadata.identifiers.iter().any(|i| i.contains("978-3-16")));
    assert_eq!(pkg.spine, vec!["a"], "IDREF must match idref");
}

#[test]
fn lowercase_cdata_markers_are_handled() {
    let text = turbosearch::format::html::extract_text(b"<body><![cdata[inside]]><p>out</p></body>").unwrap();
    let text = String::from_utf8_lossy(&text);
    assert!(text.contains("inside"), "{text}");
    assert!(text.contains("out"), "{text}");
}

#[test]
fn hrefs_with_uppercase_extensions_are_content() {
    use turbosearch::format::epub::{is_content_extension, is_content_item};
    assert!(is_content_extension("ChapterOne.XHTML"));
    assert!(is_content_extension("a.Html"));
    assert!(is_content_extension("a.HTM"));
    assert!(is_content_extension("a.xhtm"));
    assert!(!is_content_extension("a.css"));
    assert!(!is_content_extension("a.png"));
    // An extension decides on its own, media type ignored.
    assert!(is_content_item("a.XHTML", "garbage/type"));
    // No extension: the media type gets a say.
    assert!(is_content_item("text", "application/xhtml+xml"));
    assert!(is_content_item("text", "Application/XHTML+XML"));
    // No extension and no usable media type: not content, so images and CSS
    // never slip through.
    assert!(!is_content_item("cover", ""));
    assert!(!is_content_item("image1", "image/jpeg"));
}
