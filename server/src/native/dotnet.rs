//! .NET assemblies (ECMA-335 Partition II metadata) read without a runtime: types in TypeDef
//! order with their fields, methods, custom attributes and IL, so the game's serialization
//! layouts and Netcode RPC hashes come from its DLLs instead of from running it.

#![allow(dead_code)] // the table reader uses it; until that lands, only the tests do

use std::cell::Cell;
use std::io;
use std::ops::Range;
use std::path::{Path, PathBuf};

/// One assembly: its types in TypeDef table order, and the image for IL.
pub struct Assembly {
    pub name: String,
    types: Vec<TypeDef>,
    img: Image,
    /// Field and MethodDef row (minus 1) → (type index, position in its list).
    field_at: Vec<Option<(usize, usize)>>,
    method_at: Vec<Option<(usize, usize)>>,
    /// An attribute argument had another assembly's enum, read as int32.
    guessed_enums: bool,
}

/// A TypeDef row. A nested type is named "Outer/Inner", in its outermost type's namespace.
#[derive(Debug)]
pub struct TypeDef {
    pub namespace: String,
    pub name: String,
    /// TypeAttributes (0x2000 Serializable, 0x20 Interface, 0x100 Sealed, 0x80 Abstract, ...).
    pub flags: u32,
    /// NestedClass: the enclosing type's index.
    pub enclosing: Option<usize>,
    pub extends: Option<TypeSig>,
    pub interfaces: Vec<TypeSig>,
    pub generic_params: Vec<String>,
    /// In declaration order (metadata row order).
    pub fields: Vec<FieldDef>,
    pub methods: Vec<MethodDef>,
    pub attributes: Vec<Attr>,
    /// ClassLayout: packing size, class size.
    pub class_layout: Option<(u16, u32)>,
}

#[derive(Debug)]
pub struct FieldDef {
    pub name: String,
    /// FieldAttributes (access in 0x7: 1 private, 6 public; 0x10 static, 0x80 NotSerialized).
    pub flags: u16,
    pub sig: TypeSig,
    pub attributes: Vec<Attr>,
    /// FieldLayout: the explicit offset.
    pub offset: Option<u32>,
    /// Constant: a literal's value (enum members), when an integer, bool or char.
    pub constant: Option<i64>,
}

#[derive(Debug)]
pub struct MethodDef {
    pub name: String,
    /// MethodAttributes (0x10 static, 0x40 virtual, 0x800 SpecialName, ...).
    pub flags: u16,
    /// The body's address, 0 without one.
    pub rva: u32,
    pub ret: TypeSig,
    /// Names from the Param table ("" without a row), types from the signature.
    pub params: Vec<(String, TypeSig)>,
    pub attributes: Vec<Attr>,
}

/// What an IL metadata token names.
#[derive(Debug, Clone, PartialEq)]
pub enum Member {
    Field { owner: TypeSig, name: String },
    /// `instance`: takes `this` (HASTHIS); `ret`: its return type; `generic`: a generic
    /// method's type arguments (MethodSpec).
    Method { owner: TypeSig, name: String, params: Vec<TypeSig>, instance: bool, ret: TypeSig, generic: Vec<TypeSig> },
}

/// A custom attribute: its type and decoded constructor and named arguments.
#[derive(Debug, Clone, PartialEq)]
pub struct Attr {
    pub ty: TypeName,
    pub fixed: Vec<AttrArg>,
    pub named: Vec<(String, AttrArg)>,
}

/// An attribute argument. Enums are Int or UInt (their underlying type), a char is UInt, a
/// null array is empty, and one the reader cannot decode is Other and ends the list.
#[derive(Debug, Clone, PartialEq)]
pub enum AttrArg {
    Bool(bool),
    Int(i64),
    UInt(u64),
    Float(f64),
    String(Option<String>),
    /// System.Type, by its assembly-qualified name.
    Type(Option<String>),
    Array(Vec<AttrArg>),
    Other,
}

/// A type by name. scope: the AssemblyRef a TypeRef points into; None for one defined here.
#[derive(Debug, Clone, PartialEq)]
pub struct TypeName {
    pub scope: Option<String>,
    pub namespace: String,
    /// Nested types: "Outer/Inner".
    pub name: String,
}

/// A type in a signature (II.23.2.12). Unknown: an element type byte the reader does not know.
#[derive(Debug, Clone, PartialEq)]
pub enum TypeSig {
    Void,
    Bool,
    Char,
    I1,
    U1,
    I2,
    U2,
    I4,
    U4,
    I8,
    U8,
    R4,
    R8,
    String,
    Object,
    IntPtr,
    UIntPtr,
    TypedRef,
    Class(TypeName),
    ValueType(TypeName),
    SzArray(Box<TypeSig>),
    /// Element type, rank.
    Array(Box<TypeSig>, u32),
    GenericInst { ty: TypeName, value_type: bool, args: Vec<TypeSig> },
    Var(u32),
    MVar(u32),
    Ptr(Box<TypeSig>),
    ByRef(Box<TypeSig>),
    FnPtr,
    Unknown(u8),
}

/// Several assemblies together: resolve names across them.
pub struct Assemblies(pub Vec<Assembly>);

