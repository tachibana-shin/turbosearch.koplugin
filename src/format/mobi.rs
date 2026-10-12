//! PalmDB and MOBI/AZW/AZW3 parsing.
//!
//! A MOBI file is a Palm Database, not a ZIP: a fixed 78-byte header, a list of
//! record offsets, then the records. Record 0 holds the PalmDOC header plus an
//! EXTH block carrying the book's metadata; the remaining records hold the book
//! text, PalmDOC-compressed.
//!
//! Only what the indexer needs is implemented: record locations, text, and the
//! EXTH metadata fields. Layout, rendering and DRM are out of scope.

use std::io;

/// Fixed size of the PalmDB header.
const PDB_HEADER_LEN: usize = 78;
/// Offset of the PalmDB type code.
const PDB_TYPE_OFFSET: usize = 60;
/// PalmDB type code for a MOBI book.
const PDB_TYPE_MOBI: &[u8] = b"BOOKMOBI";
/// Size of one PalmDB record-info entry.
const PDB_RECORD_INFO_SIZE: usize = 8;

/// EXTH record types the indexer keeps.
const EXTH_TITLE: u32 = 503;
const EXTH_AUTHOR: u32 = 507;
const EXTH_PUBLISHER: u32 = 108;
const EXTH_DESCRIPTION: u32 = 103;
const EXTH_SUBJECT: u32 = 125;
const EXTH_LANGUAGE: u32 = 524;
const EXTH_ISBN: u32 = 104;

/// PalmDOC compression types.
const COMPRESSION_NONE: u16 = 1;
const COMPRESSION_PALMDOC: u16 = 2;
const COMPRESSION_HUFFMAN: u16 = 17480;

/// One parsed MOBI file.
#[derive(Debug, Clone, Default)]
pub struct Mobi {
    /// PalmDOC compression used by the text records.
    pub compression: u16,
    /// Decompressed book text, records concatenated in order.
    pub text: Vec<u8>,
    /// Metadata recovered from the EXTH block.
    pub metadata: MobiMetadata,
}

/// The subset of EXTH the indexer stores.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MobiMetadata {
    pub title: String,
    pub authors: String,
    pub publisher: String,
    pub description: String,
    pub subjects: Vec<String>,
    pub language: String,
    pub isbn: String,
}

/// The byte range of one PalmDB record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Record {
    pub offset: usize,
    pub len: usize,
}

/// Parses a MOBI/AZW/AZW3 file.
pub fn parse(bytes: &[u8]) -> io::Result<Mobi> {
    if bytes.len() < PDB_HEADER_LEN {
        return Err(invalid("truncated PalmDB header"));
    }
    if &bytes[PDB_TYPE_OFFSET..PDB_TYPE_OFFSET + 8] != PDB_TYPE_MOBI {
        return Err(invalid("not a MOBI PalmDB"));
    }

    let record_count = u16::from_be_bytes([bytes[76], bytes[77]]) as usize;
    let records = parse_records(bytes, record_count)?;

    // Record 0 carries the PalmDOC header and, usually, the EXTH metadata.
    let metadata = MobiMetadata::default();
    let mut metadata = metadata;
    let mut compression = COMPRESSION_PALMDOC;
    let mut text_length = 0usize;

    if let Some(record0) = records
        .first()
        .and_then(|r| bytes.get(r.offset..r.offset + r.len))
    {
        if record0.len() >= 16 {
            compression = u16::from_be_bytes([record0[0], record0[1]]);
            text_length =
                u32::from_be_bytes([record0[4], record0[5], record0[6], record0[7]]) as usize;
        }
        if let Some(exth) = find(record0, b"EXTH") {
            parse_exth(exth, &mut metadata);
        }
    }

    let mut text = Vec::with_capacity(text_length);
    for record in records.iter().skip(1) {
        let Some(slice) = bytes.get(record.offset..record.offset + record.len) else {
            continue;
        };
        match compression {
            COMPRESSION_NONE => text.extend_from_slice(slice),
            // 17480 is the Huffman/CDIC variant used by some AZW files. Its
            // codewords occupy the same byte ranges as PalmDOC's, so decoding
            // it as PalmDOC silently produces garbage; leave the text empty
            // instead. `compression` is returned so the caller can tell.
            COMPRESSION_HUFFMAN => {}
            _ => palmdoc_decompress(slice, &mut text),
        }
    }
    if text_length > 0 && text.len() > text_length {
        text.truncate(text_length);
    }

    Ok(Mobi {
        compression,
        text,
        metadata,
    })
}

