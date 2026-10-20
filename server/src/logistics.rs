//! SpaceCraft.LogisticManager and what works with it on the server: LogisticEntity (an
//! inventory's supply and demand), LogisticTask, MachineDroneStation, Drone (flight, pick up,
//! drop off) and DroneNetworkVibility. (LogisticInterplanetaryManager is rockets::interplanetary.)
//!
//! The game runs this per frame; here one frame is one network tick (30 per second, the real
//! server's frame rate): NetworkTransform first (it ticks in PreUpdate), then
//! LogisticManager.Update (drones at their target act, the flight job moves the others), then
//! the coroutines (Drone.CheckBusy, SetLogisticTasks over three frames, SetTasksCoroutine every
//! second), then DroneNetworkVibility. See hooks.rs for when the entry points run.

mod motion;

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet, VecDeque};

use indexmap::{IndexMap, IndexSet};
use serde_json::{json, Value};

use crate::clock::GameTime;
use crate::data::stable_hash;
use crate::game::{Args, Game};
use crate::idmap::Slots;
use crate::model::WorldObject;
use crate::net::{enc, NetObj, Target};
use crate::protocol::{CREATE_OBJECT, DESTROY_OBJECT, NETWORK_TRANSFORM};
use crate::wire::{Reader, Writer};
use motion::{Q, V3};

const MANAGER: &str = "LogisticManager";
/// SetTasksCoroutine(1f).
const TASKS_EVERY: f32 = 1.0;
const UP: f32 = 2.0;
/// NetworkObject.SceneObject flag.
const DESTROY_WITH_SCENE: u16 = 1 << 6;

#[derive(Clone, Copy, PartialEq, Debug, serde::Serialize, serde::Deserialize)]
enum State {
    NotAttributed,
    ToSupply,
    ToDemand,
    Loading,
    Unloading,
    Done,
}

/// LogisticTask. Drones keep a task after the manager dropped it, so tasks live in an arena.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct Task {
    wo: i32,
    group: usize,
    supply: Option<i32>,
    demand: i32,
    supply_wo: Option<i32>,
    demand_wo: i32,
    state: State,
    spawned: bool,
}

/// LogisticEntity's server state (its groups and priority live on the inventory).
#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
struct Entity {
    /// Set by Init, once found: the constructed world object holding the inventory.
    wo: Option<i32>,
    waiting: i32,
}

/// MachineDroneStation.
#[derive(serde::Serialize, serde::Deserialize)]
struct Station {
    inventory: i32,
    planet: i32,
    groups: Vec<usize>,
    /// _availableDrones: drones that went in, kept (despawned) to be spawned again.
    parked: HashSet<i32>,
}

/// A Drone component and its container's transform.
#[derive(serde::Serialize, serde::Deserialize)]
struct Drone {
    net: Option<u64>,
    /// The despawned network object while the drone waits in a station.
    parked: Option<NetObj>,
    /// _droneWorldObject was set (AddDroneToFleet ran once).
    initialized: bool,
    inventory: i32,
    task: Option<u32>,
    station: Option<i32>,
    target: V3,
    access: i32,
    forward_speed: f32,
    speed_modifier: f32,
    rotation_speed: f32,
    distance_min: f32,
    /// DroneNetworkVibility: hide distance min, max, drone count for the adjustment.
    visibility: [f32; 3],
    position: V3,
    rotation: Q,
    transform: motion::Transform,
    /// The ClientNetworkTransform behaviour of the container.
    behaviour: u16,
    /// Clients the drone is hidden from (NetworkHide).
    hidden: Vec<u64>,
}

/// LogisticManager.DroneAccessData.
#[derive(Clone, Copy, serde::Serialize, serde::Deserialize)]
struct Access {
    target: V3,
    forward: f32,
    angular: f32,
    planet: i32,
}

#[derive(Default, serde::Serialize, serde::Deserialize)]
pub struct Logistics {
    started: bool,
    paused: bool,
    enabled: bool,
    station_group: Option<usize>,
    last_tick: u64,
    next_tasks: GameTime,
    inventories_inited: bool,
    /// SetLogisticTasks frames still to come: 1 assigns idle drones, 2 releases drones.
    updating: u8,
    supply: Vec<i32>,
    demand: Vec<i32>,
    entities: HashMap<i32, Entity>,
    /// _taskPriorities: planet -> group hash -> priority.
    priorities: HashMap<i32, HashMap<i32, i32>>,
    /// _allLogisticTasks: world object to move -> task. (Slots: which task a drone gets and
    /// which station answers first depend on .NET's Dictionary order.)
    all_tasks: Slots<i32, u32>,
    tasks: HashMap<u32, Task>,
    next_task: u32,
    stations: Slots<i32, Station>,
    drones: IndexMap<i32, Drone>,
    fleet: IndexSet<i32>,
    access_created: bool,
    access: Vec<Option<i32>>,
    access_data: Vec<Access>,
    access_available: i32,
    at_target: VecDeque<usize>,
    min_target_distance: f32,
    /// Drones whose CheckBusy runs next frame.
    check_busy: Vec<i32>,
}

// ---- small lookups ------------------------------------------------------------------------

fn tick_seconds(game: &Game) -> f32 {
    1.0 / game.net_data.tick_rate as f32
}

/// The game time of a network tick (the frame's `Time.time`): the float product of the tick and
/// its length while the clock is young (the game's), the exact quotient after.
fn tick_time(game: &Game, tick: u64) -> GameTime {
    let young = tick as f32 * tick_seconds(game);
    if f64::from(young) < crate::clock::EXACT_AFTER { GameTime::at(f64::from(young)) } else { GameTime::at(tick as f64 / game.net_data.tick_rate as f64) }
}

/// The GameObject's position, for a world object that has one.
fn go_position(game: &Game, wo: i32) -> Option<V3> {
    let net = game.wos.get(&wo)?.net?;
    game.net.objects.get(&net).map(|o| o.position)
}

fn wo_position(game: &Game, wo: i32) -> V3 {
    game.wos.get(&wo).map(|w| w.position).unwrap_or([0.0; 3])
}

fn planet_of(game: &Game, wo: i32) -> i32 {
    game.wos.get(&wo).map(|w| w.planet_hash()).unwrap_or(0)
}

fn entity_wo(game: &Game, inventory: i32) -> Option<i32> {
    game.ext.logistics.entities.get(&inventory).and_then(|e| e.wo)
}

fn entity_planet(game: &Game, inventory: i32) -> i32 {
    entity_wo(game, inventory).map(|w| planet_of(game, w)).unwrap_or(0)
}

fn task_planet(game: &Game, t: &Task) -> i32 {
    entity_planet(game, t.supply.unwrap_or(t.demand))
}

fn is_full(game: &Game, inventory: i32) -> bool {
    game.inventories.get(&inventory).is_some_and(|i| i.is_full())
}

fn items(game: &Game, inventory: i32) -> Vec<i32> {
    game.inventories.get(&inventory).map(|i| i.items.clone()).unwrap_or_default()
}

fn drone_task(game: &Game, d: i32) -> Option<u32> {
    game.ext.logistics.drones.get(&d).and_then(|dr| dr.task)
}

fn task_state(game: &Game, uid: u32) -> Option<State> {
    game.ext.logistics.tasks.get(&uid).map(|t| t.state)
}

fn valid_station(game: &Game, st: Option<i32>) -> Option<i32> {
    st.filter(|s| game.ext.logistics.stations.contains_key(s))
}

fn station_position(game: &Game, st: i32) -> V3 {
    go_position(game, st).unwrap_or_else(|| wo_position(game, st))
}