impl Assembly {
    /// Reads the assembly at `path`. Another assembly's enum in an attribute argument is read as
    /// int32; Assemblies::load_dir reads those with their real underlying type.
    pub fn load(path: &Path) -> io::Result<Assembly> {
        let data = std::fs::read(path)?;
        Assembly::parse(data, &|_| None)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, format!("{}: not a .NET assembly", path.display())))
    }

    /// In TypeDef table order: index 0 is <Module>.
    pub fn types(&self) -> &[TypeDef] {
        &self.types
    }

    /// A type's index by namespace and name (nested types: "Outer/Inner").
    pub fn find(&self, namespace: &str, name: &str) -> Option<usize> {
        // ponytail: linear scan; index by name if resolve gets hot
        self.types.iter().position(|t| t.namespace == namespace && t.name == name)
    }

    /// A method body's IL code (II.25.4.2 tiny or II.25.4.3 fat header); None without a body.
    pub fn il(&self, method: &MethodDef) -> Option<&[u8]> {
        let d = &self.img.data;
        let at = self.img.offset(method.rva)?;
        let head = Reader::at(d, at).u16()?;
        let (code, len) = match head & 3 {
            2 => (at + 1, usize::from(head as u8 >> 2)),
            3 => (at + 4 * usize::from(head >> 12), Reader::at(d, at + 4).u32()? as usize),
            _ => return None,
        };
        d.get(code..code.checked_add(len)?)
    }

    /// The field or method an IL operand token (FieldDef, MethodDef, MemberRef, MethodSpec) names.
    pub fn member(&self, token: u32) -> Option<Member> {
        let (table, row) = ((token >> 24) as usize, token & 0xFF_FFFF);
        let img = &self.img;
        let type_of = |t: usize| self.types.get(t).map(|d| TypeSig::Class(TypeName { scope: Some(self.name.clone()), namespace: d.namespace.clone(), name: d.name.clone() }));
        match table {
            FIELD => {
                let (t, i) = (*self.field_at.get((row as usize).checked_sub(1)?)?)?;
                Some(Member::Field { owner: type_of(t)?, name: self.types[t].fields[i].name.clone() })
            }
            METHOD => {
                let (t, i) = (*self.method_at.get((row as usize).checked_sub(1)?)?)?;
                let m = &self.types[t].methods[i];
                Some(Member::Method { owner: type_of(t)?, name: m.name.clone(), params: m.params.iter().map(|p| p.1.clone()).collect(), instance: m.flags & 0x10 == 0, ret: m.ret.clone(), generic: Vec::new() })
            }
            MEMBER_REF => {
                let owner = self.type_sig(img.refer(MEMBER_REF, row, 0))?;
                let name = img.string(img.cell(MEMBER_REF, row, 1));
                let sig = img.blob(img.cell(MEMBER_REF, row, 2));
                if sig.first() == Some(&0x06) {
                    return Some(Member::Field { owner, name });
                }
                let (ret, params) = self.method_sig(&mut Reader::new(sig))?;
                Some(Member::Method { owner, name, params, instance: sig.first()? & 0x20 != 0, ret, generic: Vec::new() })
            }
            METHOD_SPEC => {
                let (t, r) = img.refer(METHOD_SPEC, row, 0);
                let mut m = self.member(((t as u32) << 24) | r)?;
                // MethodSpec instantiation (II.23.2.15): 0x0A, the count, the type arguments.
                let mut b = Reader::new(img.blob(img.cell(METHOD_SPEC, row, 1)));
                if b.u8() == Some(0x0A) {
                    let args = (0..b.compressed()?).map(|_| self.sig_type(&mut b)).collect::<Option<Vec<_>>>()?;
                    if let Member::Method { generic, .. } = &mut m {
                        *generic = args;
                    }
                }
                Some(m)
            }
            _ => None,
        }
    }

    /// A type token's type (TypeDef, TypeRef, TypeSpec): newarr, box, castclass, ldtoken, ...
    pub fn type_token(&self, token: u32) -> Option<TypeSig> {
        let table = (token >> 24) as usize;
        matches!(table, TYPE_DEF | TYPE_REF | TYPE_SPEC).then(|| self.type_sig((table, token & 0xFF_FFFF))).flatten()
    }

    /// A method body's local variable types (the fat header's LocalVarSig, II.23.2.6).
    pub fn locals(&self, method: &MethodDef) -> Vec<TypeSig> {
        let d = &self.img.data;
        let Some(at) = self.img.offset(method.rva) else { return Vec::new() };
        if Reader::at(d, at).u16().is_none_or(|h| h & 3 != 3) {
            return Vec::new();
        }
        let Some(token) = Reader::at(d, at + 8).u32().filter(|t| *t >> 24 == STANDALONE_SIG as u32) else { return Vec::new() };
        let mut r = Reader::new(self.img.blob(self.img.cell(STANDALONE_SIG, token & 0xFF_FFFF, 0)));
        if r.u8() != Some(0x07) {
            return Vec::new();
        }
        let n = r.compressed().unwrap_or(0);
        (0..n).map_while(|_| {
            // Constraint (pinned) and byref markers.
            while matches!(r.peek(), Some(0x45)) {
                r.u8();
            }
            self.sig_type(&mut r)
        }).collect()
    }

    /// A static field's initial data (FieldRVA: the bytes array initializers copy), `len` bytes.
    pub fn field_data(&self, namespace: &str, class: &str, field: &str, len: usize) -> Option<&[u8]> {
        let t = self.find(namespace, class)?;
        let i = self.types[t].fields.iter().position(|f| f.name == field)?;
        let row = self.field_at.iter().position(|x| *x == Some((t, i)))? as u32 + 1;
        let r = (1..=self.img.rows(FIELD_RVA)).find(|&r| self.img.cell(FIELD_RVA, r, 1) == row)?;
        let at = self.img.offset(self.img.cell(FIELD_RVA, r, 0))?;
        self.img.data.get(at..at + len)
    }

    /// An ldstr operand's literal.
    pub fn user_string(&self, token: u32) -> Option<String> {
        (token >> 24 == 0x70).then(|| self.img.user_string(token & 0xFF_FFFF)).flatten()
    }

    /// `foreign`: the underlying type of an enum another assembly defines, if known.
    fn parse(data: Vec<u8>, foreign: &dyn Fn(&TypeName) -> Option<TypeSig>) -> Option<Assembly> {
        let img = Image::parse(data)?;
        let name = match img.rows(ASSEMBLY) {
            0 => img.string(img.cell(MODULE, 1, 1)),
            _ => img.string(img.cell(ASSEMBLY, 1, 7)),
        };
        let n = img.rows(TYPE_DEF) as usize;
        let mut enclosing = vec![None; n];
        for r in 1..=img.rows(NESTED_CLASS) {
            let (inner, outer) = (img.cell(NESTED_CLASS, r, 0) as usize, img.cell(NESTED_CLASS, r, 1) as usize);
            if (1..=n).contains(&inner) && (1..=n).contains(&outer) {
                enclosing[inner - 1] = Some(outer - 1);
            }
        }
        let raw = |i: usize| (img.string(img.cell(TYPE_DEF, i as u32 + 1, 2)), img.string(img.cell(TYPE_DEF, i as u32 + 1, 1)));
        let types = (0..n)
            .map(|i| {
                let (mut namespace, mut name) = raw(i);
                let mut at = i;
                // Bounded: a corrupt NestedClass table can loop.
                for _ in 0..64 {
                    let Some(outer) = enclosing[at] else { break };
                    let (ns, outer_name) = raw(outer);
                    (namespace, name, at) = (ns, format!("{outer_name}/{name}"), outer);
                }
                TypeDef {
                    namespace,
                    name,
                    flags: img.cell(TYPE_DEF, i as u32 + 1, 0),
                    enclosing: enclosing[i],
                    extends: None,
                    interfaces: Vec::new(),
                    generic_params: Vec::new(),
                    fields: Vec::new(),
                    methods: Vec::new(),
                    attributes: Vec::new(),
                    class_layout: None,
                }
            })
            .collect();
        let mut asm = Assembly { name, types, img, field_at: Vec::new(), method_at: Vec::new(), guessed_enums: false };
        asm.read_members();
        let (attrs, guessed) = asm.read_attributes(foreign);
        asm.set_attributes(attrs);
        asm.guessed_enums = guessed;
        Some(asm)
    }

    /// Base types, fields, methods, interfaces, generic parameters and layouts.
    fn read_members(&mut self) {
        self.field_at = vec![None; self.img.rows(FIELD) as usize];
        self.method_at = vec![None; self.img.rows(METHOD) as usize];
        for i in 0..self.types.len() {
            let (extends, fields, methods) = self.members(i as u32 + 1);
            let t = &mut self.types[i];
            t.extends = extends;
            for (k, (row, f)) in fields.into_iter().enumerate() {
                if let Some(at) = slot(&mut self.field_at, row) {
                    *at = Some((i, k));
                }
                t.fields.push(f);
            }
            for (k, (row, m)) in methods.into_iter().enumerate() {
                if let Some(at) = slot(&mut self.method_at, row) {
                    *at = Some((i, k));
                }
                t.methods.push(m);
            }
        }
        for r in 1..=self.img.rows(INTERFACE_IMPL) {
            let sig = self.type_sig(self.img.refer(INTERFACE_IMPL, r, 1));
            if let (Some(t), Some(sig)) = (slot(&mut self.types, self.img.cell(INTERFACE_IMPL, r, 0)), sig) {
                t.interfaces.push(sig);
            }
        }
        for r in 1..=self.img.rows(GENERIC_PARAM) {
            let (owner, row) = self.img.refer(GENERIC_PARAM, r, 2);
            let number = self.img.cell(GENERIC_PARAM, r, 0) as usize;
            let name = self.img.string(self.img.cell(GENERIC_PARAM, r, 3));
            if owner == TYPE_DEF
                && let Some(t) = slot(&mut self.types, row)
            {
                let names = &mut t.generic_params;
                if names.len() <= number {
                    names.resize(number + 1, String::new());
                }
                names[number] = name;
            }
        }
        for r in 1..=self.img.rows(CLASS_LAYOUT) {
            let layout = (self.img.cell(CLASS_LAYOUT, r, 0) as u16, self.img.cell(CLASS_LAYOUT, r, 1));
            if let Some(t) = slot(&mut self.types, self.img.cell(CLASS_LAYOUT, r, 2)) {
                t.class_layout = Some(layout);
            }
        }
        for r in 1..=self.img.rows(CONSTANT) {
            let (table, row) = self.img.refer(CONSTANT, r, 1);
            let value = constant(self.img.cell(CONSTANT, r, 0) as u8, self.img.blob(self.img.cell(CONSTANT, r, 2)));
            if table == FIELD
                && let Some((t, k)) = slot(&mut self.field_at, row).and_then(|at| *at)
            {
                self.types[t].fields[k].constant = value;
            }
        }
        for r in 1..=self.img.rows(FIELD_LAYOUT) {
            if let Some((t, k)) = slot(&mut self.field_at, self.img.cell(FIELD_LAYOUT, r, 1)).and_then(|at| *at) {
                self.types[t].fields[k].offset = Some(self.img.cell(FIELD_LAYOUT, r, 0));
            }
        }
    }

    /// TypeDef row `row`'s base type, and its fields and methods with their rows.
    #[allow(clippy::type_complexity)]
    fn members(&self, row: u32) -> (Option<TypeSig>, Vec<(u32, FieldDef)>, Vec<(u32, MethodDef)>) {
        let img = &self.img;
        let fields = img.run(TYPE_DEF, row, 4, FIELD, FIELD_PTR).into_iter().map(|f| {
            let field = FieldDef {
                name: img.string(img.cell(FIELD, f, 1)),
                flags: img.cell(FIELD, f, 0) as u16,
                sig: self.field_sig(img.blob(img.cell(FIELD, f, 2))),
                attributes: Vec::new(),
                offset: None,
                constant: None,
            };
            (f, field)
        });
        let methods = img.run(TYPE_DEF, row, 5, METHOD, METHOD_PTR).into_iter().map(|m| (m, self.method(m)));
        (self.type_sig(img.refer(TYPE_DEF, row, 3)), fields.collect(), methods.collect())
    }

    fn method(&self, m: u32) -> MethodDef {
        let img = &self.img;
        let (ret, types) = self.method_sig(&mut Reader::new(img.blob(img.cell(METHOD, m, 4)))).unwrap_or((TypeSig::Unknown(0), Vec::new()));
        // Param rows by sequence number: 0 is the return value, 1.. the parameters.
        let names: Vec<(u32, String)> = img.run(METHOD, m, 5, PARAM, PARAM_PTR).into_iter().map(|p| (img.cell(PARAM, p, 1), img.string(img.cell(PARAM, p, 2)))).collect();
        let params = types.into_iter().zip(1..).map(|(t, seq)| (names.iter().find(|n| n.0 == seq).map(|n| n.1.clone()).unwrap_or_default(), t)).collect();
        MethodDef { name: img.string(img.cell(METHOD, m, 3)), flags: img.cell(METHOD, m, 2) as u16, rva: img.cell(METHOD, m, 0), ret, params, attributes: Vec::new() }
    }

    /// The attributes on types, fields and methods, with their parents (table, row), and whether
    /// an enum argument was guessed to be int32.
    #[allow(clippy::type_complexity)]
    fn read_attributes(&self, foreign: &dyn Fn(&TypeName) -> Option<TypeSig>) -> (Vec<((usize, u32), Attr)>, bool) {
        let img = &self.img;
        let guessed = Cell::new(false);
        let underlying = |n: &TypeName| {
            let local = n.scope.as_ref().is_none_or(|s| *s == self.name).then(|| self.find(&n.namespace, &n.name)).flatten();
            let found = match local {
                Some(t) => enum_base(&self.types[t]),
                None => foreign(n),
            };
            found.unwrap_or_else(|| {
                guessed.set(true);
                TypeSig::I4
            })
        };
        let attrs = (1..=img.rows(CUSTOM_ATTRIBUTE))
            .filter_map(|r| {
                let parent = img.refer(CUSTOM_ATTRIBUTE, r, 0);
                if ![TYPE_DEF, FIELD, METHOD].contains(&parent.0) {
                    return None;
                }
                let (ty, params) = self.ctor(img.refer(CUSTOM_ATTRIBUTE, r, 1))?;
                Some((parent, decode_attr(ty, &params, img.blob(img.cell(CUSTOM_ATTRIBUTE, r, 2)), &underlying)))
            })
            .collect();
        (attrs, guessed.get())
    }

    fn set_attributes(&mut self, attrs: Vec<((usize, u32), Attr)>) {
        for t in &mut self.types {
            t.attributes.clear();
            t.fields.iter_mut().for_each(|f| f.attributes.clear());
            t.methods.iter_mut().for_each(|m| m.attributes.clear());
        }
        for ((table, row), attr) in attrs {
            let list = match table {
                TYPE_DEF => slot(&mut self.types, row).map(|t| &mut t.attributes),
                FIELD => slot(&mut self.field_at, row).and_then(|at| *at).map(|(t, k)| &mut self.types[t].fields[k].attributes),
                _ => slot(&mut self.method_at, row).and_then(|at| *at).map(|(t, k)| &mut self.types[t].methods[k].attributes),
            };
            if let Some(list) = list {
                list.push(attr);
            }
        }
    }

    /// An attribute constructor (a MethodDef or MemberRef): its type and parameter types.
    fn ctor(&self, (table, row): (usize, u32)) -> Option<(TypeName, Vec<TypeSig>)> {
        let img = &self.img;
        let (ty, sig) = match table {
            METHOD => {
                let (owner, _) = (*self.method_at.get((row as usize).checked_sub(1)?)?)?;
                (self.name_of(TYPE_DEF, owner as u32 + 1)?, img.cell(METHOD, row, 4))
            }
            MEMBER_REF => match self.type_sig(img.refer(MEMBER_REF, row, 0))? {
                TypeSig::Class(ty) | TypeSig::GenericInst { ty, .. } => (ty, img.cell(MEMBER_REF, row, 2)),
                _ => return None,
            },
            _ => return None,
        };
        Some((ty, self.method_sig(&mut Reader::new(img.blob(sig)))?.1))
    }

    /// A TypeDef or TypeRef row's name.
    fn name_of(&self, table: usize, row: u32) -> Option<TypeName> {
        let img = &self.img;
        match table {
            TYPE_DEF => {
                let t = self.types.get((row as usize).checked_sub(1)?)?;
                Some(TypeName { scope: None, namespace: t.namespace.clone(), name: t.name.clone() })
            }
            TYPE_REF if (1..=img.rows(TYPE_REF)).contains(&row) => {
                let (mut namespace, mut name) = (img.string(img.cell(TYPE_REF, row, 2)), img.string(img.cell(TYPE_REF, row, 1)));
                let mut scope = img.refer(TYPE_REF, row, 0);
                // A nested type's scope is its enclosing TypeRef (bounded: corrupt ones loop).
                for _ in 0..64 {
                    if scope.0 != TYPE_REF {
                        break;
                    }
                    name = format!("{}/{name}", img.string(img.cell(TYPE_REF, scope.1, 1)));
                    namespace = img.string(img.cell(TYPE_REF, scope.1, 2));
                    scope = img.refer(TYPE_REF, scope.1, 0);
                }
                let scope = (scope.0 == ASSEMBLY_REF).then(|| img.string(img.cell(ASSEMBLY_REF, scope.1, 6)));
                Some(TypeName { scope, namespace, name })
            }
            _ => None,
        }
    }

    /// A TypeDefOrRef target: a TypeDef or TypeRef as a class, a TypeSpec decoded.
    fn type_sig(&self, (table, row): (usize, u32)) -> Option<TypeSig> {
        match table {
            TYPE_SPEC => self.sig_type(&mut Reader::new(self.img.blob(self.img.cell(TYPE_SPEC, row, 0)))),
            _ => self.name_of(table, row).map(TypeSig::Class),
        }
    }

    /// FieldSig (II.23.2.4); Unknown(0) when malformed.
    fn field_sig(&self, blob: &[u8]) -> TypeSig {
        let mut r = Reader::new(blob);
        (r.u8() == Some(0x06)).then(|| self.sig_type(&mut r)).flatten().unwrap_or(TypeSig::Unknown(0))
    }

    /// MethodDefSig or MethodRefSig (II.23.2.1-3): the return type and parameter types.
    fn method_sig(&self, r: &mut Reader) -> Option<(TypeSig, Vec<TypeSig>)> {
        if r.u8()? & 0x10 != 0 {
            r.compressed()?; // generic parameter count
        }
        let n = r.compressed()?;
        let ret = self.sig_type(r)?;
        Some((ret, (0..n).map(|_| self.sig_type(r)).collect::<Option<_>>()?))
    }

    /// One type in a signature (II.23.2.12), custom modifiers skipped.
    fn sig_type(&self, r: &mut Reader) -> Option<TypeSig> {
        Some(match r.u8()? {
            0x0F => TypeSig::Ptr(Box::new(self.sig_type(r)?)),
            0x10 => TypeSig::ByRef(Box::new(self.sig_type(r)?)),
            0x11 => TypeSig::ValueType(self.sig_token(r)?),
            0x12 => TypeSig::Class(self.sig_token(r)?),
            0x13 => TypeSig::Var(r.compressed()?),
            0x14 => {
                let ty = self.sig_type(r)?;
                let rank = r.compressed()?;
                // Sizes, then lower bounds: counted lists of compressed integers.
                for _ in 0..2 {
                    for _ in 0..r.compressed()? {
                        r.compressed()?;
                    }
                }
                TypeSig::Array(Box::new(ty), rank)
            }
            0x15 => {
                let value_type = r.u8()? == 0x11;
                let ty = self.sig_token(r)?;
                let args = (0..r.compressed()?).map(|_| self.sig_type(r)).collect::<Option<_>>()?;
                TypeSig::GenericInst { ty, value_type, args }
            }
            0x1B => {
                self.method_sig(r)?;
                TypeSig::FnPtr
            }
            0x1D => TypeSig::SzArray(Box::new(self.sig_type(r)?)),
            0x1E => TypeSig::MVar(r.compressed()?),
            // modreq, modopt: a type token, then the type.
            0x1F | 0x20 => {
                r.compressed()?;
                return self.sig_type(r);
            }
            // The vararg sentinel, pinned: then the type.
            0x41 | 0x45 => return self.sig_type(r),
            b => primitive(b).unwrap_or(TypeSig::Unknown(b)),
        })
    }

    /// A TypeDefOrRefOrSpecEncoded class (II.23.2.8); compilers emit no TypeSpec here, and one
    /// fails the signature.
    fn sig_token(&self, r: &mut Reader) -> Option<TypeName> {
        let v = r.compressed()?;
        self.name_of([TYPE_DEF, TYPE_REF, NONE, NONE][v as usize & 3], v >> 2)
    }
}

