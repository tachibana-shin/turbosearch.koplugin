//! Byte packing primitives for redb values.
//!
//! Every value in the database is a hand-packed byte string. Doing it by hand
//! (instead of pulling in serde) keeps the schema explicit: changing it is a
//! deliberate edit in exactly one place, not a side effect of a derive macro.

use std::borrow::Cow;

pub type Result<T> = std::result::Result<T, PackError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackError {
    /// Reader ran off the end of the buffer.
    UnexpectedEof,
    /// Buffer contained data that does not match the field being decoded.
    InvalidData(&'static str),
    /// A counted field claimed a length larger than the remaining buffer.
    LengthOverrun,
    /// A byte sequence was not valid UTF-8 where text was expected.
    InvalidUtf8,
}

impl std::fmt::Display for PackError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PackError::UnexpectedEof => write!(f, "unexpected end of packed buffer"),
            PackError::InvalidData(what) => write!(f, "invalid packed data: {what}"),
            PackError::LengthOverrun => write!(f, "length prefix overruns buffer"),
            PackError::InvalidUtf8 => write!(f, "packed text field is not valid UTF-8"),
        }
    }
}

impl std::error::Error for PackError {}

/// Appends fields to a byte buffer.
#[derive(Debug, Default, Clone)]
pub struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_capacity(cap: usize) -> Self {
        Self {
            buf: Vec::with_capacity(cap),
        }
    }

    pub fn u8(mut self, v: u8) -> Self {
        self.buf.push(v);
        self
    }

    pub fn u16(mut self, v: u16) -> Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }

    pub fn u32(mut self, v: u32) -> Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }

    pub fn u64(mut self, v: u64) -> Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }

    pub fn i64(mut self, v: i64) -> Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }

    /// Length-prefixed byte blob. Prefix is `u32`, capping a field at 4 GiB.
    pub fn bytes(mut self, v: &[u8]) -> Self {
        self.buf.extend_from_slice(&(v.len() as u32).to_le_bytes());
        self.buf.extend_from_slice(v);
        self
    }

    pub fn str(mut self, v: &str) -> Self {
        let len = v.len() as u32;
        self.buf.extend_from_slice(&len.to_le_bytes());
        self.buf.extend_from_slice(v.as_bytes());
        self
    }

    /// Length-prefixed list of byte blobs (each itself length-prefixed).
    pub fn byte_list(mut self, items: &[impl AsRef<[u8]>]) -> Self {
        self = self.u32(items.len() as u32);
        for item in items {
            self = self.bytes(item.as_ref());
        }
        self
    }

    /// Length-prefixed list of strings.
    pub fn str_list(mut self, items: &[String]) -> Self {
        self = self.u32(items.len() as u32);
        for item in items {
            self = self.str(item);
        }
        self
    }

    pub fn bool(mut self, v: bool) -> Self {
        self.buf.push(v as u8);
        self
    }

    pub fn finish(self) -> Vec<u8> {
        self.buf
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// True when nothing has been written yet.
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }
}

/// Reads fields back out of a buffer produced by [`Writer`].
#[derive(Debug, Clone)]
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    pub fn u8(&mut self) -> Result<u8> {
        let b = *self.buf.get(self.pos).ok_or(PackError::UnexpectedEof)?;
        self.pos += 1;
        Ok(b)
    }

    pub fn u16(&mut self) -> Result<u16> {
        let bytes = self.take(2)?;
        Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    pub fn u32(&mut self) -> Result<u32> {
        let bytes = self.take(4)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    pub fn u64(&mut self) -> Result<u64> {
        let bytes = self.take(8)?;
        let mut out = [0u8; 8];
        out.copy_from_slice(bytes);
        Ok(u64::from_le_bytes(out))
    }

    pub fn i64(&mut self) -> Result<i64> {
        Ok(self.u64()? as i64)
    }

    pub fn bool(&mut self) -> Result<bool> {
        Ok(match self.u8()? {
            0 => false,
            1 => true,
            _ => return Err(PackError::InvalidData("bool must be 0 or 1")),
        })
    }

    /// Borrows a slice from the underlying buffer. No allocation.
    pub fn bytes_ref(&mut self) -> Result<&'a [u8]> {
        let len = self.u32()? as usize;
        let end = self.pos.checked_add(len).ok_or(PackError::LengthOverrun)?;
        let slice = self
            .buf
            .get(self.pos..end)
            .ok_or(PackError::LengthOverrun)?;
        self.pos = end;
        Ok(slice)
    }

    /// Owns a slice, for data that outlives the database guard.
    pub fn bytes(&mut self) -> Result<Vec<u8>> {
        Ok(self.bytes_ref()?.to_vec())
    }

    pub fn str(&mut self) -> Result<String> {
        let bytes = self.bytes_ref()?;
        String::from_utf8(bytes.to_vec()).map_err(|_| PackError::InvalidUtf8)
    }

    pub fn str_list(&mut self) -> Result<Vec<String>> {
        let count = self.u32()? as usize;
        let mut out = Vec::with_capacity(count.min(4096));
        for _ in 0..count {
            out.push(self.str()?);
        }
        Ok(out)
    }

    pub fn byte_list(&mut self) -> Result<Vec<Vec<u8>>> {
        let count = self.u32()? as usize;
        let mut out = Vec::with_capacity(count.min(4096));
        for _ in 0..count {
            out.push(self.bytes()?);
        }
        Ok(out)
    }

    /// True once every written byte has been consumed.
    pub fn is_empty(&self) -> bool {
        self.pos >= self.buf.len()
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.pos.checked_add(n).ok_or(PackError::UnexpectedEof)?;
        let slice = self
            .buf
            .get(self.pos..end)
            .ok_or(PackError::UnexpectedEof)?;
        self.pos = end;
        Ok(slice)
    }
}

