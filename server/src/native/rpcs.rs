//! rpcs.json from the game's code: every RPC of its NetworkBehaviours (and of the Netcode classes
//! they extend) with the hash Netcode's code generator gave it (the static
//! `__rpc_handler_<hash>` beside it, loaded by `ldc.i4` in the RPC's own IL), who it goes to,
//! and how each argument is written (NetData.Rpcs). Anything it cannot read is an error.

use std::collections::HashSet;

use serde_json::{json, Map, Value};

use super::dotnet::{Assemblies, Attr, AttrArg, MethodDef, TypeDef, TypeName, TypeSig};

const STATIC: u16 = 0x10;
/// TypeAttributes: the layout bits and their SequentialLayout value.
const LAYOUT: u32 = 0x18;
const SEQUENTIAL: u32 = 0x08;
const NETWORK_BEHAVIOUR: &str = "Unity.Netcode.NetworkBehaviour";
const WRITER: &str = "Unity.Netcode.FastBufferWriter";

type Result<T> = std::result::Result<T, String>;

struct Code<'a> {
    asm: &'a Assemblies,
    /// Types a static `WriteValueSafe(this FastBufferWriter, [in] T)` writes (full names).
    extensions: HashSet<String>,
    /// FastBufferWriter.WriteValueSafe's first parameters: the types it has an overload for.
    writer: Vec<TypeSig>,
}

fn named(sig: &TypeSig) -> Option<&TypeName> {
    match sig {
        TypeSig::Class(t) | TypeSig::ValueType(t) | TypeSig::GenericInst { ty: t, .. } => Some(t),
        _ => None,
    }
}

fn is_named(sig: &TypeSig, full: &str) -> bool {
    named(sig).is_some_and(|n| n.full() == full)
}

impl<'a> Code<'a> {
    fn new(asm: &'a Assemblies) -> Code<'a> {
        let extensions = asm.0.iter().flat_map(|a| a.types()).flat_map(|t| &t.methods)
            .filter(|m| m.name == "WriteValueSafe" && m.flags & STATIC != 0)
            .filter_map(|m| match m.params.as_slice() {
                [(_, w), (_, v)] if is_named(w, WRITER) => named(match v {
                    TypeSig::ByRef(v) => v,
                    v => v,
                })
                .map(TypeName::full),
                _ => None,
            })
            .collect();
        let writer = asm.0.iter().flat_map(|a| a.types()).filter(|t| t.namespace == "Unity.Netcode" && t.name == "FastBufferWriter");
        let writer = writer.flat_map(|t| &t.methods).filter(|m| m.name == "WriteValueSafe" && m.flags & STATIC == 0).filter_map(|m| m.params.first().map(|p| p.1.clone())).collect();
        Code { asm, extensions, writer }
    }

    fn def(&self, t: &TypeName) -> Result<&TypeDef> {
        self.asm.def(t).ok_or_else(|| format!("type {} not found", t.full()))
    }

    fn is_enum(&self, t: &TypeName) -> bool {
        self.asm.base(t).is_some_and(|b| b.full() == "System.Enum")
    }

    fn implements(&self, t: &TypeName, interface: &str) -> bool {
        self.asm.def(t).is_some_and(|d| d.interfaces.iter().any(|i| is_named(i, interface)))
    }

    /// FastBufferWriter has `WriteValueSafe(in T)` (or `(T[])` for an array's items) for `t`.
    fn writer_has(&self, t: &TypeName, array: bool) -> bool {
        self.writer.iter().any(|p| match (p, array) {
            (TypeSig::ByRef(x), false) | (TypeSig::SzArray(x), true) => is_named(x, &t.full()),
            _ => false,
        })
    }

    /// Type.Name as NetData.TypeName writes it: C# aliases, "Outer.Inner" for nested types.
    fn type_name(&self, sig: &TypeSig) -> Result<String> {
        Ok(match sig {
            TypeSig::SzArray(item) => format!("{}[]", self.type_name(item)?),
            TypeSig::I4 => "int".into(),
            TypeSig::U4 => "uint".into(),
            TypeSig::I2 => "short".into(),
            TypeSig::U2 => "ushort".into(),
            TypeSig::I8 => "long".into(),
            TypeSig::U8 => "ulong".into(),
            TypeSig::Bool => "bool".into(),
            TypeSig::R4 => "float".into(),
            TypeSig::R8 => "double".into(),
            TypeSig::U1 => "byte".into(),
            TypeSig::String => "string".into(),
            TypeSig::I1 => "SByte".into(),
            TypeSig::Char => "Char".into(),
            TypeSig::Class(t) | TypeSig::ValueType(t) | TypeSig::GenericInst { ty: t, .. } => t.name.replace('/', "."),
            other => return Err(format!("no type name for {other:?}")),
        })
    }

    /// NetData.Encoding: how Netcode's generated code writes a value of this type. "extension":
    /// a WriteValueSafe extension method the game defines; "serializable": INetworkSerializable;
    /// "struct": INetworkSerializeByMemcpy (the struct's bytes).
    fn encoding(&self, sig: &TypeSig, in_array: bool) -> Result<&'static str> {
        use TypeSig::*;
        Ok(match sig {
            String if !in_array => "string",
            I2 | U2 | I4 | U4 | I8 | U8 if !in_array => "packed",
            Bool | Char | I1 | U1 | I2 | U2 | I4 | U4 | I8 | U8 | R4 | R8 => "raw",
            _ => {
                let t = named(sig).ok_or_else(|| format!("{sig:?}: no Netcode encoding the reader knows"))?;
                if !in_array && self.extensions.contains(&t.full()) {
                    "extension"
                } else if self.is_enum(t) {
                    "enum"
                } else if self.implements(t, "Unity.Netcode.INetworkSerializable") {
                    "serializable"
                } else if self.implements(t, "Unity.Netcode.INetworkSerializeByMemcpy") {
                    "struct"
                } else if self.writer_has(t, in_array) {
                    "raw"
                } else {
                    return Err(format!("{}{}: no Netcode encoding the reader knows", t.full(), if in_array { "[]" } else { "" }));
                }
            }
        })
    }

