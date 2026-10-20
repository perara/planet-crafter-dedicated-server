//! Animals, pets, vehicles and who simulates them: the server side of WorldAnimalSpawnerHandler
//! and InventorySpawnContent (wild animals and animal shelters, feeders), AIAnimalStateController,
//! AnimalCustomizationController, GrowthProxy and PetProxy on the animals they spawn;
//! OwnershipSwitcher, UpdateNetworkRigidBodyState, VehicleBackToGarage, VehicleEquipment and
//! LockNetworkPlayerPosition on vehicles; GroupNetworkContainer.RemoveOwnershipServerRpc; and
//! SceneInterpolationHandler. See hooks.rs for when each entry point runs.
//!
//! The game's server simulates what it owns (navmesh animals, rigidbody vehicles). This server
//! simulates no physics: what the game's server would own and move, the nearest client owns
//! instead (the game's own hand-over rules, with the server counted as a player that is never
//! near anything).

mod interpolation;
pub mod spawn;
mod vehicles;

use std::collections::HashSet;

use crate::clock::GameTime;
use crate::game::{Args, Game};
use crate::model::WorldObject;
use crate::net::enc;

/// The game's frame rate as the server's Update/LateUpdate cadence (the tick rate, 30).
const FRAME: f32 = 1.0 / 30.0;
/// NetworkObject.DontDestroyWithOwner in SceneObject flags.
const DONT_DESTROY_WITH_OWNER: u16 = 1 << 7;

#[derive(Default, serde::Serialize, serde::Deserialize)]
pub struct State {
    pub spawn: spawn::State,
    pub vehicles: vehicles::State,
    pub interpolation: interpolation::State,
    next_frame: GameTime,
    /// Clients connected at the last tick, to see who left (NetworkManager.OnClientDisconnectCallback).
    clients: HashSet<u64>,
}

/// Placed objects already came through on_instantiated while the save loaded.
pub fn init(_game: &mut Game) {}

pub fn tick(game: &mut Game) {
    // Who left since the last tick (most ticks: nobody, and nothing is built for it).
    let known = &game.ext.animals.clients;
    let mut approved = 0;
    let same = game.clients.values().filter(|c| c.approved).all(|c| {
        approved += 1;
        known.contains(&c.id)
    }) && approved == known.len();
    if !same {
        let now: HashSet<u64> = game.clients.values().filter(|c| c.approved).map(|c| c.id).collect();
        let left: Vec<u64> = game.ext.animals.clients.difference(&now).copied().collect();
        game.ext.animals.clients = now;
        for client in left {
            vehicles::client_left(game, client);
        }
    }
    vehicles::watch_owners(game);
    spawn::keep_sequences_unplaced(game);
    if game.time < game.ext.animals.next_frame {
        return;
    }
    game.ext.animals.next_frame = game.time + FRAME;
    spawn::frame(game);
    vehicles::frame(game);
    interpolation::frame(game);
}

pub fn rpc(game: &mut Game, client: u64, object: u64, _behaviour: u16, name: &str, a: &Args) -> bool {
    match name {
        "GroupNetworkContainer.RemoveOwnershipServerRpc" => remove_ownership(game, object),
        _ => return spawn::rpc(game, client, object, name, a) || vehicles::rpc(game, client, object, name, a) || interpolation::rpc(game, client, name, a),
    }
    true
}

pub fn on_instantiated(game: &mut Game, wo: i32) {
    spawn::on_instantiated(game, wo);
    vehicles::on_instantiated(game, wo);
}

pub fn on_destroyed(game: &mut Game, wo: &WorldObject) {
    spawn::on_destroyed(game, wo);
    vehicles::on_destroyed(game, wo);
}

pub fn inventory_changed(game: &mut Game, inventory: i32, wo: i32, added: bool) {
    spawn::inventory_changed(game, inventory, wo, added);
    vehicles::inventory_changed(game, inventory, wo, added);
}

/// GroupNetworkContainer.RemoveOwnershipServerRpc: the owner let its dropped item come to rest;
/// it goes back to the server, no longer kept when that client leaves, transform sync off.
fn remove_ownership(game: &mut Game, object: u64) {
    let Some(o) = game.net.objects.get_mut(&object) else { return };
    if o.owner != 0 {
        o.flags &= !DONT_DESTROY_WITH_OWNER;
        vehicles::set_owner(game, object, 0);
    }
    game.net.set(object, "GroupNetworkContainer", "_transformSync", enc::bool(false));
}

/// Players in the game (PlayersManager.playersControllers: set up by PlayerReady, loading ones
/// are not): (client, player position). The server has no player of its own.
fn players(game: &Game) -> Vec<(u64, [f32; 3])> {
    game.clients.values().filter(|c| c.ready.is_some()).filter_map(|c| Some((c.id, game.net.objects.get(&c.player?)?.position))).collect()
}

fn distance2(a: [f32; 3], b: [f32; 3]) -> f32 {
    a.iter().zip(b).map(|(x, y)| (x - y) * (x - y)).sum()
}

/// The nearest player to a position: (client, squared distance); (0, MAX) without players.
fn nearest(game: &Game, position: [f32; 3]) -> (u64, f32) {
    let mut best = (0, f32::MAX);
    for (client, p) in players(game) {
        let d = distance2(p, position);
        if d < best.1 {
            best = (client, d);
        }
    }
    best
}

/// Squared distance from a client's player to a position; MAX for the server and absent players.
fn player_distance2(game: &Game, client: u64, position: [f32; 3]) -> f32 {
    players(game).into_iter().find(|(c, _)| *c == client).map(|(_, p)| distance2(p, position)).unwrap_or(f32::MAX)
}

#[cfg(test)]
mod tests {
    use crate::testkit::*;

    /// An item a client dropped and simulates goes back to the server, which stops syncing its
    /// transform (GroupNetworkContainer.RemoveOwnershipServerRpc), and stays when its old
    /// owner leaves.
    #[test]
    fn dropped_items_go_back_to_the_server() {
        let Some((mut g, _)) = world() else { return };
        let iron = g.create_world_object(g.data.group("Iron").unwrap(), 0, true);
        g.drop_on_floor(iron, [561.0, 3.0, 497.0], [0.0, 0.0, 0.0, 1.0], 0.0, false, false, 0, 0.0, false, 1);
        let net = g.wos[&iron].net.unwrap();
        assert_eq!(g.net.objects[&net].owner, 1);
        send_on(&mut g, 1, net, "GroupNetworkContainer.RemoveOwnershipServerRpc", &[]);
        assert_eq!(g.net.objects[&net].owner, 0);
        assert_eq!(g.net.objects[&net].flags & super::DONT_DESTROY_WITH_OWNER, 0);
        assert_eq!(var(&g, net, "GroupNetworkContainer", "_transformSync"), crate::net::enc::bool(false));
        g.disconnect(1);
        assert!(g.net.objects.contains_key(&net));
    }

    #[test]
    fn loading_players_do_not_count() {
        let Some(mut g) = crate::game::test_game(crate::game::tests::save()) else { return };
        crate::game::tests::join(&mut g, 1, "Ann");
        let loading = crate::game::tests::approve(&mut g, 2);
        g.net.move_object(loading, [10.0, 0.0, 10.0], None);
        assert_eq!(super::nearest(&g, [10.0, 0.0, 10.0]).0, 1, "client 2 stands there but is still loading");
    }
}
