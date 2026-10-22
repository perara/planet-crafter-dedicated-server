//! What clients see: the spawned network objects with their behaviours' variables, and the
//! messages queued for them this tick (spawns, despawns, RPCs, variable deltas), in the order
//! Netcode would send them.

use serde_json::Value;

use crate::compact::{Bytes, Sym};
use crate::data::{Shape, ShapeBehaviour, Signature};
use crate::gamedata::Layout;
use crate::protocol::{self, CHANGE_OWNERSHIP, CLIENT_RPC, HAS_PARENT, HAS_TRANSFORM, NETWORK_VARIABLE_DELTA};
use crate::rpcspec::Spec;
use crate::wire::Writer;

/// Who a message goes to (among approved clients).
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Target {
    All,
    Only(u64),
    Except(u64),
}

impl Target {
    pub fn reaches(self, client: u64) -> bool {
        match self {
            Target::All => true,
            Target::Only(c) => c == client,
            Target::Except(c) => c != client,
        }
    }
}

/// Encoded variable values: Netcode's NetworkVariable serialization per type.
pub mod enc {
    use crate::wire::Writer;

    pub fn bool(v: bool) -> Vec<u8> {
        vec![v as u8]
    }
    pub fn int(v: i32) -> Vec<u8> {
        Writer::new().packed_i32(v).0.clone()
    }
    pub fn short(v: i16) -> Vec<u8> {
        Writer::new().packed_i16(v).0.clone()
    }
    pub fn ulong(v: u64) -> Vec<u8> {
        Writer::new().packed_u64(v).0.clone()
    }
    pub fn float(v: f32) -> Vec<u8> {
        v.to_le_bytes().to_vec()
    }
    pub fn double(v: f64) -> Vec<u8> {
        v.to_le_bytes().to_vec()
    }
    /// Enums travel as their 4-byte underlying value.
    pub fn enumeration(v: i32) -> Vec<u8> {
        v.to_le_bytes().to_vec()
    }
    pub fn color(c: [f32; 4]) -> Vec<u8> {
        c.iter().flat_map(|v| v.to_le_bytes()).collect()
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum ListEvent {
    Add(#[serde(with = "crate::bincodec::bytes")] Vec<u8>),
    RemoveAt(i32),
    Value(i32, #[serde(with = "crate::bincodec::bytes")] Vec<u8>),
    Clear,
}

fn encode_events(events: &[ListEvent]) -> Vec<u8> {
    let mut w = Writer::new();
    w.u16(events.len() as u16);
    for e in events {
        match e {
            ListEvent::Add(item) => w.u8(0).bytes(item),
            ListEvent::RemoveAt(i) => w.u8(3).packed_i32(*i),
            ListEvent::Value(i, item) => w.u8(4).packed_i32(*i).bytes(item),
            ListEvent::Clear => w.u8(5),
        };
    }
    w.0
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum VarValue {
    Plain(#[serde(with = "crate::compact::bytes")] Bytes),
    List(#[serde(with = "crate::bincodec::byte_lists")] Vec<Vec<u8>>),
}

impl VarValue {
    /// The bytes a variable is sent as in a spawn: the value, or a list's count and items.
    #[cfg(test)]
    fn full(&self) -> Vec<u8> {
        match self {
            VarValue::Plain(b) => b.to_vec(),
            VarValue::List(items) => {
                let mut w = Writer::new();
                w.u16(items.len() as u16);
                items.iter().for_each(|i| {
                    w.bytes(i);
                });
                w.0
            }
        }
    }
}

/// The shape (names, types, order) of an object's variables is in `shape`, shared by every object
/// of its kind; what the object has of its own is `values`, one per variable in the shape's order
/// (behaviour after behaviour), and the deltas waiting to go out. Serialized as it always was,
/// behaviours with their variables by name (`serde_flat` below), so the snapshot is the same bytes.
#[derive(Clone, Debug)]
pub struct NetObj {
    pub id: u64,
    /// The layout this object was made from (group id, "#static" variant, or prefab name).
    pub hash: u32,
    pub flags: u16,
    pub owner: u64,
    pub parent: Option<u64>,
    pub position: [f32; 3],
    pub rotation: [f32; 4],
    pub scale: [f32; 3],
    shape: &'static Shape,
    values: Box<[VarValue]>,
    /// Pending deltas: per variable (its place in the shape) the new value, or the list events
    /// since the last flush. None nearly always.
    dirty: Option<Box<Vec<(u16, Vec<ListEvent>)>>>,
    pub tail: Bytes,
    pub tail_position: Option<usize>,
    /// The world object it stands for.
    pub wo: Option<i32>,
    /// In-scene objects come with the scene and are never spawned or despawned.
    pub scene: bool,
}

/// One behaviour of an object, as `NetObj::behaviours` lists them.
#[derive(Clone, Copy)]
pub struct BehaviourRef<'a> {
    pub class: Sym,
    pub index: u16,
    object: &'a NetObj,
    first: usize,
    count: usize,
}

impl<'a> BehaviourRef<'a> {
    /// The value of the behaviour's variable called `name`.
    pub fn var(&self, name: &str) -> Option<&'a VarValue> {
        let at = self.object.shape.vars[self.first..self.first + self.count].iter().position(|v| v.name == name)?;
        Some(&self.object.values[self.first + at])
    }

    /// Its variables: name, type, value.
    pub fn vars(&self) -> impl Iterator<Item = (Sym, Sym, &'a VarValue)> + use<'a> {
        let o = self.object;
        (self.first..self.first + self.count).map(move |i| (o.shape.vars[i].name, o.shape.vars[i].ty, &o.values[i]))
    }
}


/// List items of a NetworkList<T> from its serialized full value; None for item types this
/// server does not know (the value then stays raw bytes).
fn list_items(ty: &str, bytes: &[u8]) -> Option<Vec<Vec<u8>>> {
    let mut r = crate::wire::Reader::new(bytes);
    let n = r.u16_raw()?;
    let mut items = Vec::new();
    for _ in 0..n {
        let start = r.pos;
        list_item(ty, &mut r)?;
        items.push(bytes[start..r.pos].to_vec());
    }
    Some(items)
}

fn value_of(ty: &str, bytes: Vec<u8>) -> VarValue {
    if ty.starts_with("NetworkList") {
        if let Some(items) = list_items(ty, &bytes) {
            return VarValue::List(items);
        }
    }
    VarValue::Plain(bytes.into())
}

/// One full value of a variable of Netcode type `ty`, read off `r`; None for types this server
/// cannot size (lists of unknown items, custom structs).
pub fn read_value(ty: &str, r: &mut crate::wire::Reader) -> Option<Vec<u8>> {
    let start = r.pos;
    let inner = ty.strip_prefix("NetworkVariable<").and_then(|t| t.strip_suffix('>'));
    match inner {
        Some("Boolean" | "Byte" | "SByte") => r.skip(1)?,
        Some("Single" | "BuildPanelSubType") => r.skip(4)?,
        Some("Double") => r.skip(8)?,
        Some("Int32") => r.packed_i32().map(|_| ())?,
        Some("Int16") => r.packed_i16().map(|_| ())?,
        Some("UInt16") => r.packed_u16().map(|_| ())?,
        Some("UInt32") => r.packed_u32().map(|_| ())?,
        Some("UInt64" | "Int64") => r.packed_u64().map(|_| ())?,
        Some("Color" | "Quaternion" | "Vector4") => r.skip(16)?,
        Some("Vector3") => r.skip(12)?,
        Some("Vector2" | "Vector2Int") => r.skip(8)?,
        None if ty.starts_with("WorldUnit") => r.skip(20)?,
        Some(t) if t.starts_with("FixedString") => {
            let n = r.i32()?;
            r.skip(n.max(0) as usize)?
        }
        // Enums travel as 4 bytes.
        Some(t) if t.ends_with("Type") || t.ends_with("Status") || t.ends_with("State") => r.skip(4)?,
        _ => {
            if !ty.starts_with("NetworkList<") {
                return None;
            }
            let n = r.u16_raw()?;
            for _ in 0..n {
                list_item(ty, r)?;
            }
        }
    }
    Some(r.data[start..r.pos].to_vec())
}

fn list_item(ty: &str, r: &mut crate::wire::Reader) -> Option<()> {
    match ty {
        "NetworkList<Int32>" => r.packed_i32().map(|_| ()),
        "NetworkList<Int16>" => r.packed_i16().map(|_| ()),
        "NetworkList<GroupCountData>" | "NetworkList<Message>" => r.skip(8),
        "NetworkList<PlayerData>" => r.skip(56),
        _ => None,
    }
}

/// The variables the game lets an object's owner write (NetworkVariableWritePermission.Owner in
/// the game's classes; the tables do not carry the permission). The rest are the server's.
const OWNER_WRITES: &[(&str, &str)] = &[
    ("AIAnimalStateController", "_randomState"),
    ("AIAnimalStateController", "_spawnPosition"),
    ("AIAnimalStateController", "_currentState"),
    ("AIAnimalStateController", "_avoidancePriority"),
    ("AIAnimalStateController", "_disabled"),
    ("GroupNetworkContainer", "_scale"),
    ("GroupNetworkContainer", "_transformSync"),
    ("GroupNetworkContainer", "_groupHashOverride"),
    ("PlayerGaugesHandler", "_playerCanBreath"),
    ("PlayerGaugesHandler", "_playerIsUnderWater"),
    ("PlayerGaugesHandler", "_pauseConsumption"),
    ("PlayerShareState", "_jetpacking"),
    ("PlayerShareState", "_emote"),
    ("PlayerStatus", "_ready"),
    ("VehicleShareData", "_steerInput"),
    ("VehicleShareData", "_angularVelocity"),
    ("VehicleShareData", "_throttleInput"),
    ("VehicleShareData", "_lights"),
];

/// Applies a NetworkVariableDelta the owning client wrote; false (nothing applied, nothing to
/// pass on) when the sender does not own the object, or the delta writes a variable the game
/// does not let the owner write (as Netcode's server: it reads no further), one it cannot
/// read, or a value that is not a number.
pub fn apply_delta(net: &mut Net, client: u64, body: &[u8]) -> bool {
    let mut r = crate::wire::Reader::new(body);
    let (Some(object), Some(behaviour)) = (r.packed_u64(), r.packed_u16()) else { return false };
    if r.i32().is_none() {
        return false;
    }
    let Some(o) = net.objects.get_mut(&object) else { return false };
    if o.owner != client {
        return false;
    }
    let Some(b) = o.shape.behaviours.iter().find(|b| b.index == behaviour) else { return false };
    // Read first, applied only when every value is good: a NaN or infinity is neither kept nor
    // passed on (a float the server keeps could make the snapshot unreadable).
    let mut writes = Vec::new();
    for i in b.first as usize..(b.first + b.count) as usize {
        let v = &o.shape.vars[i];
        // Netcode writes a flag for every variable of the behaviour: a message that ends before the
        // last is cut, and is neither applied nor passed on (every other client would read past its end).
        let Some(dirty) = r.bool() else { return false };
        if !dirty {
            continue;
        }
        if !OWNER_WRITES.contains(&(b.class.as_str(), v.name.as_str())) {
            return false;
        }
        let Some(bytes) = read_value(&v.ty, &mut r).filter(|bytes| finite(&v.ty, bytes)) else { return false };
        writes.push((i, bytes));
    }
    for (i, bytes) in writes {
        o.values[i] = VarValue::Plain(bytes.into());
    }
    true
}

/// A number within rpcspec::SANE_FLOAT.
fn sane(v: f64) -> bool {
    v.is_finite() && v.abs() <= crate::rpcspec::SANE_FLOAT
}

/// Whether a value of a floating-point type (float, double, vectors, quaternion, color) is all
/// numbers within rpcspec::SANE_FLOAT; values of other types are.
fn finite(ty: &str, bytes: &[u8]) -> bool {
    match ty.strip_prefix("NetworkVariable<").and_then(|t| t.strip_suffix('>')) {
        Some("Double") => bytes.chunks_exact(8).all(|c| sane(f64::from_le_bytes(c.try_into().unwrap()))),
        Some("Single" | "Vector2" | "Vector3" | "Vector4" | "Quaternion" | "Color") => bytes.chunks_exact(4).all(|c| sane(f32::from_le_bytes(c.try_into().unwrap()) as f64)),
        _ => true,
    }
}

impl NetObj {
    pub fn from_layout(layout: &Layout) -> NetObj {
        let compiled = &*layout.compiled;
        // No bytes: the server sets the value (an empty value is none, see check).
        let values = compiled.shape.vars.iter().zip(compiled.defaults.iter()).map(|(v, d)| if v.list { value_of(&v.ty, d.to_vec()) } else { VarValue::Plain(d.clone()) }).collect();
        NetObj {
            id: 0,
            hash: layout.hash,
            flags: layout.flags,
            owner: 0,
            parent: None,
            position: [0.0; 3],
            rotation: [0.0, 0.0, 0.0, 1.0],
            scale: layout.scale,
            shape: compiled.shape,
            values,
            dirty: None,
            tail: Bytes::new(&layout.tail),
            tail_position: layout.tail_position,
            wo: None,
            scene: false,
        }
    }

    /// The object's behaviours, in order.
    pub fn behaviours(&self) -> impl Iterator<Item = BehaviourRef<'_>> + '_ {
        self.shape.behaviours.iter().map(move |b| BehaviourRef { class: b.class, index: b.index, object: self, first: b.first as usize, count: b.count as usize })
    }

    /// The first behaviour of a class.
    pub fn behaviour(&self, class: &str) -> Option<BehaviourRef<'_>> {
        self.behaviours().find(|b| b.class == class)
    }

    /// The place of a variable: in the first behaviour of `class`, the one called `name`.
    fn var_of(&self, class: &str, name: &str) -> Option<usize> {
        let b = self.shape.behaviours.iter().find(|b| b.class == class)?;
        self.var_in(b, name)
    }

    /// The place of the variable called `name` in the behaviour with this index.
    fn var_of_behaviour(&self, behaviour: u16, name: &str) -> Option<usize> {
        let b = self.shape.behaviours.iter().find(|b| b.index == behaviour)?;
        self.var_in(b, name)
    }

    fn var_in(&self, b: &ShapeBehaviour, name: &str) -> Option<usize> {
        let (first, count) = (b.first as usize, b.count as usize);
        Some(first + self.shape.vars[first..first + count].iter().position(|v| v.name == name)?)
    }

    /// Sets a variable's initial value, without a delta (before the object is spawned).
    pub fn init(&mut self, class: &str, name: &str, bytes: Vec<u8>) {
        if let Some(i) = self.var_of(class, name) {
            self.values[i] = value_of(&self.shape.vars[i].ty, bytes);
        }
    }

    pub fn init_list(&mut self, class: &str, name: &str, items: Vec<Vec<u8>>) {
        if let Some(i) = self.var_of(class, name) {
            self.values[i] = VarValue::List(items);
        }
    }

    /// Sets a plain variable of the behaviour with this index, without a delta; whether it changed
    /// (None: no such variable).
    pub fn init_in_behaviour(&mut self, behaviour: u16, name: &str, bytes: Vec<u8>) -> Option<bool> {
        let i = self.var_of_behaviour(behaviour, name)?;
        let changed = !matches!(&self.values[i], VarValue::Plain(b) if *b == bytes);
        self.values[i] = VarValue::Plain(bytes.into());
        Some(changed)
    }

    /// The events of a variable's pending delta, made if it had none.
    fn dirty_mut(&mut self, var: usize) -> &mut Vec<ListEvent> {
        let list = self.dirty.get_or_insert_with(Default::default);
        let var = var as u16;
        let at = match list.iter().position(|(v, _)| *v == var) {
            Some(at) => at,
            None => {
                list.push((var, Vec::new()));
                list.len() - 1
            }
        };
        &mut list[at].1
    }

    /// Sets a plain variable and marks it for a delta; false (and no change) when it already holds the value.
    fn set_plain(&mut self, var: usize, bytes: Vec<u8>) -> bool {
        if matches!(&self.values[var], VarValue::Plain(b) if *b == bytes) {
            return false;
        }
        self.values[var] = VarValue::Plain(bytes.into());
        self.dirty_mut(var).clear();
        true
    }

    /// Panics on a variable without a value: one the tables leave to the server (its type has
    /// its own serializer) that the server did not set before the object went out.
    pub fn check(&self) {
        for b in self.behaviours() {
            for (name, ty, value) in b.vars() {
                if matches!(value, VarValue::Plain(bytes) if bytes.is_empty()) {
                    panic!("{}.{name} ({ty}) has no value: the server must set it before the object is sent", b.class);
                }
            }
        }
    }

    /// Every behaviour's variables in order, then the custom sync data (transform state).
    #[cfg(test)]
    pub fn sync(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.sync_into(&mut out);
        out
    }

    /// `sync`, appended to `out`: no Vec of its own for each variable and the tail.
    pub fn sync_into(&self, out: &mut Vec<u8>) {
        for v in self.values.iter() {
            match v {
                VarValue::Plain(b) => out.extend_from_slice(b),
                VarValue::List(items) => {
                    out.extend_from_slice(&(items.len() as u16).to_le_bytes());
                    items.iter().for_each(|i| out.extend_from_slice(i));
                }
            }
        }
        let start = out.len();
        out.extend_from_slice(&self.tail);
        if let Some(at) = self.tail_position {
            if let Some(slot) = out.get_mut(start + at..start + at + 12).filter(|_| at + 12 <= self.tail.len()) {
                for (i, v) in self.position.iter().enumerate() {
                    slot[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
                }
            }
            // A world object container's state ends with its Euler angles after the position.
            if self.tail.len() == at + 24 {
                for (i, v) in crate::transform::euler_angles(self.rotation).iter().enumerate() {
                    out[start + at + 12 + i * 4..start + at + 16 + i * 4].copy_from_slice(&v.to_le_bytes());
                }
            }
        }
        if self.tail.is_empty() {
            out.push(0);
        }
    }

    /// NetworkObject.SceneObject as Netcode writes it.
    pub fn scene_object(&self, scene_handle: i32) -> Vec<u8> {
        let mut w = Writer::new();
        self.scene_object_into(&mut w, scene_handle);
        w.0
    }

    /// `scene_object` appended to `w` (a join writes every object of the world this way).
    pub fn scene_object_into(&self, w: &mut Writer, scene_handle: i32) {
        w.u16(self.flags).u32(self.hash).packed_u64(self.id).packed_u64(self.owner);
        if self.flags & HAS_PARENT != 0 {
            w.packed_u64(self.parent.unwrap_or(0));
        }
        if self.flags & HAS_TRANSFORM != 0 {
            w.vec3(self.position).quat(self.rotation).vec3(self.scale);
        }
        w.i32(scene_handle);
        // The length of the sync data comes before it: written once it is known.
        let at = w.0.len();
        w.i32(0);
        self.sync_into(&mut w.0);
        let len = (w.0.len() - at - 4) as i32;
        w.0[at..at + 4].copy_from_slice(&len.to_le_bytes());
    }
}

// ---- the snapshot's NetObj -----------------------------------------------------------------
//
// An object is written as it was when it held its behaviours and their variables by name (the
// snapshot is the same bytes whatever an object keeps in memory), and read back into a shape:
// the words of the names and types are looked up, not made, and the shape is the one every
// object of that kind has.

struct Variable<'a> {
    name: Sym,
    ty: Sym,
    value: &'a VarValue,
    dirty: Option<&'a Vec<ListEvent>>,
}

impl serde::Serialize for Variable<'_> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut t = s.serialize_struct("NetVar", 4)?;
        t.serialize_field("name", &self.name)?;
        t.serialize_field("ty", &self.ty)?;
        t.serialize_field("value", self.value)?;
        t.serialize_field("dirty", &self.dirty)?;
        t.end()
    }
}

struct Variables<'a>(&'a NetObj, BehaviourRef<'a>);

impl serde::Serialize for Variables<'_> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeSeq;
        let (o, b) = (self.0, self.1);
        let mut t = s.serialize_seq(Some(b.count))?;
        for i in b.first..b.first + b.count {
            let v = &o.shape.vars[i];
            t.serialize_element(&Variable { name: v.name, ty: v.ty, value: &o.values[i], dirty: o.dirty.as_ref().and_then(|d| d.iter().find(|(var, _)| *var as usize == i)).map(|(_, e)| e) })?;
        }
        t.end()
    }
}

struct Behaviours<'a>(&'a NetObj);