/// Clients that see a drone: everyone approved but those it is hidden from.
fn send_to_observers(game: &mut Game, hidden: &[u64], kind: u32, body: Vec<u8>) {
    if hidden.is_empty() {
        game.net.out.push((Target::All, kind, body));
        return;
    }
    // A drone far from many players is hidden from many: a set, not a list searched for every client.
    let clients: Vec<u64> = if hidden.len() > 8 {
        let hidden: crate::idmap::FxHashSet<u64> = hidden.iter().copied().collect();
        game.clients.values().filter(|c| c.approved && !hidden.contains(&c.id)).map(|c| c.id).collect()
    } else {
        game.clients.values().filter(|c| c.approved && !hidden.contains(&c.id)).map(|c| c.id).collect()
    };
    for c in clients {
        game.net.out.push((Target::Only(c), kind, body.clone()));
    }
}

/// LogisticTask.SetTaskState.
fn set_state(game: &mut Game, uid: u32, state: State) {
    let l = &mut game.ext.logistics;
    let Some(t) = l.tasks.get_mut(&uid) else { return };
    t.state = state;
    let e = l.entities.entry(t.demand).or_default();
    if state == State::Done {
        e.waiting -= 1;
    }
    e.waiting = e.waiting.max(0);
}

/// RemoveItemFromInventoryServerRpc as the server calls it (no reply: it is to itself).
fn remove_item(game: &mut Game, wo: i32, inventory: i32) -> bool {
    let ok = game.wos.contains_key(&wo) && game.inventory_remove(inventory, wo);
    if ok {
        game.propagate(inventory, wo, false);
        game.dirty_inventory(inventory, 0, false);
    }
    ok
}

/// AddWorldObjectToInventoryServerRpc as the server calls it: a world object lying in the
/// world goes into an inventory.
pub fn add_world_object_to_inventory(game: &mut Game, wo: i32, inventory: i32, grabbed: bool) -> bool {
    if !game.wos.get(&wo).is_some_and(|w| w.is_placed()) || !game.inventories.contains_key(&inventory) || !game.inventory_add(inventory, wo, true) {
        return false;
    }
    game.propagate(inventory, wo, true);
    // The inventory's machine may have destroyed it already (MachineDestructInventoryIfFull).
    let Some(was_unsaved) = game.wos.get(&wo).map(|w| w.dont_save) else {
        game.dirty_inventory(inventory, 0, false);
        return true;
    };
    game.wos.get_mut(&wo).unwrap().dont_save = false;
    game.update_save_state(wo);
    if was_unsaved {
        crate::scene::on_world_object_from_scene_retrieved(game, wo);
    }
    if grabbed {
        crate::machines::grabbed(game, wo);
    }
    game.destroy_go_on_all_clients(wo, 0.0);
    game.dirty_inventory(inventory, 0, false);
    true
}

// ---- entry points ---------------------------------------------------------------------------

/// Before the world objects are placed: the scene variables' starting values and the paused
/// state of the save (SavedDataHandler.LoadSavedData -> SetPaused).
pub fn init(game: &mut Game) {
    game.ext.logistics.paused = game.save.world["logisticsPaused"].as_bool().unwrap_or(false);
    if let Some(o) = game.net.find_class(MANAGER).and_then(|(o, _)| game.net.objects.get_mut(&o)) {
        o.init(MANAGER, "_hasLogisticsEnabled", enc::bool(false));
    }
}

/// LogisticManager.InitLogistics, once the world is placed.
pub fn init_late(game: &mut Game) {
    let station_group = game.data.singletons[MANAGER]["droneStationGroup"].as_str().and_then(|r| game.data.group_of_ref(r));
    let planets: Vec<i32> = game.net_data.planets.iter().map(|p| stable_hash(&p.id)).collect();
    // The Inventory constructor sets supply, then demand: a group saved as both stays demand.
    for inv in game.inventories.values_mut() {
        let demand = inv.logistic.demand.clone();
        inv.logistic.supply.retain(|g| !demand.contains(g));
    }
    let tick = game.tick_count();
    let now = tick_time(game, tick);
    let l = &mut game.ext.logistics;
    l.station_group = station_group;
    l.priorities = planets.into_iter().map(|p| (p, HashMap::new())).collect();
    l.started = true;
    l.last_tick = tick;
    l.next_tasks = now + TASKS_EVERY;
}

pub fn tick(game: &mut Game) {
    if !game.ext.logistics.started {
        return;
    }
    let now = game.tick_count();
    // A stall is at most a third of a second of ticks (Game::tick bounds a frame); were the clock to
    // jump anyway, ponytail: past a second the frames in between are skipped, not replayed.
    let mut t = game.ext.logistics.last_tick.max(now.saturating_sub(u64::from(game.net_data.tick_rate)));
    while t < now {
        t += 1;
        game.ext.logistics.last_tick = t;
        frame(game, t);
    }
}

pub fn rpc(game: &mut Game, client: u64, _object: u64, _behaviour: u16, name: &str, _a: &Args) -> bool {
    let me = Target::Only(client);
    match name {
        "LogisticManager.RetrieveDronesCountServerRpc" => {
            let planet = game.planet_hash();
            let (mut supply, mut demand) = (0, 0);
            for (_, uid) in game.ext.logistics.all_tasks.iter() {
                let t = &game.ext.logistics.tasks[uid];
                if task_planet(game, t) == planet {
                    match t.state {
                        State::ToSupply => supply += 1,
                        State::ToDemand => demand += 1,
                        _ => {}
                    }
                }
            }
            let total = drones_in_logistics(game);
            game.net.rpc_scene(me, "LogisticManager.RetrieveDronesCountClientRpc", &[json!(total), json!(demand), json!(supply)]);
        }
        "LogisticManager.RetrieveSupplyAndDemandGroupsServerRpc" => {
            let demand = groups_count(game, &game.ext.logistics.demand, true);
            let supply = groups_count(game, &game.ext.logistics.supply, false);
            game.net.rpc_scene(me, "LogisticManager.RetrieveSupplyAndDemandGroupsClientRpc", &[json!(demand), json!(supply)]);
        }
        _ => return false,
    }
    true
}

/// A world object got its GameObject: a drone wakes up (Drone.Awake), a drone station
/// registers (MachineDroneStation.SetDroneStationInventory).
pub fn on_instantiated(game: &mut Game, wo: i32) {
    let Some(w) = game.wos.get(&wo) else { return };
    let id = game.data.groups[w.group].id.clone();
    if let Some(c) = game.data.prefab_components(&id, "Drone").first().map(|c| (*c).clone()) {
        let vis = game.data.prefab_components(&id, "DroneNetworkVibility").first().map(|c| (*c).clone()).unwrap_or(Value::Null);
        awake_drone(game, wo, &c, &vis);
    }
    if let Some(c) = game.data.prefab_components(&id, "MachineDroneStation").first() {
        let groups: Vec<usize> = c["droneGroupsData"].as_array().into_iter().flatten().filter_map(|r| game.data.group_of_ref(r.as_str()?)).collect();
        let w = &game.wos[&wo];
        let station = Station { inventory: w.linked_inventory, planet: w.planet_hash(), groups, parked: HashSet::new() };
        if !game.ext.logistics.stations.contains_key(&wo) {
            game.ext.logistics.stations.insert(wo, station);
        }
    }
}

/// A world object's GameObject was destroyed and the world object stays (a drone picked up):
/// Drone.OnDestroy.
pub fn on_go_destroyed(game: &mut Game, wo: i32) {
    if game.ext.logistics.drones.contains_key(&wo) {
        drone_on_destroy(game, wo);
        game.ext.logistics.drones.shift_remove(&wo);
        game.ext.logistics.check_busy.retain(|d| *d != wo);
    }
}

