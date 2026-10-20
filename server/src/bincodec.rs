//! The snapshot's binary codec (handoff format 4): serde's data model written with no names and
//! no tags, in declaration order, so a struct is its fields back to back. Not self-describing:
//! a type must be read as the type it was written as (the snapshot's head names the format,
//! the format fixes the types). Little-endian; integers of 2 bytes and more, the lengths and the
//! enum variants are LEB128 (zigzag for the signed: most ids and counts are small), floats and
//! bytes fixed width.
//!
//! Why not the text: serde_json writes every field's name and every number as digits, and reads
//! them back by matching names and parsing digits; the same state is 4 to 7 times smaller and
//! several times faster here (docs/hot-update.md, "Flip speed").
//!
//! `Value` is not readable without a tag, so a Value inside the state is written through
//! [`json_value`] (as its JSON text), and a byte string through [`bytes`] (one copy instead of
//! one element at a time): both fall back to the ordinary serde form where `is_human_readable`
//! is true (JSON), so the JSON snapshots of format 3 read and write as before.

use serde::de::{self, DeserializeSeed, IntoDeserializer, Visitor};
use serde::{ser, Deserialize, Serialize};

/// One pointer wide: a `Result<f32, Error>` then travels in registers, which the happy path of
/// every value read depends on.
#[derive(Debug)]
pub struct Error(Box<str>);

impl Error {
    fn new(m: impl Into<String>) -> Error {
        Error(m.into().into_boxed_str())
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Error {}
impl ser::Error for Error {
    fn custom<T: std::fmt::Display>(m: T) -> Self {
        Error::new(m.to_string())
    }
}
impl de::Error for Error {
    fn custom<T: std::fmt::Display>(m: T) -> Self {
        Error::new(m.to_string())
    }
}

type Result<T> = std::result::Result<T, Error>;

#[cold]
fn err<T>(m: &str) -> Result<T> {
    Err(Error::new(m))
}

// ---- writing ----------------------------------------------------------------------------------

#[cfg(test)]
pub fn to_vec<T: Serialize + ?Sized>(v: &T) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    v.serialize(&mut Writer(&mut out))?;
    Ok(out)
}

/// Appends to `out`.
pub fn append<T: Serialize + ?Sized>(v: &T, out: &mut Vec<u8>) -> Result<()> {
    v.serialize(&mut Writer(out))
}

pub struct Writer<'a>(&'a mut Vec<u8>);

