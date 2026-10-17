//! A minimal binary serialization format for on-disk databases.
//!
//! Values are little-endian; lengths and most integers use LEB128 varints, so
//! small values (the common case in device data) take one byte. There is no
//! schema: readers must consume fields in the order writers produced them.
//! Every file should start with a magic tag and a version written by the
//! owner of the format.

use std::fmt;

/// Appends encoded values to a byte buffer.
#[derive(Default, Debug, Clone)]
pub struct BinWriter {
    buf: Vec<u8>,
}

impl BinWriter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub fn raw(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    pub fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }

    pub fn bool(&mut self, v: bool) {
        self.buf.push(v as u8);
    }

    pub fn u32_fixed(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    /// Unsigned LEB128.
    pub fn uvar(&mut self, mut v: u64) {
        loop {
            let b = (v & 0x7F) as u8;
            v >>= 7;
            if v == 0 {
                self.buf.push(b);
                return;
            }
            self.buf.push(b | 0x80);
        }
    }

    /// Signed integer, zigzag-encoded then LEB128.
    pub fn ivar(&mut self, v: i64) {
        self.uvar(((v << 1) ^ (v >> 63)) as u64);
    }

    pub fn usize(&mut self, v: usize) {
        self.uvar(v as u64);
    }

    pub fn f32(&mut self, v: f32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn str(&mut self, s: &str) {
        self.usize(s.len());
        self.buf.extend_from_slice(s.as_bytes());
    }

    /// Writes a length followed by each element.
    pub fn seq<T>(&mut self, items: &[T], mut f: impl FnMut(&mut Self, &T)) {
        self.usize(items.len());
        for item in items {
            f(self, item);
        }
    }
}

/// An error while decoding: truncated or malformed data.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BinError {
    pub offset: usize,
    pub message: &'static str,
}

impl fmt::Display for BinError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "malformed data at byte {}: {}", self.offset, self.message)
    }
}

impl std::error::Error for BinError {}

/// Reads values written by [`BinWriter`].
#[derive(Debug, Clone)]
pub struct BinReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> BinReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn err<T>(&self, message: &'static str) -> Result<T, BinError> {
        Err(BinError { offset: self.pos, message })
    }

    pub fn at_end(&self) -> bool {
        self.pos == self.data.len()
    }

    pub fn raw(&mut self, n: usize) -> Result<&'a [u8], BinError> {
        if self.data.len() - self.pos < n {
            return self.err("unexpected end of data");
        }
        let s = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    pub fn u8(&mut self) -> Result<u8, BinError> {
        Ok(self.raw(1)?[0])
    }

    pub fn bool(&mut self) -> Result<bool, BinError> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => self.err("invalid bool"),
        }
    }

    pub fn u32_fixed(&mut self) -> Result<u32, BinError> {
        Ok(u32::from_le_bytes(self.raw(4)?.try_into().unwrap()))
    }

    pub fn uvar(&mut self) -> Result<u64, BinError> {
        let mut v = 0u64;
        let mut shift = 0;
        loop {
            let b = self.u8()?;
            if shift >= 64 || (shift == 63 && b > 1) {
                return self.err("varint overflow");
            }
            v |= ((b & 0x7F) as u64) << shift;
            if b & 0x80 == 0 {
                return Ok(v);
            }
            shift += 7;
        }
    }

    pub fn ivar(&mut self) -> Result<i64, BinError> {
        let u = self.uvar()?;
        Ok((u >> 1) as i64 ^ -((u & 1) as i64))
    }

    pub fn usize(&mut self) -> Result<usize, BinError> {
        let v = self.uvar()?;
        usize::try_from(v).or_else(|_| self.err("length overflow"))
    }

    /// Reads a varint that must fit in `u32`.
    pub fn u32(&mut self) -> Result<u32, BinError> {
        let v = self.uvar()?;
        u32::try_from(v).or_else(|_| self.err("value exceeds u32"))
    }

    /// Reads a varint that must fit in `u16`.
    pub fn u16(&mut self) -> Result<u16, BinError> {
        let v = self.uvar()?;
        u16::try_from(v).or_else(|_| self.err("value exceeds u16"))
    }

    pub fn f32(&mut self) -> Result<f32, BinError> {
        Ok(f32::from_le_bytes(self.raw(4)?.try_into().unwrap()))
    }

    pub fn str(&mut self) -> Result<&'a str, BinError> {
        let n = self.usize()?;
        let bytes = self.raw(n)?;
        std::str::from_utf8(bytes).or_else(|_| self.err("invalid UTF-8"))
    }

    pub fn string(&mut self) -> Result<String, BinError> {
        self.str().map(str::to_owned)
    }

    /// Reads a length-prefixed sequence.
    pub fn seq<T>(
        &mut self,
        mut f: impl FnMut(&mut Self) -> Result<T, BinError>,
    ) -> Result<Vec<T>, BinError> {
        let n = self.usize()?;
        // Guard against absurd lengths from corrupt data: each element takes
        // at least one byte.
        if n > self.data.len() - self.pos {
            return self.err("sequence length exceeds data");
        }
        let mut v = Vec::with_capacity(n);
        for _ in 0..n {
            v.push(f(self)?);
        }
        Ok(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let mut w = BinWriter::new();
        w.uvar(0);
        w.uvar(127);
        w.uvar(128);
        w.uvar(u64::MAX);
        w.ivar(-1);
        w.ivar(i64::MIN);
        w.ivar(300);
        w.str("héllo");
        w.seq(&[1u32, 2, 3], |w, &v| w.uvar(v as u64));
        w.f32(1.5);
        w.bool(true);
        let bytes = w.into_bytes();
        let mut r = BinReader::new(&bytes);
        assert_eq!(r.uvar().unwrap(), 0);
        assert_eq!(r.uvar().unwrap(), 127);
        assert_eq!(r.uvar().unwrap(), 128);
        assert_eq!(r.uvar().unwrap(), u64::MAX);
        assert_eq!(r.ivar().unwrap(), -1);
        assert_eq!(r.ivar().unwrap(), i64::MIN);
        assert_eq!(r.ivar().unwrap(), 300);
        assert_eq!(r.str().unwrap(), "héllo");
        assert_eq!(r.seq(|r| r.u32()).unwrap(), vec![1, 2, 3]);
        assert_eq!(r.f32().unwrap(), 1.5);
        assert!(r.bool().unwrap());
        assert!(r.at_end());
        assert!(r.u8().is_err());
    }

    #[test]
    fn rejects_corrupt_lengths() {
        let mut w = BinWriter::new();
        w.uvar(1 << 40);
        let bytes = w.into_bytes();
        assert!(BinReader::new(&bytes).seq(|r| r.u8()).is_err());
        assert!(BinReader::new(&[0xFF; 11]).uvar().is_err());
    }
}
