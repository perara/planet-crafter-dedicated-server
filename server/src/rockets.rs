//! Rockets, server side: launch platforms sending rockets to space (ActionSendInSpace with
//! ConstructibleProxy.SendInSpaceServerRpc), the space multipliers that keep what was sent and
//! multiply the planet's units, departure platforms (endings and interplanetary travel, see
//! MachineDeparturePlatform), and the back-and-forth rockets of the trade and interplanetary
//! exchange platforms (back_and_forth, with LogisticInterplanetaryManager in interplanetary).
//! MachinePlanetChanger, PlanetChanger and MachineRocketLand's visuals are client side only.

mod back_and_forth;
mod interplanetary;

use indexmap::IndexMap;
use serde_json::json;

use crate::game::{Args, Game};
use crate::model::WorldObject;
use crate::net::Target;
use crate::units::Kind;

// Provided by the meteo and planets ports (MeteoHandler.SendSomethingInSpace,
// PlanetNetworkLoader.SwitchToPlanet).
use crate::meteo::send_something_in_space;
use crate::planets::switch_to_planet;

// For the machines port: MachineOptimizer's fuses, and an item finishing its growth inside an
// inventory (Inventory.inventoryContentGrowthFinished).
pub use back_and_forth::add_speed_multiplier;
pub use interplanetary::inventory_growth_finished;

/// GameConfig.spaceGlobalMultipliersGroupIds: the building in space that holds the rockets
/// multiplying one unit.
const SPACE_MULTIPLIERS: [(Kind, &str); 8] = [
    (Kind::Oxygen, "SpaceMultiplierOxygen"),
    (Kind::Heat, "SpaceMultiplierHeat"),
    (Kind::Pressure, "SpaceMultiplierPressure"),
    (Kind::Plants, "SpaceMultiplierPlants"),
    (Kind::Insects, "SpaceMultiplierInsects"),
    (Kind::Animals, "SpaceMultiplierAnimals"),
    (Kind::Biomass, "SpaceMultiplierBiomass"),
    (Kind::Purification, "SpaceMultiplierPurification"),
];
/// GameConfig.spaceLocation.
const SPACE_LOCATION: [f32; 3] = [-500.0, -500.0, -500.0];
/// MachineRocket.destroyAfter: an ignited rocket despawns on the server after this long.
const ROCKET_DESPAWN_AFTER: f32 = 40.0;
/// MachineDeparturePlatform.DepartureType.InterplanetaryTravel.
const INTERPLANETARY_TRAVEL: i32 = 9;
/// MachineDeparturePlatform's MachineGetInRange around its craft spawn: the nearest rocket
/// within this distance of the platform.
const ROCKET_ON_PLATFORM_RANGE: f32 = 8.0;

#[derive(Default, serde::Serialize, serde::Deserialize)]
pub struct State {
    /// MachineRocketBackAndForth components, by world object.
    machines: IndexMap<i32, back_and_forth::Machine>,
    /// MachineDeparturePlatform components that started a sequence, by world object.
    departures: IndexMap<i32, Departure>,
    /// Space multipliers (WorldUnitMultiplierViaInventory): inventory -> world object.
    multipliers: IndexMap<i32, i32>,
    /// MachineRocket.DespawnAfterDelay: (due, rocket world object, its network object).
    despawns: Vec<(f32, i32, u64)>,
    interplanetary: interplanetary::State,
    /// Game time of the previous tick (Time.deltaTime for MachineRocketLand).
    last_tick: f32,
}

#[derive(Default, serde::Serialize, serde::Deserialize)]
struct Departure {
    /// _sequenceStarted.
    started: bool,
    /// The SwitchPlanet coroutine: when it sends SwitchPlanetClientRpc.
    switch_at: Option<f32>,
    /// SwitchPlanetClientRpc went out; the switch itself follows a frame later.
    switching: bool,
    planet: i32,
    spawn: i32,
    /// _rocket, found by the server's own run of StartDepartureSequenceClientRpc.
    rocket: Option<i32>,
    /// An ending: the clients playing it (see departures_tick).
    watchers: Option<Vec<u64>>,
}

fn has_component(game: &Game, group: usize, ty: &str) -> bool {
    !game.data.prefab_components(&game.data.groups[group].id, ty).is_empty()
}

fn component_f32(game: &Game, group: usize, ty: &str, field: &str, default: f32) -> f32 {
    game.data.prefab_components(&game.data.groups[group].id, ty).first().and_then(|c| c[field].as_f64()).map(|v| v as f32).unwrap_or(default)
}

fn wo_of(game: &Game, object: u64) -> Option<i32> {
    game.net.objects.get(&object).and_then(|o| o.wo)
}