/// Small magnitudes of either sign become small numbers.
#[inline]
fn zigzag(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

#[inline]
fn unzigzag(z: u64) -> i64 {
    (z >> 1) as i64 ^ -((z & 1) as i64)
}

#[inline]
fn varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push(v as u8 | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

impl<'a, 'b> ser::Serializer for &'b mut Writer<'a> {
    type Ok = ();
    type Error = Error;
    type SerializeSeq = Self;
    type SerializeTuple = Self;
    type SerializeTupleStruct = Self;
    type SerializeTupleVariant = Self;
    type SerializeMap = Self;
    type SerializeStruct = Self;
    type SerializeStructVariant = Self;

    fn is_human_readable(&self) -> bool {
        false
    }
    fn serialize_bool(self, v: bool) -> Result<()> {
        self.0.push(v as u8);
        Ok(())
    }
    fn serialize_i8(self, v: i8) -> Result<()> {
        self.0.push(v as u8);
        Ok(())
    }
    fn serialize_i16(self, v: i16) -> Result<()> {
        varint(self.0, zigzag(v as i64));
        Ok(())
    }
    fn serialize_i32(self, v: i32) -> Result<()> {
        varint(self.0, zigzag(v as i64));
        Ok(())
    }
    fn serialize_i64(self, v: i64) -> Result<()> {
        varint(self.0, zigzag(v));
        Ok(())
    }
    fn serialize_i128(self, v: i128) -> Result<()> {
        self.0.extend_from_slice(&v.to_le_bytes());
        Ok(())
    }
    fn serialize_u8(self, v: u8) -> Result<()> {
        self.0.push(v);
        Ok(())
    }
    fn serialize_u16(self, v: u16) -> Result<()> {
        varint(self.0, v as u64);
        Ok(())
    }
    fn serialize_u32(self, v: u32) -> Result<()> {
        varint(self.0, v as u64);
        Ok(())
    }
    fn serialize_u64(self, v: u64) -> Result<()> {
        varint(self.0, v);
        Ok(())
    }
    fn serialize_u128(self, v: u128) -> Result<()> {
        self.0.extend_from_slice(&v.to_le_bytes());
        Ok(())
    }
    fn serialize_f32(self, v: f32) -> Result<()> {
        self.0.extend_from_slice(&v.to_le_bytes());
        Ok(())
    }
    fn serialize_f64(self, v: f64) -> Result<()> {
        self.0.extend_from_slice(&v.to_le_bytes());
        Ok(())
    }
    fn serialize_char(self, v: char) -> Result<()> {
        self.serialize_u32(v as u32)
    }
    fn serialize_str(self, v: &str) -> Result<()> {
        varint(self.0, v.len() as u64);
        self.0.extend_from_slice(v.as_bytes());
        Ok(())
    }
    fn serialize_bytes(self, v: &[u8]) -> Result<()> {
        varint(self.0, v.len() as u64);
        self.0.extend_from_slice(v);
        Ok(())
    }
    fn serialize_none(self) -> Result<()> {
        self.0.push(0);
        Ok(())
    }
    fn serialize_some<T: Serialize + ?Sized>(self, v: &T) -> Result<()> {
        self.0.push(1);
        v.serialize(self)
    }
    fn serialize_unit(self) -> Result<()> {
        Ok(())
    }
    fn serialize_unit_struct(self, _: &'static str) -> Result<()> {
        Ok(())
    }
    fn serialize_unit_variant(self, _: &'static str, index: u32, _: &'static str) -> Result<()> {
        varint(self.0, index as u64);
        Ok(())
    }
    fn serialize_newtype_struct<T: Serialize + ?Sized>(self, _: &'static str, v: &T) -> Result<()> {
        v.serialize(self)
    }
    fn serialize_newtype_variant<T: Serialize + ?Sized>(self, _: &'static str, index: u32, _: &'static str, v: &T) -> Result<()> {
        varint(self.0, index as u64);
        v.serialize(self)
    }
    fn serialize_seq(self, len: Option<usize>) -> Result<Self> {
        let Some(len) = len else { return err("a sequence of unknown length") };
        varint(self.0, len as u64);
        Ok(self)
    }
    fn serialize_tuple(self, _: usize) -> Result<Self> {
        Ok(self)
    }
    fn serialize_tuple_struct(self, _: &'static str, _: usize) -> Result<Self> {
        Ok(self)
    }
    fn serialize_tuple_variant(self, _: &'static str, index: u32, _: &'static str, _: usize) -> Result<Self> {
        varint(self.0, index as u64);
        Ok(self)
    }
    fn serialize_map(self, len: Option<usize>) -> Result<Self> {
        let Some(len) = len else { return err("a map of unknown length") };
        varint(self.0, len as u64);
        Ok(self)
    }
    fn serialize_struct(self, _: &'static str, _: usize) -> Result<Self> {
        Ok(self)
    }
    fn serialize_struct_variant(self, _: &'static str, index: u32, _: &'static str, _: usize) -> Result<Self> {
        varint(self.0, index as u64);
        Ok(self)
    }
}

impl ser::SerializeSeq for &mut Writer<'_> {
    type Ok = ();
    type Error = Error;
    fn serialize_element<T: Serialize + ?Sized>(&mut self, v: &T) -> Result<()> {
        v.serialize(&mut **self)
    }
    fn end(self) -> Result<()> {
        Ok(())
    }
}
impl ser::SerializeTuple for &mut Writer<'_> {
    type Ok = ();
    type Error = Error;
    fn serialize_element<T: Serialize + ?Sized>(&mut self, v: &T) -> Result<()> {
        v.serialize(&mut **self)
    }
    fn end(self) -> Result<()> {
        Ok(())
    }
}
impl ser::SerializeTupleStruct for &mut Writer<'_> {
    type Ok = ();
    type Error = Error;
    fn serialize_field<T: Serialize + ?Sized>(&mut self, v: &T) -> Result<()> {
        v.serialize(&mut **self)
    }
    fn end(self) -> Result<()> {
        Ok(())
    }
}
impl ser::SerializeTupleVariant for &mut Writer<'_> {
    type Ok = ();
    type Error = Error;
    fn serialize_field<T: Serialize + ?Sized>(&mut self, v: &T) -> Result<()> {
        v.serialize(&mut **self)
    }
    fn end(self) -> Result<()> {
        Ok(())
    }
}
impl ser::SerializeMap for &mut Writer<'_> {
    type Ok = ();
    type Error = Error;
    fn serialize_key<T: Serialize + ?Sized>(&mut self, k: &T) -> Result<()> {
        k.serialize(&mut **self)
    }
    fn serialize_value<T: Serialize + ?Sized>(&mut self, v: &T) -> Result<()> {
        v.serialize(&mut **self)
    }
    fn end(self) -> Result<()> {
        Ok(())
    }
}
impl ser::SerializeStruct for &mut Writer<'_> {
    type Ok = ();
    type Error = Error;
    fn serialize_field<T: Serialize + ?Sized>(&mut self, _: &'static str, v: &T) -> Result<()> {
        v.serialize(&mut **self)
    }
    fn end(self) -> Result<()> {
        Ok(())
    }
}
impl ser::SerializeStructVariant for &mut Writer<'_> {
    type Ok = ();
    type Error = Error;
    fn serialize_field<T: Serialize + ?Sized>(&mut self, _: &'static str, v: &T) -> Result<()> {
        v.serialize(&mut **self)
    }
    fn end(self) -> Result<()> {
        Ok(())
    }
}

