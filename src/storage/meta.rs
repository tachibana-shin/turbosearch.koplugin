//! The single `META` row.
//!
//! Layout follows `note.md` section 2. Any version or config hash mismatch with
//! the running build means the file is stale and must be rebuilt from scratch.

use super::pack::{Reader, Writer};
use crate::storage::MAX_BOOKS;

/// Bumped whenever the packed layout of any table changes.
pub const SCHEMA_VERSION: u32 = 1;
/// Bumped when the tokenizer or normalizer changes.
pub const TOKENIZER_VERSION: u32 = 1;
/// Bumped when normalizer changes.
pub const NORMALIZER_VERSION: u32 = 1;
/// Bumped when the postings block codec changes.
pub const CODEC_VERSION: u32 = 1;

/// Lifecycle of the scanner, mirrored in the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum Status {
    #[default]
    Idle = 0,
    Scanning = 1,
    Indexing = 2,
    BudgetExceeded = 3,
}

impl Status {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Status::Idle),
            1 => Some(Status::Scanning),
            2 => Some(Status::Indexing),
            3 => Some(Status::BudgetExceeded),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Meta {
    pub schema_version: u32,
    pub tokenizer_version: u32,
    pub normalizer_version: u32,
    pub codec_version: u32,
    pub config_hash: u32,
    /// Next `book_id` to hand out. Monotone: ids are never reused.
    pub book_id_hwm: u32,
    /// Upper bound on `book_id` that may be issued for this file.
    pub book_id_limit: u32,
    /// Last partially processed path, for resuming an interrupted scan.
    pub scan_cursor: Option<String>,
    /// Bytes written to postings so far.
    pub db_bytes_written: u64,
    pub status: Status,
}

impl Meta {
    /// The `META` key, held as text so future metadata rows can live alongside.
    pub const KEY: &'static str = "meta";

    pub fn new() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            tokenizer_version: TOKENIZER_VERSION,
            normalizer_version: NORMALIZER_VERSION,
            codec_version: CODEC_VERSION,
            config_hash: 0,
            book_id_hwm: 0,
            book_id_limit: MAX_BOOKS,
            scan_cursor: None,
            db_bytes_written: 0,
            status: Status::Idle,
        }
    }

    /// True when this row was written by the currently running build.
    ///
    /// Any mismatch means the on-disk file must be discarded rather than
    /// interpreted with the wrong layout — see `note.md` section 2.
    pub fn is_compatible(&self) -> bool {
        self.schema_version == SCHEMA_VERSION
            && self.tokenizer_version == TOKENIZER_VERSION
            && self.normalizer_version == NORMALIZER_VERSION
            && self.codec_version == CODEC_VERSION
    }

    pub fn encode(&self) -> Vec<u8> {
        // `scan_cursor` is optional: an empty string means "no cursor".
        let cursor = self.scan_cursor.clone().unwrap_or_default();
        Writer::with_capacity(64)
            .u32(self.schema_version)
            .u32(self.tokenizer_version)
            .u32(self.normalizer_version)
            .u32(self.codec_version)
            .u32(self.config_hash)
            .u32(self.book_id_hwm)
            .u32(self.book_id_limit)
            .str(&cursor)
            .u64(self.db_bytes_written)
            .u8(self.status as u8)
            .finish()
    }

    pub fn decode(bytes: &[u8]) -> super::pack::Result<Self> {
        let mut r = Reader::new(bytes);
        let m = Meta {
            schema_version: r.u32()?,
            tokenizer_version: r.u32()?,
            normalizer_version: r.u32()?,
            codec_version: r.u32()?,
            config_hash: r.u32()?,
            book_id_hwm: r.u32()?,
            book_id_limit: r.u32()?,
            scan_cursor: {
                let s = r.str()?;
                if s.is_empty() { None } else { Some(s) }
            },
            db_bytes_written: r.u64()?,
            status: Status::from_u8(r.u8()?).unwrap_or(Status::Idle),
        };
        Ok(m)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_defaults() {
        let m = Meta::new();
        let bytes = m.encode();
        assert_eq!(Meta::decode(&bytes).unwrap(), m);
    }

    #[test]
    fn roundtrip_populated() {
        let m = Meta {
            schema_version: SCHEMA_VERSION,
            tokenizer_version: TOKENIZER_VERSION,
            normalizer_version: NORMALIZER_VERSION,
            codec_version: CODEC_VERSION,
            config_hash: 0xDEADBEEF,
            book_id_hwm: 1234,
            book_id_limit: MAX_BOOKS,
            scan_cursor: Some("documents/A Bá Tá [99].txt".into()),
            db_bytes_written: 1_000_000,
            status: Status::Indexing,
        };
        let bytes = m.encode();
        let back = Meta::decode(&bytes).unwrap();
        assert_eq!(back, m);
        assert_eq!(
            back.scan_cursor.as_deref(),
            Some("documents/A Bá Tá [99].txt")
        );
    }

    #[test]
    fn empty_cursor_decodes_as_none() {
        let m = Meta {
            scan_cursor: None,
            ..Meta::new()
        };
        let back = Meta::decode(&m.encode()).unwrap();
        assert_eq!(back.scan_cursor, None);
    }

    #[test]
    fn all_statuses_roundtrip() {
        for s in [
            Status::Idle,
            Status::Scanning,
            Status::Indexing,
            Status::BudgetExceeded,
        ] {
            let m = Meta {
                status: s,
                ..Meta::new()
            };
            assert_eq!(Meta::decode(&m.encode()).unwrap().status, s);
        }
    }

    #[test]
    fn unknown_status_byte_falls_back_to_idle() {
        let mut m = Meta::new();
        m.status = Status::Indexing;
        let mut bytes = m.encode();
        // Last byte is the status field.
        *bytes.last_mut().unwrap() = 99;
        assert_eq!(Meta::decode(&bytes).unwrap().status, Status::Idle);
    }

    #[test]
    fn fresh_meta_is_compatible() {
        assert!(Meta::new().is_compatible());
    }

    #[test]
    fn stale_schema_is_rejected() {
        let mut m = Meta::new();
        m.schema_version = SCHEMA_VERSION + 1;
        assert!(!m.is_compatible());

        let mut m = Meta::new();
        m.codec_version = CODEC_VERSION - 1;
        assert!(!m.is_compatible(), "codec changes must invalidate the file");
    }

    #[test]
    fn truncated_value_errors() {
        let bytes = Meta::new().encode();
        assert!(Meta::decode(&bytes[..3]).is_err());
        assert!(Meta::decode(&[]).is_err());
    }

    #[test]
    fn sizes_are_bounded() {
        let m = Meta::new();
        // Guard against accidental bloat: this row is read on every startup.
        assert!(
            m.encode().len() < 64,
            "META row is {} bytes",
            m.encode().len()
        );
    }

    #[test]
    fn book_id_limit_covers_u16_range() {
        assert!(Meta::new().book_id_limit >= MAX_BOOKS);
    }
}
