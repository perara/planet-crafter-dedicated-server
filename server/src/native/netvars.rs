//! A NetworkBehaviour class's Netcode variables as a spawn writes them: in the order Netcode's
//! code generator registers them (`__initializeVariables`, base class where it calls the
//! base's), with the type name and the encoded value its constructor gives each. Variables of
//! a type with its own serializer (custom NetworkVariableBase classes, INetworkSerializable
//! values) get no value here: the server must set those itself.

use serde_json::{json, Value};

use super::dotnet::{Assemblies, Assembly, Member, TypeName, TypeSig};
use super::serial::Raw;

/// One IL instruction: its opcode (0xFExx for two-byte ones) and operand.
struct Op {
    code: u16,
    arg: i64,
    /// Byte offset of the instruction, and of the next one (branch targets count from it).
    at: usize,
    end: usize,
    /// switch: its targets' offsets.
    targets: Vec<usize>,
}

/// Operand size of an opcode (ECMA-335 III); None for unknown opcodes.
fn operand(code: u16) -> Option<usize> {
    Some(match code {
        0x00..=0x0D | 0x14..=0x1E | 0x25 | 0x26 | 0x2A | 0x46..=0x6E | 0x76 | 0x7A | 0x82..=0x8B | 0x8E | 0x90..=0xA2 | 0xB3..=0xBA | 0xC3 | 0xD1..=0xDC | 0xDF | 0xE0 => 0,
        0x0E..=0x13 | 0x1F | 0x2B..=0x37 | 0xDE => 1,
        0x20 | 0x22 | 0x27..=0x29 | 0x38..=0x45 | 0x6F..=0x75 | 0x79 | 0x7B..=0x81 | 0x8C | 0x8D | 0x8F | 0xA3..=0xA5 | 0xC2 | 0xC6 | 0xD0 | 0xDD => 4,
        0x21 | 0x23 => 8,
        0xFE00..=0xFE05 | 0xFE0F | 0xFE11 | 0xFE13 | 0xFE14 | 0xFE17 | 0xFE18 | 0xFE1A | 0xFE1D | 0xFE1E => 0,
        0xFE12 | 0xFE19 => 1,
        0xFE09..=0xFE0E => 2,
        0xFE06 | 0xFE07 | 0xFE15 | 0xFE16 | 0xFE1C => 4,
        _ => return None,
    })
}

fn decode(il: &[u8]) -> Result<Vec<Op>, String> {
    let mut out = Vec::new();
    let mut at = 0;
    while at < il.len() {
        let start = at;
        let mut code = il[at] as u16;
        at += 1;
        if code == 0xFE {
            code = 0xFE00 | *il.get(at).ok_or("truncated IL")? as u16;
            at += 1;
        }
        let n = operand(code).ok_or_else(|| format!("IL opcode {code:#x} is not known"))?;
        let bytes = il.get(at..at + n).ok_or("truncated IL")?;
        at += n;
        let mut b = [0u8; 8];
        b[..n].copy_from_slice(bytes);
        let arg = match n {
            1 if code == 0x1F || (0x2B..=0x37).contains(&code) || code == 0xDE => b[0] as i8 as i64,
            1 => b[0] as i64,
            2 => u16::from_le_bytes([b[0], b[1]]) as i64,
            4 => i32::from_le_bytes(b[..4].try_into().unwrap()) as i64,
            _ => i64::from_le_bytes(b),
        };
        let mut raw = Vec::new();
        if code == 0x45 {
            // switch: the count, then that many 4-byte targets.
            for _ in 0..arg as u32 {
                raw.push(i32::from_le_bytes(il.get(at..at + 4).ok_or("truncated IL")?.try_into().unwrap()));
                at += 4;
            }
        }
        out.push(Op { code, arg, at: start, end: at, targets: raw.iter().map(|t| (at as i64 + *t as i64) as usize).collect() });
    }
    Ok(out)
}

