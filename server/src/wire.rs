//! Netcode for GameObjects byte formats: FastBufferWriter/Reader primitives, BytePacker
//! bit-packing, and message batches (u16 magic 4448, u16 count, i32 size, u64 XXHash64 of the
//! rest; then per message a bit-packed type, a bit-packed size and the body).

pub const BATCH_MAGIC: u16 = 4448;

#[derive(Default, Clone)]
pub struct Writer(pub Vec<u8>);

impl Writer {
    pub fn new() -> Self {
        Writer(Vec::new())
    }
    pub fn len(&self) -> usize {
        self.0.len()
    }
    pub fn bytes(&mut self, data: &[u8]) -> &mut Self {
        self.0.extend_from_slice(data);
        self
    }
    pub fn u8(&mut self, v: u8) -> &mut Self {
        self.0.push(v);
        self
    }
    pub fn bool(&mut self, v: bool) -> &mut Self {
        self.u8(v as u8)
    }
    pub fn u16(&mut self, v: u16) -> &mut Self {
        self.bytes(&v.to_le_bytes())
    }
    pub fn i32(&mut self, v: i32) -> &mut Self {
        self.bytes(&v.to_le_bytes())
    }
    pub fn u32(&mut self, v: u32) -> &mut Self {
        self.bytes(&v.to_le_bytes())
    }
    pub fn u64(&mut self, v: u64) -> &mut Self {
        self.bytes(&v.to_le_bytes())
    }
    pub fn f32(&mut self, v: f32) -> &mut Self {
        self.bytes(&v.to_le_bytes())
    }
    pub fn vec3(&mut self, v: [f32; 3]) -> &mut Self {
        v.iter().for_each(|c| {
            self.f32(*c);
        });
        self
    }
    pub fn quat(&mut self, v: [f32; 4]) -> &mut Self {
        v.iter().for_each(|c| {
            self.f32(*c);
        });
        self
    }

    /// BytePacker.WriteValueBitPacked(ushort): 2 low bits = byte count (3 = raw u16 follows).
    pub fn packed_u16(&mut self, v: u16) -> &mut Self {
        if v <= 0x3F {
            self.u8(((v << 2) | 1) as u8)
        } else if v <= 0x3FFF {
            self.u16((v << 2) | 2)
        } else {
            self.u8(3).u16(v)
        }
    }
    /// BytePacker.WriteValueBitPacked(uint): 3 low bits = byte count (5 = raw u32 follows).
    pub fn packed_u32(&mut self, v: u32) -> &mut Self {
        let shifted = (v as u64) << 3;
        let n = byte_count(shifted);
        if n <= 4 {
            self.bytes(&(shifted | n as u64).to_le_bytes()[..n])
        } else {
            self.u8(5).u32(v)
        }
    }
    /// BytePacker.WriteValueBitPacked(ulong): 4 low bits = byte count (9 = raw u64 follows).
    pub fn packed_u64(&mut self, v: u64) -> &mut Self {
        if v >> 60 != 0 {
            return self.u8(9).u64(v);
        }
        let shifted = v << 4;
        let n = byte_count(shifted);
        self.bytes(&(shifted | n as u64).to_le_bytes()[..n])
    }
    pub fn packed_i16(&mut self, v: i16) -> &mut Self {
        self.packed_u16(((v << 1) ^ (v >> 15)) as u16)
    }
    pub fn packed_i32(&mut self, v: i32) -> &mut Self {
        self.packed_u32(((v << 1) ^ (v >> 31)) as u32)
    }
    /// FastBufferWriter.WriteValueSafe(T[]) for primitives: i32 length, raw elements.
    pub fn i32_array(&mut self, values: &[i32]) -> &mut Self {
        self.i32(values.len() as i32);
        values.iter().for_each(|v| {
            self.i32(*v);
        });
        self
    }
}

/// Smallest byte count holding `v` (at least one byte).
fn byte_count(v: u64) -> usize {
    ((64 - v.leading_zeros()) as usize).div_ceil(8).max(1)
}

pub struct Reader<'a> {
    pub data: &'a [u8],
    pub pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Reader { data, pos: 0 }
    }
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let slice = self.data.get(self.pos..self.pos + n)?;
        self.pos += n;
        Some(slice)
    }
    pub fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }
    pub fn bool(&mut self) -> Option<bool> {
        Some(self.u8()? != 0)
    }
    pub fn i32(&mut self) -> Option<i32> {
        Some(i32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    pub fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    pub fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
    pub fn u16_raw(&mut self) -> Option<u16> {
        Some(u16::from_le_bytes(self.take(2)?.try_into().ok()?))
    }
    pub fn i16(&mut self) -> Option<i16> {
        Some(i16::from_le_bytes(self.take(2)?.try_into().ok()?))
    }
    pub fn f32(&mut self) -> Option<f32> {
        Some(f32::from_bits(self.u32()?))
    }
    pub fn skip(&mut self, n: usize) -> Option<()> {
        self.take(n).map(|_| ())
    }
    pub fn packed_i16(&mut self) -> Option<i16> {
        let v = self.packed_u16()?;
        Some(((v >> 1) as i16) ^ -((v & 1) as i16))
    }
    pub fn packed_u16(&mut self) -> Option<u16> {
        let n = (*self.data.get(self.pos)? & 3) as usize;
        match n {
            1 => Some((self.u8()? >> 2) as u16),
            2 => Some(u16::from_le_bytes(self.take(2)?.try_into().ok()?) >> 2),
            3 => {
                self.pos += 1;
                Some(u16::from_le_bytes(self.take(2)?.try_into().ok()?))
            }
            _ => None,
        }
    }
    pub fn packed_u32(&mut self) -> Option<u32> {
        let n = (*self.data.get(self.pos)? & 7) as usize;
        match n {
            1..=4 => {
                let mut b = [0u8; 4];
                b[..n].copy_from_slice(self.take(n)?);
                Some(u32::from_le_bytes(b) >> 3)
            }
            5 => {
                self.pos += 1;
                self.u32()
            }
            _ => None,
        }
    }
    pub fn packed_u64(&mut self) -> Option<u64> {
        let n = (*self.data.get(self.pos)? & 15) as usize;
        match n {
            1..=8 => {
                let mut b = [0u8; 8];
                b[..n].copy_from_slice(self.take(n)?);
                Some(u64::from_le_bytes(b) >> 4)
            }
            9 => {
                self.pos += 1;
                self.u64()
            }
            _ => None,
        }
    }
    pub fn packed_i32(&mut self) -> Option<i32> {
        let v = self.packed_u32()?;
        Some(((v >> 1) as i32) ^ -((v & 1) as i32))
    }
    /// FastBufferWriter.WriteValueSafe(string): u32 length, UTF-16LE characters.
    pub fn string(&mut self) -> Option<String> {
        let n = self.u32()? as usize;
        let raw = self.take(n * 2)?;
        let units: Vec<u16> = raw.chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        Some(String::from_utf16_lossy(&units))
    }
    pub fn rest(&self) -> &'a [u8] {
        &self.data[self.pos.min(self.data.len())..]
    }
}