impl serde::Serialize for Behaviours<'_> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::{SerializeSeq, SerializeStruct};
        struct One<'a>(&'a NetObj, BehaviourRef<'a>);
        impl serde::Serialize for One<'_> {
            fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                let mut t = s.serialize_struct("NetBehaviour", 3)?;
                t.serialize_field("class", &self.1.class)?;
                t.serialize_field("index", &self.1.index)?;
                t.serialize_field("vars", &Variables(self.0, self.1))?;
                t.end()
            }
        }
        let mut t = s.serialize_seq(Some(self.0.shape.behaviours.len()))?;
        for b in self.0.behaviours() {
            t.serialize_element(&One(self.0, b))?;
        }
        t.end()
    }
}

impl serde::Serialize for NetObj {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        struct Tail<'a>(&'a Bytes);
        impl serde::Serialize for Tail<'_> {
            fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                crate::compact::bytes::serialize(self.0, s)
            }
        }
        let mut t = s.serialize_struct("NetObj", 13)?;
        t.serialize_field("id", &self.id)?;
        t.serialize_field("hash", &self.hash)?;
        t.serialize_field("flags", &self.flags)?;
        t.serialize_field("owner", &self.owner)?;
        t.serialize_field("parent", &self.parent)?;
        t.serialize_field("position", &self.position)?;
        t.serialize_field("rotation", &self.rotation)?;
        t.serialize_field("scale", &self.scale)?;
        t.serialize_field("behaviours", &Behaviours(self))?;
        t.serialize_field("tail", &Tail(&self.tail))?;
        t.serialize_field("tail_position", &self.tail_position)?;
        t.serialize_field("wo", &self.wo)?;
        t.serialize_field("scene", &self.scene)?;
        t.end()
    }
}

