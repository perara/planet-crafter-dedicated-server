//! SpaceCraft.PlanetNetworkLoader and PlanetUnlocker, server side: the whole session moving to
//! another planet (SwitchToPlanetServerRpc), what the server does once that planet is loaded
//! (SessionController's planetIsLoaded), and the unlocked planets. Also WorldRandomizer and
//! Unity.Mathematics.Random, and the hooks of the instance, loot and toxic-area ports.
//!
//! Where the port differs from the game, because the server has no scene:
//! - The landing spot on a planet that has a departure platform is the platform's
//!   MachineDeparturePlatform._landingPosition child; with game tables extracted before the
//!   dump carried its pose, the platform's own position and rotation stand in for it.
//! - The host loads the new planet's scene before planetIsLoaded runs; here it runs right after
//!   the RPC, in the same tick.
//! - Only planets in _availablePlanets can be travelled to (the game does not check): the
//!   standalone server offers only the free planets and cannot check DLC ownership.

mod loot;

use std::collections::HashMap;

use serde_json::json;

use crate::data::stable_hash;
use crate::geometry::MathRandom;
use crate::game::{Args, Game};
use crate::model::WorldObject;
use crate::net::{enc, Target};

#[derive(Default, serde::Serialize, serde::Deserialize)]
pub struct State {
    pub instances: crate::instances::State,
}

fn singleton_group(game: &Game, class: &str, field: &str) -> Option<usize> {
    game.data.group_of_ref(game.data.singletons[class][field].as_str()?)
}

fn interplanetary_pod(game: &Game) -> Option<usize> {
    singleton_group(game, "PlanetNetworkLoader", "_interplanetaryPodGroup")
}

/// PlanetUnlocker.GetIsPlanetUnlocked: unlocked planets are the ones with terraformation units.
pub fn is_unlocked(game: &Game, planet_hash: i32) -> bool {
    game.units.planet(planet_hash).is_some()
}

/// PlanetUnlocker.SetPlanetUnlocked: an available planet not unlocked yet becomes unlocked for
/// everyone, and the host's UnlockPlanetClientRpc creates its units.
pub fn set_planet_unlocked(game: &mut Game, index: usize) -> bool {
    let id = game.net_data.planets[index].id.clone();
    let hash = stable_hash(&id);
    if !crate::scene::available_planets(game).contains(&index) || is_unlocked(game, hash) {
        return false;
    }
    game.net.rpc_scene(Target::All, "PlanetUnlocker.UnlockPlanetClientRpc", &[json!(hash)]);
    crate::units::create(game, &id, None);
    // scene::on_client_approved tells joining clients about the saved planets: this is one now.
    game.save.planets.push(json!({ "planetId": id }));
    true
}

/// WorldObjectsHandler.UpdateAllWorldObjectsActivation: objects of other planets go inactive.
fn update_all_world_objects_activation(game: &mut Game) {
    let current = game.planet_hash();
    let objects: Vec<(u64, bool)> = game.wos.values().filter(|w| w.planet_hash() != 0).filter_map(|w| Some((w.net?, w.planet_hash() == current))).collect();
    for (net, active) in objects {
        for class in ["GroupNetworkContainer", "ConstructibleProxy"] {
            if game.net.behaviour_index(net, class).is_some() {
                game.net.set(net, class, "_active", enc::bool(active));
            }
        }
    }
}

/// planetIsLoaded after a planet switch: WorldInstanceHandler closes the opened instance, then
/// SessionController updates the activations (the planet's machines wake up) and unlocks the
/// planet.
fn planet_loaded(game: &mut Game) {
    crate::instances::unload_world_instance(game);
    update_all_world_objects_activation(game);
    crate::machines::planet_loaded(game);
    set_planet_unlocked(game, game.planet);
}

