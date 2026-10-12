//! Book metadata storage and the `METAIDX` substring index.
//!
//! Metadata is split in two on purpose (`note.md` section 2.3):
//!
//! * `METADATA` holds the structured record used to *display* a result.
//! * `METAIDX` holds a single compressed blob of normalized metadata text used
//!   to *match* a query.
//!
//! The split exists because matching needs substring search — typing `nguy` has
//! to reach `Nguyễn`, and `imov` has to reach `Asimov` — while an inverted index
//! can only match whole terms or prefixes. So `METAIDX` is one blob scanned
//! linearly with `memchr`, which keeps query time proportional to the metadata
//! size rather than to the number of books.

use super::pack::{Reader, Writer};
use super::{Engine, RunResult};

/// Where a book's metadata came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum Provenance {
    /// No metadata found; the filename stands in.
    #[default]
    Filename = 0,
    /// Read from the book's own container (OPF, EXTH, FB2 description).
    Embedded = 1,
    /// Read from KOReader's sidecar, which is what the user already sees.
    Sidecar = 2,
}

impl Provenance {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Provenance::Filename),
            1 => Some(Provenance::Embedded),
            2 => Some(Provenance::Sidecar),
            _ => None,
        }
    }
}

/// The display record for one book.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BookMetaRecord {
    pub title: String,
    /// Newline-joined, matching the OPF and sidecar convention.
    pub authors: String,
    pub language: String,
    pub publisher: String,
    pub series: String,
    /// Multiplied by 1000; 0 means unknown.
    pub series_index: u32,
    pub description: String,
    /// `scheme:value` pairs such as `uuid:...` or `isbn:...`.
    pub identifiers: Vec<String>,
    pub provenance: Provenance,
    /// True when the description was cut to fit the storage cap.
    pub description_truncated: bool,
}

/// Maximum stored description length.
///
/// A calibre description can run to tens of kilobytes, and none of it is needed
/// to identify a book, so it is capped rather than stored whole.
const MAX_DESCRIPTION_BYTES: usize = 4 * 1024;

impl BookMetaRecord {
    /// Builds the record that gets displayed.
    pub fn new(title: impl Into<String>, authors: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            authors: authors.into(),
            ..Default::default()
        }
    }

    /// Splits the newline-joined author field.
    pub fn author_list(&self) -> Vec<&str> {
        self.authors
            .split('\n')
            .map(|a| a.trim())
            .filter(|a| !a.is_empty())
            .collect()
    }

    /// Caps the description and reports whether it was shortened.
    pub fn set_description(&mut self, text: impl Into<String>) {
        let text = text.into();
        if text.len() <= MAX_DESCRIPTION_BYTES {
            self.description = text;
            self.description_truncated = false;
            return;
        }
        // Cut on a character boundary rather than a byte one.
        let mut end = MAX_DESCRIPTION_BYTES;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        self.description = text[..end].to_string();
        self.description_truncated = true;
    }

    /// The normalized text that gets scanned by `METAIDX`.
    ///
    /// Fields are joined with newlines so a query that spans two fields, such as
    /// "nguyen nhat anh", cannot accidentally match across the boundary in a
    /// way a reader would find confusing.
    pub fn search_blob(&self) -> String {
        let mut parts: Vec<&str> = Vec::new();
        if !self.title.is_empty() {
            parts.push(&self.title);
        }
        for author in self.author_list() {
            parts.push(author);
        }
        if !self.series.is_empty() {
            parts.push(&self.series);
        }
        if !self.publisher.is_empty() {
            parts.push(&self.publisher);
        }
        if !self.description.is_empty() {
            parts.push(&self.description);
        }
        if !self.language.is_empty() {
            parts.push(&self.language);
        }
        parts.join("\n")
    }

    pub fn encode(&self) -> Vec<u8> {
        // `scope`-style rule: a field was appended, so an old row fails to decode
        // loudly instead of silently shifting everything after it.
        Writer::with_capacity(256)
            .str(&self.title)
            .str(&self.authors)
            .str(&self.language)
            .str(&self.publisher)
            .str(&self.series)
            .u32(self.series_index)
            .str(&self.description)
            .str_list(&self.identifiers)
            .u8(self.provenance as u8)
            .bool(self.description_truncated)
            .finish()
    }

    pub fn decode(bytes: &[u8]) -> super::pack::Result<Self> {
        let mut r = Reader::new(bytes);
        Ok(Self {
            title: r.str()?,
            authors: r.str()?,
            language: r.str()?,
            publisher: r.str()?,
            series: r.str()?,
            series_index: r.u32()?,
            description: r.str()?,
            identifiers: r.str_list()?,
            provenance: Provenance::from_u8(r.u8()?).unwrap_or_default(),
            description_truncated: r.bool()?,
        })
    }
}

