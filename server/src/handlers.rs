//! The small server RPCs: the world-object proxies (text, color, setting, linked groups and
//! planet, panels), chat, messages, tokens, and the player animations shared with others.

use serde_json::json;

use crate::game::{Args, Game};
use crate::net::{enc, ListEvent, Target, VarValue};

/// Clients other than `client` whose player is within `range` of `client`'s player
/// (NetworkUtils.GetAllButSenderClientParamsInRange), among PlayersManager.playersControllers
/// (players set up by PlayerReady; loading ones are not).
pub fn others_in_range(game: &Game, client: u64, range: f32) -> Vec<u64> {
    let Some(me) = game.player_object(client).and_then(|o| game.net.objects.get(&o)).map(|o| o.position) else { return Vec::new() };
    game.clients
        .values()
        .filter(|c| c.id != client && c.ready.is_some())
        .filter(|c| c.player.and_then(|o| game.net.objects.get(&o)).is_some_and(|o| o.position.iter().zip(me).map(|(a, b)| (a - b) * (a - b)).sum::<f32>() < range * range))
        .map(|c| c.id)
        .collect()
}

fn broadcast(game: &mut Game, client: u64, object: u64, name: &str, args: &[serde_json::Value], range: f32) {
    for c in others_in_range(game, client, range) {
        game.net.rpc(Target::Only(c), object, name, args);
    }
}

/// A component field of the player prefab (as the host's copy was dumped).
fn player_field(game: &Game, class: &str, field: &str, default: f32) -> f32 {
    game.data.singletons[class][field].as_f64().map(|v| v as f32).unwrap_or(default)
}