/// A value on the evaluation stack of a constructor's field initializers.
#[derive(Clone, Debug)]
enum Val {
    This,
    Int(i64),
    Float(f64),
    Null,
    Str(String),
    /// default(T) of a struct (initobj).
    Zero,
    /// A static property or method's result (Vector3.zero, ...).
    Call(TypeSig, String),
    New(TypeSig, Vec<Val>),
    /// One of `this`'s own fields (its serialized value).
    Field(String),
    Local(i64),
    Unknown,
}

/// What a constructor stores in each of `this`'s fields, last store winning. Instructions this
/// does not model leave the stack unknown until the statement ends.
fn initializers(asm: &Assembly, il: &[u8]) -> Result<Vec<(String, Val)>, String> {
    let mut stack: Vec<Val> = Vec::new();
    let mut locals: std::collections::HashMap<i64, Val> = Default::default();
    let mut out: Vec<(String, Val)> = Vec::new();
    let pop = |s: &mut Vec<Val>| s.pop().unwrap_or(Val::Unknown);
    for op in decode(il)? {
        let token = op.arg as u32;
        match op.code {
            0x00 => {}
            0x02 => stack.push(Val::This),
            0x06..=0x09 => stack.push(locals.get(&(op.code as i64 - 6)).cloned().unwrap_or(Val::Unknown)),
            0x11 | 0xFE0C => stack.push(locals.get(&op.arg).cloned().unwrap_or(Val::Unknown)),
            0x0A..=0x0D => {
                let v = pop(&mut stack);
                locals.insert(op.code as i64 - 0x0A, v);
            }
            0x13 | 0xFE0E => {
                let v = pop(&mut stack);
                locals.insert(op.arg, v);
            }
            0x12 | 0xFE0D => stack.push(Val::Local(op.arg)),
            0xFE15 => {
                if let Val::Local(n) = pop(&mut stack) {
                    locals.insert(n, Val::Zero);
                }
            }
            0x14 => stack.push(Val::Null),
            0x15..=0x1E => stack.push(Val::Int(op.code as i64 - 0x16)),
            0x1F | 0x20 | 0x21 => stack.push(Val::Int(op.arg)),
            0x22 => stack.push(Val::Float(f32::from_bits(op.arg as u32) as f64)),
            0x23 => stack.push(Val::Float(f64::from_bits(op.arg as u64))),
            0x6B | 0x6C | 0x76 => match pop(&mut stack) {
                Val::Int(i) => stack.push(Val::Float(i as f64)),
                v => stack.push(v),
            },
            0x67..=0x6A | 0x6D | 0x6E | 0xD1..=0xD4 => {}
            0x72 => stack.push(asm.user_string(token).map(Val::Str).unwrap_or(Val::Unknown)),
            0x25 => {
                let v = stack.last().cloned().unwrap_or(Val::Unknown);
                stack.push(v);
            }
            0x26 => {
                pop(&mut stack);
            }
            0x73 => match asm.member(token) {
                Some(Member::Method { owner, params, .. }) => {
                    let at = stack.len().saturating_sub(params.len());
                    let args = stack.split_off(at);
                    stack.push(Val::New(owner, args));
                }
                _ => stack.clear(),
            },
            0x7E => match asm.member(token) {
                Some(Member::Field { owner: TypeSig::Class(t), name }) if t.full() == "System.String" && name == "Empty" => stack.push(Val::Str(String::new())),
                _ => stack.push(Val::Unknown),
            },
            // An implicit conversion (string to FixedString, ...) keeps the constant.
            0x28 if matches!(asm.member(token), Some(Member::Method { ref name, instance: false, ref params, .. }) if name == "op_Implicit" && params.len() == 1) => {
                let v = pop(&mut stack);
                stack.push(if matches!(v, Val::Str(_) | Val::Int(_) | Val::Float(_)) { v } else { Val::Unknown });
            }
            0x28 | 0x6F => match asm.member(token) {
                Some(Member::Method { owner, name, params, instance, ret, .. }) => {
                    let at = stack.len().saturating_sub(params.len() + instance as usize);
                    stack.truncate(at);
                    if ret != TypeSig::Void {
                        stack.push(if instance { Val::Unknown } else { Val::Call(owner, name) });
                    }
                }
                _ => stack.clear(),
            },
            0x7B => match (pop(&mut stack), asm.member(token)) {
                (Val::This, Some(Member::Field { name, .. })) => stack.push(Val::Field(name)),
                _ => stack.push(Val::Unknown),
            },
            0x7D => {
                let v = pop(&mut stack);
                if let (Val::This, Some(Member::Field { name, .. })) = (pop(&mut stack), asm.member(token)) {
                    out.retain(|(n, _)| *n != name);
                    out.push((name, v));
                }
            }
            0x2A => break,
            _ => stack.clear(),
        }
    }
    Ok(out)
}

