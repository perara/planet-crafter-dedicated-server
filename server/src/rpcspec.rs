//! Every game RPC with its argument encoding (data/<version>/rpcs.json, which `pcds read-game`
//! reads from the game's code), so RPCs are read and written by name. Arguments are JSON values
//! in signature order: numbers, bools, strings, arrays for vectors ([x, y, z]), colors and
//! structs (fields in order), arrays of those, and null for an absent optional argument.

use std::collections::HashMap;

use serde::Deserialize;
use serde_json::{json, Value};

use crate::wire::{Reader, Writer};

#[derive(Deserialize, Clone)]
pub struct Rpc {
    pub name: String,
    pub args: Vec<Arg>,
    /// No INetworkSerializable argument (its NetworkSerialize is not ported): decode gives None.
    #[serde(skip)]
    decodable: bool,
}

#[derive(Deserialize, Clone)]
pub struct Arg {
    pub name: String,
    #[serde(rename = "type")]
    pub ty: String,
    pub enc: String,
    #[serde(default)]
    pub item: Option<String>,
    #[serde(default)]
    pub optional: bool,
    /// Field types of a memcpy struct (or of an array's struct items), in layout order.
    #[serde(default)]
    pub fields: Option<Vec<String>>,
    /// An enum's underlying type (its size on the wire).
    #[serde(default)]
    pub base: Option<String>,
}

#[derive(Default, Clone)]
pub struct Spec {
    by_hash: HashMap<u32, Rpc>,
    by_name: HashMap<String, u32>,
}

/// Types write_raw and read_raw know ("pad1": a byte of struct padding).
const RAW: &[&str] = &["bool", "float", "int", "uint", "short", "ushort", "byte", "long", "double", "ulong", "pad1", "Vector2Int", "Vector2", "Vector3", "Quaternion", "Color"];
const PACKED: &[&str] = &["int", "short", "uint", "ushort", "ulong", "long"];

/// Every encoding of `a` is one this module reads and writes (Ok(false): an INetworkSerializable).
fn check(a: &Arg) -> Result<bool, String> {
    let element = if a.enc == "array" { a.item.as_deref().ok_or("an array without its item encoding")? } else { a.enc.as_str() };
    let ty = if a.enc == "array" { a.ty.strip_suffix("[]").ok_or("an array type without []")? } else { a.ty.as_str() };
    let ok = match (a.enc.as_str(), element) {
        (_, "raw") => RAW.contains(&ty) && ty != "pad1",
        ("packed", _) => PACKED.contains(&ty),
        (_, "enum") => a.base.as_deref().is_some_and(|b| PACKED.contains(&b) || b == "byte"),
        (_, "struct") => a.fields.as_ref().is_some_and(|f| !f.is_empty() && f.iter().all(|f| RAW.contains(&f.as_str()))),
        ("string", _) => true,
        ("extension", _) => ty == "WorldInstanceData",
        (_, "serializable") => return Ok(false),
        _ => false,
    };
    if ok { Ok(true) } else { Err(format!("encoding {} (item {:?}, base {:?}, fields {:?}) of {} is not supported", a.enc, a.item, a.base, a.fields, a.ty)) }
}

/// The largest float a client may send in an RPC or a transform. Every position, rotation, time
/// and amount of the game is far below it (positions are within tens of thousands, a month is
/// 2.6e6 seconds), and above it the server's own arithmetic overflows: a quaternion or position of
/// 1e30 in LoadInstanceFromScene made NaN positions in the objects the server then created,
/// which no snapshot can hold (hot updates refused) and the save writes as "NaN".
pub const SANE_FLOAT: f64 = 1.0e9;

/// The most items an RPC's array may hold; a longer one is no RPC of the game's and is not decoded.
/// The game's lists are an inventory's items (at most 1000), groups (about 730) and a handful of
/// traits; a megabyte of them was tens of milliseconds of a handler's time, at the cost of one RPC.
pub const MAX_ARRAY_ITEMS: i32 = 4096;

