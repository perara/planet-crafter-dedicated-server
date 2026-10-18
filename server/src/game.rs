//! The game server proper: the world model (world objects, inventories, players) as the game's
//! server-side handlers keep it, what clients see of it (net), and the dispatch of client
//! messages. The handlers themselves are ported class by class into sibling modules
//! (objects, inventories, players, ...), each an `impl Game` block.

use std::collections::HashMap;

use indexmap::{IndexMap, IndexSet};
use serde_json::{json, Value};

use crate::data::{stable_hash, NetData, ObjectDef};
use crate::gamedata::{GameData, Layout};
use crate::idmap::{IdMap, IdSet};
use crate::model::{Inventory, WorldObject};
use crate::net::{self, ListEvent, Net, NetObj, Target};
use crate::protocol::{self, CONNECTION_APPROVED, CONNECTION_REQUEST, NETWORK_TRANSFORM, NETWORK_VARIABLE_DELTA};
use crate::rpcspec::Spec;
use crate::save::SaveFile;
use crate::wire::{self, Reader, Writer};

/// Network object ids from here on are ours to hand out (the scene uses the ones below).
const FIRST_DYNAMIC_ID: u64 = 22;
/// GameConfig.startPlayerInventorySize / startPlayerEquipmentSize.
pub const START_INVENTORY_SIZE: i32 = 12;
pub const START_EQUIPMENT_SIZE: i32 = 4;

/// RPC arguments in signature order, with typed accessors.
pub struct Args(pub Vec<Value>);

impl Args {
    pub fn i(&self, n: usize) -> i32 {
        self.0.get(n).and_then(Value::as_i64).unwrap_or(0) as i32
    }
    pub fn u(&self, n: usize) -> u64 {
        self.0.get(n).and_then(Value::as_u64).unwrap_or(0)
    }
    pub fn f(&self, n: usize) -> f32 {
        self.0.get(n).and_then(Value::as_f64).unwrap_or(0.0) as f32
    }
    pub fn b(&self, n: usize) -> bool {
        self.0.get(n).and_then(Value::as_bool).unwrap_or(false)
    }
    pub fn s(&self, n: usize) -> Option<String> {
        self.0.get(n).and_then(Value::as_str).map(str::to_string)
    }
    pub fn v3(&self, n: usize) -> [f32; 3] {
        let v = self.0.get(n);
        [0, 1, 2].map(|i| v.and_then(|v| v.get(i)).and_then(Value::as_f64).unwrap_or(0.0) as f32)
    }
    pub fn q(&self, n: usize) -> [f32; 4] {
        let v = self.0.get(n);
        [0, 1, 2, 3].map(|i| v.and_then(|v| v.get(i)).and_then(Value::as_f64).unwrap_or(0.0) as f32)
    }
    /// An optional int[]; None when absent (null).
    pub fn ints(&self, n: usize) -> Option<Vec<i32>> {
        self.0.get(n)?.as_array().map(|a| a.iter().filter_map(Value::as_i64).map(|v| v as i32).collect())
    }
}

/// A connected Netcode client and its player.
pub struct Client {
    pub id: u64,
    pub approved: bool,
    /// SteamID proven by the transport (Steam networking), else None.
    pub verified_steam_id: Option<u64>,
    /// May its reported SteamID be believed (direct IP from this machine)?
    pub trusted: bool,
    pub player: Option<u64>,
    /// The player as the save knows them, once PlayerReady arrived.
    pub ready: Option<PlayerState>,
    /// Direct-IP clients' address (Steam players have none).
    pub ip: Option<String>,
    pub since: std::time::Instant,
}

#[derive(Clone, Debug)]
pub struct PlayerState {
    pub id: u64,
    pub name: String,
    pub inventory: i32,
    pub equipment: i32,
    pub position: [f32; 3],
    pub rotation: [f32; 4],
    pub planet: String,
    pub total_crafted: u32,
    pub total_tokens: u64,
    pub camera_view: i32,
    /// Oxygen, thirst, health, toxic.
    pub gauges: [f32; 4],
}

/// Small, fast pseudo-random numbers (xorshift64*), like UnityEngine.Random.
pub struct Rng(u64);

impl Rng {
    /// PCDS_RNG_SEED fixes the sequence (benchmarks and replays compare runs).
    pub fn seeded() -> Rng {
        if let Some(seed) = std::env::var("PCDS_RNG_SEED").ok().and_then(|s| s.parse::<u64>().ok()) {
            return Rng(seed | 1);
        }
        let seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(1);
        Rng(seed | 1)
    }
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545F4914F6CDD1D)
    }
    /// Random.Range(min, max) for ints: max exclusive.
    pub fn range(&mut self, min: i32, max: i32) -> i32 {
        if max <= min {
            return min;
        }
        min + (self.next() % (max - min) as u64) as i32
    }
    /// Random.Range(min, max) for floats: both inclusive.
    pub fn rangef(&mut self, min: f32, max: f32) -> f32 {
        min + (self.next() >> 40) as f32 / (1u64 << 24) as f32 * (max - min)
    }
}

