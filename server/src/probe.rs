//! A scripted player for differential tests: joins a server over direct IP like the game does,
//! sends the RPCs a script lists, and prints everything the server sends back as JSON lines.
//! Run the same script against the real game (the plugin server) and this server, then compare.
//!
//! Usage: pcds probe HOST:PORT DATA_DIR SCRIPT.json   (PROBE_NAME=... renames the player)
//! Script: {"name": "Probe", "steps": [{"wait": MS}, {"rpc": "Class.MethodServerRpc",
//! "on": TARGET, "args": [...]}]}. TARGET is a scene behaviour class ("InventoriesHandler"),
//! "player:Class" for a behaviour on our own player, or [object id, behaviour index].
//! "$player" in args stands for our player's object id, "$client" for our client id,
//! "$inventory" / "$equipment" for the inventory ids the server gave us, "#Iron" for the stable
//! hash of a group (or planet) id.
//! A step may capture a value from what the server answered, for later steps as "$name":
//! "capture": {"var": "box", "rpc": "WorldObjectsHandler.CreateAndInstantiateWorldObjectClientRpc",
//! "arg": 0} (an RPC argument) or {"var": "crate", "create": true} (the id of the first object
//! spawned). Targets may then be ["$box", BEHAVIOUR_INDEX]. "$hash:398a5515" is the first object
//! of that prefab hash the server spawned for us. "ready": false skips the
//! PlayerReady the game sends first, so the player stays as it spawned; "id": N makes the player
//! claim Steam id N there. A step's "tag" is printed with its call (which sweep row it exercises);
//! a step whose "$name" was never captured is skipped (printed with "skipped").
//! {"transform": TARGET, "position": [x, y, z]} moves an object the player owns (a
//! NetworkTransformMessage); "transforms": ["$var", ...] prints the transforms the server relays
//! for those objects. "$near:HASH@x,y,z" is the object of that prefab spawned nearest to the
//! point (within 20 m): something a machine made, to harvest.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::data::NetData;
use crate::protocol::{self, CLIENT_RPC, CONNECTION_APPROVED, CONNECTION_REQUEST, CREATE_OBJECT, DESTROY_OBJECT, NETWORK_TRANSFORM, NETWORK_VARIABLE_DELTA, RPC, TIME_SYNC};
use crate::rpcspec::Spec;
use crate::util::to_hex;
use crate::utp::{Event, Utp};
use crate::wire::{self, Reader, Writer};
use crate::world::hex;

struct Session {
    utp: Utp,
    token: crate::utp::Token,
    started: Instant,
    client: u64,
    player: u64,
    inventory: i64,
    equipment: i64,
    vars: std::collections::HashMap<String, Value>,
    /// Network objects whose transforms are printed.
    watched: Vec<u64>,
    /// Every object spawned for us: (id, prefab hash, position), for "$near:".
    spawned: Vec<(u64, u32, [f32; 3])>,
}

fn print(started: Instant, mut line: Value) {
    line["t"] = json!(started.elapsed().as_millis() as u64);
    println!("{line}");
}

impl Session {
    fn send(&mut self, messages: &[(u32, Vec<u8>)]) {
        self.utp.send(&self.token, &wire::batch(messages));
        self.utp.flush();
    }

    /// Receives for `ms`, printing every message; returns them too.
    fn pump(&mut self, ms: u64, spec: &Spec) -> Vec<(u32, Vec<u8>)> {
        let mut got = Vec::new();
        let end = Instant::now() + Duration::from_millis(ms);
        while Instant::now() < end {
            for event in self.utp.poll() {
                match event {
                    Event::Data(_, batch) => {
                        for (kind, body) in wire::messages(&batch) {
                            self.describe(kind, body, spec);
                            got.push((kind, body.to_vec()));
                        }
                    }
                    Event::Disconnected(_) => {
                        print(self.started, json!({"msg": "disconnected"}));
                        std::process::exit(1);
                    }
                    Event::Connected(..) => {}
                }
            }
            self.utp.flush();
            std::thread::sleep(Duration::from_millis(2));
        }
        got
    }

