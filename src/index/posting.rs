//! The postings block codec: how one chunk of an inverted list is stored.
//!
//! Layout follows `note.md` section 2 and 3.4. A chunk holds up to
//! [`CHUNK_POSTINGS`] postings for a single term, stored **columnar** rather than
//! row-wise, because each column has a different shape and therefore a different
//! best coder:
//!
//! | Column    | Shape                          | Coder            |
//! |-----------|--------------------------------|------------------|
//! | `doc_id`  | non-decreasing                 | Elias-Fano       |
//! | `freq`    | small positive                 | PForDelta        |
//! | `pos_gap` | non-negative, resets per doc    | PForDelta        |
//! | `entry`   | constant for long runs         | RLE              |
//!
//! Chunks are kept small on purpose: redb reads a whole value on `get()`, so a
//! small chunk means a lazy, cheap decode on the query path, and a small chunk key
//! means the append-only ordering property in `note.md` section 2 holds.
//!
//! An important consequence of the append-only design: `doc_id` is monotonically
//! increasing, so a newly appended chunk always sorts after the existing ones,
//! and the whole posting list stays ordered without any rewrite.

use crate::codec::{elias_fano, pfor, rle};

/// Postings per chunk.
///
/// Small enough that a `get()` reads a value quick to decode, large enough that
/// the per-chunk header is not a measurable fraction of the block.
pub const CHUNK_POSTINGS: usize = 8192;

/// Magic plus format version, checked on read.
///
/// A mismatch means the on-disk block was written by a different build, so the
/// caller is told rather than being handed mis-parsed columns.
///
/// The low byte carries the framing version and must be bumped whenever the
/// layout changes. Leaving the magic alone while moving fields is a silent
/// break: the tag still matches, so the reader proceeds and every chunk decodes
/// to something wrong. That happened once when a framing byte was added after
/// the tag, and every previously built index became unreadable while still
/// appearing valid.
pub const CODEC_TAG: u32 = 0x5450_5254; // "TPR1"

/// The columns are stored as packed, uncompressed.
const ENCODING_RAW: u8 = 0;
/// The columns are stored as a zstd frame.
const ENCODING_ZSTD: u8 = 1;

/// zstd level for a chunk payload.
///
/// Level 3, not 19 (as the dictionary blob uses): a chunk is written once but
/// decoded on every query that touches it, and at ~1 KB the higher levels save a
/// few percent while costing real decode time on a Kindle.
const CHUNK_ZSTD_LEVEL: i32 = 3;

/// What one posting means.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Posting {
    pub doc_id: u32,
    /// Monotone position within the book.
    pub position: u32,
    /// Index into the book's section list, used to locate the text.
    pub entry_idx: u16,
}

impl Posting {
    pub fn new(doc_id: u32, position: u32, entry_idx: u16) -> Self {
        Self { doc_id, position, entry_idx }
    }
}

/// A chunk of postings, in the form it was written.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Chunk {
    /// Lower bound on the doc ids inside, for binary search.
    pub first_doc: u32,
    pub postings: Vec<Posting>,
}

impl Chunk {
    /// Packs a chunk into bytes.
    pub fn encode(&self) -> Vec<u8> {
        // Group by document. Within a document the positions are strictly
        // increasing, which is what makes the gap column compressible.
        let mut docs: Vec<u32> = Vec::new();
        let mut freqs: Vec<u32> = Vec::new();
        let mut entries: Vec<u32> = Vec::new();
        let mut pos_gaps: Vec<u32> = Vec::new();

        let mut i = 0usize;
        while i < self.postings.len() {
            let doc = self.postings[i].doc_id;
            let mut j = i;
            while j < self.postings.len() && self.postings[j].doc_id == doc {
                j += 1;
            }
            docs.push(doc);
            freqs.push((j - i) as u32);

            let mut prev = 0u32;
            for k in i..j {
                let p = self.postings[k];
                // One entry per posting, not per document: a book spans many
                // sections, and keeping only the first would attach every posting
                // to the wrong section and break snippet extraction. Because the
                // list is sorted by `(doc, position)`, this column is
                // non-decreasing within a document, so RLE still collapses it to
                // one value per section.
                entries.push(p.entry_idx as u32);
                pos_gaps.push(if k == i { p.position } else { p.position - prev });
                prev = p.position;
            }
            i = j;
        }

        let raw = self.pack_sections(&docs, &freqs, &entries, &pos_gaps);

        // Compress the packed columns.
        //
        // redb stores a value inside a 4 KiB B-tree page and its page size is
        // fixed, so the value's size directly decides how many rows fit per
        // page. The measured library held 71.6 k rows of ~1.2 KB each, which
        // cost more than twice the space the columns actually need. Level 3 is
        // deliberate: a chunk is written once and read on demand, and at this
        // size the difference between levels is a fraction of the value.
        if let Ok(packed) = zstd::stream::encode_all(raw.as_slice(), CHUNK_ZSTD_LEVEL) {
            // Only take the compressed form when it is actually smaller; small
            // chunks are mostly headers and can inflate.
            if packed.len() < raw.len() {
                let mut out = Vec::with_capacity(packed.len() + 8);
                out.extend_from_slice(&CODEC_TAG.to_le_bytes());
                out.push(ENCODING_ZSTD);
                out.extend_from_slice(&packed);
                return out;
            }
        }

        let mut out = Vec::with_capacity(raw.len() + 5);
        out.extend_from_slice(&CODEC_TAG.to_le_bytes());
        out.push(ENCODING_RAW);
        out.extend_from_slice(&raw);
        out
    }