    fn enum_base(&self, t: &TypeName) -> Result<TypeSig> {
        self.asm.enum_base(t).ok_or_else(|| format!("enum {}: no underlying type", t.full()))
    }

    /// A memcpy struct's instance field types; only sequential layout without packing or a
    /// fixed size (what the layout below computes).
    fn struct_def(&self, t: &TypeName) -> Result<Vec<&TypeSig>> {
        let d = self.def(t)?;
        if d.flags & LAYOUT != SEQUENTIAL || d.class_layout.is_some_and(|l| l != (0, 0)) {
            return Err(format!("struct {}: layout flags {:#x}, class layout {:?} (only sequential is read)", t.full(), d.flags & LAYOUT, d.class_layout));
        }
        let fields: Vec<&TypeSig> = d.fields.iter().filter(|f| f.flags & STATIC == 0).map(|f| &f.sig).collect();
        if fields.is_empty() {
            return Err(format!("struct {}: no fields", t.full()));
        }
        Ok(fields)
    }

    /// In-memory size and alignment, which Netcode's memcpy copies: sequential, naturally
    /// aligned, bool one byte, an enum its underlying type.
    fn layout(&self, sig: &TypeSig) -> Result<(usize, usize)> {
        use TypeSig::*;
        Ok(match sig {
            Bool | I1 | U1 => (1, 1),
            I2 | U2 => (2, 2),
            I4 | U4 | R4 => (4, 4),
            I8 | U8 | R8 => (8, 8),
            ValueType(t) if self.is_enum(t) => self.layout(&self.enum_base(t)?)?,
            ValueType(t) => {
                let (mut size, mut align) = (0usize, 1usize);
                for f in self.struct_def(t)? {
                    let (s, a) = self.layout(f)?;
                    size = size.next_multiple_of(a) + s;
                    align = align.max(a);
                }
                (size.next_multiple_of(align), align)
            }
            other => return Err(format!("{other:?} in a memcpy struct")),
        })
    }

    /// NetData.StructFields: a memcpy struct's field types in layout order ("pad1": a byte of
    /// padding, also at the end up to its size); an enum field is its underlying type.
    fn struct_fields(&self, t: &TypeName) -> Result<Vec<Value>> {
        let mut out = Vec::new();
        let mut at = 0usize;
        for f in self.struct_def(t)? {
            let (size, align) = self.layout(f)?;
            let start = at.next_multiple_of(align);
            out.extend((at..start).map(|_| json!("pad1")));
            let ty = match f {
                TypeSig::ValueType(n) if self.is_enum(n) => self.enum_base(n)?,
                _ => f.clone(),
            };
            out.push(json!(self.type_name(&ty)?));
            at = start + size;
        }
        let (size, _) = self.layout(&TypeSig::ValueType(t.clone()))?;
        out.extend((at..size).map(|_| json!("pad1")));
        Ok(out)
    }