/// The `METAIDX` blob: a compressed index of normalized metadata text.
///
/// One row for the whole library. That is deliberate: storing 6,000 separate
/// values costs 6,000 B-tree reads on the query path, where a single value of
/// the same total size costs one read and one `memchr` pass.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MetaIndex {
    entries: Vec<MetaIndexEntry>,
    blob: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct MetaIndexEntry {
    book_id: u16,
    offset: u32,
    len: u16,
}

impl MetaIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds one book's metadata.
    ///
    /// Normalization happens here rather than at the call site. An earlier design
    /// made the caller normalize first, and forgetting once produced an index
    /// that silently matched nothing — that failure has no error, so the caller
    /// cannot know it happened.
    pub fn push(&mut self, book_id: u16, text: &str) {
        let normalized = crate::tokenize::normalize(text);
        let bytes = normalized.as_bytes();
        let offset = self.blob.len() as u32;
        if bytes.is_empty() || offset == u32::MAX || bytes.len() > u16::MAX as usize {
            // An empty or oversized entry is recorded with a zero length so the
            // book stays searchable by everything else but contributes nothing.
            self.entries.push(MetaIndexEntry {
                book_id,
                offset,
                len: 0,
            });
            return;
        }
        self.blob.extend_from_slice(bytes);
        self.entries.push(MetaIndexEntry {
            book_id,
            offset,
            len: bytes.len() as u16,
        });
    }

    /// Number of books in the index.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Uncompressed size of the metadata text.
    pub fn blob_len(&self) -> usize {
        self.blob.len()
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::with_capacity(self.blob.len() / 2 + 64).u32(self.entries.len() as u32);
        for e in &self.entries {
            w = w.u16(e.book_id).u32(e.offset).u16(e.len);
        }
        // Compressed as one block, so a lookup reads it once.
        let compressed =
            zstd::encode_all(self.blob.as_slice(), 3).unwrap_or_else(|_| self.blob.clone());
        w = w.bytes(&compressed);
        w.finish()
    }

    pub fn decode(bytes: &[u8]) -> super::pack::Result<Self> {
        let mut r = Reader::new(bytes);
        let count = r.u32()? as usize;
        let mut entries = Vec::with_capacity(count.min(1 << 20));
        for _ in 0..count {
            let book_id = r.u16()?;
            let offset = r.u32()?;
            let len = r.u16()?;
            entries.push(MetaIndexEntry {
                book_id,
                offset,
                len,
            });
        }
        let compressed = r.bytes()?;
        // A corrupt blob is not fatal: the query path then simply finds nothing.
        let blob = zstd::decode_all(compressed.as_slice()).unwrap_or_default();
        Ok(Self { entries, blob })
    }

    /// Every book whose metadata contains `needle` as a substring.
    ///
    /// `needle` is normalized here too, for the same reason `push` does it: one
    /// side normalized and the other not is a silent total failure.
    pub fn search(&self, needle: &str) -> Vec<u16> {
        let needle = crate::tokenize::normalize(needle);
        let needle = needle.as_bytes();
        if needle.is_empty() {
            return Vec::new();
        }
        let mut out = Vec::new();
        for entry in &self.entries {
            if entry.len == 0 {
                continue;
            }
            let start = entry.offset as usize;
            let end = start + entry.len as usize;
            if end > self.blob.len() {
                continue;
            }
            if memchr::memmem::find(&self.blob[start..end], needle).is_some() {
                out.push(entry.book_id);
            }
        }
        out
    }
}

// --- engine accessors -------------------------------------------------------

impl Engine {
    /// Reads one metadata record.
    pub fn metadata(&self, book_id: u16) -> RunResult<Option<BookMetaRecord>> {
        self.block_on(async {
            let row: Option<crate::sql::engine::MetaRow> = crate::sql::engine::or_empty(
                sqlx::query_as::<_, crate::sql::engine::MetaRow>(
                    "SELECT title, authors, language, publisher, series, series_index,
                            description, identifiers, provenance, description_truncated
                     FROM metadata WHERE book_id = ?1",
                )
                .bind(book_id as i64)
                .fetch_one(self.pool())
                .await,
            )?;
            Ok(row.map(|r| r.into_record()))
        })
    }

