//! The scene's handlers (in-scene network objects with ids 2-21) and their shared state: the
//! values they start with from the save, what they hand back to the save, and the timers the
//! game runs with Invoke and coroutines.

use serde_json::{json, Value};

use crate::data::stable_hash;
use crate::game::Game;
use crate::net::{enc, Target, VarValue};

#[derive(serde::Serialize, serde::Deserialize)]
pub enum Timer {
    /// GroupNetworkContainer.DespawnAfterDelay: (world object, its network object). An Invoke on
    /// the GameObject: picking the item up despawns it, and the timer goes with it.
    Despawn(i32, u64),
    /// GroupNetworkBase.FinalDeconstructServer: (world object, deconstructing client).
    FinalDeconstruct(i32, u64),
}

/// Pending timers: (game time when due, what).
#[derive(Default, serde::Serialize, serde::Deserialize)]
pub struct Timers(pub Vec<(f32, Timer)>);

/// Rules: a dropped world object's despawn moves to `delay` seconds from now (0: never).
pub fn set_despawn(game: &mut Game, wo: i32, delay: f32) {
    game.timers.0.retain(|(_, t)| !matches!(t, Timer::Despawn(w, _) if *w == wo));
    if let Some(net) = game.wos.get(&wo).and_then(|w| w.net).filter(|_| delay > f32::EPSILON) {
        schedule(game, delay, Timer::Despawn(wo, net));
    }
}

pub fn schedule(game: &mut Game, delay: f32, timer: Timer) {
    let due = game.time + delay;
    // One despawn per world object: dropping it again replaces the timer.
    if let Timer::Despawn(wo, _) = timer {
        game.timers.0.retain(|(_, t)| !matches!(t, Timer::Despawn(w, _) if *w == wo));
    }
    game.timers.0.push((due, timer));
}

fn list_i32(items: impl Iterator<Item = i32>) -> Vec<Vec<u8>> {
    items.map(enc::int).collect()
}

/// Planet indices players may travel to: the free ones (DLC planets need an owner, which a
/// standalone server cannot check).
pub fn available_planets(game: &Game) -> Vec<usize> {
    game.net_data.planets.iter().enumerate().filter(|(_, p)| p.steam_app_id == 0).map(|(i, _)| i).collect()
}

pub fn init(game: &mut Game) {
    // SavedDataHandler.LoadMessagesData: messages the game data no longer has are dropped (a
    // table without any messages drops none).
    let known: std::collections::HashSet<&str> = game.data.assets.iter().filter(|(k, _)| k.starts_with("MessageData:")).filter_map(|(_, m)| m["stringId"].as_str()).collect();
    if !known.is_empty() {
        game.save.messages.retain(|m| m["stringId"].as_str().is_some_and(|id| known.contains(id)));
    }
    let save = &game.save;
    let index = |id: &str| game.net_data.planet_index(id).map(|i| i as i16);
    let starting = index(save.settings["planetId"].as_str().unwrap_or("Prime")).unwrap_or(0);
    let current = game.planet as i16;
    let available: Vec<Vec<u8>> = available_planets(game).into_iter().map(|i| enc::short(i as i16)).collect();
    let story = list_i32(save.story_events.iter().filter_map(|e| e["stringId"].as_str()).map(stable_hash));
    let messages: Vec<Vec<u8>> = save
        .messages
        .iter()
        .filter_map(|m| Some([vec![m["isRead"].as_bool().unwrap_or(false) as u8, 0, 0, 0], stable_hash(m["stringId"].as_str()?).to_le_bytes().to_vec()].concat()))
        .collect();
    let unlocked = list_i32(crate::model::groups_of(save.world["unlockedGroups"].as_str().unwrap_or(""), &game.data).iter().map(|g| game.data.groups[*g].hash));
    let tokens = enc::ulong(save.world["terraTokens"].as_u64().unwrap_or(0));
    let mut settings = save.settings.clone();
    settings["saveDisplayName"] = Value::String(String::new());
    let settings_json = serde_json::to_string(&settings).unwrap_or_default();
    let mut source = (settings_json.len() as i32).to_le_bytes().to_vec();
    source.extend(settings_json.as_bytes());

    let set = |game: &mut Game, class: &str, var: &str, value: VarValue| {
        let Some((object, _)) = game.net.find_class(class) else { return };
        let o = game.net.objects.get_mut(&object).unwrap();
        match value {
            VarValue::Plain(bytes) => o.init(class, var, bytes),
            VarValue::List(items) => o.init_list(class, var, items),
        }
    };
    set(game, "PlanetNetworkLoader", "_startingPlanetIndex", VarValue::Plain(enc::short(starting)));
    set(game, "PlanetNetworkLoader", "_planetIndex", VarValue::Plain(enc::short(current)));
    set(game, "PlanetNetworkLoader", "_availablePlanets", VarValue::List(available));
    set(game, "StoryEventsHandler", "_triggeredStoryEvents", VarValue::List(story));
    set(game, "MessagesHandler", "_receivedMessages", VarValue::List(messages));
    set(game, "UnlockedGroupsHandler", "_unlockedGroups", VarValue::List(unlocked));
    set(game, "TokensHandler", "_tokensNumber", VarValue::Plain(tokens));
    set(game, "GameSettingsHandler", "_source", VarValue::Plain(source));
    set(game, "GameSettingsHandler", "_numPlayers", VarValue::Plain(enc::int(0)));
    set(game, "PlayersDataManager", "_playersData", VarValue::List(Vec::new()));
    for list in ["_allWorldObjectsGroups", "_constructedWorldObjectsGroups", "_constructedPanelTypes"] {
        set(game, "WorldObjectsHandler", list, VarValue::List(Vec::new()));
    }
    crate::units::init(game);
    crate::meteo::init(game);
    crate::logistics::init(game);
}

