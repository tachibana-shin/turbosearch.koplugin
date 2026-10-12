//! The term dictionary.
//!
//! Per `note.md` section 2 the dictionary is **one key-value pair**, not one
//! pair per term: a million separate keys would cost 30–50 MB in redb page
//! overhead alone. Stored as a single front-coded, zstd-compressed blob it is
//! roughly 400 KB for a 70 000-term library, and it lives in RAM while the
//! engine is open (`note.md` section 5), which also turns prefix search into a
//! binary search in memory.
//!
//! During an indexing session the changes do **not** go into this blob: that
//! would mean rewriting and recompressing the whole dictionary per book. They go
//! into `DICT_DELTA` instead, and are merged once at session close.

use crate::codec::front;
use crate::storage::{Engine, RunResult};

/// The single key holding the dictionary blob.
pub const DICT_KEY: &[u8] = b"dict";

/// Positions were capped for this term, so its list is incomplete.
pub const FLAG_PARTIAL: u16 = 1;
/// A chunk was cut short, so its tail is missing.
pub const FLAG_TRUNCATED: u16 = 2;

/// Per-term statistics, kept small on purpose: they are read on every query.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DictEntry {
    /// Documents containing the term.
    pub df: u32,
    /// Total occurrences across the library.
    pub tf: u32,
    /// Chunks stored in `POSTINGS` for this term.
    pub chunk_count: u32,
    pub flags: u16,
}

impl DictEntry {
    pub fn is_partial(&self) -> bool {
        self.flags & FLAG_PARTIAL != 0
    }
}

/// The whole dictionary, in RAM.
#[derive(Debug, Clone, Default)]
pub struct Dict {
    /// Sorted, byte-wise.
    terms: Vec<Vec<u8>>,
    entries: Vec<DictEntry>,
}