    fn arg(&self, name: &str, sig: &TypeSig) -> Result<Value> {
        let value_type = !matches!(sig, TypeSig::String | TypeSig::SzArray(_) | TypeSig::Array(..) | TypeSig::Object | TypeSig::Class(_)) && !matches!(sig, TypeSig::GenericInst { value_type: false, .. });
        let mut a = Map::new();
        a.insert("name".into(), json!(name));
        a.insert("type".into(), json!(self.type_name(sig)?));
        a.insert("optional".into(), json!(!value_type));
        let (element, enc) = match sig {
            TypeSig::SzArray(item) => {
                a.insert("enc".into(), json!("array"));
                let enc = self.encoding(item, true)?;
                a.insert("item".into(), json!(enc));
                (item.as_ref(), enc)
            }
            _ => {
                let enc = self.encoding(sig, false)?;
                a.insert("enc".into(), json!(enc));
                (sig, enc)
            }
        };
        let element_type = || named(element).ok_or_else(|| format!("{name}: {element:?} has no type name"));
        match enc {
            "struct" => {
                a.insert("fields".into(), Value::Array(self.struct_fields(element_type()?)?));
            }
            // The underlying type, which sets the enum's size on the wire.
            "enum" => {
                a.insert("base".into(), json!(self.type_name(&self.enum_base(element_type()?)?)?));
            }
            _ => {}
        }
        Ok(Value::Object(a))
    }

    /// NetData.Target, from the RPC's attribute ([Rpc(SendTo.X)] names X, lowercased).
    fn target(&self, attr: &Attr) -> Result<String> {
        match attr.ty.name.as_str() {
            "ServerRpcAttribute" => Ok("server".into()),
            "ClientRpcAttribute" => Ok("client".into()),
            _ => {
                let value = match attr.fixed.first() {
                    Some(AttrArg::Int(v)) => *v,
                    Some(AttrArg::UInt(v)) => i64::try_from(*v).map_err(|_| format!("SendTo {v}"))?,
                    other => return Err(format!("[Rpc] without a SendTo: {other:?}")),
                };
                let send_to = TypeName { scope: Some("Unity.Netcode.Runtime".into()), namespace: "Unity.Netcode".into(), name: "SendTo".into() };
                let field = self.def(&send_to)?.fields.iter().find(|f| f.constant == Some(value));
                field.map(|f| f.name.to_lowercase()).ok_or_else(|| format!("SendTo has no member {value}"))
            }
        }
    }
}

/// The method's Netcode RPC attribute ([ServerRpc], [ClientRpc] or [Rpc]).
fn rpc_attribute(m: &MethodDef) -> Option<&Attr> {
    m.attributes.iter().find(|a| a.ty.namespace == "Unity.Netcode" && matches!(a.ty.name.as_str(), "ServerRpcAttribute" | "ClientRpcAttribute" | "RpcAttribute"))
}

/// The distinct handler hashes the IL loads with `ldc.i4`.
fn hashes_in(il: &[u8], hashes: &[u32]) -> Vec<u32> {
    let mut found: Vec<u32> = (0..il.len().saturating_sub(4)).filter(|i| il[*i] == 0x20).map(|i| u32::from_le_bytes(il[i + 1..i + 5].try_into().unwrap())).filter(|v| hashes.contains(v)).collect();
    found.sort_unstable();
    found.dedup();
    found
}

fn is_rpc_params(sig: &TypeSig) -> bool {
    named(sig).is_some_and(|n| n.namespace == "Unity.Netcode" && matches!(n.name.as_str(), "ServerRpcParams" | "ClientRpcParams" | "RpcParams"))
}

