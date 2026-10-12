//! Binary Interpolative Coding (BIC) for strictly increasing `u32` sequences.
//!
//! BIC spends `log2(C(range, count))` bits for the whole sequence instead of
//! `log2(range)` bits per element. It wins big on position gaps, whose values are
//! spread unevenly inside a known, comparatively tight range.
//!
//! **Precondition:** the input must be **strictly increasing** and all values must
//! fit in a caller-supplied universe. Position gaps reset at every document
//! boundary, so the position column must be split into monotone runs before it is
//! handed to this coder (see `mod::split_runs`).

use super::bits::{bits_for, BitReader, BitWriter};
use super::vbyte;
use std::io;

/// Header: universe (exclusive upper bound) and element count.
///
/// Layout: `[leb128: universe] [leb128: count] [bit-packed payload]`
pub fn encode(values: &[u32], universe: u32) -> Vec<u8> {
    debug_assert!(
        values.windows(2).all(|w| w[0] < w[1]),
        "BIC requires a strictly increasing input"
    );
    debug_assert!(values.last().is_none_or(|&m| m < universe), "value exceeds universe");
    debug_assert!(values.len() as u64 <= universe as u64);

    let mut out = Vec::new();
    vbyte::write_leb128(&mut out, universe);
    vbyte::write_leb128(&mut out, values.len() as u32);

    if values.is_empty() {
        return out;
    }

    let mut writer = BitWriter::with_capacity_bytes(values.len() * 2);
    encode_recursive(&mut writer, values, 0, universe as u64 - 1);
    out.extend_from_slice(&writer.finish());
    out
}

fn encode_recursive(writer: &mut BitWriter, values: &[u32], lo: u64, hi: u64) {
    let n = values.len();
    if n == 0 {
        return;
    }

    let range = hi - lo + 1;
    // Every slot in the range is occupied: nothing left to encode.
    if n as u64 == range {
        return;
    }

    let bits = bits_for(range);
    let mid_index = n / 2;
    let pivot = values[mid_index] as u64;

    // `pivot` sits in [lo, hi]. When the range is dense the pivot is implied,
    // but writing it uniformly keeps decode symmetric and cheap.
    writer.write_bits(pivot - lo, bits);

    // Guarding on a non-empty sub-slice also guarantees `pivot - 1` and
    // `pivot + 1` cannot underflow or overflow.
    if mid_index > 0 {
        encode_recursive(writer, &values[..mid_index], lo, pivot - 1);
    }
    if mid_index + 1 < n {
        encode_recursive(writer, &values[mid_index + 1..], pivot + 1, hi);
    }
}

pub fn decode(bytes: &[u8]) -> io::Result<Vec<u32>> {
    if bytes.is_empty() {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "empty BIC block"));
    }
    let mut pos = 0usize;
    let universe = vbyte::read_leb128(bytes, &mut pos)?;
    let n = vbyte::read_leb128(bytes, &mut pos)? as usize;

    if universe == 0 && n > 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "BIC block claims values in an empty universe",
        ));
    }

    if n == 0 {
        return Ok(Vec::new());
    }

    let payload = bytes.get(pos..).ok_or_else(|| {
        io::Error::new(io::ErrorKind::UnexpectedEof, "missing BIC payload")
    })?;
    let mut reader = BitReader::new(payload);
    let mut out = vec![0u32; n];
    decode_recursive(&mut reader, &mut out, 0, universe as u64 - 1)?;
    Ok(out)
}

fn decode_recursive(
    reader: &mut BitReader,
    out: &mut [u32],
    lo: u64,
    hi: u64,
) -> io::Result<()> {
    let n = out.len();
    if n == 0 {
        return Ok(());
    }

    let range = hi - lo + 1;
    if n as u64 == range {
        // Dense: every value is present, in order.
        for (i, slot) in out.iter_mut().enumerate() {
            *slot = (lo + i as u64) as u32;
        }
        return Ok(());
    }

    let bits = bits_for(range);
    let pivot = reader.read_bits(bits) + lo;
    let pivot_u32: u32 = pivot.try_into().map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidData, "BIC pivot exceeds u32")
    })?;

    let mid = n / 2;
    out[mid] = pivot_u32;

    if mid > 0 {
        decode_recursive(reader, &mut out[..mid], lo, pivot - 1)?;
    }
    if mid + 1 < n {
        decode_recursive(reader, &mut out[mid + 1..], pivot + 1, hi)?;
    }
    Ok(())
}

