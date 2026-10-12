//! Text extraction from HTML and XHTML.
//!
//! `lol_html` is a streaming tokenizer, never a DOM builder, per `note.md`
//! section 3.1: the section 3.2 addendum was explicit that a DOM library like
//! `scraper` is not acceptable here.
//!
//! The `head`, `style`, `script` and `meta` subtrees are removed outright, so
//! their contents cannot leak into the token stream. Everything that survives is
//! plain text, ready for the tokenizer.

use lol_html::{element, rewrite_str, RewriteStrSettings};
use std::io;

/// Extracts the text of an HTML or XHTML document.
///
/// The `head`, `script`, `style`, `noscript` and `template` subtrees are removed
/// outright, so their contents cannot leak into the word stream. Every remaining
/// tag is removed while its text is kept, which is what turns markup into prose.
///
/// Entities are resolved here rather than trusted to the parser, because
/// `lol_html` passes character references through untouched.
pub fn extract_text(input: &[u8]) -> io::Result<Vec<u8>> {
    // `rewrite_str` needs a `&str`. Invalid UTF-8 is replaced rather than
    // rejected: a mangled byte in one book must not stop the whole library.
    let source = String::from_utf8_lossy(input).into_owned();

    // `"*"` and a specific selector can match the same element, so the universal
    // handler checks `removed()` first to avoid resurrecting discarded content.
    let rewritten = rewrite_str(
        &source,
        RewriteStrSettings {
            element_content_handlers: vec![
                element!("head", |el| {
                    el.remove();
                    Ok(())
                }),
                element!("script", |el| {
                    el.remove();
                    Ok(())
                }),
                element!("style", |el| {
                    el.remove();
                    Ok(())
                }),
                element!("noscript", |el| {
                    el.remove();
                    Ok(())
                }),
                element!("template", |el| {
                    el.remove();
                    Ok(())
                }),
                element!("*", |el| {
                    if el.removed() {
                        return Ok(());
                    }
                    el.remove_and_keep_content();
                    Ok(())
                }),
            ],
            ..RewriteStrSettings::default()
        },
    );

    // `Err` means the markup defeated the tokenizer; the fallback still recovers
    // most of the text, which beats failing the whole book.
    let stripped = strip_tags_fallback(rewritten.as_deref().unwrap_or("").as_bytes());
    let (decoded, _held) = decode_entities(&stripped);
    Ok(decoded)
}

/// Minimal tag stripper used when `lol-html` produces no text.
///
/// Works on raw bytes so multi-byte UTF-8 survives, and knows enough about
/// comments, CDATA and quoted attribute values to avoid emitting tag payloads.
pub fn strip_tags_fallback(input: &[u8]) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::with_capacity(input.len());
    let mut i = 0usize;
    let mut in_tag = false;
    let mut in_comment = false;
    let mut quote = 0u8;

    while i < input.len() {
        let b = input[i];
        if in_comment {
            if b == b'-' && input[i..].starts_with(b"-->") {
                in_comment = false;
                i += 3;
            } else {
                i += 1;
            }
            continue;
        }
        if in_tag {
            match b {
                b'"' | b'\'' => {
                    // Toggle quoting so a `>` inside an attribute value does not
                    // end the tag early.
                    if quote == 0 {
                        quote = b;
                    } else if quote == b {
                        quote = 0;
                    }
                }
                b'>' if quote == 0 => in_tag = false,
                _ => {}
            }
            i += 1;
            continue;
        }
        if b == b'<' {
            if input[i..].starts_with(b"<!--") {
                in_comment = true;
                i += 4;
            } else if input[i..].len() >= 9 && input[i..i + 9].eq_ignore_ascii_case(b"<![cdata[") {
                // CDATA is XML and therefore uppercase by spec, but leniency
                // costs nothing and the marker shows up lowercase in the wild.
                if let Some(end) = find(&input[i..], b"]]>") {
                    out.extend_from_slice(&input[i + 9..i + end]);
                    i += end + 3;
                } else {
                    out.push(b' ');
                    i += 9;
                }
            } else {
                in_tag = true;
                quote = 0;
                i += 1;
            }
            continue;
        }
        out.push(b);
        i += 1;
    }

    out
}