/// Unity's Quaternion.eulerAngles.y, in [0, 360).
fn yaw_degrees(q: [f32; 4]) -> f32 {
    let y = crate::geometry::euler_y(q);
    if y < 0.0 { y + 360.0 } else { y }
}

/// MachineDeparturePlatform.GetLandingTransform: the platform's _landingPosition child, which sits
/// where the dump's pose (relative to the prefab root) puts it; the platform itself without one.
fn landing_transform(game: &Game, platform: &WorldObject) -> ([f32; 3], [f32; 4]) {
    let components = game.data.group_prefabs[&game.data.groups[platform.group].id]["components"].as_array().cloned().unwrap_or_default();
    let pose = components.iter().find(|c| c["type"] == "MachineDeparturePlatform").map(|c| c["poses"]["_landingPosition"].clone()).filter(|p| !p.is_null());
    let Some(pose) = pose else { return (platform.position, platform.rotation) };
    let f = |v: &serde_json::Value, i: usize| v[i].as_f64().unwrap_or(if i == 3 { 1.0 } else { 0.0 }) as f32;
    let offset = crate::geometry::rotate(platform.rotation, [0, 1, 2].map(|i| f(&pose["position"], i)));
    ([0, 1, 2].map(|i| platform.position[i] + offset[i]), crate::geometry::mul(platform.rotation, [0, 1, 2, 3].map(|i| f(&pose["rotation"], i))))
}

/// PlanetNetworkLoader.SwitchToPlanetServerRpc: the session moves to another planet. The
/// rocket (a world object, 0 for none) has its inventory moved into the arrival pod.
pub fn switch_to_planet(game: &mut Game, planet_hash: i32, rocket_wo: i32, spawn_index: i32) {
    let Some(index) = game.net_data.planets.iter().position(|p| stable_hash(&p.id) == planet_hash) else { return };
    if !crate::scene::available_planets(game).contains(&index) {
        eprintln!("switch to planet {}: not available on this server", game.net_data.planets[index].id);
        return;
    }
    let planet_id = game.net_data.planets[index].id.clone();
    let (mut position, mut rotation) = ([0.0f32; 3], [0.0f32, 0.0, 0.0, 1.0]);
    if !is_unlocked(game, planet_hash) {
        let spawns = game.data.asset(&format!("PlanetData:PlanetData-{planet_id}")).map(|p| p["spawnPositions"][spawn_index.max(0) as usize]["positions"].clone());
        let Some(list) = spawns.as_ref().and_then(|s| s.as_array()).filter(|l| !l.is_empty() && spawn_index >= 0) else {
            eprintln!("switch to planet {planet_id}: no spawn positions {spawn_index}");
            return;
        };
        let spawn = &list[game.rng.range(0, list.len() as i32) as usize];
        let f = |v: &serde_json::Value, i: usize| v.get(i).and_then(|x| x.as_f64()).unwrap_or(0.0) as f32;
        position = [0, 1, 2].map(|i| f(&spawn["position"], i));
        rotation = [0, 1, 2, 3].map(|i| f(&spawn["rotation"], i));
    } else {
        let platforms: Vec<usize> = game.data.singletons["PlanetNetworkLoader"]["_departurePlatformGroups"].as_array().into_iter().flatten().filter_map(|r| game.data.group_of_ref(r.as_str()?)).collect();
        if let Some(w) = platforms.iter().find_map(|g| game.first_world_object_of_group(*g, planet_hash)).and_then(|w| game.wos.get(&w)) {
            (position, rotation) = landing_transform(game, w);
        }
    }
    let Some(pod_group) = interplanetary_pod(game) else { return };
    // The pod left at the last arrival: its content drops at the new arrival, then it goes.
    if let Some(pod) = game.first_world_object_of_group(pod_group, planet_hash) {
        let inventory = game.wos[&pod].linked_inventory;
        let items = game.inventories.get(&inventory).map(|i| i.items.clone()).unwrap_or_default();
        for wo in items.iter().rev() {
            game.drop_on_floor(*wo, [position[0], position[1] + 2.0, position[2]], [0.0, 0.0, 0.0, 1.0], 0.0, true, false, 0, 0.0, false, 0);
            game.inventories_rpc(0, "InventoriesHandler.RemoveItemFromInventoryServerRpc", &Args(vec![json!(wo), json!(inventory), json!(false)]));
            if let Some(w) = game.wos.get_mut(wo) {
                w.planet = planet_hash;
            }
        }
        game.destroy_world_object(pod, true);
    }
    game.planet = index;
    game.set_on("PlanetNetworkLoader", "_planetIndex", enc::short(index as i16));
    let arrival = [position[0], position[1] + 1.0, position[2]];
    game.net.rpc_scene(Target::All, "PlanetNetworkLoader.SwitchToPlanetClientRpc", &[json!(index), json!(arrival), json!(yaw_degrees(rotation) as i32)]);
    let pod = game.new_world_object_id();
    game.create_world_object(pod_group, pod, true);
    let w = game.wos.get_mut(&pod).unwrap();
    w.position = position;
    // Unity normalizes a zero quaternion (some spawn positions have one) to the identity.
    w.rotation = if rotation == [0.0; 4] { [0.0, 0.0, 0.0, 1.0] } else { rotation };
    w.planet = planet_hash;
    game.instantiate(pod, false, 0);
    if rocket_wo != 0 {
        if let Some(from) = game.wos.get(&rocket_wo).map(|w| w.linked_inventory) {
            let to = game.wos[&pod].linked_inventory;
            game.inventories_rpc(0, "InventoriesHandler.TransferAllItemsServerRpc", &Args(vec![json!(from), json!(to)]));
            game.destroy_world_object(rocket_wo, true);
        }
    }
    planet_loaded(game);
}