// ---- reading ----------------------------------------------------------------------------------

/// The whole of `bytes` read as a `T`; Err when it ends early or has bytes left over.
pub fn from_slice<'de, T: Deserialize<'de>>(bytes: &'de [u8]) -> Result<T> {
    let mut r = Reader(bytes);
    let v = T::deserialize(&mut r)?;
    if r.0.is_empty() { Ok(v) } else { err("bytes left after the value") }
}

/// A sequence of exactly `out.len()` elements read into `out` (one chunk of a bigger Vec being
/// filled on several threads). Ok: every element of `out` is initialized. Err: none is (what
/// was read before the error is dropped).
pub fn fill<'de, T: Deserialize<'de>>(bytes: &'de [u8], out: &mut [std::mem::MaybeUninit<T>]) -> Result<()> {
    let mut r = Reader(bytes);
    if r.len()? != out.len() {
        return err("a chunk of another length than its directory says");
    }
    let mut done = 0;
    let mut result = Ok(());
    for slot in out.iter_mut() {
        match T::deserialize(&mut r) {
            Ok(v) => {
                slot.write(v);
                done += 1;
            }
            Err(e) => {
                result = Err(e);
                break;
            }
        }
    }
    if result.is_ok() && !r.0.is_empty() {
        result = err("bytes left after the chunk");
    }
    if result.is_err() {
        // SAFETY: exactly the first `done` elements were written above, each once.
        out[..done].iter_mut().for_each(|slot| unsafe { slot.assume_init_drop() });
    }
    result
}

pub struct Reader<'de>(&'de [u8]);

impl<'de> Reader<'de> {
    #[inline(always)]
    fn take(&mut self, n: usize) -> Result<&'de [u8]> {
        if self.0.len() < n {
            return err("the snapshot ends inside a value");
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Ok(a)
    }
    #[inline(always)]
    fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        let a = self.take(N)?;
        Ok(a.try_into().unwrap())
    }
    #[inline(always)]
    fn varint(&mut self) -> Result<u64> {
        // Most are one byte.
        if let Some((&b, rest)) = self.0.split_first()
            && b < 0x80
        {
            self.0 = rest;
            return Ok(b as u64);
        }
        self.varint_long()
    }

    #[inline(never)]
    fn varint_long(&mut self) -> Result<u64> {
        let mut v = 0u64;
        for shift in (0..70).step_by(7) {
            let b = self.take(1)?[0];
            v |= ((b & 0x7f) as u64) << shift;
            if b < 0x80 {
                return Ok(v);
            }
        }
        err("a number longer than 64 bits")
    }
    /// A length: no more elements than bytes left (every element takes a byte, one of the
    /// types here taking none is a unit), so a damaged length cannot ask for gigabytes.
    fn len(&mut self) -> Result<usize> {
        let n = self.varint()?;
        if n > self.0.len() as u64 {
            return err("a length beyond the snapshot");
        }
        Ok(n as usize)
    }
}

