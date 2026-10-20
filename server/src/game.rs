//! The game server proper: the world model (world objects, inventories, players) as the game's
//! server-side handlers keep it, what clients see of it (net), and the dispatch of client
//! messages. The handlers themselves are ported class by class into sibling modules
//! (objects, inventories, players, ...), each an `impl Game` block.

use std::collections::HashMap;

use indexmap::IndexMap;
use serde_json::{json, Value};

use crate::data::{stable_hash, NetData};
use crate::gamedata::GameData;
use crate::idmap::{SlotSet, Slots};
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
/// Approved clients from one address that are not players yet (see approve).
const MAX_JOINING_PER_IP: usize = 2;
/// The RPCs a game client sends while it loads, before its PlayerReady (the name and whitelist
/// checks): what the objects of the world it was sent and the planet scene ask for as they
/// spawn and start (GrowthProxy.InitGrowth, ConstructibleProxy.OnNetworkSpawn and
/// MachineRocketBackAndForth.Awake, the inventory displays' Start, WorldObjectFromScene,
/// MachineGenerationGroupVein, SceneInterpolationBase and GameEventActivateInterpolators,
/// ToxicAreaVolumes and ToxicAreaDecal, WorldAnimalSpawnerData, ProceduralSceneInstance).
/// Every other RPC before PlayerReady is dropped.
pub(crate) const LOADING_RPCS: &[&str] = &[
    "PlayerMainController.PlayerReadyServerRpc",
    "GrowthProxy.GetCurrentGrowthServerRpc",
    "ConstructibleProxy.MachineOutsideGrowerUpdateStartServerRpc",
    "ConstructibleProxy.RetrieveRocketBackAndForthSpeedMultiplierServerRpc",
    "InventoryAssociatedProxy.GetInventoryIdServerRpc",
    "InventoriesHandler.GetOrCreateNewInventoryServerRpc",
    "InventoriesHandler.BeginInventoryWatchServerRpc",
    "WorldObjectAssociatedProxy.GetWorldObjectIdServerRpc",
    "WorldObjectsHandler.IsWorldObjectFromSceneActiveServerRpc",
    "WorldObjectsHandler.RefreshSceneWorldObjectGrowthServerRpc",
    "SceneInterpolationHandler.RegisterInterpolatorServerRpc",
    "SceneInterpolationHandler.CheckInterpolatorActiveServerRpc",
    "ToxicAreaHandler.RegisterToAreaToxicFinishedServerRpc",
    "ToxicAreaHandler.RegisterToAreaToxicProgressServerRpc",
    "WorldAnimalSpawnerHandler.SpawnerRegisteredServerRpc",
    "ProceduralInstancesHandler.LoadInstanceFromSceneServerRpc",
];

/// The most of an animator message the server passes on: it is relayed undecoded to every other
/// client, and the game's (the parameters of an animator and the states of its layers) are a few
/// hundred bytes.
const MAX_ANIMATOR_MESSAGE: usize = 8 << 10;

/// The server RPCs the game declares `[Rpc(SendTo.Server)]` without `InvokePermission =
/// Everyone`: Netcode drops one whose sender does not own the object it is sent to
/// (RpcMessageHelpers.Handle).
const OWNER_ONLY_RPCS: &[&str] = &[
    "GroupNetworkContainer.RemoveOwnershipServerRpc",
    "PlayerMainController.PlayerReadyServerRpc",
    "PlayerShareState.InputOnJumpServerRpc",
    "PlayerShareState.StartMiningServerRpc",
    "PlayerShareState.StopMiningServerRpc",
    "PlayerStatus.HandleDyingConsequencesServerRpc",
];

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
#[derive(serde::Serialize, serde::Deserialize)]
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
    #[serde(with = "crate::handoff::instant")]
    pub since: std::time::Instant,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
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
#[derive(serde::Serialize, serde::Deserialize)]
pub struct Rng(u64);

