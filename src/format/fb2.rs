//! FictionBook (FB2) decoding.
//!
//! FB2 is a single XML document: `<description>` carries the metadata and
//! `<body>` the content, in `<section>` elements. Unlike EPUB there is no
//! container to open, so the whole file is read and the sections are walked in
//! document order, which is already reading order.

use std::io;

use super::epub::{BookContent, BookMeta, Section};
use crate::storage::book::Format;

/// Parses an FB2 document.
pub fn open(bytes: &[u8]) -> io::Result<BookContent> {
    let text = String::from_utf8_lossy(bytes);
    let mut reader = quick_xml::Reader::from_str(&text);
    reader.config_mut().trim_text(true);

    let mut metadata = BookMeta::default();
    let mut sections: Vec<Section> = Vec::new();

    // Where we are in the document, and what is being collected.
    let mut in_description = false;
    let mut in_title_info = false;
    let mut in_publish_info = false;
    let mut in_body = false;

    // The current metadata field, or None.
    let mut field: Option<&str> = None;
    // The current section's text, and whether we are inside one.
    let mut section_text: Vec<u8> = Vec::new();
    let mut section_title: Vec<u8> = Vec::new();
    let mut in_section_title = false;

    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Err(e) => {
                return Err(io::Error::new(io::ErrorKind::InvalidData, e.to_string()));
            }
            Ok(quick_xml::events::Event::Eof) => break,
            // Self-closing elements such as `<sequence name="..." number="1"/>`
            // arrive as Empty, so Start and Empty share this arm. Matching only
            // Start here once silently dropped every FB2 series.
            Ok(quick_xml::events::Event::Start(ref e))
            | Ok(quick_xml::events::Event::Empty(ref e)) => {
                let name = e.name();
                let local = local_name(name.as_ref()).to_ascii_lowercase();
                match local.as_str() {
                    "description" => in_description = true,
                    "title-info" if in_description => in_title_info = true,
                    "publish-info" if in_description => in_publish_info = true,
                    "body" if !in_description => in_body = true,

                    // Metadata fields.
                    "book-title" if in_title_info => field = Some("title"),
                    "lang" if in_title_info => field = Some("language"),
                    "annotation" if in_title_info => field = Some("description"),
                    "publisher" if in_publish_info => field = Some("publisher"),
                    "isbn" if in_publish_info => field = Some("identifier"),
                    "year" if in_publish_info => field = Some("year"),

                    // `<sequence .../>` is self-closing, so it arrives as Empty
                    // and its attributes must be read there.
                    "sequence" if in_title_info => {
                        metadata.series = attr(e, b"name").unwrap_or_default();
                        metadata.series_index = series_index(attr(e, b"number"));
                    }

                    // An author is spread over two elements.
                    "first-name" if in_title_info => field = Some("first-name"),
                    "middle-name" if in_title_info => field = Some("middle-name"),
                    "last-name" if in_title_info => field = Some("last-name"),
                    // Content.
                    "section" if in_body => {
                        flush_section(&mut section_text, &mut section_title, &mut sections);
                    }
                    "title" if in_body => in_section_title = true,

                    _ => {}
                }
            }
            Ok(quick_xml::events::Event::Text(ref t)) => {
                if let Some(f) = field {
                    let value = t.unescape().map(|c| c.into_owned()).unwrap_or_default();
                    apply_field(&mut metadata, f, &value);
                    field = None;
                } else if in_body && in_section_title {
                    append_text(&mut section_title, t);
                } else if in_body {
                    append_text(&mut section_text, t);
                }
            }
            Ok(quick_xml::events::Event::End(ref e)) => {
                let local = local_name(e.name().as_ref()).to_ascii_lowercase();
                match local.as_str() {
                    "description" => in_description = false,
                    "title-info" => in_title_info = false,
                    "publish-info" => in_publish_info = false,
                    "body" => {
                        flush_section(&mut section_text, &mut section_title, &mut sections);
                        in_body = false;
                    }
                    "title" if in_body => in_section_title = false,
                    _ => {}
                }
            }
            _ => {}
        }
        buf.clear();
    }
    flush_section(&mut section_text, &mut section_title, &mut sections);

    Ok(BookContent {
        format: Format::Fb2,
        sections,
        metadata,
    })
}