impl Assemblies {
    /// Every *.dll in `dir` that is a .NET assembly (others are skipped), in file name order.
    pub fn load_dir(dir: &Path) -> io::Result<Assemblies> {
        let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)?
            .filter_map(|e| Some(e.ok()?.path()))
            .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("dll")))
            .collect();
        paths.sort();
        let mut set = Assemblies(paths.iter().filter_map(|p| Assembly::load(p).ok()).collect());
        // Other assemblies' enums in attribute arguments: read again now that they resolve.
        for i in 0..set.0.len() {
            if set.0[i].guessed_enums {
                let (attrs, guessed) = set.0[i].read_attributes(&|n| set.enum_base(n));
                set.0[i].set_attributes(attrs);
                set.0[i].guessed_enums = guessed;
            }
        }
        Ok(set)
    }

    /// (assembly index, type index) of a type: in its scope's assembly if that defines it, else
    /// the first that does (type forwarders, and scope None: names several assemblies define
    /// resolve to the first).
    pub fn resolve(&self, name: &TypeName) -> Option<(usize, usize)> {
        let find = |a: usize| self.0[a].find(&name.namespace, &name.name).map(|t| (a, t));
        let scoped = name.scope.as_ref().and_then(|s| self.0.iter().position(|a| a.name == *s));
        scoped.and_then(find).or_else(|| (0..self.0.len()).find_map(find))
    }

    /// An enum's underlying type; None for other types.
    pub fn enum_base(&self, name: &TypeName) -> Option<TypeSig> {
        let (a, t) = self.resolve(name)?;
        enum_base(&self.0[a].types[t])
    }

    pub fn def(&self, name: &TypeName) -> Option<&TypeDef> {
        let (a, t) = self.resolve(name)?;
        self.0[a].types.get(t)
    }

    /// The assembly that defines a type.
    pub fn assembly_of(&self, name: &TypeName) -> Option<&Assembly> {
        self.resolve(name).map(|(a, _)| &self.0[a])
    }

    /// The class a type extends, by name.
    pub fn base(&self, name: &TypeName) -> Option<TypeName> {
        match self.def(name)?.extends.as_ref()? {
            TypeSig::Class(b) | TypeSig::ValueType(b) | TypeSig::GenericInst { ty: b, .. } => Some(b.clone()),
            _ => None,
        }
    }

    /// The type or one of its bases is `ancestor` ("Namespace.Name").
    pub fn derives_from(&self, name: &TypeName, ancestor: &str) -> bool {
        let mut cur = Some(name.clone());
        for _ in 0..32 {
            let Some(c) = cur else { return false };
            if c.full() == ancestor {
                return true;
            }
            cur = self.base(&c);
        }
        false
    }
}