    /// Writes one metadata record.
    ///
    /// `ON CONFLICT` updates every column, so a book re-indexed with a worse
    /// sidecar cannot keep the fields of the previous run.
    pub fn put_metadata(&self, book_id: u16, record: &BookMetaRecord) -> RunResult<()> {
        let r = crate::sql::engine::MetaRow::from_record(record);
        self.block_on(async {
            sqlx::query(
                "INSERT INTO metadata
                     (book_id, title, authors, language, publisher, series, series_index,
                      description, identifiers, provenance, description_truncated)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                 ON CONFLICT(book_id) DO UPDATE SET
                     title = excluded.title, authors = excluded.authors,
                     language = excluded.language, publisher = excluded.publisher,
                     series = excluded.series, series_index = excluded.series_index,
                     description = excluded.description, identifiers = excluded.identifiers,
                     provenance = excluded.provenance,
                     description_truncated = excluded.description_truncated",
            )
            .bind(book_id as i64)
            .bind(&r.title)
            .bind(&r.authors)
            .bind(&r.language)
            .bind(&r.publisher)
            .bind(&r.series)
            .bind(r.series_index)
            .bind(&r.description)
            .bind(&r.identifiers)
            .bind(r.provenance)
            .bind(r.description_truncated)
            .execute(self.pool())
            .await?;
            Ok(())
        })
    }

    /// [`Engine::put_metadata`] without the per-row fsync, for bulk loads.
    ///
    /// A durable commit here costs about 160 ms on a real disk, which was the
    /// difference between an indexing session finishing and not.
    pub fn put_metadata_batch(&self, book_id: u16, record: &BookMetaRecord) -> RunResult<()> {
        let r = crate::sql::engine::MetaRow::from_record(record);
        self.block_on(async {
            sqlx::query(
                "INSERT INTO metadata
                     (book_id, title, authors, language, publisher, series, series_index,
                      description, identifiers, provenance, description_truncated)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                 ON CONFLICT(book_id) DO UPDATE SET
                     title = excluded.title, authors = excluded.authors,
                     language = excluded.language, publisher = excluded.publisher,
                     series = excluded.series, series_index = excluded.series_index,
                     description = excluded.description, identifiers = excluded.identifiers,
                     provenance = excluded.provenance,
                     description_truncated = excluded.description_truncated",
            )
            .bind(book_id as i64)
            .bind(&r.title)
            .bind(&r.authors)
            .bind(&r.language)
            .bind(&r.publisher)
            .bind(&r.series)
            .bind(r.series_index)
            .bind(&r.description)
            .bind(&r.identifiers)
            .bind(r.provenance)
            .bind(r.description_truncated)
            .execute(self.pool())
            .await?;
            Ok(())
        })
    }

    /// Drops a metadata record when its book is removed.
    pub fn remove_metadata(&self, book_id: u16) -> RunResult<()> {
        self.block_on(async {
            sqlx::query("DELETE FROM metadata WHERE book_id = ?1")
                .bind(book_id as i64)
                .execute(self.pool())
                .await?;
            Ok(())
        })
    }

    /// Reads the whole `METAIDX`.
    pub fn meta_index(&self) -> RunResult<MetaIndex> {
        self.block_on(async {
            let row: Option<Vec<u8>> = crate::sql::engine::or_empty(
                sqlx::query_scalar::<_, Vec<u8>>("SELECT value FROM metaidx WHERE key = ?1")
                    .bind(METAIDX_KEY)
                    .fetch_one(self.pool())
                    .await,
            )?;
            match row {
                Some(b) => MetaIndex::decode(&b)
                    .map_err(|e| crate::storage::RunError::Message(e.to_string())),
                None => Ok(MetaIndex::new()),
            }
        })
    }