impl Rng {
    /// In a test build, PCDS_RNG_SEED fixes the sequence (benchmarks and replays compare runs);
    /// the server itself never reads it.
    pub fn seeded() -> Rng {
        #[cfg(test)]
        if let Some(seed) = std::env::var("PCDS_RNG_SEED").ok().and_then(|s| s.parse::<u64>().ok()) {
            return Rng(seed | 1);
        }
        let seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(1);
        Rng(seed | 1)
    }
    /// A fixed sequence, for the tests whose outcome must not depend on the ids a clock picked
    /// (a vegetube's growth is a sequence seeded by its id).
    #[cfg(test)]
    pub fn fixed(seed: u64) -> Rng {
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

#[derive(serde::Serialize, serde::Deserialize)]
pub struct Game {
    #[serde(skip)]
    pub data: GameData,
    #[serde(skip)]
    pub net_data: NetData,
    pub net: Net,
    /// The save's sections the handlers below keep as JSON (world state, planets, players...).
    pub save: SaveFile,
    pub wos: Slots<i32, WorldObject>,
    /// InventoriesHandler._allWorldInventories, a Dictionary: the save lists it in its order.
    pub inventories: Slots<i32, Inventory>,
    /// WorldObjectsHandler._constructedWorldObjects / _itemsPickablesWorldObjects.
    pub constructed: SlotSet<i32>,
    pub pickables: SlotSet<i32>,
    /// Every world object that has a network object, and perhaps some that had (set_wo_net keeps
    /// it; a scan for what stands in the scene looks at these, not at every world object).
    #[serde(skip)]
    pub net_wos: crate::idmap::FxHashSet<i32>,
    /// World object -> (the inventory holding it, in how many inventories it is): the
    /// inventories' content indexed (inventories.rs hold/release), rebuilt from them on load.
    #[serde(skip)]
    pub holders: crate::idmap::FxHashMap<i32, (i32, u32)>,
    /// InventoriesHandler._clientInventoryWatchers: inventory -> client -> count.
    pub watchers: HashMap<i32, IndexMap<u64, u32>>,
    pub tmp_inventory_id: i32,
    pub clients: crate::idmap::IdMap<u64, Client>,
    /// SavedDataHandler._playersData: saved players not in the game right now.
    #[serde(with = "crate::bincodec::json_values")]
    pub saved_players: Vec<Value>,
    pub total_crafted: HashMap<u64, u32>,
    pub all_time_tokens: HashMap<u64, u64>,
    /// Index of the current planet in net_data.planets.
    pub planet: usize,
    /// Seconds since start (the game's Time.time; clock.rs: its float below an hour, exact after).
    pub time: crate::clock::GameTime,
    #[serde(with = "crate::handoff::instant")]
    pub started: std::time::Instant,
    /// A fixed clock (seconds since start) for benchmarks; None: the real one.
    pub sim_time: Option<f64>,
    /// Seconds the clock stood still while nobody was in the game (rules: world.pauseWhenEmpty),
    /// and the clock at the last tick.
    paused: f64,
    /// The admin paused the world (POST /api/pause): it stands still as with nobody in it,
    /// players in it or not, until resumed.
    pub held: bool,
    last_clock: f64,
    /// The clock moved since main last took this: an autosave of a world that stood still since
    /// the last one would write that one again.
    pub advanced: bool,
    pub rng: Rng,
    /// The wreck layouts and the planets' scene dumps (main.rs scene_tables): static, given
    /// again to a restored game.
    #[serde(skip)]
    pub scenes: crate::Scenes,
    pub timers: crate::scene::Timers,
    /// The server's own crafted-objects count (the save's stats).
    pub crafted_total: u64,
    pub units: crate::units::Units,
    pub ext: crate::ext::Ext,
    /// Clients the transport should drop, with the reason they are shown.
    pub kicks: Vec<(u64, String)>,
    pub access: crate::access::Access,
    pub rules: crate::rules::Rules,
    /// Where set() keeps the rules (None: not kept, as in tests).
    pub rules_path: Option<String>,
    /// The invite code's last 4 digits (SavedDataHandler.sessionId).
    pub session_number: u32,
    next_time_sync: crate::clock::GameTime,
    #[serde(skip)]
    unhandled: std::collections::HashSet<String>,
    /// Clients refused a change to another player's inventory (logged once each).
    #[serde(skip)]
    pub refused_inventories: std::collections::HashSet<u64>,
    /// A client's RPC that waited for its turn panicked in the tick and was contained (its client
    /// kicked): the bank taints the world as it does for the panics it contains (bank/panic.rs).
    #[serde(skip)]
    pub panicked: bool,
    /// RPCs refused for the object they were sent to (logged once each).
    #[serde(skip)]
    misrouted: std::collections::HashSet<String>,
    /// A subsystem asks for a save (SavedDataHandler.SaveWorldData); main writes it after the tick.
    pub save_requested: bool,
    /// Chat rate and RPC work per client.
    pub limits: crate::handlers::Limits,
    /// While the world loads: what the machines' energy checks share, the water boxes and which
    /// groups work underwater (None: not loading, each check makes its own). The boxes only move
    /// with terraformation, which stands still while the world is placed.
    #[serde(skip)]
    pub load_water: Option<crate::energy::Underwater>,
    /// The hour (Unix time / 3600) players::forget_unseen last looked, so that it looks once an
    /// hour; 0 after a start or a hot update (it looks at once, which does nothing twice).
    #[serde(skip)]
    pub forgot_hour: u64,
}

/// An RPC message's method hash (after sender, object and behaviour).
fn rpc_method(body: &[u8]) -> Option<u32> {
    let mut r = Reader::new(body);
    r.packed_u64()?;
    r.packed_u64()?;
    r.packed_u16()?;
    r.packed_u32()
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

/// World objects the tables cannot place are dropped when the world loads, and gone from the
/// next save: a save from a newer game than these tables (or with mods), or an entry edited by
/// hand. The start stops instead, saying which.
pub fn check_save_content(save: &SaveFile, data: &GameData, net_data: &NetData) -> Result<(), String> {
    world_planet(save, net_data)?;
    let unknown: Vec<&str> = save.objects.iter().filter(|o| WorldObject::from_json(o, data, 0).is_none()).map(|o| o["gId"].as_str().unwrap_or("(no group)")).collect();
    match unknown.first() {
        None => Ok(()),
        Some(first) => Err(format!("{} world objects of groups these game tables do not know (the first: {first}): a save from a newer game version, or edited? Update the server or its game data (`pcds update`), then try again", unknown.len())),
    }
}

/// The planet the world is on when it starts, and its place in the tables. SavedDataHandler.
/// InitSavedData loads the host's planet; this server has no host, so it keeps the planet it
/// travelled to in the world state (pcdsCurrentPlanet; the game ignores unknown fields) and marks a
/// saved player host for the game (mark_host). A save that names no planet (settings.planetId is
/// what the game starts a world with), or one these tables do not have, is an error saying so:
/// there is no planet to start a world on instead.
pub fn world_planet(save: &SaveFile, net_data: &NetData) -> Result<(String, usize), String> {
    let known = |what: &str, id: &str| net_data.planet_index(id).ok_or_else(|| format!("{what} is {id:?}, a planet these game tables do not have: a save from a newer game version, or edited? Update the server or its game data (`pcds update`), then try again"));
    let started = save.settings["planetId"].as_str().ok_or("settings.planetId is missing: it is the planet the world started on, and every save of the game has it. Not a save of this game, or edited?")?;
    known("settings.planetId", started)?;
    let host = save.players.iter().find(|p| p["host"].as_bool() == Some(true)).and_then(|p| p["planetId"].as_str()).filter(|p| !p.is_empty());
    let (what, id) = match (save.world["pcdsCurrentPlanet"].as_str(), host) {
        (Some(current), _) => ("world.pcdsCurrentPlanet", current),
        (None, Some(host)) => ("the host player's planetId", host),
        (None, None) => ("settings.planetId", started),
    };
    Ok((id.to_string(), known(what, id)?))
}

/// A game on a save with the test tables; None (the test skips) without them.
#[cfg(test)]
pub fn test_game(save: SaveFile) -> Option<Game> {
    let (net_data, data, spec, scenes) = test_tables()?;
    Some(Game::new(net_data, data, spec, save, scenes))
}

/// The test tables, read once per test process (parsing them is most of a test's time).
#[cfg(test)]
pub fn test_tables() -> Option<(NetData, GameData, Spec, crate::Scenes)> {
    static TABLES: std::sync::OnceLock<(NetData, GameData, Spec, crate::Scenes)> = std::sync::OnceLock::new();
    let dir = crate::test_data()?;
    Some(TABLES.get_or_init(|| {
        let (net_data, data, spec) = crate::tables(dir);
        let scenes = crate::scene_tables(dir, &net_data, &data);
        (net_data, data, spec, scenes)
    }).clone())
}

#[cfg(test)]
mod play_tests;

impl Game {
    pub fn new(net_data: NetData, data: GameData, spec: Spec, save: SaveFile, scenes: crate::Scenes) -> Game {
        Game::with_clock(net_data, data, spec, save, scenes, None)
    }

    /// A game whose clock is `sim_time` from the start (the bench's fixed clock): what the load
    /// spawns gets the same ticks in every run, not the real time the load took.
    pub fn with_clock(net_data: NetData, data: GameData, spec: Spec, save: SaveFile, scenes: crate::Scenes, sim_time: Option<f64>) -> Game {
        let (_, planet) = world_planet(&save, &net_data).unwrap_or_else(|e| panic!("a save the start would have refused (check_save_content): {e}"));
        let mut game = Game {
            data,
            net_data,
            net: Net::new(spec, FIRST_DYNAMIC_ID),
            save,
            wos: Slots::default(),
            inventories: Slots::default(),
            constructed: SlotSet::default(),
            pickables: SlotSet::default(),
            net_wos: Default::default(),
            holders: Default::default(),
            watchers: HashMap::new(),
            tmp_inventory_id: -2,
            clients: Default::default(),
            forgot_hour: 0,
            saved_players: Vec::new(),
            total_crafted: HashMap::new(),
            all_time_tokens: HashMap::new(),
            planet,
            time: crate::clock::GameTime::ZERO,
            started: std::time::Instant::now(),
            sim_time: None,
            paused: 0.0,
            held: false,
            last_clock: 0.0,
            advanced: false,
            rng: Rng::seeded(),
            scenes,
            timers: Default::default(),
            crafted_total: 0,
            units: Default::default(),
            ext: Default::default(),
            kicks: Vec::new(),
            access: crate::access::Access::load(None),
            rules: Default::default(),
            rules_path: None,
            session_number: crate::config::env_number("PCDS_SESSION") as u32,
            next_time_sync: crate::clock::GameTime::ZERO,
            unhandled: Default::default(),
            misrouted: Default::default(),
            refused_inventories: Default::default(),
            panicked: false,
            save_requested: false,
            limits: Default::default(),
            load_water: None,
        };
        game.sim_time = sim_time;
        // A clock that starts late (the time-travel tests' old worlds): the world begins at it.
        game.last_clock = sim_time.unwrap_or(0.0);
        game.time = crate::clock::GameTime::at(game.last_clock);
        game.change_generation_for_groups();
        game.crafted_total = game.save.stats["craftedObjects"].as_u64().unwrap_or(0);
        game.load();
        game
    }

    pub fn planet_id(&self) -> &str {
        &self.net_data.planets[self.planet].id
    }

    pub fn planet_hash(&self) -> i32 {
        stable_hash(self.planet_id())
    }

    /// Seconds since start, before the pauses: the bench's clock or the real one.
    fn clock(&self) -> f64 {
        self.sim_time.unwrap_or_else(|| self.started.elapsed().as_secs_f64())
    }

    /// The planet the world started on (settings.planetId), which `world_planet` has seen to be a
    /// planet of the tables when the world was loaded.
    pub fn started_planet(&self) -> &str {
        self.save.settings["planetId"].as_str().expect("settings.planetId: checked by world_planet when the world loaded")
    }

    /// Network ticks since the game started, in 64 bits: nothing that counts them stops.
    pub fn tick_count(&self) -> u64 {
        ((self.clock() - self.paused) * self.net_data.tick_rate as f64) as u64
    }

    /// The tick as the game's int (NetworkTime.Tick): the same up to 2^31 ticks (828 days at 30
    /// a second), then the low 32 bits, wrapping as an unchecked int does. The game's own
    /// `(int)double` there is out of range and stays at one value for good; here the clients'
    /// time sync (a hard reset when the server time jumps by 0.2 s) takes the wrap as a jump.
    pub fn tick_number(&self) -> i32 {
        self.tick_count() as i32
    }

    /// SavedDataHandler.LoadSavedData: world objects, inventories, planets, players; then the
    /// scene's network objects and every placed world object of the current planet.
    fn load(&mut self) {
        let default_planet = stable_hash(self.started_planet());
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
        self.rebuild_holders();
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
        self.load_water = Some(Default::default());
        for id in ids {
            if self.wos.get(&id).is_some_and(|w| w.is_placed()) {
                self.instantiate(id, true, 0);
            }
        }
        self.load_water = None;
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
            let mut o = NetObj::from_layout(&def.layout(0));
            o.id = def.id;
            o.parent = def.parent;
            self.net.add_scene_object(o);
        }
        crate::scene::init(self);
        // Every variable has its value before anyone joins (players: from their prefab).
        self.net.objects.values().for_each(NetObj::check);
        NetObj::from_layout(&self.net_data.prefabs["Player"].layout(0)).check();
    }

    // ---- clients -------------------------------------------------------------------------

    pub fn connect(&mut self, client: u64, verified_steam_id: Option<u64>, trusted: bool, ip: Option<String>) {
        self.clients.insert(client, Client { id: client, approved: false, verified_steam_id, trusted, player: None, ready: None, ip, since: std::time::Instant::now() });
    }

    /// The client left or was dropped: everyone else sees the player go; the save keeps them.
    pub fn disconnect(&mut self, client: u64) {
        let Some(c) = self.clients.shift_remove(&client) else { return };
        self.limits.forget(client);
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
            info!("client {client}: refused (game {:?}, session {session})", version.chars().take(32).collect::<String>());
            self.kick(client, "MENU_Network_Error_version_mismatch");
            return;
        }
        if let Some(ip) = self.clients.get(&client).and_then(|c| c.ip.clone()) {
            if !self.access.ip_allowed(&ip) {
                info!("client {client}: refused ({ip})");
                self.kick(client, crate::access::REFUSED);
                return;
            }
        }
        // A banned SteamID never gets the world (names are only known at PlayerReady).
        let steam_id = self.clients.get(&client).and_then(|c| c.verified_steam_id);
        if let Some(id) = steam_id.filter(|id| self.access.steam_id_banned(*id)) {
            info!("client {client}: refused (SteamID {id} is banned)");
            self.kick(client, crate::access::REFUSED);
            return;
        }
        // Nor does one the whitelist can no longer let in: it lists SteamIDs only, not this
        // client's (a name entry may still match at PlayerReady).
        let whitelist = &self.access.lists.whitelist;
        let steam_entry = |v: &String| v.parse::<u64>().ok().filter(|id| *id >= crate::players::FIRST_STEAM_ID);
        if !whitelist.is_empty() && whitelist.iter().all(|v| steam_entry(v).is_some_and(|id| Some(id) != steam_id)) {
            info!("client {client}: refused (not on the whitelist)");
            self.kick(client, crate::access::REFUSED);
            return;
        }
        // A forged source address gets a session without seeing what it is sent: at most
        // MAX_JOINING_PER_IP worlds go to one address before its players are in (loopback is
        // exempt: nobody outside can forge it, and behind a UDP relay every player has it).
        if let Some(ip) = self.clients.get(&client).and_then(|c| c.ip.clone()).filter(|ip| !ip.parse().is_ok_and(crate::utp::is_loopback)) {
            let joining = self.clients.values().filter(|c| c.id != client && c.approved && c.ready.is_none() && c.ip.as_deref() == Some(ip.as_str())).count();
            if joining >= MAX_JOINING_PER_IP {
                info!("client {client}: refused ({joining} others from {ip} are still joining)");
                self.kick(client, "Another player from your address is still joining: try again in a moment");
                return;
            }
        }
        let max = self.rules.max_players();
        if self.clients.values().filter(|c| c.approved).count() >= max {
            info!("client {client}: refused (server full, {max} players)");
            self.kick(client, "MULTIPLAYER_Game_Session_Full");
            return;
        }
        // The world is sent to a session at the rate a forged address could make the server reflect it.
        if self.clients.get(&client).and_then(|c| c.ip.clone()).is_some_and(|ip| !ip.parse().is_ok_and(crate::utp::is_loopback)) && !self.limits.join(std::time::Instant::now(), max) {
            info!("client {client}: refused (worlds are being sent to new sessions as fast as they are allowed)");
            self.kick(client, "Many players are joining at once: try again in a moment");
            return;
        }
        let mut o = NetObj::from_layout(&self.net_data.prefabs["Player"].layout(self.tick_number()));
        o.owner = client;
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
            o.scene_object_into(&mut w, self.net.scene_handle);
        }
        w.0
    }