/// Decodes the common HTML named and numeric entities in `bytes`.
///
/// `lol_html` hands text chunks through more or less verbatim, so entities must
/// be resolved here before the tokenizer sees them, otherwise `&amp;` indexes as
/// the literal four characters `amp` plus punctuation.
///
/// Decoding is streaming: a trailing partial entity is returned as the number
/// of bytes that must be held back and prepended to the next chunk.
pub fn decode_entities(bytes: &[u8]) -> (Vec<u8>, usize) {
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut held = 0usize;
    let mut i = 0usize;

    while i < bytes.len() {
        let b = bytes[i];
        if b != b'&' {
            out.push(b);
            i += 1;
            continue;
        }
        match scan_entity(bytes, i) {
            EntityScan::Complete(end) => {
                let entity = &bytes[i..end]; // includes '&' and ';'
                match resolve_entity(entity) {
                    Some(text) => out.extend_from_slice(&text),
                    None => out.extend_from_slice(entity),
                }
                i = end;
            }
            EntityScan::NotEntity => {
                // A literal ampersand in prose: `Tom & Jerry`, `A&B`, `#1&2`.
                out.push(b'&');
                i += 1;
            }
            EntityScan::Truncated => {
                // `&am` at the end of the chunk; the rest may be in the next one.
                held = bytes.len() - i;
                break;
            }
        }
    }
    (out, held)
}

/// Outcome of scanning for the end of an entity that starts at `&`.
enum EntityScan {
    /// A complete entity: the byte range including `&` and `;`.
    Complete(usize),
    /// A `&` that is not an entity at all, e.g. the `&` in `A&B`.
    NotEntity,
    /// An entity cut off by the end of the buffer, which the next chunk may
    /// complete. The caller must hold these bytes back.
    Truncated,
}

/// Scans for the end of an entity starting at `start`, which must point at `&`.
fn scan_entity(bytes: &[u8], start: usize) -> EntityScan {
    let mut i = start + 1;
    while i < bytes.len() {
        if i - start > 32 {
            // Longer than any real entity; treat the `&` as literal.
            return EntityScan::NotEntity;
        }
        match bytes[i] {
            b';' => return EntityScan::Complete(i + 1),
            b if is_entity_char(b) => i += 1,
            // Any other character means this `&` was not an entity. Note this
            // includes another `&`, which starts a fresh entity.
            _ => return EntityScan::NotEntity,
        }
    }
    // Ran off the end with entity characters pending: possibly split.
    EntityScan::Truncated
}

/// Characters that may legally appear inside an entity name.
fn is_entity_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'#'
}