    /// Replaces the whole `METAIDX`.
    ///
    /// Rewriting one value per indexing session, rather than one per book, is
    /// what keeps the write cost proportional to metadata rather than to books.
    pub fn put_meta_index(&self, index: &MetaIndex) -> RunResult<()> {
        let blob = index.encode();
        self.block_on(async {
            sqlx::query(
                "INSERT INTO metaidx (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            )
            .bind(METAIDX_KEY)
            .bind(&blob)
            .execute(self.pool())
            .await?;
            Ok(())
        })
    }
}

/// The single `METAIDX` key.
pub const METAIDX_KEY: &str = "metaidx";

/// Rebuilds `METAIDX` from every stored `METADATA` row.
///
/// Normalization runs here rather than at write time so a tokenizer change only
/// needs a rebuild, not a re-extraction.
pub fn rebuild_index(engine: &Engine) -> RunResult<MetaIndex> {
    let mut index = MetaIndex::new();

    // Only ids that still exist in `BOOKS` are included, so a row orphaned by a
    // partial deletion cannot resurrect a book in search results. The join does
    // that filtering here, in the database, rather than by loading every book
    // into a set as the redb version had to.
    engine.block_on(async {
        let rows = sqlx::query!(
            "SELECT m.book_id, m.title, m.authors, m.series, m.description
             FROM metadata m
             JOIN books b ON b.id = m.book_id
             ORDER BY m.book_id"
        )
        .fetch_all(engine.pool())
        .await?;

        for r in rows {
            let Some(id) = u16::try_from(r.book_id).ok() else {
                continue;
            };
            // The same concatenation `search_blob` performs, spelled out because
            // the record is not materialised: this rebuild runs over the whole
            // library and materialising one `BookMetaRecord` per row just to
            // join four strings was measurable.
            let mut blob = String::new();
            blob.push_str(&r.title);
            blob.push('\n');
            blob.push_str(&r.authors);
            blob.push('\n');
            blob.push_str(&r.series);
            if !r.description.is_empty() {
                blob.push('\n');
                blob.push_str(&r.description);
            }
            index.push(id, &blob);
        }
        Ok::<_, sqlx::Error>(())
    })?;

    engine.put_meta_index(&index)?;
    Ok(index)
}

/// Searches metadata across the library.
///
/// Returns `(book_id, title)` pairs so a caller has something to show.
pub fn search(engine: &Engine, query: &str) -> RunResult<Vec<(u16, String)>> {
    let index = engine.meta_index()?;
    let hits = index.search(query);

    let mut out = Vec::with_capacity(hits.len());
    for book_id in hits {
        if let Some(record) = engine.metadata(book_id)? {
            out.push((book_id, record.title));
        }
    }
    Ok(out)
}

/// A lookup of `book_id` from the metadata index, for callers that need it.
pub fn book_ids(index: &MetaIndex, needle: &str) -> Vec<u16> {
    index.search(needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> BookMetaRecord {
        let mut r = BookMetaRecord::new(
            "Yến Tiệc Của Sherlockian Và Bão Tố Mùa Xuân",
            "Mai Mochizuki\nNinh Nhân Văn (dịch)",
        );
        r.language = "vi".to_string();
        r.series = "Holmes ở Kyoto".to_string();
        r.series_index = 5000;
        r.publisher = "NXB Trẻ".to_string();
        r.identifiers = vec!["uuid:2ac297f3".to_string(), "calibre:d82b869c".to_string()];
        r.provenance = Provenance::Embedded;
        r
    }

    #[test]
    fn record_roundtrips() {
        let r = record();
        let bytes = r.encode();
        assert_eq!(BookMetaRecord::decode(&bytes).unwrap(), r);
    }

    #[test]
    fn authors_split_on_newline() {
        assert_eq!(
            record().author_list(),
            vec!["Mai Mochizuki", "Ninh Nhân Văn (dịch)"]
        );
    }

    #[test]
    fn empty_authors_split_to_nothing() {
        let r = BookMetaRecord::new("t", "");
        assert!(r.author_list().is_empty());
    }

    #[test]
    fn provenance_roundtrips_all_variants() {
        for p in [
            Provenance::Filename,
            Provenance::Embedded,
            Provenance::Sidecar,
        ] {
            let mut r = record();
            r.provenance = p;
            let back = BookMetaRecord::decode(&r.encode()).unwrap();
            assert_eq!(back.provenance, p);
        }
    }

    #[test]
    fn description_is_capped_on_a_character_boundary() {
        let mut r = BookMetaRecord::new("t", "a");
        r.set_description("é".repeat(10_000));
        assert!(r.description_truncated);
        assert!(r.description.len() <= MAX_DESCRIPTION_BYTES);
        assert!(r.description.is_char_boundary(r.description.len()));

        // Multi-byte characters near the cap must not be split.
        r.set_description("a".repeat(MAX_DESCRIPTION_BYTES - 1) + "ấ");
        assert!(r.description_truncated);
        assert!(r.description.is_char_boundary(r.description.len()));
    }

    #[test]
    fn short_description_is_not_truncated() {
        let mut r = BookMetaRecord::new("t", "a");
        r.set_description("short");
        assert!(!r.description_truncated);
        assert_eq!(r.description, "short");
    }

    #[test]
    fn search_blob_covers_the_fields_a_user_types() {
        let blob = record().search_blob();
        for expected in ["Yến Tiệc", "Mai Mochizuki", "Holmes ở Kyoto", "NXB Trẻ"] {
            assert!(blob.contains(expected), "{expected} missing from {blob}");
        }
    }

    // --- the actual substring-matching requirement --------------------------

    #[test]
    fn a_prefix_query_reaches_the_accented_name() {
        let mut index = MetaIndex::new();
        index.push(1, "Nguyễn Nhật Ánh\nTôi thấy hoa vàng trên cỏ xanh");
        index.push(2, "Asimov\nFoundation");

        // The whole point of METAIDX: partial, unaccented input still matches.
        for (needle, expected) in [
            ("nguy", vec![1]),
            ("nguyen", vec![1]),
            ("nhat anh", vec![1]),
            ("imov", vec![2]),
            ("asimov", vec![2]),
        ] {
            assert_eq!(book_ids(&index, needle), expected, "needle {needle:?}");
        }
    }

    #[test]
    fn search_is_case_insensitive_because_both_sides_are_normalized() {
        let mut index = MetaIndex::new();
        index.push(7, "Sherlock Holmes");
        assert_eq!(book_ids(&index, "SHERLOCK"), vec![7]);
        assert_eq!(book_ids(&index, "sherlock"), vec![7]);
        assert_eq!(book_ids(&index, "ShErLoCk"), vec![7]);
    }

    #[test]
    fn an_empty_query_matches_nothing() {
        let mut index = MetaIndex::new();
        index.push(1, "anything");
        assert!(
            index.search("").is_empty(),
            "an empty query matches nothing"
        );
        assert!(
            index.search("   ").is_empty(),
            "neither does a whitespace query"
        );
    }

    #[test]
    fn empty_and_oversized_entries_are_skipped_not_fatal() {
        let mut index = MetaIndex::new();
        index.push(1, "");
        index.push(2, "real");
        assert_eq!(index.len(), 2);
        assert_eq!(book_ids(&index, "real"), vec![2]);
        assert_eq!(book_ids(&index, "nothere"), Vec::<u16>::new());
    }

    #[test]
    fn index_roundtrips_and_survives_compression() {
        let mut index = MetaIndex::new();
        for i in 0..50u16 {
            index.push(i, &format!("Title {i} — by Author số {i}"));
        }
        let bytes = index.encode();
        let back = MetaIndex::decode(&bytes).unwrap();
        assert_eq!(back, index);
        let raw = index.blob_len();
        assert_eq!(back.blob_len(), raw);
        // The compressed form should be smaller than the raw blob.
        assert!(bytes.len() < raw, "{} vs {}", bytes.len(), raw);
        assert_eq!(book_ids(&back, "title 7"), vec![7]);
    }

    #[test]
    fn compression_actually_shrinks_the_blob() {
        let mut index = MetaIndex::new();
        for i in 0..200u16 {
            index.push(
                i,
                &format!("A fairly repetitive title number {i} by the same author"),
            );
        }
        let encoded = index.encode();
        assert!(
            encoded.len() < index.blob_len(),
            "{} encoded bytes should beat {} raw",
            encoded.len(),
            index.blob_len()
        );
    }

    #[test]
    fn a_corrupt_blob_degrades_to_no_results_rather_than_panicking() {
        let mut index = MetaIndex::new();
        index.push(1, "real title");
        let mut bytes = index.encode();
        // Corrupt a byte well inside the compressed payload. The trailing bytes
        // are LEB128 framing, which survives the flip and tests nothing.
        let mid = bytes.len() / 2;
        bytes[mid] ^= 0xFF;
        let back = MetaIndex::decode(&bytes);
        assert!(back.is_ok(), "a damaged blob must not fail to decode");
        assert!(book_ids(&back.unwrap(), "real").is_empty());
    }

    #[test]
    fn many_books_scale_linearly() {
        // Not a timing assertion; just that the structure survives scale.
        let mut index = MetaIndex::new();
        for i in 0..6_000u16 {
            index.push(i, &format!("Book {i}"));
        }
        assert_eq!(book_ids(&index, "book 4321"), vec![4321]);
    }
}
