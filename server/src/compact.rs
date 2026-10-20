//! Values for the long-lived collections that take less room than their obvious types: interned
//! strings (`Sym`: 8 bytes, shared by every holder) and short byte strings kept inside their owner
//! (`Bytes`: 24 bytes, no allocation up to 22). Both read as the `str` and `[u8]` they stand for and
//! are written by serde as those, so the snapshot, the save and the wire are the same bytes.
//!
//! Why: a world of 200,000 objects held a `String` for every class, variable name and type of its
//! 30,000 network objects (3 allocations of 32 bytes for each of 4.6 variables) and a `Vec<u8>` for
//! each value: 1.7 KB an object, most of it the same few hundred words and a few bytes.

use std::collections::HashSet;
use std::sync::{OnceLock, RwLock};

/// An interned string: every `Sym` of the same text points at the same leaked copy, so a `Sym` is
/// one pointer, copies for free and compares by address. The table only grows with the number of
/// distinct words (class, variable and type names of the game's tables: a few hundred), made when
/// the tables are read and when a snapshot is restored, never per tick.
#[derive(Clone, Copy)]
pub struct Sym(&'static &'static str);

fn table() -> &'static RwLock<HashSet<Sym>> {
    static TABLE: OnceLock<RwLock<HashSet<Sym>>> = OnceLock::new();
    TABLE.get_or_init(Default::default)
}

// The words this thread made lately, by a few of their letters: a snapshot restored says the same
// names and types for every network object, and a lookup in the table (a hash, a lock) for each costs
// more than the object did before it had words.
thread_local!(static RECENT: std::cell::RefCell<[Option<Sym>; 256]> = const { std::cell::RefCell::new([None; 256]) });

impl Sym {
    pub fn new(s: &str) -> Sym {
        let b = s.as_bytes();
        let letter = |at: Option<usize>| usize::from(at.and_then(|i| b.get(i)).copied().unwrap_or(0));
        let slot = (b.len().wrapping_mul(31) ^ (letter(Some(0)) << 1) ^ letter(b.len().checked_sub(1)) ^ (letter(Some(b.len() / 2)) << 3)) & 255;
        if let Some(sym) = RECENT.with_borrow(|recent| recent[slot]).filter(|sym| sym.as_str() == s) {
            return sym;
        }
        let sym = Sym::of(s);
        RECENT.with_borrow_mut(|recent| recent[slot] = Some(sym));
        sym
    }

    /// The word in the table, made if it is not there.
    fn of(s: &str) -> Sym {
        let table = table();
        if let Some(sym) = table.read().unwrap_or_else(|e| e.into_inner()).get(s) {
            return *sym;
        }
        let mut write = table.write().unwrap_or_else(|e| e.into_inner());
        if let Some(sym) = write.get(s) {
            return *sym;
        }
        let text: &'static str = Box::leak(Box::<str>::from(s));
        let sym = Sym(Box::leak(Box::new(text)));
        write.insert(sym);
        sym
    }

    pub fn as_str(&self) -> &'static str {
        self.0
    }

    /// The word's address: equal for equal words, the key of a table of what is made of words.
    pub fn addr(&self) -> usize {
        self.0 as *const &str as usize
    }
}

impl std::ops::Deref for Sym {
    type Target = str;
    fn deref(&self) -> &str {
        self.0
    }
}

impl std::borrow::Borrow<str> for Sym {
    fn borrow(&self) -> &str {
        self.0
    }
}

impl Default for Sym {
    fn default() -> Sym {
        Sym::new("")
    }
}

impl From<&str> for Sym {
    fn from(s: &str) -> Sym {
        Sym::new(s)
    }
}

impl From<String> for Sym {
    fn from(s: String) -> Sym {
        Sym::new(&s)
    }
}