/// Lossy-converts a borrowed byte blob to text for logging and diagnostics.
pub fn debug_text(bytes: &[u8]) -> Cow<'_, str> {
    String::from_utf8_lossy(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integers_roundtrip() {
        let bytes = Writer::new()
            .u8(0xFF)
            .u16(0x1234)
            .u32(0xDEADBEEF)
            .u64(u64::MAX)
            .i64(-42)
            .finish();
        assert_eq!(bytes.len(), 1 + 2 + 4 + 8 + 8);

        let mut r = Reader::new(&bytes);
        assert_eq!(r.u8().unwrap(), 0xFF);
        assert_eq!(r.u16().unwrap(), 0x1234);
        assert_eq!(r.u32().unwrap(), 0xDEADBEEF);
        assert_eq!(r.u64().unwrap(), u64::MAX);
        assert_eq!(r.i64().unwrap(), -42);
        assert!(r.is_empty());
    }

    #[test]
    fn integers_roundtrip_at_bounds() {
        for v in [
            0u64,
            1,
            255,
            256,
            u16::MAX as u64,
            u32::MAX as u64,
            u64::MAX,
            i64::MIN as u64,
        ] {
            let bytes = Writer::new().u64(v).finish();
            assert_eq!(Reader::new(&bytes).u64().unwrap(), v);
        }
    }

    #[test]
    fn strings_roundtrip() {
        let bytes = Writer::new()
            .str("Yến Tiệc Của Sherlockian")
            .str("")
            .str("中文测试")
            .finish();

        let mut r = Reader::new(&bytes);
        assert_eq!(r.str().unwrap(), "Yến Tiệc Của Sherlockian");
        assert_eq!(r.str().unwrap(), "");
        assert_eq!(r.str().unwrap(), "中文测试");
        assert!(r.is_empty());
    }

    #[test]
    fn empty_and_long_lists() {
        let bytes = Writer::new()
            .str_list(&[])
            .str_list(&["a".into(), "bb".into(), "".into()])
            .finish();

        let mut r = Reader::new(&bytes);
        assert!(r.str_list().unwrap().is_empty());
        assert_eq!(r.str_list().unwrap(), vec!["a", "bb", ""]);
        assert!(r.is_empty());
    }

    #[test]
    fn byte_lists_roundtrip() {
        let items: Vec<Vec<u8>> = vec![vec![], vec![0, 1, 2, 255], vec![42; 300]];
        let bytes = Writer::new().byte_list(&items).finish();
        assert_eq!(Reader::new(&bytes).byte_list().unwrap(), items);
    }

    #[test]
    fn truncated_reads_error() {
        let bytes = Writer::new().u32(1).finish();
        let mut r = Reader::new(&bytes);
        assert_eq!(r.u32().unwrap(), 1);
        assert!(matches!(r.u8(), Err(PackError::UnexpectedEof)));
        assert!(matches!(r.u16(), Err(PackError::UnexpectedEof)));
        assert!(matches!(r.u64(), Err(PackError::UnexpectedEof)));
        assert!(matches!(r.str(), Err(PackError::UnexpectedEof)));
    }

    #[test]
    fn length_prefix_larger_than_buffer_errors() {
        // Claim 100 bytes, provide 3.
        let bytes = Writer::new().u32(100).bytes(&[1, 2, 3]).finish();
        // Truncate the header's claimed length by rewriting it as 3 bytes only.
        let bytes = bytes[..7].to_vec();
        assert!(matches!(
            Reader::new(&bytes).bytes(),
            Err(PackError::LengthOverrun)
        ));
    }

    #[test]
    fn invalid_utf8_is_rejected() {
        let bytes = Writer::new().bytes(&[0xFF, 0xFE, 0x00]).finish();
        assert!(matches!(
            Reader::new(&bytes).str(),
            Err(PackError::InvalidUtf8)
        ));
    }

    #[test]
    fn bool_rejects_non_zero_one() {
        let bytes = Writer::new().u8(7).finish();
        assert!(matches!(
            Reader::new(&bytes).bool(),
            Err(PackError::InvalidData(_))
        ));
    }

    #[test]
    fn bytes_ref_borrows_without_copying() {
        let payload = b"borrowed data".to_vec();
        let bytes = Writer::new().bytes(&payload).finish();
        let mut r = Reader::new(&bytes);
        let got = r.bytes_ref().unwrap();
        assert_eq!(got, payload.as_slice());
    }

    #[test]
    fn zero_length_reads_are_fine() {
        let bytes = Writer::new().str("").finish();
        let mut r = Reader::new(&bytes);
        assert_eq!(r.str().unwrap(), "");
    }

    #[test]
    fn debug_text_handles_invalid_utf8() {
        assert_eq!(debug_text(b"ok"), "ok");
        assert!(debug_text(&[0xFF]).contains('\u{FFFD}'));
    }
}