/// WorldUnit.ForceResetValues for one unit of a planet.
fn force_reset(game: &mut Game, kind: Kind, planet: i32) {
    if let Some(p) = game.units.planet_mut(planet) {
        p.units.iter_mut().filter(|u| u.kind == kind).for_each(|u| u.force_reset = true);
    }
}

pub fn init(game: &mut Game) {
    interplanetary::reset_list(game);
    game.ext.rockets.last_tick = game.time;
}

pub fn tick(game: &mut Game) {
    let dt = game.time - game.ext.rockets.last_tick;
    game.ext.rockets.last_tick = game.time;
    despawn_ignited_rockets(game);
    departures_tick(game);
    back_and_forth::tick(game, dt);
}

pub fn rpc(game: &mut Game, client: u64, object: u64, _behaviour: u16, name: &str, a: &Args) -> bool {
    match name {
        "ConstructibleProxy.SendInSpaceServerRpc" => send_in_space(game, client, object),
        "MachineDeparturePlatform.StartDepartureSequenceServerRpc" => start_departure(game, object, a.i(0), a.i(1), a.i(2)),
        "LogisticInterplanetaryManager.TransferGroupsServerRpc" => {
            if let Some(group) = game.data.group_by_hash(a.i(0)) {
                interplanetary::transfer_groups(game, group, a.i(1), a.i(2), a.i(3));
            }
        }
        _ => return back_and_forth::rpc(game, client, object, name, a),
    }
    true
}

pub fn on_instantiated(game: &mut Game, wo: i32) {
    interplanetary::on_instantiated(game, wo);
    back_and_forth::spawned(game, wo);
    space_multiplier_spawned(game, wo);
}

pub fn on_destroyed(game: &mut Game, wo: &WorldObject) {
    let s = &mut game.ext.rockets;
    s.machines.shift_remove(&wo.id);
    s.departures.shift_remove(&wo.id);
    s.multipliers.retain(|_, w| *w != wo.id);
    interplanetary::on_destroyed(game, wo);
}

pub fn inventory_changed(game: &mut Game, inventory: i32, wo: i32, added: bool) {
    interplanetary::inventory_changed(game, inventory, wo, added);
    space_multiplier_changed(game, inventory, wo, added);
    back_and_forth::inventory_changed(game, inventory);
}

// ---- ActionSendInSpace -------------------------------------------------------------------

/// The rocket standing on a platform: the nearest placed world object with a network object
/// and a `component` on its prefab (see ROCKET_ON_PLATFORM_RANGE).
fn rocket_on(game: &Game, platform: i32, component: &str) -> Option<i32> {
    let p = game.wos.get(&platform)?;
    let (position, planet) = (p.position, p.planet_hash());
    game.wos
        .values()
        .filter(|w| w.id != platform && w.net.is_some() && w.is_placed() && w.planet_hash() == planet && has_component(game, w.group, component))
        .map(|w| (w.position.iter().zip(position).map(|(a, b)| (a - b) * (a - b)).sum::<f32>().sqrt(), w.id))
        .filter(|(d, _)| *d < ROCKET_ON_PLATFORM_RANGE)
        .min_by(|a, b| a.0.total_cmp(&b.0))
        .map(|(_, id)| id)
}

/// A platform's ActionSendInSpace pose (`key`: "pose", the button's own; or "poses" field
/// `field`), in the world. Err: the tables do not have it, which they always do from tables
/// format 2 (pcds update reads them again).
fn send_pose(game: &Game, platform: i32, field: Option<&str>) -> Result<[f32; 3], String> {
    let p = &game.wos[&platform];
    let group = &game.data.groups[p.group].id;
    let component = game.data.group_prefabs[group]["components"].as_array().into_iter().flatten().find(|c| c["type"] == "ActionSendInSpace");
    let pose = component.map(|c| match field {
        Some(f) => &c["poses"][f]["position"],
        None => &c["pose"]["position"],
    });
    let pose = pose.and_then(|v| v.as_array()).filter(|a| a.len() == 3 && a.iter().all(|x| x.is_number())).ok_or_else(|| format!("{group}: the tables have no ActionSendInSpace {} pose", field.unwrap_or("button")))?;
    let d = crate::geometry::rotate(p.rotation, [0, 1, 2].map(|i| pose[i].as_f64().unwrap_or(0.0) as f32));
    Ok([0, 1, 2].map(|i| p.position[i] + d[i]))
}

/// ActionSendInSpace.SendRocket's lookup: WorldObjectsHandler.GetWorldObjectAtPosition, the
/// first world object within sqrt(1.19e-7) m of the spot, if it has a game object there.
fn object_at(game: &Game, spot: [f32; 3]) -> Option<i32> {
    let first = game.wos.values().find(|w| w.position.iter().zip(spot).map(|(a, b)| (a - b) * (a - b)).sum::<f32>() < 1.1920929e-7)?;
    first.net.map(|_| first.id)
}

