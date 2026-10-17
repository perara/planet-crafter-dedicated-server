//! SpaceCraft.PlayerMainController.PlayerReadyServerRpc, PlayersDataManager and the saved
//! player states (SavedDataHandler): who a joining player is, what they get, and what the save
//! keeps when they leave.

use serde_json::{json, Value};

use crate::game::{Args, Game, PlayerState, START_EQUIPMENT_SIZE, START_INVENTORY_SIZE};
use crate::model::{floats_string, parse_floats};
use crate::net::{Target, VarValue};

/// SteamID64 values start here; ids below are direct-IP players known by name.
pub const FIRST_STEAM_ID: u64 = 0x0110_0001_0000_0000;
/// Ids from name_id start here; below are the connection numbers older saves used.
const NAME_IDS: u64 = 0x00F0_0000_0000_0000;
const PLAYERS: &str = "PlayersDataManager";

/// The plugin server's NameId: a stable id for a player known only by name.
pub fn name_id(name: &str) -> u64 {
    let hash = name.bytes().fold(14695981039346656037u64, |h, b| (h ^ b as u64).wrapping_mul(1099511628211));
    0x00F0_0000_0000_0000 | (hash & 0x0000_FFFF_FFFF_FFFF)
}

/// SpaceCraft.PlayerData copied as memory (56 bytes): ulong id, FixedString32Bytes name
/// (u16 length + 30 bytes), short lightTier, bool lightActivated, short backpackTier,
/// short jetpackTier, short skinIndex.
pub fn player_data(client: u64, name: &str, light: (bool, i16), backpack: i16, jetpack: i16, skin: i16) -> Vec<u8> {
    let mut name_bytes = name.as_bytes().to_vec();
    name_bytes.truncate(29);
    let mut out = Vec::with_capacity(56);
    out.extend(client.to_le_bytes());
    out.extend((name_bytes.len() as u16).to_le_bytes());
    out.extend(&name_bytes);
    out.resize(8 + 32, 0);
    out.extend(light.1.to_le_bytes());
    out.push(light.0 as u8);
    out.push(0);
    out.extend(backpack.to_le_bytes());
    out.extend(jetpack.to_le_bytes());
    out.extend(skin.to_le_bytes());
    out.resize(56, 0);
    out
}

impl Game {
    /// PlayerMainController._joiningPlayersInventory, one entry per item, in the game's order.
    fn joining_kit(&self) -> Vec<usize> {
        let dumped = self.data.singletons["PlayerMainController"]["_joiningPlayersInventory"].as_array().cloned();
        let pairs: Vec<(String, i64)> = match dumped {
            Some(pairs) => pairs.iter().filter_map(|p| Some((self.data.assets.get(p[0].as_str()?)?["id"].as_str()?.to_string(), p[1].as_i64()?))).collect(),
            // As the 2.103 server gave it (before the data dump carried dictionaries).
            None => vec![("astrofood".into(), 3), ("WaterBottle1".into(), 2), ("OxygenCapsule1".into(), 2)],
        };
        pairs.iter().filter_map(|(id, n)| Some((self.data.group(id)?, *n))).flat_map(|(g, n)| std::iter::repeat_n(g, n.max(0) as usize)).collect()
    }

    /// Who a joining player is. Over Steam networking the connection carries the verified
    /// SteamID; over direct IP the reported one is only believed from this machine, else the
    /// name decides.
    fn player_id(&self, client: u64, claimed: u64, name: &str) -> Option<u64> {
        let c = self.clients.get(&client)?;
        Some(match c.verified_steam_id {
            Some(steam_id) => steam_id,
            None if c.trusted && claimed >= FIRST_STEAM_ID => claimed,
            None => name_id(name),
        })
    }