/// ECMA-335 III opcode names.
fn mnemonic(code: u16) -> &'static str {
    const ONE: [&str; 0xE1] = [
        "nop", "break", "ldarg.0", "ldarg.1", "ldarg.2", "ldarg.3", "ldloc.0", "ldloc.1", "ldloc.2", "ldloc.3", "stloc.0", "stloc.1", "stloc.2", "stloc.3", "ldarg.s", "ldarga.s",
        "starg.s", "ldloc.s", "ldloca.s", "stloc.s", "ldnull", "ldc.i4.m1", "ldc.i4.0", "ldc.i4.1", "ldc.i4.2", "ldc.i4.3", "ldc.i4.4", "ldc.i4.5", "ldc.i4.6", "ldc.i4.7", "ldc.i4.8", "ldc.i4.s",
        "ldc.i4", "ldc.i8", "ldc.r4", "ldc.r8", "?", "dup", "pop", "jmp", "call", "calli", "ret", "br.s", "brfalse.s", "brtrue.s", "beq.s", "bge.s",
        "bgt.s", "ble.s", "blt.s", "bne.un.s", "bge.un.s", "bgt.un.s", "ble.un.s", "blt.un.s", "br", "brfalse", "brtrue", "beq", "bge", "bgt", "ble", "blt",
        "bne.un", "bge.un", "bgt.un", "ble.un", "blt.un", "switch", "ldind.i1", "ldind.u1", "ldind.i2", "ldind.u2", "ldind.i4", "ldind.u4", "ldind.i8", "ldind.i", "ldind.r4", "ldind.r8",
        "ldind.ref", "stind.ref", "stind.i1", "stind.i2", "stind.i4", "stind.i8", "stind.r4", "stind.r8", "add", "sub", "mul", "div", "div.un", "rem", "rem.un", "and",
        "or", "xor", "shl", "shr", "shr.un", "neg", "not", "conv.i1", "conv.i2", "conv.i4", "conv.i8", "conv.r4", "conv.r8", "conv.u4", "conv.u8", "callvirt",
        "cpobj", "ldobj", "ldstr", "newobj", "castclass", "isinst", "conv.r.un", "?", "?", "unbox", "throw", "ldfld", "ldflda", "stfld", "ldsfld", "ldsflda",
        "stsfld", "stobj", "conv.ovf.i1.un", "conv.ovf.i2.un", "conv.ovf.i4.un", "conv.ovf.i8.un", "conv.ovf.u1.un", "conv.ovf.u2.un", "conv.ovf.u4.un", "conv.ovf.u8.un", "conv.ovf.i.un", "conv.ovf.u.un", "box", "newarr", "ldlen", "ldelema",
        "ldelem.i1", "ldelem.u1", "ldelem.i2", "ldelem.u2", "ldelem.i4", "ldelem.u4", "ldelem.i8", "ldelem.i", "ldelem.r4", "ldelem.r8", "ldelem.ref", "stelem.i", "stelem.i1", "stelem.i2", "stelem.i4", "stelem.i8",
        "stelem.r4", "stelem.r8", "stelem.ref", "ldelem", "stelem", "unbox.any", "?", "?", "?", "?", "?", "?", "?", "?", "?", "?",
        "?", "?", "?", "conv.ovf.i1", "conv.ovf.u1", "conv.ovf.i2", "conv.ovf.u2", "conv.ovf.i4", "conv.ovf.u4", "conv.ovf.i8", "conv.ovf.u8", "?", "?", "?", "?", "?",
        "?", "?", "refanyval", "ckfinite", "?", "?", "mkrefany", "?", "?", "?", "?", "?", "?", "?", "?", "?",
        "ldtoken", "conv.u2", "conv.u1", "conv.i", "conv.ovf.i", "conv.ovf.u", "add.ovf", "add.ovf.un", "mul.ovf", "mul.ovf.un", "sub.ovf", "sub.ovf.un", "endfinally", "leave", "leave.s", "stind.i",
        "conv.u",
    ];
    const TWO: [&str; 0x1F] = [
        "arglist", "ceq", "cgt", "cgt.un", "clt", "clt.un", "ldftn", "ldvirtftn", "?", "ldarg", "ldarga", "starg", "ldloc", "ldloca", "stloc", "localloc",
        "?", "endfilter", "unaligned.", "volatile.", "tail.", "initobj", "constrained.", "cpblk", "initblk", "no.", "rethrow", "?", "sizeof", "refanytype", "readonly.",
    ];
    match code {
        0..=0xE0 => ONE[code as usize],
        0xFE00..=0xFE1E => TWO[(code - 0xFE00) as usize],
        _ => "?",
    }
}