pub struct Game {
    pub data: GameData,
    pub net_data: NetData,
    pub net: Net,
    /// The save's sections the handlers below keep as JSON (world state, planets, players...).
    pub save: SaveFile,
    pub wos: IdMap<i32, WorldObject>,
    pub inventories: IdMap<i32, Inventory>,
    /// WorldObjectsHandler._constructedWorldObjects / _itemsPickablesWorldObjects.
    pub constructed: IdSet<i32>,
    pub pickables: IndexSet<i32>,
    /// InventoriesHandler._clientInventoryWatchers: inventory -> client -> count.
    pub watchers: HashMap<i32, IndexMap<u64, u32>>,
    pub tmp_inventory_id: i32,
    pub clients: IndexMap<u64, Client>,
    /// SavedDataHandler._playersData: saved players not in the game right now.
    pub saved_players: Vec<Value>,
    pub total_crafted: HashMap<u64, u32>,
    pub all_time_tokens: HashMap<u64, u64>,
    /// Index of the current planet in net_data.planets.
    pub planet: usize,
    /// Seconds since start (the game's Time.time).
    pub time: f32,
    pub started: std::time::Instant,
    /// A fixed clock (seconds since start) for benchmarks; None: the real one.
    pub sim_time: Option<f64>,
    pub rng: Rng,
    pub trace: bool,
    /// The per-version data directory (data/<version>/), for subsystems that load more of it.
    pub data_dir: String,
    pub timers: crate::scene::Timers,
    /// The server's own crafted-objects count (the save's stats).
    pub crafted_total: u64,
    pub units: crate::units::Units,
    pub ext: crate::ext::Ext,
    /// Clients the transport should drop, with the reason they are shown.
    pub kicks: Vec<(u64, String)>,
    pub access: crate::access::Access,
    /// The invite code's last 4 digits (SavedDataHandler.sessionId).
    pub session_number: u32,
    next_time_sync: f32,
    unhandled: std::collections::HashSet<String>,
    /// A subsystem asks for a save (SavedDataHandler.SaveWorldData); main writes it after the tick.
    pub save_requested: bool,
}

/// ConnectionRequestMessage's connection data: after the message versions and the config hash.
fn connection_data(body: &[u8]) -> Option<String> {
    let mut r = Reader::new(body);
    for _ in 0..r.packed_i32()? {
        r.u32()?;
        r.packed_i32()?;
    }
    r.u64()?;
    let n = r.i32()? as usize;
    Some(String::from_utf8_lossy(r.rest().get(..n)?).to_string())
}

/// JsonableGameState.ConvertOldFormatToNew: saves from before the dying consequences setting
/// had only a mode, which also set the gauge drain.
fn convert_old_format_to_new(settings: &mut Value) {
    if !settings.is_object() || !settings["dyingConsequencesLabel"].is_null() {
        return;
    }
    let (dying, drain) = match settings["mode"].as_str() {
        Some("Chill") => ("NoConsequences", Some(0.800000011920929)),
        Some("Intense") => ("DropAllItems", Some(1.2000000476837158)),
        Some("Hardcore") => ("DeleteSaveFile", Some(1.600000023841858)),
        _ => ("DropSomeItems", None),
    };
    settings["dyingConsequencesLabel"] = json!(dying);
    if let Some(drain) = drain {
        settings["modifierGaugeDrain"] = json!(drain);
    }
    settings["startLocationLabel"] = json!("");
}

/// The netdata player and planet-units prefabs as layouts.
pub fn layout_of(def: &ObjectDef) -> Layout {
    Layout {
        prefab: String::new(),
        hash: def.hash,
        flags: def.flags,
        scale: def.scale.unwrap_or([1.0; 3]),
        behaviours: def.behaviours.clone(),
        tail: crate::world::hex(&def.sync_tail),
        // Player tail: behaviour count, u16 index, i32 length, 3-byte flags, 2-byte tick, position.
        tail_position: if def.sync_tail.is_empty() { None } else { Some(12) },
    }
}

/// A game on a save with the test tables; None (the test skips) without them.
#[cfg(test)]
pub fn test_game(save: SaveFile) -> Option<Game> {
    let dir = crate::test_data()?;
    let (net_data, data, spec) = crate::tables(dir);
    Some(Game::new(net_data, data, spec, save, dir))
}

impl Game {
    pub fn new(net_data: NetData, data: GameData, spec: Spec, mut save: SaveFile, data_dir: &str) -> Game {
        convert_old_format_to_new(&mut save.settings);
        let scene_handle = net_data.scene_handle;
        // SavedDataHandler.InitSavedData loads the host's planet. This server has no host, so it
        // keeps the planet it travelled to in the world state (pcdsCurrentPlanet; the game
        // ignores unknown fields) and marks a saved player host for the game (mark_host).
        let host_planet = save.players.iter().find(|p| p["host"].as_bool() == Some(true)).and_then(|p| p["planetId"].as_str()).filter(|p| !p.is_empty());
        let planet_id = save.world["pcdsCurrentPlanet"].as_str().or(host_planet).or(save.settings["planetId"].as_str()).unwrap_or("Prime").to_string();
        let planet = net_data.planet_index(&planet_id).unwrap_or(0);
        let mut game = Game {
            data,
            net_data,
            net: Net::new(spec, scene_handle, FIRST_DYNAMIC_ID),
            save,
            wos: IdMap::default(),
            inventories: IdMap::default(),
            constructed: IdSet::default(),
            pickables: IndexSet::new(),
            watchers: HashMap::new(),
            tmp_inventory_id: -2,
            clients: IndexMap::new(),
            saved_players: Vec::new(),
            total_crafted: HashMap::new(),
            all_time_tokens: HashMap::new(),
            planet,
            time: 0.0,
            started: std::time::Instant::now(),
            sim_time: None,
            rng: Rng::seeded(),
            trace: std::env::var_os("PCDS_TRACE").is_some(),
            data_dir: String::new(),
            timers: Default::default(),
            crafted_total: 0,
            units: Default::default(),
            ext: Default::default(),
            kicks: Vec::new(),
            access: crate::access::Access::load(None),
            session_number: 4242,
            next_time_sync: 0.0,
            unhandled: Default::default(),
            save_requested: false,
        };
        game.change_generation_for_groups();
        game.crafted_total = game.save.stats["craftedObjects"].as_u64().unwrap_or(0);
        game.data_dir = data_dir.to_string();
        game.load();
        game
    }