    /// PlayerReadyServerRpc: finds the saved player (or makes a new one) and sets them up.
    fn player_ready(&mut self, client: u64, claimed: u64, name: String) {
        let Some(id) = self.player_id(client, claimed, &name) else { return };
        let found = self.saved_players.iter().position(|p| p["id"].as_u64() == Some(id)).or_else(|| {
            // Older saves keyed direct-IP players by connection number (small ids, below the
            // name-based ones): take a matching name.
            self.saved_players.iter().position(|p| p["id"].as_u64().is_some_and(|i| i != 0 && i < NAME_IDS) && p["name"].as_str() == Some(&name))
        });
        let state = match found {
            Some(i) => {
                let p = self.saved_players.remove(i);
                println!("client {client}: {name} ({id}) ready, saved player");
                saved_state(&p, id, &name)
            }
            None => {
                println!("client {client}: {name} ({id}) ready, new player");
                let Some((position, rotation)) = self.new_player_spawn() else {
                    eprintln!("no starting pod: cannot place a new player");
                    return;
                };
                let planet = self.planet_id().to_string();
                let inventory = self.create_inventory(START_INVENTORY_SIZE, 0, 0, &[], &[]);
                let equipment = self.create_inventory(START_EQUIPMENT_SIZE, 0, 0, &[], &[]);
                // _joiningPlayersInventory: a hosted game gives everyone who joins a starter kit.
                for group in self.joining_kit() {
                    self.add_new_item(group, inventory, 0);
                }
                PlayerState {
                    id,
                    name: name.clone(),
                    inventory,
                    equipment,
                    position,
                    rotation,
                    planet,
                    total_crafted: 0,
                    total_tokens: 0,
                    camera_view: 0,
                    gauges: [100.0, 100.0, 100.0, 0.0],
                }
            }
        };
        self.setup_player(client, state);
    }

    /// Where PlayerReadyServerRpc puts a new player: the host's respawn position on the planet
    /// the session is on (PlayerStatus.GetRespawnPosition). A dedicated server has no host, so
    /// no place the host last breathed in: the first livable place stands for it
    /// (GetFirstLivablePosition, the game's own fallback; the escape pod and the arrival pod are
    /// livable). Without one, the starting pod (the game's branch without a host player).
    fn new_player_spawn(&self) -> Option<([f32; 3], [f32; 4])> {
        let livable = crate::instances::first_livable_position(self);
        if livable != [0.0; 3] {
            return Some((livable, [0.0, 0.0, 0.0, 1.0]));
        }
        let pod = crate::scene::starting_pod_group(self).and_then(|g| self.first_world_object_of_group(g, 0))?;
        let pod = &self.wos[&pod];
        Some((pod.position, crate::geometry::from_euler_y(crate::geometry::euler_y(pod.rotation))))
    }

    fn setup_player(&mut self, client: u64, mut state: PlayerState) {
        let Some(object) = self.player_object(client) else { return };
        if state.planet.is_empty() {
            state.planet = self.save.settings["planetId"].as_str().unwrap_or("Prime").to_string();
        }
        if state.planet != self.planet_id() {
            // Saved on another planet: PlayerStatus.GetRespawnPosition (the first livable
            // building), else where the player spawned (where new players start here).
            let livable = crate::instances::first_livable_position(self);
            if livable != [0.0; 3] {
                state.position = livable;
            } else if let Some(pod) = crate::scene::starting_pod_group(self).and_then(|g| self.first_world_object_of_group(g, self.planet_hash())) {
                state.position = self.wos[&pod].position;
            }
        }
        let me = Target::Only(client);
        self.net.move_object(object, state.position, Some(state.rotation));
        self.net.rpc(me, object, "PlayerMainController.InitPlayerClientRpc", &[json!(state.inventory), json!(state.equipment), json!(state.position), json!(state.rotation), json!(state.camera_view)]);
        crate::gauges::start(self, client, object, state.gauges, state.equipment);
        let name = state.name.clone();
        self.net.rpc_scene(Target::Except(client), "ChatHandler.ReceiveServicePlayerJoinedMessageClientRpc", &[json!(name)]);
        self.all_time_tokens.insert(client, state.total_tokens);
        self.net.rpc_scene(me, "TokensHandler.SetAllTimeTokensNumberClientRpc", &[json!(state.total_tokens)]);
        self.total_crafted.insert(client, state.total_crafted);
        self.net.rpc_scene(me, "WorldObjectsHandler.SetTotalCraftedObjectsClientRpc", &[json!(state.total_crafted)]);
        // PlayersManager.AddPlayerController -> PlayersDataManager adds the player to the list.
        let (backpack, jetpack, skin) = crate::equipment::tiers(self, state.equipment);
        let item = player_data(client, &name, (false, 0), backpack, jetpack, skin);
        if let Some((list, _)) = self.net.find_class(PLAYERS) {
            self.net.list_events(list, PLAYERS, "_playersData", vec![crate::net::ListEvent::Add(item)]);
        }
        if let Some(c) = self.clients.get_mut(&client) {
            c.ready = Some(state);
        }
        crate::scene::player_count_changed(self);
    }