macro_rules! fixed {
    ($name:ident, $visit:ident, $t:ty) => {
        fn $name<V: Visitor<'de>>(self, v: V) -> Result<V::Value> {
            v.$visit(<$t>::from_le_bytes(self.array()?))
        }
    };
}

impl<'de> de::Deserializer<'de> for &mut Reader<'de> {
    type Error = Error;

    fn is_human_readable(&self) -> bool {
        false
    }
    fn deserialize_any<V: Visitor<'de>>(self, _: V) -> Result<V::Value> {
        err("the binary snapshot is not self-describing (a Value or an untagged type in the state?)")
    }
    fn deserialize_bool<V: Visitor<'de>>(self, v: V) -> Result<V::Value> {
        match self.take(1)?[0] {
            0 => v.visit_bool(false),
            1 => v.visit_bool(true),
            b => err(&format!("{b} is no bool")),
        }
    }
    fixed!(deserialize_i8, visit_i8, i8);
    fn deserialize_i16<V: Visitor<'de>>(self, v: V) -> Result<V::Value> {
        let n = unzigzag(self.varint()?);
        v.visit_i16(i16::try_from(n).map_err(|_| Error::new("a number too big for 16 bits"))?)
    }
    fn deserialize_i32<V: Visitor<'de>>(self, v: V) -> Result<V::Value> {
        let n = unzigzag(self.varint()?);
        v.visit_i32(i32::try_from(n).map_err(|_| Error::new("a number too big for 32 bits"))?)
    }
    fn deserialize_i64<V: Visitor<'de>>(self, v: V) -> Result<V::Value> {
        v.visit_i64(unzigzag(self.varint()?))
    }
    fixed!(deserialize_i128, visit_i128, i128);
    fixed!(deserialize_u8, visit_u8, u8);
    fn deserialize_u16<V: Visitor<'de>>(self, v: V) -> Result<V::Value> {
        v.visit_u16(u16::try_from(self.varint()?).map_err(|_| Error::new("a number too big for 16 bits"))?)
    }
    fn deserialize_u32<V: Visitor<'de>>(self, v: V) -> Result<V::Value> {
        v.visit_u32(u32::try_from(self.varint()?).map_err(|_| Error::new("a number too big for 32 bits"))?)
    }
    fn deserialize_u64<V: Visitor<'de>>(self, v: V) -> Result<V::Value> {
        v.visit_u64(self.varint()?)
    }
    fixed!(deserialize_u128, visit_u128, u128);
    fixed!(deserialize_f32, visit_f32, f32);
    fixed!(deserialize_f64, visit_f64, f64);
    fn deserialize_char<V: Visitor<'de>>(self, v: V) -> Result<V::Value> {
        let c = u32::from_le_bytes(self.array()?);
        v.visit_char(char::from_u32(c).ok_or_else(|| Error::new(format!("{c} is no char")))?)
    }
    fn deserialize_str<V: Visitor<'de>>(self, v: V) -> Result<V::Value> {
        let n = self.len()?;
        let s = std::str::from_utf8(self.take(n)?).map_err(|e| Error::new(e.to_string()))?;
        v.visit_borrowed_str(s)
    }
    fn deserialize_string<V: Visitor<'de>>(self, v: V) -> Result<V::Value> {
        self.deserialize_str(v)
    }
    fn deserialize_bytes<V: Visitor<'de>>(self, v: V) -> Result<V::Value> {
        let n = self.len()?;
        v.visit_borrowed_bytes(self.take(n)?)
    }
    fn deserialize_byte_buf<V: Visitor<'de>>(self, v: V) -> Result<V::Value> {
        self.deserialize_bytes(v)
    }
    fn deserialize_option<V: Visitor<'de>>(self, v: V) -> Result<V::Value> {
        match self.take(1)?[0] {
            0 => v.visit_none(),
            1 => v.visit_some(self),
            b => err(&format!("{b} is no option tag")),
        }
    }
    fn deserialize_unit<V: Visitor<'de>>(self, v: V) -> Result<V::Value> {
        v.visit_unit()
    }
    fn deserialize_unit_struct<V: Visitor<'de>>(self, _: &'static str, v: V) -> Result<V::Value> {
        v.visit_unit()
    }
    fn deserialize_newtype_struct<V: Visitor<'de>>(self, _: &'static str, v: V) -> Result<V::Value> {
        v.visit_newtype_struct(self)
    }
    fn deserialize_seq<V: Visitor<'de>>(self, v: V) -> Result<V::Value> {
        let n = self.len()?;
        v.visit_seq(Counted { r: self, left: n })
    }
    fn deserialize_tuple<V: Visitor<'de>>(self, n: usize, v: V) -> Result<V::Value> {
        v.visit_seq(Counted { r: self, left: n })
    }
    fn deserialize_tuple_struct<V: Visitor<'de>>(self, _: &'static str, n: usize, v: V) -> Result<V::Value> {
        v.visit_seq(Counted { r: self, left: n })
    }
    fn deserialize_map<V: Visitor<'de>>(self, v: V) -> Result<V::Value> {
        let n = self.len()?;
        v.visit_map(Counted { r: self, left: n })
    }
    fn deserialize_struct<V: Visitor<'de>>(self, _: &'static str, fields: &'static [&'static str], v: V) -> Result<V::Value> {
        v.visit_seq(Counted { r: self, left: fields.len() })
    }
    fn deserialize_enum<V: Visitor<'de>>(self, _: &'static str, _: &'static [&'static str], v: V) -> Result<V::Value> {
        v.visit_enum(self)
    }
    fn deserialize_identifier<V: Visitor<'de>>(self, v: V) -> Result<V::Value> {
        v.visit_u32(self.varint()? as u32)
    }
    fn deserialize_ignored_any<V: Visitor<'de>>(self, _: V) -> Result<V::Value> {
        err("a binary snapshot cannot skip a value it does not know")
    }
}

