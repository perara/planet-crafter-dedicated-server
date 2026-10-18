//! What the tests share: a world with a player in it, RPCs sent as a client sends them (the
//! whole way in: batch, message, decode, handler), and what the server sent back.

use serde_json::Value;

pub use crate::game::tests::{join, save};
use crate::game::{test_game, Game};
use crate::net::{Target, VarValue};
use crate::wire::{Reader, Writer};

/// The test world (an escape pod on Prime) with Ann in it as client 1: the game and Ann's
/// player object. None without game tables.
pub fn world() -> Option<(Game, u64)> {
    let mut g = test_game(save())?;
    let player = join(&mut g, 1, "Ann");
    g.net.out.clear();
    Some((g, player))
}

/// An RPC as a client sends it to behaviour `behaviour` of network object `object`.
pub fn send_rpc(g: &mut Game, client: u64, object: u64, behaviour: u16, name: &str, args: &[Value]) {
    let (method, body) = g.net.spec.encode(name, args);
    assert_ne!(method, 0, "{name} is not an RPC the tables can encode");
    let mut w = Writer::new();
    w.packed_u64(client).packed_u64(object).packed_u16(behaviour).packed_u32(method).bytes(&body);
    g.receive(client, &crate::wire::batch(&[(crate::protocol::RPC, w.0)]));
}

/// An RPC of a scene handler ("WorldObjectsHandler.X"), sent to the handler's own object.
pub fn send_scene(g: &mut Game, client: u64, name: &str, args: &[Value]) {
    let class = name.split('.').next().unwrap();
    let (object, behaviour) = g.net.find_class(class).unwrap_or_else(|| panic!("no {class} in the scene"));
    send_rpc(g, client, object, behaviour, name, args);
}

/// An RPC to the first behaviour of class `name`'s class on `object` (a world object's proxy).
pub fn send_on(g: &mut Game, client: u64, object: u64, name: &str, args: &[Value]) {
    let class = name.split('.').next().unwrap();
    let behaviour = g.net.behaviour_index(object, class).unwrap_or_else(|| panic!("object {object} has no {class}"));
    send_rpc(g, client, object, behaviour, name, args);
}

/// One ClientRpc the server queued.
#[derive(Debug, PartialEq)]
pub struct Sent {
    pub to: Target,
    pub object: u64,
    pub behaviour: u16,
    pub args: Vec<Value>,
}

/// The queued ClientRpcs called `name`, in order.
pub fn sent(g: &Game, name: &str) -> Vec<Sent> {
    let decode = |to: Target, body: &[u8]| {
        let mut r = Reader::new(body);
        let (object, behaviour, method) = (r.packed_u64()?, r.packed_u16()?, r.packed_u32()?);
        (g.net.spec.get(method)?.name == name).then(|| Some(Sent { to, object, behaviour, args: g.net.spec.decode(method, r.rest())? }))?
    };
    g.net.out.iter().filter(|(_, kind, _)| *kind == crate::protocol::CLIENT_RPC).filter_map(|(to, _, body)| decode(*to, body)).collect()
}

/// The arguments of the queued ClientRpcs called `name`, with the behaviour each went to.
pub fn client_rpcs(g: &Game, name: &str) -> Vec<(u16, Vec<Value>)> {
    sent(g, name).into_iter().map(|s| (s.behaviour, s.args)).collect()
}

/// The arguments of the one queued ClientRpc called `name`; fails unless exactly one went out.
pub fn one(g: &Game, name: &str) -> Vec<Value> {
    let mut s = sent(g, name);
    assert_eq!(s.len(), 1, "{name} sent {} times", s.len());
    s.remove(0).args
}

/// A plain network variable's bytes.
pub fn var(g: &Game, object: u64, class: &str, name: &str) -> Vec<u8> {
    match g.net.get(object, class, name) {
        Some(VarValue::Plain(b)) => b.clone(),
        v => panic!("{class}.{name} of {object}: {v:?}"),
    }
}

/// A list network variable's items.
pub fn list(g: &Game, object: u64, class: &str, name: &str) -> Vec<Vec<u8>> {
    match g.net.get(object, class, name) {
        Some(VarValue::List(items)) => items.clone(),
        v => panic!("{class}.{name} of {object}: {v:?}"),
    }
}

/// A new world object of a group placed and spawned at `position`: (world object, network object).
pub fn place(g: &mut Game, group: &str, position: [f32; 3]) -> (i32, u64) {
    let group = g.data.group(group).unwrap_or_else(|| panic!("no group {group}"));
    g.create_and_instantiate(group, position, [0.0, 0.0, 0.0, 1.0], false, true, false)
}

/// A new item of a group in an inventory; returns the item's world object.
pub fn give(g: &mut Game, group: &str, inventory: i32) -> i32 {
    let group = g.data.group(group).unwrap_or_else(|| panic!("no group {group}"));
    let id = g.create_world_object(group, 0, true);
    assert!(g.inventory_add(inventory, id, true), "inventory {inventory} took no {}", g.data.groups[group].id);
    id
}

/// A ready client's inventory and equipment.
pub fn backpack(g: &Game, client: u64) -> (i32, i32) {
    let r = g.clients[&client].ready.as_ref().unwrap();
    (r.inventory, r.equipment)
}

/// The group hash of a group id.
pub fn hash(g: &Game, group: &str) -> i32 {
    g.data.groups[g.data.group(group).unwrap_or_else(|| panic!("no group {group}"))].hash
}