/// A type without its assembly: Namespace.Name, generic arguments in <>.
pub fn type_text(sig: &TypeSig) -> String {
    match sig {
        TypeSig::Class(t) | TypeSig::ValueType(t) => t.full(),
        TypeSig::GenericInst { ty, args, .. } => format!("{}<{}>", ty.full(), args.iter().map(type_text).collect::<Vec<_>>().join(",")),
        TypeSig::SzArray(e) => format!("{}[]", type_text(e)),
        TypeSig::Var(n) => format!("!{n}"),
        TypeSig::MVar(n) => format!("!!{n}"),
        TypeSig::ByRef(e) => format!("{}&", type_text(e)),
        TypeSig::Array(e, r) => format!("{}[{}]", type_text(e), ",".repeat(*r as usize - 1)),
        TypeSig::Ptr(e) => format!("{}*", type_text(e)),
        other => format!("{other:?}"),
    }
}

/// Debug: an IL body, one instruction a line: offset, opcode name, operand (branch targets as
/// offsets, members and strings by name).
pub fn disassemble(asm: &Assembly, il: &[u8]) -> Vec<String> {
    let ops = decode(il).unwrap_or_else(|e| {
        eprintln!("{e}");
        Vec::new()
    });
    ops.iter()
        .map(|op| {
            let operand = match op.code {
                0x2B..=0x37 | 0x38..=0x44 | 0xDD | 0xDE => format!("IL_{:04x}", (op.end as i64 + op.arg) as usize),
                0x45 => op.targets.iter().map(|t| format!("IL_{t:04x}")).collect::<Vec<_>>().join(","),
                0x22 => format!("{}", f32::from_bits(op.arg as u32)),
                0x23 => format!("{}", f64::from_bits(op.arg as u64)),
                0x72 => format!("{:?}", asm.user_string(op.arg as u32).unwrap_or_default()),
                _ if op.end - op.at > 1 + (op.code > 0xFF) as usize => match asm.member(op.arg as u32) {
                    Some(Member::Method { owner, name, params, generic, .. }) => {
                        let g = if generic.is_empty() { String::new() } else { format!("<{}>", generic.iter().map(type_text).collect::<Vec<_>>().join(",")) };
                        format!("{}::{name}{g}({})", type_text(&owner), params.iter().map(type_text).collect::<Vec<_>>().join(","))
                    }
                    Some(Member::Field { owner, name }) => format!("{}::{name}", type_text(&owner)),
                    None => asm.type_token(op.arg as u32).map(|t| type_text(&t)).unwrap_or_else(|| format!("{:#x}", op.arg)),
                },
                _ => String::new(),
            };
            format!("IL_{:04x}: {} {operand}", op.at, mnemonic(op.code))
        })
        .collect()
}

/// The methods an IL body calls, creates or takes the address of (call, callvirt, newobj,
/// ldftn, ldvirtftn), with the opcode.
pub fn calls(asm: &Assembly, il: &[u8]) -> Vec<(u16, Member)> {
    decode(il).unwrap_or_default().iter().filter(|op| matches!(op.code, 0x28 | 0x6F | 0x73 | 0xFE06 | 0xFE07)).filter_map(|op| Some((op.code, asm.member(op.arg as u32)?))).collect()
}