impl TypeName {
    /// "Namespace.Name" ("Outer/Inner" for nested types).
    pub fn full(&self) -> String {
        if self.namespace.is_empty() { self.name.clone() } else { format!("{}.{}", self.namespace, self.name) }
    }
}

/// An enum's underlying type: the type of its instance field (value__).
fn enum_base(t: &TypeDef) -> Option<TypeSig> {
    let is_enum = matches!(&t.extends, Some(TypeSig::Class(n)) if n.namespace == "System" && n.name == "Enum");
    t.fields.iter().find(|f| is_enum && f.flags & 0x10 == 0).map(|f| f.sig.clone())
}

/// A custom attribute blob (II.23.3): prolog 0x0001, the constructor's arguments, then named
/// field and property arguments.
fn decode_attr(ty: TypeName, params: &[TypeSig], blob: &[u8], underlying: &dyn Fn(&TypeName) -> TypeSig) -> Attr {
    let mut attr = Attr { ty, fixed: Vec::new(), named: Vec::new() };
    let mut r = Reader::new(blob);
    if r.u16() != Some(1) {
        if !blob.is_empty() {
            attr.fixed.push(AttrArg::Other);
        }
        return attr;
    }
    for p in params {
        let Some(a) = arg(&mut r, p, underlying) else {
            attr.fixed.push(AttrArg::Other);
            return attr;
        };
        attr.fixed.push(a);
    }
    for _ in 0..r.u16().unwrap_or(0) {
        let Some(named) = named_arg(&mut r, underlying) else {
            attr.named.push((String::new(), AttrArg::Other));
            break;
        };
        attr.named.push(named);
    }
    attr
}

/// A NamedArg: FIELD (0x53) or PROPERTY (0x54), its type, name and value.
fn named_arg(r: &mut Reader, underlying: &dyn Fn(&TypeName) -> TypeSig) -> Option<(String, AttrArg)> {
    r.u8()?;
    let ty = elem_type(r)?;
    let name = r.ser_string()?.unwrap_or_default();
    Some((name, arg(r, &ty, underlying)?))
}

