//! Every game RPC with its argument encoding (data/<version>/rpcs.json, generated from the game by
//! tools/rpc_signatures.py), so RPCs are read and written by name. Arguments are JSON values
//! in signature order: numbers, bools, strings, arrays for vectors ([x, y, z]), colors and
//! structs (fields in order), arrays of those, and null for an absent optional argument.

use std::collections::HashMap;

use serde::Deserialize;
use serde_json::{json, Value};

use crate::wire::{Reader, Writer};

#[derive(Deserialize)]
pub struct Rpc {
    pub name: String,
    pub to: String,
    pub args: Vec<Arg>,
}

#[derive(Deserialize)]
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
}

pub struct Spec {
    by_hash: HashMap<u32, Rpc>,
    by_name: HashMap<String, u32>,
}

/// Fields of a memcpy struct an RPC carries: from rpcs.json when the game exported them, else
/// the layouts known for 2.103; names starting with "pad" are alignment padding.
fn struct_fields(ty: &str, given: Option<&[String]>) -> Vec<String> {
    if let Some(f) = given {
        return f.to_vec();
    }
    let known: &[&str] = match ty {
        "Message" => &["bool", "pad3", "int"],
        "GeneticTraitData.SerializedData" => &["int", "Color", "int"],
        "Group.GroupCountData" => &["int", "int"],
        "Group.EnergyGroupData" => &["int", "float", "int"],
        _ => {
            eprintln!("unknown RPC struct {ty}");
            &[]
        }
    };
    known.iter().map(|s| s.to_string()).collect()
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
        "int" | "enum" => {
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
        "pad3" => {
            w.bytes(&[0, 0, 0]);
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
        _ => eprintln!("unknown raw RPC type {ty}"),
    }
}

fn read_raw(r: &mut Reader, ty: &str) -> Option<Value> {
    Some(match ty {
        "bool" => json!(r.bool()?),
        "float" => json!(r.f32()?),
        "int" | "enum" => json!(r.i32()?),
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
        "pad3" => {
            r.skip(3)?;
            Value::Null
        }
        "Vector2Int" => json!([r.i32()?, r.i32()?]),
        "Vector2" => json!([r.f32()?, r.f32()?]),
        "Vector3" => json!([r.f32()?, r.f32()?, r.f32()?]),
        "Quaternion" | "Color" => json!([r.f32()?, r.f32()?, r.f32()?, r.f32()?]),
        _ => return None,
    })
}

fn write_value(w: &mut Writer, enc: &str, ty: &str, item: Option<&str>, fields: Option<&[String]>, v: &Value) {
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
                _ => {
                    eprintln!("unknown packed RPC type {ty}");
                    w
                }
            };
        }
        "raw" => write_raw(w, ty, v),
        "enum" => write_raw(w, "enum", v),
        "struct" => {
            let mut values = v.as_array().into_iter().flatten();
            for field in struct_fields(ty, fields) {
                let value = if field.starts_with("pad") { &Value::Null } else { values.next().unwrap_or(&Value::Null) };
                write_raw(w, &field, value);
            }
        }
        "string" => {
            let units: Vec<u16> = v.as_str().unwrap_or("").encode_utf16().collect();
            w.u32(units.len() as u32);
            units.iter().for_each(|u| {
                w.u16(*u);
            });
        }
        "array" => {
            let items = v.as_array().cloned().unwrap_or_default();
            let item_ty = ty.trim_end_matches("[]");
            w.i32(items.len() as i32);
            for i in &items {
                write_value(w, item.unwrap_or("raw"), item_ty, None, fields, i);
            }
        }
        // WorldInstanceDataSerializationExtensions: [seed, time left, scene, loot rarity,
        // difficulty, recipe group hashes]; a seed of 0 is no instance.
        "serializable" if ty == "WorldInstanceData" => {
            let a = v.as_array().cloned().unwrap_or_default();
            let int = |i: usize| a.get(i).and_then(|x| x.as_i64()).unwrap_or(0) as i32;
            w.i32(int(0));
            if int(0) != 0 {
                w.i32(int(1));
                write_value(w, "string", "string", None, None, a.get(2).unwrap_or(&Value::Null));
                w.i32(int(3)).i32(int(4));
                write_value(w, "array", "int[]", Some("raw"), None, a.get(5).unwrap_or(&Value::Null));
            }
        }
        _ => eprintln!("RPC encoding {enc} {ty} is not supported"),
    }
}

fn read_value(r: &mut Reader, enc: &str, ty: &str, item: Option<&str>, fields: Option<&[String]>) -> Option<Value> {
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
            _ => return None,
        },
        "raw" => read_raw(r, ty)?,
        "enum" => read_raw(r, "enum")?,
        "struct" => {
            let mut out = Vec::new();
            for field in struct_fields(ty, fields) {
                let v = read_raw(r, &field)?;
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
                items.push(read_value(r, item.unwrap_or("raw"), item_ty, None, fields)?);
            }
            Value::Array(items)
        }
        "serializable" if ty == "WorldInstanceData" => {
            let seed = r.i32()?;
            if seed == 0 {
                return Some(Value::Null);
            }
            let time_left = r.i32()?;
            let scene = r.string()?;
            let (loot, difficulty) = (r.i32()?, r.i32()?);
            json!([seed, time_left, scene, loot, difficulty, read_value(r, "array", "int[]", Some("raw"), None)?])
        }
        _ => return None,
    })
}

impl Spec {
    pub fn load(path: &str) -> Result<Spec, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
        let raw: HashMap<String, Rpc> = serde_json::from_str(&text).map_err(|e| format!("{path}: {e}"))?;
        let by_hash: HashMap<u32, Rpc> = raw.into_iter().filter_map(|(h, r)| Some((h.parse().ok()?, r))).collect();
        let by_name = by_hash.iter().map(|(h, r)| (r.name.clone(), *h)).collect();
        Ok(Spec { by_hash, by_name })
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
        let mut w = Writer::new();
        for (i, arg) in rpc.args.iter().enumerate() {
            let v = args.get(i).unwrap_or(&Value::Null);
            if arg.optional {
                w.bool(!v.is_null());
                if v.is_null() {
                    continue;
                }
            }
            write_value(&mut w, &arg.enc, &arg.ty, arg.item.as_deref(), arg.fields.as_deref(), v);
        }
        (hash, w.0)
    }

    pub fn decode(&self, hash: u32, body: &[u8]) -> Option<Vec<Value>> {
        let rpc = self.by_hash.get(&hash)?;
        let mut r = Reader::new(body);
        let mut out = Vec::new();
        for arg in &rpc.args {
            if arg.optional && !r.bool()? {
                out.push(Value::Null);
                continue;
            }
            out.push(read_value(&mut r, &arg.enc, &arg.ty, arg.item.as_deref(), arg.fields.as_deref())?);
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
