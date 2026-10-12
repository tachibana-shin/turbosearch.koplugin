//! Bit-level primitives shared by the column codecs.
//!
//! All coders in this module are LSB-first and operate on the same primitive
//! so that a bit-packed column can be decoded without knowing its neighbours'
//! layout.

/// Maximum number of bits that may be written in a single [`BitWriter::write_bits`]
/// call. Kept below 64 so the shift `value << count` can never overflow.
pub const MAX_BITS_PER_WRITE: u32 = 57;

/// Appends values in `n`-bit chunks, least-significant-bit first.
#[derive(Debug, Default, Clone)]
pub struct BitWriter {
    bytes: Vec<u8>,
    buffer: u64,
    count: u32,
}

impl BitWriter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_capacity_bytes(cap: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(cap),
            buffer: 0,
            count: 0,
        }
    }

    /// Writes the low `n` bits of `value`.
    ///
    /// `n` must be at most [`MAX_BITS_PER_WRITE`], and `value` must fit in `n` bits.
    pub fn write_bits(&mut self, value: u64, n: u32) {
        debug_assert!(
            n <= MAX_BITS_PER_WRITE,
            "n={n} exceeds {MAX_BITS_PER_WRITE}"
        );
        debug_assert!(
            n == 64 || value < (1u64 << n),
            "value {value} does not fit in {n} bits"
        );

        if n == 0 {
            return;
        }

        // Invariant on entry: `count < 8`, therefore `count + n <= 7 + 57 = 64`.
        self.buffer |= value << self.count;
        self.count += n;

        while self.count >= 8 {
            self.bytes.push((self.buffer & 0xFF) as u8);
            self.buffer >>= 8;
            self.count -= 8;
        }
    }

    /// Writes `n` zero bits followed by a single one bit.
    pub fn write_unary(&mut self, n: u32) {
        // Flush in 8-bit groups to avoid pathological loops on huge `n`.
        let mut remaining = n;
        while remaining >= 8 {
            self.write_bits(0, 8);
            remaining -= 8;
        }
        self.write_bits(1 << remaining, remaining + 1);
    }

    /// Number of bits written so far, excluding padding.
    pub fn bit_len(&self) -> usize {
        self.bytes.len() * 8 + self.count as usize
    }

    /// Consumes the writer and returns the padded byte stream.
    pub fn finish(mut self) -> Vec<u8> {
        if self.count > 0 {
            self.bytes.push((self.buffer & 0xFF) as u8);
            self.buffer = 0;
            self.count = 0;
        }
        self.bytes
    }
}

/// Reads back a stream produced by [`BitWriter`].
#[derive(Debug, Clone)]
pub struct BitReader<'a> {
    bytes: &'a [u8],
    bit_pos: usize,
}

impl<'a> BitReader<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, bit_pos: 0 }
    }

    /// Reads `n` bits, least significant first. Past-the-end reads yield zero.
    pub fn read_bits(&mut self, n: u32) -> u64 {
        let mut result: u64 = 0;
        let mut filled = 0u32;
        while filled < n {
            let byte_idx = self.bit_pos >> 3;
            if byte_idx >= self.bytes.len() {
                // Past the end: treat as zero padding and advance so the reader
                // stays usable (callers must validate `n` themselves).
                self.bit_pos += (n - filled) as usize;
                return result;
            }
            let byte = self.bytes[byte_idx] as u64;
            let bit_off = self.bit_pos & 7;
            let available = 8 - bit_off as u32;
            let take = available.min(n - filled);
            let mask = if take == 64 {
                u64::MAX
            } else {
                (1u64 << take) - 1
            };
            result |= ((byte >> bit_off) & mask) << filled;
            filled += take;
            self.bit_pos += take as usize;
        }
        result
    }

    pub fn read_bit(&mut self) -> u8 {
        self.read_bits(1) as u8
    }

    /// Reads a run written by [`BitWriter::write_unary`].
    pub fn read_unary(&mut self) -> u32 {
        let mut count = 0u32;
        loop {
            if self.read_bit() == 1 {
                return count;
            }
            count += 1;
        }
    }

    pub fn bit_position(&self) -> usize {
        self.bit_pos
    }

    /// Skips to the next byte boundary. Used by the block framing
    #[allow(dead_code)]
    pub fn align_to_byte(&mut self) {
        let rem = self.bit_pos & 7;
        if rem != 0 {
            self.bit_pos += 8 - rem;
        }
    }
}