/// Whether an argument holds a float (not an integer) past SANE_FLOAT.
fn huge(v: &Value) -> bool {
    match v {
        Value::Number(n) => n.is_f64() && n.as_f64().is_some_and(|f| f.abs() > SANE_FLOAT),
        Value::Array(items) => items.iter().any(huge),
        _ => false,
    }
}

fn floats(v: &Value, n: usize) -> Vec<f32> {
    (0..n).map(|i| v.get(i).and_then(Value::as_f64).unwrap_or(0.0) as f32).collect()
}

fn write_raw(w: &mut Writer, ty: &str, v: &Value) {
    match ty {
        "bool" => {
            w.bool(v.as_bool().unwrap_or(false));
        }
        "float" => {
            w.f32(v.as_f64().unwrap_or(0.0) as f32);
        }
        "int" => {
            w.i32(v.as_i64().unwrap_or(0) as i32);
        }
        "uint" => {
            w.u32(v.as_u64().unwrap_or(0) as u32);
        }
        "short" => {
            w.bytes(&(v.as_i64().unwrap_or(0) as i16).to_le_bytes());
        }
        "ushort" => {
            w.u16(v.as_u64().unwrap_or(0) as u16);
        }
        "byte" => {
            w.u8(v.as_u64().unwrap_or(0) as u8);
        }
        "long" => {
            w.bytes(&v.as_i64().unwrap_or(0).to_le_bytes());
        }
        "double" => {
            w.bytes(&v.as_f64().unwrap_or(0.0).to_le_bytes());
        }
        "pad1" => {
            w.u8(0);
        }
        "ulong" => {
            w.u64(v.as_u64().unwrap_or(0));
        }
        "Vector2Int" => {
            w.i32(v[0].as_i64().unwrap_or(0) as i32).i32(v[1].as_i64().unwrap_or(0) as i32);
        }
        "Vector2" | "Vector3" | "Quaternion" | "Color" => {
            let n = match ty {
                "Vector2" => 2,
                "Vector3" => 3,
                _ => 4,
            };
            floats(v, n).iter().for_each(|f| {
                w.f32(*f);
            });
        }
        _ => unreachable!("raw RPC type {ty} passed Spec::load"),
    }
}

fn read_raw(r: &mut Reader, ty: &str) -> Option<Value> {
    Some(match ty {
        "bool" => json!(r.bool()?),
        "float" => json!(r.f32()?),
        "int" => json!(r.i32()?),
        "uint" => json!(r.u32()?),
        "short" => json!(r.i16()?),
        "ushort" => json!(r.u16_raw()?),
        "byte" => json!(r.u8()?),
        "long" => json!(r.u64()? as i64),
        "double" => json!(f64::from_bits(r.u64()?)),
        "pad1" => {
            r.skip(1)?;
            Value::Null
        }
        "ulong" => json!(r.u64()?),
        "Vector2Int" => json!([r.i32()?, r.i32()?]),
        "Vector2" => json!([r.f32()?, r.f32()?]),
        "Vector3" => json!([r.f32()?, r.f32()?, r.f32()?]),
        "Quaternion" | "Color" => json!([r.f32()?, r.f32()?, r.f32()?, r.f32()?]),
        _ => unreachable!("raw RPC type {ty} passed Spec::load"),
    })
}

fn write_string(w: &mut Writer, s: &str) {
    let units: Vec<u16> = s.encode_utf16().collect();
    w.u32(units.len() as u32);
    units.iter().for_each(|u| {
        w.u16(*u);
    });
}