    pub fn planet_id(&self) -> &str {
        &self.net_data.planets[self.planet].id
    }

    pub fn planet_hash(&self) -> i32 {
        stable_hash(self.planet_id())
    }

    pub fn tick_number(&self) -> i32 {
        (self.sim_time.unwrap_or_else(|| self.started.elapsed().as_secs_f64()) * self.net_data.tick_rate as f64) as i32
    }

    /// SavedDataHandler.LoadSavedData: world objects, inventories, planets, players; then the
    /// scene's network objects and every placed world object of the current planet.
    fn load(&mut self) {
        let default_planet = stable_hash(self.save.settings["planetId"].as_str().unwrap_or("Prime"));
        for o in std::mem::take(&mut self.save.objects) {
            if let Some(wo) = WorldObject::from_json(&o, &self.data, default_planet) {
                self.wos.insert(wo.id, wo);
            }
        }
        for inv in std::mem::take(&mut self.save.inventories) {
            if let Some(inv) = Inventory::from_json(&inv, &self.data, |id| self.wos.contains_key(&id)) {
                self.inventories.insert(inv.id, inv);
            }
        }
        self.saved_players = self.save.players.clone();
        let crafted = self.save.stats["craftedObjects"].as_u64().unwrap_or(0);
        let all_time = self.save.world["allTimeTerraTokens"].as_u64().unwrap_or(0);
        let single = self.saved_players.len() == 1;
        // The host this server marks (players::mark_host) is no host: only the game's count here.
        let game_host = !self.save.world["pcdsCurrentPlanet"].is_string();
        for p in &mut self.saved_players {
            if ((game_host && p["host"].as_bool() == Some(true)) || p["id"].as_u64() == Some(0) || single) && p["totalCraftedObjects"].as_u64().unwrap_or(0) == 0 && p["totalTerraTokenEarned"].as_u64().unwrap_or(0) == 0 {
                p["totalCraftedObjects"] = json!(crafted);
                p["totalTerraTokenEarned"] = json!(all_time);
            }
        }
        self.spawn_scene();
        // SetAllWorldObjects: the group counts, in save order.
        let ids: Vec<i32> = self.wos.keys().copied().collect();
        for id in &ids {
            self.add_to_specific_lists(*id);
            self.change_state_index(*id);
        }
        // PlaceAllWorldObjects.
        for id in ids {
            if self.wos.get(&id).is_some_and(|w| w.is_placed()) {
                self.instantiate(id, true, 0);
            }
        }
        crate::hooks::init(self);
        crate::unlocks::add_unlocked_groups(self);
        // Spawns before any client connects go out with ConnectionApproved instead.
        self.net.out.clear();
        self.net.flush();
        self.net.out.clear();
    }

    /// The in-scene network objects (handlers and managers) and one WorldUnitsPlanet per planet.
    fn spawn_scene(&mut self) {
        for def in self.net_data.scene_objects.clone() {
            let mut o = NetObj::from_layout(&layout_of(&def));
            o.id = def.id;
            o.parent = def.parent;
            self.net.add_scene_object(o);
        }
        crate::scene::init(self);
    }

    // ---- clients -------------------------------------------------------------------------

    pub fn connect(&mut self, client: u64, verified_steam_id: Option<u64>, trusted: bool, ip: Option<String>) {
        self.clients.insert(client, Client { id: client, approved: false, verified_steam_id, trusted, player: None, ready: None, ip, since: std::time::Instant::now() });
    }

    /// The client left or was dropped: everyone else sees the player go; the save keeps them.
    pub fn disconnect(&mut self, client: u64) {
        let Some(c) = self.clients.shift_remove(&client) else { return };
        if !c.approved {
            return;
        }
        self.player_left(&c);
        self.out_to(Target::Except(client), protocol::client_disconnected(client));
        for inventory in self.watchers.keys().copied().collect::<Vec<_>>() {
            self.stop_inventory_watch(inventory, client);
        }
    }

    pub fn out_to(&mut self, to: Target, message: (u32, Vec<u8>)) {
        self.net.out.push((to, message.0, message.1));
    }

