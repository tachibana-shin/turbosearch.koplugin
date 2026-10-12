//! Run-length coding for near-constant columns.
//!
//! The `entry_idx` column of a postings block is constant for long stretches
//! (every posting of a book file that is one ZIP entry), so run-length beats any
//! bit-packed scheme by a wide margin.

use super::vbyte;
use std::io;

const TAG_RLE: u8 = 0;
const TAG_VBYTE: u8 = 1;

/// Encodes `values` as `(value, run_length)` pairs.
///
/// Layout: `[tag] [leb128: run_count]` then `run_count × ([leb128: value] [leb128: run_len])`.
pub fn encode(values: &[u32]) -> Vec<u8> {
    let mut runs: Vec<(u32, u32)> = Vec::new();
    for &v in values {
        match runs.last_mut() {
            Some(last) if last.0 == v => last.1 += 1,
            _ => runs.push((v, 1)),
        }
    }

    let mut out = Vec::new();
    out.push(TAG_RLE);
    vbyte::write_leb128(&mut out, runs.len() as u32);
    for (value, run_len) in runs {
        vbyte::write_leb128(&mut out, value);
        vbyte::write_leb128(&mut out, run_len);
    }
    out
}

/// Picks whichever of RLE and VByte is smaller for this input.
pub fn encode_auto(values: &[u32]) -> Vec<u8> {
    let rle = encode(values);
    let vbyte_size = 1
        + 5
        + values
            .iter()
            .map(|v| vbyte::leb_len(*v) as usize)
            .sum::<usize>();

    if values.is_empty() {
        let mut out = Vec::new();
        out.push(TAG_VBYTE);
        vbyte::write_leb128(&mut out, 0);
        return out;
    }

    if vbyte_size < rle.len() {
        let mut out = Vec::with_capacity(vbyte_size);
        out.push(TAG_VBYTE);
        vbyte::write_leb128(&mut out, values.len() as u32);
        for v in values {
            vbyte::write_leb128(&mut out, *v);
        }
        out
    } else {
        rle
    }
}

pub fn decode(bytes: &[u8]) -> io::Result<Vec<u32>> {
    if bytes.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "empty RLE block",
        ));
    }
    let tag = bytes[0];
    let mut pos = 1usize;

    match tag {
        TAG_RLE => {
            let run_count = vbyte::read_leb128(bytes, &mut pos)? as usize;
            let mut out = Vec::new();
            for _ in 0..run_count {
                let value = vbyte::read_leb128(bytes, &mut pos)?;
                let run_len = vbyte::read_leb128(bytes, &mut pos)?;
                out.resize(out.len() + run_len as usize, value);
            }
            Ok(out)
        }
        TAG_VBYTE => {
            let n = vbyte::read_leb128(bytes, &mut pos)? as usize;
            vbyte::decode(&bytes[pos..], n)
        }
        other => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unknown RLE tag {other}"),
        )),
    }
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
    fn constant_column_is_tiny() {
        let vals = vec![7u32; 4096];
        let bytes = encode(&vals);
        assert!(
            bytes.len() < 16,
            "constant column compressed to {} bytes",
            bytes.len()
        );
        assert_eq!(decode(&bytes).unwrap(), vals);
    }

    #[test]
    fn constant_near_is_tiny() {
        let mut vals = vec![7u32; 2000];
        vals.extend(vec![8u32; 2000]);
        vals.extend(vec![6u32; 96]);
        let bytes = encode(&vals);
        assert!(bytes.len() < 32);
        assert_eq!(decode(&bytes).unwrap(), vals);
    }

    #[test]
    fn alternating_column_falls_back() {
        let vals: Vec<u32> = (0..1000).map(|i| i % 2).collect();
        let bytes = encode_auto(&vals);
        assert_eq!(decode(&bytes).unwrap(), vals);
        assert_eq!(bytes[0], TAG_VBYTE, "alternating input should pick VByte");
    }

    #[test]
    fn constant_column_picks_rle() {
        let bytes = encode_auto(&[5u32; 1000]);
        assert_eq!(bytes[0], TAG_RLE);
        assert_eq!(decode(&bytes).unwrap(), vec![5u32; 1000]);
    }

    #[test]
    fn roundtrip_various() {
        let cases: Vec<Vec<u32>> = vec![
            vec![0],
            vec![1, 1, 1],
            vec![1, 2, 3, 4, 5],
            vec![100, 100, 1, 2, 2, 2, 3, u32::MAX, u32::MAX],
            (0..500).map(|i| i / 50).collect(),
        ];
        for vals in cases {
            let bytes = encode_auto(&vals);
            assert_eq!(decode(&bytes).unwrap(), vals);
        }
    }

    #[test]
    fn leb_len_matches_actual() {
        for &v in &[0u32, 1, 127, 128, 16383, 16384, u32::MAX] {
            let mut buf = Vec::new();
            vbyte::write_leb128(&mut buf, v);
            assert_eq!(vbyte::leb_len(v) as usize, buf.len(), "v={v}");
        }
    }
}