/// A FieldOrPropType (II.23.3): the type of a named or boxed argument.
fn elem_type(r: &mut Reader) -> Option<TypeSig> {
    Some(match r.u8()? {
        0x1D => TypeSig::SzArray(Box::new(elem_type(r)?)),
        0x50 => TypeSig::Class(TypeName { scope: None, namespace: "System".into(), name: "Type".into() }),
        0x51 => TypeSig::Object,
        0x55 => TypeSig::ValueType(parse_type_name(&r.ser_string()??)),
        b => primitive(b)?,
    })
}

/// One argument value (II.23.3 Elem) of type `ty`; an enum as its underlying type.
fn arg(r: &mut Reader, ty: &TypeSig, underlying: &dyn Fn(&TypeName) -> TypeSig) -> Option<AttrArg> {
    use AttrArg as A;
    use TypeSig as T;
    Some(match ty {
        T::Bool => A::Bool(r.u8()? != 0),
        T::Char | T::U2 => A::UInt(r.u16()?.into()),
        T::I1 => A::Int((r.u8()? as i8).into()),
        T::U1 => A::UInt(r.u8()?.into()),
        T::I2 => A::Int((r.u16()? as i16).into()),
        T::I4 => A::Int((r.u32()? as i32).into()),
        T::U4 => A::UInt(r.u32()?.into()),
        T::I8 => A::Int(r.u64()? as i64),
        T::U8 => A::UInt(r.u64()?),
        T::R4 => A::Float(f32::from_bits(r.u32()?).into()),
        T::R8 => A::Float(f64::from_bits(r.u64()?)),
        T::String => A::String(r.ser_string()?),
        T::Class(n) if n.namespace == "System" && n.name == "Type" => A::Type(r.ser_string()?),
        // Boxed: the value's own type first.
        T::Object => {
            let ty = elem_type(r)?;
            return arg(r, &ty, underlying);
        }
        T::SzArray(item) => match r.u32()? {
            u32::MAX => A::Array(Vec::new()),
            n => A::Array((0..n).map(|_| arg(r, item, underlying)).collect::<Option<_>>()?),
        },
        T::ValueType(n) => match underlying(n) {
            base @ (T::Bool | T::Char | T::I1 | T::U1 | T::I2 | T::U2 | T::I4 | T::U4 | T::I8 | T::U8) => return arg(r, &base, underlying),
            _ => return None,
        },
        _ => return None,
    })
}

/// A type as an attribute blob names it: "Namespace.Outer+Inner, Assembly, Version=...".
fn parse_type_name(s: &str) -> TypeName {
    let mut parts = s.split(',');
    let full = parts.next().unwrap_or_default().trim().replace('+', "/");
    let scope = parts.next().map(|a| a.trim().to_string());
    let (namespace, name) = match full.split('/').next().unwrap_or_default().rfind('.') {
        Some(i) => (&full[..i], &full[i + 1..]),
        None => ("", &full[..]),
    };
    TypeName { scope, namespace: namespace.to_string(), name: name.to_string() }
}

/// A Constant value (II.22.9) of element type `ty` as i64, when an integer, bool or char.
fn constant(ty: u8, blob: &[u8]) -> Option<i64> {
    match arg(&mut Reader::new(blob), &primitive(ty)?, &|_| TypeSig::I4)? {
        AttrArg::Bool(b) => Some(b.into()),
        AttrArg::Int(i) => Some(i),
        AttrArg::UInt(u) => Some(u as i64),
        _ => None,
    }
}

/// The element types that are a single byte (II.23.1.16).
fn primitive(b: u8) -> Option<TypeSig> {
    use TypeSig::*;
    Some(match b {
        0x01 => Void,
        0x02 => Bool,
        0x03 => Char,
        0x04 => I1,
        0x05 => U1,
        0x06 => I2,
        0x07 => U2,
        0x08 => I4,
        0x09 => U4,
        0x0A => I8,
        0x0B => U8,
        0x0C => R4,
        0x0D => R8,
        0x0E => String,
        0x16 => TypedRef,
        0x18 => IntPtr,
        0x19 => UIntPtr,
        0x1C => Object,
        _ => return None,
    })
}

/// The element of `v` a 1-based row number names.
fn slot<T>(v: &mut [T], row: u32) -> Option<&mut T> {
    v.get_mut((row as usize).checked_sub(1)?)
}

// Metadata tables (II.22) by number.
const MODULE: usize = 0x00;
const TYPE_REF: usize = 0x01;
const TYPE_DEF: usize = 0x02;
const FIELD_PTR: usize = 0x03;
const FIELD: usize = 0x04;
const METHOD_PTR: usize = 0x05;
const METHOD: usize = 0x06;
const PARAM_PTR: usize = 0x07;
const PARAM: usize = 0x08;
const INTERFACE_IMPL: usize = 0x09;
const MEMBER_REF: usize = 0x0A;
const CONSTANT: usize = 0x0B;
const CUSTOM_ATTRIBUTE: usize = 0x0C;
const DECL_SECURITY: usize = 0x0E;
const CLASS_LAYOUT: usize = 0x0F;
const FIELD_LAYOUT: usize = 0x10;
const STANDALONE_SIG: usize = 0x11;
const EVENT: usize = 0x14;
const PROPERTY: usize = 0x17;
const MODULE_REF: usize = 0x1A;
const TYPE_SPEC: usize = 0x1B;
const FIELD_RVA: usize = 0x1D;
const ASSEMBLY: usize = 0x20;
const ASSEMBLY_REF: usize = 0x23;
const FILE: usize = 0x26;
const EXPORTED_TYPE: usize = 0x27;
const MANIFEST_RESOURCE: usize = 0x28;
const NESTED_CLASS: usize = 0x29;
const GENERIC_PARAM: usize = 0x2A;
const METHOD_SPEC: usize = 0x2B;
const GENERIC_PARAM_CONSTRAINT: usize = 0x2C;
/// A coded index tag no table has.
const NONE: usize = usize::MAX;

// Coded indexes (II.24.2.6): the tables a tag selects, in tag order.
const TYPE_DEF_OR_REF: &[usize] = &[TYPE_DEF, TYPE_REF, TYPE_SPEC];
const HAS_CONSTANT: &[usize] = &[FIELD, PARAM, PROPERTY];
const HAS_CUSTOM_ATTRIBUTE: &[usize] = &[
    METHOD, FIELD, TYPE_REF, TYPE_DEF, PARAM, INTERFACE_IMPL, MEMBER_REF, MODULE, DECL_SECURITY, PROPERTY, EVENT, STANDALONE_SIG,
    MODULE_REF, TYPE_SPEC, ASSEMBLY, ASSEMBLY_REF, FILE, EXPORTED_TYPE, MANIFEST_RESOURCE, GENERIC_PARAM, GENERIC_PARAM_CONSTRAINT,
    METHOD_SPEC,
];
const HAS_FIELD_MARSHAL: &[usize] = &[FIELD, PARAM];
const HAS_DECL_SECURITY: &[usize] = &[TYPE_DEF, METHOD, ASSEMBLY];
const MEMBER_REF_PARENT: &[usize] = &[TYPE_DEF, TYPE_REF, MODULE_REF, METHOD, TYPE_SPEC];
const HAS_SEMANTICS: &[usize] = &[EVENT, PROPERTY];
const METHOD_DEF_OR_REF: &[usize] = &[METHOD, MEMBER_REF];
const MEMBER_FORWARDED: &[usize] = &[FIELD, METHOD];
const IMPLEMENTATION: &[usize] = &[FILE, ASSEMBLY_REF, EXPORTED_TYPE];
const CUSTOM_ATTRIBUTE_TYPE: &[usize] = &[NONE, NONE, METHOD, MEMBER_REF, NONE];
const RESOLUTION_SCOPE: &[usize] = &[MODULE, MODULE_REF, ASSEMBLY_REF, TYPE_REF];
const TYPE_OR_METHOD_DEF: &[usize] = &[TYPE_DEF, METHOD];