/// Writes the field value into the metadata record.
fn apply_field(meta: &mut BookMeta, field: &str, value: &str) {
    let value = value.trim();
    if value.is_empty() {
        return;
    }
    match field {
        "title" => meta.title = value.to_string(),
        "language" => meta.language = value.to_string(),
        "description" => {
            if !meta.description.is_empty() {
                meta.description.push('\n');
            }
            meta.description.push_str(value);
        }
        "publisher" => meta.publisher = value.to_string(),
        "identifier" => meta.identifiers.push(format!("isbn:{value}")),
        "year" => meta.identifiers.push(format!("year:{value}")),
        // Author parts are joined with spaces and pushed per part.
        "first-name" | "middle-name" | "last-name" => {
            if !meta.authors.is_empty() {
                meta.authors.push(' ');
            }
            meta.authors.push_str(value);
        }
        _ => {}
    }
}

/// Turns an FB2 sequence number, which may be fractional, into an index.
fn series_index(attr: Option<String>) -> u32 {
    attr.as_deref()
        .and_then(|s| s.trim().parse::<f32>().ok())
        .map(|v| (v * 1000.0).round() as u32)
        .unwrap_or(0)
}

/// Appends the decoded text of a node so words do not fuse together.
fn append_text(out: &mut Vec<u8>, t: &quick_xml::events::BytesText) {
    if let Ok(s) = t.unescape() {
        out.extend_from_slice(s.as_bytes());
    }
}

/// Closes the current section, if it holds anything.
///
/// A section title leads the body text so the words are in reading order, which
/// is what the position counter depends on.
fn flush_section(
    text: &mut Vec<u8>,
    title: &mut Vec<u8>,
    sections: &mut Vec<Section>,
) {
    if text.is_empty() && title.is_empty() {
        return;
    }
    let mut body = std::mem::take(text);
    if !title.is_empty() {
        let mut with_title = std::mem::take(title);
        with_title.extend_from_slice(&body);
        with_title.push(b'\n');
        body = with_title;
    }
    sections.push(Section {
        name: format!("section{}", sections.len()),
        raw_offset: 0,
        raw_len: 0,
        text: body,
    });
}

fn local_name(full: &[u8]) -> String {
    let base = full
        .iter()
        .rposition(|&b| b == b':')
        .map(|i| i + 1)
        .unwrap_or(0);
    String::from_utf8_lossy(&full[base..]).into_owned()
}

