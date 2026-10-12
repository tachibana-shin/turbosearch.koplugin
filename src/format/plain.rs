//! Decoders for formats with no container: plain text and markdown.
//!
//! These formats carry no metadata beyond the filename, so the whole file is one
//! section and the indexer's `provenance` becomes `filename` (`note.md`
//! section 2.3).

use std::io;

/// One readable unit of a book, in reading order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    /// Stable identifier stored in `ENTRYMAP`: the entry name inside a
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

/// Decodes text bytes, honouring a BOM and falling back to heuristics.
///
/// `encoding_rs` handles the legacy 8-bit encodings that turn up in older text
/// files, which a UTF-8-only path would mangle into replacement characters.
pub fn decode_text(bytes: &[u8]) -> io::Result<Vec<u8>> {
    // A UTF-8 BOM is stripped so it does not become a spurious first token.
    if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return Ok(bytes[3..].to_vec());
    }
    if bytes.starts_with(&[0xFF, 0xFE]) {
        return Ok(decode_utf16(bytes, false));
    }
    if bytes.starts_with(&[0xFE, 0xFF]) {
        return Ok(decode_utf16(bytes, true));
    }

    if std::str::from_utf8(bytes).is_ok() {
        return Ok(bytes.to_vec());
    }

    // Windows-1252 is the encoding of last resort for European text: it is
    // latin-1 with extra punctuation, so decoding never fails or drops bytes.
    let (decoded, _, _) = encoding_rs::WINDOWS_1252.decode(bytes);
    Ok(decoded.into_owned().into_bytes())
}

fn decode_utf16(bytes: &[u8], big_endian: bool) -> Vec<u8> {
    let units: Vec<u16> = if big_endian {
        bytes[2..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_be_bytes([c[0], c[1]]))
            .collect()
    } else {
        bytes[2..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect()
    };
    String::from_utf16_lossy(&units).into_bytes()
}

/// Builds the single section for a whole-file format.
pub fn single_section(bytes: &[u8]) -> io::Result<Vec<Section>> {
    Ok(vec![Section {
        name: "body".to_string(),
        raw_offset: 0,
        raw_len: bytes.len() as u64,
        text: decode_text(bytes)?,
    }])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_passes_through() {
        assert_eq!(
            decode_text("Thám tử".as_bytes()).unwrap(),
            "Thám tử".as_bytes()
        );
    }

    #[test]
    fn utf8_bom_is_stripped() {
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(b"hello");
        assert_eq!(decode_text(&bytes).unwrap(), b"hello");
    }

    #[test]
    fn utf16_boms_are_decoded() {
        let mut le = vec![0xFF, 0xFE];
        for c in "hi".encode_utf16() {
            le.extend_from_slice(&c.to_le_bytes());
        }
        assert_eq!(decode_text(&le).unwrap(), b"hi");

        let mut be = vec![0xFE, 0xFF];
        for c in "hi".encode_utf16() {
            be.extend_from_slice(&c.to_be_bytes());
        }
        assert_eq!(decode_text(&be).unwrap(), b"hi");
    }

    #[test]
    fn an_odd_trailing_byte_does_not_panic() {
        let mut bytes = vec![0xFF, 0xFE];
        bytes.extend_from_slice(b"h\x00i\x00\x00"); // dangling half unit
        let out = decode_text(&bytes).unwrap();
        assert!(!out.is_empty());
    }

    #[test]
    fn single_section_covers_the_whole_file() {
        let secs = single_section(b"one section only").unwrap();
        assert_eq!(secs.len(), 1);
        assert_eq!(secs[0].text, b"one section only");
        assert_eq!(secs[0].raw_offset, 0);
        assert_eq!(secs[0].raw_len, 16, "\"one section only\" is 16 bytes");
        assert_eq!(secs[0].name, "body");
    }
}