// Interned: the same text is the same address. `Borrow<str>` wants the same answer as `str` gives,
// and it does: two Syms are equal exactly when their texts are.
impl PartialEq for Sym {
    fn eq(&self, other: &Sym) -> bool {
        std::ptr::eq(self.0, other.0)
    }
}
impl Eq for Sym {}
impl std::hash::Hash for Sym {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        (**self.0).hash(state)
    }
}
impl PartialEq<str> for Sym {
    fn eq(&self, other: &str) -> bool {
        **self.0 == *other
    }
}
impl PartialEq<&str> for Sym {
    fn eq(&self, other: &&str) -> bool {
        **self.0 == **other
    }
}
impl PartialEq<String> for Sym {
    fn eq(&self, other: &String) -> bool {
        **self.0 == **other
    }
}
impl PartialEq<Sym> for str {
    fn eq(&self, other: &Sym) -> bool {
        *self == **other.0
    }
}
impl PartialEq<Sym> for &str {
    fn eq(&self, other: &Sym) -> bool {
        **self == **other.0
    }
}
impl PartialEq<Sym> for String {
    fn eq(&self, other: &Sym) -> bool {
        **self == **other.0
    }
}

impl std::fmt::Debug for Sym {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        std::fmt::Debug::fmt(*self.0, f)
    }
}
impl std::fmt::Display for Sym {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl serde::Serialize for Sym {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.0)
    }
}

impl<'de> serde::Deserialize<'de> for Sym {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Sym, D::Error> {
        struct V;
        impl serde::de::Visitor<'_> for V {
            type Value = Sym;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a string")
            }
            fn visit_str<E>(self, v: &str) -> Result<Sym, E> {
                Ok(Sym::new(v))
            }
        }
        d.deserialize_str(V)
    }
}

/// Bytes the string can hold without an allocation.
pub const INLINE: usize = 22;

/// A byte string, kept inside the value up to `INLINE` bytes and in a box beyond: the values of
/// the game's network variables are 1 to 20 bytes (a flag, an int, a vector, a color), and a
/// `Vec<u8>` of them is 24 bytes and a 32-byte allocation each.
#[derive(Clone)]
pub enum Bytes {
    Inline(u8, [u8; INLINE]),
    Heap(Box<[u8]>),
}

const _: () = assert!(std::mem::size_of::<Bytes>() == 24 && std::mem::size_of::<Sym>() == 8);

impl Bytes {
    pub fn new(v: &[u8]) -> Bytes {
        if v.len() <= INLINE {
            let mut buf = [0; INLINE];
            buf[..v.len()].copy_from_slice(v);
            Bytes::Inline(v.len() as u8, buf)
        } else {
            Bytes::Heap(v.into())
        }
    }

    pub fn as_slice(&self) -> &[u8] {
        match self {
            Bytes::Inline(n, buf) => &buf[..*n as usize],
            Bytes::Heap(b) => b,
        }
    }
}

impl Default for Bytes {
    fn default() -> Bytes {
        Bytes::Inline(0, [0; INLINE])
    }
}

impl std::ops::Deref for Bytes {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        self.as_slice()
    }
}

impl From<Vec<u8>> for Bytes {
    fn from(v: Vec<u8>) -> Bytes {
        if v.len() <= INLINE { Bytes::new(&v) } else { Bytes::Heap(v.into_boxed_slice()) }
    }
}

impl From<&[u8]> for Bytes {
    fn from(v: &[u8]) -> Bytes {
        Bytes::new(v)
    }
}

impl PartialEq for Bytes {
    fn eq(&self, other: &Bytes) -> bool {
        self.as_slice() == other.as_slice()
    }
}
impl Eq for Bytes {}
impl PartialEq<Vec<u8>> for Bytes {
    fn eq(&self, other: &Vec<u8>) -> bool {
        self.as_slice() == other.as_slice()
    }
}
impl PartialEq<[u8]> for Bytes {
    fn eq(&self, other: &[u8]) -> bool {
        self.as_slice() == other
    }
}
impl PartialEq<&[u8]> for Bytes {
    fn eq(&self, other: &&[u8]) -> bool {
        self.as_slice() == *other
    }
}
impl<const N: usize> PartialEq<[u8; N]> for Bytes {
    fn eq(&self, other: &[u8; N]) -> bool {
        self.as_slice() == other.as_slice()
    }
}