    /// ConnectionRequest: spawn the player and send the world (SessionController.ApprovalCheck
    /// plus the access rules: refused addresses are told why and dropped).
    fn approve(&mut self, client: u64, request: &[u8]) {
        // SessionController.ApprovalCheck: "version|session" must match this server (direct-IP
        // clients send session 0, invite codes carry the 4-digit session number).
        let data = connection_data(request).unwrap_or_default();
        let (version, session) = data.split_once('|').unwrap_or((data.as_str(), ""));
        let session: u32 = session.parse().unwrap_or(u32::MAX);
        if version != self.net_data.game_version || (session != 0 && session != self.session_number) {
            println!("client {client}: refused (game {:?}, session {session})", version.chars().take(32).collect::<String>());
            self.kick(client, "MENU_Network_Error_version_mismatch");
            return;
        }
        if let Some(ip) = self.clients.get(&client).and_then(|c| c.ip.clone()) {
            if !self.access.ip_allowed(&ip) {
                println!("client {client}: refused ({ip})");
                self.kick(client, crate::access::REFUSED);
                return;
            }
        }
        // A banned SteamID never gets the world (names are only known at PlayerReady).
        if let Some(id) = self.clients.get(&client).and_then(|c| c.verified_steam_id).filter(|id| self.access.steam_id_banned(*id)) {
            println!("client {client}: refused (SteamID {id} is banned)");
            self.kick(client, crate::access::REFUSED);
            return;
        }
        let max = std::env::var("PCDS_MAX_PLAYERS").ok().and_then(|v| v.parse().ok()).unwrap_or(16usize);
        if self.clients.values().filter(|c| c.approved).count() >= max {
            println!("client {client}: refused (server full, {max} players)");
            self.kick(client, "MULTIPLAYER_Game_Session_Full");
            return;
        }
        let def = self.net_data.prefabs["Player"].clone();
        let mut o = NetObj::from_layout(&layout_of(&def));
        o.owner = client;
        if !def.spawn_sync.is_empty() {
            o.set_spawn_sync(&crate::world::hex(&def.spawn_sync));
        }
        let object = self.net.spawn(o);
        // The spawn above went to everyone already approved; the joiner gets it in the list.
        let mut connected = vec![0];
        connected.extend(self.clients.values().filter(|c| c.approved).map(|c| c.id));
        connected.push(client);
        let body = self.connection_approved(client, &connected);
        self.net.out.push((Target::Only(client), CONNECTION_APPROVED, body));
        if let Some(c) = self.clients.get_mut(&client) {
            c.approved = true;
            c.player = Some(object);
        }
        self.out_to(Target::Except(client), protocol::client_connected(client));
        crate::scene::on_client_approved(self, client);
    }

    fn connection_approved(&self, owner: u64, clients: &[u64]) -> Vec<u8> {
        let mut w = Writer::new();
        w.packed_i32(self.net_data.message_versions.len() as i32);
        for (hash, version) in &self.net_data.message_versions {
            w.u32(*hash).packed_i32(*version);
        }
        w.packed_u64(owner).packed_i32(self.tick_number());
        w.i32(clients.len() as i32);
        clients.iter().for_each(|c| {
            w.u64(*c);
        });
        w.u32(self.net.objects.len() as u32);
        for o in self.net.objects.values() {
            w.bytes(&o.scene_object(self.net.scene_handle));
        }
        w.0
    }

    /// One batch from a client.
    pub fn receive(&mut self, client: u64, batch: &[u8]) {
        for (kind, body) in wire::messages(batch) {
            if self.trace {
                println!("trace client {client} -> {kind} {}", body.iter().map(|b| format!("{b:02x}")).collect::<String>());
            }
            let approved = self.clients.get(&client).is_some_and(|c| c.approved);
            match kind {
                CONNECTION_REQUEST if !approved => self.approve(client, body),
                _ if !approved => {}
                NETWORK_TRANSFORM => self.transform(client, body),
                NETWORK_VARIABLE_DELTA => self.owner_variables(client, body),
                protocol::RPC => self.incoming_rpc(client, body),
                _ => {}
            }
        }
    }

    /// Owner-authoritative movement: merge the axes that moved into the object's state, keep it
    /// for late joiners and the save (WorldObjectAssociated.SavePositionAndRotation), and pass
    /// it on to everyone else.
    fn transform(&mut self, client: u64, body: &[u8]) {
        // As Netcode: only the owner moves an object.
        let mut r = Reader::new(body);
        let Some(object) = r.packed_u64() else { return };
        if self.net.objects.get(&object).is_none_or(|o| o.owner != client) {
            return;
        }
        if let Some(u) = crate::transform::update(body) {
            if let Some(o) = self.net.objects.get_mut(&u.object) {
                u.apply(&mut o.position, &mut o.rotation);
                let (position, rotation) = (o.position, o.rotation);
                if let Some(w) = o.wo.and_then(|wo| self.wos.get_mut(&wo)) {
                    w.position = position;
                    w.rotation = rotation;
                }
            }
        }
        self.out_to(Target::Except(client), (NETWORK_TRANSFORM, body.to_vec()));
    }

    /// Variables the owning client writes (drop physics state, emotes): apply and pass on.
    fn owner_variables(&mut self, client: u64, body: &[u8]) {
        if net::apply_delta(&mut self.net, client, body) {
            self.out_to(Target::Except(client), (NETWORK_VARIABLE_DELTA, body.to_vec()));
        }
    }

    fn incoming_rpc(&mut self, client: u64, body: &[u8]) {
        let mut r = Reader::new(body);
        let (Some(_sender), Some(object), Some(behaviour), Some(method)) = (r.packed_u64(), r.packed_u64(), r.packed_u16(), r.packed_u32()) else { return };
        let Some(args) = self.net.spec.decode(method, r.rest()) else {
            eprintln!("client {client}: undecodable RPC {method}");
            return;
        };
        let Some(name) = self.net.spec.get(method).map(|r| r.name.clone()) else { return };
        if self.trace {
            println!("trace client {client} rpc {name} on {object}/{behaviour} {}", Value::Array(args.clone()));
        }
        self.rpc(client, object, behaviour, &name, Args(args));
    }

    /// Hands an RPC to the ported handler that owns it.
    fn rpc(&mut self, client: u64, object: u64, behaviour: u16, name: &str, a: Args) {
        let handled = self.world_objects_rpc(client, name, &a)
            || self.inventories_rpc(client, name, &a)
            || self.players_rpc(client, name, &a)
            || crate::gauges::rpc(self, client, object, name, &a)
            || crate::unlocks::rpc(self, client, name, &a)
            || crate::handlers::rpc(self, client, object, behaviour, name, &a)
            || crate::hooks::rpc(self, client, object, behaviour, name, &a);
        if !handled && self.unhandled.insert(name.to_string()) {
            eprintln!("rpc {name} is not handled");
        }
    }