    /// The player's object went away: their state goes back to the save list, the others see
    /// them leave.
    pub fn player_left(&mut self, c: &crate::game::Client) {
        if let Some(state) = &c.ready {
            let position = c.player.and_then(|o| self.net.objects.get(&o)).map(|o| o.position).unwrap_or(state.position);
            let gauges = c.player.map(|o| crate::gauges::values(self, o)).unwrap_or(state.gauges);
            let mut state = state.clone();
            if let Some(o) = c.player.and_then(|o| self.net.objects.get(&o)) {
                state.rotation = o.rotation;
            }
            let entry = self.state_json(c.id, &state, position, gauges);
            self.saved_players.retain(|p| p["id"].as_u64() != Some(state.id));
            self.saved_players.push(entry);
            self.net.rpc_scene(Target::Except(c.id), "ChatHandler.ReceiveServicePlayerLeftMessageClientRpc", &[json!(state.name)]);
        }
        if let Some(object) = c.player {
            self.net.despawn(object);
        }
        if let Some((list, _)) = self.net.find_class(PLAYERS) {
            if let Some(VarValue::List(items)) = self.net.get(list, PLAYERS, "_playersData") {
                if let Some(i) = items.iter().position(|i| u64::from_le_bytes(i[0..8].try_into().unwrap()) == c.id) {
                    self.net.list_events(list, PLAYERS, "_playersData", vec![crate::net::ListEvent::RemoveAt(i as i32)]);
                }
            }
        }
        // Objects the leaving client owned (dropped items still falling) go back to the server.
        let owned: Vec<u64> = self.net.objects.values().filter(|o| o.owner == c.id).map(|o| o.id).collect();
        for object in owned {
            self.net.change_owner(object, 0);
        }
        self.total_crafted.remove(&c.id);
        self.all_time_tokens.remove(&c.id);
        crate::scene::player_count_changed(self);
    }

    /// JsonablePlayerState for a player in the game.
    fn state_json(&self, client: u64, state: &PlayerState, position: [f32; 3], gauges: [f32; 4]) -> Value {
        json!({
            "id": state.id,
            "name": state.name,
            "inventoryId": state.inventory,
            "equipmentId": state.equipment,
            "playerPosition": floats_string(&position),
            "playerRotation": floats_string(&state.rotation),
            "playerGaugeOxygen": crate::model::JsonFloat(gauges[0]),
            "playerGaugeThirst": crate::model::JsonFloat(gauges[1]),
            "playerGaugeHealth": crate::model::JsonFloat(gauges[2]),
            "playerGaugeToxic": crate::model::JsonFloat(gauges[3]),
            "host": false,
            "planetId": self.planet_id(),
            "totalCraftedObjects": self.total_crafted.get(&client).copied().unwrap_or(state.total_crafted),
            "totalTerraTokenEarned": self.all_time_tokens.get(&client).copied().unwrap_or(state.total_tokens),
            "cameraView": state.camera_view,
        })
    }

