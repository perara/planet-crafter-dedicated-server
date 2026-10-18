//! Unity's serialization of the game's MonoBehaviour and ScriptableObject fields, rebuilt from
//! the scripts' assemblies (a player build ships no type trees): which fields are serialized, in
//! what order and encoding. `read` turns an object's data into the values GameData.cs writes.

use std::collections::HashMap;
use std::rc::Rc;

use serde_json::{json, Value};

use super::dotnet::{Assemblies, TypeName, TypeSig};
use super::unity::{PPtr, Reader};

/// How Unity serializes one field.
#[derive(Debug)]
pub enum Kind {
    Bool,
    I8,
    U8,
    I16,
    U16,
    Char,
    I32,
    U32,
    I64,
    U64,
    F32,
    F64,
    String,
    /// An enum: its underlying integer, the names by value, whether it is [Flags].
    Enum(Box<Kind>, Rc<Vec<(i64, String)>>, bool),
    /// A UnityEngine.Object reference; `class` is the field's declared type.
    Object(TypeName),
    Array(Box<Kind>),
    /// A [Serializable] class or struct, inline: its full type name and fields.
    Struct(TypeName, Rc<Vec<Field>>),
    Builtin(Builtin),
    /// A field GameData.cs writes that Unity does not serialize (an event, a dictionary...):
    /// no data, and its value at runtime is unset.
    Runtime,
    /// Not stored either, but the constructor creates it empty: [] at runtime.
    Created,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Builtin {
    Vector2,
    Vector3,
    Vector4,
    Quaternion,
    Color,
    Color32,
    Rect,
    Bounds,
    Matrix4x4,
    LayerMask,
    Vector2Int,
    Vector3Int,
    AnimationCurve,
    Gradient,
    Hash128,
}

#[derive(Debug)]
pub struct Field {
    pub name: String,
    pub kind: Kind,
    /// How far down the class hierarchy it is declared (the root base class's fields are 0).
    pub level: usize,
}

/// Unity's limit on nested serializable classes.
const MAX_NESTING: usize = 10;

pub struct Layouts {
    pub asm: Rc<Assemblies>,
    cache: HashMap<String, Rc<Vec<Field>>>,
}

fn full(t: &TypeName) -> String {
    if t.namespace.is_empty() {
        t.name.clone()
    } else {
        format!("{}.{}", t.namespace, t.name)
    }
}

/// Unity's own types serialized as plain data.
fn builtin(name: &str) -> Option<Builtin> {
    Some(match name {
        "UnityEngine.Vector2" => Builtin::Vector2,
        "UnityEngine.Vector3" => Builtin::Vector3,
        "UnityEngine.Vector4" => Builtin::Vector4,
        "UnityEngine.Quaternion" => Builtin::Quaternion,
        "UnityEngine.Color" => Builtin::Color,
        "UnityEngine.Color32" => Builtin::Color32,
        "UnityEngine.Rect" => Builtin::Rect,
        "UnityEngine.Bounds" => Builtin::Bounds,
        "UnityEngine.Matrix4x4" => Builtin::Matrix4x4,
        "UnityEngine.LayerMask" => Builtin::LayerMask,
        "UnityEngine.Vector2Int" => Builtin::Vector2Int,
        "UnityEngine.Vector3Int" => Builtin::Vector3Int,
        "UnityEngine.AnimationCurve" => Builtin::AnimationCurve,
        "UnityEngine.Gradient" => Builtin::Gradient,
        "UnityEngine.Hash128" => Builtin::Hash128,
        _ => return None,
    })
}

impl Layouts {
    pub fn new(asm: Rc<Assemblies>) -> Layouts {
        Layouts { asm, cache: HashMap::new() }
    }

    /// Whether a type derives from UnityEngine.Object.
    fn is_unity_object(&self, t: &TypeName) -> bool {
        let mut name = t.clone();
        for _ in 0..32 {
            if full(&name) == "UnityEngine.Object" {
                return true;
            }
            let Some((a, i)) = self.asm.resolve(&name) else { return false };
            match &self.asm.0[a].types()[i].extends {
                Some(TypeSig::Class(b)) | Some(TypeSig::GenericInst { ty: b, .. }) => name = b.clone(),
                _ => return false,
            }
        }
        false
    }

    /// The serialized fields of a script class, base classes first, as Unity lays them out
    /// (MonoBehaviour's own header excluded). `generic` substitutes the class's type
    /// parameters.
    pub fn fields(&mut self, ty: &TypeName) -> Rc<Vec<Field>> {
        let key = full(ty);
        if let Some(f) = self.cache.get(&key) {
            return f.clone();
        }
        let fields = Rc::new(self.class_fields(ty, &[], 0));
        self.cache.insert(key, fields.clone());
        fields
    }

