//! What the tests share: a world with a player in it, RPCs sent as a client sends them (the
//! whole way in: batch, message, decode, handler), and what the server sent back.

use std::path::PathBuf;

use serde_json::Value;

pub use crate::game::tests::{approve, join, save};
use crate::game::{test_game, Game};
use crate::net::{Target, VarValue};
use crate::wire::{Reader, Writer};

/// The installed game (PCDS_GAME_DIR, else the default Steam library). Without one the tests
/// that read it fail, unless PCDS_GAME_DIR=none says there is none (CI).
pub fn game_dir() -> Option<PathBuf> {
    let game = match std::env::var("PCDS_GAME_DIR").ok().filter(|v| !v.is_empty()) {
        Some(v) if v == "none" => return None,
        Some(v) => PathBuf::from(v),
        None => PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".local/share/Steam/steamapps/common/The Planet Crafter"),
    };
    assert!(game.join("Planet Crafter_Data/globalgamemanagers").exists(), "no game in {}: set PCDS_GAME_DIR to the game's folder, or to none to skip the tests that read it", game.display());
    Some(game)
}

/// The installed game's data folder.
pub fn game_data() -> Option<PathBuf> {
    Some(game_dir()?.join("Planet Crafter_Data"))
}

/// The installed game's Managed folder (its .NET assemblies).
pub fn managed() -> Option<PathBuf> {
    Some(game_data()?.join("Managed"))
}

/// The test world (an escape pod on Prime) with Ann in it as client 1: the game and Ann's
/// player object. None without game tables.
pub fn world() -> Option<(Game, u64)> {
    let mut g = test_game(save())?;
    let player = join(&mut g, 1, "Ann");
    g.net.out.clear();
    Some((g, player))
}

/// The DLC planets' ground (id, scene dump) in the test tables. Tables read before the server knew
/// the DLC planets, or by a reader that could not, have none: the tests that look at the real
/// ground say so, loudly, and skip (the game's DLC data is never in the repository, so CI has none).
pub fn dlc_grounds(g: &Game, test: &str) -> Vec<(String, crate::PlanetScene)> {
    let grounds: Vec<_> = g.net_data.planets.iter().filter(|p| p.steam_app_id != 0).filter_map(|p| Some((p.id.clone(), g.scenes.planets.get(&p.id)?.clone()))).collect();
    if grounds.is_empty() {
        eprintln!("SKIPPED {test}: the test tables hold no DLC ground (scenes/Humble.json, scenes/Toxicity.json); read them with `pcds extract` from an install that has the DLC planets");
    }
    grounds
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

/// An RPC to the first behaviour of class `name`'s class on `object` (a world object's proxy);
/// GroupNetworkBase's are on its subclass, ConstructibleProxy or GroupNetworkContainer.
pub fn send_on(g: &mut Game, client: u64, object: u64, name: &str, args: &[Value]) {
    let class = name.split('.').next().unwrap();
    let classes: &[&str] = if class == "GroupNetworkBase" { &["ConstructibleProxy", "GroupNetworkContainer"] } else { &[class] };
    let behaviour = classes.iter().find_map(|c| g.net.behaviour_index(object, c)).unwrap_or_else(|| panic!("object {object} has no {class}"));
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
        Some(VarValue::Plain(b)) => b.to_vec(),
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

/// The world saved as the game's save text and loaded again, as a server restart does.
pub fn reload(g: &mut Game) -> Game {
    test_game(crate::save::SaveFile::from_text(&g.snapshot().to_text())).unwrap()
}

/// A saved world object of the snapshot, by id.
pub fn saved_object(g: &mut Game, id: i32) -> Option<Value> {
    g.snapshot().objects.into_iter().find(|o| o["id"] == id)
}

/// The first group (by id) whose network object has a behaviour of every class, and is a
/// building when `building`.
pub fn group_with(g: &Game, classes: &[&str], building: bool) -> String {
    let mut ids: Vec<&String> = g.data.layouts.keys().filter(|k| !k.contains('#')).collect();
    ids.sort();
    ids.into_iter()
        .find(|id| {
            let layout = &g.data.layouts[*id];
            let group = g.data.group(id).map(|i| &g.data.groups[i]);
            group.is_some_and(|gr| gr.has_prefab && gr.constructible == building) && classes.iter().all(|c| layout.behaviours.iter().any(|b| b.class == *c))
        })
        .unwrap_or_else(|| panic!("no group with {classes:?}"))
        .clone()
}

/// One HTTP request read off a test listener: (headers, lowercase names; body).
pub fn read_request(stream: &mut std::net::TcpStream) -> (Vec<(String, String)>, String) {
    use std::io::{BufRead, Read};
    let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
    let (mut headers, mut line) = (Vec::new(), String::new());
    reader.read_line(&mut line).unwrap();
    loop {
        line.clear();
        if reader.read_line(&mut line).unwrap() == 0 || line.trim().is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }
    let length = headers.iter().find(|(k, _)| k == "content-length").map_or(0, |(_, v)| v.parse().unwrap());
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
    (headers, String::from_utf8(body).unwrap())
}