/// A world object was destroyed: its drone (Drone.OnDestroy) or station goes.
pub fn on_destroyed(game: &mut Game, wo: &WorldObject) {
    if game.ext.logistics.drones.contains_key(&wo.id) {
        drone_on_destroy(game, wo.id);
        game.ext.logistics.drones.shift_remove(&wo.id);
        game.ext.logistics.check_busy.retain(|d| *d != wo.id);
    }
    game.ext.logistics.stations.remove(&wo.id);
}

/// Inventory.PropagateModification: nothing of the logistics listens (the interplanetary
/// counts are rockets::interplanetary).
pub fn inventory_changed(_game: &mut Game, _inventory: i32, _wo: i32, _added: bool) {}

/// An inventory was destroyed (InventoriesHandler.DestroyInventory ->
/// LogisticManager.RemoveInventoryFromLogistics).
pub fn remove_inventory(game: &mut Game, inventory: i32) {
    let l = &mut game.ext.logistics;
    remove_first(&mut l.demand, inventory);
    remove_first(&mut l.supply, inventory);
    let tasks: Vec<u32> = l.all_tasks.iter().map(|(_, uid)| *uid).collect();
    for uid in tasks {
        let t = &game.ext.logistics.tasks[&uid];
        if t.demand == inventory || (t.supply == Some(inventory) && t.state == State::ToSupply) {
            set_state(game, uid, State::Done);
        }
    }
    // The LogisticEntity went with its inventory: one created with the same id later is
    // another, found again (an entity kept would name the old building, gone or elsewhere).
    game.ext.logistics.entities.remove(&inventory);
}

/// LogisticEntity.SetDirty after DirtyInventoryLogisticsServerRpc set the groups: Deserialize
/// keeps a group only once and only as supply when sent as both, then the manager re-files the
/// inventory.
pub fn set_dirty(game: &mut Game, inventory: i32) {
    if let Some(i) = game.inventories.get_mut(&inventory) {
        let dedup = |v: &mut Vec<usize>| {
            let mut seen = HashSet::new();
            v.retain(|g| seen.insert(*g));
        };
        dedup(&mut i.logistic.demand);
        dedup(&mut i.logistic.supply);
        let supply = i.logistic.supply.clone();
        i.logistic.demand.retain(|g| !supply.contains(g));
    }
    set_inventory_status(game, inventory);
}

fn remove_first(list: &mut Vec<i32>, v: i32) {
    if let Some(at) = list.iter().position(|x| *x == v) {
        list.remove(at);
    }
}

// ---- one frame ------------------------------------------------------------------------------

fn frame(game: &mut Game, tick: u64) {
    // The wire has the game's int tick (Game::tick_number).
    send_transforms(game, tick as i32);
    update(game);
    for d in std::mem::take(&mut game.ext.logistics.check_busy) {
        check_busy(game, d);
    }
    match game.ext.logistics.updating {
        1 => assign_idle_drones(game),
        2 => release_drones(game),
        _ => {}
    }
    let now = tick_time(game, tick);
    if now >= game.ext.logistics.next_tasks {
        game.ext.logistics.next_tasks = now + TASKS_EVERY;
        tasks_coroutine(game);
    }
    visibility(game);
}

/// LogisticManager.Update: drones the job found at their target act, then the job moves the
/// others (DroneTransformUpdateJob) on planets with enough energy.
fn update(game: &mut Game) {
    let l = &game.ext.logistics;
    if !l.enabled || !l.access_created {
        return;
    }
    while let Some(i) = game.ext.logistics.at_target.pop_front() {
        if let Some(Some(d)) = game.ext.logistics.access.get(i).copied() {
            update_state(game, d);
        }
    }
    if game.ext.logistics.fleet.is_empty() {
        return;
    }
    // The planets drones fly on (a few), and which of them have the energy.
    let mut powered: Vec<i32> = Vec::new();
    let mut unpowered: Vec<i32> = Vec::new();
    for a in &game.ext.logistics.access_data {
        let p = a.planet;
        if p != 0 && !powered.contains(&p) && !unpowered.contains(&p) {
            let list = if crate::energy::has_enough(game, p) { &mut powered } else { &mut unpowered };
            list.push(p);
        }
    }
    let dt = tick_seconds(game);
    let l = &mut game.ext.logistics;
    for i in 0..l.access.len() {
        let (Some(d), a) = (l.access[i], l.access_data[i]) else { continue };
        let Some(dr) = l.drones.get_mut(&d) else { continue };
        if motion::length_sq(a.target) <= motion::EPSILON || !powered.contains(&a.planet) {
            continue;
        }
        if motion::distance(a.target, dr.position) < l.min_target_distance {
            l.at_target.push_back(i);
            continue;
        }
        (dr.position, dr.rotation) = motion::step(dr.position, dr.rotation, a.target, a.forward, a.angular, dt);
    }
}

/// SetTasksCoroutine's loop body.
fn tasks_coroutine(game: &mut Game) {
    if !game.ext.logistics.inventories_inited {
        let ids: Vec<i32> = game.inventories.values().filter(|i| !i.logistic.demand.is_empty() || !i.logistic.supply.is_empty()).map(|i| i.id).collect();
        for id in ids {
            set_inventory_status(game, id);
        }
        // _demandInventories.Sort(CompareInventoryPriority): List<T>.Sort is an introsort, which
        // reorders equal priorities (above 16 demand inventories).
        let mut demand = std::mem::take(&mut game.ext.logistics.demand);
        let priority = |i: &i32| game.inventories.get(i).map(|i| i.logistic.priority).unwrap_or(0);
        // The priorities are plain integers: a comparer that is an order cannot fail.
        let _ = crate::dotnet_sort::sort_by(&mut demand, |x, y| priority(y).cmp(&priority(x)));
        game.ext.logistics.demand = demand;
        game.ext.logistics.inventories_inited = true;
    }
    if game.ext.logistics.updating == 0 {
        // SetGlobalLogisticsEnabled: is a drone station built anywhere?
        let enabled = game.ext.logistics.station_group.is_some_and(|g| game.group_count(true, g) > 0);
        if enabled != game.ext.logistics.enabled {
            game.ext.logistics.enabled = enabled;
            game.set_on(MANAGER, "_hasLogisticsEnabled", enc::bool(enabled));
        }
        if enabled {
            set_logistic_tasks(game);
        }
    }
}

// ---- LogisticManager: supply and demand -----------------------------------------------------

/// SetInventoryStatusInLogistics.
fn set_inventory_status(game: &mut Game, inventory: i32) {
    if entity_wo(game, inventory).is_none() {
        // LogisticEntity.Init: WorldObjectsHandler.GetWorldObjectForInventory.
        let found = game.constructed.iter().copied().find(|id| game.wos.get(id).is_some_and(|w| w.linked_inventory == inventory || w.secondary_inventories.contains(&inventory)));
        game.ext.logistics.entities.entry(inventory).or_default().wo = found;
    }
    remove_first(&mut game.ext.logistics.demand, inventory);
    let Some(inv) = game.inventories.get(&inventory) else { return };
    let (demand_groups, supply_groups) = (inv.logistic.demand.clone(), inv.logistic.supply.clone());
    if !demand_groups.is_empty() {
        add_new_demand_inventory(game, inventory);
    }
    let l = &mut game.ext.logistics;
    if !supply_groups.is_empty() && !l.supply.contains(&inventory) {
        l.supply.push(inventory);
    } else if supply_groups.is_empty() {
        remove_first(&mut l.supply, inventory);
    }
    let tasks: Vec<u32> = l.all_tasks.iter().map(|(_, uid)| *uid).collect();
    for uid in tasks {
        let t = &game.ext.logistics.tasks[&uid];
        let stale = if t.demand == inventory { !demand_groups.contains(&t.group) } else { t.supply == Some(inventory) && !supply_groups.contains(&t.group) };
        if stale {
            set_state(game, uid, State::Done);
        }
    }
}

