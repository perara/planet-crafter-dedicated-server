//! What clients see: the spawned network objects with their behaviours' variables, and the
//! messages queued for them this tick (spawns, despawns, RPCs, variable deltas), in the order
//! Netcode would send them.

use serde_json::Value;

use crate::data::BehaviourDef;
use crate::gamedata::Layout;
use crate::protocol::{self, CHANGE_OWNERSHIP, CLIENT_RPC, HAS_PARENT, HAS_TRANSFORM, NETWORK_VARIABLE_DELTA};
use crate::rpcspec::Spec;
use crate::wire::Writer;
use crate::world::hex;

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
    /// FixedStringNBytes: i32 length, UTF-8 (capacity N - 3 bytes).
    pub fn fixed_string(s: &str, capacity: usize) -> Vec<u8> {
        let mut bytes = s.as_bytes().to_vec();
        while bytes.len() > capacity - 3 {
            bytes.pop();
            while !bytes.is_empty() && std::str::from_utf8(&bytes).is_err() {
                bytes.pop();
            }
        }
        let mut out = (bytes.len() as i32).to_le_bytes().to_vec();
        out.extend(bytes);
        out
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum ListEvent {
    Add(Vec<u8>),
    RemoveAt(i32),
    Value(i32, Vec<u8>),
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
    Plain(Vec<u8>),
    List(Vec<Vec<u8>>),
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct NetVar {
    pub name: String,
    pub ty: String,
    pub value: VarValue,
    /// Pending delta: the new value, or the list events since the last flush.
    dirty: Option<Vec<ListEvent>>,
}

impl NetVar {
    fn full(&self) -> Vec<u8> {
        match &self.value {
            VarValue::Plain(b) => b.clone(),
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

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct NetBehaviour {
    pub class: String,
    pub index: u16,
    pub vars: Vec<NetVar>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
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
    pub behaviours: Vec<NetBehaviour>,
    pub tail: Vec<u8>,
    pub tail_position: Option<usize>,
    /// The world object it stands for.
    pub wo: Option<i32>,
    /// In-scene objects come with the scene and are never spawned or despawned.
    pub scene: bool,
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
    VarValue::Plain(bytes)
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
    let Some(b) = o.behaviours.iter_mut().find(|b| b.index == behaviour) else { return false };
    // Read first, applied only when every value is good: a NaN or infinity is neither kept nor
    // passed on (a float the server keeps could make the snapshot unreadable).
    let mut writes = Vec::new();
    for (i, v) in b.vars.iter().enumerate() {
        let Some(dirty) = r.bool() else { break };
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
        b.vars[i].value = VarValue::Plain(bytes);
    }
    true
}

/// Whether a value of a floating-point type (float, double, vectors, quaternion, color) is all
/// numbers; values of other types are.
fn finite(ty: &str, bytes: &[u8]) -> bool {
    match ty.strip_prefix("NetworkVariable<").and_then(|t| t.strip_suffix('>')) {
        Some("Double") => bytes.chunks_exact(8).all(|c| f64::from_le_bytes(c.try_into().unwrap()).is_finite()),
        Some("Single" | "Vector2" | "Vector3" | "Vector4" | "Quaternion" | "Color") => bytes.chunks_exact(4).all(|c| f32::from_le_bytes(c.try_into().unwrap()).is_finite()),
        _ => true,
    }
}

impl NetObj {
    pub fn from_layout(layout: &Layout) -> NetObj {
        NetObj {
            id: 0,
            hash: layout.hash,
            flags: layout.flags,
            owner: 0,
            parent: None,
            position: [0.0; 3],
            rotation: [0.0, 0.0, 0.0, 1.0],
            scale: layout.scale,
            behaviours: behaviours(&layout.behaviours),
            tail: layout.tail.clone(),
            tail_position: layout.tail_position,
            wo: None,
            scene: false,
        }
    }

    pub fn behaviour(&self, class: &str) -> Option<&NetBehaviour> {
        self.behaviours.iter().find(|b| b.class == class)
    }

    fn var_mut(&mut self, class: &str, name: &str) -> Option<&mut NetVar> {
        self.behaviours.iter_mut().find(|b| b.class == class)?.vars.iter_mut().find(|v| v.name == name)
    }

    /// Sets a variable's initial value, without a delta (before the object is spawned).
    pub fn init(&mut self, class: &str, name: &str, bytes: Vec<u8>) {
        if let Some(v) = self.var_mut(class, name) {
            v.value = value_of(&v.ty, bytes);
        }
    }

    pub fn init_list(&mut self, class: &str, name: &str, items: Vec<Vec<u8>>) {
        if let Some(v) = self.var_mut(class, name) {
            v.value = VarValue::List(items);
        }
    }

    /// Panics on a variable without a value: one the tables leave to the server (its type has
    /// its own serializer) that the server did not set before the object went out.
    pub fn check(&self) {
        for b in &self.behaviours {
            for v in &b.vars {
                if matches!(&v.value, VarValue::Plain(bytes) if bytes.is_empty()) {
                    panic!("{}.{} ({}) has no value: the server must set it before the object is sent", b.class, v.name, v.ty);
                }
            }
        }
    }

    /// Every behaviour's variables in order, then the custom sync data (transform state).
    pub fn sync(&self) -> Vec<u8> {
        let mut out: Vec<u8> = self.behaviours.iter().flat_map(|b| b.vars.iter().flat_map(|v| v.full())).collect();
        let mut tail = self.tail.clone();
        if let Some(at) = self.tail_position {
            if let Some(slot) = tail.get_mut(at..at + 12) {
                slot.copy_from_slice(&self.position.iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<_>>());
            }
            // A world object container's state ends with its Euler angles after the position.
            if let Some(slot) = tail.get_mut(at + 12..).filter(|s| s.len() == 12) {
                slot.copy_from_slice(&crate::transform::euler_angles(self.rotation).iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<_>>());
            }
        }
        if tail.is_empty() {
            tail.push(0);
        }
        out.extend(tail);
        out
    }

    /// NetworkObject.SceneObject as Netcode writes it.
    pub fn scene_object(&self, scene_handle: i32) -> Vec<u8> {
        let mut w = Writer::new();
        w.u16(self.flags).u32(self.hash).packed_u64(self.id).packed_u64(self.owner);
        if self.flags & HAS_PARENT != 0 {
            w.packed_u64(self.parent.unwrap_or(0));
        }
        if self.flags & HAS_TRANSFORM != 0 {
            w.vec3(self.position).quat(self.rotation).vec3(self.scale);
        }
        let sync = self.sync();
        w.i32(scene_handle).i32(sync.len() as i32).bytes(&sync);
        w.0
    }
}

pub fn behaviours(defs: &[BehaviourDef]) -> Vec<NetBehaviour> {
    defs.iter()
        .map(|b| NetBehaviour {
            class: b.class.clone(),
            index: b.behaviour_id,
            vars: b
                .vars
                .iter()
                .map(|v| {
                    // No bytes: the server sets the value (an empty value is none, see check).
                    let bytes = v.bytes.as_deref().map(hex).unwrap_or_default();
                    NetVar { name: v.name.clone(), ty: v.ty.clone(), value: value_of(&v.ty, bytes), dirty: None }
                })
                .collect(),
        })
        .collect()
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
}

impl Net {
    pub fn new(spec: Spec, first_dynamic_id: u64) -> Net {
        Net { objects: Default::default(), next_id: first_dynamic_id, out: Vec::new(), spec, scene_handle: SCENE_HANDLE, dirty: Vec::new() }
    }

    /// Adds an in-scene object (fixed id, part of every join, never sent as a spawn).
    pub fn add_scene_object(&mut self, mut o: NetObj) {
        o.scene = true;
        self.objects.insert(o.id, o);
    }

    /// Spawns an object for everyone; returns its network object id.
    pub fn spawn(&mut self, mut o: NetObj) -> u64 {
        o.check();
        o.id = self.next_id;
        self.next_id += 1;
        let mut w = Writer::new();
        w.u8(1).bytes(&o.scene_object(self.scene_handle));
        self.out.push((Target::All, protocol::CREATE_OBJECT, w.0));
        let id = o.id;
        // A respawned object (a parked drone) keeps changes made while it was away.
        if o.behaviours.iter().any(|b| b.vars.iter().any(|v| v.dirty.is_some())) {
            self.dirty.push(id);
        }
        self.objects.insert(id, o);
        id
    }

    pub fn despawn(&mut self, id: u64) {
        if self.objects.shift_remove(&id).is_some() {
            self.out.push(protocol::destroy_object(id).into_target(Target::All));
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
            eprintln!("rpc {name}: object {object} has no {class}");
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
            None => eprintln!("rpc {name}: no {class} in the scene"),
        }
    }

    pub fn get(&self, object: u64, class: &str, name: &str) -> Option<&VarValue> {
        Some(&self.objects.get(&object)?.behaviour(class)?.vars.iter().find(|v| v.name == name)?.value)
    }

    pub fn behaviour_var(&self, object: u64, index: u16, name: &str) -> Option<&VarValue> {
        Some(&self.objects.get(&object)?.behaviours.iter().find(|b| b.index == index)?.vars.iter().find(|v| v.name == name)?.value)
    }

    /// Sets a variable of one behaviour by index (objects with several of the same class).
    pub fn set_behaviour_var(&mut self, object: u64, index: u16, name: &str, bytes: Vec<u8>) {
        let Some(v) = self.objects.get_mut(&object).and_then(|o| o.behaviours.iter_mut().find(|b| b.index == index)).and_then(|b| b.vars.iter_mut().find(|v| v.name == name)) else { return };
        if matches!(&v.value, VarValue::Plain(b) if *b == bytes) {
            return;
        }
        v.value = VarValue::Plain(bytes);
        v.dirty = Some(Vec::new());
        self.dirty.push(object);
    }

    /// Sets a plain variable; a change goes out as a delta at the end of the tick.
    pub fn set(&mut self, object: u64, class: &str, name: &str, bytes: Vec<u8>) {
        let Some(v) = self.objects.get_mut(&object).and_then(|o| o.var_mut(class, name)) else {
            eprintln!("set {class}.{name}: not on object {object}");
            return;
        };
        if matches!(&v.value, VarValue::Plain(b) if *b == bytes) {
            return;
        }
        v.value = VarValue::Plain(bytes);
        v.dirty = Some(Vec::new());
        self.dirty.push(object);
    }

    /// Applies explicit list events (when the game's own add/remove order matters).
    pub fn list_events(&mut self, object: u64, class: &str, name: &str, events: Vec<ListEvent>) {
        let Some(v) = self.objects.get_mut(&object).and_then(|o| o.var_mut(class, name)) else { return };
        let VarValue::List(items) = &mut v.value else { return };
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
        v.dirty.get_or_insert_with(Vec::new).extend(events);
        self.dirty.push(object);
    }

    /// NetworkVariableDelta messages for everything changed this tick.
    pub fn flush(&mut self) {
        const RELIABLE_FRAGMENTED_SEQUENCED: i32 = 4;
        let mut messages = Vec::new();
        // In object order, as a walk over every object would find them.
        let mut dirty = std::mem::take(&mut self.dirty);
        let mut at: Vec<usize> = dirty.drain(..).filter_map(|id| self.objects.get_index_of(&id)).collect();
        at.sort_unstable();
        at.dedup();
        self.dirty = dirty;
        for i in at {
            let (_, o) = self.objects.get_index_mut(i).unwrap();
            for b in &mut o.behaviours {
                if b.vars.iter().all(|v| v.dirty.is_none()) {
                    continue;
                }
                let mut w = Writer::new();
                w.packed_u64(o.id).packed_u16(b.index).i32(RELIABLE_FRAGMENTED_SEQUENCED);
                for v in &mut b.vars {
                    match v.dirty.take() {
                        None => {
                            w.bool(false);
                        }
                        Some(events) => {
                            w.bool(true);
                            match &v.value {
                                // FixedStringSerializer.WriteDelta: a flag byte first, 1 = the
                                // whole string follows. ponytail: always whole; the game sends
                                // only the changed bytes (flag 0) when that is shorter.
                                VarValue::Plain(bytes) if v.ty.starts_with("NetworkVariable<FixedString") => w.u8(1).bytes(bytes),
                                VarValue::Plain(bytes) => w.bytes(bytes),
                                VarValue::List(_) => w.bytes(&encode_events(&events)),
                            };
                        }
                    }
                }
                messages.push((Target::All, NETWORK_VARIABLE_DELTA, w.0));
            }
        }
        self.out.extend(messages);
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
            .flat_map(|o| o.behaviours.iter().flat_map(move |b| b.vars.iter().map(move |v| (o.id, b, v))))
            .find(|(_, _, v)| v.ty.starts_with("NetworkVariable<FixedString"))
            .map(|(o, b, v)| (o, b.class.clone(), v.name.clone()))
            .expect("a FixedString variable");
        g.net.flush();
        g.net.out.clear();
        g.net.set(object, &class, &var, enc::fixed_string("hi", 128));
        g.net.flush();
        let (_, kind, body) = g.net.out.last().unwrap();
        assert_eq!(*kind, NETWORK_VARIABLE_DELTA);
        assert!(body.windows(8).any(|w| w == [1, 1, 2, 0, 0, 0, b'h', b'i']), "dirty, flag, string: {body:?}");
    }

    #[test]
    fn fixed_strings_cut_at_capacity() {
        assert_eq!(enc::fixed_string("abc", 128), vec![3, 0, 0, 0, b'a', b'b', b'c']);
        assert_eq!(enc::fixed_string(&"x".repeat(200), 128).len(), 4 + 125);
    }
}