/// A table column: fixed width, heap index, row index into a table, or coded index.
#[derive(Clone, Copy)]
enum Col {
    U16,
    U32,
    Str,
    Guid,
    Blob,
    T(usize),
    C(&'static [usize]),
}

use Col::*;

/// The columns of every table ECMA-335 defines (II.22), by table number: a stream's tables are
/// stored back to back, so each one's row size is needed to find the next.
const SCHEMA: [&[Col]; 0x2D] = [
    &[U16, Str, Guid, Guid, Guid],                       // Module
    &[C(RESOLUTION_SCOPE), Str, Str],                    // TypeRef
    &[U32, Str, Str, C(TYPE_DEF_OR_REF), T(FIELD), T(METHOD)], // TypeDef
    &[T(FIELD)],                                         // FieldPtr
    &[U16, Str, Blob],                                   // Field
    &[T(METHOD)],                                        // MethodPtr
    &[U32, U16, U16, Str, Blob, T(PARAM)],               // MethodDef
    &[T(PARAM)],                                         // ParamPtr
    &[U16, U16, Str],                                    // Param
    &[T(TYPE_DEF), C(TYPE_DEF_OR_REF)],                  // InterfaceImpl
    &[C(MEMBER_REF_PARENT), Str, Blob],                  // MemberRef
    &[U16, C(HAS_CONSTANT), Blob],                       // Constant (type byte + padding)
    &[C(HAS_CUSTOM_ATTRIBUTE), C(CUSTOM_ATTRIBUTE_TYPE), Blob], // CustomAttribute
    &[C(HAS_FIELD_MARSHAL), Blob],                       // FieldMarshal
    &[U16, C(HAS_DECL_SECURITY), Blob],                  // DeclSecurity
    &[U16, U32, T(TYPE_DEF)],                            // ClassLayout
    &[U32, T(FIELD)],                                    // FieldLayout
    &[Blob],                                             // StandAloneSig
    &[T(TYPE_DEF), T(EVENT)],                            // EventMap
    &[T(EVENT)],                                         // EventPtr
    &[U16, Str, C(TYPE_DEF_OR_REF)],                     // Event
    &[T(TYPE_DEF), T(PROPERTY)],                         // PropertyMap
    &[T(PROPERTY)],                                      // PropertyPtr
    &[U16, Str, Blob],                                   // Property
    &[U16, T(METHOD), C(HAS_SEMANTICS)],                 // MethodSemantics
    &[T(TYPE_DEF), C(METHOD_DEF_OR_REF), C(METHOD_DEF_OR_REF)], // MethodImpl
    &[Str],                                              // ModuleRef
    &[Blob],                                             // TypeSpec
    &[U16, C(MEMBER_FORWARDED), Str, T(MODULE_REF)],     // ImplMap
    &[U32, T(FIELD)],                                    // FieldRVA
    &[U32, U32],                                         // EncLog
    &[U32],                                              // EncMap
    &[U32, U16, U16, U16, U16, U32, Blob, Str, Str],     // Assembly
    &[U32],                                              // AssemblyProcessor
    &[U32, U32, U32],                                    // AssemblyOS
    &[U16, U16, U16, U16, U32, Blob, Str, Str, Blob],    // AssemblyRef
    &[U32, T(ASSEMBLY_REF)],                             // AssemblyRefProcessor
    &[U32, U32, U32, T(ASSEMBLY_REF)],                   // AssemblyRefOS
    &[U32, Str, Blob],                                   // File
    &[U32, U32, Str, Str, C(IMPLEMENTATION)],            // ExportedType
    &[U32, U32, Str, C(IMPLEMENTATION)],                 // ManifestResource
    &[T(TYPE_DEF), T(TYPE_DEF)],                         // NestedClass
    &[U16, U16, C(TYPE_OR_METHOD_DEF), Str],             // GenericParam
    &[C(METHOD_DEF_OR_REF), Blob],                       // MethodSpec
    &[T(GENERIC_PARAM), C(TYPE_DEF_OR_REF)],             // GenericParamConstraint
];

/// The tag bits of a coded index over `tables`.
fn tag_bits(tables: &[usize]) -> u32 {
    tables.len().next_power_of_two().trailing_zeros()
}

struct Table {
    offset: usize,
    rows: u32,
    row_size: usize,
    /// Offset in the row and width (2 or 4) of each column.
    cols: Vec<(usize, usize)>,
}

/// The PE file, and where its metadata heaps and tables are in it.
struct Image {
    data: Vec<u8>,
    /// Section headers: virtual size, virtual address, file size, file offset.
    sections: Vec<[u32; 4]>,
    strings: Range<usize>,
    blob: Range<usize>,
    us: Range<usize>,
    tables: Vec<Table>,
}

impl Image {
    /// The DOS and PE headers, the CLI header (data directory 14) and the metadata root (II.24-25).
    fn parse(data: Vec<u8>) -> Option<Image> {
        let d = &data[..];
        if d.get(..2)? != b"MZ" {
            return None;
        }
        let pe = Reader::at(d, 0x3C).u32()? as usize;
        if d.get(pe..pe + 4)? != b"PE\0\0" {
            return None;
        }
        let count = usize::from(Reader::at(d, pe + 6).u16()?);
        let opt = pe + 24;
        let section_table = opt + usize::from(Reader::at(d, pe + 20).u16()?);
        // The data directories follow 96 bytes of PE32 or 112 of PE32+ optional header.
        let dirs = match Reader::at(d, opt).u16()? {
            0x10B => opt + 96,
            0x20B => opt + 112,
            _ => return None,
        };
        if Reader::at(d, dirs - 4).u32()? <= 14 {
            return None;
        }
        let cli = Reader::at(d, dirs + 14 * 8).u32()?;
        let sections = (0..count)
            .map(|i| {
                let mut r = Reader::at(d, section_table + i * 40 + 8);
                Some([r.u32()?, r.u32()?, r.u32()?, r.u32()?])
            })
            .collect::<Option<_>>()?;
        let mut img = Image { data, sections, strings: 0..0, blob: 0..0, us: 0..0, tables: Vec::new() };
        img.metadata(cli)?;
        Some(img)
    }

    /// The metadata root ("BSJB"), its streams, and the tables stream's header (II.24.2).
    fn metadata(&mut self, cli: u32) -> Option<()> {
        let d = &self.data[..];
        let root = self.offset(Reader::at(d, self.offset(cli)? + 8).u32()?)?;
        let mut r = Reader::at(d, root);
        if r.u32()? != 0x424A_5342 {
            return None;
        }
        r.p += 8; // versions, reserved
        let len = r.u32()? as usize;
        r.p += len + 2; // version string, flags
        let mut tables = None;
        for _ in 0..r.u16()? {
            let (at, size) = (root + r.u32()? as usize, r.u32()? as usize);
            let name = d.get(r.p..)?.split(|&b| b == 0).next()?;
            r.p += (name.len() + 4) & !3; // the name, NUL-terminated, padded to 4 bytes
            match name {
                b"#~" | b"#-" => tables = Some(at..at + size),
                b"#Strings" => self.strings = at..at + size,
                b"#Blob" => self.blob = at..at + size,
                b"#US" => self.us = at..at + size,
                _ => {} // #GUID: nothing reads it
            }
        }
        let stream = tables?;
        let mut r = Reader::at(d, stream.start + 6);
        let heaps = r.u8()?;
        r.p += 1;
        let present = r.u64()?;
        r.p += 8; // sorted
        if present >> SCHEMA.len() != 0 {
            return None; // a table ECMA-335 does not define: its row size, so the rest, is unknown
        }
        let mut rows = [0u32; SCHEMA.len()];
        for (t, n) in rows.iter_mut().enumerate() {
            if present >> t & 1 == 1 {
                *n = r.u32()?;
            }
        }
        if heaps & 0x40 != 0 {
            r.p += 4; // extra data some #- streams have
        }
        let wide = |flag: u8| if heaps & flag != 0 { 4 } else { 2 };
        let count = |t: usize| rows.get(t).copied().unwrap_or(0);
        let width = |c: Col| match c {
            U16 => 2,
            U32 => 4,
            Str => wide(1),
            Guid => wide(2),
            Blob => wide(4),
            T(t) => if count(t) < 1 << 16 { 2 } else { 4 },
            C(list) => if list.iter().map(|&t| count(t)).max().unwrap_or(0) < 1 << (16 - tag_bits(list)) { 2 } else { 4 },
        };
        let mut at = r.p;
        let tables: Vec<Table> = SCHEMA
            .iter()
            .zip(rows)
            .map(|(cols, rows)| {
                let mut row_size = 0;
                let cols = cols
                    .iter()
                    .map(|&c| {
                        row_size += width(c);
                        (row_size - width(c), width(c))
                    })
                    .collect();
                let table = Table { offset: at, rows, row_size, cols };
                at += row_size * rows as usize;
                table
            })
            .collect();
        if at > stream.end.min(d.len()) {
            return None;
        }        self.tables = tables;
        Some(())
    }