/// AddNewDemandInventory: by priority, before the first of equal or lower priority.
fn add_new_demand_inventory(game: &mut Game, inventory: i32) {
    let inv = &game.inventories[&inventory];
    let priority = inv.logistic.priority;
    let waiting = game.ext.logistics.entities.get(&inventory).map(|e| e.waiting).unwrap_or(0);
    let available = (inv.size - inv.items.len() as i32 - waiting).max(0);
    let hashes: Vec<i32> = inv.logistic.demand.iter().map(|g| game.data.groups[*g].hash).collect();
    let planet = entity_planet(game, inventory);
    let priority_of = |i: &i32| game.inventories.get(i).map(|i| i.logistic.priority).unwrap_or(0);
    let at = game.ext.logistics.demand.iter().position(|d| priority >= priority_of(d));
    let l = &mut game.ext.logistics;
    if available > 0 {
        if let Some(p) = l.priorities.get_mut(&planet) {
            for h in hashes {
                let e = p.entry(h).or_insert(priority);
                *e = (*e).max(priority);
            }
        }
    }
    match at {
        Some(at) => l.demand.insert(at, inventory),
        None => l.demand.push(inventory),
    }
}

/// CreateNewTaskForWorldObject / ...ForSpawnedObject.
fn create_task(game: &mut Game, supply: Option<i32>, demand: i32, wo: i32, spawned: bool) -> Option<u32> {
    if game.ext.logistics.all_tasks.contains_key(&wo) {
        return None;
    }
    let demand_wo = entity_wo(game, demand)?;
    let supply_wo = match supply {
        Some(s) if !spawned => Some(entity_wo(game, s)?),
        _ => None,
    };
    let group = game.wos.get(&wo)?.group;
    let l = &mut game.ext.logistics;
    let uid = l.next_task;
    l.next_task += 1;
    l.tasks.insert(uid, Task { wo, group, supply: supply.filter(|_| !spawned), demand, supply_wo, demand_wo, state: State::NotAttributed, spawned });
    l.entities.entry(demand).or_default().waiting += 1;
    l.all_tasks.insert(wo, uid);
    Some(uid)
}

/// ActionGrabable.GetCanGrab: MachineGrowerVegetationHarvestable lets its growables (placed,
/// in its inventory) be taken once grown; anything else on the ground can be.
fn can_grab(game: &Game, w: &WorldObject) -> bool {
    w.growth >= 100.0 || !game.is_held(w.id)
}

/// What demands of one group on one planet may take in a search for tasks (SetLogisticTasks), in
/// the order the game meets it: the items of the supplies in the supplies' order, or the dropped items
/// in theirs. An item that has a task is taken for good, so each demand goes through the items left,
/// not through all of them again.
struct Candidates {
    /// (the supply it is in, the world object).
    list: Vec<(Option<i32>, i32)>,
    taken: Vec<bool>,
    /// Everything before this is taken.
    first: usize,
}

impl Candidates {
    fn new(game: &Game, list: Vec<(Option<i32>, i32)>) -> Candidates {
        let taken = list.iter().map(|(_, wo)| game.ext.logistics.all_tasks.contains_key(wo)).collect();
        let mut c = Candidates { list, taken, first: 0 };
        c.skip_taken();
        c
    }

    fn skip_taken(&mut self) {
        while self.first < self.list.len() && self.taken[self.first] {
            self.first += 1;
        }
    }

    /// Tasks for the demand `demand`, which wants `available` more (above 0): each item that gets a
    /// task is one less, and it stops at none. Returns what it still wants. create_task refuses an
    /// item that has a task, which is then taken for the rest of the search. The demand's own
    /// items are not for it.
    fn take(&mut self, game: &mut Game, demand: i32, mut available: i32, spawned: bool) -> i32 {
        let mut i = self.first;
        while i < self.list.len() {
            let (supply, wo) = self.list[i];
            if !self.taken[i] && supply != Some(demand) {
                self.taken[i] = true;
                if create_task(game, supply, demand, wo, spawned).is_some() {
                    available -= 1;
                    if available == 0 {
                        break;
                    }
                }
            }
            i += 1;
        }
        self.skip_taken();
        available
    }
}