pub fn rpc(game: &mut Game, client: u64, object: u64, behaviour: u16, name: &str, a: &Args) -> bool {
    let wo = game.net.objects.get(&object).and_then(|o| o.wo);
    match name {
        "TextProxy.SetTextServerRpc" => {
            if game.net.behaviour_index(object, "TextProxy").is_none() {
                return true;
            }
            // JSONExport.CleanStringForInsertInJson, then the FixedString128 the variable holds
            // (the world object takes the variable's value).
            let bytes = enc::fixed_string(&a.s(0).unwrap_or_default().replace(['@', '|'], " "), 128);
            let text = String::from_utf8_lossy(&bytes[4..]).into_owned();
            game.net.set(object, "TextProxy", "_text", bytes);
            if let Some(w) = wo.and_then(|w| game.wos.get_mut(&w)) {
                w.text = text;
            }
        }
        "ColorProxy.SetColorServerRpc" => set_color(game, object, a.q(0)),
        "SettingProxy.SetSettingServerRpc" => {
            game.net.set(object, "SettingProxy", "_setting", enc::int(a.i(0)));
            if let Some(w) = wo.and_then(|w| game.wos.get_mut(&w)) {
                w.setting = a.i(0);
            }
        }
        "LinkedGroupsProxy.SetLinkedGroupsServerRpc" => set_linked_groups(game, object, &a.ints(0).unwrap_or_default()),
        "LinkedPlanetProxy.SetLinkedPlanetServerRpc" => {
            game.net.set(object, "LinkedPlanetProxy", "_linkedPlanet", enc::int(a.i(0)));
            if let Some(w) = wo.and_then(|w| game.wos.get_mut(&w)) {
                w.planet_linked = a.i(0);
            }
        }
        "Panel.ChangePanelServerRpc" => change_panel(game, object, behaviour, a.i(0), a.b(1), a.b(2)),
        "LockNetworkPlayerPosition.SetPlayerLockedServerRpc" => {
            // A vehicle has one per seat: the RPC's own behaviour.
            game.net.set_behaviour_var(object, behaviour, "_active", enc::bool(a.b(0)));
            game.net.set_behaviour_var(object, behaviour, "_playerId", enc::ulong(a.u(1)));
        }
        "ActionnableInteractiveProxy.BroadcastToAllClientsServerRpc" => {
            let range = game.data.prefab_components(&game.wos.get(&wo.unwrap_or(0)).map(|w| game.data.groups[w.group].id.clone()).unwrap_or_default(), "ActionnableInteractiveProxy").first().and_then(|c| c["_shareDistance"].as_f64()).unwrap_or(50.0) as f32;
            broadcast(game, client, object, "ActionnableInteractiveProxy.BroadcastToAllClientsClientRpc", &[], range);
        }
        "PlayerShareState.InputOnJumpServerRpc" | "PlayerShareState.StartConstructingServerRpc" | "PlayerShareState.StartDeconstructingServerRpc" => {
            let range = player_field(game, "PlayerShareState", "_broadcastRange", 100.0);
            let reply = name.replace("ServerRpc", "ClientRpc");
            broadcast(game, client, object, &reply, &[], range);
        }
        "PlayerShareState.StartMiningServerRpc" => {
            let range = player_field(game, "PlayerShareState", "_broadcastRange", 100.0);
            broadcast(game, client, object, "PlayerShareState.StartMiningClientRpc", &[a.0[0].clone(), a.0[1].clone(), a.0[2].clone()], range);
        }
        "PlayerShareState.StopMiningServerRpc" => {
            game.net.rpc(Target::Except(client), object, "PlayerShareState.StopMiningClientRpc", &[]);
        }
        "PlayerStatus.HandleDyingConsequencesServerRpc" => crate::status::handle_dying_consequences(game, client),
        "GroupNetworkBase.DeconstructServerRpc" => crate::deconstruct::deconstruct(game, client, object, a.f(0)),
        "ChatHandler.SendChatMessageServerRpc" => {
            let message = a.s(0).unwrap_or_default();
            game.net.rpc_scene(Target::All, "ChatHandler.ReceiveChatMessageClientRpc", &[json!(client), json!(message)]);
        }
        "MessagesHandler.AddNewReceivedMessageServerRpc" => crate::handlers::add_message(game, a.i(0), a.b(1)),
        "MessagesHandler.MarkAsReadServerRpc" => {
            let hash = a.0[0][1].as_i64().unwrap_or(0) as i32;
            mark_read(game, hash);
        }
        "TokensHandler.GainTokensServerRpc" => gain_tokens(game, a.i(0)),
        "WorldObjectAssociatedProxy.GetWorldObjectIdServerRpc" => {
            if let Some(w) = wo.and_then(|w| game.wos.get(&w)) {
                let hash = game.data.groups[w.group].hash;
                game.net.rpc(Target::Only(client), object, "WorldObjectAssociatedProxy.GetWorldObjectIdClientRpc", &[json!(w.id), json!(hash)]);
            }
        }
        "WorldObjectAssociatedProxy.GetWorldObjectDetailsServerRpc" => {
            if let Some(w) = wo.and_then(|w| game.wos.get(&w)) {
                // One multiplier per unit of the planet, in WorldUnitsPlanet order.
                let order = ["Oxygen", "Heat", "Pressure", "Energy", "Terraformation", "Biomass", "Plants", "Insects", "Animals", "Purification"];
                let multipliers: Vec<f32> = order.iter().map(|u| crate::model::unit_index(u).map(|i| w.unit_multiplier[i]).unwrap_or(0.0)).collect();
                let hash = game.data.groups[w.group].hash;
                let args = [json!(w.id), json!(hash), json!(w.energy), json!(multipliers), json!(w.hunger), json!(w.linked_inventory)];
                game.net.rpc(Target::Only(client), object, "WorldObjectAssociatedProxy.GetWorldObjectDetailsClientRpc", &args);
            }
        }
        "InventoryAssociatedProxy.GetInventoryIdServerRpc" => {
            // The RPC's own proxy: a building can have one per inventory (growers, farms, the truck).
            let nth = game.net.objects.get(&object).and_then(|o| o.behaviours.iter().filter(|b| b.class == "InventoryAssociatedProxy").position(|b| b.index == behaviour)).unwrap_or(0);
            let group = wo.and_then(|w| game.wos.get(&w)).map(|w| game.data.groups[w.group].id.clone()).unwrap_or_default();
            let index = game.data.prefab_components(&group, "InventoryAssociatedProxy").get(nth).and_then(|c| c["_secondaryInventoryIndex"].as_i64()).unwrap_or(-1);
            // GetRequestedInventoryData: (world object, its linked or n-th secondary inventory).
            if let Some(w) = wo.and_then(|w| game.wos.get(&w)) {
                let inventory = if index < 0 { w.linked_inventory } else { w.secondary_inventories.get(index as usize).copied().unwrap_or(0) };
                let args = [json!(w.id), json!(game.data.groups[w.group].hash), json!(inventory)];
                game.net.rpc(Target::Only(client), object, "InventoryAssociatedProxy.GetInventoryIdClientRpc", &args);
            }
        }
        // PlayStation Network only: nothing to do on PC.
        n if n.starts_with("PlaystationNetworkPlayersHandler.") => {}
        _ => return false,
    }
    true
}