pub fn rpcs(asm: &Assemblies) -> Result<Map<String, Value>> {
    let code = Code::new(asm);
    let game = asm.0.iter().position(|a| a.name == "Assembly-CSharp").ok_or("no Assembly-CSharp")?;
    // The game's NetworkBehaviours in TypeDef order, then the other assemblies' classes between
    // them and NetworkBehaviour (Netcode's NetworkAnimator under ClientNetworkAnimator, ...).
    let mut types: Vec<(usize, usize)> = Vec::new();
    let mut bases: Vec<(usize, usize)> = Vec::new();
    for (i, t) in asm.0[game].types().iter().enumerate() {
        let mut name = TypeName { scope: Some(asm.0[game].name.clone()), namespace: t.namespace.clone(), name: t.name.clone() };
        if !asm.derives_from(&name, NETWORK_BEHAVIOUR) {
            continue;
        }
        types.push((game, i));
        while name.full() != NETWORK_BEHAVIOUR {
            let at = asm.resolve(&name).ok_or_else(|| format!("type {} not found", name.full()))?;
            if at.0 != game && !bases.contains(&at) {
                bases.push(at);
            }
            name = asm.base(&name).ok_or_else(|| format!("{}: no base class", name.full()))?;
        }
    }
    types.extend(bases);
    let mut out = Map::new();
    let mut names = HashSet::new();
    for (a, i) in types {
        let (assembly, t) = (&asm.0[a], &asm.0[a].types()[i]);
        let class = t.name.rsplit('/').next().unwrap_or(&t.name);
        let mut handlers = Vec::new();
        for m in t.methods.iter().filter(|m| m.flags & STATIC != 0) {
            if let Some(h) = m.name.strip_prefix("__rpc_handler_") {
                handlers.push(h.parse::<u32>().map_err(|_| format!("{class}.{}: not a hash", m.name))?);
            }
        }
        for m in &t.methods {
            let Some(attr) = rpc_attribute(m) else { continue };
            let full = format!("{class}.{}", m.name);
            let il = assembly.il(m).ok_or_else(|| format!("{full}: no IL"))?;
            let [hash] = hashes_in(il, &handlers)[..] else { return Err(format!("{full}: loads {:?} of its class's handler hashes, not one", hashes_in(il, &handlers))) };
            let args = m.params.iter().filter(|(_, sig)| !is_rpc_params(sig)).map(|(n, sig)| code.arg(n, sig)).collect::<Result<Vec<Value>>>().map_err(|e| format!("{full}: {e}"))?;
            let to = code.target(attr).map_err(|e| format!("{full}: {e}"))?;
            if out.insert(hash.to_string(), json!({"name": full, "to": to, "args": args})).is_some() {
                return Err(format!("{full}: hash {hash} already taken"));
            }
            if !names.insert(full.clone()) {
                return Err(format!("{full}: two RPCs of that name"));
            }
        }
        if let Some(h) = handlers.iter().find(|h| !out.contains_key(&h.to_string())) {
            return Err(format!("{class}: __rpc_handler_{h} has no RPC method loading its hash"));
        }
    }
    if out.is_empty() {
        return Err("no RPCs in Assembly-CSharp".into());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(namespace: &str, name: &str) -> TypeName {
        TypeName { scope: None, namespace: namespace.into(), name: name.into() }
    }

    #[test]
    fn finds_one_handler_hash_per_rpc() {
        let mut il = vec![0x02, 0x20];
        il.extend(7u32.to_le_bytes());
        il.extend([0x20, 9, 0, 0, 0, 0x20]);
        il.extend(7u32.to_le_bytes());
        assert_eq!(hashes_in(&il, &[7, 8]), vec![7]);
        assert_eq!(hashes_in(&il, &[7, 9]), vec![7, 9]);
        assert!(hashes_in(&il[..5], &[7]).is_empty());
    }

    #[test]
    fn needs_assembly_csharp() {
        assert_eq!(rpcs(&Assemblies(Vec::new())).unwrap_err(), "no Assembly-CSharp");
    }

    #[test]
    fn rpc_params_are_netcodes_only() {
        assert!(is_rpc_params(&TypeSig::ValueType(name("Unity.Netcode", "ServerRpcParams"))));
        assert!(!is_rpc_params(&TypeSig::ValueType(name("SpaceCraft", "ServerRpcParams"))));
    }

    /// Against the installed game: every handler has its RPC (the Netcode bases' included),
    /// enums carry their underlying type, the memcpy structs their in-memory layout.
    #[test]
    fn reads_the_games_rpcs() {
        let Some(dir) = crate::testkit::managed() else { return };
        let asm = Assemblies::load_dir(&dir).unwrap();
        let code = Code::new(&asm);
        let rpcs = rpcs(&asm).unwrap();
        let by_name = |n: &str| rpcs.values().find(|r| r["name"] == n).unwrap_or_else(|| panic!("no {n}"));
        assert_eq!(by_name("NetworkTransform.SetStateServerRpc")["to"], "server");
        assert_eq!(by_name("NetworkAnimator.SendAnimStateServerRpc")["args"][0]["enc"], "serializable");
        assert_eq!(by_name("WorldInstanceHandler.SetAndOpenWorldInstanceSceneServerRpc")["args"][0]["enc"], "extension");
        assert_eq!(by_name("MessagesHandler.MarkAsReadServerRpc")["args"][0]["fields"], json!(["bool", "pad1", "pad1", "pad1", "int"]));
        assert_eq!(by_name("GeneticTraitHandler.CreateNewTraitWorldObjectServerRpc")["args"][0]["fields"], json!(["int", "Color", "int"]));
        let enums: Vec<&Value> = rpcs.values().flat_map(|r| r["args"].as_array().unwrap()).filter(|a| a["enc"] == "enum").collect();
        assert!(!enums.is_empty() && enums.iter().all(|a| a["base"].is_string()));
        // In-memory layout, not Marshal's: a bool is one byte, a char is refused.
        assert_eq!(code.layout(&TypeSig::ValueType(name("SpaceCraft", "Message"))).unwrap(), (8, 4));
        assert!(code.layout(&TypeSig::Char).is_err());
        assert!(code.encoding(&TypeSig::ValueType(name("UnityEngine", "Vector3")), false).is_ok());
        assert!(code.encoding(&TypeSig::Object, false).is_err());
    }
}