/// The fields a class's constructor or Awake sets to a new, empty collection (List, Dictionary,
/// HashSet, ...): what they hold when nothing has been added yet.
pub fn created_collections(asm: &Assemblies, class: &TypeName) -> Vec<String> {
    let (Some(def), Some(a)) = (asm.def(class), asm.assembly_of(class)) else { return Vec::new() };
    let mut out = Vec::new();
    for m in def.methods.iter().filter(|m| (m.name == ".ctor" || m.name == "Awake") && m.params.is_empty()) {
        for (name, v) in initializers(a, a.il(m).unwrap_or_default()).unwrap_or_default() {
            if let Val::New(TypeSig::GenericInst { ty, .. }, args) = v {
                if ty.namespace == "System.Collections.Generic" && args.iter().all(|a| matches!(a, Val::Int(_))) && !out.contains(&name) {
                    out.push(name);
                }
            }
        }
    }
    out
}

/// The bool constant passed to the first call of `method` in an IL body (ldc.i4.0/1 right
/// before it).
pub fn bool_argument(asm: &Assembly, il: &[u8], method: &str) -> Option<bool> {
    let ops = decode(il).ok()?;
    let at = ops.iter().position(|op| matches!(op.code, 0x28 | 0x6F) && matches!(asm.member(op.arg as u32), Some(Member::Method { ref name, .. }) if name == method))?;
    match ops.get(at.checked_sub(1)?)?.code {
        0x16 => Some(false),
        0x17 => Some(true),
        _ => None,
    }
}

/// The first string literal an IL body loads (ldstr).
pub fn first_string(asm: &Assembly, il: &[u8]) -> Option<String> {
    decode(il).ok()?.iter().find(|op| op.code == 0x72).and_then(|op| asm.user_string(op.arg as u32))
}

/// The first int32 constant an IL body loads (ldc.i4.*).
pub fn first_int(il: &[u8]) -> Option<i64> {
    decode(il).ok()?.iter().find_map(|op| match op.code {
        0x15..=0x1E => Some(op.code as i64 - 0x16),
        0x1F | 0x20 => Some(op.arg),
        _ => None,
    })
}

fn short(t: &TypeName) -> &str {
    let n = t.name.rsplit('/').next().unwrap_or(&t.name);
    n.split('`').next().unwrap_or(n)
}

/// The .NET name of a type argument (Type.Name): Int16, Boolean, Single, Vector3, ...
fn arg_name(sig: &TypeSig) -> String {
    use TypeSig::*;
    match sig {
        Bool => "Boolean".into(),
        Char => "Char".into(),
        I1 => "SByte".into(),
        U1 => "Byte".into(),
        I2 => "Int16".into(),
        U2 => "UInt16".into(),
        I4 => "Int32".into(),
        U4 => "UInt32".into(),
        I8 => "Int64".into(),
        U8 => "UInt64".into(),
        R4 => "Single".into(),
        R8 => "Double".into(),
        String => "String".into(),
        Class(t) | ValueType(t) | GenericInst { ty: t, .. } => short(t).into(),
        other => format!("{other:?}"),
    }
}

fn packed_u64(v: u64) -> Vec<u8> {
    crate::wire::Writer::new().packed_u64(v).0.clone()
}