/// WorldRandomizer.GetReplacementForGroup: what a mineable group becomes in a world with
/// randomized mineables (the world seed picks it, the same on every peer). Clients replace
/// their scene minables themselves; the server needs it for ore veins (extractors).
#[allow(dead_code)]
pub fn randomized_group(game: &Game, group: usize) -> Option<usize> {
    if game.save.settings["randomizeMineables"].as_bool() != Some(true) {
        return None;
    }
    let refs = |field: &str| -> Vec<usize> { game.data.singletons["WorldRandomizer"][field].as_array().into_iter().flatten().filter_map(|r| game.data.group_of_ref(r.as_str()?)).collect() };
    let all = refs("canBeReplacedBy");
    // ponytail: a world seed of 0 makes the game seed from the clock; here it draws zeros.
    let mut random = MathRandom::new(game.save.settings["worldSeed"].as_u64().unwrap_or(0) as u32);
    let mut list = all.clone();
    let mut replaced = HashMap::new();
    for g in refs("toReplace") {
        let i = random.next_int(0, list.len() as i32) as usize;
        replaced.insert(g, list.remove(i));
        if list.is_empty() {
            list = all.clone();
        }
    }
    replaced.get(&group).copied()
}

pub fn init(game: &mut Game) {
    // The first planetIsLoaded: the save's instances (LoadSavedData), then SetPlanetUnlocked.
    crate::instances::init(game);
    set_planet_unlocked(game, game.planet);
}

pub fn tick(game: &mut Game) {
    crate::instances::tick(game);
}

pub fn rpc(game: &mut Game, client: u64, object: u64, _behaviour: u16, name: &str, a: &Args) -> bool {
    match name {
        "PlanetNetworkLoader.SwitchToPlanetServerRpc" => switch_to_planet(game, a.i(0), a.i(1), a.i(2)),
        _ => return crate::instances::rpc(game, client, object, name, a) || crate::toxic::rpc(game, client, name, a),
    }
    true
}

pub fn on_instantiated(game: &mut Game, wo: i32) {
    loot::populate_random_inventory(game, wo);
}