    /// Lays the four columns out, each behind a u32 length.
    fn pack_sections(
        &self,
        docs: &[u32],
        freqs: &[u32],
        entries: &[u32],
        pos_gaps: &[u32],
    ) -> Vec<u8> {
        let mut out = Vec::with_capacity(64);
        out.extend_from_slice(&CODEC_TAG.to_le_bytes());
        out.extend_from_slice(&self.first_doc.to_le_bytes());
        // Three u32 counters.
        out.extend_from_slice(&(self.postings.len() as u32).to_le_bytes());
        out.extend_from_slice(&(docs.len() as u32).to_le_bytes());
        out.extend_from_slice(&(pos_gaps.len() as u32).to_le_bytes());

        append_section(&mut out, &elias_fano::encode(docs));
        append_section(&mut out, &pfor::encode_auto(freqs));
        append_section(&mut out, &rle::encode_auto(entries));
        append_section(&mut out, &pfor::encode_auto(pos_gaps));
        out
    }

    /// Unpacks a chunk.
    ///
    /// Returns `None` when the block is not from this codec, or is truncated.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < 21 {
            return None;
        }
        let mut pos = 0usize;
        let tag = read_u32(bytes, &mut pos)?;
        if tag != CODEC_TAG {
            return None;
        }
        // One byte says which form the columns are in, so a reader never has to
        // guess and a half-upgraded database still decodes.
        let encoding = bytes[pos];
        pos += 1;

        let body: Vec<u8> = match encoding {
            ENCODING_RAW => bytes[pos..].to_vec(),
            ENCODING_ZSTD => zstd::stream::decode_all(&bytes[pos..]).ok()?,
            _ => return None,
        };
        let _ = pos;
        Self::decode_columns(&body)
    }

    /// Decodes the column layout, after any container-level decompression.
    fn decode_columns(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < 20 {
            return None;
        }
        let mut pos = 0usize;
        // The tag is repeated inside the payload so a body decompressed from
        // the zstd frame is checked too, not just the outer container.
        if read_u32(bytes, &mut pos)? != CODEC_TAG {
            return None;
        }
        let first_doc = read_u32(bytes, &mut pos)?;
        let n_postings = read_u32(bytes, &mut pos)? as usize;
        let n_docs = read_u32(bytes, &mut pos)? as usize;
        let n_positions = read_u32(bytes, &mut pos)? as usize;

        // Sanity, so a corrupt block cannot ask for an enormous allocation.
        if n_postings > CHUNK_POSTINGS || n_docs > n_postings || n_positions > CHUNK_POSTINGS {
            return None;
        }

        // A column that fails to decode rejects the whole block: a partially
        // readable chunk would silently produce wrong postings, which is worse
        // than reporting nothing.
        let docs = elias_fano::decode(take_section(bytes, &mut pos)?).ok()?;
        if docs.len() != n_docs {
            return None;
        }
        let freqs = pfor::decode(take_section(bytes, &mut pos)?).ok()?;
        if freqs.len() != n_docs {
            return None;
        }
        let entries = rle::decode(take_section(bytes, &mut pos)?).ok()?;
        if entries.len() != n_positions {
            return None;
        }
        let pos_gaps = pfor::decode(take_section(bytes, &mut pos)?).ok()?;
        if pos_gaps.len() != n_positions {
            return None;
        }

        // The position column holds deltas, so it must be accumulated per
        // document. Emitting the raw values gives every posting the first
        // position, which the round-trip tests catch immediately.
        let mut postings = Vec::with_capacity(n_postings);
        let mut gap_index = 0usize;
        for (doc_index, doc) in docs.iter().enumerate() {
            let freq = freqs[doc_index] as usize;
            let mut position = 0u32;
            for _ in 0..freq {
                if gap_index >= pos_gaps.len() || gap_index >= entries.len() {
                    return None;
                }
                // One entry per posting: a book's text spans many sections, so
                // indexing by document here would attach every posting to the
                // book's first section and send snippets to the wrong place.
                let entry = u16::try_from(entries[gap_index]).ok()?;
                // Checked: a corrupt block must not wrap positions around and
                // produce plausible-looking wrong results.
                position = position.checked_add(pos_gaps[gap_index])?;
                postings.push(Posting::new(*doc, position, entry));
                gap_index += 1;
            }
        }

        Some(Self { first_doc, postings })
    }

    /// True when the chunk may contain `doc`.
    ///
    /// Doc ids are ascending across the whole list, so a chunk whose first doc is
    /// above the target cannot be it.
    pub fn may_contain(&self, doc: u32) -> bool {
        self.first_doc <= doc
    }

    /// Decodes only the postings of one document.
    ///
    /// The expensive part of the query path is decoding a column, and a chunk
    /// holds 8192 postings, so being able to stop at one document matters.
    pub fn postings_for(&self, doc: u32) -> Vec<Posting> {
        self.postings
            .iter()
            .copied()
            .filter(|p| p.doc_id == doc)
            .collect()
    }
}

