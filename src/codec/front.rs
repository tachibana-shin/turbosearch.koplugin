//! Front coding (a.k.a. prefix compression) for sorted byte strings.
//!
//! The term dictionary is sorted, so consecutive terms share long prefixes.
//! Storing `(shared_prefix_length, suffix)` instead of the whole term cuts the
//! dictionary to roughly half of its raw size before any entropy coding.

use super::vbyte;
use std::io;

/// Encodes a sorted slice of strings.
///
/// Layout: `[leb128: count]` then for the first string `[leb128: len][bytes]`,
/// and for each subsequent string `[leb128: shared][leb128: suffix_len][suffix]`.
pub fn encode(strings: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    vbyte::write_leb128(&mut out, strings.len() as u32);

    let mut prev: &[u8] = &[];
    for (i, s) in strings.iter().enumerate() {
        if i == 0 {
            vbyte::write_leb128(&mut out, s.len() as u32);
            out.extend_from_slice(s);
        } else {
            let shared = common_prefix_len(prev, s);
            debug_assert!(shared < s.len(), "sorted input must be strictly increasing");
            vbyte::write_leb128(&mut out, shared as u32);
            vbyte::write_leb128(&mut out, (s.len() - shared) as u32);
            out.extend_from_slice(&s[shared..]);
        }
        prev = s;
    }
    out
}

pub fn decode(bytes: &[u8]) -> io::Result<Vec<Vec<u8>>> {
    if bytes.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "empty front-coded block",
        ));
    }
    let mut pos = 0usize;
    let count = vbyte::read_leb128(bytes, &mut pos)? as usize;

    let mut out = Vec::with_capacity(count);
    let mut prev: Vec<u8> = Vec::new();
    for i in 0..count {
        if i == 0 {
            let len = vbyte::read_leb128(bytes, &mut pos)? as usize;
            let end = pos.checked_add(len).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "front-coded length overruns block",
                )
            })?;
            let s = bytes.get(pos..end).ok_or_else(|| {
                io::Error::new(io::ErrorKind::UnexpectedEof, "truncated first term")
            })?;
            prev = s.to_vec();
            out.push(prev.clone());
            pos = end;
        } else {
            let shared = vbyte::read_leb128(bytes, &mut pos)? as usize;
            let suffix_len = vbyte::read_leb128(bytes, &mut pos)? as usize;
            if shared > prev.len() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "shared prefix longer than previous term",
                ));
            }
            let end = pos.checked_add(suffix_len).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "front-coded length overruns block",
                )
            })?;
            let suffix = bytes
                .get(pos..end)
                .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "truncated suffix"))?;
            prev.truncate(shared);
            prev.extend_from_slice(suffix);
            out.push(prev.clone());
            pos = end;
        }
    }
    Ok(out)
}

#[inline]
pub fn common_prefix_len(a: &[u8], b: &[u8]) -> usize {
    let limit = a.len().min(b.len());
    let mut i = 0;
    while i < limit && a[i] == b[i] {
        i += 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(strings: &[&[u8]]) {
        let bytes = encode(strings);
        let decoded = decode(&bytes).unwrap();
        assert_eq!(decoded, strings);
    }

    #[test]
    fn empty_block() {
        let bytes = encode(&[]);
        assert_eq!(decode(&bytes).unwrap(), Vec::<Vec<u8>>::new());
    }

    #[test]
    fn single_term() {
        roundtrip(&[b"hello"]);
    }

    #[test]
    fn shared_prefixes_compress() {
        let terms: Vec<&[u8]> = vec![b"sherlock", b"sherlockian", b"sherlockiana", b"sherries"];
        let bytes = encode(&terms);
        let raw: usize = terms.iter().map(|t| t.len() + 1).sum();
        assert!(
            bytes.len() < raw,
            "front coding {} bytes vs raw {}",
            bytes.len(),
            raw
        );
        roundtrip(&terms);
    }

    #[test]
    fn realistic_dictionary() {
        let terms: Vec<Vec<u8>> = vec![
            b"abandon".to_vec(),
            b"ability".to_vec(),
            b"able".to_vec(),
            b"about".to_vec(),
            b"above".to_vec(),
            b"abroad".to_vec(),
            b"absence".to_vec(),
            b"absolute".to_vec(),
        ];
        let refs: Vec<&[u8]> = terms.iter().map(|t| t.as_slice()).collect();
        let bytes = encode(&refs);
        let raw: usize = terms.iter().map(|t| t.len() + 1).sum();
        assert_eq!(decode(&bytes).unwrap(), terms);
        assert!(bytes.len() < raw);
    }

    #[test]
    fn no_shared_prefix() {
        roundtrip(&[b"apple", b"banana", b"cherry", b"date"]);
    }

    #[test]
    fn one_byte_apart() {
        roundtrip(&[b"ab", b"ac", b"ad", b"ae"]);
    }

    #[test]
    fn unicode_terms() {
        // Input must be byte-sorted; these Vietnamese terms are.
        let terms: Vec<&[u8]> = vec![
            "thám".as_bytes(),
            "thám_tử".as_bytes(),
            "thư".as_bytes(),
            "thư_viện".as_bytes(),
        ];
        assert!(
            terms.windows(2).all(|w| w[0] < w[1]),
            "fixture must stay sorted"
        );
        roundtrip(&terms);
    }

    #[test]
    fn common_prefix_lengths() {
        assert_eq!(common_prefix_len(b"", b"abc"), 0);
        assert_eq!(common_prefix_len(b"abc", b""), 0);
        assert_eq!(common_prefix_len(b"abc", b"abc"), 3);
        assert_eq!(common_prefix_len(b"abc", b"abd"), 2);
        assert_eq!(common_prefix_len(b"abcd", b"ab"), 2);
        assert_eq!(common_prefix_len(b"ab", b"abcd"), 2);
    }
}