    pub fn kick(&mut self, client: u64, reason: &str) {
        self.kicks.push((client, reason.to_string()));
    }

    /// GameSettingsHandler.ChangeGenerationForGroups: the world's pace and power settings
    /// scale every building's generation.
    fn change_generation_for_groups(&mut self) {
        let setting = |k: &str| self.save.settings[k].as_f64().unwrap_or(1.0) as f32;
        let (power, pace) = (setting("modifierPowerConsumption"), setting("modifierTerraformationPace"));
        let energy = crate::model::unit_index("Energy").unwrap();
        for g in self.data.groups.iter_mut().filter(|g| g.constructible) {
            if g.units[energy] < 0.0 {
                g.units[energy] *= power;
            }
            for unit in ["Heat", "Oxygen", "Pressure", "Insects", "Plants", "Animals", "Purification"] {
                g.units[crate::model::unit_index(unit).unwrap()] *= pace;
            }
        }
    }

    /// One tick of the simulation, then the variable deltas.
    pub fn tick(&mut self) {
        self.time = match self.sim_time {
            Some(t) => t as f32,
            None => self.started.elapsed().as_secs_f32(),
        };
        crate::scene::tick(self);
        if self.time >= self.next_time_sync {
            self.next_time_sync = self.time + 1.0;
            let tick = self.tick_number();
            self.out_to(Target::All, protocol::time_sync(tick));
            // A connection that never becomes a player (no request, no PlayerReady) has the
            // world without the checks PlayerReady makes: give it two minutes.
            let stalled: Vec<u64> = self.clients.values().filter(|c| c.ready.is_none() && c.since.elapsed().as_secs() > 120).map(|c| c.id).collect();
            for client in stalled {
                println!("client {client}: never became a player, disconnecting");
                self.kick(client, "MENU_Network_Error_Timeout");
            }
        }
        self.net.flush();
    }

    /// The full save, written the way the game writes it.
    pub fn snapshot(&mut self) -> SaveFile {
        crate::players::update_saved_players(self);
        let data = &self.data;
        let mut save = SaveFile {
            world: self.save.world.clone(),
            planets: self.save.planets.clone(),
            players: self.saved_players.clone(),
            objects: self.wos.values().filter(|w| !w.dont_save).map(|w| w.to_json(data)).collect(),
            inventories: self.inventories.values().filter(|i| i.id >= 0).map(|i| i.to_json(data)).collect(),
            stats: self.save.stats.clone(),
            messages: self.save.messages.clone(),
            story_events: self.save.story_events.clone(),
            settings: self.save.settings.clone(),
            procedural: self.save.procedural.clone(),
        };
        save.world["pcdsCurrentPlanet"] = json!(self.planet_id());
        crate::players::mark_host(&mut save.players, self.planet_id());
        crate::scene::fill_save(self, &mut save);
        save
    }

    pub fn list_events_on(&mut self, class: &str, var: &str, events: Vec<ListEvent>) {
        if let Some((object, _)) = self.net.find_class(class) {
            self.net.list_events(object, class, var, events);
        }
    }

    pub fn set_on(&mut self, class: &str, var: &str, bytes: Vec<u8>) {
        if let Some((object, _)) = self.net.find_class(class) {
            self.net.set(object, class, var, bytes);
        }
    }

    /// The player object a client owns.
    pub fn player_object(&self, client: u64) -> Option<u64> {
        self.clients.get(&client)?.player
    }

    /// NetworkUtils.GetNearestClientId: the client whose player is closest to a position, among
    /// PlayersManager.playersControllers (players set up by PlayerReady; loading ones are not).
    pub fn nearest_client(&self, position: [f32; 3]) -> u64 {
        let mut best = (0u64, f32::MAX);
        for c in self.clients.values().filter(|c| c.ready.is_some()) {
            if let Some(p) = c.player.and_then(|o| self.net.objects.get(&o)) {
                let d: f32 = p.position.iter().zip(position).map(|(a, b)| (a - b) * (a - b)).sum();
                if d < best.1 {
                    best = (c.id, d);
                }
            }
        }
        best.0
    }