/// Panel.ChangePanelServerRpc: the panel's look; the world object keeps every panel's type.
/// The RPC addresses one of the object's Panel behaviours by index.
pub fn change_panel(game: &mut Game, object: u64, index: u16, sub_type: i32, refresh: bool, disolve: bool) {
    let current = game.net.behaviour_var(object, index, "_currentSubPanelType").cloned();
    if current.is_none() || matches!(current, Some(VarValue::Plain(ref v)) if *v == enc::enumeration(sub_type)) {
        return;
    }
    game.net.set_behaviour_var(object, index, "_currentSubPanelType", enc::enumeration(sub_type));
    game.panel_constructed(sub_type);
    if disolve {
        let flipped = !matches!(game.net.behaviour_var(object, index, "_disolveOnSwitch"), Some(VarValue::Plain(v)) if v.first() == Some(&1));
        game.net.set_behaviour_var(object, index, "_disolveOnSwitch", enc::bool(flipped));
    }
    if refresh {
        let panels: Vec<i32> = game.net.objects[&object]
            .behaviours
            .iter()
            .filter(|b| b.class == "Panel")
            .map(crate::panels::sub_type)
            .collect();
        if let Some(w) = game.net.objects[&object].wo.and_then(|w| game.wos.get_mut(&w)) {
            w.panels = Some(panels);
        }
    }
}

impl Game {
    /// WorldObjectsHandler.PanelConstructed.
    pub fn panel_constructed(&mut self, sub_type: i32) {
        let Some((object, _)) = self.net.find_class("WorldObjectsHandler") else { return };
        let items = match self.net.get(object, "WorldObjectsHandler", "_constructedPanelTypes") {
            Some(VarValue::List(items)) => items.clone(),
            _ => return,
        };
        let event = match items.iter().position(|i| i32::from_le_bytes(i[0..4].try_into().unwrap()) == sub_type) {
            None => ListEvent::Add(Game::count_item(sub_type, 1)),
            Some(i) => ListEvent::Value(i as i32, Game::count_item(sub_type, i32::from_le_bytes(items[i][4..8].try_into().unwrap()) + 1)),
        };
        self.net.list_events(object, "WorldObjectsHandler", "_constructedPanelTypes", vec![event]);
    }
}

/// MessagesHandler.AddNewReceivedMessageServerRpc.
pub fn add_message(game: &mut Game, hash: i32, popup: bool) {
    if game.save.messages.iter().any(|m| m["stringId"].as_str().map(crate::data::stable_hash) == Some(hash)) {
        return;
    }
    let Some(id) = game.data.assets.iter().find(|(k, v)| k.starts_with("MessageData:") && v["stringId"].as_str().map(crate::data::stable_hash) == Some(hash)).and_then(|(_, v)| v["stringId"].as_str().map(str::to_string)) else { return };
    game.save.messages.push(json!({"stringId": id, "isRead": false}));
    let item = [vec![0, 0, 0, 0], hash.to_le_bytes().to_vec()].concat();
    game.list_events_on("MessagesHandler", "_receivedMessages", vec![ListEvent::Add(item)]);
    if popup {
        game.net.rpc_scene(Target::All, "MessagesHandler.AddNewReceivedMessageClientRpc", &[]);
    }
}

fn mark_read(game: &mut Game, hash: i32) {
    let mut events = Vec::new();
    for (i, m) in game.save.messages.iter_mut().enumerate() {
        if m["stringId"].as_str().map(crate::data::stable_hash) == Some(hash) {
            m["isRead"] = json!(true);
            events.push(ListEvent::Value(i as i32, [vec![1, 0, 0, 0], hash.to_le_bytes().to_vec()].concat()));
        }
    }
    game.list_events_on("MessagesHandler", "_receivedMessages", events);
}