    /// One batch from a client.
    pub fn receive(&mut self, client: u64, batch: &[u8]) {
        for (kind, body) in wire::messages(batch) {
            // Kicked (refused at its request or PlayerReady, banned): nothing more counts.
            if self.kicks.iter().any(|(c, _)| *c == client) {
                return;
            }
            if self.rules.log.trace {
                trace!("trace client {client} -> {kind} {}", crate::util::to_hex(body));
            }
            let approved = self.clients.get(&client).is_some_and(|c| c.approved);
            let now = self.clock();
            if approved && matches!(kind, NETWORK_TRANSFORM | NETWORK_VARIABLE_DELTA) && !self.limits.motion(client, body.len(), now) {
                continue;
            }
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
        let update = crate::transform::update(body);
        // Passed on to everyone, so a state that is cut or garbage after its object id is not: the
        // other clients would read past its end. (Half-float states are relayed undecoded.)
        if update.is_none() && !crate::transform::is_half_float_state(body) {
            self.limits.warn(client, "a transform state that is cut or not a state, dropped");
            return;
        }
        // NaN or infinity: neither kept (the snapshot and save could not be read back) nor
        // passed on; the next state moves the object on.
        if update.as_ref().is_some_and(|u| !u.finite()) {
            self.limits.warn(client, "a transform that is not a number (NaN or infinity), dropped");
            return;
        }
        if let Some(u) = update.filter(|u| !u.local) {
            if let Some(o) = self.net.objects.get_mut(&u.object) {
                // Finite values can still overflow once merged onto the last state.
                let Some((position, rotation)) = u.applied(o.position, o.rotation) else {
                    self.limits.warn(client, "a transform that overflows to NaN or infinity, dropped");
                    return;
                };
                (o.position, o.rotation) = (position, rotation);
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
        let Some(method) = rpc_method(body) else { return };
        // Until PlayerReady (the name and whitelist checks) only what a loading client asks.
        let loading = self.clients.get(&client).is_none_or(|c| c.ready.is_none());
        if let Some(name) = self.net.spec.get(method).map(|r| r.name.as_str()).filter(|n| loading && !LOADING_RPCS.contains(n)) {
            if self.unhandled.insert(format!("{name} before PlayerReady")) {
                warn!("client {client}: {name} before PlayerReady, dropped (logged once)");
            }
            return;
        }
        if self.net.spec.get(method).is_some_and(|r| r.name == "ChatHandler.SendChatMessageServerRpc") && !self.limits.chat(client, self.clock()) {
            return;
        }
        // A tick's work per client is bounded; the rest waits for the next ticks (tick()).
        if !self.limits.spend(client, self.rpc_cost(body), body) {
            return;
        }
        self.run_rpc(client, body);
    }

    /// The work of an RPC (handlers::rpc_cost).
    fn rpc_cost(&self, body: &[u8]) -> u32 {
        let name = rpc_method(body).and_then(|m| self.net.spec.get(m)).map(|r| r.name.as_str());
        crate::handlers::rpc_cost(name.unwrap_or(""), self.wos.len(), body.len())
    }

    fn run_rpc(&mut self, client: u64, body: &[u8]) {
        let mut r = Reader::new(body);
        let (Some(_sender), Some(object), Some(behaviour), Some(method)) = (r.packed_u64(), r.packed_u64(), r.packed_u16(), r.packed_u32()) else { return };
        if self.animator_relay(client, object, behaviour, method, r.rest()) {
            return;
        }
        let Some(args) = self.net.spec.decode(method, r.rest()) else {
            // Logged once for each RPC of the game's, not for each message (a client can send them by
            // the thousand) and not at all for a number that is none of them (nor kept: there are 2^32).
            if self.net.spec.get(method).is_some() && self.unhandled.insert(format!("undecodable {method}")) {
                warn!("client {client}: undecodable RPC {method} (logged once)");
            }
            return;
        };
        let Some(name) = self.net.spec.get(method).map(|r| r.name.clone()) else { return };
        if self.rules.log.trace {
            trace!("trace client {client} rpc {name} on {object}/{behaviour} {}", Value::Array(args.clone()));
        }
        // Netcode runs an RPC on a behaviour of the object it is sent to: one of the RPC's class
        // or a subclass. Sent to any other object it runs nowhere (handlers here go by name).
        let class = name.split('.').next().unwrap_or("");
        if !self.net.carries(object, class) {
            if self.misrouted.insert(name.clone()) {
                warn!("client {client}: rpc {name} on object {object}, which has no {class}: ignored (logged once per RPC)");
            }
            return;
        }
        if OWNER_ONLY_RPCS.contains(&name.as_str()) && self.net.objects.get(&object).is_none_or(|o| o.owner != client) {
            if self.misrouted.insert(format!("{name} by a client that is not the owner")) {
                warn!("client {client}: rpc {name} on object {object}, which it does not own: ignored (logged once per RPC)");
            }
            return;
        }
        self.rpc(client, object, behaviour, &name, Args(args));
    }

    /// NetworkAnimator.Send*ServerRpc of an owner-authoritative animator (the animals'
    /// ClientNetworkAnimator): as Netcode, only from the owner, passed on unchanged as the
    /// matching ClientRpc to every client but the sender. The message is INetworkSerializable,
    /// so its bytes go through undecoded.
    // ponytail: the server keeps no animator, so a late joiner gets the spawn-time parameters
    // until the owner's next change; keep the last ParametersUpdate per object if that shows.
    pub(crate) fn animator_relay(&mut self, client: u64, object: u64, behaviour: u16, method: u32, body: &[u8]) -> bool {
        let Some(name) = self.net.spec.get(method).map(|r| r.name.clone()) else { return false };
        let Some(client_rpc) = name.strip_prefix("NetworkAnimator.").and_then(|m| m.strip_suffix("ServerRpc")).map(|m| format!("NetworkAnimator.{m}ClientRpc")) else { return false };
        let Some(hash) = self.net.spec.hash(&client_rpc) else {
            warn!("rpc {name}: no {client_rpc} to pass it on");
            return true;
        };
        if self.net.objects.get(&object).is_some_and(|o| o.owner == client) && body.len() <= MAX_ANIMATOR_MESSAGE {
            let mut w = Writer::new();
            w.packed_u64(object).packed_u16(behaviour).packed_u32(hash).bytes(body);
            self.out_to(Target::Except(client), (protocol::CLIENT_RPC, w.0));
        }
        true
    }

    /// Hands an RPC to the ported handler that owns it.
    pub(crate) fn rpc(&mut self, client: u64, object: u64, behaviour: u16, name: &str, a: Args) -> bool {
        // A plugin that may refuse this RPC did: it does not run (and is not "unhandled").
        if crate::events::rpc_before(self, client, object, name, &a.0) {
            return true;
        }
        let handled = self.world_objects_rpc(client, name, &a)
            || self.inventories_rpc(client, name, &a)
            || self.players_rpc(client, name, &a)
            || crate::gauges::rpc(self, client, object, name, &a)
            || crate::unlocks::rpc(self, client, name, &a)
            || crate::handlers::rpc(self, client, object, behaviour, name, &a)
            || crate::hooks::rpc(self, client, object, behaviour, name, &a);
        if handled {
            crate::events::rpc_after(self, client, object, name, &a.0);
        }
        if !handled && self.unhandled.insert(name.to_string()) {
            warn!("rpc {name} is not handled");
        }
        handled
    }

    pub fn kick(&mut self, client: u64, reason: &str) {
        self.kicks.push((client, reason.to_string()));
    }

    /// GameSettingsHandler.ChangeGenerationForGroups: the world's pace and power settings
    /// scale every building's generation.
    pub(crate) fn change_generation_for_groups(&mut self) {
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
        let clock = self.clock();
        let last = std::mem::replace(&mut self.last_clock, clock);
        // Time.maximumDeltaTime: the game's time goes on by one frame at most across a stall (a
        // stopped process, a long save, a busy host), as Unity's does. What is over is the world
        // standing still, like a pause: no timer is due for it, no gauge drains for it. Before the
        // RPCs: they read the network tick, which must not jump and fall back.
        let stall = (clock - last - crate::clock::MAX_FRAME).max(0.0);
        if stall >= 1.0 {
            info!("world: the game loop was away {:.1} s (a stopped process, a long save or a busy host): the world's time went on by {:.2} s only, as a game's does across a hitch", stall + crate::clock::MAX_FRAME, crate::clock::MAX_FRAME);
        }
        self.paused += stall;
        section!("rpcs", crate::handlers::run_pending(self, |g, rpc| g.rpc_cost(rpc), |g, client, rpc| g.run_rpc(client, rpc)));
        let empty = self.rules.world.pause_when_empty && !self.clients.values().any(|c| c.ready.is_some());
        if empty || self.held {
            // Nobody in the game, or paused by the admin: the clock (game time and network ticks)
            // holds, and every timer with it; it goes on from here. Clients still hear it each
            // second; players in a paused world still get the chat.
            self.paused += clock - last - stall;
            if empty {
                crate::chathooks::drop_replies(self);
            } else {
                // Players in a world the admin paused still act (run_pending above): the
                // autosave must not skip what they did.
                self.advanced |= self.clients.values().any(|c| c.ready.is_some());
                crate::chathooks::tick(self);
            }
            if clock.floor() > last.floor() {
                self.every_second();
            }
            self.net.flush();
            return;
        }
        self.time = crate::clock::GameTime::at(clock - self.paused);
        self.advanced = true;
        section!("scene", crate::scene::tick(self));
        if self.time >= self.next_time_sync {
            self.next_time_sync = self.time + 1.0;
            section!("every_second", self.every_second());
        }
        section!("flush", self.net.flush());
    }

    /// The new bank of a flip failed and this one goes on: the time it stood since its last tick is
    /// not the world's, whatever its length.
    pub fn resume_after_stand(&mut self) {
        let clock = self.clock();
        self.paused += clock - self.last_clock;
        self.last_clock = clock;
    }

    fn every_second(&mut self) {
        let now = crate::util::now_secs();
        if self.rules.players.forget_after_days > 0 && now / 3600 != self.forgot_hour {
            self.forgot_hour = now / 3600;
            crate::players::forget_unseen(self, now);
        }
        let tick = self.tick_number();
        self.out_to(Target::All, protocol::time_sync(tick));
        // A connection that never becomes a player (no request, no PlayerReady) has the world
        // without the checks PlayerReady makes: give it two minutes.
        let stalled: Vec<u64> = self.clients.values().filter(|c| c.ready.is_none() && c.since.elapsed().as_secs() > 120).map(|c| c.id).collect();
        for client in stalled {
            info!("client {client}: never became a player, disconnecting");
            self.kick(client, "MENU_Network_Error_Timeout");
        }
    }

    /// The full save as JSON trees (what the tests read; the server writes save_text).
    #[cfg(test)]
    pub fn snapshot(&mut self) -> SaveFile {
        let mut save = self.snapshot_head();
        let data = &self.data;
        save.objects = self.wos.values().filter(|w| !w.dont_save).map(|w| w.to_json(data)).collect();
        save.inventories = self.inventories.values().filter(|i| i.id >= 0).map(|i| i.to_json(data)).collect();
        save
    }

    /// Everything of the save but the world objects and the inventories.
    fn snapshot_head(&mut self) -> SaveFile {
        crate::players::update_saved_players(self);
        let mut save = SaveFile {
            world: self.save.world.clone(),
            planets: self.save.planets.clone(),
            players: self.saved_players.clone(),
            objects: Vec::new(),
            inventories: Vec::new(),
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

    /// `snapshot().to_text()`: the same bytes, the world objects and inventories written
    /// straight to the text without a JSON tree of each (a third of the time, and no 3.5 KB of
    /// tree per world object in memory).
    pub fn save_text(&mut self) -> String {
        let save = self.snapshot_head();
        let data = &self.data;
        let objects = |out: &mut String| {
            let mut first = true;
            for w in self.wos.values().filter(|w| !w.dont_save) {
                if !std::mem::take(&mut first) {
                    out.push_str("|\n");
                }
                w.write_save(data, out);
            }
        };
        let inventories = |out: &mut String| {
            let mut first = true;
            for i in self.inventories.values().filter(|i| i.id >= 0) {
                if !std::mem::take(&mut first) {
                    out.push_str("|\n");
                }
                i.write_save(data, out);
            }
        };
        save.text_with(Some((&objects, &inventories)))
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

    #[test]
    fn planet_units_and_players_spawn_as_the_game_spawns_them() {
        let Some(mut g) = test_game(save()) else { return };
        g.sim_time = Some(12.0);
        approve(&mut g, 1);
        let body = g.net.out.iter().find(|(to, kind, _)| *to == Target::Only(1) && *kind == CONNECTION_APPROVED).map(|m| m.2.clone()).unwrap();
        let (_, objects) = protocol::read_approved(&body).unwrap();
        // WorldUnitsHandler.CreateUnits spawns the planet's units below the handler.
        let handler = g.net.find_class("WorldUnitsHandler").unwrap().0;
        let units = objects.iter().find(|o| o.hash == g.net_data.prefabs["WorldUnitsPlanet"].hash).unwrap();
        assert_eq!((units.parent, units.flags & protocol::HAS_PARENT != 0), (Some(handler), true));
        // The player's NetworkTransform states carry the tick it spawned at.
        let player = objects.iter().find(|o| o.hash == g.net_data.prefabs["Player"].hash).unwrap();
        let vars: usize = g.net_data.prefabs["Player"].behaviours.iter().flat_map(|b| &b.vars).map(|v| v.bytes.as_ref().unwrap().len() / 2).sum();
        // The variables, then: transform count (u8), behaviour (u16), length (i32), flags, tick.
        let mut r = crate::wire::Reader::new(&player.sync[vars + 1 + 2 + 4..]);
        r.packed_u32().unwrap();
        assert_eq!(r.packed_i32(), Some(g.tick_number()));
    }

    /// Connects a client and readies its player (PlayerReadyServerRpc); returns the player object.
    pub fn join(game: &mut Game, client: u64, name: &str) -> u64 {
        let object = approve(game, client);
        game.players_rpc(client, "PlayerMainController.PlayerReadyServerRpc", &Args(vec![json!(0), json!(name)]));
        assert!(game.clients[&client].ready.is_some());
        object
    }

    /// A stall (SIGSTOP, a long save, a GC-like gap of the loop) is not game time past one frame, as
    /// in Unity (Time.maximumDeltaTime): after a gap of any length the world is exactly where one
    /// third of a second after the last tick leaves it: the same game time, network tick, player
    /// gauges and terraformation, no fast-forward and no 40x drain while the gauges catch up.
    #[test]
    fn a_stall_of_the_loop_is_not_game_time_past_one_frame() {
        let setup = || {
            let mut g = test_game(save())?;
            let ann = join(&mut g, 1, "Ann");
            // Two seconds of the game in frames of 50 ms.
            for i in 1..=40 {
                g.sim_time = Some(f64::from(i) * 0.05);
                g.tick();
            }
            Some((g, ann))
        };
        let Some((control, ann)) = setup() else { return };
        let after = |gap: f64, later: &dyn Fn(&mut Game)| {
            let (mut g, _) = setup().unwrap();
            g.sim_time = Some(2.0 + gap);
            g.tick();
            later(&mut g);
            g
        };
        let more = |g: &mut Game| {
            // (Not on a whole tick of the network: the stalled clocks differ from the control's by
            // a rounding that a tick boundary would show.)
            for _ in 0..40 {
                g.sim_time = Some(g.sim_time.unwrap() + 0.0501);
                g.tick();
            }
        };
        let seen = |g: &Game| (g.time, g.tick_number(), crate::gauges::values(g, ann), crate::units::value(g, crate::units::Kind::Terraformation, 0));
        // The reference: one frame of exactly the longest a frame may be.
        let one_frame = after(crate::clock::MAX_FRAME, &more);
        assert!((one_frame.time.secs() - (2.0 + crate::clock::MAX_FRAME + 2.004)).abs() < 1e-3, "{}", one_frame.time);
        for gap in [0.34, 1.5, 10.0, 600.0, 86_400.0] {
            let g = after(gap, &more);
            assert_eq!(seen(&g), seen(&one_frame), "a gap of {gap} s");
        }
        // A frame shorter than the limit counts whole; the world is not slowed otherwise.
        let short = after(0.3, &more);
        assert!((short.time.secs() - (2.0 + 0.3 + 2.004)).abs() < 1e-3, "{}", short.time);
        let before = seen(&control);
        assert!((before.0.secs() - 2.0).abs() < 1e-3);
    }

    /// The new bank of a flip failed and the old one goes on: the time it stood is not the world's,
    /// whatever its length, and the network tick does not jump.
    #[test]
    fn the_stand_of_a_failed_flip_is_not_game_time() {
        let Some(mut g) = test_game(save()) else { return };
        join(&mut g, 1, "Ann");
        for i in 1..=20 {
            g.sim_time = Some(f64::from(i) * 0.05);
            g.tick();
        }
        let (time, tick) = (g.time, g.tick_count());
        // The snapshot is taken and the front decides: 0.3 s later (under a frame) the old bank resumes.
        g.sim_time = Some(1.3);
        g.resume_after_stand();
        assert_eq!((g.time, g.tick_count()), (time, tick), "the clock of the tick stands as it was");
        g.sim_time = Some(1.35);
        g.tick();
        assert!((g.time.secs() - (time.secs() + 0.05)).abs() < 1e-3, "the first frame after is a normal one: {}", g.time);
        // And again after a stand much longer than a frame.
        let time = g.time;
        g.sim_time = Some(31.35);
        g.resume_after_stand();
        g.sim_time = Some(31.40);
        g.tick();
        assert!((g.time.secs() - (time.secs() + 0.05)).abs() < 1e-3, "{}", g.time);
    }

    /// ConnectionRequest: another game version, another session's invite code, a refused
    /// address, a banned SteamID or a full server are each told why and dropped; the rest get
    /// the world and everyone hears they came; leaving, everyone hears they went.
    #[test]
    fn connections_are_refused_as_the_game_and_the_access_lists_say() {
        let Some(mut g) = test_game(save()) else { return };
        let request = |data: &str| {
            let mut w = Writer::new();
            w.packed_i32(0).u64(0).i32(data.len() as i32).bytes(data.as_bytes());
            w.0
        };
        let version = g.net_data.game_version.clone();
        g.access.lists.banned_ips.push("10.6.6.6".into());
        g.access.lists.banned_names.push("76561198000000066".into());
        let cases: [(u64, Option<u64>, Option<&str>, String, &str); 4] = [
            (1, None, None, "0.9|0".to_string(), "MENU_Network_Error_version_mismatch"),
            (2, None, None, format!("{version}|{}", g.session_number.wrapping_add(1)), "MENU_Network_Error_version_mismatch"),
            (3, None, Some("10.6.6.6"), format!("{version}|0"), crate::access::REFUSED),
            (4, Some(76561198000000066), None, format!("{version}|0"), crate::access::REFUSED),
        ];
        for (client, steam, ip, data, why) in cases {
            g.connect(client, steam, false, ip.map(String::from));
            g.approve(client, &request(&data));
            assert!(!g.clients[&client].approved && g.kicks.contains(&(client, why.to_string())), "client {client}");
        }
        g.rules.limits.max_players = Some(1);
        join(&mut g, 5, "Ann");
        g.connect(6, None, false, None);
        g.approve(6, &request(&format!("{version}|{}", g.session_number)));
        assert!(g.kicks.contains(&(6, "MULTIPLAYER_Game_Session_Full".to_string())));
        g.net.out.clear();
        g.disconnect(5);
        assert!(g.net.out.iter().any(|(to, kind, _)| *to == Target::Except(5) && *kind == protocol::client_disconnected(5).0));
        g.disconnect(6);
        g.disconnect(99);
    }

    /// Every animator ServerRpc an owner sends goes out as its ClientRpc.
    #[test]
    fn every_animator_server_rpc_is_relayed() {
        let Some(mut g) = test_game(save()) else { return };
        let ann = join(&mut g, 1, "Ann");
        for (server, client) in [("NetworkAnimator.SendAnimStateServerRpc", "NetworkAnimator.SendAnimStateClientRpc"), ("NetworkAnimator.SendParametersUpdateServerRpc", "NetworkAnimator.SendParametersUpdateClientRpc"), ("NetworkAnimator.SendAnimTriggerServerRpc", "NetworkAnimator.SendAnimTriggerClientRpc")] {
            let mut w = Writer::new();
            w.packed_u64(1).packed_u64(ann).packed_u16(7).packed_u32(g.net.spec.hash(server).unwrap()).bytes(&[9, 9]);
            g.net.out.clear();
            g.receive(1, &wire::batch(&[(protocol::RPC, w.0)]));
            let mut want = Writer::new();
            want.packed_u64(ann).packed_u16(7).packed_u32(g.net.spec.hash(client).unwrap()).bytes(&[9, 9]);
            assert_eq!(g.net.out.iter().filter(|m| m.1 == protocol::CLIENT_RPC).map(|m| (m.0, m.2.clone())).collect::<Vec<_>>(), vec![(Target::Except(1), want.0)], "{server}");
        }
    }

    /// An animal's owner animates it for everyone else; nobody else can.
    #[test]
    fn animator_server_rpcs_reach_the_other_clients_as_client_rpcs() {
        let Some(mut g) = test_game(save()) else { return };
        let ann = join(&mut g, 1, "Ann");
        join(&mut g, 2, "Bob");
        let message = vec![3, 1, 4, 1, 5];
        let send = |g: &mut Game, client: u64, message: &[u8]| {
            let mut w = Writer::new();
            w.packed_u64(client).packed_u64(ann).packed_u16(7).packed_u32(g.net.spec.hash("NetworkAnimator.SendAnimTriggerServerRpc").unwrap()).bytes(message);
            g.net.out.clear();
            g.receive(client, &wire::batch(&[(protocol::RPC, w.0)]));
            g.net.out.iter().filter(|m| m.1 == protocol::CLIENT_RPC).map(|(to, _, body)| (*to, body.clone())).collect::<Vec<_>>()
        };
        let mut w = Writer::new();
        w.packed_u64(ann).packed_u16(7).packed_u32(g.net.spec.hash("NetworkAnimator.SendAnimTriggerClientRpc").unwrap()).bytes(&message);
        assert!(matches!(&send(&mut g, 1, &message)[..], [(Target::Except(1), body)] if *body == w.0));
        assert!(send(&mut g, 2, &message).is_empty(), "not the owner");
        // What goes to everyone else is an animator's message, not a megabyte of anything.
        assert_eq!(send(&mut g, 1, &vec![7u8; MAX_ANIMATOR_MESSAGE]).len(), 1, "up to the limit");
        assert!(send(&mut g, 1, &vec![7u8; MAX_ANIMATOR_MESSAGE + 1]).is_empty(), "past it");
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

    /// Two sessions from one address may be joining at once; a third waits until one is in.
    /// Loopback is exempt.
    #[test]
    fn at_most_two_clients_from_one_address_are_joining() {
        let Some(mut g) = test_game(save()) else { return };
        let request = |g: &Game| {
            let data = format!("{}|0", g.net_data.game_version);
            let mut w = Writer::new();
            w.packed_i32(0).u64(0).i32(data.len() as i32).bytes(data.as_bytes());
            w.0
        };
        for (client, ip) in [(1, "10.0.0.5"), (2, "10.0.0.5"), (3, "10.0.0.5"), (4, "10.0.0.6"), (5, "127.0.0.1"), (6, "127.0.0.1"), (7, "127.0.0.1")] {
            g.connect(client, None, false, Some(ip.into()));
            let r = request(&g);
            g.approve(client, &r);
        }
        let approved: Vec<u64> = g.clients.values().filter(|c| c.approved).map(|c| c.id).collect();
        assert_eq!(approved, [1, 2, 4, 5, 6, 7]);
        assert_eq!(g.kicks.iter().map(|(c, _)| *c).collect::<Vec<_>>(), [3]);
        g.players_rpc(1, "PlayerMainController.PlayerReadyServerRpc", &Args(vec![json!(0), json!("Ann")]));
        g.connect(8, None, false, Some("10.0.0.5".into()));
        let r = request(&g);
        g.approve(8, &r);
        assert!(g.clients[&8].approved, "Ann is in: one joining from her address");
    }

    /// Sessions from other machines get the world at a bounded rate (a forged address makes the
    /// server reflect it): a burst, then one a second; sessions from this machine, and Steam's
    /// (no address), are not held.
    #[test]
    fn worlds_go_to_sessions_from_other_machines_at_a_bounded_rate() {
        let Some(mut g) = test_game(save()) else { return };
        let request = |g: &Game| {
            let data = format!("{}|0", g.net_data.game_version);
            let mut w = Writer::new();
            w.packed_i32(0).u64(0).i32(data.len() as i32).bytes(data.as_bytes());
            w.0
        };
        let mut told = 0;
        for client in 1..=60u64 {
            g.connect(client, None, false, Some(format!("203.0.113.{client}")));
            let r = request(&g);
            g.approve(client, &r);
            told += g.net.out.iter().filter(|(to, kind, _)| *to == Target::Only(client) && *kind == CONNECTION_APPROVED).count();
            g.net.out.clear();
            g.disconnect(client);
        }
        assert!((16..=18).contains(&told), "the burst, and not much more within a few milliseconds: {told}");
        assert!(g.kicks.iter().all(|(_, why)| why.contains("joining at once")) && g.kicks.len() == 60 - told);
        // From this machine (a relay's players, the tests') and over Steam: no limit.
        for client in 100..160u64 {
            g.connect(client, None, false, Some("127.0.0.1".into()));
            let r = request(&g);
            g.approve(client, &r);
            assert!(g.clients[&client].approved, "{client}");
            g.disconnect(client);
        }
    }

    /// What the server remembers of RPCs it could not run is bounded by the RPCs there are, not by
    /// the numbers a client sends.
    #[test]
    fn rpc_numbers_that_are_none_are_not_remembered() {
        let Some(mut g) = test_game(save()) else { return };
        join(&mut g, 1, "Ann");
        let object = *g.net.objects.keys().next().unwrap();
        let before = g.unhandled.len();
        for method in 1..5000u32 {
            let mut w = Writer::new();
            w.packed_u64(1).packed_u64(object).packed_u16(0).packed_u32(method.wrapping_mul(2_654_435_761)).bytes(&[1, 2, 3]);
            g.run_rpc(1, &w.0);
        }
        assert_eq!(g.unhandled.len(), before);
    }

    /// A dropped item's owner sends NaN: nothing changes, nothing is passed on, and the
    /// snapshot still reads back.
    #[test]
    fn non_finite_transforms_are_dropped() {
        let Some(mut g) = test_game(save()) else { return };
        join(&mut g, 1, "Ann");
        join(&mut g, 2, "Bob");
        let iron = g.create_world_object(g.data.group("Iron").unwrap(), 0, true);
        g.drop_on_floor(iron, [560.0, 3.0, 500.0], [0.0, 0.0, 0.0, 1.0], 0.0, false, false, 0, 0.0, false, 1);
        let net = g.wos[&iron].net.unwrap();
        g.net.out.clear();
        for bad in [f32::NAN, f32::INFINITY] {
            g.transform(1, &transform_message(net, 2 | 4 | 8, &[bad, 1.0, 2.0]));
            g.transform(1, &transform_message(net, 32, &[bad]));
        }
        assert!(g.net.out.is_empty(), "not passed on");
        assert_eq!((g.wos[&iron].position, g.net.objects[&net].position), ([560.0, 3.0, 500.0], [560.0, 3.0, 500.0]));
        assert!(crate::handoff::snapshot(&mut g).is_ok());
        g.transform(1, &transform_message(net, 2, &[561.0]));
        assert_eq!(g.wos[&iron].position[0], 561.0);
        assert_eq!(g.net.out.len(), 1);
    }

    /// A NetworkVariableDelta for `object`'s behaviour of `class`: `var` set to `bytes`.
    pub fn delta(g: &Game, object: u64, class: &str, var: &str, bytes: &[u8]) -> Vec<u8> {
        let b = g.net.objects[&object].behaviour(class).unwrap();
        let mut w = Writer::new();
        w.packed_u64(object).packed_u16(b.index).i32(4);
        for v in &b.vars {
            w.bool(v.name == var);
            if v.name == var {
                w.bytes(bytes);
            }
        }
        w.0
    }

    /// A float variable the owner writes, as NaN or infinity: neither kept nor passed on.
    #[test]
    fn non_finite_owner_variables_are_dropped() {
        let Some(mut g) = test_game(save()) else { return };
        join(&mut g, 1, "Ann");
        let iron = g.create_world_object(g.data.group("Iron").unwrap(), 0, true);
        g.drop_on_floor(iron, [560.0, 3.0, 500.0], [0.0, 0.0, 0.0, 1.0], 0.0, false, false, 0, 0.0, false, 1);
        let net = g.wos[&iron].net.unwrap();
        let scale = |g: &Game| g.net.get(net, "GroupNetworkContainer", "_scale").cloned();
        let before = scale(&g);
        g.net.out.clear();
        for bad in [f32::NAN, f32::NEG_INFINITY] {
            let d = delta(&g, net, "GroupNetworkContainer", "_scale", &bad.to_le_bytes());
            g.receive(1, &wire::batch(&[(NETWORK_VARIABLE_DELTA, d)]));
        }
        assert!(g.net.out.is_empty());
        assert_eq!(format!("{:?}", scale(&g)), format!("{before:?}"));
        let d = delta(&g, net, "GroupNetworkContainer", "_scale", &0.5f32.to_le_bytes());
        g.receive(1, &wire::batch(&[(NETWORK_VARIABLE_DELTA, d)]));
        assert!(matches!(scale(&g), Some(crate::net::VarValue::Plain(b)) if b == 0.5f32.to_le_bytes()));
        assert_eq!(g.net.out.len(), 1, "passed on");
    }

    /// A transform state that is garbage after the object id the sender owns (or cut) is not relayed;
    /// whole ones, and half-float ones (relayed undecoded), are.
    #[test]
    fn a_transform_state_that_is_garbage_is_not_passed_on() {
        let Some(mut g) = test_game(save()) else { return };
        let ann = join(&mut g, 1, "Ann");
        join(&mut g, 2, "Bob");
        g.sim_time = Some(100.0);
        g.net.out.clear();
        let relayed = |g: &Game| g.net.out.iter().filter(|(to, kind, _)| *kind == NETWORK_TRANSFORM && to.reaches(2)).count();
        let mut whole = Writer::new();
        whole.packed_u64(ann).packed_i32(0).packed_u32(2).packed_i32(1).bytes(&1.0f32.to_le_bytes());
        for cut in 0..whole.0.len() {
            g.receive(1, &wire::batch(&[(NETWORK_TRANSFORM, whole.0[..cut].to_vec())]));
        }
        let mut junk = Writer::new();
        junk.packed_u64(ann).bytes(&[0xff; 7]);
        g.receive(1, &wire::batch(&[(NETWORK_TRANSFORM, junk.0)]));
        assert_eq!(relayed(&g), 0, "cut and garbage states");
        g.receive(1, &wire::batch(&[(NETWORK_TRANSFORM, whole.0.clone())]));
        let mut half = Writer::new();
        half.packed_u64(ann).packed_i32(0).packed_u32(2 | (1 << 14)).packed_i32(1).bytes(&[1, 2]);
        g.receive(1, &wire::batch(&[(NETWORK_TRANSFORM, half.0)]));
        assert_eq!(relayed(&g), 2, "a whole state, and a half-float one");
    }

    /// Netcode writes a dirty flag for each variable of the behaviour: a delta that ends before the
    /// last is not passed on (the other clients would read past its end), whatever it set before.
    #[test]
    fn a_cut_variable_delta_is_not_passed_on() {
        let Some(mut g) = test_game(save()) else { return };
        join(&mut g, 1, "Ann");
        let iron = g.create_world_object(g.data.group("Iron").unwrap(), 0, true);
        g.drop_on_floor(iron, [561.0, 3.0, 497.0], [0.0, 0.0, 0.0, 1.0], 0.0, false, false, 0, 0.0, false, 1);
        let net = g.wos[&iron].net.unwrap();
        join(&mut g, 2, "Bob");
        g.net.out.clear();
        let whole = delta(&g, net, "GroupNetworkContainer", "_scale", &0.5f32.to_le_bytes());
        for cut in 1..whole.len() - 4 {
            g.receive(1, &wire::batch(&[(NETWORK_VARIABLE_DELTA, whole[..cut].to_vec())]));
            assert!(g.net.out.is_empty(), "a delta cut at {cut} of {} bytes was passed on", whole.len());
        }
        g.receive(1, &wire::batch(&[(NETWORK_VARIABLE_DELTA, whole)]));
        assert_eq!(g.net.out.len(), 1, "the whole one is");
    }

    /// RPC floats come through JSON, which has no NaN or infinity: they read as 0.
    #[test]
    fn non_finite_rpc_floats_read_as_zero() {
        let Some(g) = test_game(save()) else { return };
        let name = "WorldObjectsHandler.HasWorldObjectAtPositionServerRpc";
        let (method, mut body) = g.net.spec.encode(name, &[json!([1.0, 2.0, 3.0])]);
        let at = body.len() - 12;
        body.splice(at.., [f32::NAN, f32::INFINITY, 1.0].iter().flat_map(|f| f.to_le_bytes()));
        assert_eq!(Args(g.net.spec.decode(method, &body).unwrap()).v3(0), [0.0, 0.0, 1.0]);
    }

    /// An RPC to `object`, or with 0 to the scene handler of its class (RPCs run only on an
    /// object that has their class).
    fn rpc_batch(g: &Game, client: u64, object: u64, name: &str, args: &[Value]) -> Vec<u8> {
        let (method, body) = g.net.spec.encode(name, args);
        let (object, behaviour) = match object {
            0 => g.net.find_class(name.split('.').next().unwrap()).unwrap(),
            o => (o, 0),
        };
        let mut w = Writer::new();
        w.packed_u64(client).packed_u64(object).packed_u16(behaviour).packed_u32(method).bytes(&body);
        wire::batch(&[(protocol::RPC, w.0)])
    }

    /// Before PlayerReady only what a loading client asks is answered; after it, everything.
    /// A client refused at PlayerReady gets nothing more done, even in the same batch.
    #[test]
    fn rpcs_wait_for_player_ready_and_stop_at_a_kick() {
        let Some(mut g) = test_game(save()) else { return };
        let player = approve(&mut g, 1);
        let chat = |g: &Game| rpc_batch(g, 1, 0, "ChatHandler.SendChatMessageServerRpc", &[json!("hi")]);
        let chats = |g: &Game| g.net.out.iter().filter(|m| m.1 == protocol::CLIENT_RPC).count();
        g.net.out.clear();
        let b = chat(&g);
        g.receive(1, &b);
        assert_eq!(chats(&g), 0, "chat before PlayerReady is dropped");
        let b = rpc_batch(&g, 1, 0, "WorldObjectsHandler.IsWorldObjectFromSceneActiveServerRpc", &[json!(208217122)]);
        g.receive(1, &b);
        assert_eq!(chats(&g), 1, "a loading query is answered");
        g.net.out.clear();
        let b = rpc_batch(&g, 1, player, "PlayerMainController.PlayerReadyServerRpc", &[json!(0), json!("Ann")]);
        g.receive(1, &b);
        g.net.out.clear();
        let b = chat(&g);
        g.receive(1, &b);
        assert_eq!(chats(&g), 1, "a player chats");
        // Mallory is banned by name: refused at PlayerReady, her chat in the same batch is not sent.
        g.access.lists.banned_names = vec!["Mallory".into()];
        let player = approve(&mut g, 2);
        let mut both = wire::messages(&rpc_batch(&g, 2, player, "PlayerMainController.PlayerReadyServerRpc", &[json!(0), json!("Mallory")])).into_iter().map(|(k, b)| (k, b.to_vec())).collect::<Vec<_>>();
        both.extend(wire::messages(&rpc_batch(&g, 2, 0, "ChatHandler.SendChatMessageServerRpc", &[json!("spam")])).into_iter().map(|(k, b)| (k, b.to_vec())));
        g.net.out.clear();
        g.receive(2, &wire::batch(&both));
        assert!(g.kicks.iter().any(|(c, _)| *c == 2) && g.clients[&2].ready.is_none());
        assert_eq!(chats(&g), 0);
    }

    /// A chat line is cut at MAX_CHAT characters; past the burst, messages wait for the rate.
    #[test]
    fn chat_has_a_length_and_a_rate() {
        let Some(mut g) = test_game(save()) else { return };
        join(&mut g, 1, "Ann");
        let said = |g: &mut Game, text: &str| {
            g.net.out.clear();
            let b = rpc_batch(g, 1, 0, "ChatHandler.SendChatMessageServerRpc", &[json!(text)]);
            g.receive(1, &b);
            g.net.out.iter().find(|m| m.1 == protocol::CLIENT_RPC).map(|m| {
                let mut r = Reader::new(&m.2);
                let (_, _, method) = (r.packed_u64(), r.packed_u16(), r.packed_u32().unwrap());
                g.net.spec.decode(method, r.rest()).unwrap()[1].as_str().unwrap().chars().count()
            })
        };
        assert_eq!(said(&mut g, &"x".repeat(5000)), Some(crate::handlers::MAX_CHAT));
        for _ in 0..9 {
            assert_eq!(said(&mut g, "hi"), Some(2));
        }
        assert_eq!(said(&mut g, "hi"), None, "the 11th at once");
        // The clock runs on while the world is held (the game's time stands): players still chat.
        g.held = true;
        g.sim_time = Some(g.clock() + 1.0);
        g.tick();
        assert_eq!(said(&mut g, "hi"), Some(2), "one a second after that, held or not");
        assert_eq!(said(&mut g, "hi"), None);
    }

    /// 100 RPCs in one batch: 32 now, the rest in the next ticks, in order. A lookup over the
    /// whole world costs by its size.
    #[test]
    fn a_tick_does_a_bounded_share_of_one_clients_rpcs() {
        let Some(mut g) = test_game(save()) else { return };
        join(&mut g, 1, "Ann");
        let one = wire::messages(&rpc_batch(&g, 1, 0, "WorldObjectsHandler.IsWorldObjectFromSceneActiveServerRpc", &[json!(208217122)])).into_iter().map(|(k, b)| (k, b.to_vec())).next().unwrap();
        let answers = |g: &mut Game| {
            let n = g.net.out.iter().filter(|m| m.1 == protocol::CLIENT_RPC && m.0 == Target::Only(1)).count();
            g.net.out.clear();
            n
        };
        g.net.out.clear();
        g.receive(1, &wire::batch(&vec![one; 100]));
        assert_eq!(answers(&mut g), 32);
        let run = |g: &mut Game| crate::handlers::run_pending(g, |g, rpc| g.rpc_cost(rpc), |g, client, rpc| g.run_rpc(client, rpc));
        for n in [32, 32, 4, 0] {
            run(&mut g);
            assert_eq!(answers(&mut g), n);
        }
        assert_eq!(crate::handlers::rpc_cost("WorldObjectsHandler.HasWorldObjectAtPositionServerRpc", 50_000, 40), 49);
        // An RPC costs one more for each KiB it carries (at most the budget), so a list of thousands of items is not one RPC's work.
        assert_eq!((crate::handlers::rpc_cost("A.B", 0, 200), crate::handlers::rpc_cost("A.B", 0, 5000), crate::handlers::rpc_cost("A.B", 0, 1 << 20)), (1, 5, 33));
    }

    /// A whitelist of SteamIDs only: who is not on it never gets the world.
    #[test]
    fn a_steam_id_whitelist_is_checked_before_the_world_goes_out() {
        let Some(mut g) = test_game(save()) else { return };
        let listed = crate::players::FIRST_STEAM_ID + 42;
        g.access.lists.whitelist = vec![listed.to_string()];
        let request = |g: &Game| {
            let data = format!("{}|0", g.net_data.game_version);
            let mut w = Writer::new();
            w.packed_i32(0).u64(0).i32(data.len() as i32).bytes(data.as_bytes());
            w.0
        };
        for (client, steam) in [(1, Some(listed)), (2, Some(listed + 1)), (3, None)] {
            g.connect(client, steam, false, None);
            let r = request(&g);
            g.approve(client, &r);
        }
        assert_eq!(g.kicks.iter().map(|(c, _)| *c).collect::<Vec<_>>(), [2, 3]);
        assert!(g.clients[&1].approved && !g.clients[&2].approved);
        // With a name on it, everyone gets in to be checked at PlayerReady.
        g.access.lists.whitelist.push("Ann".into());
        g.connect(4, None, false, None);
        let r = request(&g);
        g.approve(4, &r);
        assert!(g.clients[&4].approved);
    }

    /// The owner writes its emote, not its health maximum or dying state (the server's).
    #[test]
    fn owners_write_only_the_variables_the_game_lets_them() {
        let Some(mut g) = test_game(save()) else { return };
        let player = join(&mut g, 1, "Ann");
        let bob = join(&mut g, 2, "Bob");
        let var = |g: &Game, class: &str, name: &str| format!("{:?}", g.net.get(player, class, name));
        let before = (var(&g, "PlayerGaugesHandler", "_healthMaxValue"), var(&g, "PlayerStatus", "_isDying"));
        g.net.out.clear();
        for (class, name, bytes) in [("PlayerGaugesHandler", "_healthMaxValue", 1000f32.to_le_bytes().to_vec()), ("PlayerStatus", "_isDying", vec![1])] {
            let d = delta(&g, player, class, name, &bytes);
            g.receive(1, &wire::batch(&[(NETWORK_VARIABLE_DELTA, d)]));
        }
        assert_eq!((var(&g, "PlayerGaugesHandler", "_healthMaxValue"), var(&g, "PlayerStatus", "_isDying")), before);
        assert!(g.net.out.is_empty(), "nothing passed on");
        let d = delta(&g, player, "PlayerShareState", "_emote", &crate::net::enc::short(3));
        g.receive(1, &wire::batch(&[(NETWORK_VARIABLE_DELTA, d.clone())]));
        assert_eq!(var(&g, "PlayerShareState", "_emote"), format!("{:?}", Some(crate::net::VarValue::Plain(crate::net::enc::short(3)))));
        assert!(matches!(&g.net.out[..], [(Target::Except(1), NETWORK_VARIABLE_DELTA, body)] if *body == d));
        g.net.out.clear();
        let d = delta(&g, bob, "PlayerShareState", "_emote", &crate::net::enc::short(3));
        g.receive(1, &wire::batch(&[(NETWORK_VARIABLE_DELTA, d)]));
        assert!(g.net.out.is_empty(), "not the owner");
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

    /// World objects of a group the tables do not know would be dropped at load and gone from the
    /// next save: the start is refused, naming the group.
    #[test]
    fn a_save_with_objects_the_tables_do_not_know_is_refused() {
        let Some(g) = test_game(save()) else { return };
        let mut s = save();
        assert!(check_save_content(&s, &g.data, &g.net_data).is_ok());
        s.objects.push(json!({"id": 205_000_001, "gId": "NoSuchThingInThisGame"}));
        s.objects.push(json!({"gId": "Iron"}));
        let err = check_save_content(&s, &g.data, &g.net_data).unwrap_err();
        assert!(err.contains("2 world objects") && err.contains("NoSuchThingInThisGame"), "{err}");
    }

    /// A save that names no planet to start on, or one these tables do not have, is refused with
    /// what is wrong; no planet is made up (Prime was, and an unknown one read as the first).
    #[test]
    fn a_save_without_a_known_planet_is_refused() {
        let Some(g) = test_game(save()) else { return };
        let check = |s: &SaveFile| check_save_content(s, &g.data, &g.net_data);
        assert!(check(&save()).is_ok());
        let mut s = save();
        s.settings.as_object_mut().unwrap().remove("planetId");
        let e = check(&s).unwrap_err();
        assert!(e.starts_with("settings.planetId is missing"), "{e}");
        let mut s = save();
        s.settings["planetId"] = json!("Nowhere");
        let e = check(&s).unwrap_err();
        assert!(e.contains("settings.planetId is \"Nowhere\"") && e.contains("pcds update"), "{e}");
        // The planet the world travelled to, and the host's, must be known too.
        let mut s = save();
        s.world["pcdsCurrentPlanet"] = json!("Elsewhere");
        assert!(check(&s).unwrap_err().contains("world.pcdsCurrentPlanet is \"Elsewhere\""));
        let mut s = save();
        s.players = vec![json!({"id": 5, "name": "Old", "host": true, "planetId": "Elsewhere"})];
        assert!(check(&s).unwrap_err().contains("host player's planetId is \"Elsewhere\""));
        assert_eq!(world_planet(&save(), &g.net_data).unwrap().0, "Prime");
        let mut s = save();
        s.world["pcdsCurrentPlanet"] = json!("Selenea");
        assert_eq!(world_planet(&s, &g.net_data).unwrap().0, "Selenea");
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
            // Mostly to an object that runs it (others are refused before any handler), now
            // and then to any.
            let class = name.split('.').next().unwrap_or("");
            let carriers: Vec<u64> = g.net.objects.keys().copied().filter(|o| g.net.carries(*o, class)).collect();
            let objects: Vec<u64> = if carriers.is_empty() || r.next().is_multiple_of(4) { g.net.objects.keys().copied().collect() } else { carriers };
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
            let mut outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| g.incoming_rpc(client, &message))).err().map(|e| (name.clone(), e));
            if outcome.is_none() && round % 50 == 0 {
                g.time += 0.7;
                outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| g.tick())).err().map(|e| (format!("tick after {name}"), e));
            }
            // The main loop sends and clears the queue every tick.
            g.net.out.clear();
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

    /// Transform states and deltas go to every other client, so one client's flood of them is
    /// cut at its budget (a second's worth), not multiplied by the players: what a real client
    /// sends is far under it.
    #[test]
    fn a_flood_of_transforms_is_cut_at_the_clients_budget() {
        let Some(mut g) = test_game(save()) else { return };
        let ann = join(&mut g, 1, "Ann");
        join(&mut g, 2, "Bob");
        g.sim_time = Some(100.0);
        g.net.out.clear();
        let mut state = Writer::new();
        state.packed_u64(ann).packed_i32(0).packed_u32(2).packed_i32(1).bytes(&1.0f32.to_le_bytes());
        let relayed = |g: &Game| g.net.out.iter().filter(|(to, kind, _)| *kind == NETWORK_TRANSFORM && to.reaches(2)).count();
        g.receive(1, &wire::batch(&vec![(NETWORK_TRANSFORM, state.0.clone()); 100]));
        assert_eq!(relayed(&g), 100, "a real client's states all pass");
        let flood = wire::batch(&vec![(NETWORK_TRANSFORM, state.0.clone()); 30_000]);
        g.receive(1, &flood);
        assert!(relayed(&g) < 9_000, "{} of 30100 states relayed", relayed(&g));
        // One state with a megabyte of trailing bytes is a flood by itself.
        let mut fat = state.0.clone();
        fat.resize(1 << 20, 7);
        g.net.out.clear();
        g.receive(1, &wire::batch(&[(NETWORK_TRANSFORM, fat)]));
        assert_eq!(relayed(&g), 0);
        // Another client has a budget of its own.
        g.receive(2, &wire::batch(&[(NETWORK_VARIABLE_DELTA, vec![0; 3])]));
        g.limits.forget(1);
        g.receive(1, &wire::batch(&[(NETWORK_TRANSFORM, state.0.clone())]));
        assert_eq!(relayed(&g), 1);
        // And the budget comes back with the clock: a second's worth after a second.
        g.net.out.clear();
        g.receive(1, &flood);
        g.net.out.clear();
        g.sim_time = Some(101.0);
        g.receive(1, &flood);
        assert!(relayed(&g) > 7_000 && relayed(&g) < 9_000, "{} relayed a second after", relayed(&g));
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

#[cfg(any(fuzzing, pcds_corpus))]
#[path = "../fuzz/targets/game.rs"]
pub mod fuzz;
