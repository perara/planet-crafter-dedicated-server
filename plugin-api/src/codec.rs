//! The byte format events, query answers and state share: little-endian integers, strings as a
//! u32 length and UTF-8 bytes. A reader never panics and ignores bytes after the fields it knows
//! (that is how a newer server adds a field at the end of an event without breaking a plugin
//! built for the older one).

use alloc::string::String;
use alloc::vec::Vec;

/// Why bytes could not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    /// The bytes end before the event's fields do.
    Short,
    /// A string that is not UTF-8.
    Utf8,
    /// An event kind this API version does not know.
    UnknownKind(u32),
}

impl core::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            DecodeError::Short => f.write_str("the bytes end too soon"),
            DecodeError::Utf8 => f.write_str("a string is not UTF-8"),
            DecodeError::UnknownKind(k) => write!(f, "event kind {k} is not known to this API version"),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for DecodeError {}

/// Builds the bytes of an event, a query answer or a saved state. Each call appends and returns
/// the writer, so calls chain. The bytes are the public field.
#[derive(Default)]
pub struct Writer(pub Vec<u8>);

impl Writer {
    /// An empty writer.
    pub fn new() -> Writer {
        Writer(Vec::new())
    }
    /// Appends a byte.
    pub fn u8(&mut self, v: u8) -> &mut Self {
        self.0.push(v);
        self
    }
    /// Appends a flag, as one byte (0 or 1).
    pub fn bool(&mut self, v: bool) -> &mut Self {
        self.u8(v as u8)
    }
    /// Appends a little-endian `u32`.
    pub fn u32(&mut self, v: u32) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    /// Appends a little-endian `i32`.
    pub fn i32(&mut self, v: i32) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    /// Appends a little-endian `u64`.
    pub fn u64(&mut self, v: u64) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    /// Appends a little-endian `i64`.
    pub fn i64(&mut self, v: i64) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    /// Appends a little-endian `f32`.
    pub fn f32(&mut self, v: f32) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    /// Appends a little-endian `f64`.
    pub fn f64(&mut self, v: f64) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    /// Appends a string: a `u32` length and the UTF-8 bytes.
    pub fn str(&mut self, v: &str) -> &mut Self {
        self.bytes(v.as_bytes())
    }
    /// A `u32` length and the bytes (a string is the same, checked to be UTF-8 on reading).
    pub fn bytes(&mut self, v: &[u8]) -> &mut Self {
        self.u32(v.len() as u32);
        self.0.extend_from_slice(v);
        self
    }
}

/// Reads what `Writer` wrote. Every read returns `Err(DecodeError::Short)` instead of panicking
/// when the bytes end too soon.
pub struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    /// A reader over `bytes`.
    pub fn new(bytes: &'a [u8]) -> Reader<'a> {
        Reader(bytes)
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        if self.0.len() < n {
            return Err(DecodeError::Short);
        }
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        Ok(head)
    }
    /// Reads a byte.
    pub fn u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.take(1)?[0])
    }
    /// Reads a flag (any non-zero byte is true).
    pub fn bool(&mut self) -> Result<bool, DecodeError> {
        Ok(self.u8()? != 0)
    }
    /// Reads a little-endian `u32`.
    pub fn u32(&mut self) -> Result<u32, DecodeError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().map_err(|_| DecodeError::Short)?))
    }
    /// Reads a little-endian `i32`.
    pub fn i32(&mut self) -> Result<i32, DecodeError> {
        Ok(self.u32()? as i32)
    }
    /// Reads a little-endian `u64`.
    pub fn u64(&mut self) -> Result<u64, DecodeError> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().map_err(|_| DecodeError::Short)?))
    }
    /// Reads a little-endian `i64`.
    pub fn i64(&mut self) -> Result<i64, DecodeError> {
        Ok(self.u64()? as i64)
    }
    /// Reads a little-endian `f32`.
    pub fn f32(&mut self) -> Result<f32, DecodeError> {
        Ok(f32::from_bits(self.u32()?))
    }
    /// Reads a little-endian `f64`.
    pub fn f64(&mut self) -> Result<f64, DecodeError> {
        Ok(f64::from_bits(self.u64()?))
    }
    /// Reads a string, borrowed from the bytes. A string that is not UTF-8 is `DecodeError::Utf8`.
    pub fn str(&mut self) -> Result<&'a str, DecodeError> {
        let n = self.u32()? as usize;
        core::str::from_utf8(self.take(n)?).map_err(|_| DecodeError::Utf8)
    }
    /// Reads a `u32` length and that many bytes, borrowed.
    pub fn bytes(&mut self) -> Result<&'a [u8], DecodeError> {
        let n = self.u32()? as usize;
        self.take(n)
    }
    /// Reads a string and copies it.
    pub fn string(&mut self) -> Result<String, DecodeError> {
        self.str().map(String::from)
    }
    /// The bytes not read yet.
    pub fn rest(&self) -> &'a [u8] {
        self.0
    }
}
