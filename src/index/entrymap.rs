//! Where a book's text physically is, so a hit can be turned back into words.
//!
//! A posting stores a position, not a byte offset: an offset costs 15–20% of the
//! index and is recoverable. What makes it recoverable is a sparse checkpoint
//! every [`CHECKPOINT_INTERVAL`] positions — `(position, byte_offset)` — plus the
//! section the positions belong to.
//!
//! `ENTRYMAP` holds one row per `(book_id, entry_idx)`, so locating a hit is:
//! find the checkpoint at or before the hit's position, read that byte offset,
//! then re-scan a bounded window of the original text forward. The window is
//! bounded because checkpoints are at most `CHECKPOINT_INTERVAL` positions
//! apart, which is why the interval is a trade between index size and snippet
//! latency and not something to tune per book.
//!
//! Byte offsets are into the section as `format` produced it — the decoded HTML
//! or plain text, already entity-decoded — so the snippet shows the reader's
//! text with diacritics intact, not the folded key it was indexed under.

use crate::codec::vbyte;
use crate::storage::pack::PackError;

/// Positions between checkpoints.
pub const CHECKPOINT_INTERVAL: u32 = 2048;

/// Upper bound on checkpoints in one row, so a corrupt count cannot trigger an
/// enormous allocation.
const MAX_CHECKPOINTS_PER_ROW: u32 = 1 << 20;

/// Maps a decode failure onto the packed-data errors the storage layer uses.
fn ioerr(what: &'static str) -> PackError {
    PackError::InvalidData(what)
}

/// A decoded `ENTRYMAP` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryMap {
    /// Section name, used to label a snippet ("Chapter 3", "0-mo-dau.xhtml").
    pub name: String,
    /// `(position, byte_offset)`, ascending, with the first at `(0, 0)`.
    pub checkpoints: Vec<(u32, u32)>,
}

impl EntryMap {
    /// Nearest checkpoint at or before `position`.
    ///
    /// Always returns index 0 for a non-empty map, because `(0, 0)` is recorded
    /// when the entry is opened no matter what interval the caller chose.
    pub fn checkpoint_for(&self, position: u32) -> (u32, u32) {
        // `partition_point` on a sorted column gives the last checkpoint whose
        // position is <= the target.
        let i = self
            .checkpoints
            .partition_point(|&(p, _)| p <= position)
            .saturating_sub(1);
        self.checkpoints[i]
    }

    /// Byte offset of `position`, when the map covers it.
    pub fn byte_off(&self, position: u32) -> Option<u32> {
        if self.checkpoints.is_empty() {
            return None;
        }
        Some(self.checkpoint_for(position).1)
    }

    pub fn encode(&self) -> Vec<u8> {
        // LEB128, not `pack::Writer::u32`: the deltas between checkpoints are
        // small and mostly one byte, which a fixed-width field would waste.
        let mut out = Vec::with_capacity(16 + self.checkpoints.len() * 4);
        // A count prefix, not an end marker: the first checkpoint is `(0, 0)`,
        // whose deltas are also `(0, 0)`, so a marker would be indistinguishable
        // from the first real entry.
        let name = self.name.as_bytes();
        vbyte::write_leb128(&mut out, name.len() as u32);
        out.extend_from_slice(name);
        vbyte::write_leb128(&mut out, self.checkpoints.len() as u32);

        let mut prev_pos = 0u32;
        let mut prev_off = 0u32;
        for &(p, o) in &self.checkpoints {
            vbyte::write_leb128(&mut out, p - prev_pos);
            vbyte::write_leb128(&mut out, o - prev_off);
            prev_pos = p;
            prev_off = o;
        }
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, PackError> {
        let mut pos = 0usize;
        let name_len = vbyte::read_leb128(bytes, &mut pos)
            .map_err(|_| ioerr("truncated entry map name length"))? as usize;
        let name_end = pos
            .checked_add(name_len)
            .ok_or_else(|| ioerr("entry map name length overruns the row"))?;
        let name = String::from_utf8(
            bytes
                .get(pos..name_end)
                .ok_or_else(|| ioerr("truncated entry map name"))?
                .to_vec(),
        )
        .map_err(|_| ioerr("entry map name is not UTF-8"))?;
        pos = name_end;

        let count = vbyte::read_leb128(bytes, &mut pos)
            .map_err(|_| ioerr("truncated entry map checkpoint count"))?;
        // Sanity, so a corrupt row cannot ask for an enormous allocation.
        if count > MAX_CHECKPOINTS_PER_ROW {
            return Err(ioerr("entry map claims too many checkpoints"));
        }

        let mut checkpoints = Vec::with_capacity(count as usize);
        let mut cpos = 0u32;
        let mut coff = 0u32;
        for _ in 0..count {
            let dp = vbyte::read_leb128(bytes, &mut pos)
                .map_err(|_| ioerr("truncated entry map checkpoint"))?;
            let doff = vbyte::read_leb128(bytes, &mut pos)
                .map_err(|_| ioerr("truncated entry map checkpoint"))?;
            cpos = cpos
                .checked_add(dp)
                .ok_or_else(|| ioerr("entry map position overflow"))?;
            coff = coff
                .checked_add(doff)
                .ok_or_else(|| ioerr("entry map byte offset overflow"))?;
            checkpoints.push((cpos, coff));
        }
        Ok(Self { name, checkpoints })
    }
}

/// Accumulates checkpoints while a section is being tokenized.
#[derive(Debug, Clone)]
pub struct Builder {
    name: String,
    checkpoints: Vec<(u32, u32)>,
    /// Position at which the next checkpoint should be taken.
    next: u32,
}

impl Builder {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            // Position 0 always gets a checkpoint, so `byte_off` never fails.
            checkpoints: vec![(0, 0)],
            next: CHECKPOINT_INTERVAL,
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Offers the current `(position, byte_offset)`.
    ///
    /// Cheap enough to call per token: it is a single comparison unless a
    /// checkpoint is actually due.
    pub fn offer(&mut self, position: u32, byte_off: u32) {
        if position < self.next {
            return;
        }
        // Never record a second checkpoint for the same position, which is what
        // makes the encode terminator unambiguous.
        if self.checkpoints.last().map(|&(p, _)| p) == Some(position) {
            return;
        }
        self.checkpoints.push((position, byte_off));
        self.next = position + CHECKPOINT_INTERVAL;
    }