    /// The PlayersDataManager and PlayerMainController RPCs; false when not one of them.
    pub fn players_rpc(&mut self, client: u64, name: &str, a: &Args) -> bool {
        let edit = |game: &mut Game, f: &dyn Fn(&mut Vec<u8>)| {
            let Some((list, _)) = game.net.find_class(PLAYERS) else { return };
            let Some(VarValue::List(items)) = game.net.get(list, PLAYERS, "_playersData") else { return };
            if let Some(i) = items.iter().position(|i| u64::from_le_bytes(i[0..8].try_into().unwrap()) == client) {
                let mut item = items[i].clone();
                f(&mut item);
                game.net.list_events(list, PLAYERS, "_playersData", vec![crate::net::ListEvent::Value(i as i32, item)]);
            }
        };
        match name {
            "PlayerMainController.PlayerReadyServerRpc" => {
                let claimed = a.u(0);
                // Printable, and short enough for logs and the player list.
                let player_name: String = a.s(1).unwrap_or_default().chars().filter(|c| !c.is_control()).take(64).collect();
                let Some(c) = self.clients.get(&client) else { return true };
                // Once per connection: another PlayerReady would take another saved character.
                if c.ready.is_some() {
                    return true;
                }
                let verified = c.verified_steam_id;
                if !self.access.name_allowed(&player_name, verified) {
                    println!("client {client}: refused ({player_name:?})");
                    self.kick(client, crate::access::REFUSED);
                    return true;
                }
                // One session per player: a second one would take the same character (or one
                // by the same name). A reconnecting player gets in once the old session is gone.
                let id = self.player_id(client, claimed, &player_name);
                let online = self.clients.values().filter(|o| o.id != client).filter_map(|o| o.ready.as_ref()).any(|s| Some(s.id) == id || s.name.eq_ignore_ascii_case(&player_name));
                if online {
                    println!("client {client}: refused ({player_name:?} is already online)");
                    self.kick(client, "Already connected");
                    return true;
                }
                self.player_ready(client, claimed, player_name);
            }
            "PlayersDataManager.SetLightStatusServerRpc" => {
                let (on, tier) = (a.b(0), a.i(1) as i16);
                edit(self, &|item| {
                    item[40..42].copy_from_slice(&tier.to_le_bytes());
                    item[42] = on as u8;
                });
            }
            "PlayersDataManager.SetBackpackStatusServerRpc" => {
                let tier = if a.b(0) { a.i(1) as i16 } else { 0 };
                edit(self, &|item| item[44..46].copy_from_slice(&tier.to_le_bytes()));
            }
            "PlayersDataManager.SetJetpackStatusServerRpc" => {
                let tier = if a.b(0) { a.i(1) as i16 } else { 0 };
                edit(self, &|item| item[46..48].copy_from_slice(&tier.to_le_bytes()));
            }
            "PlayersDataManager.SetSkinStatusServerRpc" => {
                let skin = a.i(0) as i16;
                edit(self, &|item| item[48..50].copy_from_slice(&skin.to_le_bytes()));
            }
            "PlayersDataManager.GetPlayersPositionsServerRpc" => {
                let (mut ids, mut positions) = (Vec::new(), Vec::new());
                for c in self.clients.values().filter(|c| c.id != client && c.ready.is_some()) {
                    if let Some(o) = c.player.and_then(|o| self.net.objects.get(&o)) {
                        ids.push(c.id);
                        positions.push(if crate::instances::is_inside_an_instance(self, o.position) { [0.0; 3] } else { o.position });
                    }
                }
                if !ids.is_empty() {
                    self.net.rpc_scene(Target::Only(client), "PlayersDataManager.UpdateMapMarkerPositionsClientRpc", &[json!(ids), json!(positions)]);
                }
            }
            _ => return false,
        }
        true
    }
}

fn saved_state(p: &Value, id: u64, name: &str) -> PlayerState {
    let f = |k: &str| p[k].as_f64().unwrap_or(0.0) as f32;
    PlayerState {
        id,
        name: name.to_string(),
        inventory: p["inventoryId"].as_i64().unwrap_or(1) as i32,
        equipment: p["equipmentId"].as_i64().unwrap_or(2) as i32,
        position: parse_floats(p["playerPosition"].as_str().unwrap_or("")).unwrap_or([0.0; 3]),
        rotation: parse_floats(p["playerRotation"].as_str().unwrap_or("")).unwrap_or([0.0, 0.0, 0.0, 1.0]),
        planet: p["planetId"].as_str().unwrap_or("").to_string(),
        total_crafted: p["totalCraftedObjects"].as_u64().unwrap_or(0) as u32,
        total_tokens: p["totalTerraTokenEarned"].as_u64().unwrap_or(0),
        camera_view: p["cameraView"].as_i64().unwrap_or(0) as i32,
        gauges: [f("playerGaugeOxygen"), f("playerGaugeThirst"), f("playerGaugeHealth"), f("playerGaugeToxic")],
    }
}