/// SetLogisticTasks, first frame: tasks for every demand that supplies or the ground can
/// fill, by priority, then the tasks that are over. The next two frames follow in
/// assign_idle_drones and release_drones. (The game also pauses every 400 iterations; one
/// frame does it all here.)
fn set_logistic_tasks(game: &mut Game) {
    game.ext.logistics.updating = 1;
    // The ground's items by group, in the pickables' order: a demand asks for a few groups, and
    // asked of every pickable it was the whole list for each (no task changes a world object's group).
    let mut pickables: crate::idmap::FxHashMap<usize, Vec<i32>> = Default::default();
    for &wo in game.pickables.iter() {
        if let Some(w) = game.wos.get(&wo) {
            pickables.entry(w.group).or_default().push(wo);
        }
    }
    // The supplies that supply a group, with their planet (nothing the loop does changes either).
    let mut wanted = vec![false; game.data.groups.len()];
    for i in game.ext.logistics.demand.iter().filter_map(|d| game.inventories.get(d)) {
        for &g in &i.logistic.demand {
            wanted[g] = true;
        }
    }
    let mut suppliers: Option<Vec<Vec<(i32, i32, usize)>>> = None;
    let mut item_groups: crate::idmap::FxHashMap<i32, crate::idmap::FxHashMap<usize, Vec<i32>>> = Default::default();
    let (mut from_supplies, mut from_ground): (crate::idmap::FxHashMap<(usize, i32), Candidates>, crate::idmap::FxHashMap<(usize, i32), Candidates>) = Default::default();
    for p in game.ext.logistics.priorities.values_mut() {
        p.clear();
    }
    let mut i = 0;
    while i < game.ext.logistics.demand.len() {
        let d = game.ext.logistics.demand[i];
        i += 1;
        let Some(inv) = game.inventories.get(&d) else { continue };
        let waiting = game.ext.logistics.entities.get(&d).map(|e| e.waiting).unwrap_or(0);
        let mut available = (inv.size - inv.items.len() as i32 - waiting).max(0);
        let (full, priority, groups) = (inv.is_full(), inv.logistic.priority, inv.logistic.demand.clone());
        let planet = entity_planet(game, d);
        if full || available == 0 || entity_wo(game, d).is_none() || !game.ext.logistics.priorities.contains_key(&planet) {
            continue;
        }
        for &g in &groups {
            let hash = game.data.groups[g].hash;
            if game.ext.logistics.priorities[&planet].get(&hash).is_some_and(|v| priority < *v) {
                continue;
            }
            let suppliers_of_g = suppliers.get_or_insert_with(|| {
                // Per wanted group, in the order of `supply`: (inventory, its planet, its place in `supply`).
                let mut by_group: Vec<Vec<(i32, i32, usize)>> = vec![Vec::new(); wanted.len()];
                for (place, &s) in game.ext.logistics.supply.iter().enumerate() {
                    let (Some(inv), Some(_)) = (game.inventories.get(&s), entity_wo(game, s)) else { continue };
                    let planet = entity_planet(game, s);
                    for &g in &inv.logistic.supply {
                        // A group listed twice by one inventory is one supplier.
                        if wanted[g] && by_group[g].last().is_none_or(|e| e.2 != place) {
                            by_group[g].push((s, planet, place));
                        }
                    }
                }
                by_group
            });
            // The game's loops count `available` down to none and stop there: a group met with none left
            // (an earlier group of the demand took the last) takes all it finds, and its first supply is
            // the only one looked at when it finds nothing. That is done as the game does it.
            if available <= 0 {
                for i in 0..suppliers_of_g[g].len() {
                    let (s, supply_planet, _) = suppliers_of_g[g][i];
                    if s == d || supply_planet != planet {
                        continue;
                    }
                    let by_group = item_groups.entry(s).or_insert_with(|| {
                        let mut by_group: crate::idmap::FxHashMap<usize, Vec<i32>> = Default::default();
                        for &wo in game.inventories.get(&s).map(|i| i.items.as_slice()).unwrap_or_default() {
                            if let Some(w) = game.wos.get(&wo) {
                                by_group.entry(w.group).or_default().push(wo);
                            }
                        }
                        by_group
                    });
                    for &wo in by_group.get(&g).map(Vec::as_slice).unwrap_or_default() {
                        if create_task(game, Some(s), d, wo, false).is_some() {
                            available -= 1;
                            if available == 0 {
                                break;
                            }
                        }
                    }
                    if available == 0 {
                        break;
                    }
                }
                if available == 0 {
                    continue;
                }
                for &wo in pickables.get(&g).map(Vec::as_slice).unwrap_or_default() {
                    if game.ext.logistics.all_tasks.contains_key(&wo) {
                        continue;
                    }
                    let ok = game.wos.get(&wo).is_some_and(|w| w.net.is_some() && motion::length_sq(w.position) >= 1e-10 && can_grab(game, w) && w.planet_hash() == planet);
                    if ok && create_task(game, None, d, wo, true).is_some() {
                        available -= 1;
                        if available == 0 {
                            break;
                        }
                    }
                }
                continue;
            }
            // What a demand of this group on this planet may take, from the supplies and then from the ground.
            let supplies = match from_supplies.entry((g, planet)) {
                Entry::Occupied(o) => o.into_mut(),
                Entry::Vacant(v) => {
                    let mut list = Vec::new();
                    for &(s, supply_planet, _) in &suppliers_of_g[g] {
                        if supply_planet != planet {
                            continue;
                        }
                        // The supply's items of the group, in its order (nothing the tasks do changes an
                        // inventory or a group): its items sorted by group once per search.
                        let by_group = item_groups.entry(s).or_insert_with(|| {
                            let mut by_group: crate::idmap::FxHashMap<usize, Vec<i32>> = Default::default();
                            for &wo in game.inventories.get(&s).map(|i| i.items.as_slice()).unwrap_or_default() {
                                if let Some(w) = game.wos.get(&wo) {
                                    by_group.entry(w.group).or_default().push(wo);
                                }
                            }
                            by_group
                        });
                        list.extend(by_group.get(&g).into_iter().flatten().map(|&wo| (Some(s), wo)));
                    }
                    v.insert(Candidates::new(game, list))
                }
            };
            available = supplies.take(game, d, available, false);
            if available == 0 {
                continue;
            }
            let ground = match from_ground.entry((g, planet)) {
                Entry::Occupied(o) => o.into_mut(),
                Entry::Vacant(v) => {
                    // A task for it already: create_task refuses it, so its checks can wait.
                    let list = pickables.get(&g).into_iter().flatten().copied().filter(|wo| !game.ext.logistics.all_tasks.contains_key(wo)).filter(|wo| game.wos.get(wo).is_some_and(|w| w.net.is_some() && motion::length_sq(w.position) >= 1e-10 && can_grab(game, w) && w.planet_hash() == planet)).map(|wo| (None, wo)).collect();
                    v.insert(Candidates::new(game, list))
                }
            };
            available = ground.take(game, d, available, true);
        }
        if available > 0 {
            let hashes: Vec<i32> = groups.iter().map(|g| game.data.groups[*g].hash).collect();
            if let Some(p) = game.ext.logistics.priorities.get_mut(&planet) {
                for h in hashes {
                    let e = p.entry(h).or_insert(priority);
                    *e = (*e).max(priority);
                }
            }
        }
    }
    let entries: Vec<(i32, u32)> = game.ext.logistics.all_tasks.iter().map(|(k, v)| (*k, *v)).collect();
    let mut over = Vec::new();
    for (wo, uid) in entries {
        let t = &game.ext.logistics.tasks[&uid];
        if is_full(game, t.demand) {
            set_state(game, uid, State::Done);
            over.push(wo);
        } else if t.state == State::Done {
            over.push(wo);
        }
    }
    let l = &mut game.ext.logistics;
    for wo in over {
        l.all_tasks.remove(&wo);
    }
    let held: HashSet<u32> = l.all_tasks.iter().map(|(_, uid)| *uid).chain(l.drones.values().filter_map(|d| d.task)).collect();
    l.tasks.retain(|uid, _| held.contains(uid));
}

/// SetLogisticTasks, second frame: drones already flying take free tasks.
fn assign_idle_drones(game: &mut Game) {
    if !game.ext.logistics.access_created {
        release_drones(game);
        return;
    }
    game.ext.logistics.updating = 2;
    let mut i = 0;
    while i < game.ext.logistics.access.len() {
        if let Some(d) = game.ext.logistics.access[i] {
            let Some(task) = non_attributed_task(game, planet_of(game, d)) else { break };
            if drone_task(game, d).is_none() {
                set_task(game, d, Some(task));
            }
        }
        i += 1;
    }
}

/// SetLogisticTasks, last frame: each free task gets a drone from the nearest station that
/// has one.
fn release_drones(game: &mut Game) {
    game.ext.logistics.updating = 0;
    if non_attributed_task(game, 0).is_none() {
        return;
    }
    let tasks: Vec<u32> = game.ext.logistics.all_tasks.iter().map(|(_, uid)| *uid).collect();
    for uid in tasks {
        let Some(t) = game.ext.logistics.tasks.get(&uid).cloned() else { continue };
        if t.state != State::NotAttributed || (!t.spawned && t.supply_wo.is_none()) {
            continue;
        }
        let planet = task_planet(game, &t);
        let from = wo_position(game, if t.spawned { t.wo } else { t.supply_wo.unwrap() });
        let mut distances: Vec<(i32, i32)> = game
            .ext
            .logistics
            .stations
            .iter()
            .filter(|(_, s)| s.planet == planet)
            .map(|(st, _)| (*st, motion::distance(station_position(game, *st), from).round_ties_even() as i32))
            .collect();
        // _stationsDistancesBuffer.Sort(): LogisticStationDistanceToTask.CompareTo, nearest first.
        let _ = crate::dotnet_sort::sort_by(&mut distances, |a, b| a.1.cmp(&b.1));
        for (st, _) in distances {
            if let Some(d) = release_one_drone(game, st) {
                add_to_fleet(game, d, false);
                set_task(game, d, Some(uid));
                break;
            }
        }
    }
}

/// GetANonAttributedTask (none while paused).
fn non_attributed_task(game: &Game, planet: i32) -> Option<u32> {
    let l = &game.ext.logistics;
    if l.paused {
        return None;
    }
    l.all_tasks.iter().map(|(_, uid)| *uid).find(|uid| {
        let t = &l.tasks[uid];
        t.state == State::NotAttributed && (planet == 0 || task_planet(game, t) == planet)
    })
}