/// Splits a non-monotone sequence into strictly increasing runs.
///
/// The position column of a postings block resets at every document boundary, so
/// it is not globally monotone. Each run still is, and BIC needs monotone input.
pub fn split_runs(values: &[u32]) -> Vec<(usize, &[u32])> {
    let mut runs = Vec::new();
    let mut start = 0usize;
    for i in 1..values.len() {
        if values[i] <= values[i - 1] {
            runs.push((start, &values[start..i]));
            start = i;
        }
    }
    if start < values.len() {
        runs.push((start, &values[start..]));
    }
    runs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(values: &[u32], universe: u32) {
        let bytes = encode(values, universe);
        assert_eq!(decode(&bytes).unwrap(), values, "roundtrip failed");
    }

    #[test]
    fn empty_sequence() {
        let bytes = encode(&[], 100);
        assert_eq!(decode(&bytes).unwrap(), Vec::<u32>::new());
    }

    #[test]
    fn single_element() {
        for v in [0u32, 5, 99] {
            roundtrip(&[v], 100);
        }
    }

    #[test]
    fn dense_sequence_is_free() {
        // All slots occupied: payload needs zero bits.
        let vals: Vec<u32> = (0..64).collect();
        let bytes = encode(&vals, 64);
        assert_eq!(bytes.len(), 2, "expected header only, got {} bytes", bytes.len());
        assert_eq!(decode(&bytes).unwrap(), vals);
    }

    #[test]
    fn sparse_sequence() {
        let vals = vec![0u32, 50, 101, 250, 999];
        roundtrip(&vals, 1000);
    }

    #[test]
    fn uneven_gaps() {
        let vals = vec![0u32, 1, 2, 3, 4, 5, 6, 7, 8, 500, 900];
        roundtrip(&vals, 1000);
    }

    #[test]
    fn many_values_mixed_shapes() {
        // Mix of tight clusters and scattered points.
        let mut vals: Vec<u32> = Vec::new();
        vals.extend(0..10);
        vals.push(200);
        vals.extend(300..310);
        vals.push(700);
        vals.push(701);
        vals.push(702);
        vals.push(5000);
        roundtrip(&vals, 6000);
    }

    #[test]
    fn split_runs_finds_boundaries() {
        // Runs break wherever the sequence stops strictly increasing.
        let vals = vec![1u32, 2, 3, 10, 11, 4, 5, 6, 100, 100];
        let runs = split_runs(&vals);
        assert_eq!(runs.len(), 3);
        assert_eq!(runs[0].1, &[1, 2, 3, 10, 11]);
        assert_eq!(runs[1].1, &[4, 5, 6, 100]);
        assert_eq!(runs[2].1, &[100]);

        // Each run must be strictly increasing.
        for (_, run) in &runs {
            assert!(run.windows(2).all(|w| w[0] < w[1]), "run {run:?} not increasing");
        }

        // Runs must tile the input exactly, in order.
        let mut rebuilt: Vec<u32> = Vec::new();
        for (start, run) in &runs {
            rebuilt.extend_from_slice(run);
            assert_eq!(*start, rebuilt.len() - run.len(), "run offsets not contiguous");
        }
        assert_eq!(rebuilt, vals);
    }

    #[test]
    fn split_runs_empty_and_single() {
        assert!(split_runs(&[]).is_empty());
        assert_eq!(split_runs(&[42]).len(), 1);
    }

    #[test]
    fn dense_beats_vbyte_when_universe_tight() {
        let vals: Vec<u32> = (0..4096).step_by(1).collect();
        let bic = encode(&vals, 4096).len();
        let vb = vbyte::encode(&vals).len();
        assert!(bic <= vb, "BIC {bic} should not lose to VByte {vb}");
    }
}