/// Reads the record-info list, deriving each record's length from the next
/// record's offset (the last one runs to end of file).
fn parse_records(bytes: &[u8], record_count: usize) -> io::Result<Vec<Record>> {
    let mut records = Vec::with_capacity(record_count);
    for i in 0..record_count {
        let base = PDB_HEADER_LEN + i * PDB_RECORD_INFO_SIZE;
        let offset = u32::from_be_bytes([
            *bytes
                .get(base)
                .ok_or_else(|| invalid("missing record offset"))?,
            *bytes
                .get(base + 1)
                .ok_or_else(|| invalid("missing record offset"))?,
            *bytes
                .get(base + 2)
                .ok_or_else(|| invalid("missing record offset"))?,
            *bytes
                .get(base + 3)
                .ok_or_else(|| invalid("missing record offset"))?,
        ]) as usize;

        let next = if i + 1 < record_count {
            let b = PDB_HEADER_LEN + (i + 1) * PDB_RECORD_INFO_SIZE;
            u32::from_be_bytes([
                *bytes.get(b).unwrap_or(&0),
                *bytes.get(b + 1).unwrap_or(&0),
                *bytes.get(b + 2).unwrap_or(&0),
                *bytes.get(b + 3).unwrap_or(&0),
            ]) as usize
        } else {
            bytes.len()
        };
        let len = next.saturating_sub(offset);
        records.push(Record { offset, len });
    }
    Ok(records)
}

/// Parses EXTH metadata out of a slice starting with "EXTH".
fn parse_exth(exth: &[u8], out: &mut MobiMetadata) {
    if exth.len() < 12 {
        return;
    }
    let count = u32::from_be_bytes([exth[8], exth[9], exth[10], exth[11]]) as usize;
    let mut pos = 12usize;

    for _ in 0..count {
        if pos + 8 > exth.len() {
            break;
        }
        let rec_type = u32::from_be_bytes([exth[pos], exth[pos + 1], exth[pos + 2], exth[pos + 3]]);
        let len = u32::from_be_bytes([exth[pos + 4], exth[pos + 5], exth[pos + 6], exth[pos + 7]])
            as usize;
        if len < 8 || pos + len > exth.len() {
            break;
        }
        // EXTH strings are NUL terminated; the NUL is not part of the text.
        let raw = &exth[pos + 8..pos + len];
        let raw = &raw[..raw.iter().position(|&b| b == 0).unwrap_or(raw.len())];
        let text = String::from_utf8_lossy(raw).into_owned();

        match rec_type {
            EXTH_TITLE if out.title.is_empty() => out.title = text,
            EXTH_AUTHOR if out.authors.is_empty() => out.authors = text,
            EXTH_PUBLISHER if out.publisher.is_empty() => out.publisher = text,
            EXTH_DESCRIPTION if out.description.is_empty() => out.description = text,
            EXTH_SUBJECT if out.subjects.len() < 32 => out.subjects.push(text),
            EXTH_LANGUAGE if out.language.is_empty() => out.language = text,
            EXTH_ISBN if out.isbn.is_empty() => out.isbn = text,
            _ => {}
        }
        pos += len;
    }
}