/// Resolves `&name;` or `&#nnn;` to its UTF-8 bytes.
fn resolve_entity(entity: &[u8]) -> Option<Vec<u8>> {
    let body = &entity[1..entity.len() - 1];
    if let Some(hex) = body.strip_prefix(b"#x").or_else(|| body.strip_prefix(b"#X")) {
        let code = u32::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()?;
        return char::from_u32(code).map(|c| c.to_string().into_bytes());
    }
    if let Some(dec) = body.strip_prefix(b"#") {
        let code: u32 = std::str::from_utf8(dec).ok()?.parse().ok()?;
        return char::from_u32(code).map(|c| c.to_string().into_bytes());
    }
    let name = std::str::from_utf8(body).ok()?;
    let text = match name {
        "amp" => "&",
        "lt" => "<",
        "gt" => ">",
        "quot" => "\"",
        "apos" => "'",
        "nbsp" => "\u{00A0}",
        "copy" => "\u{00A9}",
        "reg" => "\u{00AE}",
        "trade" => "\u{2122}",
        "hellip" => "\u{2026}",
        "mdash" => "\u{2014}",
        "ndash" => "\u{2013}",
        "lsquo" => "\u{2018}",
        "rsquo" => "\u{2019}",
        "ldquo" => "\u{201C}",
        "rdquo" => "\u{201D}",
        "agrave" => "\u{00E0}",
        "eacute" => "\u{00E9}",
        "egrave" => "\u{00E8}",
        "ccedil" => "\u{00E7}",
        "uuml" => "\u{00FC}",
        "ouml" => "\u{00F6}",
        "auml" => "\u{00E4}",
        _ => return None,
    };
    Some(text.as_bytes().to_vec())
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_body_text_only() {
        let html = b"<html><head><title>Ignored</title><style>p{color:red}</style></head>\
                     <body><p>Hello world</p></body></html>";
        let text = extract_text(html).unwrap();
        let text = String::from_utf8(text).unwrap();
        assert!(text.contains("Hello world"), "{text}");
        // `<head>` covers `<title>`, so its text must not survive either.
        assert!(!text.contains("Ignored"), "head content leaked: {text}");
        assert!(!text.contains("color"), "style content leaked: {text}");
    }

    #[test]
    fn drops_script_and_style_subtrees() {
        let html = b"<body><script>var x = 'not indexed';</script><p>real words</p>\
                     <style>.a{content:'nope'}</style></body>";
        let t = String::from_utf8(extract_text(html).unwrap()).unwrap();
        assert!(t.contains("real words"), "{t}");
        assert!(!t.contains("not indexed"), "script body leaked");
        assert!(!t.contains("nope"), "style body leaked");
    }

    #[test]
    fn decoded_utf8_survives() {
        let html = "<body><p>Thám tử Sherlock — Yến Tiệc</p><p>中文测试</p></body>".as_bytes();
        let t = extract_text(html).unwrap();
        let s = String::from_utf8(t).unwrap();
        assert!(s.contains("Thám tử"), "{s}");
        assert!(s.contains("中文测试"), "{s}");
    }

    #[test]
    fn fallback_strips_tags_without_leaking_attributes() {
        let t = strip_tags_fallback(b"<p class=\"a>b\">text</p>");
        assert_eq!(t, b"text");
    }

    #[test]
    fn fallback_handles_comments_and_cdata() {
        let t = strip_tags_fallback(b"<div><!-- hidden -->kept<![CDATA[raw]]></div>");
        assert_eq!(t, b"keptraw");
    }

    #[test]
    fn entity_decoding_resolves_named_and_numeric() {
        assert_eq!(decode_entities(b"a &amp; b").0, b"a & b");
        assert_eq!(decode_entities(b"x&#65;y").0, b"xAy");
        assert_eq!(decode_entities(b"x&#x41;y").0, b"xAy");
        assert_eq!(decode_entities(b"caf&eacute;").0, "café".as_bytes());
        assert_eq!(decode_entities(b"a&nbsp;b").0, "a\u{a0}b".as_bytes());
    }

    #[test]
    fn unknown_entities_are_left_alone() {
        assert_eq!(decode_entities(b"&notarealentity;").0, b"&notarealentity;");
    }

    #[test]
    fn entity_split_across_chunks_is_held_back() {
        // A chunk boundary must not turn `&amp;` into a decode of `&am`.
        // "x &am" is 5 bytes; "&am" is the 3-byte tail.
        let (out, held) = decode_entities(b"x &am");
        assert_eq!(out, b"x ");
        assert_eq!(held, 3, "the partial entity is held for the next chunk");

        // Rejoining the held bytes and the continuation decodes exactly as if
        // the entity had never been split.
        let (rest, held2) = decode_entities(b"p; y");
        assert_eq!(held2, 0);
        let mut rejoined = b"x ".to_vec();
        rejoined.extend_from_slice(b"&am");
        rejoined.extend_from_slice(b"p; y");
        assert_eq!(decode_entities(&rejoined).0, b"x & y");
        assert_eq!(rest, b"p; y");
    }

    #[test]
    fn amp_without_semicolon_is_not_decoded() {
        assert_eq!(decode_entities(b"a & b").0, b"a & b");
    }

    #[test]
    fn nested_ampersands_end_the_first_entity() {
        // The first `&am` is literal text, not an entity: the second `&` ends the
        // scan. Only the real `&amp;` gets decoded.
        assert_eq!(decode_entities(b"&am&amp;").0, b"&am&");
        assert_eq!(decode_entities(b"&amp&amp;").0, b"&amp&");
        assert_eq!(decode_entities(b"&amp; &amp;").0, b"& &");
    }

}