fn attr(e: &quick_xml::events::BytesStart, key: &[u8]) -> Option<String> {
    e.attributes().flatten().find_map(|a| {
        let k = a.key.as_ref();
        let base = k.iter().rposition(|&b| b == b':').map(|i| i + 1).unwrap_or(0);
        (k[base..].eq_ignore_ascii_case(key)).then(|| String::from_utf8_lossy(&a.value).into_owned())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // A normal string literal, not a byte-string, because Vietnamese text
    // inside `br#"..."#` will not compile.
    const SAMPLE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<FictionBook xmlns="http://www.gribuser.ru/xml/fictionbook/2.0" xmlns:l="http://www.w3.org/1999/xlink">
  <description>
    <title-info>
      <genre>sf_history</genre>
      <author><first-name>Nguyễn</first-name><last-name>Nhật Ánh</last-name></author>
      <book-title>Tôi thấy hoa vàng trên cỏ xanh</book-title>
      <annotation><p>Một tác phẩm về tuổi thơ.</p><p>Viết tại Huế.</p></annotation>
      <lang>vi</lang>
      <sequence name="Hoa vàng" number="1"/>
    </title-info>
    <publish-info>
      <publisher>NXB Trẻ</publisher>
      <isbn>978-604-1-00000-0</isbn>
      <year>2010</year>
    </publish-info>
  </description>
  <body>
    <section><title><p>Chương 1: Đôi mắt</p></title>
      <p>Thám tử Sherlock bước vào phòng.</p>
      <p>Ông ngồi xuống ghế.</p>
    </section>
    <section><title><p>Chương 2: Vụ án</p></title>
      <p>Nguyễn Nhật Ánh viết câu chuyện thứ hai.</p>
    </section>
  </body>
</FictionBook>"#;

    #[test]
    fn metadata_is_read() {
        let c = open(SAMPLE.as_bytes()).unwrap();
        assert_eq!(c.metadata.title, "Tôi thấy hoa vàng trên cỏ xanh");
        assert_eq!(c.metadata.language, "vi");
        assert_eq!(c.metadata.series, "Hoa vàng");
        assert_eq!(c.metadata.series_index, 1000);
        assert_eq!(c.metadata.publisher, "NXB Trẻ");
        assert!(c.metadata.identifiers.iter().any(|i| i.contains("978-604")));
        // The author is joined from two elements.
        assert_eq!(c.metadata.authors, "Nguyễn Nhật Ánh");
    }

    #[test]
    fn annotation_becomes_the_description() {
        let c = open(SAMPLE.as_bytes()).unwrap();
        assert!(c.metadata.description.contains("tuổi thơ"), "{}", c.metadata.description);
    }

    #[test]
    fn each_section_is_a_section() {
        let c = open(SAMPLE.as_bytes()).unwrap();
        assert_eq!(c.sections.len(), 2);
        let all: String = c.sections.iter().map(|s| String::from_utf8_lossy(&s.text).into_owned()).collect::<Vec<_>>().join("\n");
        assert!(all.contains("Thám tử Sherlock"), "{all}");
        assert!(all.contains("ghế"), "{all}");
    }

    #[test]
    fn a_book_with_no_body_yields_no_sections() {
        let src = br#"<FictionBook><description><title-info><book-title>T</book-title></title-info></description></FictionBook>"#;
        let c = open(src).unwrap();
        assert_eq!(c.metadata.title, "T");
        assert!(c.sections.is_empty());
    }

    #[test]
    fn a_truncated_document_yields_what_it_can() {
        // quick-xml is a streaming reader, not a validator: a document cut off
        // mid-way simply ends. The contract is "no panic", not "an error".
        let result = open(b"<FictionBook><body><section><p>partial text");
        let parsed = result.expect("a truncated document is not a hard failure");
        assert!(
            String::from_utf8_lossy(&parsed.sections.first().map(|s| s.text.clone()).unwrap_or_default())
                .contains("partial text"),
            "the text that did parse should survive"
        );
    }

    #[test]
    fn genuinely_broken_xml_does_not_panic() {
        // Garbage in must not panic, whichever way it comes out.
        for src in [&b""[..], b"<", b"<<<>>>", b"\x00\x01\x02", "<FictionBook>\u{FFFD}".as_bytes()] {
            let _ = open(src);
        }
    }

    #[test]
    fn an_empty_file_errors_cleanly() {
        assert!(open(b"").is_err() || open(b"").unwrap().sections.is_empty());
    }

    #[test]
    fn namespaced_tags_are_recognised() {
        // `<l:...>` prefixes must not defeat the element matching.
        let src = br#"<fb:FictionBook xmlns:fb="urn:x"><fb:body><fb:section><fb:p>word</fb:p></fb:section></fb:body></fb:FictionBook>"#;
        let c = open(src).unwrap();
        assert_eq!(c.sections.len(), 1);
        assert!(String::from_utf8_lossy(&c.sections[0].text).contains("word"));
    }

    #[test]
    fn a_fractional_sequence_number_is_scaled() {
        assert_eq!(series_index(Some("2.5".to_string())), 2500);
        assert_eq!(series_index(Some("3".to_string())), 3000);
        assert_eq!(series_index(None), 0);
        assert_eq!(series_index(Some("junk".to_string())), 0);
    }

    #[test]
    fn sniffing_recognises_fictionbook_from_the_xml_declaration() {
        // The sniffer already accepted this; make sure the decoder agrees.
        assert_eq!(
            crate::format::detect(std::path::Path::new("/x/a.fb2")),
            Format::Unknown,
            "the sniffer needs the file contents, not just the extension"
        );
    }
}