    fn class_fields(&mut self, ty: &TypeName, generic: &[TypeSig], depth: usize) -> Vec<Field> {
        let asm = self.asm.clone();
        let Some((a, i)) = asm.resolve(ty) else { return Vec::new() };
        let def = &asm.0[a].types()[i];
        // Unity stops at its own base classes: they serialize nothing of the script's.
        let mut out = match &def.extends {
            Some(TypeSig::Class(b)) if !matches!(full(b).as_str(), "UnityEngine.MonoBehaviour" | "UnityEngine.ScriptableObject" | "UnityEngine.Object" | "System.Object" | "UnityEngine.Behaviour" | "UnityEngine.Component") => self.class_fields(b, &[], depth),
            Some(TypeSig::GenericInst { ty: b, args, .. }) => {
                let args: Vec<TypeSig> = args.iter().map(|x| substitute(x, generic)).collect();
                self.class_fields(b, &args, depth)
            }
            _ => Vec::new(),
        };
        let level = out.iter().map(|f| f.level + 1).max().unwrap_or(0);
        let mut created: Option<Vec<String>> = None;
        let fields: Vec<(String, TypeSig, u16, bool, bool)> = def
            .fields
            .iter()
            .map(|f| {
                let has = |n: &str| f.attributes.iter().any(|x| full(&x.ty) == n);
                (f.name.clone(), substitute(&f.sig, generic), f.flags, has("UnityEngine.SerializeField"), has("UnityEngine.SerializeReference"))
            })
            .collect();
        for (name, sig, flags, serialize_field, serialize_reference) in fields {
            const STATIC: u16 = 0x10;
            const INIT_ONLY: u16 = 0x20;
            const NOT_SERIALIZED: u16 = 0x80;
            let public = flags & 0x7 == 0x6;
            // What GameData.cs writes: public (and not [NonSerialized]) or [SerializeField].
            let written = flags & STATIC == 0 && ((public && flags & NOT_SERIALIZED == 0) || serialize_field);
            // What Unity stores: the same, minus readonly fields and types it cannot store.
            let stored = written && flags & INIT_ONLY == 0 || serialize_reference && flags & STATIC == 0;
            if serialize_reference {
                // ponytail: [SerializeReference] fields (a managed reference id, the objects in
                // a registry after the fields) are not read; none of the tables need one so far.
                out.push(Field { name, kind: Kind::I64, level });
                continue;
            }
            match stored.then(|| self.kind(&sig, depth)).flatten() {
                Some(kind) => out.push(Field { name, kind, level }),
                // A written field Unity cannot store keeps its default: null for a reference type.
                None if written && !is_value_type(&sig) => {
                    let created = created.get_or_insert_with(|| super::netvars::created_collections(&asm, ty)).contains(&name);
                    out.push(Field { name, kind: if created { Kind::Created } else { Kind::Runtime }, level })
                }
                None => {}
            }
        }
        out
    }

    /// How a field of this type is serialized; None when Unity does not serialize it.
    fn kind(&mut self, sig: &TypeSig, depth: usize) -> Option<Kind> {
        Some(match sig {
            TypeSig::Bool => Kind::Bool,
            TypeSig::I1 => Kind::I8,
            TypeSig::U1 => Kind::U8,
            TypeSig::I2 => Kind::I16,
            TypeSig::U2 => Kind::U16,
            TypeSig::Char => Kind::Char,
            TypeSig::I4 => Kind::I32,
            TypeSig::U4 => Kind::U32,
            TypeSig::I8 => Kind::I64,
            TypeSig::U8 => Kind::U64,
            TypeSig::R4 => Kind::F32,
            TypeSig::R8 => Kind::F64,
            TypeSig::String => Kind::String,
            TypeSig::SzArray(item) => Kind::Array(Box::new(self.item(item, depth)?)),
            TypeSig::GenericInst { ty, args, .. } if full(ty) == "System.Collections.Generic.List`1" => Kind::Array(Box::new(self.item(args.first()?, depth)?)),
            TypeSig::Class(t) | TypeSig::ValueType(t) => self.named(t, &[], depth)?,
            TypeSig::GenericInst { ty, args, .. } => self.named(ty, args, depth)?,
            _ => return None,
        })
    }

    /// An array's item: no arrays of arrays.
    fn item(&mut self, sig: &TypeSig, depth: usize) -> Option<Kind> {
        if matches!(sig, TypeSig::SzArray(_)) || matches!(sig, TypeSig::GenericInst { ty, .. } if full(ty) == "System.Collections.Generic.List`1") {
            return None;
        }
        self.kind(sig, depth)
    }