/// `left` elements (or entries) to go.
struct Counted<'a, 'de> {
    r: &'a mut Reader<'de>,
    left: usize,
}

impl<'de> de::SeqAccess<'de> for Counted<'_, 'de> {
    type Error = Error;
    #[inline]
    fn next_element_seed<T: DeserializeSeed<'de>>(&mut self, seed: T) -> Result<Option<T::Value>> {
        if self.left == 0 {
            return Ok(None);
        }
        self.left -= 1;
        seed.deserialize(&mut *self.r).map(Some)
    }
    fn size_hint(&self) -> Option<usize> {
        Some(self.left)
    }
}

impl<'de> de::MapAccess<'de> for Counted<'_, 'de> {
    type Error = Error;
    fn next_key_seed<K: DeserializeSeed<'de>>(&mut self, seed: K) -> Result<Option<K::Value>> {
        if self.left == 0 {
            return Ok(None);
        }
        self.left -= 1;
        seed.deserialize(&mut *self.r).map(Some)
    }
    fn next_value_seed<V: DeserializeSeed<'de>>(&mut self, seed: V) -> Result<V::Value> {
        seed.deserialize(&mut *self.r)
    }
    fn size_hint(&self) -> Option<usize> {
        Some(self.left)
    }
}

impl<'de> de::EnumAccess<'de> for &mut Reader<'de> {
    type Error = Error;
    type Variant = Self;
    fn variant_seed<V: DeserializeSeed<'de>>(self, seed: V) -> Result<(V::Value, Self)> {
        let index = self.varint()? as u32;
        let v = seed.deserialize(index.into_deserializer())?;
        Ok((v, self))
    }
}