/// One value of `a` with encoding `enc` and type `ty` (an array's: its item's).
fn write_value(w: &mut Writer, a: &Arg, enc: &str, ty: &str, v: &Value) {
    match enc {
        "packed" => {
            match ty {
                "int" => w.packed_i32(v.as_i64().unwrap_or(0) as i32),
                "short" => w.packed_i16(v.as_i64().unwrap_or(0) as i16),
                "uint" => w.packed_u32(v.as_u64().unwrap_or(0) as u32),
                "ushort" => w.packed_u16(v.as_u64().unwrap_or(0) as u16),
                "ulong" => w.packed_u64(v.as_u64().unwrap_or(0)),
                "long" => {
                    let i = v.as_i64().unwrap_or(0);
                    w.packed_u64(((i << 1) ^ (i >> 63)) as u64)
                }
                _ => unreachable!("packed RPC type {ty} passed Spec::load"),
            };
        }
        "raw" => write_raw(w, ty, v),
        "enum" => write_raw(w, a.base.as_deref().unwrap_or_default(), v),
        "struct" => {
            let mut values = v.as_array().into_iter().flatten();
            for field in a.fields.iter().flatten() {
                let value = if field.starts_with("pad") { &Value::Null } else { values.next().unwrap_or(&Value::Null) };
                write_raw(w, field, value);
            }
        }
        "string" => write_string(w, v.as_str().unwrap_or("")),
        "array" => {
            let items = v.as_array().cloned().unwrap_or_default();
            let item_ty = ty.trim_end_matches("[]");
            w.i32(items.len() as i32);
            for i in &items {
                write_value(w, a, a.item.as_deref().unwrap_or_default(), item_ty, i);
            }
        }
        // WorldInstanceDataSerializationExtensions.WriteValueSafe: [seed, time left, scene, loot
        // rarity, difficulty, recipe group hashes]; a seed of 0 is no instance.
        "extension" => {
            let a = v.as_array().cloned().unwrap_or_default();
            let int = |i: usize| a.get(i).and_then(|x| x.as_i64()).unwrap_or(0) as i32;
            w.i32(int(0));
            if int(0) != 0 {
                w.i32(int(1));
                write_string(w, a.get(2).and_then(Value::as_str).unwrap_or(""));
                w.i32(int(3)).i32(int(4));
                let recipe = a.get(5).and_then(Value::as_array).cloned().unwrap_or_default();
                w.i32(recipe.len() as i32);
                recipe.iter().for_each(|h| {
                    w.i32(h.as_i64().unwrap_or(0) as i32);
                });
            }
        }
        _ => unreachable!("RPC encoding {enc} {ty} passed Spec::load"),
    }
}

fn read_value(r: &mut Reader, a: &Arg, enc: &str, ty: &str) -> Option<Value> {
    Some(match enc {
        "packed" => match ty {
            "int" => json!(r.packed_i32()?),
            "short" => json!(r.packed_i16()?),
            "uint" => json!(r.packed_u32()?),
            "ushort" => json!(r.packed_u16()?),
            "ulong" => json!(r.packed_u64()?),
            "long" => {
                let u = r.packed_u64()?;
                json!(((u >> 1) as i64) ^ -((u & 1) as i64))
            }
            _ => unreachable!("packed RPC type {ty} passed Spec::load"),
        },
        "raw" => read_raw(r, ty)?,
        "enum" => read_raw(r, a.base.as_deref().unwrap_or_default())?,
        "struct" => {
            let mut out = Vec::new();
            for field in a.fields.iter().flatten() {
                let v = read_raw(r, field)?;
                if !field.starts_with("pad") {
                    out.push(v);
                }
            }
            Value::Array(out)
        }
        "string" => json!(r.string()?),
        "array" => {
            let n = r.i32()?;
            if n > MAX_ARRAY_ITEMS {
                return None;
            }
            let item_ty = ty.trim_end_matches("[]");
            let mut items = Vec::new();
            for _ in 0..n.max(0) {
                items.push(read_value(r, a, a.item.as_deref().unwrap_or_default(), item_ty)?);
            }
            Value::Array(items)
        }
        "extension" => {
            let seed = r.i32()?;
            if seed == 0 {
                return Some(Value::Null);
            }
            let time_left = r.i32()?;
            let scene = r.string()?;
            let (loot, difficulty) = (r.i32()?, r.i32()?);
            let n = r.i32()?;
            if n > MAX_ARRAY_ITEMS {
                return None;
            }
            let recipe = (0..n.max(0)).map(|_| r.i32()).collect::<Option<Vec<i32>>>()?;
            json!([seed, time_left, scene, loot, difficulty, recipe])
        }
        _ => unreachable!("RPC encoding {enc} {ty} passed Spec::load"),
    })
}