/// A value of `ty` as NetworkVariableSerialization writes it; None when the type has a
/// serializer of its own.
fn encode(asm: &Assemblies, ty: &TypeSig, v: &Val) -> Result<Option<Vec<u8>>, String> {
    use crate::wire::Writer;
    use TypeSig::*;
    let int = || match v {
        Val::Int(i) => Ok(*i),
        Val::Zero => Ok(0),
        other => Err(format!("{other:?} for {ty:?}")),
    };
    let float = || match v {
        Val::Float(f) => Ok(*f),
        Val::Int(i) => Ok(*i as f64),
        Val::Zero => Ok(0.0),
        other => Err(format!("{other:?} for {ty:?}")),
    };
    Ok(Some(match ty {
        Bool | U1 | I1 => vec![int()? as u8],
        I2 => Writer::new().packed_i16(int()? as i16).0.clone(),
        U2 => Writer::new().packed_u16(int()? as u16).0.clone(),
        I4 => Writer::new().packed_i32(int()? as i32).0.clone(),
        U4 => Writer::new().packed_u32(int()? as u32).0.clone(),
        I8 => {
            let i = int()?;
            packed_u64(((i << 1) ^ (i >> 63)) as u64)
        }
        U8 => packed_u64(int()? as u64),
        R4 => (float()? as f32).to_le_bytes().to_vec(),
        R8 => float()?.to_le_bytes().to_vec(),
        ValueType(t) if t.namespace == "UnityEngine" && matches!(t.name.as_str(), "Vector2" | "Vector3" | "Vector4" | "Quaternion" | "Color") => {
            let n = match t.name.as_str() {
                "Vector2" => 2,
                "Vector3" => 3,
                _ => 4,
            };
            let values: Vec<f64> = match v {
                Val::Zero => vec![0.0; n],
                Val::Call(_, m) if m == "get_zero" => vec![0.0; n],
                Val::Call(_, m) if m == "get_one" => vec![1.0; n],
                Val::Call(_, m) if m == "get_identity" => vec![0.0, 0.0, 0.0, 1.0],
                Val::New(_, args) if args.len() == n => args.iter().map(|a| match a {
                    Val::Float(f) => Ok(*f),
                    Val::Int(i) => Ok(*i as f64),
                    other => Err(format!("{other:?} in a {}", t.name)),
                }).collect::<Result<_, _>>()?,
                other => return Err(format!("{other:?} for a {}", t.name)),
            };
            values.iter().flat_map(|f| (*f as f32).to_le_bytes()).collect()
        }
        ValueType(t) if t.namespace == "Unity.Collections" && t.name.starts_with("FixedString") => {
            let s = match v {
                Val::Zero => std::string::String::new(),
                Val::Str(s) => s.clone(),
                Val::New(_, args) => match args.as_slice() {
                    [Val::Str(s)] => s.clone(),
                    other => return Err(format!("{other:?} for a {}", t.name)),
                },
                other => return Err(format!("{other:?} for a {}", t.name)),
            };
            let capacity: usize = t.name.trim_start_matches("FixedString").trim_end_matches("Bytes").parse().map_err(|_| t.name.clone())?;
            crate::net::enc::fixed_string(&s, capacity)
        }
        ValueType(t) => match asm.enum_base(t) {
            Some(base) => {
                let i = int()?;
                let size = match base {
                    U1 | I1 => 1,
                    U2 | I2 => 2,
                    U8 | I8 => 8,
                    _ => 4,
                };
                i.to_le_bytes()[..size].to_vec()
            }
            None => return Ok(None),
        },
        _ => return Ok(None),
    }))
}

/// The class's Netcode variables: [{name, type, bytes}] with bytes null when the server sets
/// the value. `fields`: the component's serialized fields, for values taken from them.
pub fn variables(asm: &Assemblies, class: &TypeName, fields: &[(String, Raw)]) -> Result<Vec<Value>, String> {
    let mut out = Vec::new();
    collect(asm, class, fields, &mut out, 0)?;
    Ok(out)
}

/// A serialized field's value as a constant.
fn serialized(fields: &[(String, Raw)], name: &str) -> Result<Val, String> {
    Ok(match super::netdata::field(fields, name) {
        Some(Raw::Bool(b)) => Val::Int(*b as i64),
        Some(Raw::Int(i) | Raw::Enum(i, ..)) => Val::Int(*i),
        Some(Raw::UInt(u)) => Val::Int(*u as i64),
        Some(Raw::Float(f)) => Val::Float(*f),
        Some(Raw::Single(f)) => Val::Float(*f as f64),
        Some(Raw::Str(s)) => Val::Str(s.clone()),
        other => return Err(format!("from field {name} ({other:?})")),
    })
}