pub fn on_destroyed(game: &mut Game, wo: &WorldObject) {
    crate::instances::on_destroyed(game, wo);
}

pub fn inventory_changed(_game: &mut Game, _inventory: i32, _wo: i32, _added: bool) {}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::net::VarValue;

    /// A Prime world with an escape pod, an Iron in a rocket container, and nothing else.
    pub fn game() -> Option<Game> {
        let mut save = crate::save::SaveFile::empty();
        save.world = json!({"terraTokens": 0, "openedInstanceSeed": 0, "openedInstanceTimeLeft": 0});
        save.planets = vec![json!({"planetId": "Prime"})];
        save.objects = vec![
            json!({"id": 208217122, "gId": "EscapePod", "pos": "558.2,2.47,494.1", "rot": "0,0,0,1", "planet": -1140328421}),
            json!({"id": 205000000, "gId": "Container1", "liId": 2, "pos": "560,3,500", "rot": "0,0,0,1", "planet": -1140328421}),
            json!({"id": 205000001, "gId": "Iron"}),
        ];
        save.inventories = vec![json!({"id": 2, "woIds": "205000001", "size": 30})];
        save.stats = json!({"craftedObjects": 0});
        save.settings = json!({"planetId": "Prime", "worldSeed": 620174666, "randomizeMineables": false, "dyingConsequencesLabel": "DropSomeItems"});
        crate::game::test_game(save)
    }

    pub fn rpcs_sent(game: &Game) -> Vec<String> {
        game.net.out.iter().filter(|(_, kind, _)| *kind == crate::protocol::CLIENT_RPC).filter_map(|(_, _, body)| {
            let mut r = crate::wire::Reader::new(body);
            r.packed_u64()?;
            r.packed_u16()?;
            Some(game.net.spec.get(r.packed_u32()?)?.name.clone())
        }).collect()
    }

    #[test]
    fn math_random_matches_unity_mathematics() {
        // new Random(1): state 1 -> 270369 after the constructor's NextState.
        let mut r = MathRandom::new(1);
        assert_eq!(r.state, 270369);
        assert_eq!(r.next_state(), 270369);
        let mut r = MathRandom::new(123456789);
        let v: Vec<i32> = (0..4).map(|_| r.next_int(0, 5)).collect();
        assert!(v.iter().all(|x| (0..5).contains(x)));
    }

    #[test]
    fn travelling_moves_the_session_and_the_rocket_content() {
        let Some(mut g) = game() else { return };
        let selenea = stable_hash("Selenea");
        assert!(!is_unlocked(&g, selenea));
        let iron = 205000001;
        g.net.out.clear();
        switch_to_planet(&mut g, selenea, 205000000, 0);
        assert_eq!(g.planet_id(), "Selenea");
        assert!(is_unlocked(&g, selenea), "the units of a newly reached planet exist");
        let (loader, _) = g.net.find_class("PlanetNetworkLoader").unwrap();
        assert!(matches!(g.net.get(loader, "PlanetNetworkLoader", "_planetIndex"), Some(VarValue::Plain(b)) if *b == enc::short(2)));
        let sent = rpcs_sent(&g);
        let switch = sent.iter().position(|n| n == "PlanetNetworkLoader.SwitchToPlanetClientRpc").unwrap();
        let unlock = sent.iter().position(|n| n == "PlanetUnlocker.UnlockPlanetClientRpc").unwrap();
        assert!(switch < unlock);
        // The arrival pod on Selenea at the Standard spawn, holding the rocket's content.
        let pod_group = interplanetary_pod(&g).unwrap();
        let pod = g.first_world_object_of_group(pod_group, selenea).unwrap();
        assert_eq!(g.wos[&pod].position, [-415.0, 34.65, -82.0]);
        assert_eq!(g.inventories[&g.wos[&pod].linked_inventory].items, vec![iron]);
        assert!(!g.wos.contains_key(&205000000), "the rocket is gone");
        // Prime's escape pod is now inactive for clients.
        let escape = g.wos[&208217122].net.unwrap();
        assert!(matches!(g.net.get(escape, "ConstructibleProxy", "_active"), Some(VarValue::Plain(b)) if *b == enc::bool(false)));
        assert!(g.save.planets.iter().any(|p| p["planetId"] == "Selenea"));
        // Coming back: Prime is unlocked, there is no platform, so the arrival is at 0,0,0;
        // a second trip to Selenea empties the pod left there onto the floor.
        switch_to_planet(&mut g, stable_hash("Prime"), 0, 0);
        assert_eq!(g.planet_id(), "Prime");
        switch_to_planet(&mut g, selenea, 0, 0);
        assert_eq!(g.wos[&iron].planet, selenea);
        assert!(g.wos[&iron].is_placed());
        assert!(!g.wos.contains_key(&pod), "the old pod is replaced");
    }

    #[test]
    fn arrivals_land_on_the_platform_landing_spot() {
        let Some(mut g) = game() else { return };
        let platform = g.data.group("DeparturePlatform").unwrap();
        let has_pose = g.data.group_prefabs["DeparturePlatform"]["components"].as_array().unwrap().iter().any(|c| c["poses"]["_landingPosition"].is_object());
        assert!(has_pose, "game tables extracted before the dump carried component poses: extract them again");
        let selenea = stable_hash("Selenea");
        switch_to_planet(&mut g, selenea, 0, 0);
        // A departure platform on Selenea, turned a quarter.
        let id = g.new_world_object_id();
        g.create_world_object(platform, id, true);
        let w = g.wos.get_mut(&id).unwrap();
        (w.position, w.rotation, w.planet) = ([100.0, 20.0, 50.0], crate::geometry::from_euler_y(90.0), selenea);
        g.instantiate(id, false, 0);
        switch_to_planet(&mut g, stable_hash("Prime"), 0, 0);
        g.net.out.clear();
        switch_to_planet(&mut g, selenea, 0, 0);
        // _landingPosition sits at (-2.95, 7.062, -9.97) in the platform, turned a quarter more.
        let pod = g.first_world_object_of_group(interplanetary_pod(&g).unwrap(), selenea).unwrap();
        let (p, r) = (g.wos[&pod].position, g.wos[&pod].rotation);
        let expected = [90.03, 27.062, 52.95];
        assert!((0..3).all(|i| (p[i] - expected[i]).abs() < 1e-3), "{p:?}");
        assert!((yaw_degrees(r) - 180.0).abs() < 0.01, "{r:?}");
    }

    /// The travel RPC (a departure platform's SwitchPlanet) moves the world to the planet; a
    /// planet the server does not offer is refused.
    #[test]
    fn players_travel_by_rpc() {
        use crate::testkit::*;
        let Some(mut g) = game() else { return };
        join(&mut g, 1, "Ann");
        send_scene(&mut g, 1, "PlanetNetworkLoader.SwitchToPlanetServerRpc", &[json!(stable_hash("Humble")), json!(0), json!(0)]);
        assert_eq!(g.planet_id(), "Prime");
        send_scene(&mut g, 1, "PlanetNetworkLoader.SwitchToPlanetServerRpc", &[json!(stable_hash("Selenea")), json!(0), json!(0)]);
        assert_eq!(g.planet_id(), "Selenea");
        assert_eq!(sent(&g, "PlanetNetworkLoader.SwitchToPlanetClientRpc").len(), 1);
        let g = reload(&mut g);
        assert_eq!(g.planet_id(), "Selenea", "the save keeps where the world is");
    }

    #[test]
    fn dlc_planets_cannot_be_reached() {
        let Some(mut g) = game() else { return };
        switch_to_planet(&mut g, stable_hash("Humble"), 0, 0);
        assert_eq!(g.planet_id(), "Prime");
    }
}