/// SetAndGetPlayerStateValues: everyone in the game is written with their current state.
pub fn update_saved_players(game: &mut Game) {
    let entries: Vec<(u64, Value)> = game
        .clients
        .values()
        .filter_map(|c| {
            let state = c.ready.as_ref()?;
            let position = c.player.and_then(|o| game.net.objects.get(&o)).map(|o| o.position).unwrap_or(state.position);
            let gauges = c.player.map(|o| crate::gauges::values(game, o)).unwrap_or(state.gauges);
            let mut state = state.clone();
            if let Some(o) = c.player.and_then(|o| game.net.objects.get(&o)) {
                state.rotation = o.rotation;
            }
            Some((state.id, game.state_json(c.id, &state, position, gauges)))
        })
        .collect();
    for (id, entry) in entries {
        game.saved_players.retain(|p| p["id"].as_u64() != Some(id));
        game.saved_players.push(entry);
    }
}

/// The saved player a save moved back into the game treats as its host: SavedDataHandler.
/// InitSavedData loads the host's planet (the starting planet without one). The server keeps its
/// planet in pcdsCurrentPlanet, which the game does not read, so the player saved last on that
/// planet is marked host; none when nobody is saved there. The cost: whoever hosts that save
/// without a character of their own takes the marked player's (PlayerReadyServerRpc gives a
/// host the host entry); hosts with their own character keep theirs.
pub fn mark_host(players: &mut [Value], planet: &str) {
    // A Steam player (the game's host always is one), else anyone.
    let on_planet = |p: &Value| p["planetId"].as_str() == Some(planet);
    let host = players.iter().rposition(|p| on_planet(p) && p["id"].as_u64().is_some_and(|i| i >= FIRST_STEAM_ID)).or_else(|| players.iter().rposition(on_planet));
    for (i, p) in players.iter_mut().enumerate() {
        p["host"] = json!(Some(i) == host);
    }
}

/// Inventory changes that matter to players: equipment changes what the player can carry.
pub fn inventory_changed(game: &mut Game, inventory: i32, wo: i32, added: bool) {
    crate::equipment::inventory_changed(game, inventory, wo, added);
}

#[cfg(test)]
mod tests {
    use crate::data::stable_hash;
    use crate::game::tests::join;

    #[test]
    fn the_last_player_saved_on_the_current_planet_is_the_host() {
        let mut save = crate::game::tests::save();
        save.players = vec![serde_json::json!({"id": 5, "name": "Old", "host": true, "planetId": "Selenea", "totalCraftedObjects": 0, "totalTerraTokenEarned": 0})];
        save.stats["craftedObjects"] = serde_json::json!(42);
        let Some(mut g) = crate::game::test_game(save) else { return };
        for (client, name) in [(1, "Ann"), (2, "Bob")] {
            join(&mut g, client, name);
            g.disconnect(client);
        }
        let saved = g.snapshot();
        let hosts: Vec<&str> = saved.players.iter().filter(|p| p["host"] == true).filter_map(|p| p["name"].as_str()).collect();
        assert_eq!(hosts, vec!["Bob"], "the game would load Bob's planet, Prime");
        // Loaded back by this server, the host it marked does not take the world's craft count.
        let Some(g) = crate::game::test_game(saved) else { return };
        let bob = g.saved_players.iter().find(|p| p["name"] == "Bob").unwrap();
        assert_eq!(bob["totalCraftedObjects"], 0);
    }

    #[test]
    fn new_players_spawn_where_the_host_would_respawn() {
        let Some(mut g) = crate::planets::tests::game() else { return };
        // The escape pod is the first livable place: its pivot, one meter up.
        let ann = join(&mut g, 1, "Ann");
        assert_eq!(g.net.objects[&ann].position, [558.2, 3.47, 494.1]);
        // Travelled to Selenea, nothing built there yet: the arrival pod (livable too).
        crate::planets::switch_to_planet(&mut g, stable_hash("Selenea"), 0, 0);
        let bob = join(&mut g, 2, "Bob");
        assert_eq!(g.net.objects[&bob].position, [-415.0, 35.65, -82.0]);
        assert_eq!(g.clients[&2].ready.as_ref().unwrap().planet, "Selenea");
    }
}