    fn describe(&mut self, kind: u32, body: &[u8], spec: &Spec) {
        let line = match kind {
            CONNECTION_APPROVED => match protocol::read_approved(body) {
                Some((owner, objects)) => {
                    self.client = owner;
                    let mut by_hash: std::collections::BTreeMap<String, usize> = Default::default();
                    for o in &objects {
                        *by_hash.entry(format!("{:08x}", o.hash)).or_default() += 1;
                        self.vars.entry(format!("hash:{:08x}", o.hash)).or_insert(json!(o.id));
                        // "$near:" finds the world's own objects too (a script on a real save).
                        self.spawned.push((o.id, o.hash, o.position.unwrap_or([f32::MAX; 3])));
                    }
                    // Every object the approval carries, as a spawn would show it (PROBE_OBJECTS=0: counts only).
                    if std::env::var("PROBE_OBJECTS").as_deref() != Ok("0") {
                        for o in &objects {
                            print(self.started, object_line(o, "approved object"));
                        }
                    }
                    json!({"msg": "approved", "client": owner, "objects": objects.len(), "hashes": by_hash, "header": approved_header(body)})
                }
                None => json!({"msg": "approved", "error": "unreadable"}),
            },
            CLIENT_RPC => {
                let mut r = Reader::new(body);
                let (object, behaviour, method) = (r.packed_u64(), r.packed_u16(), r.packed_u32());
                let args = method.and_then(|m| spec.decode(m, r.rest()));
                let name = method.and_then(|m| spec.get(m)).map(|s| s.name.clone());
                if name.as_deref() == Some("PlayerMainController.InitPlayerClientRpc") {
                    if let Some(a) = &args {
                        self.inventory = a[0].as_i64().unwrap_or(0);
                        self.equipment = a[1].as_i64().unwrap_or(0);
                    }
                }
                json!({"msg": "rpc", "name": name.unwrap_or_else(|| format!("{method:?}")), "object": object, "behaviour": behaviour,
                       "args": args.unwrap_or_else(|| vec![json!(to_hex(r.rest()))])})
            }
            CREATE_OBJECT => {
                let mut r = Reader::new(body);
                r.u8();
                match protocol::read_scene_object(&mut r) {
                    Some(o) => {
                        self.vars.entry(format!("hash:{:08x}", o.hash)).or_insert(json!(o.id));
                        self.spawned.push((o.id, o.hash, o.position.unwrap_or([f32::MAX; 3])));
                        object_line(&o, "create")
                    }
                    None => json!({"msg": "create", "hex": to_hex(body)}),
                }
            }
            DESTROY_OBJECT => {
                let id = Reader::new(body).packed_u64();
                self.spawned.retain(|(o, _, _)| Some(*o) != id);
                json!({"msg": "destroy", "id": id})
            }
            NETWORK_VARIABLE_DELTA => {
                let mut r = Reader::new(body);
                json!({"msg": "delta", "object": r.packed_u64(), "behaviour": r.packed_u16(), "hex": to_hex(r.rest())})
            }
            // Transforms of the objects the script watches ("transforms": ["$var", ...]).
            NETWORK_TRANSFORM => match crate::transform::update(body) {
                Some(u) if self.watched.contains(&u.object) => json!({"msg": "transform", "object": u.object, "position": u.position}),
                _ => return,
            },
            TIME_SYNC => return,
            _ => json!({"msg": kind, "hex": to_hex(body)}),
        };
        print(self.started, line);
    }
}

/// ConnectionApprovedMessage before its objects: message versions, the client's id, the
/// server tick (time: left out) and the connected clients.
fn approved_header(body: &[u8]) -> Value {
    let mut r = Reader::new(body);
    let mut versions = Vec::new();
    for _ in 0..r.packed_i32().unwrap_or(0) {
        versions.push(json!([r.u32(), r.packed_i32()]));
    }
    let owner = r.packed_u64();
    r.packed_i32();
    let clients: Vec<Option<u64>> = (0..r.i32().unwrap_or(0)).map(|_| r.u64()).collect();
    json!({"versions": versions, "owner": owner, "clients": clients})
}

fn object_line(o: &protocol::SceneObjectInfo, msg: &str) -> Value {
    json!({"msg": msg, "id": o.id, "hash": format!("{:08x}", o.hash), "owner": o.owner, "flags": o.flags,
           "position": o.position, "rotation": o.rotation, "sync": to_hex(&o.sync)})
}