/// Smallest number of bits that can represent `v` (0 for `v == 0`).
#[inline]
pub fn bits_for(v: u64) -> u32 {
    if v == 0 { 0 } else { 64 - v.leading_zeros() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_read_roundtrip_single_values() {
        for &n in &[0u32, 1, 2, 7, 8, 9, 16, 31, 32, 33, 57] {
            if n == 0 {
                let mut w = BitWriter::new();
                w.write_bits(0, 0);
                let bytes = w.finish();
                assert_eq!(bytes.len(), 0);
                continue;
            }
            let max = if n == 64 { u64::MAX } else { (1u64 << n) - 1 };
            for &v in &[0u64, 1, max, max / 2, 0b1011_0110] {
                let v = v.min(max);
                let mut w = BitWriter::new();
                w.write_bits(v, n);
                let bytes = w.finish();
                let mut r = BitReader::new(&bytes);
                assert_eq!(r.read_bits(n), v, "n={n} v={v}");
            }
        }
    }

    #[test]
    fn write_read_roundtrip_many() {
        let mut w = BitWriter::new();
        let values: Vec<(u64, u32)> = vec![(1, 1), (3, 2), (255, 8), (7, 3), (1000, 10), (0, 5)];
        for (v, n) in &values {
            w.write_bits(*v, *n);
        }
        let bytes = w.finish();
        let mut r = BitReader::new(&bytes);
        for (v, n) in &values {
            assert_eq!(r.read_bits(*n), *v);
        }
    }

    #[test]
    fn unary_roundtrip() {
        for n in [0u32, 1, 5, 100, 3000, 70_000] {
            let mut w = BitWriter::new();
            w.write_unary(n);
            let bytes = w.finish();
            let mut r = BitReader::new(&bytes);
            assert_eq!(r.read_unary(), n);
        }
    }

    #[test]
    fn bit_len_tracks_writes() {
        let mut w = BitWriter::new();
        w.write_bits(0b1, 3);
        assert_eq!(w.bit_len(), 3);
        w.write_bits(0b101, 3);
        assert_eq!(w.bit_len(), 6);
        w.write_bits(0, 8);
        assert_eq!(w.bit_len(), 14);
        let bytes = w.finish();
        assert_eq!(bytes.len(), 2);
    }

    #[test]
    fn bits_for_matches_log2() {
        for &(v, b) in &[
            (0u64, 0u32),
            (1, 1),
            (2, 2),
            (3, 2),
            (4, 3),
            (7, 3),
            (255, 8),
            (256, 9),
        ] {
            assert_eq!(bits_for(v), b, "v={v}");
        }
        assert_eq!(bits_for(u32::MAX as u64), 32);
        assert_eq!(bits_for(u64::MAX), 64);
    }

    #[test]
    fn interleaved_unary_and_bits() {
        let mut w = BitWriter::new();
        w.write_unary(5);
        w.write_bits(0b1010, 4);
        w.write_unary(0);
        w.write_bits((1u64 << 32) - 1, 32);
        let bytes = w.finish();
        let mut r = BitReader::new(&bytes);
        assert_eq!(r.read_unary(), 5);
        assert_eq!(r.read_bits(4), 0b1010);
        assert_eq!(r.read_unary(), 0);
        assert_eq!(r.read_bits(32), (1u64 << 32) - 1);
    }
}
