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
/// The saved player's field with the Unix time they were last in the game (players.forgetAfterDays).
const LAST_SEEN: &str = "pcdsLastSeen";

/// The plugin server's NameId: a stable id for a player known only by name.
pub fn name_id(name: &str) -> u64 {
    let hash = name.bytes().fold(14695981039346656037u64, |h, b| (h ^ b as u64).wrapping_mul(1099511628211));
    0x00F0_0000_0000_0000 | (hash & 0x0000_FFFF_FFFF_FFFF)
}

/// SpaceCraft.PlayerData copied as memory (56 bytes): ulong id, FixedString32Bytes name
/// (u16 length + 30 bytes), short lightTier, bool lightActivated, short backpackTier,
/// short jetpackTier, short skinIndex.
pub fn player_data(client: u64, name: &str, light: (bool, i16), backpack: i16, jetpack: i16, skin: i16) -> Vec<u8> {
    // The 29 bytes the FixedString32Bytes holds, cut between characters: half of one is not UTF-8,
    // and every client reads this list.
    let mut end = name.len().min(29);
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    let name_bytes = name.as_bytes()[..end].to_vec();
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
        let dumped = self.data.singletons["PlayerMainController"]["_joiningPlayersInventory"].as_array();
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
                info!("client" = client, "name" = name, "id" = id; "client {client}: {name} ({id}) ready, saved player");
                saved_state(&p, id, &name)
            }
            None => {
                info!("client" = client, "name" = name, "id" = id; "client {client}: {name} ({id}) ready, new player");
                let Some((position, rotation)) = self.new_player_spawn() else {
                    error!("no starting pod: cannot place a new player");
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
            state.planet = self.started_planet().to_string();
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
        // A saved backpack or equipment that is gone (an older server let clients destroy
        // them) comes back empty, rather than the player pointing at a missing inventory.
        if !self.inventories.contains_key(&state.inventory) {
            state.inventory = self.create_inventory(START_INVENTORY_SIZE, state.inventory, 0, &[], &[]);
        }
        if !self.inventories.contains_key(&state.equipment) {
            state.equipment = self.create_inventory(START_EQUIPMENT_SIZE, state.equipment, 0, &[], &[]);
        }
        // PlayerEquipment.InitEquipment, run by the server for everyone who joins: the equipment
        // takes items of the Equipment category only (AddAuthorizedGroup, so not _isEquipment).
        let equipment: Vec<usize> = (0..self.data.groups.len()).filter(|g| !self.data.groups[*g].constructible && self.data.groups[*g].str("itemCategory") == "Equipment").collect();
        if let Some(inv) = self.inventories.get_mut(&state.equipment) {
            for g in equipment {
                if !inv.authorized.contains(&g) {
                    inv.authorized.push(g);
                }
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
        crate::events::player_joined(self, client);
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
            crate::events::player_left(self, c.id, &state.name, state.id);
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
        crate::toxic::client_left(self, c.id);
        crate::scene::player_count_changed(self);
    }

    /// JsonablePlayerState for a player in the game.
    fn state_json(&self, client: u64, state: &PlayerState, position: [f32; 3], gauges: [f32; 4]) -> Value {
        let mut entry = json!({
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
            // UpdatePlayerStateData: a player other than the host is saved with camera view 1
            // exactly when it faces yaw 0 (the game tests the rotation, not the camera).
            "cameraView": i32::from(crate::geometry::euler_y(state.rotation).abs() < 1.1920929e-7),
        });
        // When players.forgetAfterDays is on, the time they were last in the game (the game ignores
        // the field, as it ignores pcdsCurrentPlanet). Off: the save is the game's, field for field.
        if self.rules.players.forget_after_days > 0 {
            entry[LAST_SEEN] = json!(crate::util::now_secs());
        }
        entry
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
                // Printable, and short enough for logs and the player list. The save's
                // delimiters go as JSONExport.CleanStringForInsertInJson takes them out of texts
                // ('|' and '@' to spaces), and quotes too: the game finds a player's name with
                // "name":"[^"]*?", so one with a quote in it makes the save unloadable.
                let player_name: String = a.s(1).unwrap_or_default().chars().filter(|c| !c.is_control()).take(64).map(|c| match c {
                    '|' | '@' => ' ',
                    '"' => '\'',
                    c => c,
                }).collect();
                let Some(c) = self.clients.get(&client) else { return true };
                // Once per connection: another PlayerReady would take another saved character.
                if c.ready.is_some() {
                    return true;
                }
                let verified = c.verified_steam_id;
                if !self.access.name_allowed(&player_name, verified) {
                    info!("client {client}: refused ({player_name:?})");
                    self.kick(client, crate::access::REFUSED);
                    return true;
                }
                // One session per player: a second one would take the same character (or one
                // by the same name). A reconnecting player gets in once the old session is gone.
                let id = self.player_id(client, claimed, &player_name);
                let online = self.clients.values().filter(|o| o.id != client).filter_map(|o| o.ready.as_ref()).any(|s| Some(s.id) == id || crate::access::same_name(&s.name, &player_name));
                if online {
                    info!("client {client}: refused ({player_name:?} is already online)");
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

/// players.forgetAfterDays: a saved player is forgotten when all of this holds, at `now` (Unix
/// seconds):
/// - they have been away that many days (their `pcdsLastSeen`; a record without one is stamped now,
///   so the days count from the first look, never from before the rule was on);
/// - they are not in the game, and are not the host this save marks;
/// - they never crafted anything or earned a terra token;
/// - their equipment is empty and their backpack holds no more than the joining kit (the starter
///   items every new player gets, `joining_kit`), counted by item.
///
/// Their record and their two inventories with those items go; nothing else is touched. Returns how
/// many were forgotten.
pub fn forget_unseen(game: &mut Game, now: u64) -> usize {
    let limit = u64::from(game.rules.players.forget_after_days) * 86_400;
    if limit == 0 {
        return 0;
    }
    let online: Vec<u64> = game.clients.values().filter_map(|c| c.ready.as_ref().map(|s| s.id)).collect();
    for p in game.saved_players.iter_mut().filter(|p| p[LAST_SEEN].as_u64().is_none()) {
        p[LAST_SEEN] = json!(now);
    }
    let kit = game.joining_kit();
    let forget: Vec<(u64, String, i32, i32)> = game
        .saved_players
        .iter()
        .filter_map(|p| {
            let id = p["id"].as_u64().filter(|id| !online.contains(id))?;
            let away = now.saturating_sub(p[LAST_SEEN].as_u64()?);
            let made_nothing = p["totalCraftedObjects"].as_u64().unwrap_or(0) == 0 && p["totalTerraTokenEarned"].as_u64().unwrap_or(0) == 0;
            let (backpack, equipment) = (i32::try_from(p["inventoryId"].as_i64()?).ok()?, i32::try_from(p["equipmentId"].as_i64()?).ok()?);
            let holds_no_more_than_new = game.inventories.get(&equipment).is_none_or(|i| i.items.is_empty()) && game.holds_no_more_than(backpack, &kit);
            (p["host"] != true && away >= limit && made_nothing && holds_no_more_than_new).then(|| (id, p["name"].as_str().unwrap_or("").to_string(), backpack, equipment))
        })
        .collect();
    for (id, name, backpack, equipment) in &forget {
        game.saved_players.retain(|p| p["id"].as_u64() != Some(*id));
        // Their inventories are nobody's now, so destroy_inventory takes them (and the items).
        game.destroy_inventory(*backpack, false);
        game.destroy_inventory(*equipment, false);
        info!("players: forgot {name} ({id}), away for {} days with nothing past the starter items", game.rules.players.forget_after_days);
    }
    forget.len()
}

impl Game {
    /// Whether an inventory holds no more than `kit` (a group once per item), counted by group. A
    /// missing inventory holds nothing.
    fn holds_no_more_than(&self, inventory: i32, kit: &[usize]) -> bool {
        let Some(inv) = self.inventories.get(&inventory) else { return true };
        let mut left = kit.to_vec();
        inv.items.iter().all(|wo| {
            let group = self.wos.get(wo).map(|w| w.group);
            group.and_then(|g| left.iter().position(|k| *k == g)).map(|i| left.swap_remove(i)).is_some()
        })
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
    use serde_json::json;

    use super::{forget_unseen, PLAYERS};
    use crate::data::stable_hash;
    use crate::game::tests::join;
    use crate::net::Target;
    use crate::testkit::*;

    /// A name longer than the 29 bytes of the list's FixedString32Bytes is cut between characters: the
    /// name is the client's (64 characters of any script), the list is read by every client.
    #[test]
    fn a_long_name_is_cut_between_characters_in_the_players_list() {
        for name in ["é".repeat(40), "日本語".repeat(10), "x".repeat(40), "😀".repeat(30), "ab".to_string(), String::new()] {
            let item = super::player_data(1, &name, (false, 0), 0, 0, 0);
            let len = u16::from_le_bytes([item[8], item[9]]) as usize;
            assert!(len <= 29 && item.len() == 56, "{name}");
            let shown = std::str::from_utf8(&item[10..10 + len]).unwrap_or_else(|_| panic!("{name}: not UTF-8"));
            assert!(name.starts_with(shown) && (name.len() <= 29 || shown.len() > 29 - 4), "{name} {shown}");
        }
    }

    /// A player's PlayersData entry: (light tier, light on, backpack, jetpack, skin).
    fn entry(g: &crate::game::Game, client: u64) -> (i16, bool, i16, i16, i16) {
        let (object, _) = g.net.find_class(PLAYERS).unwrap();
        let item = list(g, object, PLAYERS, "_playersData").into_iter().find(|i| u64::from_le_bytes(i[0..8].try_into().unwrap()) == client).unwrap();
        let short = |at: usize| i16::from_le_bytes(item[at..at + 2].try_into().unwrap());
        (short(40), item[42] == 1, short(44), short(46), short(48))
    }

    /// The light, backpack, jetpack and skin each player shows the others, in their entry of
    /// the players list; a player's RPC changes only its own.
    #[test]
    fn status_rpcs_change_the_senders_entry_in_the_players_list() {
        let Some((mut g, _)) = world() else { return };
        join(&mut g, 2, "Bob");
        let before = entry(&g, 2);
        send_scene(&mut g, 1, "PlayersDataManager.SetLightStatusServerRpc", &[json!(true), json!(2)]);
        send_scene(&mut g, 1, "PlayersDataManager.SetBackpackStatusServerRpc", &[json!(true), json!(3)]);
        send_scene(&mut g, 1, "PlayersDataManager.SetJetpackStatusServerRpc", &[json!(true), json!(4)]);
        send_scene(&mut g, 1, "PlayersDataManager.SetSkinStatusServerRpc", &[json!(5)]);
        assert_eq!(entry(&g, 1), (2, true, 3, 4, 5));
        assert_eq!(entry(&g, 2), before);
        // Taken off: tier 0, whatever tier is sent.
        send_scene(&mut g, 1, "PlayersDataManager.SetBackpackStatusServerRpc", &[json!(false), json!(3)]);
        send_scene(&mut g, 1, "PlayersDataManager.SetJetpackStatusServerRpc", &[json!(false), json!(4)]);
        send_scene(&mut g, 1, "PlayersDataManager.SetLightStatusServerRpc", &[json!(false), json!(2)]);
        assert_eq!(entry(&g, 1), (2, false, 0, 0, 5));
    }

    /// The map shows the other players where they stand; nobody else in the game: no answer.
    #[test]
    fn map_markers_show_the_other_players() {
        let Some((mut g, _)) = world() else { return };
        send_scene(&mut g, 1, "PlayersDataManager.GetPlayersPositionsServerRpc", &[]);
        assert!(sent(&g, "PlayersDataManager.UpdateMapMarkerPositionsClientRpc").is_empty());
        let bob = join(&mut g, 2, "Bob");
        g.net.move_object(bob, [600.0, 5.0, 480.0], None);
        approve(&mut g, 3);
        g.net.out.clear();
        send_scene(&mut g, 1, "PlayersDataManager.GetPlayersPositionsServerRpc", &[]);
        let s = sent(&g, "PlayersDataManager.UpdateMapMarkerPositionsClientRpc");
        assert_eq!(s.iter().map(|s| (s.to, s.args.clone())).collect::<Vec<_>>(), vec![(Target::Only(1), vec![json!([2]), json!([[600.0, 5.0, 480.0]])])], "not the one still loading");
    }

    /// PlayerReady: printable names of at most 64 characters, refused when access says no.
    #[test]
    fn player_names_are_cleaned_and_checked() {
        let Some(mut g) = crate::game::test_game(save()) else { return };
        approve(&mut g, 1);
        let long = format!("A\u{7}nn{}", "x".repeat(100));
        g.players_rpc(1, "PlayerMainController.PlayerReadyServerRpc", &crate::game::Args(vec![json!(0), json!(long)]));
        let name = &g.clients[&1].ready.as_ref().unwrap().name;
        assert_eq!((name.len(), &name[..3]), (64, "Ann"));
        g.access.lists.banned_names.push("mallory".into());
        approve(&mut g, 2);
        g.players_rpc(2, "PlayerMainController.PlayerReadyServerRpc", &crate::game::Args(vec![json!(0), json!("Mallory")]));
        assert!(g.clients[&2].ready.is_none());
        assert!(g.kicks.contains(&(2, crate::access::REFUSED.to_string())));
    }

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

    /// The saved camera view follows the game's rule for players who are not the host.
    #[test]
    fn saved_camera_view_is_one_at_yaw_zero() {
        let Some(mut g) = crate::planets::tests::game() else { return };
        let ann = join(&mut g, 1, "Ann");
        let saved = |g: &mut crate::game::Game| g.snapshot().players.into_iter().find(|p| p["name"] == "Ann").unwrap()["cameraView"].clone();
        g.net.objects.get_mut(&ann).unwrap().rotation = [0.0, 0.0, 0.0, 1.0];
        assert_eq!(saved(&mut g), 1);
        g.net.objects.get_mut(&ann).unwrap().rotation = crate::geometry::from_euler_y(90.0);
        assert_eq!(saved(&mut g), 0);
    }

    /// PlayerEquipment.InitEquipment on the server: a joined player's equipment takes
    /// Equipment items only (sweep_use: Iron into the equipment is refused by the game).
    #[test]
    fn equipment_takes_equipment_items_only() {
        let Some((mut g, _)) = world() else { return };
        let (inventory, equipment) = { let s = g.clients[&1].ready.as_ref().unwrap(); (s.inventory, s.equipment) };
        let item = |g: &mut crate::game::Game, id: &str| g.add_new_item(g.data.group_by_hash(stable_hash(id)).unwrap(), inventory, 0).1;
        let (iron, tank) = (item(&mut g, "Iron"), item(&mut g, "OxygenTank1"));
        g.net.out.clear();
        send_scene(&mut g, 1, "InventoriesHandler.TransferItemServerRpc", &[json!(inventory), json!(equipment), json!(iron)]);
        assert_eq!(one(&g, "InventoriesHandler.AddOrRemoveItemClientRpc"), vec![json!(false)]);
        g.net.out.clear();
        send_scene(&mut g, 1, "InventoriesHandler.TransferItemServerRpc", &[json!(inventory), json!(equipment), json!(tank)]);
        assert_eq!(one(&g, "InventoriesHandler.AddOrRemoveItemClientRpc"), vec![json!(true)]);
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

    /// players.forgetAfterDays: only a player away that long, who made and earned nothing and holds
    /// no more than the joining kit, is forgotten, with their two inventories and the kit; anyone
    /// in the game, the host, a crafter and anyone with an item of their own stay.
    #[test]
    fn players_away_with_nothing_past_the_kit_are_forgotten_and_no_one_else() {
        let Some(mut g) = crate::planets::tests::game() else { return };
        let day = 86_400;
        let t0 = crate::util::now_secs();
        let has = |g: &crate::game::Game, name: &str| g.saved_players.iter().any(|p| p["name"] == name);
        let leave = |g: &mut crate::game::Game, client: u64, name: &str, f: &dyn Fn(&mut crate::game::Game, i32)| {
            let backpack = {
                join(g, client, name);
                g.clients[&client].ready.as_ref().unwrap().inventory
            };
            f(g, backpack);
            g.disconnect(client);
        };
        // Off (the default): the record carries no time, and nothing is ever forgotten.
        leave(&mut g, 1, "Ann", &|_, _| {});
        assert!(g.saved_players.iter().all(|p| p["pcdsLastSeen"].is_null()), "the save is the game's while the rule is off");
        assert_eq!(forget_unseen(&mut g, t0 + 1000 * day), 0);
        assert!(has(&g, "Ann"));
        g.rules.players.forget_after_days = 30;
        let iron = g.data.group("Iron").unwrap();
        leave(&mut g, 2, "Bob", &|g, backpack| {
            g.add_new_item(iron, backpack, 0);
        });
        leave(&mut g, 3, "Cat", &|g, _| {
            g.total_crafted.insert(3, 1);
        });
        leave(&mut g, 4, "Dan", &|_, _| {});
        join(&mut g, 5, "Eve");
        // The host this save marks, away and empty, stays too.
        g.saved_players.iter_mut().find(|p| p["name"] == "Dan").unwrap()["host"] = json!(true);
        let ann = g.saved_players.iter().find(|p| p["name"] == "Ann").unwrap().clone();
        let (ann_backpack, ann_equipment) = (ann["inventoryId"].as_i64().unwrap() as i32, ann["equipmentId"].as_i64().unwrap() as i32);
        let kit_items = g.inventories[&ann_backpack].items.clone();
        assert!(!kit_items.is_empty() && kit_items.iter().all(|wo| g.wos.contains_key(wo)));
        // Ann left before the rule was on: unstamped, so her days count from the first look.
        assert!(g.saved_players.iter().find(|p| p["name"] == "Ann").unwrap()["pcdsLastSeen"].is_null());
        assert_eq!(forget_unseen(&mut g, t0), 0);
        assert_eq!(forget_unseen(&mut g, t0 + 29 * day), 0, "29 days is not 30");
        assert!(["Ann", "Bob", "Cat", "Dan"].iter().all(|n| has(&g, n)));
        // 31 days on: only Ann (kit, no crafts, no tokens). Eve is in the game, Bob holds iron, Cat
        // crafted, Dan is marked host.
        g.saved_players.iter_mut().filter(|p| p["name"] != "Ann").for_each(|p| p["pcdsLastSeen"] = json!(t0));
        assert_eq!(forget_unseen(&mut g, t0 + 31 * day), 1);
        assert!(!has(&g, "Ann") && ["Bob", "Cat", "Dan"].iter().all(|n| has(&g, n)));
        assert!(!g.inventories.contains_key(&ann_backpack) && !g.inventories.contains_key(&ann_equipment));
        assert!(kit_items.iter().all(|wo| !g.wos.contains_key(wo)), "her starter items go with her inventory");
        assert!(g.clients[&5].ready.is_some());
        let bob = g.saved_players.iter().find(|p| p["name"] == "Bob").unwrap().clone();
        assert!(g.inventories.contains_key(&(bob["inventoryId"].as_i64().unwrap() as i32)), "an item of his own keeps his things");
        // Eve is online: her record may be in the list at a save; she is never forgotten.
        crate::players::update_saved_players(&mut g);
        g.saved_players.iter_mut().find(|p| p["name"] == "Eve").unwrap()["pcdsLastSeen"] = json!(0);
        assert_eq!(forget_unseen(&mut g, t0 + 1000 * day), 0);
        assert!(has(&g, "Eve"));
        // A clock that stepped back: a time ahead of now is not "away".
        g.saved_players.iter_mut().find(|p| p["name"] == "Bob").unwrap()["pcdsLastSeen"] = json!(t0 + 5000 * day);
        assert_eq!(forget_unseen(&mut g, t0 + 1000 * day), 0);
        // Rule off again: nothing.
        g.rules.players.forget_after_days = 0;
        assert_eq!(forget_unseen(&mut g, t0 + 9999 * day), 0);
    }
}
