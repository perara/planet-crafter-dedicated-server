//! The game's tables as parsed documents (`Doc`): a JSON value that reads like `serde_json::Value`
//! (`doc["key"]`, `as_str`, `as_f64`, `== "text"`...) in a quarter of the room. A `Value` is a
//! 32-byte node, each object an `IndexMap` with a hash table and a `String` for every key; the
//! tables hold 1,600 objects with 37,000 fields whose keys are a few hundred words, and 15,000
//! strings of 3,000 distinct texts: 9 MiB as `Value`s, 2 as `Doc`s. Keys and strings are interned
//! (`Sym`), objects are their entries in order, and numbers are what serde_json's are (an integer
//! below 2^63 reads as `as_i64`, a float with a point only as `as_f64`), so a table reads the same.
//!
//! For the tables only (their words are kept for good): never for what players send or write.

use serde::de::{MapAccess, SeqAccess, Visitor};
use serde::ser::{SerializeMap, SerializeSeq};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

use crate::compact::Sym;

#[derive(Clone, Debug, PartialEq, Default)]
pub enum Doc {
    #[default]
    Null,
    Bool(bool),
    /// A negative integer.
    Neg(i64),
    /// An integer of 0 or more.
    Pos(u64),
    Float(f64),
    Str(Sym),
    Array(Box<[Doc]>),
    /// The fields in the order the document has them.
    Object(Box<[(Sym, Doc)]>),
}

const _: () = assert!(std::mem::size_of::<Doc>() == 24);

static NULL: Doc = Doc::Null;

impl Doc {
    /// The document a JSON text is.
    pub fn parse(text: &str) -> Result<Doc, serde_json::Error> {
        serde_json::from_str(text)
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Doc::Null)
    }
    pub fn is_string(&self) -> bool {
        matches!(self, Doc::Str(_))
    }
    pub fn is_boolean(&self) -> bool {
        matches!(self, Doc::Bool(_))
    }
    pub fn is_number(&self) -> bool {
        matches!(self, Doc::Neg(_) | Doc::Pos(_) | Doc::Float(_))
    }
    pub fn is_i64(&self) -> bool {
        self.as_i64().is_some()
    }
    pub fn is_array(&self) -> bool {
        matches!(self, Doc::Array(_))
    }
    pub fn is_object(&self) -> bool {
        matches!(self, Doc::Object(_))
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Doc::Str(s) => Some(s.as_str()),
            _ => None,
        }
    }
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Doc::Bool(b) => Some(*b),
            _ => None,
        }
    }
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Doc::Neg(n) => Some(*n),
            Doc::Pos(n) => i64::try_from(*n).ok(),
            _ => None,
        }
    }
    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Doc::Pos(n) => Some(*n),
            _ => None,
        }
    }
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Doc::Neg(n) => Some(*n as f64),
            Doc::Pos(n) => Some(*n as f64),
            Doc::Float(f) => Some(*f),
            _ => None,
        }
    }
    pub fn as_array(&self) -> Option<&[Doc]> {
        match self {
            Doc::Array(a) => Some(a),
            _ => None,
        }
    }
    pub fn as_object(&self) -> Option<Fields<'_>> {
        match self {
            Doc::Object(o) => Some(Fields(o)),
            _ => None,
        }
    }

    /// The field of an object; None for a field it does not have and for another kind of document.
    pub fn get(&self, key: &str) -> Option<&Doc> {
        self.as_object()?.get(key)
    }

    /// The same document as a `Value`, for what is built from it with `json!` or kept in the state.
    pub fn to_value(&self) -> Value {
        match self {
            Doc::Null => Value::Null,
            Doc::Bool(b) => Value::Bool(*b),
            Doc::Neg(n) => Value::from(*n),
            Doc::Pos(n) => Value::from(*n),
            Doc::Float(f) => serde_json::Number::from_f64(*f).map_or(Value::Null, Value::Number),
            Doc::Str(s) => Value::String(s.to_string()),
            Doc::Array(a) => Value::Array(a.iter().map(Doc::to_value).collect()),
            Doc::Object(o) => Value::Object(o.iter().map(|(k, v)| (k.to_string(), v.to_value())).collect()),
        }
    }
}