    fn named(&mut self, t: &TypeName, args: &[TypeSig], depth: usize) -> Option<Kind> {
        let name = full(t);
        if let Some(b) = builtin(&name) {
            return Some(Kind::Builtin(b));
        }
        if self.is_unity_object(t) {
            return Some(Kind::Object(t.clone()));
        }
        let asm = self.asm.clone();
        let (a, i) = asm.resolve(t)?;
        let def = &asm.0[a].types()[i];
        let base = def.extends.as_ref().and_then(|e| match e {
            TypeSig::Class(b) | TypeSig::ValueType(b) => Some(full(b)),
            TypeSig::GenericInst { ty, .. } => Some(full(ty)),
            _ => None,
        });
        if base.as_deref() == Some("System.Enum") {
            let underlying = def.fields.iter().find(|f| f.flags & 0x10 == 0).and_then(|f| self.kind(&f.sig, depth)).unwrap_or(Kind::I32);
            let names: Vec<(i64, String)> = def.fields.iter().filter(|f| f.flags & 0x40 != 0).map(|f| (f.constant.unwrap_or(0), f.name.clone())).collect();
            let flags = def.attributes.iter().any(|x| full(&x.ty) == "System.FlagsAttribute");
            return Some(Kind::Enum(Box::new(underlying), Rc::new(names), flags));
        }
        // [Serializable] classes and structs (not abstract, not interfaces), nested inline.
        const SERIALIZABLE: u32 = 0x2000;
        const ABSTRACT: u32 = 0x80;
        const INTERFACE: u32 = 0x20;
        if def.flags & SERIALIZABLE == 0 || def.flags & (ABSTRACT | INTERFACE) != 0 || depth >= MAX_NESTING || name.starts_with("System.") {
            return None;
        }
        let fields = Rc::new(self.class_fields(t, args, depth + 1));
        Some(Kind::Struct(t.clone(), fields))
    }
}

fn is_value_type(sig: &TypeSig) -> bool {
    matches!(sig, TypeSig::ValueType(_) | TypeSig::GenericInst { value_type: true, .. }) || !matches!(sig, TypeSig::Class(_) | TypeSig::GenericInst { .. } | TypeSig::SzArray(_) | TypeSig::Array(..) | TypeSig::String | TypeSig::Object)
}

/// A generic parameter replaced by its argument.
fn substitute(sig: &TypeSig, args: &[TypeSig]) -> TypeSig {
    match sig {
        TypeSig::Var(n) => args.get(*n as usize).cloned().unwrap_or(sig.clone()),
        TypeSig::SzArray(x) => TypeSig::SzArray(Box::new(substitute(x, args))),
        TypeSig::GenericInst { ty, value_type, args: inner } => TypeSig::GenericInst { ty: ty.clone(), value_type: *value_type, args: inner.iter().map(|x| substitute(x, args)).collect() },
        _ => sig.clone(),
    }
}

/// A value read from data, before references are named.
#[derive(Debug, Clone)]
pub enum Raw {
    Null,
    Bool(bool),
    Int(i64),
    UInt(u64),
    Float(f64),
    Single(f32),
    Str(String),
    Enum(i64, Rc<Vec<(i64, String)>>, bool),
    Ref(PPtr, TypeName),
    List(Vec<Raw>),
    Fields(TypeName, Vec<(String, Raw)>),
    /// Already in GameData.cs form (vectors, colours, curves, layer masks).
    Json(Value),
    /// A value GameData.cs leaves out (Unity structs it has no case for).
    Omit,
    /// A field with no serialized data: null at runtime.
    Unset,
}

fn read_builtin(b: Builtin, r: &mut Reader) -> Option<Raw> {
    let floats = |r: &mut Reader, n: usize| -> Option<Value> { Some(Value::Array((0..n).map(|_| r.f32().map(super::tables::single)).collect::<Option<Vec<Value>>>()?)) };
    Some(Raw::Json(match b {
        Builtin::Vector2 => floats(r, 2)?,
        Builtin::Vector3 => floats(r, 3)?,
        Builtin::Vector4 | Builtin::Quaternion | Builtin::Color => floats(r, 4)?,
        Builtin::Rect => {
            r.bytes(16)?;
            return Some(Raw::Omit);
        }
        Builtin::Bounds => {
            r.bytes(24)?;
            return Some(Raw::Omit);
        }
        Builtin::Matrix4x4 => {
            r.bytes(64)?;
            return Some(Raw::Omit);
        }
        Builtin::Color32 => {
            r.u32()?;
            return Some(Raw::Omit);
        }
        Builtin::LayerMask => json!(r.u32()?),
        Builtin::Vector2Int => json!([r.i32()?, r.i32()?]),
        Builtin::Vector3Int => json!([r.i32()?, r.i32()?, r.i32()?]),
        Builtin::Hash128 => {
            r.bytes(16)?;
            return Some(Raw::Omit);
        }
        Builtin::AnimationCurve => {
            // m_Curve: keyframes (time, value, inSlope, outSlope, weightedMode, inWeight,
            // outWeight), then pre/post infinity and rotation order.
            let mut keys = Vec::new();
            for _ in 0..r.count()? {
                let (time, value, inslope, outslope) = (r.f32()?, r.f32()?, r.f32()?, r.f32()?);
                r.i32()?;
                r.f32()?;
                r.f32()?;
                keys.push(Value::Array([time, value, inslope, outslope].map(super::tables::single).to_vec()));
            }
            r.i32()?;
            r.i32()?;
            r.i32()?;
            json!(keys)
        }
        Builtin::Gradient => {
            // ponytail: 8 colour keys, then 8 alpha... read as Unity 6 lays them out; the
            // tables carry no gradient (GameData.cs writes none).
            r.bytes(8 * 16)?;
            r.bytes(8 * 2)?;
            r.bytes(8 * 2)?;
            r.i32()?;
            r.u8()?;
            r.u8()?;
            r.u8()?;
            r.align();
            return Some(Raw::Omit);
        }
    }))
}

/// Reads a field list (an object's script fields, or a nested struct's).
/// The fields' values, read in Unity's order (base class first) and returned in .NET's
/// (Type.GetFields: the class's own fields, then each base class's).
pub fn read_fields(fields: &[Field], r: &mut Reader) -> Option<Vec<(String, Raw)>> {
    let mut out = Vec::with_capacity(fields.len());
    for f in fields {
        let v = read_kind(&f.kind, r)?;
        out.push((f.level, f.name.clone(), v));
    }
    out.sort_by_key(|(level, ..)| std::cmp::Reverse(*level));
    Some(out.into_iter().map(|(_, n, v)| (n, v)).collect())
}

fn read_kind(kind: &Kind, r: &mut Reader) -> Option<Raw> {
    read_value(kind, r, false)
}

/// `item`: an array element, which Unity packs (only the whole array is padded).
fn read_value(kind: &Kind, r: &mut Reader, item: bool) -> Option<Raw> {
    // Fields smaller than 4 bytes are padded to 4, as are strings and arrays.
    let pad = |r: &mut Reader| {
        if !item {
            r.align();
        }
    };
    let v = match kind {
        Kind::Bool => {
            let b = r.bool()?;
            pad(r);
            Raw::Bool(b)
        }
        Kind::I8 => {
            let v = r.u8()? as i8;
            pad(r);
            Raw::Int(v as i64)
        }
        Kind::U8 => {
            let v = r.u8()?;
            pad(r);
            Raw::UInt(v as u64)
        }
        Kind::I16 => {
            let v = r.i16()?;
            pad(r);
            Raw::Int(v as i64)
        }
        Kind::U16 | Kind::Char => {
            let v = r.u16()?;
            pad(r);
            Raw::UInt(v as u64)
        }
        Kind::I32 => Raw::Int(r.i32()? as i64),
        Kind::U32 => Raw::UInt(r.u32()? as u64),
        Kind::I64 => Raw::Int(r.i64()?),
        Kind::U64 => Raw::UInt(r.u64()?),
        Kind::F32 => Raw::Single(r.f32()?),
        Kind::F64 => Raw::Float(r.f64()?),
        Kind::String => Raw::Str(r.string()?),
        Kind::Enum(underlying, names, flags) => {
            let v = match read_kind(underlying, r)? {
                Raw::Int(v) => v,
                Raw::UInt(v) => v as i64,
                _ => 0,
            };
            Raw::Enum(v, names.clone(), *flags)
        }
        Kind::Object(t) => Raw::Ref(r.pptr()?, t.clone()),
        Kind::Array(item) => {
            let n = r.count()?;
            let mut items = Vec::with_capacity(n.min(4096));
            for _ in 0..n {
                items.push(read_value(item, r, true)?);
            }
            r.align();
            Raw::List(items)
        }
        Kind::Struct(t, fields) => {
            let v = read_fields(fields, r)?;
            Raw::Fields(t.clone(), v)
        }
        Kind::Builtin(b) => read_builtin(*b, r)?,
        Kind::Runtime => Raw::Unset,
        Kind::Created => Raw::Json(Value::Array(Vec::new())),
    };
    Some(v)
}

/// Enum.ToString(): the name, "A, B" for flag combinations, else the number.
pub fn enum_name(v: i64, names: &[(i64, String)], flags: bool) -> String {
    if let Some((_, n)) = names.iter().find(|(x, _)| *x == v) {
        return n.clone();
    }
    if flags && v != 0 {
        let mut parts: Vec<&str> = Vec::new();
        let mut rest = v;
        for (x, n) in names.iter().rev() {
            if *x != 0 && rest & x == *x {
                parts.push(n);
                rest &= !x;
            }
        }
        if rest == 0 {
            parts.reverse();
            return parts.join(", ");
        }
    }
    v.to_string()
}