/// ConstructibleProxy.SendInSpaceServerRpc with ActionSendInSpace.SendRocket.
fn send_in_space(game: &mut Game, client: u64, object: u64) {
    let Some(platform) = wo_of(game, object) else { return };
    // GetComponentInChildren<ActionSendInSpace>() on anything else throws: no reply.
    if !game.wos.get(&platform).is_some_and(|w| has_component(game, w.group, "ActionSendInSpace")) {
        return;
    }
    // ActionSendInSpace.locationGameObject (the craft spawn) and the button itself.
    let (spot, platform_position) = match (send_pose(game, platform, Some("locationGameObject")), send_pose(game, platform, None)) {
        (Ok(s), Ok(b)) => (s, b),
        (Err(e), _) | (_, Err(e)) => {
            eprintln!("rocket launch refused: {e}");
            game.net.rpc(Target::Only(client), object, "ConstructibleProxy.SendInSpaceDisplayErrorClientRpc", &[]);
            return;
        }
    };
    let Some(rocket) = object_at(game, spot) else {
        game.net.rpc(Target::Only(client), object, "ConstructibleProxy.SendInSpaceDisplayErrorClientRpc", &[]);
        return;
    };
    let w = game.wos.get_mut(&rocket).unwrap();
    w.reset_position();
    let (group, net) = (w.group, w.net);
    send_something_in_space(game, group, platform_position);
    handle_rocket_multiplier(game, rocket);
    game.net.rpc(Target::All, object, "ConstructibleProxy.SendInSpaceClientRpc", &[]);
    // The server's own run of SendInSpaceClientRpc ignites the rocket (MachineRocket.Ignite),
    // which despawns its network object later.
    if let Some(net) = net {
        let due = game.time + ROCKET_DESPAWN_AFTER;
        game.ext.rockets.despawns.push((due, rocket, net));
    }
}

/// ActionSendInSpace.HandleRocketMultiplier: the rocket goes into the current planet's space
/// multiplier of each unit it multiplies (created at GameConfig.spaceLocation if needed).
fn handle_rocket_multiplier(game: &mut Game, rocket: i32) {
    let planet = game.planet_hash();
    let kinds: Vec<Kind> = game.units.planet(planet).map(|p| p.units.iter().map(|u| u.kind).collect()).unwrap_or_default();
    let rocket_group = game.wos[&rocket].group;
    for kind in kinds {
        if game.data.groups[rocket_group].unit(kind.unit_type()) == 0.0 {
            continue;
        }
        let Some(group) = SPACE_MULTIPLIERS.iter().find(|(k, _)| *k == kind).and_then(|(_, id)| game.data.group(id)) else { continue };
        let existing = game.constructed.iter().copied().find(|c| game.wos.get(c).is_some_and(|w| w.group == group && w.planet_hash() == planet));
        let multiplier = match existing {
            Some(w) => w,
            None => {
                let id = game.create_world_object(group, 0, true);
                let w = game.wos.get_mut(&id).unwrap();
                w.position = SPACE_LOCATION;
                w.rotation = [0.0, 0.0, 0.0, 1.0];
                w.planet = planet;
                game.instantiate(id, false, 0);
                id
            }
        };
        let inventory = game.wos[&multiplier].linked_inventory;
        game.add_existing_item(rocket, inventory, true, 0);
        force_reset(game, kind, planet);
    }
}

fn despawn_ignited_rockets(game: &mut Game) {
    let now = game.time;
    let (due, waiting): (Vec<_>, Vec<_>) = std::mem::take(&mut game.ext.rockets.despawns).into_iter().partition(|(t, _, _)| *t <= now);
    game.ext.rockets.despawns = waiting;
    for (_, wo, net) in due {
        game.net.despawn(net);
        if let Some(w) = game.wos.get_mut(&wo).filter(|w| w.net == Some(net)) {
            w.net = None;
        }
    }
}

// ---- Space multipliers (WorldUnitMultiplierViaInventory on SpaceMultiplier*) ------------

fn space_multiplier_kind(game: &Game, group: usize) -> Option<Kind> {
    SPACE_MULTIPLIERS.iter().find(|(_, id)| game.data.groups[group].id == *id).map(|(k, _)| *k)
}

/// The unit multiplier one item adds: GetGroupUnitMultiplier / 100.
fn item_multiplier(game: &Game, item: i32, kind: Kind) -> Option<f32> {
    game.wos.get(&item).map(|w| game.data.groups[w.group].unit(kind.unit_type()) / 100.0)
}