/// GetDronesInLogistics: drones in stations and flying, on the current planet.
fn drones_in_logistics(game: &Game) -> i32 {
    let planet = game.planet_hash();
    let l = &game.ext.logistics;
    let in_stations: usize = l
        .stations
        .iter()
        .filter(|(_, s)| s.planet == planet)
        .map(|(_, s)| items(game, s.inventory).iter().filter(|w| game.wos.get(*w).is_some_and(|w| s.groups.contains(&w.group))).count())
        .sum();
    let flying = l.fleet.iter().filter(|d| planet_of(game, **d) == planet).count();
    (in_stations + flying) as i32
}

/// RetrieveSupplyAndDemandGroupsServerRpc's RetrieveGroups: per group, how many inventories
/// on the current planet ask for (or offer) it.
fn groups_count(game: &Game, inventories: &[i32], demand: bool) -> Vec<Value> {
    let planet = game.planet_hash();
    let mut counts: IndexMap<i32, i32> = IndexMap::new();
    for id in inventories {
        let Some(inv) = game.inventories.get(id).filter(|_| entity_planet(game, *id) == planet) else { continue };
        for g in if demand { &inv.logistic.demand } else { &inv.logistic.supply } {
            *counts.entry(game.data.groups[*g].hash).or_insert(0) += 1;
        }
    }
    counts.into_iter().map(|(h, c)| json!([h, c])).collect()
}

// ---- LogisticManager: the fleet -------------------------------------------------------------

/// LogisticManager.AddDroneToFleet.
fn manager_add_drone(game: &mut Game, d: i32) {
    let planet = planet_of(game, d);
    let l = &mut game.ext.logistics;
    if l.fleet.insert(d) {
        if !l.access_created {
            l.access_created = true;
            l.access_available = 0;
        }
        l.at_target.clear();
        let start = l.access_available as usize;
        let index = if start >= l.access.len() { start } else { (start..l.access.len()).find(|i| l.access[*i].is_none()).unwrap_or(l.access.len()) };
        // AddOrInsertDroneData.
        let dr = l.drones.get_mut(&d).unwrap();
        let data = Access { target: [0.0; 3], forward: dr.forward_speed, angular: dr.rotation_speed, planet };
        if index < l.access.len() {
            l.access[index] = Some(d);
            l.access_data[index] = data;
            dr.access = index as i32;
            l.access_available = index as i32 + 1;
        } else {
            l.access.push(Some(d));
            l.access_data.push(data);
            dr.access = l.access.len() as i32 - 1;
            l.access_available = l.access.len() as i32;
        }
        l.min_target_distance = dr.distance_min;
    }
    if drone_task(game, d).is_some() {
        return;
    }
    let inventory = game.ext.logistics.drones[&d].inventory;
    if !items(game, inventory).is_empty() {
        find_demand_or_destroy_content(game, d);
    }
}

/// LogisticManager.RemoveDroneFromFleet.
fn manager_remove_drone(game: &mut Game, d: i32) {
    let l = &mut game.ext.logistics;
    if let Some(i) = l.drones.get(&d).map(|dr| dr.access).filter(|i| *i >= 0) {
        // Only the drone's own slot: a stale index of a drone that went into a station must not
        // free the slot another drone took since.
        if l.access.get(i as usize) == Some(&Some(d)) {
            l.access_available = l.access_available.min(i);
            l.access[i as usize] = None;
        }
    }
    l.fleet.shift_remove(&d);
}

/// FindDemandForDroneOrDestroyContent. A drone's own inventory never belongs to a constructed
/// world object, so CreateNewTaskForWorldObject refuses it and the content is always destroyed.
fn find_demand_or_destroy_content(game: &mut Game, d: i32) {
    let planet = planet_of(game, d);
    let inventory = game.ext.logistics.drones[&d].inventory;
    let Some(&wo) = items(game, inventory).first() else { return };
    set_task(game, d, None);
    let group = game.wos.get(&wo).map(|w| w.group);
    for demand in game.ext.logistics.demand.clone() {
        let Some(inv) = game.inventories.get(&demand) else { continue };
        if inv.is_full() || entity_planet(game, demand) != planet {
            continue;
        }
        for g in inv.logistic.demand.clone() {
            if Some(g) == group {
                if let Some(task) = create_task(game, Some(inventory), demand, wo, false) {
                    set_state(game, task, State::ToDemand);
                    set_task(game, d, Some(task));
                    return;
                }
            }
        }
    }
    if drone_task(game, d).is_none() {
        game.destroy_world_object(wo, false);
    }
}

// ---- MachineDroneStation ----------------------------------------------------------------------

/// TryToReleaseOneDrone: the first drone in the station flies out from 2 m above it.
fn release_one_drone(game: &mut Game, st: i32) -> Option<i32> {
    let (inventory, groups, planet) = {
        let s = game.ext.logistics.stations.get(&st)?;
        (s.inventory, s.groups.clone(), s.planet)
    };
    for wo in items(game, inventory) {
        if !game.wos.get(&wo).is_some_and(|w| groups.contains(&w.group)) {
            continue;
        }
        remove_item(game, wo, inventory);
        let mut position = station_position(game, st);
        position[1] += UP;
        let w = game.wos.get_mut(&wo)?;
        w.position = position;
        w.rotation = motion::IDENTITY;
        w.planet = planet;
        let parked = game.ext.logistics.stations.get_mut(&st).is_some_and(|s| s.parked.remove(&wo));
        if parked && respawn_drone(game, wo) {
            return Some(wo);
        }
        return game.instantiate(wo, false, 0).map(|_| wo);
    }
    None
}

/// ReturnDroneToStation.
fn return_drone_to_station(game: &mut Game, st: i32, d: i32) -> bool {
    let Some(inventory) = game.ext.logistics.stations.get(&st).map(|s| s.inventory) else { return false };
    if !game.inventories.get(&inventory).is_some_and(|i| !i.is_full()) {
        return false;
    }
    game.add_existing_item(d, inventory, true, 0);
    park_drone(game, d);
    if let Some(s) = game.ext.logistics.stations.get_mut(&st) {
        s.parked.insert(d);
    }
    true
}

/// NetworkObject.Despawn(destroy: false): the drone's GameObject waits, inactive, in the
/// station; clients get a DestroyObjectMessage without the destroy flag.
fn park_drone(game: &mut Game, d: i32) {
    let Some(dr) = game.ext.logistics.drones.get_mut(&d) else { return };
    let Some(net) = dr.net.take() else { return };
    dr.parked = game.net.objects.shift_remove(&net);
    game.net.removed += 1;
    let hidden = std::mem::take(&mut dr.hidden);
    game.set_wo_net(d, None);
    let body = Writer::new().packed_u64(net).u8(0).0.clone();
    send_to_observers(game, &hidden, DESTROY_OBJECT, body);
}

/// NetworkObject.Spawn() of a parked drone: it comes back where its GameObject was.
fn respawn_drone(game: &mut Game, d: i32) -> bool {
    let tick = game.tick_number();
    let Some(dr) = game.ext.logistics.drones.get_mut(&d) else { return false };
    let Some(mut o) = dr.parked.take() else { return false };
    o.position = dr.position;
    o.rotation = dr.rotation;
    o.tail = motion::Transform::sync_tail(dr.behaviour, dr.position, dr.rotation, tick).into();
    // Spawn() without destroyWithScene, unlike InstantiateWorldObject's first spawn.
    o.flags &= !DESTROY_WITH_SCENE;
    dr.transform = motion::Transform::default();
    let net = game.net.spawn(o);
    game.ext.logistics.drones.get_mut(&d).unwrap().net = Some(net);
    game.set_wo_net(d, Some(net));
    true
}