/// TokensHandler.GainTokensServerRpc.
pub fn gain_tokens(game: &mut Game, gained: i32) {
    let current = match game.net.find_class("TokensHandler").and_then(|(o, _)| game.net.get(o, "TokensHandler", "_tokensNumber")) {
        Some(VarValue::Plain(b)) => crate::wire::Reader::new(b).packed_u64().unwrap_or(0),
        _ => 0,
    };
    let total = (current as i128 + gained as i128).clamp(0, u64::MAX as i128) as u64;
    game.set_on("TokensHandler", "_tokensNumber", enc::ulong(total));
    if gained <= 0 {
        return;
    }
    game.net.rpc_scene(Target::All, "TokensHandler.GainTokensClientRpc", &[json!(gained)]);
    for t in game.all_time_tokens.values_mut() {
        *t = t.saturating_add(gained as u64);
    }
    let all_time = game.save.world["allTimeTerraTokens"].as_u64().unwrap_or(0).saturating_add(gained as u64);
    game.save.world["allTimeTerraTokens"] = json!(all_time);
}

/// LinkedGroupsProxy.SetLinkedGroupsServerRpc: the list is cleared, then each group added;
/// the world object follows the list (OnListChanged).
pub fn set_linked_groups(game: &mut Game, object: u64, hashes: &[i32]) {
    let mut events = vec![ListEvent::Clear];
    events.extend(hashes.iter().map(|h| ListEvent::Add(enc::int(*h))));
    game.net.list_events(object, "LinkedGroupsProxy", "_linkedGroupsHash", events);
    let groups: Vec<usize> = hashes.iter().filter_map(|h| game.data.group_by_hash(*h)).collect();
    if let Some(w) = game.net.objects.get(&object).and_then(|o| o.wo).and_then(|w| game.wos.get_mut(&w)) {
        w.linked_groups = Some(groups);
    }
}

/// InventorySetFirstItemAsLinked (Container1-3): the server shows the first item's group on
/// the container. inventory -> (container, previousGroupDefined).
#[derive(Default, serde::Serialize, serde::Deserialize)]
pub struct FirstItemLinked(std::collections::HashMap<i32, (i32, Option<usize>)>);

/// Start/RegisterToInventory, when the container gets its network object.
pub fn first_item_linked_instantiated(game: &mut Game, wo: i32) {
    let Some(w) = game.wos.get(&wo) else { return };
    if game.data.prefab_components(&game.data.groups[w.group].id, "InventorySetFirstItemAsLinked").is_empty() {
        return;
    }
    let (inventory, empty) = (w.linked_inventory, w.linked_groups.as_ref().is_none_or(|g| g.is_empty()));
    game.ext.first_item_linked.0.insert(inventory, (wo, None));
    if empty {
        first_item_linked_changed(game, inventory);
    }
}

/// OnInventoryModified.
pub fn first_item_linked_changed(game: &mut Game, inventory: i32) {
    let Some(&(wo, previous)) = game.ext.first_item_linked.0.get(&inventory) else { return };
    let Some(net) = game.wos.get(&wo).and_then(|w| w.net) else { return };
    let first = game.inventories.get(&inventory).and_then(|i| i.items.first()).and_then(|i| game.wos.get(i)).map(|w| w.group);
    let hashes = match first {
        Some(g) if previous != Some(g) => vec![game.data.groups[g].hash],
        Some(_) => return,
        None => Vec::new(),
    };
    game.ext.first_item_linked.0.insert(inventory, (wo, first));
    set_linked_groups(game, net, &hashes);
}

/// OnDestroy.
pub fn first_item_linked_destroyed(game: &mut Game, wo: &crate::model::WorldObject) {
    game.ext.first_item_linked.0.remove(&wo.linked_inventory);
}


/// ColorProxy.SetColorServerRpc: opaque, and the world object follows (OnValueChanged).
pub fn set_color(game: &mut Game, object: u64, mut color: [f32; 4]) {
    color[3] = 1.0;
    game.net.set(object, "ColorProxy", "_color", enc::color(color));
    if let Some(w) = game.net.objects.get(&object).and_then(|o| o.wo).and_then(|w| game.wos.get_mut(&w)) {
        w.color = color;
    }
}