/// An object's fields, in order.
#[derive(Clone, Copy)]
pub struct Fields<'a>(&'a [(Sym, Doc)]);

impl<'a> Fields<'a> {
    pub fn get(&self, key: &str) -> Option<&'a Doc> {
        self.0.iter().find(|(k, _)| *k == key).map(|(_, v)| v)
    }
    pub fn contains_key(&self, key: &str) -> bool {
        self.get(key).is_some()
    }
    pub fn len(&self) -> usize {
        self.0.len()
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    pub fn keys(&self) -> impl Iterator<Item = &'a str> + use<'a> {
        self.0.iter().map(|(k, _)| k.as_str())
    }
    pub fn values(&self) -> impl Iterator<Item = &'a Doc> + use<'a> {
        self.0.iter().map(|(_, v)| v)
    }
    pub fn iter(&self) -> impl Iterator<Item = (&'a str, &'a Doc)> + use<'a> {
        self.0.iter().map(|(k, v)| (k.as_str(), v))
    }
}

impl<'a> IntoIterator for Fields<'a> {
    type Item = (&'a str, &'a Doc);
    type IntoIter = std::iter::Map<std::slice::Iter<'a, (Sym, Doc)>, fn(&'a (Sym, Doc)) -> (&'a str, &'a Doc)>;
    fn into_iter(self) -> Self::IntoIter {
        fn pair(e: &(Sym, Doc)) -> (&str, &Doc) {
            (e.0.as_str(), &e.1)
        }
        self.0.iter().map(pair as fn(&'a (Sym, Doc)) -> (&'a str, &'a Doc))
    }
}

impl std::ops::Index<&str> for Fields<'_> {
    type Output = Doc;
    fn index(&self, key: &str) -> &Doc {
        self.get(key).unwrap_or(&NULL)
    }
}

/// `doc["key"]`: the field, or null for a field that is not there and for a document that is not an object.
impl std::ops::Index<&str> for Doc {
    type Output = Doc;
    fn index(&self, key: &str) -> &Doc {
        self.get(key).unwrap_or(&NULL)
    }
}

impl std::ops::Index<&String> for Doc {
    type Output = Doc;
    fn index(&self, key: &String) -> &Doc {
        self.get(key).unwrap_or(&NULL)
    }
}

impl std::ops::Index<String> for Doc {
    type Output = Doc;
    fn index(&self, key: String) -> &Doc {
        self.get(&key).unwrap_or(&NULL)
    }
}

impl std::ops::Index<&&str> for Doc {
    type Output = Doc;
    fn index(&self, key: &&str) -> &Doc {
        self.get(key).unwrap_or(&NULL)
    }
}

/// `doc[3]`: the element, or null.
impl std::ops::Index<usize> for Doc {
    type Output = Doc;
    fn index(&self, at: usize) -> &Doc {
        self.as_array().and_then(|a| a.get(at)).unwrap_or(&NULL)
    }
}

impl PartialEq<str> for Doc {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == Some(other)
    }
}
impl PartialEq<&str> for Doc {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == Some(*other)
    }
}
impl PartialEq<String> for Doc {
    fn eq(&self, other: &String) -> bool {
        self.as_str() == Some(other.as_str())
    }
}
impl PartialEq<bool> for Doc {
    fn eq(&self, other: &bool) -> bool {
        self.as_bool() == Some(*other)
    }
}
impl PartialEq<Doc> for &str {
    fn eq(&self, other: &Doc) -> bool {
        other.as_str() == Some(*self)
    }
}

impl From<&Value> for Doc {
    fn from(v: &Value) -> Doc {
        match v {
            Value::Null => Doc::Null,
            Value::Bool(b) => Doc::Bool(*b),
            Value::Number(n) => match (n.as_u64(), n.as_i64()) {
                (Some(u), _) => Doc::Pos(u),
                (None, Some(i)) => Doc::Neg(i),
                _ => Doc::Float(n.as_f64().unwrap_or(f64::NAN)),
            },
            Value::String(s) => Doc::Str(Sym::new(s)),
            Value::Array(a) => Doc::Array(a.iter().map(Doc::from).collect()),
            Value::Object(o) => Doc::Object(o.iter().map(|(k, v)| (Sym::new(k), Doc::from(v))).collect()),
        }
    }
}

