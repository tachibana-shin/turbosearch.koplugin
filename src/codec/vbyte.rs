//! Variable-length byte (LEB128) coding for the framing bytes of a block.

use std::io;

/// Writes `value` as LEB128. `u32::MAX` costs 5 bytes.
pub fn write_leb128(out: &mut Vec<u8>, mut value: u32) {
    loop {
        let byte = (value & 0x7F) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

/// Reads a LEB128 written by [`write_leb128`], advancing `pos`.
pub fn read_leb128(bytes: &[u8], pos: &mut usize) -> io::Result<u32> {
    let mut result: u32 = 0;
    let mut shift = 0u32;
    loop {
        let byte = *bytes.get(*pos).ok_or_else(|| {
            io::Error::new(io::ErrorKind::UnexpectedEof, "truncated LEB128")
        })?;
        *pos += 1;

        if shift >= 32 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "LEB128 overflows u32",
            ));
        }

        result |= ((byte & 0x7F) as u32) << shift;
        if byte & 0x80 == 0 {
            return Ok(result);
        }
        shift += 7;
    }
}

/// Encodes a whole sequence with no framing.
pub fn encode(values: &[u32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(values.len());
    for v in values {
        write_leb128(&mut out, *v);
    }
    out
}

/// Byte length of `value` when LEB128 encoded.
pub fn leb_len(mut value: u32) -> u32 {
    let mut n = 1;
    while value >= 0x80 {
        value >>= 7;
        n += 1;
    }
    n
}

/// Decodes `n` LEB128 values.
pub fn decode(bytes: &[u8], n: usize) -> io::Result<Vec<u32>> {
    let mut out = Vec::with_capacity(n);
    let mut pos = 0usize;
    for _ in 0..n {
        out.push(read_leb128(bytes, &mut pos)?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_values() {
        for &v in &[0u32, 1, 127, 128, 16383, 16384, 0x0FFFFFFF, u32::MAX] {
            let mut buf = Vec::new();
            write_leb128(&mut buf, v);
            let mut pos = 0;
            assert_eq!(read_leb128(&buf, &mut pos).unwrap(), v);
            assert_eq!(pos, buf.len());
        }
    }

    #[test]
    fn roundtrip_sequence() {
        let vals: Vec<u32> = (0..2000).map(|i| i * 7919 % 100_000).collect();
        let bytes = encode(&vals);
        assert_eq!(decode(&bytes, vals.len()).unwrap(), vals);
    }

    #[test]
    fn small_values_cost_one_byte() {
        let mut buf = Vec::new();
        write_leb128(&mut buf, 42);
        assert_eq!(buf.len(), 1);
    }

    #[test]
    fn truncated_input_errors() {
        let mut buf = Vec::new();
        write_leb128(&mut buf, 100_000);
        buf.pop();
        let mut pos = 0;
        assert!(read_leb128(&buf, &mut pos).is_err());
    }
}