/// Messages of one batch: (type, body).
pub fn messages(batch: &[u8]) -> Vec<(u32, &[u8])> {
    let mut out = Vec::new();
    if batch.len() < 16 || u16::from_le_bytes([batch[0], batch[1]]) != BATCH_MAGIC {
        return out;
    }
    let count = u16::from_le_bytes([batch[2], batch[3]]);
    let mut r = Reader::new(batch);
    r.pos = 16;
    for _ in 0..count {
        let (Some(kind), Some(size)) = (r.packed_u32(), r.packed_u32()) else { break };
        let Some(body) = batch.get(r.pos..r.pos + size as usize) else { break };
        r.pos += size as usize;
        out.push((kind, body));
    }
    out
}

/// One batch holding the given messages.
pub fn batch(messages: &[(u32, Vec<u8>)]) -> Vec<u8> {
    batch_of(messages.iter().map(|(kind, data)| (*kind, data.as_slice())))
}

/// One batch of (message type, body) pairs.
pub fn batch_of<'a>(messages: impl Iterator<Item = (u32, &'a [u8])>) -> Vec<u8> {
    let mut b = Batch::default();
    messages.for_each(|(kind, data)| b.push(kind, data));
    b.finish()
}

/// A batch being filled: the 16-byte header goes in front once the body (and so its length
/// and hash) is known.
#[derive(Default)]
pub struct Batch {
    out: Writer,
    count: u16,
}

impl Batch {
    pub fn push(&mut self, kind: u32, data: &[u8]) {
        if self.out.0.is_empty() {
            self.out.0.resize(16, 0);
        }
        self.out.packed_u32(kind).packed_u32(data.len() as u32).bytes(data);
        self.count = self.count.wrapping_add(1);
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    pub fn finish(mut self) -> Vec<u8> {
        if self.out.0.is_empty() {
            self.out.0.resize(16, 0);
        }
        let hash = xxhash_rust::xxh64::xxh64(&self.out.0[16..], 0);
        let length = self.out.0.len() as i32;
        let out = &mut self.out.0;
        out[0..2].copy_from_slice(&BATCH_MAGIC.to_le_bytes());
        out[2..4].copy_from_slice(&self.count.to_le_bytes());
        out[4..8].copy_from_slice(&length.to_le_bytes());
        out[8..16].copy_from_slice(&hash.to_le_bytes());
        self.out.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packing_matches_netcode_samples() {
        // From the manifest: Int16 0 -> 01, Int32 -1 -> 09, Int32 1 -> 11,
        // Int32 1768717124 -> 0588f6d8d2, Int32 -1140328421 -> 05c90ff087.
        assert_eq!(Writer::new().packed_i16(0).0, [0x01]);
        assert_eq!(Writer::new().packed_i32(-1).0, [0x09]);
        assert_eq!(Writer::new().packed_i32(1).0, [0x11]);
        assert_eq!(Writer::new().packed_i32(1768717124).0, hex("0588f6d8d2"));
        assert_eq!(Writer::new().packed_i32(-1140328421).0, hex("05c90ff087"));
        assert_eq!(Writer::new().packed_u64(0).0, [0x01]);
        for v in [0u32, 5, 31, 32, 1000, 1 << 20, u32::MAX >> 3, u32::MAX] {
            let w = Writer::new().packed_u32(v).0.clone();
            assert_eq!(Reader::new(&w).packed_u32(), Some(v), "u32 {v}");
        }
        for v in [0u64, 7, 1 << 40, u64::MAX >> 4, u64::MAX] {
            let w = Writer::new().packed_u64(v).0.clone();
            assert_eq!(Reader::new(&w).packed_u64(), Some(v), "u64 {v}");
        }
        for v in [0u16, 63, 64, 0x3FFF, 0x4000, u16::MAX] {
            let w = Writer::new().packed_u16(v).0.clone();
            assert_eq!(Reader::new(&w).packed_u16(), Some(v), "u16 {v}");
        }
    }

    #[test]
    fn batch_round_trip() {
        let b = batch(&[(21, vec![0x11]), (5, vec![1, 2, 3])]);
        let m = messages(&b);
        assert_eq!(m.len(), 2);
        assert_eq!(m[1], (5, &[1u8, 2, 3][..]));
    }

    pub fn hex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }
}