impl From<Value> for Doc {
    fn from(v: Value) -> Doc {
        Doc::from(&v)
    }
}

impl std::fmt::Display for Doc {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match serde_json::to_string(self) {
            Ok(text) => f.write_str(&text),
            Err(_) => Err(std::fmt::Error),
        }
    }
}

impl Serialize for Doc {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Doc::Null => s.serialize_unit(),
            Doc::Bool(b) => s.serialize_bool(*b),
            Doc::Neg(n) => s.serialize_i64(*n),
            Doc::Pos(n) => s.serialize_u64(*n),
            Doc::Float(f) => s.serialize_f64(*f),
            Doc::Str(t) => s.serialize_str(t),
            Doc::Array(a) => {
                let mut seq = s.serialize_seq(Some(a.len()))?;
                a.iter().try_for_each(|e| seq.serialize_element(e))?;
                seq.end()
            }
            Doc::Object(o) => {
                let mut map = s.serialize_map(Some(o.len()))?;
                o.iter().try_for_each(|(k, v)| map.serialize_entry(k, v))?;
                map.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for Doc {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Doc, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Doc;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("any JSON value")
            }
            fn visit_bool<E>(self, v: bool) -> Result<Doc, E> {
                Ok(Doc::Bool(v))
            }
            fn visit_i64<E>(self, v: i64) -> Result<Doc, E> {
                Ok(if v < 0 { Doc::Neg(v) } else { Doc::Pos(v as u64) })
            }
            fn visit_u64<E>(self, v: u64) -> Result<Doc, E> {
                Ok(Doc::Pos(v))
            }
            fn visit_f64<E>(self, v: f64) -> Result<Doc, E> {
                Ok(Doc::Float(v))
            }
            fn visit_str<E>(self, v: &str) -> Result<Doc, E> {
                Ok(Doc::Str(Sym::new(v)))
            }
            fn visit_unit<E>(self) -> Result<Doc, E> {
                Ok(Doc::Null)
            }
            fn visit_none<E>(self) -> Result<Doc, E> {
                Ok(Doc::Null)
            }
            fn visit_some<D2: Deserializer<'de>>(self, d: D2) -> Result<Doc, D2::Error> {
                Doc::deserialize(d)
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Doc, A::Error> {
                let mut items = Vec::with_capacity(seq.size_hint().unwrap_or(0).min(4096));
                while let Some(item) = seq.next_element()? {
                    items.push(item);
                }
                Ok(Doc::Array(items.into_boxed_slice()))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Doc, A::Error> {
                let mut fields: Vec<(Sym, Doc)> = Vec::with_capacity(map.size_hint().unwrap_or(0).min(4096));
                while let Some((key, value)) = map.next_entry::<Sym, Doc>()? {
                    // A key twice keeps its first place and the last value, as serde_json's map does.
                    match fields.iter_mut().find(|(k, _)| *k == key) {
                        Some(slot) => slot.1 = value,
                        None => fields.push((key, value)),
                    }
                }
                Ok(Doc::Object(fields.into_boxed_slice()))
            }
        }
        d.deserialize_any(V)
    }
}