    /// The file offset of an RVA: in a section's file data, or None.
    fn offset(&self, rva: u32) -> Option<usize> {
        let &[_, va, _, at] = self.sections.iter().find(|&&[_, va, size, _]| rva >= va && rva - va < size)?;
        Some((rva - va + at) as usize)
    }

    fn rows(&self, table: usize) -> u32 {
        self.tables[table].rows
    }

    /// Column `col` of 1-based row `row`; 0 outside the table.
    fn cell(&self, table: usize, row: u32, col: usize) -> u32 {
        let t = &self.tables[table];
        if row == 0 || row > t.rows {
            return 0;
        }
        let (offset, width) = t.cols[col];
        let mut r = Reader::at(&self.data, t.offset + (row as usize - 1) * t.row_size + offset);
        if width == 2 { r.u16().map_or(0, u32::from) } else { r.u32().unwrap_or(0) }
    }

    /// A coded index cell as (table, row).
    fn refer(&self, table: usize, row: u32, col: usize) -> (usize, u32) {
        let v = self.cell(table, row, col);
        let C(list) = SCHEMA[table][col] else { return (NONE, 0) };
        let bits = tag_bits(list);
        (list.get((v & ((1 << bits) - 1)) as usize).copied().unwrap_or(NONE), v >> bits)
    }

    /// The rows of `target` that row `row` of `table` owns through its list column `col` (II.22:
    /// from its value up to the next row's), read through the `ptr` table when there is one.
    fn run(&self, table: usize, row: u32, col: usize, target: usize, ptr: usize) -> Vec<u32> {
        let through = self.rows(ptr) > 0;
        let last = if through { self.rows(ptr) } else { self.rows(target) };
        let end = if row < self.rows(table) { self.cell(table, row + 1, col) } else { last + 1 };
        (self.cell(table, row, col).max(1)..end.min(last + 1)).map(|i| if through { self.cell(ptr, i, 0) } else { i }).collect()
    }

    /// A #Strings entry: NUL-terminated UTF-8.
    fn string(&self, index: u32) -> String {
        let s = self.data.get(self.strings.start + index as usize..self.strings.end).unwrap_or_default();
        String::from_utf8_lossy(s.split(|&b| b == 0).next().unwrap_or_default()).into_owned()
    }

    /// A #Blob entry: a compressed length, then the bytes.
    fn blob(&self, index: u32) -> &[u8] {
        let mut r = Reader::at(self.data.get(self.blob.clone()).unwrap_or_default(), index as usize);
        r.compressed().and_then(|n| r.bytes(n as usize)).unwrap_or_default()
    }

    /// A #US entry (an ldstr literal): a compressed length, UTF-16 units, a final flag byte.
    fn user_string(&self, index: u32) -> Option<String> {
        let mut r = Reader::at(self.data.get(self.us.clone())?, index as usize);
        let n = r.compressed()? as usize;
        let b = r.bytes(n.checked_sub(1)?)?;
        String::from_utf16(&b.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect::<Vec<_>>()).ok()
    }
}

/// Little-endian reads from a byte slice; None past its end.
struct Reader<'a> {
    d: &'a [u8],
    p: usize,
}

