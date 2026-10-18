//! rpcs.json from the game's code: every RPC of its NetworkBehaviours with the hash Netcode's
//! code generator gave it (the static `__rpc_handler_<hash>` beside it, loaded by `ldc.i4` in
//! the RPC's own IL), who it goes to, and how each argument is written (NetData.Rpcs).

use serde_json::{json, Map, Value};

use super::dotnet::{Assemblies, AttrArg, TypeDef, TypeName, TypeSig};

const STATIC: u16 = 0x10;

fn full(t: &TypeName) -> String {
    if t.namespace.is_empty() {
        t.name.clone()
    } else {
        format!("{}.{}", t.namespace, t.name)
    }
}

struct Code<'a> {
    asm: &'a Assemblies,
}

impl Code<'_> {
    fn def(&self, t: &TypeName) -> Option<&TypeDef> {
        let (a, i) = self.asm.resolve(t)?;
        self.asm.0[a].types().get(i)
    }

    fn base(&self, t: &TypeName) -> Option<TypeName> {
        match self.def(t)?.extends.as_ref()? {
            TypeSig::Class(b) | TypeSig::ValueType(b) | TypeSig::GenericInst { ty: b, .. } => Some(b.clone()),
            _ => None,
        }
    }

    fn derives_from(&self, t: &TypeName, ancestor: &str) -> bool {
        let mut cur = Some(t.clone());
        for _ in 0..32 {
            let Some(c) = cur else { return false };
            if full(&c) == ancestor {
                return true;
            }
            cur = self.base(&c);
        }
        false
    }

    fn is_enum(&self, t: &TypeName) -> bool {
        self.base(t).is_some_and(|b| full(&b) == "System.Enum")
    }

    fn implements(&self, t: &TypeName, interface: &str) -> bool {
        self.def(t).is_some_and(|d| {
            d.interfaces.iter().any(|i| match i {
                TypeSig::Class(n) | TypeSig::GenericInst { ty: n, .. } => full(n) == interface,
                _ => false,
            })
        })
    }

    /// Type.Name as NetData.TypeName writes it: C# aliases, "Outer.Inner" for nested types.
    fn type_name(&self, sig: &TypeSig) -> String {
        match sig {
            TypeSig::SzArray(item) => format!("{}[]", self.type_name(item)),
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
            _ => "?".into(),
        }
    }

    fn named(sig: &TypeSig) -> Option<&TypeName> {
        match sig {
            TypeSig::Class(t) | TypeSig::ValueType(t) | TypeSig::GenericInst { ty: t, .. } => Some(t),
            _ => None,
        }
    }

    /// NetData.Encoding: how Netcode's generated code writes a value of this type.
    fn encoding(&self, sig: &TypeSig, in_array: bool) -> &'static str {
        match sig {
            TypeSig::String => return "string",
            TypeSig::I4 | TypeSig::U4 | TypeSig::I2 | TypeSig::U2 | TypeSig::I8 | TypeSig::U8 if !in_array => return "packed",
            TypeSig::Bool | TypeSig::Char | TypeSig::I1 | TypeSig::U1 | TypeSig::I2 | TypeSig::U2 | TypeSig::I4 | TypeSig::U4 | TypeSig::I8 | TypeSig::U8 | TypeSig::R4 | TypeSig::R8 | TypeSig::IntPtr | TypeSig::UIntPtr => return "raw",
            _ => {}
        }
        let Some(t) = Self::named(sig) else { return "serializable" };
        if self.is_enum(t) {
            "enum"
        } else if t.namespace == "UnityEngine" {
            "raw"
        } else if self.implements(t, "Unity.Netcode.INetworkSerializeByMemcpy") {
            "struct"
        } else {
            "serializable"
        }
    }

    /// Marshalled size and alignment of a field (Marshal.OffsetOf's layout: bool is 4 bytes).
    fn marshal_size(&self, sig: &TypeSig) -> (usize, usize) {
        match sig {
            TypeSig::Bool | TypeSig::I4 | TypeSig::U4 | TypeSig::R4 => (4, 4),
            TypeSig::I1 | TypeSig::U1 | TypeSig::Char => (1, 1),
            TypeSig::I2 | TypeSig::U2 => (2, 2),
            TypeSig::I8 | TypeSig::U8 | TypeSig::R8 => (8, 8),
            _ => {
                let Some(t) = Self::named(sig) else { return (8, 8) };
                if self.is_enum(t) {
                    return (4, 4);
                }
                let fields = self.instance_fields(t);
                let (mut size, mut align) = (0usize, 1usize);
                for f in &fields {
                    let (s, a) = self.marshal_size(f);
                    size = size.next_multiple_of(a) + s;
                    align = align.max(a);
                }
                (size.next_multiple_of(align), align)
            }
        }
    }

    fn instance_fields(&self, t: &TypeName) -> Vec<TypeSig> {
        self.def(t).map(|d| d.fields.iter().filter(|f| f.flags & STATIC == 0).map(|f| f.sig.clone()).collect()).unwrap_or_default()
    }

    /// NetData.StructFields: a memcpy struct's field types in layout order, with padding.
    fn struct_fields(&self, t: &TypeName) -> Vec<Value> {
        let mut out = Vec::new();
        let mut end = 0usize;
        let mut at = 0usize;
        for f in self.def(t).map(|d| d.fields.iter().filter(|f| f.flags & STATIC == 0).collect::<Vec<_>>()).unwrap_or_default() {
            let (size, align) = self.marshal_size(&f.sig);
            at = at.next_multiple_of(align);
            out.extend((end..at).map(|_| json!("pad1")));
            let is_enum = Self::named(&f.sig).is_some_and(|n| self.is_enum(n));
            out.push(json!(if is_enum { "enum".to_string() } else { self.type_name(&f.sig) }));
            // As NetData counts it: a bool takes one byte, everything else its marshalled size.
            end = at + if matches!(f.sig, TypeSig::Bool) { 1 } else { size };
            at += size;
        }
        out
    }

    fn arg(&self, name: &str, sig: &TypeSig) -> Value {
        let value_type = !matches!(sig, TypeSig::String | TypeSig::SzArray(_) | TypeSig::Array(..) | TypeSig::Object | TypeSig::Class(_)) && !matches!(sig, TypeSig::GenericInst { value_type: false, .. });
        let mut a = Map::new();
        a.insert("name".into(), json!(name));
        a.insert("type".into(), json!(self.type_name(sig)));
        a.insert("optional".into(), json!(!value_type));
        let element = match sig {
            TypeSig::SzArray(item) => {
                a.insert("enc".into(), json!("array"));
                a.insert("item".into(), json!(self.encoding(item, true)));
                item.as_ref()
            }
            _ => {
                a.insert("enc".into(), json!(self.encoding(sig, false)));
                sig
            }
        };
        if self.encoding(element, false) == "struct" {
            if let Some(t) = Self::named(element) {
                a.insert("fields".into(), Value::Array(self.struct_fields(t)));
            }
        }
        Value::Object(a)
    }

    /// NetData.Target, from the RPC's attribute ([Rpc(SendTo.X)] names X, lowercased).
    fn target(&self, attrs: &[super::dotnet::Attr]) -> String {
        for a in attrs {
            match a.ty.name.as_str() {
                "ServerRpcAttribute" => return "server".into(),
                "ClientRpcAttribute" => return "client".into(),
                "RpcAttribute" => {
                    let value = match a.fixed.first() {
                        Some(AttrArg::Int(v)) => *v,
                        Some(AttrArg::UInt(v)) => *v as i64,
                        _ => return "server".into(),
                    };
                    let send_to = TypeName { scope: Some("Unity.Netcode.Runtime".into()), namespace: "Unity.Netcode".into(), name: "SendTo".into() };
                    let name = self.def(&send_to).and_then(|d| d.fields.iter().find(|f| f.constant == Some(value)).map(|f| f.name.to_lowercase()));
                    return name.unwrap_or_else(|| value.to_string());
                }
                _ => {}
            }
        }
        "?".into()
    }
}