#[derive(serde::Deserialize)]
struct FlatVar {
    name: Sym,
    ty: Sym,
    value: VarValue,
    dirty: Option<Vec<ListEvent>>,
}

#[derive(serde::Deserialize)]
struct FlatBehaviour {
    class: Sym,
    index: u16,
    vars: Vec<FlatVar>,
}

#[derive(serde::Deserialize)]
struct Flat {
    id: u64,
    hash: u32,
    flags: u16,
    owner: u64,
    parent: Option<u64>,
    position: [f32; 3],
    rotation: [f32; 4],
    scale: [f32; 3],
    behaviours: Vec<FlatBehaviour>,
    #[serde(with = "crate::compact::bytes")]
    tail: Bytes,
    tail_position: Option<usize>,
    wo: Option<i32>,
    scene: bool,
}

impl<'de> serde::Deserialize<'de> for NetObj {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<NetObj, D::Error> {
        thread_local!(static SIGNATURE: std::cell::RefCell<Signature> = std::cell::RefCell::new(Signature::default()));
        let f = Flat::deserialize(d)?;
        let mut vars = 0;
        // One signature for all the objects this thread reads: no allocation for each.
        let shape = SIGNATURE.with_borrow_mut(|sig| {
            sig.clear();
            for b in &f.behaviours {
                sig.behaviour(b.class, b.index, b.vars.len());
                b.vars.iter().for_each(|v| sig.var(v.name, v.ty));
                vars += b.vars.len();
            }
            Shape::intern(sig, || {
                let pairs: Vec<Vec<(Sym, Sym)>> = f.behaviours.iter().map(|b| b.vars.iter().map(|v| (v.name, v.ty)).collect()).collect();
                Shape::build(f.behaviours.iter().zip(&pairs).map(|(b, p)| (b.class, b.index, p.as_slice())))
            })
        });
        let (mut values, mut pending) = (Vec::with_capacity(vars), Vec::new());
        for v in f.behaviours.into_iter().flat_map(|b| b.vars) {
            if let Some(events) = v.dirty {
                pending.push((values.len() as u16, events));
            }
            values.push(v.value);
        }
        Ok(NetObj {
            id: f.id,
            hash: f.hash,
            flags: f.flags,
            owner: f.owner,
            parent: f.parent,
            position: f.position,
            rotation: f.rotation,
            scale: f.scale,
            shape,
            values: values.into_boxed_slice(),
            dirty: (!pending.is_empty()).then(|| Box::new(pending)),
            tail: f.tail,
            tail_position: f.tail_position,
            wo: f.wo,
            scene: f.scene,
        })
    }
}

