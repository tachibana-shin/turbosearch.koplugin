//! Elias-Fano coding for monotone (non-decreasing) `u32` sequences.
//!
//! This is the primary coder for the `doc_id` column of a postings block: doc ids
//! arrive in non-decreasing order, and Elias-Fano is within a couple of bits of
//! the information-theoretic floor for that shape.
//!
//! Duplicates are handled by encoding the shifted sequence `y_i = x_i + i`, which
//! turns a non-decreasing input into a strictly increasing one without changing
//! the decoded result (`x_i = y_i - i`).

use super::bits::{bits_for, BitReader, BitWriter};
use super::vbyte;
use std::io;

/// Frame tag: 0 = Elias-Fano, 1 = VByte fallback for pathological inputs.
const TAG_EF: u8 = 0;
const TAG_FALLBACK: u8 = 1;

/// Encodes `values` (non-decreasing) into a self-describing byte stream.
///
/// # Panics
/// Panics if `values` is not sorted in non-decreasing order.
pub fn encode(values: &[u32]) -> Vec<u8> {
    debug_assert!(values.windows(2).all(|w| w[0] <= w[1]), "input must be monotone");

    let n = values.len();
    if n == 0 {
        return vec![TAG_EF, 0];
    }

    let x_max = values[n - 1] as u64;
    // `y_i = x_i + i` is strictly increasing; guard against overflowing u32.
    let shift_overflow = x_max + n as u64 > u32::MAX as u64;

    if shift_overflow {
        let mut out = Vec::new();
        out.push(TAG_FALLBACK);
        vbyte::write_leb128(&mut out, n as u32);
        for v in values {
            vbyte::write_leb128(&mut out, *v);
        }
        return out;
    }

    let y_max = x_max + n as u64 - 1;
    let l_bits = if n <= 1 { 0 } else { bits_for(y_max / n as u64) };

    let mut out = vec![TAG_EF];
    vbyte::write_leb128(&mut out, n as u32);
    vbyte::write_leb128(&mut out, x_max as u32);
    out.push(l_bits as u8);

    let mut lower = BitWriter::with_capacity_bytes(n * l_bits as usize / 8 + 1);
    for (i, x) in values.iter().enumerate() {
        let y = *x as u64 + i as u64;
        lower.write_bits(y & ((1u64 << l_bits) - 1), l_bits);
    }

    let mut upper = BitWriter::with_capacity_bytes(n / 4 + 8);
    let mut prev_hi: u64 = 0;
    // Unary gaps: `u_i - u_{i-1}` zeros then a one. The first gap counts from -1,
    // so it is `u_0 + 1` zeros.
    let mut first = true;
    for (i, x) in values.iter().enumerate() {
        let y = *x as u64 + i as u64;
        let u = y >> l_bits;
        let gap = if first {
            first = false;
            u + 1
        } else {
            u - prev_hi
        };
        upper.write_unary(gap as u32);
        prev_hi = u;
    }

    out.extend_from_slice(&lower.finish());
    out.extend_from_slice(&upper.finish());
    out
}

/// Decodes a stream produced by [`encode`].
pub fn decode(bytes: &[u8]) -> io::Result<Vec<u32>> {
    if bytes.is_empty() {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "empty Elias-Fano block"));
    }
    let tag = bytes[0];
    let mut pos = 1usize;

    match tag {
        TAG_FALLBACK => {
            let n = vbyte::read_leb128(bytes, &mut pos)? as usize;
            vbyte::decode(&bytes[pos..], n)
        }
        TAG_EF => {
            let n = vbyte::read_leb128(bytes, &mut pos)? as usize;
            if n == 0 {
                return Ok(Vec::new());
            }
            let _x_max = vbyte::read_leb128(bytes, &mut pos)?;
            let l_bits = *bytes.get(pos).ok_or_else(|| {
                io::Error::new(io::ErrorKind::UnexpectedEof, "missing l_bits")
            })? as u32;
            pos += 1;

            let lower_bytes = (n * l_bits as usize).div_ceil(8);
            let lower_end = pos.checked_add(lower_bytes).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "lower section overruns block")
            })?;
            let lower_slice = bytes.get(pos..lower_end).ok_or_else(|| {
                io::Error::new(io::ErrorKind::UnexpectedEof, "truncated lower section")
            })?;
            let upper_slice = bytes.get(lower_end..).ok_or_else(|| {
                io::Error::new(io::ErrorKind::UnexpectedEof, "missing upper section")
            })?;

            let mut lower_reader = BitReader::new(lower_slice);
            let mut upper_reader = BitReader::new(upper_slice);

            let mut out = Vec::with_capacity(n);
            let mut counter: i64 = -1;
            for i in 0..n {
                while upper_reader.read_bit() == 0 {
                    counter += 1;
                }
                let hi = counter as u64;
                let low = lower_reader.read_bits(l_bits);
                let y = (hi << l_bits) | low;
                let x = y.checked_sub(i as u64).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "Elias-Fano underflow")
                })?;
                let x: u32 = x.try_into().map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidData, "Elias-Fano value exceeds u32")
                })?;
                out.push(x);
            }
            Ok(out)
        }
        other => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unknown Elias-Fano tag {other}"),
        )),
    }
}