impl std::fmt::Debug for Bytes {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        std::fmt::Debug::fmt(self.as_slice(), f)
    }
}

/// `#[serde(with = "crate::compact::bytes")]` on a `Bytes`: what `bincodec::bytes` is for a `Vec<u8>`
/// (the same bytes in the binary codec, the array of numbers in JSON).
pub mod bytes {
    use super::Bytes;
    use serde::{Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &Bytes, s: S) -> Result<S::Ok, S::Error> {
        crate::bincodec::bytes::serialize(v, s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Bytes, D::Error> {
        if d.is_human_readable() {
            return <Vec<u8> as serde::Deserialize>::deserialize(d).map(Bytes::from);
        }
        struct V;
        impl<'de> serde::de::Visitor<'de> for V {
            type Value = Bytes;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("bytes")
            }
            fn visit_borrowed_bytes<E>(self, v: &'de [u8]) -> Result<Bytes, E> {
                Ok(Bytes::new(v))
            }
        }
        d.deserialize_bytes(V)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_text_is_the_same_word() {
        let (a, b, c) = (Sym::new("_groupHash"), Sym::new(&String::from("_groupHash")), Sym::new("_active"));
        assert!(a == b && a != c);
        assert!(std::ptr::eq(a.as_str(), b.as_str()));
        assert!(a == "_groupHash" && a == *"_groupHash" && "_active" == c && a.starts_with("_group"));
        assert_eq!(format!("{a} {a:?}"), "_groupHash \"_groupHash\"");
        assert!(Sym::default() == "");
    }

    #[test]
    fn words_cross_the_codecs_as_the_strings_they_are() {
        let sym = Sym::new("NetworkVariable<Int32>");
        let binary = crate::bincodec::to_vec(&sym).unwrap();
        assert_eq!(binary, crate::bincodec::to_vec(&"NetworkVariable<Int32>".to_string()).unwrap());
        let back: Sym = crate::bincodec::from_slice(&binary).unwrap();
        assert!(back == sym && std::ptr::eq(back.as_str(), sym.as_str()));
        assert_eq!(serde_json::to_string(&sym).unwrap(), "\"NetworkVariable<Int32>\"");
        assert!(serde_json::from_str::<Sym>("\"NetworkVariable<Int32>\"").unwrap() == sym);
    }

    /// Short values stay inline, long ones go to a box, and both are the bytes they were given.
    #[test]
    fn bytes_are_the_bytes_they_were_given_at_every_length() {
        for n in 0..=INLINE + 3 {
            let v: Vec<u8> = (0..n as u8).map(|i| i.wrapping_mul(7).wrapping_add(1)).collect();
            let b = Bytes::from(v.clone());
            assert_eq!(b.len(), n);
            assert!(b == v && b == v.as_slice() && b == Bytes::new(&v), "{n}");
            assert_eq!(matches!(b, Bytes::Inline(..)), n <= INLINE);
            let wire = crate::bincodec::to_vec(&v).unwrap();
            #[derive(serde::Serialize, serde::Deserialize)]
            struct Holder(#[serde(with = "crate::compact::bytes")] Bytes);
            #[derive(serde::Serialize)]
            struct Old(#[serde(with = "crate::bincodec::bytes")] Vec<u8>);
            let old = crate::bincodec::to_vec(&Old(v.clone())).unwrap();
            let new = crate::bincodec::to_vec(&Holder(b.clone())).unwrap();
            assert_eq!(old, new, "{n}: the codec writes them as it wrote a Vec<u8>");
            let back: Holder = crate::bincodec::from_slice(&new).unwrap();
            assert!(back.0 == v, "{n}");
            assert_eq!(serde_json::to_string(&Holder(b)).unwrap(), serde_json::to_string(&v).unwrap(), "{n}");
            drop(wire);
        }
    }
}