impl<'de> de::VariantAccess<'de> for &mut Reader<'de> {
    type Error = Error;
    fn unit_variant(self) -> Result<()> {
        Ok(())
    }
    fn newtype_variant_seed<T: DeserializeSeed<'de>>(self, seed: T) -> Result<T::Value> {
        seed.deserialize(self)
    }
    fn tuple_variant<V: Visitor<'de>>(self, n: usize, v: V) -> Result<V::Value> {
        v.visit_seq(Counted { r: self, left: n })
    }
    fn struct_variant<V: Visitor<'de>>(self, fields: &'static [&'static str], v: V) -> Result<V::Value> {
        v.visit_seq(Counted { r: self, left: fields.len() })
    }
}

// ---- fields that need help ----------------------------------------------------------------------

/// `#[serde(with = "crate::bincodec::bytes")]` on a `Vec<u8>`: one copy in the binary codec; in
/// JSON the array of numbers it always was.
pub mod bytes {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(v: &[u8], s: S) -> Result<S::Ok, S::Error> {
        if s.is_human_readable() { v.serialize(s) } else { s.serialize_bytes(v) }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        if d.is_human_readable() {
            return Vec::<u8>::deserialize(d);
        }
        struct V;
        impl<'de> serde::de::Visitor<'de> for V {
            type Value = Vec<u8>;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("bytes")
            }
            fn visit_borrowed_bytes<E>(self, v: &'de [u8]) -> Result<Vec<u8>, E> {
                Ok(v.to_vec())
            }
        }
        d.deserialize_bytes(V)
    }
}

/// `#[serde(with = "crate::bincodec::floats")]` on a `[f32; N]` that is mostly all zeros (a world
/// object's position of an item in a backpack...): one byte for zeros in the binary codec, a
/// tag and the floats otherwise; a JSON array as ever.
pub mod floats {
    use serde::ser::SerializeTuple;
    use serde::{Deserializer, Serializer};

    pub fn serialize<S: Serializer, const N: usize>(v: &[f32; N], s: S) -> Result<S::Ok, S::Error> {
        if s.is_human_readable() {
            let mut t = s.serialize_tuple(N)?;
            for f in v {
                t.serialize_element(f)?;
            }
            return t.end();
        }
        // -0.0 is not a zero here: the bits come back as they went.
        if v.iter().all(|f| f.to_bits() == 0) {
            let mut t = s.serialize_tuple(1)?;
            t.serialize_element(&0u8)?;
            return t.end();
        }
        let mut t = s.serialize_tuple(1 + N)?;
        t.serialize_element(&1u8)?;
        for f in v {
            t.serialize_element(f)?;
        }
        t.end()
    }

    pub fn deserialize<'de, D: Deserializer<'de>, const N: usize>(d: D) -> Result<[f32; N], D::Error> {
        struct V<const N: usize>(bool);
        impl<'de, const N: usize> serde::de::Visitor<'de> for V<N> {
            type Value = [f32; N];
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("floats")
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<[f32; N], A::Error> {
                let tag: u8 = if self.0 { 1 } else { seq.next_element()?.ok_or_else(|| serde::de::Error::custom("no tag"))? };
                let mut out = [0.0f32; N];
                if tag == 1 {
                    for f in out.iter_mut() {
                        *f = seq.next_element()?.ok_or_else(|| serde::de::Error::custom("too few floats"))?;
                    }
                } else if tag != 0 {
                    return Err(serde::de::Error::custom("a tag of floats"));
                }
                Ok(out)
            }
        }
        let human = d.is_human_readable();
        d.deserialize_tuple(if human { N } else { 1 + N }, V(human))
    }
}

/// `#[serde(with = "crate::bincodec::byte_lists")]` on a `Vec<Vec<u8>>`.
pub mod byte_lists {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    struct One<'a>(&'a [u8]);
    impl Serialize for One<'_> {
        fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
            super::bytes::serialize(self.0, s)
        }
    }
    struct Owned(Vec<u8>);
    impl<'de> Deserialize<'de> for Owned {
        fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            super::bytes::deserialize(d).map(Owned)
        }
    }

    pub fn serialize<S: Serializer>(v: &[Vec<u8>], s: S) -> Result<S::Ok, S::Error> {
        s.collect_seq(v.iter().map(|b| One(b)))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<Vec<u8>>, D::Error> {
        Ok(Vec::<Owned>::deserialize(d)?.into_iter().map(|o| o.0).collect())
    }
}

