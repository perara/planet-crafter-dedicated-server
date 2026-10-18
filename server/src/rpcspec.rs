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

    /// The arguments of an RPC; None for an unknown or undecodable one or a short body.
    pub fn decode(&self, hash: u32, body: &[u8]) -> Option<Vec<Value>> {
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
    fn enums_take_their_underlying_size() {
        let s = Spec::parse(r#"{"1": {"name": "A.BRpc", "args": [{"name": "e", "type": "E", "enc": "enum", "base": "byte", "optional": false}, {"name": "f", "type": "F[]", "enc": "array", "item": "enum", "base": "short", "optional": true}]}}"#).unwrap();
        let (hash, bytes) = s.encode("A.BRpc", &[json!(200), json!([-2, 3])]);
        assert_eq!(bytes, [200, 1, 2, 0, 0, 0, 0xfe, 0xff, 3, 0]);
        assert_eq!(s.decode(hash, &bytes).unwrap(), vec![json!(200), json!([-2, 3])]);
    }

    #[test]
    fn decodes_a_captured_player_ready() {
        // PlayerReadyServerRpc(ulong id, string playerName) from a real client, after the
        // sender/object/behaviour/method header.
        let Some(s) = spec() else { return };
        let args = s.decode(2442368922, &crate::world::hex("11010c0000005000630064007300530065006c0066007400650073007400")).unwrap();
        assert_eq!(args, vec![json!(1), json!("PcdsSelftest")]);
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