/// `#[serde(with = "crate::doc::json")]` on a `Doc` in the state: the JSON text in the binary codec
/// (what `bincodec::json_value` writes for a `Value`: the same bytes), the document itself in JSON.
pub mod json {
    use super::Doc;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(v: &Doc, s: S) -> Result<S::Ok, S::Error> {
        if s.is_human_readable() {
            return v.serialize(s);
        }
        s.serialize_bytes(&serde_json::to_vec(v).map_err(serde::ser::Error::custom)?)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Doc, D::Error> {
        if d.is_human_readable() {
            return Doc::deserialize(d);
        }
        let text: &'de [u8] = Deserialize::deserialize(d)?;
        serde_json::from_slice(text).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// What a table reads as, through every accessor the server uses, is what the `Value` reads as.
    fn same(v: &Value, d: &Doc) {
        assert_eq!((v.is_null(), v.is_string(), v.is_boolean(), v.is_number(), v.is_i64(), v.is_array(), v.is_object()), (d.is_null(), d.is_string(), d.is_boolean(), d.is_number(), d.is_i64(), d.is_array(), d.is_object()), "{v}");
        assert_eq!((v.as_str(), v.as_bool(), v.as_i64(), v.as_u64()), (d.as_str(), d.as_bool(), d.as_i64(), d.as_u64()), "{v}");
        assert!(v.as_f64() == d.as_f64() || (v.as_f64().is_some_and(f64::is_nan) && d.as_f64().is_some_and(f64::is_nan)), "{v}");
        match v {
            Value::Array(a) => {
                assert_eq!(d.as_array().map(<[Doc]>::len), Some(a.len()));
                a.iter().enumerate().for_each(|(i, e)| same(e, &d[i]));
                assert!(d[a.len()].is_null());
            }
            Value::Object(o) => {
                let fields = d.as_object().expect("an object");
                assert_eq!(fields.keys().collect::<Vec<_>>(), o.keys().map(String::as_str).collect::<Vec<_>>(), "the order of the fields");
                o.iter().for_each(|(k, e)| same(e, &d[k.as_str()]));
                assert!(d["no such field"].is_null() && fields.get("no such field").is_none());
            }
            _ => assert!(d["a"].is_null() && d[0].is_null()),
        }
        assert_eq!(&d.to_value(), v);
        assert_eq!(serde_json::to_string(d).unwrap(), serde_json::to_string(v).unwrap());
        assert_eq!(d.to_string(), v.to_string());
    }

    #[test]
    fn a_doc_reads_as_the_value_it_was_made_from() {
        let text = r#"{"a": 1, "b": -2, "c": 3.0, "d": 1e300, "e": 18446744073709551615, "f": -9223372036854775808, "g": 9223372036854775808, "h": 0.1, "i": 0, "j": -0.0, "k": "text \"q\" å \u0000", "l": null, "m": true, "n": [1, [2, {"x": []}], {}], "o": {"z": 1, "a": {"y": null}}, "z": "", "dup": 1, "dup": 2}"#;
        let (value, doc): (Value, Doc) = (serde_json::from_str(text).unwrap(), serde_json::from_str(text).unwrap());
        same(&value, &doc);
        assert_eq!(doc, Doc::from(&value));
        assert_eq!(doc["dup"].as_i64(), Some(2));
        assert_eq!(doc.as_object().unwrap().keys().filter(|k| *k == "dup").count(), 1);
        // The numbers are serde_json's: an integer reads as one, a float with a point only as a float.
        assert_eq!((doc["a"].as_i64(), doc["c"].as_i64(), doc["c"].as_f64(), doc["e"].as_i64(), doc["e"].as_u64()), (Some(1), None, Some(3.0), None, Some(u64::MAX)));
        assert!(doc["k"] == "text \"q\" \u{e5} \0" && doc["m"] == true && "text \"q\" \u{e5} \0" == doc["k"] && doc["k"] != "other");
        assert_eq!(Doc::from(json!({"a": [1, 2]})), Doc::parse(r#"{"a": [1, 2]}"#).unwrap());
    }

    /// Every document of the game's tables reads as it does as a `Value`, and is written as the same text.
    #[test]
    fn the_games_tables_read_the_same_as_docs() {
        let Some(dir) = crate::test_data() else { return };
        let text = std::fs::read_to_string(format!("{dir}/gamedata.json")).unwrap();
        let (value, doc): (Value, Doc) = (serde_json::from_str(&text).unwrap(), serde_json::from_str(&text).unwrap());
        same(&value, &doc);
        for file in std::fs::read_dir(format!("{dir}/scenes")).unwrap().flatten().take(2) {
            let text = std::fs::read_to_string(file.path()).unwrap();
            same(&serde_json::from_str(&text).unwrap(), &serde_json::from_str(&text).unwrap());
        }
    }
}