fn collect(asm: &Assemblies, class: &TypeName, fields: &[(String, Raw)], out: &mut Vec<Value>, depth: usize) -> Result<(), String> {
    if depth > 16 || class.full() == "Unity.Netcode.NetworkBehaviour" {
        return Ok(());
    }
    let def = asm.def(class).ok_or_else(|| format!("{} is not in the game's assemblies", class.full()))?;
    let a = asm.assembly_of(class).unwrap();
    let Some(init) = def.methods.iter().find(|m| m.name == "__initializeVariables") else {
        // No variables of its own: whatever its base registers.
        return match asm.base(class) {
            Some(b) => collect(asm, &b, fields, out, depth + 1),
            None => Ok(()),
        };
    };
    // Field initializers run in the constructor; a variable it leaves out is created in Awake
    // (before Netcode registers it at spawn).
    let mut ctor_values = Vec::new();
    for m in def.methods.iter().filter(|m| (m.name == ".ctor" || m.name == "Awake") && m.params.is_empty()) {
        for (name, v) in initializers(a, a.il(m).unwrap_or_default())? {
            if !ctor_values.iter().any(|(n, _)| *n == name) {
                ctor_values.push((name, v));
            }
        }
    }
    // __initializeVariables: for each variable, ldfld it ... NetworkVariableFields.Add; the
    // base's call where it registers the base's variables.
    let mut last_field = None;
    for op in decode(a.il(init).unwrap_or_default())? {
        match (op.code, a.member(op.arg as u32)) {
            (0x7B, Some(Member::Field { name, .. })) => last_field = Some(name),
            (0x6F, Some(Member::Method { name, .. })) if name == "Add" => {
                let Some(name) = last_field.take() else { continue };
                let field = def.fields.iter().find(|f| f.name == name).ok_or_else(|| format!("{}.{name}: not a field", class.name))?;
                let value = ctor_values.iter().find(|(n, _)| *n == name).map(|(_, v)| v);
                out.push(variable(asm, class, &name, &field.sig, value, fields)?);
            }
            (0x28, Some(Member::Method { owner, name, .. })) if name == "__initializeVariables" => {
                if let TypeSig::Class(b) | TypeSig::GenericInst { ty: b, .. } = owner {
                    collect(asm, &b, fields, out, depth + 1)?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn variable(asm: &Assemblies, class: &TypeName, name: &str, sig: &TypeSig, value: Option<&Val>, fields: &[(String, Raw)]) -> Result<Value, String> {
    let at = || format!("{}.{name}", class.name);
    let (ty, bytes) = match sig {
        TypeSig::GenericInst { ty, args, .. } if ty.full() == "Unity.Netcode.NetworkVariable`1" && args.len() == 1 => {
            let initial = match value {
                Some(Val::New(_, args)) => args.first().cloned().unwrap_or(Val::Zero),
                other => return Err(format!("{}: the constructor does not create it ({other:?})", at())),
            };
            let initial = match initial {
                Val::Field(f) => serialized(fields, &f).map_err(|e| format!("{}: {e}", at()))?,
                v => v,
            };
            (format!("NetworkVariable<{}>", arg_name(&args[0])), encode(asm, &args[0], &initial).map_err(|e| format!("{}: {e}", at()))?)
        }
        TypeSig::GenericInst { ty, args, .. } if ty.full() == "Unity.Netcode.NetworkList`1" && args.len() == 1 => {
            match value {
                Some(Val::New(_, a)) if matches!(a.first(), None | Some(Val::Null)) => {}
                other => return Err(format!("{}: a list that does not start empty ({other:?})", at())),
            }
            (format!("NetworkList<{}>", arg_name(&args[0])), Some(vec![0, 0]))
        }
        TypeSig::Class(t) if asm.derives_from(t, "Unity.Netcode.NetworkVariableBase") => (short(t).to_string(), None),
        other => return Err(format!("{}: {other:?} is not a Netcode variable", at())),
    };
    let hex = bytes.map(|b| b.iter().map(|x| format!("{x:02x}")).collect::<String>());
    Ok(json!({"name": name, "type": ty, "bytes": hex}))
}