/// Encoded size in bytes for a given input, without allocating the output.
pub fn encoded_len(values: &[u32]) -> usize {
    let n = values.len();
    if n == 0 {
        return 1 + vbyte::leb_len(0) as usize;
    }

    let x_max = values[n - 1] as u64;
    if x_max + n as u64 > u32::MAX as u64 {
        // Fallback path: tag + n + each value as LEB128.
        return 1 + vbyte::leb_len(n as u32) as usize
            + values.iter().map(|v| vbyte::leb_len(*v) as usize).sum::<usize>();
    }

    let y_max = x_max + n as u64 - 1;
    let l_bits = if n <= 1 { 0 } else { bits_for(y_max / n as u64) };
    let lower_bytes = (n * l_bits as usize).div_ceil(8);
    let upper_bits: u64 = n as u64 + (y_max >> l_bits) + 1;
    let upper_bytes = upper_bits.div_ceil(8) as usize;
    1 + vbyte::leb_len(n as u32) as usize
        + vbyte::leb_len(x_max as u32) as usize
        + 1
        + lower_bytes
        + upper_bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input() {
        let bytes = encode(&[]);
        assert_eq!(decode(&bytes).unwrap(), Vec::<u32>::new());
    }

    #[test]
    fn single_element() {
        for v in [0u32, 1, 1000, u32::MAX] {
            let bytes = encode(&[v]);
            assert_eq!(decode(&bytes).unwrap(), vec![v]);
        }
    }

    #[test]
    fn strictly_increasing() {
        let vals: Vec<u32> = vec![0, 1, 2, 3, 10, 100, 1000, 9999, 100_000];
        let bytes = encode(&vals);
        assert_eq!(decode(&bytes).unwrap(), vals);
    }

    #[test]
    fn duplicates_and_gaps() {
        let vals: Vec<u32> = vec![
            5, 5, 5, 5, 8, 8, 9, 40, 40, 41, 500, 500, 500, 500, 500, 5000,
        ];
        let bytes = encode(&vals);
        assert_eq!(decode(&bytes).unwrap(), vals);
    }

    #[test]
    fn all_same_value() {
        let vals = vec![7u32; 500];
        let bytes = encode(&vals);
        assert_eq!(decode(&bytes).unwrap(), vals);
    }

    #[test]
    fn dense_small_universe() {
        // n close to the universe size: this is where Elias-Fano shines.
        let vals: Vec<u32> = (0..1000).collect();
        let bytes = encode(&vals);
        assert_eq!(decode(&bytes).unwrap(), vals);
        assert!(bytes.len() < 1000, "dense input compressed to {}", bytes.len());
    }

    #[test]
    fn sparse_large_universe() {
        // Sparse doc ids over a large universe.
        let vals: Vec<u32> = (0..500).map(|i| i * 7919).collect();
        let bytes = encode(&vals);
        assert_eq!(decode(&bytes).unwrap(), vals);
    }

    #[test]
    fn fallback_when_shift_overflows() {
        // x_max + n > u32::MAX must take the VByte path, not panic or wrap.
        let vals: Vec<u32> = vec![u32::MAX - 2, u32::MAX - 1, u32::MAX, u32::MAX];
        let bytes = encode(&vals);
        assert_eq!(bytes[0], TAG_FALLBACK);
        assert_eq!(decode(&bytes).unwrap(), vals);
    }

    #[test]
    fn encoded_len_matches_encode() {
        let cases: Vec<Vec<u32>> = vec![
            vec![],
            vec![0],
            vec![u32::MAX - 2, u32::MAX],
            (0..100).collect(),
            (0..300).map(|i| i * 37).collect(),
            vec![4; 1000],
        ];
        for vals in cases {
            assert_eq!(encoded_len(&vals), encode(&vals).len(), "input {:?} prefix", &vals[..vals.len().min(3)]);
        }
    }

    #[test]
    fn dense_beats_vbyte() {
        let vals: Vec<u32> = (0..4096).collect();
        let ef = encode(&vals).len();
        let vb = vbyte::encode(&vals).len();
        assert!(ef < vb, "EF {ef} bytes should beat VByte {vb} bytes");
    }
}