/// WorldObjectColor on buildings: 0.75 s after one spawns without a colour, the server gives
/// it the default colour, or the last colour the server itself set (a static in the game).
#[derive(Default, serde::Serialize, serde::Deserialize)]
pub struct DefaultColors {
    pending: Vec<(f32, i32)>,
    last: [f32; 4],
}

pub fn default_color_instantiated(game: &mut Game, wo: i32) {
    let Some(w) = game.wos.get(&wo) else { return };
    let Some(net) = w.net else { return };
    let uncolored = w.color[..3].iter().all(|c| *c <= 0.0);
    let building = game.net.behaviour_index(net, "ConstructibleProxy").is_some() && game.net.behaviour_index(net, "ColorProxy").is_some();
    if uncolored && building && !game.data.prefab_components(&game.data.groups[w.group].id, "WorldObjectColor").is_empty() {
        let due = game.time + 0.75;
        game.ext.default_colors.pending.push((due, wo));
    }
}

pub fn default_colors_tick(game: &mut Game) {
    let now = game.time;
    let (due, pending): (Vec<_>, Vec<_>) = std::mem::take(&mut game.ext.default_colors.pending).into_iter().partition(|(t, _)| *t <= now);
    game.ext.default_colors.pending = pending;
    for (_, wo) in due {
        let Some(w) = game.wos.get(&wo) else { continue };
        let Some(net) = w.net else { continue };
        let defaults: Vec<[f32; 4]> = game.data.prefab_components(&game.data.groups[w.group].id, "WorldObjectColor").iter().map(|c| {
            let f = |i: usize| c["defaultColor"][i].as_f64().unwrap_or(0.0) as f32;
            [f(0), f(1), f(2), f(3)]
        }).collect();
        // One Invoke per WorldObjectColor component of the building.
        for default in defaults {
            let last = game.ext.default_colors.last;
            let color = if last[3] > 0.0 { last } else { default };
            game.ext.default_colors.last = color;
            set_color(game, net, color);
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::game::Args;
    use crate::net::VarValue;
    use serde_json::json;

    #[test]
    fn containers_show_their_first_item() {
        let Some(mut g) = crate::planets::tests::game() else { return };
        let iron = g.data.group("Iron").unwrap();
        assert_eq!(g.wos[&205000000].linked_groups, Some(vec![iron]), "set at load: the container had no linked group");
        g.inventories_rpc(0, "InventoriesHandler.RemoveItemFromInventoryServerRpc", &Args(vec![json!(205000001), json!(2), json!(true)]));
        assert_eq!(g.wos[&205000000].linked_groups, Some(vec![]));
        let net = g.wos[&205000000].net.unwrap();
        assert!(matches!(g.net.get(net, "LinkedGroupsProxy", "_linkedGroupsHash"), Some(VarValue::List(l)) if l.is_empty()));
    }

    #[test]
    fn buildings_get_their_default_colour() {
        let Some(mut g) = crate::planets::tests::game() else { return };
        let lamp = g.data.group("OutsideLamp1").unwrap();
        let id = g.new_world_object_id();
        g.create_world_object(lamp, id, true);
        g.wos.get_mut(&id).unwrap().position = [565.0, 3.0, 500.0];
        g.instantiate(id, false, 0);
        let net = g.wos[&id].net.unwrap();
        assert!(matches!(g.net.get(net, "ColorProxy", "_color"), Some(VarValue::Plain(b)) if *b == crate::net::enc::color([0.0; 4])));
        g.time += 0.8;
        super::default_colors_tick(&mut g);
        assert_eq!(g.wos[&id].color, [1.0, 0.969676554, 0.8443396, 1.0]);
    }

    #[test]
    fn range_relays_skip_loading_players() {
        let Some(mut g) = crate::game::test_game(crate::game::tests::save()) else { return };
        let ann = crate::game::tests::join(&mut g, 1, "Ann");
        crate::game::tests::join(&mut g, 2, "Bob");
        let loading = crate::game::tests::approve(&mut g, 3);
        let at = g.net.objects[&ann].position;
        g.net.move_object(loading, at, None);
        assert_eq!(super::others_in_range(&g, 1, 5.0), vec![2]);
    }
}