/// SetReferenceInventory / OnInventorySet: the multiplier from what the inventory holds.
fn space_multiplier_spawned(game: &mut Game, wo: i32) {
    let Some(w) = game.wos.get(&wo) else { return };
    let (Some(kind), inventory, planet) = (space_multiplier_kind(game, w.group), w.linked_inventory, w.planet_hash()) else { return };
    game.ext.rockets.multipliers.insert(inventory, wo);
    if let Some(index) = crate::model::unit_index(kind.unit_type()) {
        let items = game.inventories.get(&inventory).map(|i| i.items.clone()).unwrap_or_default();
        let mut total = 0.0;
        for item in items {
            total += item_multiplier(game, item, kind).unwrap_or(0.0);
        }
        game.wos.get_mut(&wo).unwrap().unit_multiplier[index] = total;
    }
    force_reset(game, kind, planet);
}

/// OnInventoryModified.
fn space_multiplier_changed(game: &mut Game, inventory: i32, item: i32, added: bool) {
    let Some(&wo) = game.ext.rockets.multipliers.get(&inventory) else { return };
    let Some((kind, planet)) = game.wos.get(&wo).and_then(|w| Some((space_multiplier_kind(game, w.group)?, w.planet_hash()))) else { return };
    if let (Some(index), Some(m)) = (crate::model::unit_index(kind.unit_type()), item_multiplier(game, item, kind)) {
        game.wos.get_mut(&wo).unwrap().unit_multiplier[index] += m * if added { 1.0 } else { -1.0 };
    }
    force_reset(game, kind, planet);
}

// ---- MachineDeparturePlatform -------------------------------------------------------------

/// StartDepartureSequenceServerRpc.
fn start_departure(game: &mut Game, object: u64, ending: i32, planet: i32, spawn: i32) {
    let Some(platform) = wo_of(game, object) else { return };
    let Some(group) = game.wos.get(&platform).map(|w| w.group) else { return };
    if game.ext.rockets.departures.get(&platform).is_some_and(|d| d.started) {
        return;
    }
    // SavedDataHandler.SaveWorldData comes first in the game.
    game.save_requested = true;
    game.net.rpc(Target::All, object, "MachineDeparturePlatform.StartDepartureSequenceClientRpc", &[json!(ending)]);
    // The server's own run of that RPC finds the rocket (MachineGetInRange around the craft spawn).
    let rocket = rocket_on(game, platform, "MachineRocket");
    let launch = component_f32(game, group, "MachineDeparturePlatform", "_launchTime", 25.0);
    let mut d = Departure { started: true, rocket, ..Default::default() };
    if ending == INTERPLANETARY_TRAVEL && planet != 0 {
        d.switch_at = Some(game.time + launch + 1.0);
        d.planet = planet;
        d.spawn = spawn;
    } else if ending != INTERPLANETARY_TRAVEL {
        // The endings play on the clients with a player (the ClientRpc skips the others).
        // ponytail: PlayerReady stands for an active player controller; a client between the
        // two at the start holds the platform until it leaves.
        d.watchers = Some(game.clients.values().filter(|c| c.ready.is_some()).map(|c| c.id).collect());
    }
    game.ext.rockets.departures.insert(platform, d);
}

/// The SwitchPlanet coroutine: SwitchPlanetClientRpc at launch time + 1 s, the switch a frame
/// later. An ending's PlayEndingSequence ends in SceneLoader.LoadScene("Intro"), which ends the
/// host's session (and each client's: SessionController.OnDestroy shuts its network down), so
/// the next session starts with the platform free. This server's session goes on: the platform
/// is free again once everyone who played the ending has left for the intro.
fn departures_tick(game: &mut Game) {
    let clients = &game.clients;
    game.ext.rockets.departures.retain(|_, d| d.watchers.as_ref().is_none_or(|w| w.iter().any(|c| clients.contains_key(c))));
    let now = game.time;
    let mut switches = Vec::new();
    let mut announce = Vec::new();
    for (platform, d) in game.ext.rockets.departures.iter_mut() {
        if d.switching {
            d.switching = false;
            // PlanetLoaded clears _sequenceStarted once the destination is up.
            d.started = false;
            // Without a rocket the game's coroutine throws here: no switch.
            if let Some(rocket) = d.rocket {
                switches.push((d.planet, rocket, d.spawn));
            }
        } else if d.switch_at.is_some_and(|t| now >= t) {
            d.switch_at = None;
            d.switching = true;
            announce.push(*platform);
        }
    }
    for platform in announce {
        if let Some(net) = game.wos.get(&platform).and_then(|w| w.net) {
            game.net.rpc(Target::All, net, "MachineDeparturePlatform.SwitchPlanetClientRpc", &[]);
        }
    }
    for (planet, rocket, spawn) in switches {
        switch_to_planet(game, planet, rocket, spawn);
    }
}

#[cfg(test)]
mod tests;