/// `#[serde(with = "crate::bincodec::json_value")]` on a `serde_json::Value`: its JSON text in
/// the binary codec (the tables' own JSON, copied into the state when an object appears).
pub mod json_value {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use serde_json::Value;

    pub fn serialize<S: Serializer>(v: &Value, s: S) -> Result<S::Ok, S::Error> {
        if s.is_human_readable() {
            return v.serialize(s);
        }
        s.serialize_bytes(&serde_json::to_vec(v).map_err(serde::ser::Error::custom)?)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Value, D::Error> {
        if d.is_human_readable() {
            return Value::deserialize(d);
        }
        let text: &'de [u8] = Deserialize::deserialize(d)?;
        serde_json::from_slice(text).map_err(serde::de::Error::custom)
    }
}

/// The same for a `Vec<Value>`.
pub mod json_values {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use serde_json::Value;

    struct One<'a>(&'a Value);
    impl Serialize for One<'_> {
        fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
            super::json_value::serialize(self.0, s)
        }
    }
    struct Owned(Value);
    impl<'de> Deserialize<'de> for Owned {
        fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            super::json_value::deserialize(d).map(Owned)
        }
    }

    pub fn serialize<S: Serializer>(v: &[Value], s: S) -> Result<S::Ok, S::Error> {
        s.collect_seq(v.iter().map(One))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<Value>, D::Error> {
        Ok(Vec::<Owned>::deserialize(d)?.into_iter().map(|o| o.0).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, HashMap};

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    enum Shape {
        Unit,
        New(i32),
        Pair(u64, f32),
        Named { a: i64, b: Option<String> },
    }

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Thing {
        id: u64,
        small: i64,
        name: String,
        pos: [f32; 3],
        shapes: Vec<Shape>,
        pairs: Vec<(i32, bool)>,
        #[serde(with = "crate::bincodec::bytes")]
        raw: Vec<u8>,
        #[serde(with = "crate::bincodec::byte_lists")]
        lists: Vec<Vec<u8>>,
        #[serde(with = "crate::bincodec::json_value")]
        any: serde_json::Value,
        #[serde(with = "crate::bincodec::json_values")]
        many: Vec<serde_json::Value>,
        map: BTreeMap<String, u16>,
        hash: HashMap<u64, Vec<i32>>,
        none: Option<u8>,
        #[serde(with = "crate::bincodec::floats")]
        zeros: [f32; 4],
        #[serde(with = "crate::bincodec::floats")]
        some: [f32; 3],
        #[serde(with = "crate::bincodec::floats")]
        negative_zero: [f32; 2],
        ints: (i16, u16, i32, u32, i64),
        t: (),
    }

    #[derive(Serialize, Deserialize)]
    struct Z(#[serde(with = "crate::bincodec::floats")] [f32; 8]);

    fn thing() -> Thing {
        Thing {
            id: u64::MAX,
            small: -300,
            name: "héllo".into(),
            pos: [1.5, f32::INFINITY, -0.0],
            shapes: vec![Shape::Unit, Shape::New(-7), Shape::Pair(1 << 40, 2.5), Shape::Named { a: i64::MIN, b: Some("x".into()) }, Shape::Named { a: 0, b: None }],
            pairs: vec![(1, true), (-2, false)],
            raw: (0..=255).collect(),
            lists: vec![vec![], vec![1, 2, 3]],
            any: serde_json::json!({"a": [1, 2.5, null, "s"], "b": {"c": true}}),
            many: vec![serde_json::json!(1), serde_json::json!({"k": "v"})],
            map: [("b".to_string(), 2), ("a".to_string(), 1)].into(),
            hash: [(7, vec![1, 2]), (u64::MAX, vec![])].into(),
            none: None,
            zeros: [0.0; 4],
            some: [0.0, 5.5, 0.0],
            negative_zero: [0.0, -0.0],
            ints: (-1, 65535, i32::MIN, u32::MAX, -1),
            t: (),
        }
    }

    #[test]
    fn what_is_written_reads_back_exactly() {
        let t = thing();
        let bytes = to_vec(&t).unwrap();
        let back = from_slice::<Thing>(&bytes).unwrap();
        assert_eq!(back, t);
        assert_eq!(back.negative_zero[1].to_bits(), (-0.0f32).to_bits(), "-0.0 is not a zero to elide");
        // A float array of zeros is one byte; the small numbers are short.
        assert_eq!(to_vec(&(crate::bincodec::tests::Z([0.0; 8]), 1u8)).unwrap().len(), 2);
        assert_eq!(to_vec(&(1i32, -1i32, 300u32, 7u16)).unwrap().len(), 1 + 1 + 2 + 1);
        // NaN keeps its bits (JSON could not carry it).
        let nan = f32::from_bits(0x7fc0_1234);
        assert_eq!(from_slice::<f32>(&to_vec(&nan).unwrap()).unwrap().to_bits(), nan.to_bits());
    }

    #[test]
    fn the_same_types_are_json_when_the_format_is_json() {
        // JSON has no infinity: the one thing it cannot carry.
        let t = Thing { pos: [1.5, 2.0, -0.0], ..thing() };
        let json = serde_json::to_string(&t).unwrap();
        assert_eq!(serde_json::from_str::<Thing>(&json).unwrap(), t);
        assert!(json.contains("\"raw\":[0,1,2"), "bytes are a JSON array of numbers, as before");
    }

    #[test]
    fn damaged_snapshots_are_errors_not_panics_or_huge_allocations() {
        let bytes = to_vec(&thing()).unwrap();
        for cut in [0, 1, 5, bytes.len() / 2, bytes.len() - 1] {
            assert!(from_slice::<Thing>(&bytes[..cut]).is_err(), "cut at {cut}");
        }
        let mut more = bytes.clone();
        more.push(0);
        assert!(from_slice::<Thing>(&more).is_err(), "bytes left over");
        // A Vec claiming u64::MAX elements.
        assert!(from_slice::<Vec<u8>>(&[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01]).is_err());
        assert!(from_slice::<Vec<u32>>(&[200, 1]).is_err());
        assert!(from_slice::<bool>(&[2]).is_err());
    }

    /// By hand: cargo test --release bincodec::tests::speed -- --ignored --nocapture
    #[test]
    #[ignore]
    fn speed() {
        use crate::model::WorldObject;
        let wo = |i: i32| WorldObject {
            id: i,
            group: 17,
            position: [i as f32, 2.0, 3.0],
            rotation: [0.0, 0.0, 0.0, 1.0],
            color: [0.0; 4],
            text: String::new(),
            linked_groups: None,
            linked_inventory: 0,
            secondary_inventories: vec![],
            panels: None,
            growth: 0.0,
            setting: 0,
            trait_type: 0,
            trait_value: 0,
            hunger: 0.0,
            pet_time: crate::clock::GameTime::ZERO,
            count: [0, 0],
            linked_wo: 0,
            dont_save: false,
            lock_time: crate::clock::GameTime::ZERO,
            energy: 1.0,
            from_db: false,
            planet: 5,
            planet_linked: 0,
            unit_generation: [0.5; 8],
            unit_multiplier: [0.0; 8],
            net: Some(i as u64),
        };
        let items: Vec<Option<(i32, WorldObject)>> = (0..100_000).map(|i| Some((i, wo(i)))).collect();
        // This thread's CPU time: the host is shared, wall time is not the code's.
        let cpu = || {
            let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
            // SAFETY: a valid timespec for the call's duration.
            unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
            std::time::Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
        };
        for _ in 0..5 {
            let t = cpu();
            let bytes = to_vec(&items).unwrap();
            let w = cpu() - t;
            let t = cpu();
            let back: Vec<Option<(i32, WorldObject)>> = from_slice(&bytes).unwrap();
            let r = cpu() - t;
            println!("100k world objects: {} bytes, write {w:?} ({} ns each), read {r:?} ({} ns each)", bytes.len(), w.as_nanos() / 100_000, r.as_nanos() / 100_000);
            assert_eq!(back.len(), items.len());
        }
    }
}