impl Dict {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.terms.len()
    }

    pub fn is_empty(&self) -> bool {
        self.terms.is_empty()
    }

    /// The dictionary in order, for iteration and for tests.
    pub fn terms(&self) -> &[Vec<u8>] {
        &self.terms
    }

    pub fn entries(&self) -> &[DictEntry] {
        &self.entries
    }

    pub fn term(&self, i: usize) -> &[u8] {
        &self.terms[i]
    }

    pub fn entry(&self, i: usize) -> &DictEntry {
        &self.entries[i]
    }

    /// Exact lookup.
    pub fn get(&self, term: &[u8]) -> Option<&DictEntry> {
        let i = self.lower_bound(term);
        if i < self.terms.len() && self.terms[i] == term {
            Some(&self.entries[i])
        } else {
            None
        }
    }

    /// All terms beginning with `prefix`, in dictionary order.
    ///
    /// Terms are byte-sorted, so they form a contiguous run.
    pub fn prefix_range(&self, prefix: &[u8]) -> (usize, usize) {
        let start = self.lower_bound(prefix);
        let end = match successor(prefix) {
            Some(upper) => self.lower_bound(&upper),
            // The prefix is all 0xFF, so the run extends to the last term.
            None => self.terms.len(),
        };
        (start, end.max(start))
    }

    pub fn has_prefix(&self, prefix: &[u8]) -> bool {
        let (start, end) = self.prefix_range(prefix);
        start < end
    }

    /// Index of the first term that is not less than `term`.
    fn lower_bound(&self, term: &[u8]) -> usize {
        let mut lo = 0usize;
        let mut hi = self.terms.len();
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.terms[mid].as_slice() < term {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    }

    /// Adds a term, or updates it in place. Used when ingesting the delta.
    pub fn insert(&mut self, term: &[u8], entry: DictEntry) {
        let i = self.lower_bound(term);
        if i < self.terms.len() && self.terms[i] == term {
            // Keep the flags that are still relevant; a term that was partial
            // stays partial even after a later book refreshes the counts.
            self.entries[i] = entry;
            return;
        }
        self.terms.insert(i, term.to_vec());
        self.entries.insert(i, entry);
    }

    /// Applies one delta row, adding to the existing counts.
    pub fn add(&mut self, term: &[u8], df: u32, tf: u32, chunks: u32) {
        let i = self.lower_bound(term);
        if i < self.terms.len() && self.terms[i] == term {
            let e = &mut self.entries[i];
            e.df += df;
            e.tf += tf;
            e.chunk_count += chunks;
        } else {
            self.terms.insert(i, term.to_vec());
            self.entries.insert(
                i,
                DictEntry {
                    df,
                    tf,
                    chunk_count: chunks,
                    flags: 0,
                },
            );
        }
    }

    pub fn set_flags(&mut self, term: &[u8], flags: u16) {
        if let Some(i) = self.lower_bound_ok(term) {
            self.entries[i].flags = flags;
        }
    }

    fn lower_bound_ok(&self, term: &[u8]) -> Option<usize> {
        let i = self.lower_bound(term);
        (i < self.terms.len() && self.terms[i] == term).then_some(i)
    }

    /// Mutable access to a term's entry, for setting flags.
    pub fn entry_for_mut(&mut self, term: &[u8]) -> Option<&mut DictEntry> {
        let i = self.lower_bound(term);
        if i < self.terms.len() && self.terms[i] == term {
            Some(&mut self.entries[i])
        } else {
            None
        }
    }

    /// Serialises to the on-disk blob.
    pub fn encode(&self) -> Vec<u8> {
        let refs: Vec<&[u8]> = self.terms.iter().map(|t| t.as_slice()).collect();
        let coded = front::encode(&refs);

        let mut stats = Vec::with_capacity(self.terms.len() * 4);
        for e in &self.entries {
            vbyte::write_leb128(&mut stats, e.df);
            vbyte::write_leb128(&mut stats, e.tf);
            vbyte::write_leb128(&mut stats, e.chunk_count);
            vbyte::write_leb128(&mut stats, e.flags as u32);
        }

        // One zstd block. The two length words let the decoder split the
        // sections without scanning.
        let mut raw = Vec::with_capacity(8 + coded.len() + stats.len());
        raw.extend_from_slice(&(coded.len() as u32).to_le_bytes());
        raw.extend_from_slice(&(stats.len() as u32).to_le_bytes());
        raw.extend_from_slice(&coded);
        raw.extend_from_slice(&stats);

        zstd::stream::encode_all(raw.as_slice(), ZSTD_LEVEL).unwrap_or(raw)
    }

    /// Parses the on-disk blob.
    pub fn decode(bytes: &[u8]) -> Result<Self, DictError> {
        if bytes.is_empty() {
            return Err(DictError::Empty);
        }
        let raw = if bytes.starts_with(&ZSTD_MAGIC) {
            zstd::stream::decode_all(bytes).map_err(|_| DictError::Decompress)?
        } else {
            // Written by a build with zstd disabled, or a truncated write.
            bytes.to_vec()
        };
        if raw.len() < 8 {
            return Err(DictError::Truncated);
        }
        let coded_len = u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]) as usize;
        let stats_len = u32::from_le_bytes([raw[4], raw[5], raw[6], raw[7]]) as usize;
        let coded = raw.get(8..8 + coded_len).ok_or(DictError::Truncated)?;
        let stats = raw
            .get(8 + coded_len..8 + coded_len + stats_len)
            .ok_or(DictError::Truncated)?;

        let terms = front::decode(coded).map_err(|_| DictError::Decode)?;

        let mut pos = 0usize;
        let mut entries = Vec::with_capacity(terms.len());
        for _ in &terms {
            let df = vbyte::read_leb128(stats, &mut pos).map_err(|_| DictError::Truncated)?;
            let tf = vbyte::read_leb128(stats, &mut pos).map_err(|_| DictError::Truncated)?;
            let chunk_count =
                vbyte::read_leb128(stats, &mut pos).map_err(|_| DictError::Truncated)?;
            let flags = vbyte::read_leb128(stats, &mut pos).map_err(|_| DictError::Truncated)?;
            entries.push(DictEntry {
                df,
                tf,
                chunk_count,
                flags: u16::try_from(flags).unwrap_or(0),
            });
        }

        Ok(Self { terms, entries })
    }
}

/// Zstd level for the dictionary blob.
///
/// High on purpose: this is written roughly once per indexing session, and the
/// dictionary is read on every engine open.
const ZSTD_LEVEL: i32 = 19;

const ZSTD_MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DictError {
    Empty,
    Truncated,
    Decompress,
    Decode,
}

impl std::fmt::Display for DictError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DictError::Empty => write!(f, "empty dictionary blob"),
            DictError::Truncated => write!(f, "truncated dictionary blob"),
            DictError::Decompress => write!(f, "dictionary blob failed to decompress"),
            DictError::Decode => write!(f, "dictionary blob failed to decode"),
        }
    }
}

impl std::error::Error for DictError {}

/// The smallest byte string strictly greater than everything starting with
/// `prefix`.
///
/// Returns `None` when the prefix is all `0xFF`, in which case the run reaches
/// the end of the dictionary.
fn successor(prefix: &[u8]) -> Option<Vec<u8>> {
    if prefix.is_empty() {
        // Every term starts with the empty prefix, so the range is the whole
        // dictionary. Returning `None` gives exactly that.
        return None;
    }
    let mut out = prefix.to_vec();
    while let Some(last) = out.pop() {
        if last != 0xFF {
            out.push(last + 1);
            return Some(out);
        }
        // A trailing 0xFF carries into the byte before it.
    }
    None
}