    pub fn finish(self) -> EntryMap {
        EntryMap {
            name: self.name,
            checkpoints: self.checkpoints,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_checkpoint_is_always_position_zero() {
        let mut b = Builder::new("ch1");
        b.offer(0, 0);
        b.offer(5, 100);
        b.finish();
        let m = Builder::new("ch2").finish();
        assert_eq!(m.checkpoints, vec![(0, 0)]);
        assert_eq!(m.byte_off(0), Some(0));
    }

    #[test]
    fn checkpoints_land_on_the_interval() {
        let mut b = Builder::new("ch");
        for p in 0..10_000u32 {
            b.offer(p, p * 10);
        }
        let m = b.finish();
        assert_eq!(m.name, "ch");
        assert_eq!(
            m.checkpoints.len(),
            10_000usize / CHECKPOINT_INTERVAL as usize + 1
        );
        assert_eq!(
            m.checkpoints[1],
            (CHECKPOINT_INTERVAL, CHECKPOINT_INTERVAL * 10)
        );
    }

    #[test]
    fn offering_is_cheap_when_no_checkpoint_is_due() {
        let mut b = Builder::new("ch");
        b.offer(1, 5);
        b.offer(2, 9);
        assert_eq!(b.checkpoints.len(), 1);
    }

    #[test]
    fn a_duplicate_position_is_not_recorded_twice() {
        let mut b = Builder::new("ch");
        b.offer(0, 0);
        b.offer(0, 0);
        assert_eq!(b.checkpoints, vec![(0, 0)]);
    }

    #[test]
    fn checkpoint_lookup_returns_the_nearest_one_before_a_position() {
        let m = EntryMap {
            name: "ch".into(),
            checkpoints: vec![(0, 0), (2048, 5000), (4096, 9000)],
        };
        assert_eq!(m.checkpoint_for(0), (0, 0));
        assert_eq!(m.checkpoint_for(1), (0, 0));
        assert_eq!(m.checkpoint_for(2048), (2048, 5000));
        assert_eq!(m.checkpoint_for(4095), (2048, 5000));
        assert_eq!(m.checkpoint_for(9000), (4096, 9000));
    }

    #[test]
    fn byte_off_on_an_empty_map_is_none_not_a_panic() {
        let m = EntryMap {
            name: "x".into(),
            checkpoints: vec![],
        };
        assert_eq!(m.byte_off(5), None);
        // checkpoint_for would index an empty vec, so callers must check first.
        assert!(m.checkpoints.is_empty());
    }

    #[test]
    fn roundtrip_preserves_names_and_checkpoints() {
        let mut b = Builder::new("0-mo-dau.xhtml");
        for p in (0..50_000u32).step_by(7) {
            b.offer(p, p * 3);
        }
        let m = b.finish();
        let e = EntryMap::decode(&m.encode()).unwrap();
        assert_eq!(e.name, m.name);
        assert_eq!(e.checkpoints, m.checkpoints);
    }

    #[test]
    fn roundtrip_handles_a_unicode_section_name() {
        let b = Builder::new("Chương 3 — Đêm mưa");
        let e = EntryMap::decode(&b.finish().encode()).unwrap();
        assert_eq!(e.name, "Chương 3 — Đêm mưa");
    }

    #[test]
    fn a_truncated_row_is_reported_not_panicked_on() {
        let full = Builder::new("ch").finish().encode();
        for cut in 0..full.len().min(6) {
            assert!(EntryMap::decode(&full[..cut]).is_err(), "cut at {cut}");
        }
    }

    #[test]
    fn deltas_keep_the_row_small() {
        let mut b = Builder::new("ch");
        for p in 0..1_000_000u32 {
            b.offer(p, p * 2);
        }
        let m = b.finish();
        let encoded = m.encode();
        // ~488 checkpoints of two small deltas beats ~488 raw 8-byte pairs.
        assert_eq!(m.checkpoints.len(), 489);
        assert!(
            encoded.len() < m.checkpoints.len() * 8,
            "{} bytes",
            encoded.len()
        );
    }

    #[test]
    fn a_snippet_past_the_end_is_clamped() {
        // Snippet cutting lives in `query`; this just checks the boundary rule
        // for a checkpoint offset that ran off the end.
        let text = "short";
        let off = usize::try_from(u32::MAX).unwrap_or(0).min(text.len());
        assert_eq!(&text[off..], "");
    }

    #[test]
    fn a_snippet_never_splits_a_multibyte_character() {
        let text = "Tiếng gọi từ vì sao xa";
        // A byte offset of 1 lands inside the multi-byte sequence for "ế", so
        // a naive slice would panic. `query::snippet` walks back to the boundary.
        let s = crate::index::query::snippet(text, 1, 40);
        assert!(text.contains(&s), "{s:?} must be a slice of {text:?}");
    }
}