/// Resolves a script target to (object id, behaviour index).
fn target(on: &Value, data: &NetData, player: u64) -> (u64, u16) {
    if let Some([o, b]) = on.as_array().map(|a| a.as_slice()) {
        return (o.as_u64().unwrap_or(0), b.as_u64().unwrap_or(0) as u16);
    }
    let name = on.as_str().unwrap_or("");
    if let Some(class) = name.strip_prefix("player:") {
        let b = data.prefabs["Player"].behaviours.iter().find(|b| b.class == class).unwrap_or_else(|| panic!("no {class} on the player"));
        return (player, b.behaviour_id);
    }
    data.scene_objects
        .iter()
        .find_map(|o| o.behaviours.iter().find(|b| b.class == name).map(|b| (o.id, b.behaviour_id)))
        .unwrap_or_else(|| panic!("no scene behaviour {name}"))
}

fn substitute(v: &Value, s: &Session) -> Value {
    match v {
        Value::String(t) if t == "$player" => json!(s.player),
        Value::String(t) if t == "$client" => json!(s.client),
        Value::String(t) if t == "$inventory" => json!(s.inventory),
        Value::String(t) if t == "$equipment" => json!(s.equipment),
        Value::String(t) if t.starts_with('$') && s.vars.contains_key(&t[1..]) => s.vars[&t[1..]].clone(),
        Value::String(t) if t.starts_with('#') => json!(crate::data::stable_hash(&t[1..])),
        // "$near:HASH@x,y,z": the object of that prefab spawned nearest to the point, within 20 m.
        Value::String(t) if t.starts_with("$near:") => near(s, &t[6..]).map(|id| json!(id)).unwrap_or_else(|| v.clone()),
        Value::Array(items) => Value::Array(items.iter().map(|i| substitute(i, s)).collect()),
        other => other.clone(),
    }
}

fn near(s: &Session, spec: &str) -> Option<u64> {
    let (hash, at) = spec.split_once('@')?;
    let hash = u32::from_str_radix(hash, 16).ok()?;
    let p: Vec<f32> = at.split(',').filter_map(|x| x.parse().ok()).collect();
    let d2 = |q: &[f32; 3]| (0..3).map(|i| (q[i] - p.get(i).copied().unwrap_or(0.0)).powi(2)).sum::<f32>();
    s.spawned.iter().filter(|(_, h, q)| *h == hash && d2(q) < 400.0).min_by(|a, b| d2(&a.2).total_cmp(&d2(&b.2))).map(|(id, _, _)| *id)
}

/// The first "$name" left after substitution (a capture that failed), if any.
fn unresolved(v: &Value) -> Option<String> {
    match v {
        Value::String(t) if t.starts_with('$') => Some(t.clone()),
        Value::Array(items) => items.iter().find_map(unresolved),
        _ => None,
    }
}