/// Reads the dictionary out of the engine, or an empty one when absent.
pub fn load(engine: &Engine) -> LoadResult<Dict> {
    // No table-exists branch any more: the schema is created on open, so an
    // empty dictionary means "nothing indexed", not "table missing".
    match engine.dict_blob()? {
        Some(b) => Dict::decode(&b)
            .map_err(|e| crate::storage::RunError::Message(format!("dictionary: {e}"))),
        None => Ok(Dict::new()),
    }
}

/// The result of loading.
pub type LoadResult<T> = std::result::Result<T, crate::storage::RunError>;

/// One row of the running session's changes.
pub type DeltaRow = (Vec<u8>, u32, u32, u32);

/// Writes the dictionary blob.
pub fn store(engine: &Engine, dict: &Dict) -> RunResult<()> {
    let blob = dict.encode();
    engine.write_dict_blob(&blob)
}

/// Reads `DICT_DELTA`, for resuming a session that never merged.
pub fn load_deltas(engine: &Engine) -> LoadResult<Vec<DeltaRow>> {
    engine.list_deltas()
}

/// Writes `DICT_DELTA` for the terms a session touched.
///
/// Overwritten wholesale on every close: a merge into `DICT` happens in the
/// same call, so the rows are only needed by an *interrupted* session.
pub fn store_deltas(engine: &Engine, rows: &[DeltaRow]) -> RunResult<()> {
    engine.replace_deltas(rows)
}

/// Drops `DICT_DELTA`. Called after a successful merge, so a resumed session
/// does not double-count the same chunks.
pub fn clear_deltas(engine: &Engine) -> RunResult<()> {
    engine.clear_deltas()
}

use crate::codec::vbyte;

/// Packs one delta row.
///
/// `DICT_DELTA` holds one key per touched term so a session can be resumed or
/// inspected without decoding the whole dictionary.
pub fn pack_delta(df: u32, tf: u32, chunks: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(12);
    vbyte::write_leb128(&mut out, df);
    vbyte::write_leb128(&mut out, tf);
    vbyte::write_leb128(&mut out, chunks);
    out
}