/// Decompresses one PalmDOC record into `out`.
///
/// Three byte classes do the work, per the MobileRead PalmDOC spec:
/// * `0x00`, `0x09` — space and tab
/// * `0x01..=0x08` — copy that many following bytes straight from the stream
/// * `0x0A..=0x7F` — literal
/// * `0x80..=0xBF` — LZ77 back-reference: a 14-bit item built from the low six
///   bits of this byte and all eight of the next, split into an 11-bit
///   distance and a 3-bit length (`length + 3`)
/// * `0xC0..=0xFF` — a two-character expansion into a space plus the letter
///   obtained by clearing the top bit; this is what makes prose compress well
fn palmdoc_decompress(input: &[u8], out: &mut Vec<u8>) {
    let mut pos = 0usize;
    while pos < input.len() {
        let byte = input[pos];
        pos += 1;

        match byte {
            0x00 => out.push(b' '),
            0x09 => out.push(b'\t'),
            0x01..=0x08 => {
                // The byte is a literal run length, not a repeat count.
                for _ in 0..byte as usize {
                    match input.get(pos) {
                        Some(&b) => {
                            out.push(b);
                            pos += 1;
                        }
                        None => break,
                    }
                }
            }
            0x0A..=0x7F => out.push(byte),
            0x80..=0xBF => {
                let Some(&next) = input.get(pos) else {
                    // A dangling reference byte at the very end: emit it as-is.
                    out.push(byte);
                    break;
                };
                pos += 1;

                let item = (((byte & 0x3F) as usize) << 8) | next as usize;
                let distance = item >> 3;
                let length = (item & 0x07) + 3;

                if distance == 0 || distance > out.len() {
                    // Broken reference. Skipping the pair keeps the rest of the
                    // record readable instead of cascading into garbage.
                    continue;
                }
                let start = out.len() - distance;
                for k in 0..length {
                    let b = out[start + k];
                    out.push(b);
                }
            }
            _ => {
                // 0xC0..=0xFF: a space plus the letter with the top bit cleared.
                out.push(b' ');
                out.push(byte ^ 0x80);
            }
        }
    }
}

/// Finds the first occurrence of `needle` in `haystack`.
fn find<'a>(haystack: &'a [u8], needle: &[u8]) -> Option<&'a [u8]> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|i| &haystack[i..])
}

