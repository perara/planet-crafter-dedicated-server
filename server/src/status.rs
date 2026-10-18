//! SpaceCraft.PlayerStatus and DyingConsequencesHandler, server side: a player whose gauge hits
//! zero is dying for GameConfig.dyingTime seconds; their client then respawns them and asks the
//! server for the consequences (items dropped into a canister, or lost).

use std::collections::HashMap;

use serde_json::json;

use crate::game::Game;
use crate::net::{enc, Target, VarValue};

const CLASS: &str = "PlayerStatus";
const DYING_TIME: f32 = 9.0;
const LOSE_CHANCE: f32 = 50.0;

/// When each dying player started dying.
#[derive(Default, serde::Serialize, serde::Deserialize)]
pub struct Status(pub HashMap<u64, f32>);

fn var(game: &Game, object: u64, name: &str) -> bool {
    matches!(game.net.get(object, CLASS, name), Some(VarValue::Plain(b)) if b.first() == Some(&1))
}

/// PlayerStatus.Update for one player (run with the gauges).
pub fn check_dying(game: &mut Game, _client: u64, object: u64) {
    let g = crate::gauges::values(game, object);
    let dying_gauges = g[0] < f32::EPSILON || g[1] < f32::EPSILON || g[2] < f32::EPSILON;
    let is_dying = var(game, object, "_isDying");
    if var(game, object, "_ready") && !is_dying && dying_gauges {
        game.net.set(object, CLASS, "_instantDeath", enc::bool(false));
        game.net.set(object, CLASS, "_isDying", enc::bool(true));
        game.ext.status.0.insert(object, game.time);
        return;
    }
    if is_dying {
        let start = game.ext.status.0.get(&object).copied().unwrap_or(game.time);
        if game.time >= start + DYING_TIME || !dying_gauges {
            game.net.set(object, CLASS, "_isDying", enc::bool(false));
            game.ext.status.0.remove(&object);
        }
    }
}

/// HandleDyingConsequencesServerRpc.
pub fn handle_dying_consequences(game: &mut Game, client: u64) {
    let Some(state) = game.clients.get(&client).and_then(|c| c.ready.clone()) else { return };
    let Some(object) = game.player_object(client) else { return };
    let position = game.net.objects.get(&object).map(|o| o.position).unwrap_or([0.0; 3]);
    let consequence = match &game.rules.death.consequences {
        Some(c) => c.clone(),
        None => game.save.settings["dyingConsequencesLabel"].as_str().unwrap_or("DropSomeItems").to_string(),
    };
    let status = if consequence == "NoConsequences" {
        0
    } else if crate::instances::is_inside_an_instance(game, position) {
        crate::instances::drop_generated_objects(game, &state, position);
        0
    } else {
        match consequence.as_str() {
            "DropSomeItems" => drop_some(game, &state, position),
            "DropAllItems" => {
                let items = game.inventories.get(&state.inventory).map(|i| i.items.clone()).unwrap_or_default();
                for wo in items.iter().rev() {
                    if !game.wos.get(wo).is_some_and(|w| game.data.groups[w.group].bool("cantBeDestroyed")) && game.inventory_remove(state.inventory, *wo) {
                        game.destroy_world_object(*wo, false);
                        game.propagate(state.inventory, *wo, false);
                        game.dirty_inventory(state.inventory, 0, false);
                    }
                }
                2
            }
            "DeleteSaveFile" => {
                // DisconnectClient, then RemovePlayerStateData: neither leaving nor a save before
                // the drop may write them back, so they stop being ready (and are said to leave now).
                if let Some(c) = game.clients.get_mut(&client) {
                    c.ready = None;
                }
                game.net.rpc_scene(Target::Except(client), "ChatHandler.ReceiveServicePlayerLeftMessageClientRpc", &[json!(state.name)]);
                game.saved_players.retain(|p| p["id"].as_u64() != Some(state.id));
                game.kick(client, "Your character is gone (dying deletes the save)");
                3
            }
            _ => 0,
        }
    };
    if status != 0 {
        game.net.rpc(Target::Only(client), object, "PlayerStatus.HandleDyingConsequencesNotificationClientRpc", &[json!(status)]);
    }
}

fn drop_some(game: &mut Game, state: &crate::game::PlayerState, position: [f32; 3]) -> i32 {
    let items = game.inventories.get(&state.inventory).map(|i| i.items.clone()).unwrap_or_default();
    if items.is_empty() {
        return 0;
    }
    let chance = if items.len() <= 4 { 100.0 } else { game.rules.death.drop_chance.unwrap_or(LOSE_CHANCE) };
    let mut dropped = Vec::new();
    for wo in items.iter().rev() {
        if chance > game.rng.range(0, 100) as f32 && dropped.len() < 30 && !game.wos.get(wo).is_some_and(|w| game.data.groups[w.group].bool("cantBeDestroyed")) {
            dropped.push(*wo);
        }
    }
    if dropped.is_empty() {
        return 0;
    }
    let Some(canister) = game.data.singletons["PlayerStatus"]["canisterGroup"].as_str().and_then(|r| game.data.group_of_ref(r)) else { return 0 };
    // The game raycasts to the ground under the player; the player stands on it.
    let at = [position[0], position[1] - 0.1, position[2]];
    let yaw = game.rng.rangef(0.0, 360.0);
    let id = game.new_world_object_id();
    game.create_world_object(canister, id, true);
    let planet = game.planet_hash();
    let w = game.wos.get_mut(&id).unwrap();
    w.position = at;
    w.rotation = crate::geometry::from_euler_y(yaw);
    w.planet = planet;
    game.instantiate(id, false, 0);
    let inv = game.wos[&id].linked_inventory;
    game.ext.destroy_when_empty.insert(inv, id);
    if let Some(i) = game.inventories.get_mut(&inv) {
        i.size += 15;
    }
    for wo in dropped {
        game.transfer_one(state.inventory, inv, wo, 0);
    }
    1
}