/// Unpacks one delta row.
pub fn unpack_delta(bytes: &[u8]) -> Result<(u32, u32, u32), DictError> {
    let mut pos = 0usize;
    let df = vbyte::read_leb128(bytes, &mut pos).map_err(|_| DictError::Truncated)?;
    let tf = vbyte::read_leb128(bytes, &mut pos).map_err(|_| DictError::Truncated)?;
    let chunks = vbyte::read_leb128(bytes, &mut pos).map_err(|_| DictError::Truncated)?;
    Ok((df, tf, chunks))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Dict {
        let mut d = Dict::new();
        d.insert(
            b"sherlock",
            DictEntry {
                df: 12,
                tf: 340,
                chunk_count: 1,
                flags: 0,
            },
        );
        d.insert(
            b"sherlockian",
            DictEntry {
                df: 2,
                tf: 9,
                chunk_count: 1,
                flags: 0,
            },
        );
        d.insert(
            b"sherries",
            DictEntry {
                df: 5,
                tf: 30,
                chunk_count: 1,
                flags: 0,
            },
        );
        d.insert(
            b"th\xc3\xa1m",
            DictEntry {
                df: 64,
                tf: 900,
                chunk_count: 2,
                flags: 0,
            },
        );
        d.insert(
            b"th\xc6\xb0",
            DictEntry {
                df: 3,
                tf: 20,
                chunk_count: 1,
                flags: FLAG_PARTIAL,
            },
        );
        d
    }

    #[test]
    fn roundtrip_keeps_every_term_and_stat() {
        let d = sample();
        let e = Dict::decode(&d.encode()).unwrap();
        assert_eq!(e.len(), d.len());
        for i in 0..d.len() {
            assert_eq!(e.term(i), d.term(i));
            assert_eq!(e.entry(i), d.entry(i));
        }
    }

    #[test]
    fn an_empty_dictionary_roundtrips() {
        let d = Dict::new();
        let e = Dict::decode(&d.encode()).unwrap();
        assert!(e.is_empty());
    }

    #[test]
    fn encoding_is_smaller_than_the_raw_terms_for_a_realistic_dictionary() {
        // Front coding plus zstd should land well under the raw byte count.
        let mut d = Dict::new();
        for family in [
            b"sher".as_slice(),
            b"th\xc3\xa1m".as_slice(),
            b"ch".as_slice(),
        ] {
            for i in 0..200u32 {
                let term = format!("{}{}", String::from_utf8_lossy(family), i);
                d.insert(
                    term.as_bytes(),
                    DictEntry {
                        df: i % 50 + 1,
                        tf: i * 3,
                        chunk_count: 1,
                        flags: 0,
                    },
                );
            }
        }
        let raw: usize = d.terms.iter().map(|t| t.len() + 4).sum();
        assert!(
            d.encode().len() < raw / 2,
            "{} bytes vs raw {raw}",
            d.encode().len()
        );
    }

    #[test]
    fn lookup_finds_a_term_and_reports_misses() {
        let d = sample();
        assert_eq!(d.get(b"sherlock").map(|e| e.df), Some(12));
        assert_eq!(d.get(b"sherlockiana"), None);
        assert_eq!(d.get(b"z"), None);
    }

    #[test]
    fn lookup_respects_byte_order_not_utf8_order() {
        let d = sample();
        // "tham" without diacritics sorts before "thám".
        assert_eq!(d.get(b"tham"), None);
        assert_eq!(d.get(b"th\xc3\xa1m").map(|e| e.df), Some(64));
    }

    #[test]
    fn prefix_range_returns_a_contiguous_run() {
        let d = sample();
        assert_eq!(d.prefix_range(b"sher"), (0, 3));
        // Both "sherlock" and "sherlockian" start with "sherlock".
        assert_eq!(d.prefix_range(b"sherlock"), (0, 2));
        assert_eq!(d.prefix_range(b"sherlocki"), (1, 2));
        assert_eq!(d.prefix_range(b"z"), (d.len(), d.len()));
        assert!(d.has_prefix(b"sher"));
        assert!(!d.has_prefix(b"z"));
        // Byte order: "tham" sorts after "th\xc3\xa1m" in UTF-8, so a
        // diacritic-insensitive prefix must be widened to the full term.
        assert!(d.has_prefix(b"th\xc3\xa1m"));
    }

    #[test]
    fn the_empty_prefix_covers_the_whole_dictionary() {
        let d = sample();
        assert_eq!(d.prefix_range(b""), (0, d.len()));
    }

    #[test]
    fn insert_keeps_the_dictionary_sorted() {
        let mut d = sample();
        d.insert(
            b"abyss",
            DictEntry {
                df: 1,
                tf: 4,
                chunk_count: 1,
                flags: 0,
            },
        );
        let mut sorted = d.terms().to_vec();
        sorted.sort();
        assert_eq!(&d.terms(), &sorted);
    }

    #[test]
    fn insert_updates_in_place_rather_than_duplicating() {
        let mut d = sample();
        d.insert(
            b"sherlock",
            DictEntry {
                df: 99,
                tf: 999,
                chunk_count: 3,
                flags: 0,
            },
        );
        assert_eq!(d.len(), 5);
        assert_eq!(d.get(b"sherlock").unwrap().df, 99);
    }

    #[test]
    fn add_accumulates_into_an_existing_term() {
        let mut d = sample();
        d.add(b"sherlock", 3, 40, 1);
        let e = d.get(b"sherlock").unwrap();
        assert_eq!((e.df, e.tf, e.chunk_count), (15, 380, 2));
    }

    #[test]
    fn add_creates_a_missing_term() {
        let mut d = sample();
        d.add(b"holmes", 7, 70, 1);
        assert_eq!(d.get(b"holmes").map(|e| (e.df, e.tf)), Some((7, 70)));
    }

    #[test]
    fn flags_survive_a_roundtrip() {
        let d = sample();
        let e = Dict::decode(&d.encode()).unwrap();
        // The flagged term is the last one in byte order.
        let partial = e.term(e.len() - 1);
        assert_eq!(partial, b"th\xc6\xb0".as_slice());
        assert!(e.get(partial).unwrap().is_partial());
    }

    #[test]
    fn a_corrupt_blob_is_reported_not_panicked_on() {
        assert_eq!(Dict::decode(&[]).unwrap_err(), DictError::Empty);
        assert!(Dict::decode(&[0, 0, 0]).is_err());
        // A valid zstd frame over nonsense.
        let junk = zstd::stream::encode_all(&[1u8, 2, 3, 4, 5][..], 1).unwrap();
        assert!(Dict::decode(&junk).is_err());
    }

    #[test]
    fn delta_rows_pack_and_unpack() {
        let b = pack_delta(12, 340, 2);
        assert_eq!(unpack_delta(&b).unwrap(), (12, 340, 2));
        assert_eq!(unpack_delta(&[]).unwrap_err(), DictError::Truncated);
        assert_eq!(unpack_delta(&[0x0C]).unwrap_err(), DictError::Truncated);
    }

    #[test]
    fn successor_of_a_plain_prefix_is_one_past_it() {
        assert_eq!(successor(b"sher"), Some(b"shes".to_vec()));
        assert_eq!(successor(b"a"), Some(b"b".to_vec()));
    }

    #[test]
    fn successor_of_a_trailing_ff_carries_into_the_previous_byte() {
        assert_eq!(successor(b"a\xff"), Some(b"b".to_vec()));
        assert_eq!(successor(b"\xff\xff"), None);
        assert_eq!(successor(b"\xff"), None);
    }
}
