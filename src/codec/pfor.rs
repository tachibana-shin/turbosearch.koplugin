//! Patched framed-of-`k` delta coding (PForDelta).
//!
//! Bit-packs the whole block at a fixed width `b`, then spills values that do not
//! fit into a small exception area. Compared with plain VByte this wins whenever
//! most values of a column share a similar magnitude, which is exactly the shape
//! of the position-gap column inside a single document.

use super::bits::{BitReader, BitWriter, bits_for};
use super::vbyte;
use std::io;

/// Maximum fraction of a block allowed to become exceptions before we give up on
/// patching and just widen `b` to fit everything.
const MAX_EXCEPTION_RATIO: usize = 8;

const TAG_PFOR: u8 = 0;
const TAG_VBYTE: u8 = 1;

/// Encodes `values`, picking whichever of PForDelta and VByte is smaller.
pub fn encode_auto(values: &[u32]) -> Vec<u8> {
    if values.is_empty() {
        let mut out = vec![TAG_VBYTE];
        vbyte::write_leb128(&mut out, 0);
        return out;
    }

    let pfor = encode_pfor(values);
    let vbyte_len = 1
        + 5
        + values
            .iter()
            .map(|v| vbyte::leb_len(*v) as usize)
            .sum::<usize>();

    if vbyte_len <= pfor.len() {
        let mut out = Vec::with_capacity(vbyte_len);
        out.push(TAG_VBYTE);
        vbyte::write_leb128(&mut out, values.len() as u32);
        for v in values {
            vbyte::write_leb128(&mut out, *v);
        }
        out
    } else {
        pfor
    }
}

fn encode_pfor(values: &[u32]) -> Vec<u8> {
    let n = values.len();
    let max = *values.iter().max().unwrap();
    let bits_max = bits_for(max as u64);

    // Consider narrower widths that still cover most values, but never allow so
    // many exceptions that the spill area dominates.
    let best_b = (0..=bits_max)
        .rev()
        .filter(|&b| {
            let exc = values
                .iter()
                .filter(|&&v| b < 32 && v >= (1u32 << b))
                .count();
            exc * MAX_EXCEPTION_RATIO <= n
        })
        .min_by_key(|&b| {
            let exceptions = values
                .iter()
                .filter(|&&v| b < 32 && v >= (1u32 << b))
                .count();
            // Cost model: packed area + spill area, in bytes.
            (n * b as usize).div_ceil(8) + exceptions * 6 + 8
        });

    let (b, exceptions): (u32, Vec<(usize, u32)>) = match best_b {
        Some(b) if b < bits_max => {
            let exc = values
                .iter()
                .enumerate()
                .filter(|&(_, v)| b < 32 && *v >= (1u32 << b))
                .map(|(i, v)| (i, *v))
                .collect();
            (b, exc)
        }
        _ => (bits_max, Vec::new()),
    };

    let mut out = Vec::new();
    out.push(TAG_PFOR);
    vbyte::write_leb128(&mut out, n as u32);
    out.push(b as u8);
    vbyte::write_leb128(&mut out, exceptions.len() as u32);

    let mut writer = BitWriter::with_capacity_bytes((n * b as usize).div_ceil(8) + 1);
    for v in values {
        writer.write_bits((v & mask(b)) as u64, b);
    }
    out.extend_from_slice(&writer.finish());

    for (idx, value) in exceptions {
        vbyte::write_leb128(&mut out, idx as u32);
        vbyte::write_leb128(&mut out, value);
    }

    out
}