fn invalid(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leb(v: u32) -> [u8; 4] {
        v.to_be_bytes()
    }

    /// Builds a structurally real PalmDB with one uncompressed text record and
    /// an EXTH block carrying a title.
    fn build_pdb(compression: u16, text: &[u8]) -> Vec<u8> {
        let mut record0 = vec![0u8; 16];
        record0[0..2].copy_from_slice(&compression.to_be_bytes());
        record0[4..8].copy_from_slice(&leb(text.len() as u32));
        record0[10..12].copy_from_slice(&4096u16.to_be_bytes());

        // EXTH: "EXTH", header length, record count, then one title record.
        let title = b"A Title";
        let mut exth = Vec::new();
        exth.extend_from_slice(b"EXTH");
        exth.extend_from_slice(&leb(8));
        exth.extend_from_slice(&leb(1));
        exth.extend_from_slice(&leb(EXTH_TITLE));
        exth.extend_from_slice(&leb(title.len() as u32 + 8));
        exth.extend_from_slice(title);
        exth.push(0);
        record0.extend_from_slice(&exth);

        // PalmDB header: 78 bytes, then two record-info entries.
        let record_count: u16 = 2;
        let mut out = vec![0u8; PDB_HEADER_LEN];
        out[PDB_TYPE_OFFSET..PDB_TYPE_OFFSET + 8].copy_from_slice(PDB_TYPE_MOBI);
        out[76..78].copy_from_slice(&record_count.to_be_bytes());

        let info = record_count as usize * PDB_RECORD_INFO_SIZE;
        let offset0 = out.len() + info;
        out.extend_from_slice(&(offset0 as u32).to_be_bytes());
        out.extend_from_slice(&[0; 4]);
        out.extend_from_slice(&((offset0 + record0.len()) as u32).to_be_bytes());
        out.extend_from_slice(&[0; 4]);

        out.extend_from_slice(&record0);
        out.extend_from_slice(text);
        out
    }

    /// Builds a PalmDB whose single text record is PalmDOC-compressed.
    fn build_pdb_compressed(text: &[u8]) -> Vec<u8> {
        let compressed = compress_palmedoc_literals(text);
        let mut record0 = vec![0u8; 16];
        record0[0..2].copy_from_slice(&COMPRESSION_PALMDOC.to_be_bytes());
        record0[4..8].copy_from_slice(&leb(text.len() as u32));
        record0[10..12].copy_from_slice(&4096u16.to_be_bytes());

        let mut pdb = vec![0u8; PDB_HEADER_LEN];
        pdb[PDB_TYPE_OFFSET..PDB_TYPE_OFFSET + 8].copy_from_slice(PDB_TYPE_MOBI);
        pdb[76..78].copy_from_slice(&2u16.to_be_bytes());

        let offset0 = PDB_HEADER_LEN + 2 * PDB_RECORD_INFO_SIZE;
        let offset1 = offset0 + record0.len();
        pdb.extend_from_slice(&(offset0 as u32).to_be_bytes());
        pdb.extend_from_slice(&[0; 4]);
        pdb.extend_from_slice(&(offset1 as u32).to_be_bytes());
        pdb.extend_from_slice(&[0; 4]);
        pdb.extend_from_slice(&record0);
        pdb.extend_from_slice(&compressed);
        pdb
    }

    /// Trivial "compressor" emitting only literals; enough to prove the decoder
    /// path, not a real ratio test.
    fn compress_palmedoc_literals(text: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(text.len());
        for &b in text {
            match b {
                b' ' => out.push(0x00),
                b'\t' => out.push(0x09),
                0x0A..=0x7F => out.push(b),
                _ => {
                    // Fall back to a self-referencing run is not possible for
                    // arbitrary bytes, so emit the raw byte anyway; the decoder
                    // treats 0x80+ as a pair, so avoid that range in fixtures.
                    out.push(b & 0x7F);
                }
            }
        }
        out
    }

    #[test]
    fn parses_uncompressed_text_and_metadata() {
        let pdb = build_pdb(COMPRESSION_NONE, b"Sherlock Holmes in Hanoi.");
        let m = parse(&pdb).unwrap();
        assert_eq!(m.compression, COMPRESSION_NONE);
        assert_eq!(m.metadata.title, "A Title");
        assert_eq!(m.text, b"Sherlock Holmes in Hanoi.");
    }

    #[test]
    fn parses_palmedoc_compressed_text() {
        let pdb = build_pdb_compressed(b"A plain sentence.");
        let m = parse(&pdb).unwrap();
        assert_eq!(m.compression, COMPRESSION_PALMDOC);
        assert_eq!(m.text, b"A plain sentence.");
    }

    #[test]
    fn records_are_located_and_sized() {
        let pdb = build_pdb(COMPRESSION_NONE, b"0123456789");
        let records = parse_records(&pdb, 2).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].offset, PDB_HEADER_LEN + 16);
        assert_eq!(records[1].len, 10);
    }

    #[test]
    fn rejects_a_non_palmdb_file() {
        let mut bytes = vec![0u8; 128];
        bytes[PDB_TYPE_OFFSET..PDB_TYPE_OFFSET + 8].copy_from_slice(b"BOOKMOBX");
        assert!(parse(&bytes).is_err());
    }

    #[test]
    fn rejects_a_truncated_header() {
        assert!(parse(&[0u8; 10]).is_err());
        assert!(parse(&[]).is_err());
    }

    #[test]
    fn literal_and_space_and_tab_decode() {
        let mut out = Vec::new();
        palmdoc_decompress(&[b'a', 0x00, b'b', 0x09, b'c'], &mut out);
        assert_eq!(out, b"a b\tc");
    }

    #[test]
    fn a_low_byte_counts_following_literals_instead_of_repeating() {
        let mut out = Vec::new();
        // 0x01..=0x08 is a run length over the *stream*, not a repeat count over
        // the output; this is the single most misread rule in the format.
        palmdoc_decompress(&[0x03, b'x', b'y', b'z'], &mut out);
        assert_eq!(out, b"xyz");
    }

    #[test]
    fn the_high_byte_expands_to_a_space_plus_a_letter() {
        // 0xC0..=0xFF is not a back-reference: it emits two characters.
        let mut out = Vec::new();
        palmdoc_decompress(&[0xC3], &mut out);
        assert_eq!(out, b" C");
    }

    #[test]
    fn back_reference_copies_from_earlier_output() {
        let mut out = Vec::new();
        // The 14-bit item is (distance << 3) | (length - 3); 24 means distance
        // 3, length 3, so "abc" copies itself.
        palmdoc_decompress(&[b'a', b'b', b'c', 0x80, 0x18], &mut out);
        assert_eq!(out, b"abcabc");
    }

    #[test]
    fn decode_a_leading_guide_tag_from_a_real_mobi_file() {
        // Golden bytes lifted from record 1 of a Vietnamese MOBI in the test
        // corpus. A hand-waved distance formula still produces *something*
        // here, so the exact expected string is what catches regressions.
        let compressed = [
            b'<', b'h', b't', b'm', b'l', b'>', b'<', b'h', b'e', b'a', b'd', b'>', b'<', b'g',
            b'u', b'i', b'd', b'e', b'>', b'<', b'r', b'e', b'f', b'e', b'r', b'e', b'n', b'c',
            b'e', 0xF4, 0x79, b'p', b'e', b'=', b'"', b't', b'o', b'c', b'"', 0xF4, b'i', b't',
            b'l', 0x80, 0x60, b'T', b'a', b'b', b'l', b'e', 0xEF, b'f', 0xC3, b'o', b'n', b't',
            b'e', b'n', b't', b's', b'"', 0xE6, b'i', b'l', b'e', b'p', b'o', b's', b'=', b'0',
            b'0', b'0', b'0', b'5', b'5', b'2', b'3', b'1', b'7', b' ',
        ];
        let mut out = Vec::new();
        palmdoc_decompress(&compressed, &mut out);
        assert_eq!(
            String::from_utf8_lossy(&out),
            "<html><head><guide><reference type=\"toc\" title=\"Table of Contents\" filepos=0000552317 "
        );
    }

    #[test]
    fn a_broken_back_reference_is_skipped_without_panicking() {
        let mut out = vec![b'x', b'y'];
        // Asks for a distance of 2924 from two bytes of output.
        let item = 2924usize << 3;
        let b = 0x80u8 | ((item >> 8) as u8 & 0x3F);
        let next = (item & 0xFF) as u8;
        palmdoc_decompress(&[b, next], &mut out);
        assert_eq!(out, b"xy");
    }

    #[test]
    fn a_dangling_reference_byte_is_emitted_as_is() {
        let mut out = Vec::new();
        // 0x80..=0xBF promises a second byte that never arrives.
        palmdoc_decompress(&[b'a', 0x80], &mut out);
        assert_eq!(out, b"a\x80");
    }

    #[test]
    fn exth_with_only_a_header_yields_empty_metadata() {
        let exth = b"EXTH\x00\x00\x00\x08\x00\x00\x00\x00";
        let mut md = MobiMetadata::default();
        parse_exth(exth, &mut md);
        assert_eq!(md, MobiMetadata::default());
    }

    #[test]
    fn a_truncated_exth_record_is_ignored_not_fatal() {
        // Declares one record but stops before its payload.
        let mut exth = Vec::new();
        exth.extend_from_slice(b"EXTH");
        exth.extend_from_slice(&leb(8));
        exth.extend_from_slice(&leb(1));
        exth.extend_from_slice(&leb(EXTH_TITLE));
        exth.extend_from_slice(&leb(999)); // way too long
        let mut md = MobiMetadata::default();
        parse_exth(&exth, &mut md);
        assert_eq!(md.title, "");
    }
}