impl<'a> Reader<'a> {
    fn new(d: &'a [u8]) -> Self {
        Reader { d, p: 0 }
    }
    fn at(d: &'a [u8], p: usize) -> Self {
        Reader { d, p }
    }
    fn bytes(&mut self, n: usize) -> Option<&'a [u8]> {
        let b = self.d.get(self.p..self.p.checked_add(n)?)?;
        self.p += n;
        Some(b)
    }
    fn u8(&mut self) -> Option<u8> {
        Some(self.bytes(1)?[0])
    }
    fn peek(&self) -> Option<u8> {
        self.d.get(self.p).copied()
    }
    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_le_bytes(self.bytes(2)?.try_into().ok()?))
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.bytes(4)?.try_into().ok()?))
    }
    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.bytes(8)?.try_into().ok()?))
    }

    /// A compressed unsigned integer (II.23.2): big-endian in 1, 2 or 4 bytes, the length in the
    /// top bits of the first.
    fn compressed(&mut self) -> Option<u32> {
        let b = u32::from(self.u8()?);
        Some(match b {
            0x00..=0x7F => b,
            0x80..=0xBF => (b & 0x3F) << 8 | u32::from(self.u8()?),
            0xC0..=0xDF => {
                let r = self.bytes(3)?;
                (b & 0x1F) << 24 | u32::from(r[0]) << 16 | u32::from(r[1]) << 8 | u32::from(r[2])
            }
            _ => return None,
        })
    }

    /// A SerString (II.23.3): compressed length and UTF-8, or 0xFF for null.
    fn ser_string(&mut self) -> Option<Option<String>> {
        if self.d.get(self.p) == Some(&0xFF) {
            self.p += 1;
            return Some(None);
        }
        let n = self.compressed()? as usize;
        Some(Some(String::from_utf8_lossy(self.bytes(n)?).into_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::OnceLock;

    /// The game's Managed folder: PCDS_GAME_DIR, else the default Steam library. Each user has
    /// their own game, so it may be missing: then None, and the test skips.
    fn managed() -> Option<PathBuf> {
        let home = std::env::var_os("HOME").map(|h| Path::new(&h).join(".local/share/Steam/steamapps/common/The Planet Crafter"));
        let dir = std::env::var_os("PCDS_GAME_DIR").map(PathBuf::from).or(home)?.join("Planet Crafter_Data/Managed");
        if !dir.is_dir() {
            eprintln!("skipped: no game in {}", dir.display());
            return None;
        }
        Some(dir)
    }

    /// The whole Managed folder, loaded once for all tests.
    fn game() -> Option<&'static Assemblies> {
        static GAME: OnceLock<Option<Assemblies>> = OnceLock::new();
        GAME.get_or_init(|| Some(Assemblies::load_dir(&managed()?).unwrap())).as_ref()
    }

    fn name(namespace: &str, name: &str) -> TypeName {
        TypeName { scope: None, namespace: namespace.into(), name: name.into() }
    }

    #[test]
    fn reads_group_data_and_its_base_chain() {
        let Some(dir) = managed() else { return };
        let asm = Assembly::load(&dir.join("Assembly-CSharp.dll")).unwrap();
        assert_eq!(asm.name, "Assembly-CSharp");
        assert_eq!(asm.types()[0].name, "<Module>");
        for class in ["GroupData", "GroupDataItem"] {
            let t = &asm.types()[asm.find("SpaceCraft", class).unwrap()];
            println!("{class} : {:?} flags {:#x}", t.extends, t.flags);
            for f in &t.fields {
                println!("  {:#06x} {:?} {} {:?}", f.flags, f.sig, f.name, f.attributes.iter().map(|a| &a.ty.name).collect::<Vec<_>>());
            }
        }
        let group = &asm.types()[asm.find("SpaceCraft", "GroupData").unwrap()];
        let names: Vec<_> = group.fields.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names[..5], ["id", "associatedGameObject", "icon", "recipeIngredients", "hideInCrafter"]);
        let list = TypeName { scope: Some("mscorlib".into()), ..name("System.Collections.Generic", "List`1") };
        assert_eq!(group.fields[3].sig, TypeSig::GenericInst { ty: list, value_type: false, args: vec![TypeSig::Class(name("SpaceCraft", "GroupDataItem"))] });
        // A nested enum, and a [Tooltip("...")] string argument.
        let unit = group.fields.iter().find(|f| f.name == "unlockingWorldUnit").unwrap();
        assert_eq!(unit.sig, TypeSig::ValueType(name("SpaceCraft", "DataConfig/WorldUnitType")));
        assert!(asm.find("SpaceCraft", "DataConfig/WorldUnitType").is_some());
        assert!(matches!(&unit.attributes[0].fixed[..], [AttrArg::String(Some(s))] if !s.is_empty()), "{:?}", unit.attributes);
        let Some(game) = game() else { return };
        let mut chain = Vec::new();
        let mut at = game.resolve(&name("SpaceCraft", "GroupDataItem"));
        while let Some((a, t)) = at {
            let t = &game.0[a].types()[t];
            chain.push(format!("{}.{}", t.namespace, t.name));
            at = match &t.extends {
                Some(TypeSig::Class(n)) => game.resolve(n),
                _ => None,
            };
        }
        assert_eq!(chain, ["SpaceCraft.GroupDataItem", "SpaceCraft.GroupData", "UnityEngine.ScriptableObject", "UnityEngine.Object", "System.Object"]);
    }

    #[test]
    fn reports_serialize_field_on_private_fields() {
        let Some(dir) = managed() else { return };
        let asm = Assembly::load(&dir.join("Assembly-CSharp.dll")).unwrap();
        let serialized: Vec<_> = asm
            .types()
            .iter()
            .flat_map(|t| t.fields.iter().map(move |f| (t, f)))
            .filter(|(_, f)| f.flags & 7 == 1 && f.attributes.iter().any(|a| a.ty.namespace == "UnityEngine" && a.ty.name == "SerializeField"))
            .collect();
        println!("{} private [SerializeField] fields, e.g. {:?}", serialized.len(), serialized.iter().take(5).map(|(t, f)| format!("{}.{}", t.name, f.name)).collect::<Vec<_>>());
        assert!(!serialized.is_empty());
        let (_, f) = serialized[0];
        let a = f.attributes.iter().find(|a| a.ty.name == "SerializeField").unwrap();
        assert_eq!(a.ty, TypeName { scope: Some("UnityEngine.CoreModule".into()), ..name("UnityEngine", "SerializeField") });
    }

    /// rpcs.json (hash → "Class.Method"): each hash has its __rpc_handler_<hash>, the RPC
    /// method's IL loads the hash (ldc.i4) to send it, and it has a Netcode attribute:
    /// [ServerRpc] or [ClientRpc] as rpcs.json's "to" says, or [Rpc(SendTo.X)] (for which the
    /// extractor finds no SendTo and always writes "server").
    #[test]
    fn finds_every_rpc_hash_in_il() {
        let (Some(game), Some(data)) = (game(), crate::test_data()) else { return };
        let rpcs: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(format!("{data}/rpcs.json")).unwrap()).unwrap();
        let rpcs = rpcs.as_object().unwrap();
        assert!(rpcs.contains_key("384825098"));
        let (a, t) = game.resolve(&name("Unity.Netcode", "SendTo")).unwrap();
        let send_to = &game.0[a].types()[t].fields;
        let target = |m: &MethodDef| {
            m.attributes.iter().filter(|a| a.ty.namespace == "Unity.Netcode").find_map(|a| match (a.ty.name.as_str(), a.fixed.first()) {
                ("ServerRpcAttribute", _) => Some("server".to_string()),
                ("ClientRpcAttribute", _) => Some("client".to_string()),
                ("RpcAttribute", Some(AttrArg::Int(v))) => send_to.iter().find(|f| f.constant == Some(*v)).map(|f| format!("SendTo.{}", f.name)),
                _ => None,
            })
        };
        let mut targets = std::collections::BTreeMap::<String, usize>::new();
        for (hash, rpc) in rpcs {
            let full = rpc["name"].as_str().unwrap();
            let (class, method) = full.rsplit_once('.').unwrap();
            let handler = format!("__rpc_handler_{hash}");
            let found = game.0.iter().flat_map(|a| a.types().iter().map(move |t| (a, t))).find(|(_, t)| t.name == class && t.methods.iter().any(|m| m.name == handler));
            let Some((asm, t)) = found else { panic!("{full}: no type {class} with {handler}") };
            assert!(t.methods.iter().any(|m| m.name == handler && m.flags & 0x10 != 0), "{full}: handler not static");
            let mut ldc = vec![0x20];
            ldc.extend(hash.parse::<u32>().unwrap().to_le_bytes());
            let sender = t.methods.iter().find(|m| m.name == method && m.flags & 0x10 == 0 && asm.il(m).is_some_and(|il| il.windows(5).any(|w| w == ldc)));
            let Some(m) = sender else { panic!("{full}: no {method} loading {hash}") };
            let Some(to) = target(m) else { panic!("{full}: no RPC attribute: {:?}", m.attributes) };
            assert!(to.starts_with("SendTo.") || rpc["to"] == to.as_str(), "{full}: {to}, rpcs.json says {}", rpc["to"]);
            if hash == "384825098" {
                assert_eq!((t.namespace.as_str(), t.name.as_str(), to.as_str()), ("SpaceCraft", "ActionnableInteractiveProxy", "SendTo.Server"));
                println!("{full}: {:?}", m.attributes);
            }
            *targets.entry(to).or_default() += 1;
        }
        println!("{} RPCs found, sent to {targets:?}", rpcs.len());
    }

    #[test]
    fn loads_and_resolves_across_the_managed_folder() {
        let Some(game) = game() else { return };
        let dlls = std::fs::read_dir(managed().unwrap()).unwrap().filter(|e| e.as_ref().unwrap().path().extension().is_some_and(|x| x == "dll")).count();
        println!("{} of {dlls} DLLs; enums guessed in {:?}", game.0.len(), game.0.iter().filter(|a| a.guessed_enums).map(|a| &a.name).collect::<Vec<_>>());
        assert_eq!(game.0.len(), dlls);
        for a in &game.0 {
            assert_eq!(a.types()[0].name, "<Module>", "{}", a.name);
        }
        let (a, t) = game.resolve(&TypeName { scope: Some("mscorlib".into()), ..name("System", "DayOfWeek") }).unwrap();
        let days: Vec<_> = game.0[a].types()[t].fields.iter().map(|f| (f.name.as_str(), f.constant)).collect();
        assert_eq!(days[0], ("value__", None));
        assert_eq!(days[1..3], [("Sunday", Some(0)), ("Monday", Some(1))]);
        for (namespace, class, assembly) in [("Unity.Netcode", "NetworkBehaviour", "Unity.Netcode.Runtime"), ("UnityEngine", "MonoBehaviour", "UnityEngine.CoreModule")] {
            let (a, t) = game.resolve(&name(namespace, class)).unwrap();
            assert_eq!(game.0[a].name, assembly);
            assert_eq!(game.0[a].types()[t].name, class);
        }
    }
}