pub fn decode(bytes: &[u8]) -> io::Result<Vec<u32>> {
    if bytes.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "empty PForDelta block",
        ));
    }
    let tag = bytes[0];
    let mut pos = 1usize;

    match tag {
        TAG_VBYTE => {
            let n = vbyte::read_leb128(bytes, &mut pos)? as usize;
            vbyte::decode(&bytes[pos..], n)
        }
        TAG_PFOR => {
            let n = vbyte::read_leb128(bytes, &mut pos)? as usize;
            let b = *bytes
                .get(pos)
                .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "missing bit width"))?
                as u32;
            pos += 1;
            let exc_count = vbyte::read_leb128(bytes, &mut pos)? as usize;

            let packed_bytes = (n * b as usize).div_ceil(8);
            let packed_end = pos.checked_add(packed_bytes).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "packed section overruns block")
            })?;
            let packed = bytes.get(pos..packed_end).ok_or_else(|| {
                io::Error::new(io::ErrorKind::UnexpectedEof, "truncated packed section")
            })?;

            let mut reader = BitReader::new(packed);
            let m = mask(b);
            let mut out: Vec<u32> = (0..n).map(|_| (reader.read_bits(b) as u32) & m).collect();

            let mut exc_pos = packed_end;
            for _ in 0..exc_count {
                let idx = vbyte::read_leb128(bytes, &mut exc_pos)? as usize;
                let value = vbyte::read_leb128(bytes, &mut exc_pos)?;
                if idx < out.len() {
                    out[idx] = value;
                } else {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "exception index out of range",
                    ));
                }
            }
            Ok(out)
        }
        other => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unknown PForDelta tag {other}"),
        )),
    }
}

#[inline]
fn mask(b: u32) -> u32 {
    if b == 0 {
        0
    } else if b >= 32 {
        u32::MAX
    } else {
        (1u32 << b) - 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input() {
        let bytes = encode_auto(&[]);
        assert_eq!(decode(&bytes).unwrap(), Vec::<u32>::new());
    }

    #[test]
    fn all_zero_is_tiny() {
        let vals = vec![0u32; 1000];
        let bytes = encode_auto(&vals);
        assert!(bytes.len() < 32, "got {} bytes", bytes.len());
        assert_eq!(decode(&bytes).unwrap(), vals);
    }

    #[test]
    fn uniform_magnitude() {
        let vals: Vec<u32> = (0..1000).map(|_| 137).collect();
        let bytes = encode_auto(&vals);
        assert_eq!(decode(&bytes).unwrap(), vals);
    }

    #[test]
    fn tight_cluster_with_outliers() {
        // Most values in [1..8], a handful of large ones that must spill.
        let mut vals: Vec<u32> = Vec::new();
        for i in 0..512u32 {
            vals.push(1 + (i % 7));
        }
        vals[100] = 1_000_000;
        vals[300] = 2_000_000;
        let bytes = encode_auto(&vals);
        assert_eq!(decode(&bytes).unwrap(), vals);
    }

    #[test]
    fn full_width_roundtrip() {
        let vals = vec![u32::MAX, 0, u32::MAX / 3, 1, 0xFFFF_FFFF];
        let bytes = encode_auto(&vals);
        assert_eq!(decode(&bytes).unwrap(), vals);
    }

    #[test]
    fn bit_width_boundaries() {
        for b in [1u32, 2, 7, 8, 9, 15, 16, 17, 31, 32] {
            let max = if b >= 32 { u32::MAX } else { (1u32 << b) - 1 };
            let vals: Vec<u32> = (0..128).map(|i| (i as u64 % max as u64) as u32).collect();
            let bytes = encode_auto(&vals);
            assert_eq!(decode(&bytes).unwrap(), vals, "b={b}");
        }
    }

    #[test]
    fn roundtrip_random_shapes() {
        // Deterministic pseudo-random so failures reproduce.
        let mut state: u64 = 0x2545F4914F6CDD1D;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for case in 0..200 {
            let n = (next() % 130) as usize + 1;
            let universe = 1u64 << (1 + next() % 20);
            let vals: Vec<u32> = (0..n).map(|_| (next() % universe) as u32).collect();
            let bytes = encode_auto(&vals);
            assert_eq!(decode(&bytes).unwrap(), vals, "case {case}");
        }
    }

    #[test]
    fn dense_beats_vbyte() {
        // A large block of small, similar values must compress hard.
        let vals: Vec<u32> = (0..4096).map(|i| 3 + (i % 5)).collect();
        let pfor = encode_pfor(&vals).len();
        let mut vb = Vec::new();
        for v in &vals {
            vbyte::write_leb128(&mut vb, *v);
        }
        // 5 distinct values all < 8 need 3 bits: 4096 * 3/8 = 1536 bytes packed.
        assert!(
            pfor * 2 < vb.len(),
            "PForDelta {pfor} vs VByte {} must win by more than 2x",
            vb.len()
        );
    }
}