    /// Encodes a GroupCountData item.
    pub fn count_item(hash: i32, count: i32) -> Vec<u8> {
        [hash.to_le_bytes(), count.to_le_bytes()].concat()
    }

}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::players::name_id;

    /// A Prime world with an escape pod to spawn at.
    pub fn save() -> SaveFile {
        let mut save = SaveFile::empty();
        save.world = json!({"terraTokens": 0});
        save.planets = vec![json!({"planetId": "Prime"})];
        save.objects = vec![json!({"id": 208217122, "gId": "EscapePod", "pos": "558.2,2.47,494.1", "rot": "0,0,0,1", "planet": -1140328421})];
        save.stats = json!({"craftedObjects": 0});
        save.settings = json!({"planetId": "Prime", "dyingConsequencesLabel": "DropSomeItems"});
        save
    }

    /// Connects a client as the game would (ConnectionRequest), without readying the player.
    pub fn approve(game: &mut Game, client: u64) -> u64 {
        game.connect(client, None, false, None);
        let data = format!("{}|0", game.net_data.game_version);
        let mut w = Writer::new();
        w.packed_i32(0).u64(0).i32(data.len() as i32).bytes(data.as_bytes());
        game.approve(client, &w.0);
        game.player_object(client).unwrap()
    }

    /// Connects a client and readies its player (PlayerReadyServerRpc); returns the player object.
    pub fn join(game: &mut Game, client: u64, name: &str) -> u64 {
        let object = approve(game, client);
        game.players_rpc(client, "PlayerMainController.PlayerReadyServerRpc", &Args(vec![json!(0), json!(name)]));
        assert!(game.clients[&client].ready.is_some());
        object
    }

    fn transform_message(object: u64, flags: u32, floats: &[f32]) -> Vec<u8> {
        let mut w = Writer::new();
        w.packed_u64(object).packed_i32(0).packed_u32(flags).packed_i32(1);
        floats.iter().for_each(|f| {
            w.bytes(&f.to_le_bytes());
        });
        w.0
    }

    #[test]
    fn partial_transforms_move_players_and_save_dropped_items_rotation() {
        let Some(mut g) = test_game(save()) else { return };
        let player = join(&mut g, 1, "Ann");
        let start = g.net.objects[&player].position;
        g.transform(1, &transform_message(player, 4, &[9.0]));
        assert_eq!(g.net.objects[&player].position, [start[0], 9.0, start[2]], "only Y moved");
        g.transform(1, &transform_message(player, 32, &[90.0]));
        assert!((crate::geometry::euler_y(g.net.objects[&player].rotation) - 90.0).abs() < 0.01, "rotation-only state");
        // An item the client dropped (and so simulates): position and full rotation are saved.
        let iron = g.create_world_object(g.data.group("Iron").unwrap(), 0, true);
        g.drop_on_floor(iron, [560.0, 3.0, 500.0], [0.0, 0.0, 0.0, 1.0], 0.0, false, false, 0, 0.0, false, 1);
        let net = g.wos[&iron].net.unwrap();
        g.transform(1, &transform_message(net, 2 | 16 | 32 | 64, &[561.0, 10.0, 20.0, 30.0]));
        let w = &g.wos[&iron];
        assert_eq!(w.position, [561.0, 3.0, 500.0]);
        let e = crate::transform::euler_angles(w.rotation);
        assert!((e[0] - 10.0).abs() < 0.01 && (e[1] - 20.0).abs() < 0.01 && (e[2] - 30.0).abs() < 0.01, "{e:?}");
    }

    #[test]
    fn loading_clients_are_not_the_nearest() {
        let Some(mut g) = test_game(save()) else { return };
        join(&mut g, 1, "Ann");
        approve(&mut g, 2);
        assert_eq!(g.nearest_client([0.0; 3]), 1, "client 2's player still stands at the origin, loading");
    }

    #[test]
    fn oxygen_above_100_survives_a_join() {
        let Some(mut g) = test_game(save()) else { return };
        let inventory = g.create_inventory(START_INVENTORY_SIZE, 0, 0, &[], &[]);
        let equipment = g.create_inventory(START_EQUIPMENT_SIZE, 0, 0, &[], &[]);
        for item in ["OxygenTank1", "AirFilter1"] {
            g.add_new_item(g.data.group(item).unwrap(), equipment, 0);
        }
        g.saved_players.push(json!({"id": name_id("Ann"), "name": "Ann", "inventoryId": inventory, "equipmentId": equipment, "playerPosition": "558,3,494",
            "playerGaugeOxygen": 140.0, "playerGaugeThirst": 100.0, "playerGaugeHealth": 100.0, "playerGaugeToxic": 0.0, "planetId": "Prime"}));
        let player = join(&mut g, 1, "Ann");
        assert!(g.ext.gauges.0[&player].rebreather);
        g.time += 0.2;
        crate::gauges::tick(&mut g);
        let oxygen = crate::gauges::values(&g, player)[0];
        assert!(oxygen > 139.0 && oxygen < 140.0, "{oxygen}: the tank's 145 is the maximum");
    }

    #[test]
    fn server_drops_go_to_the_nearest_client() {
        let Some(mut g) = test_game(save()) else { return };
        join(&mut g, 1, "Ann");
        let iron = g.create_world_object(g.data.group("Iron").unwrap(), 0, true);
        g.net.out.clear();
        g.drop_on_floor(iron, [560.0, 3.0, 500.0], [0.0, 0.0, 0.0, 1.0], 0.0, true, false, 0, 0.0, false, 0);
        let net = g.wos[&iron].net.unwrap();
        assert_eq!(g.net.objects[&net].owner, 1);
        assert!(g.net.out.iter().any(|(to, kind, _)| *kind == protocol::CLIENT_RPC && *to == Target::Only(1)), "the physics RPC goes to the new owner");
    }

    #[test]
    fn hardcore_death_deletes_the_character() {
        let mut save = save();
        save.settings["dyingConsequencesLabel"] = json!("DeleteSaveFile");
        let Some(mut g) = test_game(save) else { return };
        join(&mut g, 1, "Ann");
        crate::status::handle_dying_consequences(&mut g, 1);
        assert!(g.kicks.iter().any(|(c, _)| *c == 1));
        let gone = |players: &[Value]| !players.iter().any(|p| p["id"].as_u64() == Some(name_id("Ann")));
        assert!(gone(&g.snapshot().players), "a save before the drop does not keep them");
        g.disconnect(1);
        assert!(gone(&g.saved_players), "nor does leaving");
    }

    #[test]
    fn crafts_count_into_the_save_stats() {
        let Some(mut g) = test_game(save()) else { return };
        g.world_objects_rpc(1, "WorldObjectsHandler.AddOneToTotalCraftServerRpc", &Args(vec![]));
        assert_eq!(g.snapshot().stats["craftedObjects"], json!(1));
    }

    #[test]
    fn world_settings_unlock_their_blueprints_and_unknown_messages_go() {
        let mut save = save();
        save.settings["unlockedDrones"] = json!(true);
        save.settings["unlockedTeleporters"] = json!(true);
        save.messages = vec![json!({"stringId": "Message_YouAreAConvict", "isRead": true}), json!({"stringId": "Message_NoLongerInTheGame"})];
        let Some(g) = test_game(save) else { return };
        for (group, unlocked) in [("DroneStation1", true), ("Teleporter1", true), ("AutoCrafter1", false)] {
            assert_eq!(crate::unlocks::is_globally_unlocked(&g, g.data.group(group).unwrap()), unlocked, "{group}");
        }
        let ids: Vec<&str> = g.save.messages.iter().filter_map(|m| m["stringId"].as_str()).collect();
        assert!(ids.contains(&"Message_YouAreAConvict") && !ids.contains(&"Message_NoLongerInTheGame"), "{ids:?}");
    }

    #[test]
    fn old_saves_get_dying_consequences_from_their_mode() {
        let mut s = json!({"mode": "Hardcore"});
        convert_old_format_to_new(&mut s);
        assert_eq!((s["dyingConsequencesLabel"].as_str(), s["modifierGaugeDrain"].as_f64()), (Some("DeleteSaveFile"), Some(1.600000023841858)));
        let mut s = json!({"mode": "Chill", "dyingConsequencesLabel": "DropAllItems"});
        convert_old_format_to_new(&mut s);
        assert_eq!(s["dyingConsequencesLabel"], "DropAllItems", "new saves are left alone");
    }

    /// Hostile input from joined clients never crashes the server: every server RPC with
    /// hostile values (other players' ids, extremes, huge strings), garbage after real RPC
    /// headers, and ticks in between. Prints each RPC that panicked, then fails.
    #[test]
    fn hostile_rpcs_do_not_crash_the_server() {
        let Some(dir) = crate::test_data() else { return };
        let table: Value = serde_json::from_str(&std::fs::read_to_string(format!("{dir}/rpcs.json")).unwrap()).unwrap();
        let mut names: Vec<String> = table.as_object().unwrap().values().filter(|r| r["to"] == "server").filter_map(|r| r["name"].as_str().map(String::from)).collect();
        names.sort();
        struct Lcg(u64);
        impl Lcg {
            fn next(&mut self) -> u64 {
                self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                self.0 >> 33
            }
        }
        fn pick(r: &mut Lcg, g: &Game) -> Value {
            let ids: Vec<i64> = match r.next() % 4 {
                0 => g.wos.keys().map(|k| *k as i64).collect(),
                1 => g.inventories.keys().map(|k| *k as i64).collect(),
                2 => g.net.objects.keys().map(|k| *k as i64).collect(),
                _ => g.data.groups.iter().map(|gr| gr.hash as i64).collect(),
            };
            let any = |r: &mut Lcg| ids.get((r.next() % ids.len().max(1) as u64) as usize).copied().unwrap_or(0);
            match r.next() % 12 {
                0 => json!(-1),
                1 => json!(0),
                2 => json!(i32::MAX),
                3 => json!(i32::MIN),
                4 => json!(u64::MAX),
                5 => json!(1.0e30),
                6 => json!("x".repeat((r.next() % 5000) as usize)),
                7 => {
                    let n = r.next() % 40;
                    json!((0..n).map(|_| any(r)).collect::<Vec<_>>())
                }
                8 => json!([r.next() as f32, -1.0e30, 0.5, 2.0]),
                9 => json!(r.next() % 2 == 0),
                _ => json!(any(r)),
            }
        }
        // PCDS_FUZZ_ROUNDS / PCDS_FUZZ_SEED for longer runs than the test suite's.
        let rounds: u64 = std::env::var("PCDS_FUZZ_ROUNDS").ok().and_then(|v| v.parse().ok()).unwrap_or(1500);
        let mut r = Lcg(std::env::var("PCDS_FUZZ_SEED").ok().and_then(|v| v.parse().ok()).unwrap_or(0x9e37_79b9_7f4a_7c15));
        let world = || {
            let mut g = test_game(save()).unwrap();
            join(&mut g, 1, "Ann");
            join(&mut g, 2, "Bob");
            g
        };
        let mut g = world();
        let mut failures: std::collections::BTreeMap<String, String> = Default::default();
        static WHERE: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());
        let quiet = std::panic::take_hook();
        std::panic::set_hook(Box::new(|info| {
            *WHERE.lock().unwrap() = info.location().map(|l| format!("{}:{}", l.file(), l.line())).unwrap_or_default();
        }));
        for round in 0..rounds {
            let name = names[(r.next() % names.len() as u64) as usize].clone();
            let args: Vec<Value> = (0..r.next() % 9).map(|_| pick(&mut r, &g)).collect();
            let client = 1 + r.next() % 2;
            let objects: Vec<u64> = g.net.objects.keys().copied().collect();
            let object = objects[(r.next() % objects.len() as u64) as usize];
            let behaviour = (r.next() % 6) as u16;
            let (hash, mut body) = g.net.spec.encode(&name, &args);
            if r.next() % 8 == 0 {
                // Truncated or trailing garbage.
                let cut = (r.next() as usize) % (body.len() + 1);
                body.truncate(cut);
                body.extend((0..r.next() % 16).map(|_| r.next() as u8));
            }
            let mut w = Writer::new();
            w.packed_u64(client).packed_u64(object).packed_u16(behaviour).packed_u32(hash).bytes(&body);
            let message = w.0;
            if std::env::var_os("PCDS_FUZZ_TRACE").is_some() {
                eprintln!("FUZZ {round} {name} {}", Value::Array(args.clone()).to_string().chars().take(200).collect::<String>());
            }
            let mut outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| g.incoming_rpc(client, &message))).err().map(|e| (name.clone(), e));
            if outcome.is_none() && round % 50 == 0 {
                g.time += 0.7;
                outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| g.tick())).err().map(|e| (format!("tick after {name}"), e));
            }
            // The main loop sends and clears the queue every tick.
            g.net.out.clear();
            if std::env::var_os("PCDS_FUZZ_TRACE").is_some() && round % 500 == 0 {
                let texts: usize = g.wos.values().map(|w| w.text.len()).sum();
                let items: usize = g.inventories.values().map(|i| i.items.len()).sum();
                eprintln!("SIZES {round} wos={} inventories={} items={items} net={} timers={} watchers={} saved={} texts={texts} unhandled={}", g.wos.len(), g.inventories.len(), g.net.objects.len(), g.timers.0.len(), g.watchers.len(), g.saved_players.len(), g.unhandled.len());
            }
            if let Some((what, e)) = outcome {
                let why = e.downcast_ref::<String>().cloned().or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_default();
                let at = WHERE.lock().unwrap().clone();
                failures.entry(format!("{at} {what}")).or_insert(format!("{why} (args {})", Value::Array(args.clone())));
                // The world may be half-changed after a panic: start over.
                g = world();
            }
        }
        std::panic::set_hook(quiet);
        for (name, why) in &failures {
            eprintln!("PANIC {name}: {}", why.chars().take(300).collect::<String>());
        }
        assert!(failures.is_empty(), "{} RPCs panicked", failures.len());
    }

    /// Garbage message batches (random kinds, truncated bodies, real object ids up front) and
    /// connection churn never crash the server.
    #[test]
    fn garbage_batches_do_not_crash_the_server() {
        let Some(mut g) = test_game(save()) else { return };
        join(&mut g, 1, "Ann");
        join(&mut g, 2, "Bob");
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let kinds = [CONNECTION_REQUEST, NETWORK_TRANSFORM, NETWORK_VARIABLE_DELTA, protocol::RPC, 0, 99];
        for round in 0..20_000u64 {
            let client = 1 + next() % 3;
            if next() % 500 == 0 {
                g.disconnect(client);
                g.connect(client, None, false, Some("127.0.0.1".into()));
            }
            let objects: Vec<u64> = g.net.objects.keys().copied().collect();
            let mut body = Writer::new();
            body.packed_u64(objects[(next() % objects.len() as u64) as usize]).packed_u16((next() % 8) as u16);
            for _ in 0..next() % 64 {
                body.u8(next() as u8);
            }
            let kind = kinds[(next() % kinds.len() as u64) as usize];
            let mut batch = wire::batch(&[(kind, body.0)]);
            if next() % 4 == 0 {
                let cut = (next() as usize) % (batch.len() + 1);
                batch.truncate(cut);
            }
            if next() % 10 == 0 {
                batch = (0..next() % 200).map(|_| next() as u8).collect();
            }
            g.receive(client, &batch);
            if round % 200 == 0 {
                g.time += 0.5;
                g.tick();
            }
        }
    }

    #[test]
    fn a_second_player_ready_and_a_second_session_are_refused() {
        let Some(mut g) = test_game(save()) else { return };
        join(&mut g, 1, "Ann");
        g.players_rpc(1, "PlayerMainController.PlayerReadyServerRpc", &Args(vec![json!(0), json!("Bob")]));
        assert_eq!(g.clients[&1].ready.as_ref().unwrap().name, "Ann", "PlayerReady counts once");
        approve(&mut g, 2);
        g.players_rpc(2, "PlayerMainController.PlayerReadyServerRpc", &Args(vec![json!(0), json!("ann")]));
        assert!(g.clients[&2].ready.is_none());
        assert!(g.kicks.iter().any(|(c, _)| *c == 2), "Ann is already online");
    }

    #[test]
    fn client_chosen_inventory_ids_never_replace_inventories() {
        let Some(mut g) = test_game(save()) else { return };
        join(&mut g, 1, "Ann");
        g.inventories_rpc(1, "InventoriesHandler.GetOrCreateNewInventoryServerRpc", &Args(vec![json!(true), json!(5), json!(9_999_998), json!(0), json!(null), json!(null)]));
        let a = g.create_inventory(5, 0, 0, &[], &[]);
        let b = g.create_inventory(5, 0, 0, &[], &[]);
        assert_ne!(a, b);
        assert!(a < crate::inventories::SPECIAL_INVENTORY_IDS && b < crate::inventories::SPECIAL_INVENTORY_IDS);
        assert!(g.inventories.contains_key(&a) && g.inventories.contains_key(&b));
    }

    #[test]
    fn picked_up_debris_outlives_its_despawn_timer() {
        let Some(mut g) = test_game(save()) else { return };
        let player = join(&mut g, 1, "Ann");
        let inventory = g.clients[&1].ready.as_ref().unwrap().inventory;
        let iron = g.data.group("Iron").unwrap();
        let id = g.create_world_object(iron, 0, true);
        g.drop_on_floor(id, [560.0, 3.0, 497.0], [0.0, 0.0, 0.0, 1.0], 0.0, false, false, 0, 1.0, false, 0);
        let hash = g.data.groups[iron].hash;
        g.inventories_rpc(1, "InventoriesHandler.AddWorldObjectToInventoryServerRpc", &Args(vec![json!(id), json!(hash), json!(inventory), json!(true)]));
        assert!(g.inventories[&inventory].items.contains(&id));
        let _ = player;
        g.time += 2.0;
        crate::scene::tick(&mut g);
        assert!(g.wos.contains_key(&id) && g.inventories[&inventory].items.contains(&id), "the despawn timer went with the dropped object");
    }
}