// ---- Drone ------------------------------------------------------------------------------------

/// Drone.Awake: the drone's container just spawned. CheckBusy runs next frame.
fn awake_drone(game: &mut Game, wo: i32, c: &Value, vis: &Value) {
    let f = |v: &Value, k: &str, default: f32| v[k].as_f64().map(|x| x as f32).unwrap_or(default);
    let Some(net) = game.wos.get(&wo).and_then(|w| w.net) else { return };
    let Some(o) = game.net.objects.get(&net) else { return };
    let behaviour = o.behaviours().find(|b| b.class == "ClientNetworkTransform").map(|b| b.index).unwrap_or(0);
    let rotation = if o.rotation.iter().all(|v| *v == 0.0) { motion::IDENTITY } else { o.rotation };
    let (position, owner) = (o.position, o.owner);
    // GroupNetworkContainer keeps server ownership of a child with a Rigidbody, as drones are.
    if owner != 0 {
        game.net.change_owner(net, 0);
    }
    let drone = Drone {
        net: Some(net),
        parked: None,
        initialized: false,
        inventory: 0,
        task: None,
        station: None,
        target: [0.0; 3],
        access: -1,
        forward_speed: f(c, "forwardSpeed", 0.0),
        speed_modifier: f(c, "forwardSpeedIntervalModifier", 0.0),
        rotation_speed: f(c, "rotationSpeed", 0.0),
        distance_min: f(c, "distanceMinToTarget", 0.0),
        visibility: [f(vis, "hideDistanceFromPlayerMin", 30.0), f(vis, "hideDistanceFromPlayerMax", 100.0), f(vis, "droneMaxNumberForVisibilityAdjustment", 100.0)],
        position,
        rotation,
        transform: motion::Transform::default(),
        behaviour,
        hidden: Vec::new(),
    };
    game.ext.logistics.drones.insert(wo, drone);
    game.ext.logistics.check_busy.push(wo);
    // The spawn just queued carries the transform as NetworkTransform.OnSynchronize writes it.
    let tick = game.tick_number();
    let handle = game.net.scene_handle;
    let o = game.net.objects.get_mut(&net).unwrap();
    o.rotation = rotation;
    o.tail = motion::Transform::sync_tail(behaviour, position, rotation, tick).into();
    o.tail_position = None;
    let body = [vec![1], o.scene_object(handle)].concat();
    let spawned = |b: &[u8]| {
        let mut r = Reader::new(b);
        r.skip(1 + 2 + 4)?;
        r.packed_u64()
    };
    if let Some(m) = game.net.out.iter_mut().rev().find(|m| m.1 == CREATE_OBJECT && spawned(&m.2) == Some(net)) {
        m.2 = body;
    }
}

/// Drone.CheckBusy.
fn check_busy(game: &mut Game, d: i32) {
    if game.ext.logistics.drones.contains_key(&d) && drone_task(game, d).is_none() {
        add_to_fleet(game, d, true);
    }
}

/// Drone.AddDroneToFleet; `then_assign` is CheckBusy's callback (SetANonAttributedTaskToDrone).
fn add_to_fleet(game: &mut Game, d: i32, then_assign: bool) {
    let Some(initialized) = game.ext.logistics.drones.get(&d).map(|dr| dr.initialized) else { return };
    if !initialized {
        let Some(inventory) = game.wos.get(&d).map(|w| w.linked_inventory) else { return };
        let modifier = game.ext.logistics.drones[&d].speed_modifier;
        let extra = game.rng.rangef(0.0, modifier);
        let dr = game.ext.logistics.drones.get_mut(&d).unwrap();
        dr.initialized = true;
        dr.inventory = inventory;
        dr.forward_speed += extra;
        manager_add_drone(game, d);
        if let Some(task) = drone_task(game, d) {
            set_task(game, d, Some(task));
        }
    } else {
        manager_add_drone(game, d);
    }
    if then_assign && game.ext.logistics.drones.contains_key(&d) {
        let task = non_attributed_task(game, planet_of(game, d));
        set_task(game, d, task);
    }
}

/// Drone.SetTarget: 2 m above what it flies to.
fn set_target(game: &mut Game, d: i32, mut target: V3) {
    let l = &mut game.ext.logistics;
    let Some(dr) = l.drones.get_mut(&d) else { return };
    if dr.access < 0 {
        return;
    }
    if motion::length_sq(target) > motion::EPSILON {
        target[1] += UP;
    }
    if let Some(a) = l.access_data.get_mut(dr.access as usize) {
        a.target = target;
    }
    dr.target = target;
}

/// Drone.SetLogisticTask: take the task and head for where it leads.
fn set_task(game: &mut Game, d: i32, task: Option<u32>) {
    let Some(dr) = game.ext.logistics.drones.get_mut(&d) else { return };
    dr.task = task;
    let (inventory, initialized) = (dr.inventory, dr.initialized);
    if let Some(uid) = task.filter(|t| task_state(game, *t) == Some(State::NotAttributed)) {
        set_state(game, uid, State::ToSupply);
    }
    if !initialized {
        return;
    }
    let Some(t) = task.and_then(|uid| game.ext.logistics.tasks.get(&uid).cloned()) else {
        go_to_station(game, d);
        return;
    };
    let target = match t.state {
        State::ToSupply if t.spawned => go_position(game, t.wo),
        State::ToSupply => t.supply_wo.filter(|_| items(game, inventory).is_empty()).and_then(|w| go_position(game, w)),
        State::ToDemand => go_position(game, t.demand_wo),
        State::Done => None,
        State::NotAttributed | State::Loading | State::Unloading => return,
    };
    match target {
        Some(p) => set_target(game, d, p),
        None => end_assignment(game, d),
    }
}

/// Drone.UpdateState: the job found the drone at its target.
fn update_state(game: &mut Game, d: i32) {
    set_task(game, d, drone_task(game, d));
    let Some(dr) = game.ext.logistics.drones.get(&d) else { return };
    if motion::distance(dr.position, dr.target) >= dr.distance_min {
        return;
    }
    set_target(game, d, [0.0; 3]);
    let Some(uid) = drone_task(game, d) else {
        go_in_station(game, d);
        return;
    };
    match task_state(game, uid) {
        Some(State::ToSupply) => set_state(game, uid, State::Loading),
        Some(State::ToDemand) => set_state(game, uid, State::Unloading),
        _ => {}
    }
    match task_state(game, uid) {
        Some(State::Loading) => drone_load(game, d, uid),
        Some(State::Unloading) => drone_unload(game, d, uid),
        _ => {}
    }
    if drone_task(game, d).is_some_and(|t| task_state(game, t) == Some(State::Done)) {
        end_assignment(game, d);
    }
    // SaveDronePosition.
    if let Some(dr) = game.ext.logistics.drones.get(&d) {
        let (position, rotation) = (dr.position, dr.rotation);
        let planet = planet_of(game, d);
        if let Some(w) = game.wos.get_mut(&d) {
            w.position = position;
            w.rotation = rotation;
            w.planet = planet;
        }
    }
}