pub fn run(args: &[String]) {
    let [addr, dir, script] = args else {
        eprintln!("usage: pcds probe HOST:PORT DATA_DIR SCRIPT.json");
        std::process::exit(2);
    };
    let (data, spec) = crate::network_tables(dir);
    let request = hex(std::fs::read_to_string(format!("{dir}/connection-request.hex")).expect("connection-request.hex").trim());
    let script: Value = serde_json::from_str(&std::fs::read_to_string(script).expect("script")).expect("script JSON");
    let addr: SocketAddr = addr.parse().expect("HOST:PORT");
    let (utp, token) = Utp::connect("0.0.0.0:0".parse().unwrap(), addr, Duration::from_secs(10)).unwrap_or_else(|e| panic!("{addr}: {e}"));
    let mut s = Session { utp, token, started: Instant::now(), client: 0, player: 0, inventory: 0, equipment: 0, vars: Default::default(), watched: Vec::new(), spawned: Vec::new() };
    s.send(&[(CONNECTION_REQUEST, request)]);
    let player_hash = data.prefabs["Player"].hash;
    let deadline = Instant::now() + Duration::from_secs(30);
    while s.player == 0 {
        assert!(Instant::now() < deadline, "no player object from the server");
        for (kind, body) in s.pump(100, &spec) {
            let objects = match kind {
                CONNECTION_APPROVED => protocol::read_approved(&body).map(|(_, o)| o).unwrap_or_default(),
                CREATE_OBJECT => protocol::read_scene_object(&mut Reader::new(&body[1..])).into_iter().collect(),
                _ => Vec::new(),
            };
            if let Some(o) = objects.iter().find(|o| o.hash == player_hash && o.owner == s.client && s.client != 0) {
                s.player = o.id;
            }
        }
    }
    print(s.started, json!({"msg": "joined", "client": s.client, "player": s.player}));
    // PROBE_NAME gives the player another name (a fresh character on both servers).
    let name = std::env::var("PROBE_NAME").unwrap_or_else(|_| script["name"].as_str().unwrap_or("Probe").to_string());
    let mut steps = Vec::new();
    if script["ready"].as_bool() != Some(false) {
        // "id": the Steam id the player claims (0, the default, is what a direct-IP game sends).
        let id = script["id"].as_u64().unwrap_or(0);
        steps = vec![json!({"rpc": "PlayerMainController.PlayerReadyServerRpc", "on": "player:PlayerMainController", "args": [id, name]}), json!({"wait": 3000})];
    }
    steps.extend(script["steps"].as_array().cloned().unwrap_or_default());
    for step in steps {
        if let Some(ms) = step["wait"].as_u64() {
            s.pump(ms, &spec);
            continue;
        }
        if let Some(names) = script["transforms"].as_array() {
            s.watched = names.iter().filter_map(|n| substitute(n, &s).as_u64()).collect();
        }
        // {"transform": TARGET, "position": [x, y, z]}: the owner moving an object (a
        // NetworkTransformMessage with the three position axes, as a ClientNetworkTransform sends).
        if !step["transform"].is_null() {
            let on = substitute(&step["transform"], &s);
            if let Some(var) = unresolved(&on) {
                print(s.started, json!({"msg": "call", "name": "transform", "skipped": var, "tag": step["tag"]}));
                continue;
            }
            let (object, behaviour) = target(&on, &data, s.player);
            let mut w = Writer::new();
            w.packed_u64(object).packed_i32(behaviour as i32).packed_u32(2 | 4 | 8).packed_i32(0);
            for f in step["position"].as_array().into_iter().flatten() {
                w.bytes(&(f.as_f64().unwrap_or(0.0) as f32).to_le_bytes());
            }
            print(s.started, json!({"msg": "call", "name": "transform", "object": object, "behaviour": behaviour, "args": step["position"], "tag": step["tag"]}));
            s.send(&[(NETWORK_TRANSFORM, w.0)]);
            s.pump(step["after"].as_u64().unwrap_or(300), &spec);
            continue;
        }
        let Some(name) = step["rpc"].as_str() else { continue };
        let on = substitute(&step["on"], &s);
        let args: Vec<Value> = step["args"].as_array().cloned().unwrap_or_default().iter().map(|a| substitute(a, &s)).collect();
        // A value an earlier step failed to capture: the call cannot be made (both servers alike).
        if let Some(var) = unresolved(&on).or_else(|| args.iter().find_map(unresolved)) {
            print(s.started, json!({"msg": "call", "name": name, "skipped": var, "tag": step["tag"]}));
            continue;
        }
        let (object, behaviour) = target(&on, &data, s.player);
        let (method, body) = spec.encode(name, &args);
        let mut w = Writer::new();
        w.packed_u64(s.client).packed_u64(object).packed_u16(behaviour).packed_u32(method).bytes(&body);
        print(s.started, json!({"msg": "call", "name": name, "object": object, "behaviour": behaviour, "args": args, "tag": step["tag"]}));
        s.send(&[(RPC, w.0)]);
        let got = s.pump(step["after"].as_u64().unwrap_or(300), &spec);
        let capture = &step["capture"];
        if let Some(var) = capture["var"].as_str() {
            let value = got.iter().find_map(|(kind, body)| match *kind {
                CREATE_OBJECT if capture["create"].as_bool() == Some(true) => protocol::read_scene_object(&mut Reader::new(&body[1..])).map(|o| json!(o.id)),
                CLIENT_RPC => {
                    let mut r = Reader::new(body);
                    let (_, _, method) = (r.packed_u64()?, r.packed_u16()?, r.packed_u32()?);
                    (spec.get(method)?.name == capture["rpc"].as_str()?).then(|| spec.decode(method, r.rest())).flatten().and_then(|a| a.get(capture["arg"].as_u64().unwrap_or(0) as usize).cloned())
                }
                _ => None,
            });
            match value {
                Some(v) => {
                    print(s.started, json!({"msg": "captured", "var": var, "value": v}));
                    s.vars.insert(var.to_string(), v);
                }
                None => print(s.started, json!({"msg": "capture failed", "var": var})),
            }
        }
    }
    s.utp.disconnect(&s.token);
}