/// PlanetUnlocker.OnClientConnectCallback: every unlocked (saved) planet.
pub fn on_client_approved(game: &mut Game, client: u64) {
    crate::machines::client_approved(game, client);
    let planets: Vec<i32> = game.save.planets.iter().filter_map(|p| p["planetId"].as_str()).map(stable_hash).collect();
    for hash in planets {
        game.net.rpc_scene(Target::Only(client), "PlanetUnlocker.UnlockPlanetClientRpc", &[json!(hash)]);
    }
}

/// GameSettingsHandler._numPlayers follows the player list.
pub fn player_count_changed(game: &mut Game) {
    let count = game.clients.values().filter(|c| c.ready.is_some()).count() as i32;
    game.set_on("GameSettingsHandler", "_numPlayers", enc::int(count));
}

pub fn tick(game: &mut Game) {
    let now = game.time;
    let due: Vec<Timer> = {
        let (ready, waiting): (Vec<_>, Vec<_>) = std::mem::take(&mut game.timers.0).into_iter().partition(|(t, _)| *t <= now);
        game.timers.0 = waiting;
        ready.into_iter().map(|(_, t)| t).collect()
    };
    for timer in due {
        match timer {
            Timer::Despawn(wo, net) => {
                if game.wos.get(&wo).is_some_and(|w| w.net == Some(net)) {
                    game.destroy_world_object(wo, true);
                }
            }
            Timer::FinalDeconstruct(wo, client) => crate::deconstruct::finish(game, wo, client),
        }
    }
    crate::gauges::tick(game);
    crate::units::tick(game);
    crate::energy::tick(game);
    crate::hooks::tick(game);
}

/// The parts of the save these handlers own.
pub fn fill_save(game: &Game, save: &mut crate::save::SaveFile) {
    let list = |class: &str, var: &str| match game.net.find_class(class).and_then(|(o, _)| game.net.get(o, class, var)) {
        Some(VarValue::List(items)) => items.clone(),
        _ => Vec::new(),
    };
    let unlocked: Vec<String> = list("UnlockedGroupsHandler", "_unlockedGroups")
        .iter()
        .filter_map(|b| crate::wire::Reader::new(b).packed_i32())
        .filter_map(|h| game.data.group_by_hash(h))
        .map(|g| game.data.groups[g].id.clone())
        .collect();
    save.world["unlockedGroups"] = json!(unlocked.join(","));
    if let Some(VarValue::Plain(b)) = game.net.find_class("TokensHandler").and_then(|(o, _)| game.net.get(o, "TokensHandler", "_tokensNumber")) {
        save.world["terraTokens"] = json!(crate::wire::Reader::new(b).packed_u64().unwrap_or(0));
    }
    save.planets = crate::units::planet_states(game);
    save.stats["craftedObjects"] = json!(game.crafted_total);
}

pub fn starting_pod_group(game: &Game) -> Option<usize> {
    pod_group(&game.data, game.save.settings["planetId"].as_str().unwrap_or("Prime"))
}

/// GroupsHandler.GetStartingPodGroup: EscapePod<planet>, else EscapePod.
pub fn pod_group(data: &crate::gamedata::GameData, planet: &str) -> Option<usize> {
    data.group(&format!("EscapePod{planet}")).or_else(|| data.group("EscapePod"))
}

pub fn on_world_object_from_scene_destroyed(game: &mut Game, wo: i32) {
    crate::machines::scene_object_removed(game, wo);
}

pub fn on_world_object_from_scene_retrieved(game: &mut Game, wo: i32) {
    crate::machines::scene_object_removed(game, wo);
}