/// The scene handle every object goes out with: none (NetworkSceneHandle.IsEmpty), so clients
/// look objects up in the scene they are synchronizing (NetworkSceneManager
/// .GetSceneRelativeInSceneNetworkObject); this server sends no scene events to map one.
const SCENE_HANDLE: i32 = 0;

/// The behaviour classes on the game's network objects that inherit RPCs from `class`, which
/// none carries itself: GroupNetworkBase (the game's C#: ConstructibleProxy and
/// GroupNetworkContainer extend it) and Netcode's NetworkTransform (ClientNetworkTransform).
/// No other class with ServerRpcs has a subclass in the game's code.
pub fn subclasses(class: &str) -> &'static [&'static str] {
    match class {
        "GroupNetworkBase" => &["ConstructibleProxy", "GroupNetworkContainer"],
        "NetworkTransform" => &["ClientNetworkTransform"],
        _ => &[],
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct Net {
    pub objects: crate::idmap::IdMap<u64, NetObj>,
    next_id: u64,
    /// Messages queued this tick, in order.
    pub out: Vec<(Target, u32, Vec<u8>)>,
    #[serde(skip)]
    pub spec: Spec,
    pub scene_handle: i32,
    /// Objects with a variable changed since the last flush (the only ones flush looks at).
    dirty: Vec<u64>,
    /// Objects taken out so far (despawn, or a parked drone): whoever watches for an object to
    /// go looks again only when this moved. Not saved: a restored game looks once.
    #[serde(skip)]
    pub removed: u64,
}

impl Net {
    pub fn new(spec: Spec, first_dynamic_id: u64) -> Net {
        Net { objects: Default::default(), next_id: first_dynamic_id, out: Vec::new(), spec, scene_handle: SCENE_HANDLE, dirty: Vec::new(), removed: 0 }
    }

    /// Adds an in-scene object (fixed id, part of every join, never sent as a spawn).
    pub fn add_scene_object(&mut self, mut o: NetObj) {
        o.scene = true;
        self.objects.insert(o.id, o);
    }

    /// Spawns an object for everyone; returns its network object id.
    pub fn spawn(&mut self, o: NetObj) -> u64 {
        let id = self.spawn_unannounced(o);
        let mut w = Writer::new();
        w.u8(1).bytes(&self.objects[&id].scene_object(self.scene_handle));
        self.out.push((Target::All, protocol::CREATE_OBJECT, w.0));
        id
    }

    /// Registers an object whose observers will receive it once its initial state is ready.
    pub fn spawn_unannounced(&mut self, mut o: NetObj) -> u64 {
        o.check();
        o.id = self.next_id;
        self.next_id += 1;
        let id = o.id;
        // A respawned object (a parked drone) keeps changes made while it was away.
        if o.dirty.is_some() {
            self.dirty.push(id);
        }
        self.objects.insert(id, o);
        id
    }

    pub fn despawn(&mut self, id: u64) {
        self.despawn_for(id, Target::All);
    }

    pub fn despawn_for(&mut self, id: u64, to: Target) {
        if self.objects.shift_remove(&id).is_some() {
            self.removed += 1;
            self.out.push(protocol::destroy_object(id).into_target(to));
        }
    }

    /// ChangeOwnershipMessage (client-server mode): object id, new owner.
    pub fn change_owner(&mut self, object: u64, owner: u64) {
        let Some(o) = self.objects.get_mut(&object) else { return };
        if o.owner == owner {
            return;
        }
        o.owner = owner;
        let mut w = Writer::new();
        w.packed_u64(object).packed_u64(owner);
        self.out.push((Target::All, CHANGE_OWNERSHIP, w.0));
    }

    /// Behaviour index of `class` on an object.
    pub fn behaviour_index(&self, object: u64, class: &str) -> Option<u16> {
        self.objects.get(&object)?.behaviour(class).map(|b| b.index)
    }

    /// Network object id and behaviour index of the first object carrying `class`.
    pub fn find_class(&self, class: &str) -> Option<(u64, u16)> {
        self.objects.values().find_map(|o| o.behaviour(class).map(|b| (o.id, b.index)))
    }

    /// Whether an object carries a behaviour that runs RPCs declared on `class`.
    pub fn carries(&self, object: u64, class: &str) -> bool {
        self.behaviour_index(object, class).is_some() || subclasses(class).iter().any(|c| self.behaviour_index(object, c).is_some())
    }

    /// A ClientRpc by "Class.Method" on the object's behaviour of that class (or of a class
    /// inheriting the method).
    pub fn rpc(&mut self, to: Target, object: u64, name: &str, args: &[Value]) {
        let class = name.split('.').next().unwrap_or("");
        let behaviour = self.behaviour_index(object, class).or_else(|| subclasses(class).iter().find_map(|c| self.behaviour_index(object, c)));
        let Some(behaviour) = behaviour else {
            debug!("rpc {name}: object {object} has no {class}");
            return;
        };
        self.rpc_on(to, object, behaviour, name, args);
    }

    /// A ClientRpc on one behaviour by index: a ServerRpc's answer goes back to the behaviour
    /// that sent it, as `this` does in the game (an object can carry several of one class).
    pub fn rpc_on(&mut self, to: Target, object: u64, behaviour: u16, name: &str, args: &[Value]) {
        let (method, body) = self.spec.encode(name, args);
        if method == 0 {
            return;
        }
        let mut w = Writer::new();
        w.packed_u64(object).packed_u16(behaviour).packed_u32(method).bytes(&body);
        self.out.push((to, CLIENT_RPC, w.0));
    }

    /// A ClientRpc on the scene singleton of the method's class.
    pub fn rpc_scene(&mut self, to: Target, name: &str, args: &[Value]) {
        let class = name.split('.').next().unwrap_or("");
        match self.find_class(class) {
            Some((object, _)) => self.rpc(to, object, name, args),
            None => debug!("rpc {name}: no {class} in the scene"),
        }
    }

    pub fn get(&self, object: u64, class: &str, name: &str) -> Option<&VarValue> {
        self.objects.get(&object)?.behaviour(class)?.var(name)
    }

    pub fn behaviour_var(&self, object: u64, index: u16, name: &str) -> Option<&VarValue> {
        self.objects.get(&object)?.behaviours().find(|b| b.index == index)?.var(name)
    }

    /// Sets a variable of one behaviour by index (objects with several of the same class).
    pub fn set_behaviour_var(&mut self, object: u64, index: u16, name: &str, bytes: Vec<u8>) {
        let Some(o) = self.objects.get_mut(&object) else { return };
        let Some(i) = o.var_of_behaviour(index, name) else { return };
        if o.set_plain(i, bytes) {
            self.dirty.push(object);
        }
    }

    /// Sets a plain variable; a change goes out as a delta at the end of the tick.
    pub fn set(&mut self, object: u64, class: &str, name: &str, bytes: Vec<u8>) {
        let Some(o) = self.objects.get_mut(&object) else {
            debug!("set {class}.{name}: not on object {object}");
            return;
        };
        let Some(i) = o.var_of(class, name) else {
            debug!("set {class}.{name}: not on object {object}");
            return;
        };
        if o.set_plain(i, bytes) {
            self.dirty.push(object);
        }
    }

    /// Applies explicit list events (when the game's own add/remove order matters).
    pub fn list_events(&mut self, object: u64, class: &str, name: &str, events: Vec<ListEvent>) {
        let Some(o) = self.objects.get_mut(&object) else { return };
        let Some(i) = o.var_of(class, name) else { return };
        let VarValue::List(items) = &mut o.values[i] else { return };
        for e in &events {
            match e {
                ListEvent::Add(i) => items.push(i.clone()),
                ListEvent::RemoveAt(at) => {
                    if (*at as usize) < items.len() {
                        items.remove(*at as usize);
                    }
                }
                ListEvent::Value(at, i) => {
                    if let Some(slot) = items.get_mut(*at as usize) {
                        *slot = i.clone();
                    }
                }
                ListEvent::Clear => items.clear(),
            }
        }
        o.dirty_mut(i).extend(events);
        self.dirty.push(object);
    }

    /// NetworkVariableDelta messages for everything changed this tick.
    pub fn flush(&mut self) {
        const RELIABLE_FRAGMENTED_SEQUENCED: i32 = 4;
        // In object order, as a walk over every object would find them.
        let mut dirty = std::mem::take(&mut self.dirty);
        // Reuse the ID buffer for slot numbers until this flush is complete.
        dirty.retain_mut(|id| match self.objects.get_index_of(id) {
            Some(index) => { *id = index as u64; true }
            None => false,
        });
        dirty.sort_unstable();
        dirty.dedup();
        for i in dirty.drain(..) {
            let (_, o) = self.objects.get_index_mut(i as usize).unwrap();
            let Some(pending) = o.dirty.take() else { continue };
            for b in o.shape.behaviours.iter() {
                let range = b.first as usize..(b.first + b.count) as usize;
                if !pending.iter().any(|(v, _)| range.contains(&(*v as usize))) {
                    continue;
                }
                let mut w = Writer::new();
                w.packed_u64(o.id).packed_u16(b.index).i32(RELIABLE_FRAGMENTED_SEQUENCED);
                for i in range {
                    match pending.iter().find(|(v, _)| *v as usize == i) {
                        None => {
                            w.bool(false);
                        }
                        Some((_, events)) => {
                            w.bool(true);
                            match &o.values[i] {
                                // FixedStringSerializer.WriteDelta: a flag byte first, 1 = the
                                // whole string follows. ponytail: always whole; the game sends
                                // only the changed bytes (flag 0) when that is shorter.
                                VarValue::Plain(bytes) if o.shape.vars[i].fixed_string => w.u8(1).bytes(bytes),
                                VarValue::Plain(bytes) => w.bytes(bytes),
                                VarValue::List(_) => w.bytes(&encode_events(events)),
                            };
                        }
                    }
                }
                self.out.push((Target::All, NETWORK_VARIABLE_DELTA, w.0));
            }
        }
        self.dirty = dirty;
    }

    /// Moves an object; objects with a transform state in their sync data get it updated too.
    pub fn move_object(&mut self, object: u64, position: [f32; 3], rotation: Option<[f32; 4]>) {
        if let Some(o) = self.objects.get_mut(&object) {
            o.position = position;
            if let Some(r) = rotation {
                o.rotation = r;
            }
        }
    }
}

trait IntoTarget {
    fn into_target(self, to: Target) -> (Target, u32, Vec<u8>);
}

impl IntoTarget for (u32, Vec<u8>) {
    fn into_target(self, to: Target) -> (Target, u32, Vec<u8>) {
        (to, self.0, self.1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scene object is written as it was when every variable and the tail had a Vec of their
    /// own: the same bytes for every kind of object the tables have (dynamic containers carry
    /// the position and Euler angles in their tail, static ones none).
    #[test]
    fn scene_objects_are_written_in_place_with_the_bytes_they_always_had() {
        let Some(g) = crate::game::test_game(crate::game::tests::save()) else { return };
        let reference = |o: &NetObj| {
            let mut out: Vec<u8> = o.behaviours().flat_map(|b| b.vars().flat_map(|(_, _, v)| v.full())).collect();
            let mut tail = o.tail.to_vec();
            if let Some(at) = o.tail_position {
                if let Some(slot) = tail.get_mut(at..at + 12) {
                    slot.copy_from_slice(&o.position.iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<_>>());
                }
                if let Some(slot) = tail.get_mut(at + 12..).filter(|s| s.len() == 12) {
                    slot.copy_from_slice(&crate::transform::euler_angles(o.rotation).iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<_>>());
                }
            }
            if tail.is_empty() {
                tail.push(0);
            }
            out.extend(tail);
            out
        };
        let (mut kinds, mut with_position) = (std::collections::HashSet::new(), 0);
        for (name, def) in g.data.layouts.iter() {
            let mut o = NetObj::from_layout(&def.layout(7));
            o.id = 5;
            o.owner = 3;
            o.position = [1.5, -2.25, 300.0];
            o.rotation = [0.1, 0.7, -0.2, 0.68];
            let sync = reference(&o);
            assert_eq!(o.sync(), sync, "{name}");
            let mut w = Writer::new();
            w.u8(9);
            o.scene_object_into(&mut w, 0);
            let tail = &w.0[1..];
            assert_eq!(&tail[tail.len() - sync.len()..], &sync[..], "{name}: the object ends with its sync data");
            assert_eq!(tail[tail.len() - sync.len() - 4..tail.len() - sync.len()], (sync.len() as i32).to_le_bytes(), "{name}: and the length before it");
            assert_eq!(o.scene_object(0), tail, "{name}");
            kinds.insert(o.tail.len());
            with_position += o.tail_position.is_some() as usize;
        }
        assert!(kinds.len() >= 3 && with_position > 100, "{} tail sizes, {with_position} objects with a position in their tail", kinds.len());
    }

    /// The shape of an object, and the snapshot it is written as: the same bytes as when an object held
    /// a String for each name and type (the structs of the old layout, here), and the same object
    /// back, on the shape every object of its kind shares.
    #[test]
    fn an_object_is_written_as_when_it_held_its_names_and_reads_back_on_a_shared_shape() {
        #[derive(serde::Serialize)]
        struct OldVar {
            name: String,
            ty: String,
            value: VarValue,
            dirty: Option<Vec<ListEvent>>,
        }
        #[derive(serde::Serialize)]
        struct OldBehaviour {
            class: String,
            index: u16,
            vars: Vec<OldVar>,
        }
        #[derive(serde::Serialize)]
        struct OldObj<'a> {
            id: u64,
            hash: u32,
            flags: u16,
            owner: u64,
            parent: Option<u64>,
            position: [f32; 3],
            rotation: [f32; 4],
            scale: [f32; 3],
            behaviours: Vec<OldBehaviour>,
            #[serde(with = "crate::bincodec::bytes")]
            tail: &'a [u8],
            tail_position: Option<usize>,
            wo: Option<i32>,
            scene: bool,
        }
        let Some(g) = crate::game::test_game(crate::game::tests::save()) else { return };
        let pending = |o: &NetObj, var: usize| o.dirty.as_ref().and_then(|d| d.iter().find(|(v, _)| *v as usize == var)).map(|(_, e)| e.clone());
        let mut shared = std::collections::HashMap::new();
        for (name, def) in g.data.layouts.iter().chain(g.net_data.prefabs.iter()) {
            let mut o = NetObj::from_layout(&def.layout(7));
            (o.id, o.owner, o.parent, o.wo) = (5, 3, Some(9), Some(77));
            o.position = [1.5, -2.25, 300.0];
            // Some variables with a delta waiting: a plain one, a list one, one twice.
            let places: Vec<usize> = (0..o.values.len()).step_by(3).collect();
            for (n, i) in places.into_iter().enumerate() {
                match n % 3 {
                    0 => o.values[i] = VarValue::Plain(vec![n as u8; n % 30].into()),
                    1 => o.values[i] = VarValue::List(vec![vec![1, 2, 3, 4]; n % 4]),
                    _ => {}
                }
                *o.dirty_mut(i) = if n % 2 == 0 { Vec::new() } else { vec![ListEvent::Add(vec![n as u8; 4]), ListEvent::RemoveAt(1), ListEvent::Clear] };
            }
            let old = OldObj {
                id: o.id,
                hash: o.hash,
                flags: o.flags,
                owner: o.owner,
                parent: o.parent,
                position: o.position,
                rotation: o.rotation,
                scale: o.scale,
                behaviours: o
                    .behaviours()
                    .map(|b| OldBehaviour {
                        class: b.class.to_string(),
                        index: b.index,
                        vars: b.vars().map(|(n, t, v)| OldVar { name: n.to_string(), ty: t.to_string(), value: v.clone(), dirty: pending(&o, o.shape.vars.iter().zip(&o.values).position(|(sv, val)| std::ptr::eq(val, v) && sv.name == n).unwrap()) }).collect(),
                    })
                    .collect(),
                tail: &o.tail,
                tail_position: o.tail_position,
                wo: o.wo,
                scene: o.scene,
            };
            let wanted = crate::bincodec::to_vec(&old).unwrap();
            let written = crate::bincodec::to_vec(&o).unwrap();
            assert_eq!(written, wanted, "{name}");
            let back: NetObj = crate::bincodec::from_slice(&written).unwrap();
            assert_eq!(crate::bincodec::to_vec(&back).unwrap(), wanted, "{name}: and back");
            assert!(std::ptr::eq(back.shape, o.shape), "{name}: the shape read is the shape made");
            assert_eq!(format!("{:?}", back.values), format!("{:?}", o.values), "{name}");
            // The same kind of object shares its shape, whatever the layout it came from.
            let key: Vec<(String, u16, usize)> = o.behaviours().map(|b| (b.class.to_string(), b.index, b.vars().count())).collect();
            shared.entry(key).or_insert_with(Vec::new).push(o.shape as *const Shape);
        }
        assert!(shared.values().any(|shapes| shapes.len() > 1 && shapes.iter().all(|s| *s == shapes[0])), "layouts of one kind share a shape");
        let (layouts, kinds) = (g.data.layouts.len() + g.net_data.prefabs.len(), shared.len());
        assert!(kinds * 3 < layouts, "{kinds} kinds of {layouts} layouts");
    }

    /// Every owner-written variable of a class the tables have is one of its variables (a
    /// renamed one would otherwise be refused without a word).
    #[test]
    fn owner_writes_name_variables_of_the_tables() {
        let Some(g) = crate::game::test_game(crate::game::tests::save()) else { return };
        let mut seen = std::collections::HashMap::new();
        let layouts = g.data.layouts.values().map(|l| &l.behaviours).chain(g.net_data.prefabs.values().map(|p| &p.behaviours));
        for b in layouts.flatten() {
            seen.entry(b.class.clone()).or_insert_with(Vec::new).extend(b.vars.iter().map(|v| v.name.clone()));
        }
        for (class, var) in OWNER_WRITES {
            if let Some(vars) = seen.get(*class) {
                assert!(vars.iter().any(|v| v == var), "{class}.{var}");
            }
        }
        assert!(seen.contains_key("PlayerShareState") && seen.contains_key("GroupNetworkContainer"));
    }

    /// A FixedString variable's delta starts with FixedStringSerializer.WriteDelta's flag, 1:
    /// the whole string follows (the real host's text deltas read 01 0b000000 "Parity sign").
    #[test]
    fn fixed_string_deltas_carry_the_whole_string_flag() {
        let Some(mut g) = crate::game::test_game(crate::game::tests::save()) else { return };
        let (object, class, var) = g
            .net
            .objects
            .values()
            .flat_map(|o| o.behaviours().flat_map(move |b| b.vars().map(move |(name, ty, _)| (o.id, b.class, name, ty))))
            .find(|(_, _, _, ty)| ty.starts_with("NetworkVariable<FixedString"))
            .map(|(o, class, name, _)| (o, class, name))
            .expect("a FixedString variable");
        g.net.flush();
        g.net.out.clear();
        g.net.set(object, &class, &var, crate::wire::fixed_string("hi", 128));
        g.net.flush();
        let (_, kind, body) = g.net.out.last().unwrap();
        assert_eq!(*kind, NETWORK_VARIABLE_DELTA);
        assert!(body.windows(8).any(|w| w == [1, 1, 2, 0, 0, 0, b'h', b'i']), "dirty, flag, string: {body:?}");
    }

    #[test]
    fn flush_keeps_object_order_with_duplicates_removed_ids_and_a_reused_buffer() {
        let mut net = Net::new(Spec::default(), 100);
        for id in [90, 10, 50] {
            let o: NetObj = serde_json::from_value(serde_json::json!({
                "id": id, "hash": 0, "flags": 0, "owner": 0, "parent": null,
                "position": [0.0, 0.0, 0.0], "rotation": [0.0, 0.0, 0.0, 1.0], "scale": [1.0, 1.0, 1.0],
                "behaviours": [{"class": "Class", "index": 3, "vars": [{"name": "value", "ty": "NetworkVariable<Int32>",
                    "value": {"Plain": enc::int(id as i32)}, "dirty": []}]}],
                "tail": [], "tail_position": null, "wo": null, "scene": false
            })).unwrap();
            net.objects.insert(id, o);
        }
        net.objects.shift_remove(&10);
        net.dirty = vec![50, 90, 10, 50];
        let capacity = net.dirty.capacity();
        net.out.push((Target::All, 999, vec![11]));
        let expected = |id: u64, value: i32| {
            let mut w = Writer::new();
            w.packed_u64(id).packed_u16(3).i32(4).bool(true).bytes(&enc::int(value));
            (Target::All, NETWORK_VARIABLE_DELTA, w.0)
        };
        net.flush();
        assert_eq!(net.out, vec![(Target::All, 999, vec![11]), expected(90, 90), expected(50, 50)]);
        assert!(net.dirty.is_empty());
        assert_eq!(net.dirty.capacity(), capacity);
        net.out.clear();
        net.set(50, "Class", "value", enc::int(51));
        net.flush();
        assert_eq!(net.out, vec![expected(50, 51)]);
        net.out.clear();
        net.flush();
        assert!(net.out.is_empty());
    }

    #[test]
    fn owner_written_floats_must_be_numbers_in_range() {
        let v3 = |a: f32, b: f32, c: f32| [a, b, c].iter().flat_map(|f| f.to_le_bytes()).collect::<Vec<u8>>();
        let ty = "NetworkVariable<Vector3>";
        assert!(finite(ty, &v3(1.0, -2.5e5, 3.0)));
        assert!(!finite(ty, &v3(1.0, f32::NAN, 3.0)));
        assert!(!finite(ty, &v3(1.0, 1.0e30, 3.0)), "finite, but past what the server's arithmetic holds");
        assert!(!finite("NetworkVariable<Double>", &1.0e300f64.to_le_bytes()));
        assert!(finite("NetworkVariable<Int32>", &i32::MAX.to_le_bytes()), "other types are not floats");
    }
}