/// The first `ldc.i4` operand in the IL that is one of the handler hashes.
fn hash_in(il: &[u8], hashes: &[u32]) -> Option<u32> {
    (0..il.len().saturating_sub(4)).filter(|i| il[*i] == 0x20).map(|i| u32::from_le_bytes(il[i + 1..i + 5].try_into().unwrap())).find(|v| hashes.contains(v))
}

pub fn rpcs(asm: &Assemblies) -> Map<String, Value> {
    let code = Code { asm };
    let mut out = Map::new();
    let Some(game) = asm.0.iter().find(|a| a.name == "Assembly-CSharp") else { return out };
    for t in game.types() {
        let name = TypeName { scope: Some(game.name.clone()), namespace: t.namespace.clone(), name: t.name.clone() };
        if !code.derives_from(&name, "Unity.Netcode.NetworkBehaviour") {
            continue;
        }
        let hashes: Vec<u32> = t.methods.iter().filter(|m| m.flags & STATIC != 0).filter_map(|m| m.name.strip_prefix("__rpc_handler_")?.parse().ok()).collect();
        let simple = t.name.rsplit('/').next().unwrap_or(&t.name);
        for m in t.methods.iter().filter(|m| m.flags & STATIC == 0 && m.name.ends_with("Rpc") && !m.name.starts_with("__")) {
            let Some(hash) = game.il(m).and_then(|il| hash_in(il, &hashes)) else { continue };
            if out.contains_key(&hash.to_string()) {
                continue;
            }
            let args: Vec<Value> = m.params.iter().filter(|(_, sig)| !code.type_name(sig).ends_with("RpcParams")).map(|(n, sig)| code.arg(n, sig)).collect();
            out.insert(hash.to_string(), json!({"name": format!("{simple}.{}", m.name), "to": code.target(&m.attributes), "args": args}));
        }
    }
    out
}