impl Spec {
    pub fn load(path: &str) -> Result<Spec, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
        Spec::parse(&text).map_err(|e| format!("{path}: {e}"))
    }

    /// rpcs.json's text; an encoding this module does not read and write is an error.
    fn parse(text: &str) -> Result<Spec, String> {
        let raw: HashMap<String, Rpc> = serde_json::from_str(text).map_err(|e| e.to_string())?;
        let mut spec = Spec::default();
        for (h, mut rpc) in raw {
            let hash: u32 = h.parse().map_err(|_| format!("RPC hash {h:?}"))?;
            rpc.decodable = true;
            for a in &rpc.args {
                rpc.decodable &= check(a).map_err(|e| format!("{}({}): {e}", rpc.name, a.name))?;
            }
            if spec.by_name.insert(rpc.name.clone(), hash).is_some() {
                return Err(format!("two RPCs named {}", rpc.name));
            }
            spec.by_hash.insert(hash, rpc);
        }
        if spec.by_hash.is_empty() {
            return Err("no RPCs".into());
        }
        Ok(spec)
    }

    pub fn get(&self, hash: u32) -> Option<&Rpc> {
        self.by_hash.get(&hash)
    }

    pub fn hash(&self, name: &str) -> Option<u32> {
        self.by_name.get(name).copied()
    }

    /// Method hash and arguments of "Class.Method"; an unknown name is logged and sent as 0.
    pub fn encode(&self, name: &str, args: &[Value]) -> (u32, Vec<u8>) {
        let Some(hash) = self.hash(name) else {
            eprintln!("unknown RPC {name}");
            return (0, Vec::new());
        };
        let rpc = &self.by_hash[&hash];
        if !rpc.decodable {
            eprintln!("RPC {name} has an INetworkSerializable argument, which is not supported");
            return (0, Vec::new());
        }
        let mut w = Writer::new();
        for (i, arg) in rpc.args.iter().enumerate() {
            let v = args.get(i).unwrap_or(&Value::Null);
            if arg.optional {
                w.bool(!v.is_null());
                if v.is_null() {
                    continue;
                }
            }
            write_value(&mut w, arg, &arg.enc, &arg.ty, v);
        }
        (hash, w.0)
    }

    /// The arguments of an RPC; None for an unknown or undecodable one, a short body, or a float
    /// past SANE_FLOAT (see there).
    pub fn decode(&self, hash: u32, body: &[u8]) -> Option<Vec<Value>> {
        let args = self.decode_all(hash, body)?;
        (!args.iter().any(huge)).then_some(args)
    }

    fn decode_all(&self, hash: u32, body: &[u8]) -> Option<Vec<Value>> {
        let rpc = self.by_hash.get(&hash).filter(|r| r.decodable)?;
        let mut r = Reader::new(body);
        let mut out = Vec::new();
        for arg in &rpc.args {
            if arg.optional && !r.bool()? {
                out.push(Value::Null);
                continue;
            }
            out.push(read_value(&mut r, arg, &arg.enc, &arg.ty)?);
        }
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> Option<Spec> {
        Some(Spec::load(&format!("{}/rpcs.json", crate::test_data()?)).unwrap())
    }

    #[test]
    fn refuses_what_it_cannot_read() {
        let one = |arg: &str| Spec::parse(&format!(r#"{{"1": {{"name": "A.BRpc", "to": "server", "args": [{arg}]}}}}"#));
        let arg = |rest: &str| format!(r#"{{"name": "x", "optional": false, {rest}}}"#);
        assert!(one(&arg(r#""type": "Message", "enc": "struct", "fields": ["bool", "pad1", "pad1", "pad1", "int"]"#)).is_ok());
        // No fallback layouts: a struct without its fields, an enum without its size.
        assert!(one(&arg(r#""type": "Message", "enc": "struct""#)).err().unwrap().contains("Message"));
        assert!(one(&arg(r#""type": "E", "enc": "enum""#)).is_err());
        assert!(one(&arg(r#""type": "E", "enc": "enum", "base": "byte""#)).is_ok());
        assert!(one(&arg(r#""type": "Char", "enc": "raw""#)).is_err());
        assert!(one(&arg(r#""type": "Other", "enc": "extension""#)).is_err());
        assert!(Spec::parse("{}").is_err());
        assert!(Spec::parse(r#"{"x": {"name": "A.B", "args": []}}"#).is_err());
        // An INetworkSerializable loads but does not decode.
        let s = one(&arg(r#""type": "M", "enc": "serializable""#)).unwrap();
        assert!(s.decode(1, &[1, 2, 3]).is_none());
        assert_eq!(s.encode("A.BRpc", &[json!(1)]), (0, Vec::new()));
    }

    #[test]
    fn floats_past_the_sane_range_are_not_decoded() {
        let s = Spec::parse(r#"{"1": {"name": "A.BRpc", "args": [{"name": "p", "type": "Vector3", "enc": "raw", "optional": false}, {"name": "n", "type": "int", "enc": "raw", "optional": false}]}}"#).unwrap();
        let (hash, ok) = s.encode("A.BRpc", &[json!([100000.0, -5.0, 3.0]), json!(i32::MAX)]);
        assert_eq!(s.decode(hash, &ok).unwrap()[1], json!(i32::MAX), "big integers are not floats");
        for bad in [1.0e30, -3.0e38, 2.0e9] {
            let (hash, body) = s.encode("A.BRpc", &[json!([0.0, bad, 0.5]), json!(1)]);
            assert!(s.decode(hash, &body).is_none(), "{bad}");
        }
        let (hash, edge) = s.encode("A.BRpc", &[json!([9.0e8, 0.0, 0.0]), json!(1)]);
        assert!(s.decode(hash, &edge).is_some());
    }

    #[test]
    fn enums_take_their_underlying_size() {
        let s = Spec::parse(r#"{"1": {"name": "A.BRpc", "args": [{"name": "e", "type": "E", "enc": "enum", "base": "byte", "optional": false}, {"name": "f", "type": "F[]", "enc": "array", "item": "enum", "base": "short", "optional": true}]}}"#).unwrap();
        let (hash, bytes) = s.encode("A.BRpc", &[json!(200), json!([-2, 3])]);
        assert_eq!(bytes, [200, 1, 2, 0, 0, 0, 0xfe, 0xff, 3, 0]);
        assert_eq!(s.decode(hash, &bytes).unwrap(), vec![json!(200), json!([-2, 3])]);
    }

    /// A list longer than the game's lists is no RPC of the game's: it is not decoded (the handlers
    /// would each spend their time on a megabyte of it, at the budget cost of one RPC).
    #[test]
    fn an_array_longer_than_the_games_lists_is_not_decoded() {
        let s = Spec::parse(r#"{"1": {"name": "A.BRpc", "args": [{"name": "f", "type": "F[]", "enc": "array", "item": "enum", "base": "short", "optional": false}]}}"#).unwrap();
        let list = |n: i32| (0..n).collect::<Vec<i32>>();
        let (hash, ok) = s.encode("A.BRpc", &[json!(list(MAX_ARRAY_ITEMS))]);
        assert_eq!(s.decode(hash, &ok).unwrap()[0].as_array().map(Vec::len), Some(MAX_ARRAY_ITEMS as usize));
        let (hash, too_many) = s.encode("A.BRpc", &[json!(list(MAX_ARRAY_ITEMS + 1))]);
        assert!(s.decode(hash, &too_many).is_none());
        let (hash, huge) = s.encode("A.BRpc", &[json!(list(60_000))]);
        assert!(s.decode(hash, &huge).is_none());
        // The world instance's recipe is a list too.
        let Some(s) = spec() else { return };
        let name = "WorldInstanceHandler.SetAndOpenWorldInstanceSceneServerRpc";
        let recipe = |n: i32| json!([[77, 600, "Instance-1", 2, 1, list(n)]]);
        for (n, decodes) in [(MAX_ARRAY_ITEMS, true), (MAX_ARRAY_ITEMS + 1, false), (100_000, false)] {
            let (hash, bytes) = s.encode(name, recipe(n).as_array().unwrap());
            assert_eq!(s.decode(hash, &bytes).is_some(), decodes, "{n}");
        }
    }

    #[test]
    fn decodes_a_captured_player_ready() {
        // PlayerReadyServerRpc(ulong id, string playerName) from a real client, after the
        // sender/object/behaviour/method header.
        let Some(s) = spec() else { return };
        let args = s.decode(2442368922, &crate::world::hex("11010c0000005000630064007300530065006c0066007400650073007400")).unwrap();
        assert_eq!(args, vec![json!(1), json!("PcdsSelftest")]);
    }

    /// Every RPC of the game's table, with arguments made up for each of its argument kinds (the edges of
    /// every integer, floats, strings with characters past the first plane, lists, structs, an
    /// absent optional): what is written reads back as it was, and a body cut anywhere reads as nothing.
    #[test]
    fn every_rpc_of_the_table_round_trips_random_arguments() {
        let Some(s) = spec() else { return };
        let mut x = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = move |n: u64| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x % n.max(1)
        };
        fn float(next: &mut dyn FnMut(u64) -> u64) -> Value {
            let f = match next(6) {
                0 => 0.0f32,
                1 => -0.0,
                2 => (next(2_000_000) as f32 - 1_000_000.0) / 8.0,
                3 => 1.0e-7 * next(1000) as f32,
                4 => f32::MIN_POSITIVE,
                _ => (next(200_000_000) as f32 - 100_000_000.0) / 3.0,
            };
            json!(f)
        }
        fn raw(ty: &str, next: &mut dyn FnMut(u64) -> u64) -> Value {
            let edge = |next: &mut dyn FnMut(u64) -> u64, a: i64, b: i64| -> i64 { [a, b, 0, 1, -1, next(1000) as i64 - 500][next(6) as usize] };
            match ty {
                "bool" => json!(next(2) == 1),
                "float" => float(next),
                "int" => json!(edge(next, i32::MIN as i64, i32::MAX as i64).clamp(i32::MIN as i64, i32::MAX as i64)),
                "uint" => json!([0u64, u32::MAX as u64, 1, next(100_000)][next(4) as usize]),
                "short" => json!(edge(next, i16::MIN as i64, i16::MAX as i64).clamp(i16::MIN as i64, i16::MAX as i64)),
                "ushort" => json!([0u64, u16::MAX as u64, 1, next(60_000)][next(4) as usize]),
                "byte" => json!(next(256)),
                "long" => json!(edge(next, i64::MIN, i64::MAX)),
                "ulong" => json!([0u64, u64::MAX, 1, next(1 << 40)][next(4) as usize]),
                "double" => json!(next(2_000_000) as f64 / 3.0 - 300_000.0),
                "Vector2Int" => json!([edge(next, i32::MIN as i64, i32::MAX as i64).clamp(i32::MIN as i64, i32::MAX as i64), next(1000) as i64 - 500]),
                "Vector2" => json!([float(next), float(next)]),
                "Vector3" => json!([float(next), float(next), float(next)]),
                "Quaternion" | "Color" => json!([float(next), float(next), float(next), float(next)]),
                other => panic!("an unknown raw type {other}"),
            }
        }
        fn one(a: &Arg, enc: &str, ty: &str, next: &mut dyn FnMut(u64) -> u64) -> Value {
            match enc {
                "raw" => raw(ty, next),
                "packed" => raw(ty, next),
                "enum" => raw(a.base.as_deref().unwrap(), next),
                "struct" => Value::Array(a.fields.iter().flatten().filter(|f| !f.starts_with("pad")).map(|f| raw(f, next)).collect()),
                "string" => json!(["", "x", "h\u{e9}llo", "\u{4e16}\u{754c}", "a \u{1f30d} b", "line\nbreak"][next(6) as usize]),
                "array" => Value::Array((0..next(5)).map(|_| one(a, a.item.as_deref().unwrap(), ty.trim_end_matches("[]"), next)).collect()),
                "extension" => {
                    if next(3) == 0 {
                        Value::Null
                    } else {
                        json!([1 + next(1 << 20) as i64, next(1000) as i64, "Instance-1", next(5) as i64, next(5) as i64, (0..next(4)).map(|_| next(1 << 20) as i64 - 500_000).collect::<Vec<i64>>()])
                    }
                }
                other => panic!("an unknown encoding {other}"),
            }
        }
        let mut hashes: Vec<u32> = s.by_hash.keys().copied().collect();
        hashes.sort();
        let (mut tried, mut skipped) = (0, 0);
        for hash in hashes {
            let rpc = &s.by_hash[&hash];
            if !rpc.decodable {
                skipped += 1;
                continue;
            }
            for round in 0..40 {
                let args: Vec<Value> = rpc.args.iter().map(|a| if a.optional && next(3) == 0 { Value::Null } else { one(a, &a.enc, &a.ty, &mut next) }).collect();
                let (h, body) = s.encode(&rpc.name, &args);
                assert_eq!(h, hash);
                assert_eq!(s.decode(hash, &body).as_ref(), Some(&args), "{} round {round}", rpc.name);
                // A body cut short anywhere never reads as the whole (and never panics).
                for cut in 0..body.len() {
                    if let Some(got) = s.decode(hash, &body[..cut]) {
                        // Only an RPC whose last arguments are optional or empty may read from less.
                        assert_eq!(got.len(), args.len(), "{}", rpc.name);
                    }
                }
                tried += 1;
            }
        }
        assert!(tried > 1000 && skipped < 100, "{tried} bodies, {skipped} RPCs skipped");
    }

    #[test]
    fn round_trips_every_argument_kind() {
        let Some(s) = spec() else { return };
        let cases = [
            ("WorldObjectsHandler.CreateAndInstantiateWorldObjectServerRpc", json!([-12345, [1.5, 2.0, -3.25], [0.0, 0.5, 0.0, 1.0], true, false, true, false])),
            ("InventoriesHandler.RetrieveInventoryClientRpc", json!([12, 7, [1, -2, 3], null, false])),
            ("MessagesHandler.MarkAsReadServerRpc", json!([[true, 99]])),
            ("ChatHandler.SendChatMessageServerRpc", json!(["héllo"])),
            ("WorldObjectsHandler.GetEnergyGroupDataClientRpc", json!([[[5, 1.5, 2]]])),
            ("WorldInstanceHandler.SetAndOpenWorldInstanceSceneServerRpc", json!([[77, 600, "Instance-1", 2, 1, [-5, 9]]])),
            ("WorldInstanceHandler.SetAndOpenWorldInstanceSceneServerRpc", json!([null])),
        ];
        for (name, args) in cases {
            let args = args.as_array().unwrap().clone();
            let (hash, bytes) = s.encode(name, &args);
            assert_eq!(s.decode(hash, &bytes).unwrap(), args, "{name}");
        }
    }
}

#[cfg(any(fuzzing, pcds_corpus))]
#[path = "../fuzz/targets/rpcspec.rs"]
pub mod fuzz;