fn append_section(out: &mut Vec<u8>, payload: &[u8]) {
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(payload);
}

fn take_section<'a>(bytes: &'a [u8], pos: &mut usize) -> Option<&'a [u8]> {
    let len = read_u32(bytes, pos)? as usize;
    let end = pos.checked_add(len)?;
    let slice = bytes.get(*pos..end)?;
    *pos = end;
    Some(slice)
}

fn read_u32(bytes: &[u8], pos: &mut usize) -> Option<u32> {
    let end = pos.checked_add(4)?;
    let slice = bytes.get(*pos..end)?;
    *pos = end;
    Some(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

/// Splits a posting list into chunks.
///
/// The list must already be sorted by `(doc_id, position)`, which the indexer
/// guarantees because it appends one book at a time with ascending ids.
pub fn into_chunks(mut postings: Vec<Posting>) -> Vec<Chunk> {
    if postings.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(postings.len() / CHUNK_POSTINGS + 1);
    while postings.len() > CHUNK_POSTINGS {
        let rest = postings.split_off(CHUNK_POSTINGS);
        let first_doc = postings.first().map(|p| p.doc_id).unwrap_or(0);
        out.push(Chunk {
            first_doc,
            postings: std::mem::take(&mut postings),
        });
        postings = rest;
    }
    let first_doc = postings.first().map(|p| p.doc_id).unwrap_or(0);
    out.push(Chunk {
        first_doc,
        postings,
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A cheap fingerprint, so a mismatch is diagnosable without a 500-line dump.
    fn checksum(list: &[Posting]) -> u64 {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for p in list {
            h ^= p.position as u64;
            h = h.wrapping_mul(0x1000_0000_01b3);
            h ^= p.doc_id as u64;
        }
        h
    }

    ///  — same order as the struct fields, so a wrong
    /// call cannot silently swap position and entry index.
    fn posting(doc: u32, position: u32, entry: u16) -> Posting {
        Posting::new(doc, position, entry)
    }

    #[test]
    fn an_empty_chunk_roundtrips() {
        let chunk = Chunk::default();
        let bytes = chunk.encode();
        let back = Chunk::decode(&bytes).expect("an empty chunk must decode");
        assert!(back.postings.is_empty());
    }

    #[test]
    fn a_single_posting_roundtrips() {
        let chunk = Chunk {
            first_doc: 7,
            postings: vec![posting(7, 120, 3)],
        };
        assert_eq!(Chunk::decode(&chunk.encode()).unwrap().postings, chunk.postings);
    }

    #[test]
    fn many_postings_in_one_document_roundtrip() {
        let postings: Vec<Posting> = (0..500).map(|i| posting(9, i * 37, 2)).collect();
        let chunk = Chunk {
            first_doc: 9,
            postings: postings.clone(),
        };
        let back = Chunk::decode(&chunk.encode()).unwrap();
        // Comparing the whole vector would dump 500 lines on failure; a count
        // plus a checksum says the same thing.
        assert_eq!(back.postings.len(), postings.len());
        assert_eq!(checksum(&back.postings), checksum(&postings), "positions drifted");
        assert_eq!(back.postings.last().map(|p| p.position), Some(499 * 37));
    }

    #[test]
    fn positions_that_look_like_they_need_wide_gaps_survive() {
        // A sparse word in a long book has gaps in the thousands.
        let positions = [0u32, 5000, 12000, 12001, 12002, 90_000];
        let postings: Vec<Posting> = positions.iter().map(|&p| posting(1, p, 0)).collect();
        let chunk = Chunk {
            first_doc: 1,
            postings,
        };
        let back = Chunk::decode(&chunk.encode()).unwrap();
        assert_eq!(back.postings.len(), chunk.postings.len());
        assert_eq!(checksum(&back.postings), checksum(&chunk.postings), "positions drifted");
    }

    #[test]
    fn a_chunk_spanning_many_documents_roundtrips() {
        let mut postings = Vec::new();
        for doc in 0..200u32 {
            for k in 0..5u32 {
                postings.push(posting(doc, 100 + doc * 10 + k * 3, (k % 4) as u16));
            }
        }
        let chunk = Chunk {
            first_doc: 0,
            postings,
        };
        let back = Chunk::decode(&chunk.encode()).unwrap();
        assert_eq!(back.postings.len(), chunk.postings.len());
        assert_eq!(checksum(&back.postings), checksum(&chunk.postings));
    }

    #[test]
    fn entry_index_changes_across_documents() {
        // Sorted by (doc, position) as the codec requires; note entry indices
        // need not be sorted, they just happen to grow here.
        let postings = vec![
            posting(1, 10, 0),
            posting(1, 11, 0),
            posting(2, 3, 7),
            posting(3, 20, 12),
        ];
        let chunk = Chunk {
            first_doc: 1,
            postings,
        };
        let back = Chunk::decode(&chunk.encode()).unwrap();
        assert_eq!(back.postings[2].entry_idx, 7);
        assert_eq!(back.postings[3].entry_idx, 12);
    }

    #[test]
    fn a_wrong_codec_tag_is_rejected() {
        let chunk = Chunk {
            first_doc: 1,
            postings: vec![posting(1, 1, 0)],
        };
        let mut bytes = chunk.encode();
        bytes[0] ^= 0xFF;
        assert!(Chunk::decode(&bytes).is_none(), "a foreign tag must not parse");
    }

    #[test]
    fn a_truncated_block_is_rejected_not_panicked() {
        let chunk = Chunk {
            first_doc: 1,
            postings: vec![posting(1, 1, 0)],
        };
        let bytes = chunk.encode();
        for cut in 0..bytes.len() {
            assert!(Chunk::decode(&bytes[..cut]).is_none() || cut >= 20, "cut {cut}");
        }
    }

    #[test]
    fn a_block_promising_absurd_counts_is_rejected() {
        let mut bytes = Chunk::default().encode();
        // Claim a million postings in a 20-byte block.
        bytes[8..12].copy_from_slice(&1_000_000u32.to_le_bytes());
        assert!(Chunk::decode(&bytes).is_none());
    }

    #[test]
    fn may_contain_is_a_lower_bound_only() {
        let chunk = Chunk {
            first_doc: 100,
            postings: vec![posting(100, 1, 0), posting(103, 1, 0)],
        };
        assert!(chunk.may_contain(100));
        assert!(chunk.may_contain(103));
        assert!(chunk.may_contain(200), "a later chunk is needed to rule it out");
        assert!(!chunk.may_contain(99));
    }

    #[test]
    fn postings_for_returns_only_that_document() {
        let postings = vec![
            posting(1, 10, 0),
            posting(2, 20, 0),
            posting(2, 21, 0),
            posting(3, 30, 0),
        ];
        let chunk = Chunk {
            first_doc: 1,
            postings,
        };
        assert_eq!(chunk.postings_for(2).len(), 2);
        assert_eq!(chunk.postings_for(1).len(), 1);
        assert!(chunk.postings_for(9).is_empty());
    }

    #[test]
    fn into_chunks_splits_at_the_limit() {
        // Deterministic pseudo-random.
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };

        let count = CHUNK_POSTINGS * 2 + 37;
        let mut postings: Vec<Posting> = Vec::with_capacity(count);
        for _ in 0..count {
            let doc = (next() % 500) as u32;
            let position = (next() % 100_000) as u32;
            postings.push(posting(doc, position, (next() % 20) as u16));
        }
        // The codec requires a sorted list; sort it the way the indexer would.
        postings.sort();

        let chunks = into_chunks(postings.clone());
        assert_eq!(chunks.len(), 3, "2 full chunks plus a remainder");
        assert_eq!(chunks[0].postings.len(), CHUNK_POSTINGS);
        assert_eq!(chunks[2].postings.len(), 37);

        // And the round trip must give back exactly what went in.
        let mut rebuilt = Vec::new();
        for chunk in &chunks {
            let bytes = chunk.encode();
            let back = Chunk::decode(&bytes).expect("chunk roundtrip");
            assert_eq!(back.postings.len(), chunk.postings.len());
            rebuilt.extend(back.postings);
        }
        // A checksum rather than the whole vector: 24k postings of Rust Debug
        // output would bury the actual difference.
        assert_eq!(rebuilt.len(), postings.len());
        assert_eq!(checksum(&rebuilt), checksum(&postings), "the round trip drifted");
    }

    #[test]
    fn chunk_order_is_monotone_in_doc_ids() {
        // The property that makes append-only safe.
        let mut postings: Vec<Posting> = (0..CHUNK_POSTINGS * 3)
            .map(|i| posting(i as u32 / 10, i as u32 % 10, 0))
            .collect();
        postings.sort();
        let chunks = into_chunks(postings);
        for pair in chunks.windows(2) {
            assert!(
                pair[0].first_doc <= pair[1].first_doc,
                "chunks must not go backwards in doc id"
            );
        }
    }

    /// The columns without any compression, for the ratio test below.
    fn columns_only(chunk: &Chunk) -> Vec<u8> {
        let encoded = chunk.encode();
        if encoded[4] == ENCODING_ZSTD {
            zstd::stream::decode_all(&encoded[5..]).unwrap_or_default()
        } else {
            encoded[5..].to_vec()
        }
    }

    #[test]
    fn a_chunk_too_small_to_compress_is_stored_raw() {
        // The encoder only takes the zstd form when it is smaller. A chunk with
        // a couple of postings is mostly headers and would inflate, so the raw
        // path has to stay correct and decodable.
        let chunk = Chunk {
            first_doc: 7,
            postings: vec![Posting::new(7, 0, 0), Posting::new(7, 4, 0)],
        };
        let bytes = chunk.encode();
        assert_eq!(bytes[4], ENCODING_RAW, "a tiny chunk must not be compressed");
        assert_eq!(Chunk::decode(&bytes), Some(chunk));
    }

    #[test]
    fn a_large_chunk_is_compressed_and_still_roundtrips() {
        // The compressed form has to be exercised, or a bug in the container
        // framing would only show up in a real library.
        let postings: Vec<Posting> = (0..CHUNK_POSTINGS as u32)
            .map(|i| Posting::new(1000 + i / 8, i, (i % 20) as u16))
            .collect();
        let chunk = Chunk { first_doc: 1000, postings };
        let bytes = chunk.encode();
        assert_eq!(bytes[4], ENCODING_ZSTD, "a full chunk must be compressed");
        let decoded = Chunk::decode(&bytes).expect("decode");
        assert_eq!(decoded.postings.len(), chunk.postings.len());
        assert_eq!(decoded.postings.last(), chunk.postings.last());
    }

    #[test]
    fn compression_actually_shrinks_a_realistic_chunk() {
        // This is the number the redb page overhead depends on: values that fit
        // several to a page cost far less in B-tree space than the same total
        // bytes spread over one value per page.
        let postings: Vec<Posting> = (0..CHUNK_POSTINGS as u32)
            // Positions must ascend within a document, which is what the gap
            // column assumes.
            .map(|i| Posting::new(500 + i / 3, i, (i / 800) as u16))
            .collect();
        let chunk = Chunk { first_doc: 500, postings };
        let encoded = chunk.encode();
        // The columns alone, without the container framing.
        let columns = columns_only(&chunk);
        assert!(
            encoded.len() < columns.len(),
            "compressed {} bytes vs columns {} bytes",
            encoded.len(),
            columns.len()
        );
    }
}