/// Drone.DroneLoad.
fn drone_load(game: &mut Game, d: i32, uid: u32) {
    let t = game.ext.logistics.tasks[&uid].clone();
    let inventory = game.ext.logistics.drones[&d].inventory;
    if t.spawned {
        add_world_object_to_inventory(game, t.wo, inventory, true);
    } else if let Some(s) = t.supply.filter(|s| game.inventories.get(s).is_some_and(|i| i.items.contains(&t.wo))) {
        remove_item(game, t.wo, s);
        game.add_existing_item(t.wo, inventory, true, 0);
    } else {
        end_assignment(game, d);
        return;
    }
    set_state(game, uid, State::ToDemand);
    set_task(game, d, Some(uid));
}

/// Drone.DroneUnload: into the demand inventory; out of the drone once that worked.
fn drone_unload(game: &mut Game, d: i32, uid: u32) {
    let t = game.ext.logistics.tasks[&uid].clone();
    let inventory = game.ext.logistics.drones[&d].inventory;
    if items(game, inventory).contains(&t.wo) && game.inventories.contains_key(&t.demand) && game.add_existing_item(t.wo, t.demand, true, 0) {
        remove_item(game, t.wo, inventory);
    }
    end_assignment(game, d);
}

/// Drone.EndAssignement: whatever is still aboard is destroyed; next task or back to a
/// station.
fn end_assignment(game: &mut Game, d: i32) {
    let Some(inventory) = game.ext.logistics.drones.get(&d).map(|dr| dr.inventory) else { return };
    if let Some(&wo) = items(game, inventory).first() {
        game.destroy_world_object(wo, false);
    }
    if let Some(uid) = drone_task(game, d) {
        set_state(game, uid, State::Done);
        if let Some(dr) = game.ext.logistics.drones.get_mut(&d) {
            dr.task = None;
        }
    }
    let next = non_attributed_task(game, planet_of(game, d));
    set_task(game, d, next);
    if game.ext.logistics.drones.contains_key(&d) && drone_task(game, d).is_none() {
        go_to_station(game, d);
    }
}

/// Drone.GoToStation.
fn go_to_station(game: &mut Game, d: i32) {
    set_target(game, d, [0.0; 3]);
    closest_available_station(game, d);
    if let Some(st) = valid_station(game, game.ext.logistics.drones.get(&d).and_then(|dr| dr.station)) {
        let p = station_position(game, st);
        set_target(game, d, p);
    }
}

/// Drone.GoInStation.
fn go_in_station(game: &mut Game, d: i32) {
    let Some(st) = valid_station(game, game.ext.logistics.drones.get(&d).and_then(|dr| dr.station)) else { return };
    if return_drone_to_station(game, st, d) {
        drone_on_destroy(game, d);
    } else {
        go_to_station(game, d);
    }
}

/// Drone.OnDestroy (also called by GoInStation when the drone went in).
fn drone_on_destroy(game: &mut Game, d: i32) {
    manager_remove_drone(game, d);
    if let Some(uid) = drone_task(game, d) {
        if task_state(game, uid) != Some(State::Done) {
            set_state(game, uid, State::Done);
            game.ext.logistics.drones.get_mut(&d).unwrap().task = None;
        }
    }
}

/// Drone.SetClosestAvailableDroneStation: the nearest station of its planet with room, else
/// the one it had, else any of its planet.
fn closest_available_station(game: &mut Game, d: i32) {
    let planet = planet_of(game, d);
    let Some(dr) = game.ext.logistics.drones.get(&d) else { return };
    let position = dr.position;
    let mut chosen = valid_station(game, dr.station);
    let mut best = f32::MAX;
    let mut candidates = Vec::new();
    for (st, s) in game.ext.logistics.stations.iter() {
        if s.planet != planet {
            continue;
        }
        candidates.push(*st);
        if game.inventories.get(&s.inventory).is_some_and(|i| !i.is_full()) {
            let d2 = motion::distance_sq(station_position(game, *st), position);
            if d2 < best {
                best = d2;
                chosen = Some(*st);
            }
        }
    }
    if chosen.is_none() && !candidates.is_empty() {
        chosen = Some(candidates[game.rng.range(0, candidates.len() as i32) as usize]);
    }
    game.ext.logistics.drones.get_mut(&d).unwrap().station = chosen;
}

// ---- what clients see of drones -------------------------------------------------------------

/// NetworkTransform on the server: each tick, the axes that moved past their threshold, to
/// the clients that see the drone (only for drones on the current planet: the others'
/// GameObjects are inactive). The spawn data is kept current for late joiners.
fn send_transforms(game: &mut Game, tick: i32) {
    let current = game.planet_hash();
    let ids: Vec<i32> = game.ext.logistics.drones.keys().copied().collect();
    for d in ids {
        let active = planet_of(game, d) == current;
        let dr = game.ext.logistics.drones.get_mut(&d).unwrap();
        let Some(net) = dr.net else { continue };
        let Some(o) = game.net.objects.get_mut(&net) else { continue };
        o.position = dr.position;
        o.rotation = dr.rotation;
        o.tail = motion::Transform::sync_tail(dr.behaviour, dr.position, dr.rotation, tick).into();
        if !active || o.owner != 0 {
            continue;
        }
        if let Some(state) = dr.transform.tick(dr.position, dr.rotation, tick) {
            let body = motion::message(net, dr.behaviour, &state);
            let hidden = dr.hidden.clone();
            send_to_observers(game, &hidden, NETWORK_TRANSFORM, body);
        }
    }
}

/// DroneNetworkVibility: drones far from a player (30 to 100 m, shorter with more drones) or
/// on another planet are hidden from that player. The real server leaves its own (host)
/// player out; every player here is a remote one.
fn visibility(game: &mut Game) {
    if game.ext.logistics.drones.is_empty() {
        return;
    }
    let count = game.ext.logistics.drones.len() as f32;
    let current = game.planet_hash();
    let players: Vec<(u64, V3)> = game
        .clients
        .values()
        .filter(|c| c.approved)
        .filter_map(|c| Some((c.id, game.net.objects.get(&c.player?)?.position)))
        .collect();
    // Who is a player and who a drone is hidden from, as sets: every drone is asked about every player at
    // every frame, and a list searched for each pair is players squared per drone.
    let player_ids: crate::idmap::FxHashSet<u64> = players.iter().map(|p| p.0).collect();
    let mut hidden_from: crate::idmap::FxHashSet<u64> = Default::default();
    let ids: Vec<i32> = game.ext.logistics.drones.keys().copied().collect();
    for d in ids {
        let active = planet_of(game, d) == current;
        let dr = game.ext.logistics.drones.get_mut(&d).unwrap();
        let Some(net) = dr.net else { continue };
        dr.hidden.retain(|c| player_ids.contains(c));
        hidden_from.clear();
        hidden_from.extend(dr.hidden.iter().copied());
        let [min, max, most] = dr.visibility;
        let range = min + (max - min) * (1.0 - (count / most).min(1.0));
        let mut changes = Vec::new();
        for (c, p) in &players {
            let far = !active || motion::distance_sq(dr.position, *p) > range * range;
            let hidden = hidden_from.contains(c);
            if far && !hidden {
                dr.hidden.push(*c);
                changes.push((*c, false));
            } else if !far && hidden {
                dr.hidden.retain(|x| x != c);
                changes.push((*c, true));
            }
        }
        for (c, show) in changes {
            if show {
                let handle = game.net.scene_handle;
                let Some(o) = game.net.objects.get(&net) else { continue };
                let body = [vec![1], o.scene_object(handle)].concat();
                game.net.out.push((Target::Only(c), CREATE_OBJECT, body));
            } else {
                game.out_to(Target::Only(c), crate::protocol::destroy_object(net));
            }
        }
    }
}

#[cfg(test)]
mod tests;
#[cfg(test)]
mod play_tests;
