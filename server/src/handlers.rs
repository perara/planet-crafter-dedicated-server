//! The small server RPCs: the world-object proxies (text, color, setting, linked groups and
//! planet, panels), chat, messages, tokens, and the player animations shared with others.

use std::collections::{HashMap, HashSet, VecDeque};

use serde_json::json;

use crate::clock::GameTime;
use crate::game::{Args, Game};
use crate::net::{enc, ListEvent, Target, VarValue};

/// Characters of a chat message passed on (the game's chat field sets no limit of its own:
/// a line longer than this is cut). A client sends CHAT_BURST messages at once and one per
/// CHAT_EVERY seconds after that; more are dropped.
pub const MAX_CHAT: usize = 500;
const CHAT_BURST: f32 = 10.0;
const CHAT_EVERY: f32 = 1.0;
/// RPC work per client per tick (most RPCs cost 1); the rest waits for the next ticks, in
/// order, up to MAX_PENDING RPCs (more are dropped).
const RPC_BUDGET: u32 = 32;
const MAX_PENDING: usize = 65_536;
/// And the bytes of those: an RPC is up to a batch (1 MiB), so the count alone allowed gigabytes.
const MAX_PENDING_BYTES: usize = 4 << 20;
/// What a client may create before its PlayerReady: inventories and the items put in them as
/// they are made. A joining game client creates its backpack and equipment (2), and the
/// containers of the planet's scene with a display make theirs (InventoryAssociated.Start, 2 s
/// after loading, with the loot the scene rolls: tens of items at most); a client that goes on
/// past these is flooding (a world object per `content` entry, up to the inventory's size).
pub(crate) const MAX_LOADING_INVENTORIES: u32 = 256;
pub(crate) const MAX_LOADING_ITEMS: u32 = 4096;

/// Transform states and variable deltas a client's objects send are passed on to every other
/// client, so what a client may send is bounded too: MOTION_PER_SECOND units, a message costing
/// 1 and one more per 64 bytes, with a second's worth as burst. A game client sends at most
/// about 2500 units a second (its player and the tens of animals and drops it simulates, 30
/// times a second each); more is dropped, the next state replaces it anyway.
const MOTION_PER_SECOND: f64 = 8000.0;

/// What one client may do how often: chat, RPC work and motion.
#[derive(Default, serde::Serialize, serde::Deserialize)]
pub struct Limits {
    /// Per client: the clock when it last sent motion and the units it may still send.
    #[serde(skip)]
    motion: HashMap<u64, (f64, f64)>,
    /// Per client: the clock of its last chat message and the messages it may still send (the
    /// clock goes on across a handoff, `started` is part of the snapshot).
    chat: HashMap<u64, (f64, f32)>,
    /// RPC work done this tick per client, and its RPCs waiting for the next ticks.
    #[serde(skip)]
    used: HashMap<u64, u32>,
    pending: HashMap<u64, VecDeque<Vec<u8>>>,
    /// The bytes of each client's pending RPCs (counted again from the queue after a restore).
    #[serde(skip)]
    pending_bytes: HashMap<u64, usize>,
    /// Clients already logged for going over a limit.
    #[serde(skip)]
    warned: HashSet<(u64, &'static str)>,
    /// Per client before PlayerReady: the inventories it made and the items put into them.
    #[serde(skip)]
    loading_created: HashMap<u64, (u32, u32)>,
    /// The worlds sent to addresses that could be forged: when the allowance was last counted, and
    /// how many joins are left in it.
    #[serde(skip)]
    joins: Option<(std::time::Instant, f64)>,
}

/// New sessions from other machines get the world (megabytes) at this rate in all, after a burst of
/// the players the server holds (at least JOIN_BURST). A UDP session needs no answer from its
/// address to be sent the world and to be told, blind, how fast to take it (utp.rs): a forged
/// source address makes the server a reflector from a few hundred bytes to the whole world. This
/// bounds what it can reflect (a join takes seconds, so players never meet it).
const JOINS_PER_SECOND: f64 = 1.0;
const JOIN_BURST: usize = 16;

impl Limits {
    /// `what` logged once for this client (forgotten with it: the table holds the clients there are).
    pub(crate) fn warn(&mut self, client: u64, what: &'static str) {
        if self.warned.insert((client, what)) {
            warn!("client {client}: {what} (logged once)");
        }
    }

    /// Whether a client's chat message goes out now (a client's, not the server's own).
    pub fn chat(&mut self, client: u64, now: f64) -> bool {
        let (last, left) = self.chat.entry(client).or_insert((now, CHAT_BURST));
        *left = (*left + (now - *last).max(0.0) as f32 / CHAT_EVERY).min(CHAT_BURST);
        *last = now;
        if *left < 1.0 {
            self.warn(client, "chat messages over the rate, dropped");
            return false;
        }
        *left -= 1.0;
        true
    }

    /// Whether a transform state or variable delta of `bytes` bytes is taken and passed on.
    pub fn motion(&mut self, client: u64, bytes: usize, now: f64) -> bool {
        let (last, left) = self.motion.entry(client).or_insert((now, MOTION_PER_SECOND));
        *left = (*left + (now - *last).max(0.0) * MOTION_PER_SECOND).min(MOTION_PER_SECOND);
        *last = now;
        let cost = 1.0 + (bytes / 64) as f64;
        if *left < cost {
            self.warn(client, "transforms and variable deltas over the rate, dropped");
            return false;
        }
        *left -= cost;
        true
    }

    /// Whether an RPC of this cost runs now; false when it waits (queued) or is dropped. The
    /// first of a tick always runs, whatever it costs.
    pub fn spend(&mut self, client: u64, cost: u32, rpc: &[u8]) -> bool {
        let used = self.used.entry(client).or_default();
        let waiting = self.pending.get(&client).is_some_and(|p| !p.is_empty());
        if !waiting && (*used == 0 || *used + cost <= RPC_BUDGET) {
            *used += cost;
            return true;
        }
        let queue = self.pending.entry(client).or_default();
        let bytes = *self.pending_bytes.entry(client).or_insert_with(|| queue.iter().map(Vec::len).sum());
        if queue.len() >= MAX_PENDING || bytes + rpc.len() > MAX_PENDING_BYTES {
            self.warn(client, "RPCs over the budget and the queue, dropped");
            return false;
        }
        queue.push_back(rpc.to_vec());
        self.pending_bytes.insert(client, bytes + rpc.len());
        self.warn(client, "RPCs over the budget of a tick, queued");
        false
    }

    /// Whether a session from an address other than this machine's gets the world now (see
    /// JOINS_PER_SECOND); `players`: the most the server holds.
    pub fn join(&mut self, now: std::time::Instant, players: usize) -> bool {
        let burst = players.max(JOIN_BURST) as f64;
        let (last, left) = self.joins.get_or_insert((now, burst));
        *left = (*left + now.saturating_duration_since(*last).as_secs_f64() * JOINS_PER_SECOND).min(burst);
        *last = now;
        if *left < 1.0 {
            return false;
        }
        *left -= 1.0;
        true
    }

    /// Whether a client that is no player yet may create an inventory with `items` initial items
    /// (MAX_LOADING_INVENTORIES and MAX_LOADING_ITEMS in all); a refusal is logged once.
    pub fn loading_create(&mut self, client: u64, items: usize) -> bool {
        let (made, put) = self.loading_created.entry(client).or_default();
        let items = items.min(u32::MAX as usize) as u32;
        if *made >= MAX_LOADING_INVENTORIES || put.saturating_add(items) > MAX_LOADING_ITEMS {
            self.warn(client, "inventories created before PlayerReady over the limit, refused");
            return false;
        }
        (*made, *put) = (*made + 1, *put + items);
        true
    }

    pub fn forget(&mut self, client: u64) {
        self.loading_created.remove(&client);
        self.chat.remove(&client);
        self.motion.remove(&client);
        self.warned.retain(|(c, _)| *c != client);
        self.used.remove(&client);
        self.pending.remove(&client);
        self.pending_bytes.remove(&client);
    }
}

/// The work an RPC makes: HasWorldObjectAtPosition looks at every world object, as the game's, and
/// the rest one each and one more for every KiB it carries (a game client's RPCs are tens of bytes;
/// a list of thousands of items is that many times the handler's work).
pub fn rpc_cost(name: &str, world_objects: usize, bytes: usize) -> u32 {
    let size = (bytes / 1024).min(RPC_BUDGET as usize) as u32;
    match name {
        "WorldObjectsHandler.HasWorldObjectAtPositionServerRpc" => 1 + (world_objects / 1024) as u32 + size,
        _ => 1 + size,
    }
}

/// A new tick: each client's waiting RPCs, in order, within its budget.
pub fn run_pending(game: &mut Game, cost: impl Fn(&Game, &[u8]) -> u32, run: impl Fn(&mut Game, u64, &[u8])) {
    game.limits.used.clear();
    let clients: Vec<u64> = game.limits.pending.keys().copied().collect();
    for client in clients {
        let mut due = Vec::new();
        let mut used = 0;
        while let Some(rpc) = game.limits.pending.get(&client).and_then(|p| p.front()) {
            let c = cost(game, rpc);
            if used > 0 && used + c > RPC_BUDGET {
                break;
            }
            used += c;
            due.extend(game.limits.pending.get_mut(&client).and_then(VecDeque::pop_front));
        }
        if let Some(bytes) = game.limits.pending_bytes.get_mut(&client) {
            *bytes = bytes.saturating_sub(due.iter().map(Vec::len).sum());
        }
        game.limits.used.insert(client, used);
        if game.limits.pending.get(&client).is_some_and(VecDeque::is_empty) {
            game.limits.pending.remove(&client);
            game.limits.pending_bytes.remove(&client);
        }
        for rpc in due {
            // These run inside the tick, which a panic ends the bank in (the world restarts from its
            // save): a client's RPC is contained as one that ran on arrival is (bank/panic.rs guarded).
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(game, client, &rpc))).is_err() {
                error!("client {client}: the server panicked on an RPC that waited for its turn; kicked");
                game.panicked = true;
                game.kick(client, "Server error");
                break;
            }
        }
    }
}

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
            let bytes = crate::wire::fixed_string(&a.s(0).unwrap_or_default().replace(['@', '|'], " "), 128);
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
            let message: String = a.s(0).unwrap_or_default().chars().take(MAX_CHAT).collect();
            game.net.rpc_scene(Target::All, "ChatHandler.ReceiveChatMessageClientRpc", &[json!(client), json!(message)]);
            crate::events::chat(game, client, &message);
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
                game.net.rpc_on(Target::Only(client), object, behaviour, "WorldObjectAssociatedProxy.GetWorldObjectIdClientRpc", &[json!(w.id), json!(hash)]);
            }
        }
        "WorldObjectAssociatedProxy.GetWorldObjectDetailsServerRpc" => {
            if let Some(w) = wo.and_then(|w| game.wos.get(&w)) {
                // One multiplier per unit of the planet, in WorldUnitsPlanet order.
                let order = ["Oxygen", "Heat", "Pressure", "Energy", "Terraformation", "Biomass", "Plants", "Insects", "Animals", "Purification"];
                let multipliers: Vec<f32> = order.iter().map(|u| crate::model::unit_index(u).map(|i| w.unit_multiplier[i]).unwrap_or(0.0)).collect();
                let hash = game.data.groups[w.group].hash;
                let args = [json!(w.id), json!(hash), json!(w.energy), json!(multipliers), json!(w.hunger), json!(w.linked_inventory)];
                game.net.rpc_on(Target::Only(client), object, behaviour, "WorldObjectAssociatedProxy.GetWorldObjectDetailsClientRpc", &args);
            }
        }
        "InventoryAssociatedProxy.GetInventoryIdServerRpc" => {
            // The RPC's own proxy: a building can have one per inventory (growers, farms, the truck).
            // The proxy keeps the id it is sent for good: when its inventory cannot be told, no
            // answer (and a log line) rather than a wrong one.
            let Some(w) = wo.and_then(|w| game.wos.get(&w)) else { return true };
            let group = game.data.groups[w.group].id.clone();
            let nth = game.net.objects.get(&object).and_then(|o| o.behaviours().filter(|b| b.class == "InventoryAssociatedProxy").position(|b| b.index == behaviour));
            // A group without a prefab of its own is spawned in GroupNetworkContainerWithInventory,
            // whose one proxy keeps the default index (-1: the linked inventory).
            let container = game.data.layouts.get(&group).is_some_and(|l| l.prefab.starts_with("GroupNetworkContainer"));
            let index = if container && nth == Some(0) {
                Some(-1)
            } else {
                nth.and_then(|n| game.data.prefab_components(&group, "InventoryAssociatedProxy").get(n).and_then(|c| c["_secondaryInventoryIndex"].as_i64()))
            };
            // GetRequestedInventoryData: (world object, its linked or n-th secondary inventory).
            let inventory = match index {
                Some(i) if i < 0 => Some(w.linked_inventory),
                Some(i) => w.secondary_inventories.get(i as usize).copied(),
                None => None,
            };
            let Some(inventory) = inventory else {
                warn!("{group} (world object {}): behaviour {behaviour} asked for an inventory it has no data for (proxy {nth:?}, secondary index {index:?})", w.id);
                return true;
            };
            {
                let args = [json!(w.id), json!(game.data.groups[w.group].hash), json!(inventory)];
                // To the proxy that asked: a proxy takes the id it is sent as its own, for good.
                game.net.rpc_on(Target::Only(client), object, behaviour, "InventoryAssociatedProxy.GetInventoryIdClientRpc", &args);
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
    // Only the game's own panel types: any other number would be shown to every client, saved,
    // and counted as a new entry of a list the wire cannot carry past 65535.
    if !(0..crate::panels::SUB_TYPES.len() as i32).contains(&sub_type) {
        warn!("panel type {sub_type} is not one of the game's (0 to {}): ignored", crate::panels::SUB_TYPES.len() - 1);
        return;
    }
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
            .behaviours()
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
    // A group each at most: a longer list is not the game's, and a NetworkList cannot carry
    // 65536 events or items on the wire (its count is a ushort), which would break every client.
    if hashes.len() > game.data.groups.len() {
        warn!("linked groups: {} groups sent for object {object}, more than the game has ({}): ignored", hashes.len(), game.data.groups.len());
        return;
    }
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
    pending: Vec<(GameTime, i32)>,
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
    if game.ext.default_colors.pending.iter().all(|(t, _)| *t > now) {
        return;
    }
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
    use crate::net::{enc, Target, VarValue};
    use crate::testkit::*;
    use serde_json::{json, Value};

    /// New sessions from other machines are let in at JOINS_PER_SECOND after a burst.
    #[test]
    fn joins_have_a_burst_and_a_rate() {
        let mut l = super::Limits::default();
        let t0 = std::time::Instant::now();
        assert!((0..16).all(|_| l.join(t0, 16)));
        assert!(!l.join(t0, 16), "the burst is spent");
        assert!(!l.join(t0 + std::time::Duration::from_millis(900), 16));
        assert!(l.join(t0 + std::time::Duration::from_millis(1000), 16) && !l.join(t0 + std::time::Duration::from_millis(1000), 16), "one a second");
        // A long quiet gives the burst back, not more; a server of more players has a bigger one.
        let later = t0 + std::time::Duration::from_secs(3600);
        assert!((0..16).all(|_| l.join(later, 16)) && !l.join(later, 16));
        let mut big = super::Limits::default();
        assert!((0..64).all(|_| big.join(t0, 64)) && !big.join(t0, 64));
    }

    /// An RPC that waited for its turn runs inside the tick; one that panics there must kick its client
    /// and go on, not end the bank (a client must not be able to restart the world by sending
    /// something that panics: it can, if only a queued RPC does it). The others' RPCs still run.
    #[test]
    fn a_waiting_rpc_that_panics_kicks_its_client_and_not_the_world() {
        let Some((mut g, _)) = world() else { return };
        crate::game::tests::join(&mut g, 2, "Bob");
        for client in [1, 2] {
            g.limits.pending.insert(client, std::collections::VecDeque::from(vec![vec![1u8], vec![2u8]]));
        }
        let ran = std::cell::RefCell::new(Vec::new());
        let quiet = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        super::run_pending(&mut g, |_, _| 1, |_, client, rpc| {
            ran.borrow_mut().push((client, rpc[0]));
            if client == 1 {
                panic!("a handler bug");
            }
        });
        std::panic::set_hook(quiet);
        assert!(g.panicked && g.kicks.iter().any(|(c, _)| *c == 1) && !g.kicks.iter().any(|(c, _)| *c == 2));
        let mut ran = ran.into_inner();
        ran.sort();
        assert_eq!(ran, [(1, 1), (2, 1), (2, 2)], "the client that panicked runs no more, the other goes on");
    }

    /// The RPCs a client has waiting for the next ticks are bounded in bytes too (a count of 8192
    /// RPCs of a batch each was gigabytes); the bytes follow the queue in and out and go with the
    /// client.
    #[test]
    fn rpcs_waiting_for_a_tick_are_bounded_in_bytes_as_well_as_in_number() {
        let Some((mut g, _)) = world() else { return };
        let rpc = vec![0u8; 1 << 20];
        // The first RPC of a tick runs whatever it costs; the rest wait, up to the byte bound.
        assert!(g.limits.spend(1, 32, &rpc));
        for _ in 0..10 {
            assert!(!g.limits.spend(1, 1, &rpc));
        }
        let waiting = g.limits.pending[&1].len();
        assert_eq!(waiting, super::MAX_PENDING_BYTES >> 20, "4 MiB wait, the others were dropped");
        assert_eq!(g.limits.pending_bytes[&1], super::MAX_PENDING_BYTES);
        // A small one fits no better: the bound is on bytes.
        assert!(!g.limits.spend(1, 1, &[1]) && g.limits.pending[&1].len() == waiting + usize::from(g.limits.pending_bytes[&1] < super::MAX_PENDING_BYTES));
        // Another client has a bound of its own.
        assert!(g.limits.spend(2, 1, &rpc));
        // The next ticks run them (32 a tick) and the bytes go down with the queue.
        let ran = std::cell::Cell::new(0);
        crate::handlers::run_pending(&mut g, |_, _| 1, |_, _, _| ran.set(ran.get() + 1));
        assert_eq!(ran.get(), waiting);
        assert!(!g.limits.pending_bytes.contains_key(&1) && !g.limits.pending.contains_key(&1));
        // A client that leaves with RPCs waiting takes its bytes along.
        assert!(g.limits.spend(3, 32, &rpc));
        assert!(!g.limits.spend(3, 1, &rpc));
        g.limits.forget(3);
        assert!(!g.limits.pending_bytes.contains_key(&3) && !g.limits.pending.contains_key(&3));
    }

    /// Text and linked groups set on a container's proxies, colour and setting on a building's:
    /// the variables everyone sees, the world object, the save, and the variables a reloaded
    /// world spawns with.
    #[test]
    fn proxies_set_the_object_its_variables_and_the_save() {
        let Some((mut g, _)) = world() else { return };
        let container = group_with(&g, &["TextProxy", "LinkedGroupsProxy"], true);
        let building = group_with(&g, &["ColorProxy", "SettingProxy"], true);
        let (a, net_a) = place(&mut g, &container, [570.0, 3.0, 500.0]);
        let (b, net_b) = place(&mut g, &building, [580.0, 3.0, 500.0]);
        let iron = hash(&g, "Iron");
        send_on(&mut g, 1, net_a, "TextProxy.SetTextServerRpc", &[json!("Ore@depot|1")]);
        send_on(&mut g, 1, net_a, "LinkedGroupsProxy.SetLinkedGroupsServerRpc", &[json!([iron])]);
        send_on(&mut g, 1, net_b, "ColorProxy.SetColorServerRpc", &[json!([0.25, 0.5, 0.75, 0.1])]);
        send_on(&mut g, 1, net_b, "SettingProxy.SetSettingServerRpc", &[json!(3)]);
        assert_eq!((g.wos[&a].text.as_str(), g.wos[&a].linked_groups.clone()), ("Ore depot 1", Some(vec![g.data.group("Iron").unwrap()])), "save delimiters cleaned");
        assert_eq!((g.wos[&b].color, g.wos[&b].setting), ([0.25, 0.5, 0.75, 1.0], 3), "colour opaque");
        assert_eq!(var(&g, net_a, "TextProxy", "_text"), crate::wire::fixed_string("Ore depot 1", 128));
        assert_eq!(list(&g, net_a, "LinkedGroupsProxy", "_linkedGroupsHash"), vec![enc::int(iron)]);
        assert_eq!(var(&g, net_b, "SettingProxy", "_setting"), enc::int(3));
        let saved = saved_object(&mut g, a).unwrap();
        assert_eq!((&saved["text"], &saved["liGrps"]), (&json!("Ore depot 1"), &json!("Iron")));
        assert_eq!(saved_object(&mut g, b).unwrap()["set"], json!(3));

        let mut g = reload(&mut g);
        join(&mut g, 1, "Ann");
        let (net_a, net_b) = (g.wos[&a].net.unwrap(), g.wos[&b].net.unwrap());
        assert_eq!(var(&g, net_a, "TextProxy", "_text"), crate::wire::fixed_string("Ore depot 1", 128));
        assert_eq!(list(&g, net_a, "LinkedGroupsProxy", "_linkedGroupsHash"), vec![enc::int(iron)]);
        assert_eq!(var(&g, net_b, "ColorProxy", "_color"), enc::color([0.25, 0.5, 0.75, 1.0]));
        assert_eq!(var(&g, net_b, "SettingProxy", "_setting"), enc::int(3));
        // A text past the 128 bytes the variable holds (125 after its length) is cut there, on
        // the object too.
        send_on(&mut g, 1, net_a, "TextProxy.SetTextServerRpc", &[json!("y".repeat(300))]);
        assert_eq!(g.wos[&a].text.len(), 125);
    }

    #[test]
    fn exchange_platforms_keep_their_linked_planet() {
        let Some((mut g, _)) = world() else { return };
        let group = group_with(&g, &["LinkedPlanetProxy"], true);
        let (id, net) = place(&mut g, &group, [570.0, 3.0, 500.0]);
        let selenea = crate::data::stable_hash("Selenea");
        send_on(&mut g, 1, net, "LinkedPlanetProxy.SetLinkedPlanetServerRpc", &[json!(selenea)]);
        assert_eq!(var(&g, net, "LinkedPlanetProxy", "_linkedPlanet"), enc::int(selenea));
        assert_eq!(saved_object(&mut g, id).unwrap()["liPlanet"], json!(selenea));
        let g = reload(&mut g);
        assert_eq!(var(&g, g.wos[&id].net.unwrap(), "LinkedPlanetProxy", "_linkedPlanet"), enc::int(selenea));
    }

    /// A habitat panel's look: the RPC's own Panel behaviour changes, the constructed panel
    /// types count it, and with refresh the world object (and the save) keeps every panel's look.
    #[test]
    fn panels_change_their_look_count_it_and_keep_it() {
        let Some((mut g, _)) = world() else { return };
        let group = group_with(&g, &["Panel"], true);
        let (id, net) = place(&mut g, &group, [570.0, 3.0, 500.0]);
        let panels: Vec<u16> = g.net.objects[&net].behaviours().filter(|b| b.class == "Panel").map(|b| b.index).collect();
        let (handler, _) = g.net.find_class("WorldObjectsHandler").unwrap();
        let counted = |g: &crate::game::Game, t: i32| list(g, handler, "WorldObjectsHandler", "_constructedPanelTypes").iter().find(|i| i32::from_le_bytes(i[0..4].try_into().unwrap()) == t).map(|i| i32::from_le_bytes(i[4..8].try_into().unwrap())).unwrap_or(0);
        let before = counted(&g, 7);
        let disolve = |g: &crate::game::Game| format!("{:?}", g.net.behaviour_var(net, panels[0], "_disolveOnSwitch"));
        let was = disolve(&g);
        send_rpc(&mut g, 1, net, panels[0], "Panel.ChangePanelServerRpc", &[json!(7), json!(true), json!(true)]);
        assert!(matches!(g.net.behaviour_var(net, panels[0], "_currentSubPanelType"), Some(VarValue::Plain(b)) if *b == enc::enumeration(7)));
        assert_eq!(counted(&g, 7), before + 1);
        assert_ne!(disolve(&g), was, "disolve flips the switch");
        let looks = g.wos[&id].panels.clone().unwrap();
        assert_eq!((looks.len(), looks[0]), (panels.len(), 7));
        assert_eq!(saved_object(&mut g, id).unwrap()["pnls"].as_str().unwrap().split(',').next(), Some("7"));
        // The same look again: nothing.
        send_rpc(&mut g, 1, net, panels[0], "Panel.ChangePanelServerRpc", &[json!(7), json!(true), json!(false)]);
        assert_eq!(counted(&g, 7), before + 1);
        // Loaded again, the saved look is set through ChangePanel and counted (the real host's
        // list at join counts every saved panel that is not the prefab's look).
        let g = reload(&mut g);
        let (handler, _) = g.net.find_class("WorldObjectsHandler").unwrap();
        let counted = list(&g, handler, "WorldObjectsHandler", "_constructedPanelTypes").iter().find(|i| i32::from_le_bytes(i[0..4].try_into().unwrap()) == 7).map(|i| i32::from_le_bytes(i[4..8].try_into().unwrap()));
        assert_eq!(counted, Some(1));
    }

    /// Jumping, building, deconstructing and mining are shown to the players in range of the
    /// sender (not to the sender, nor to one far away); stopping to mine to everyone else.
    #[test]
    fn shared_player_states_reach_the_players_in_range() {
        let Some((mut g, ann)) = world() else { return };
        join(&mut g, 2, "Bob");
        let carl = join(&mut g, 3, "Carl");
        g.net.move_object(carl, [5000.0, 3.0, 5000.0], None);
        g.net.out.clear();
        for name in ["PlayerShareState.InputOnJumpServerRpc", "PlayerShareState.StartConstructingServerRpc", "PlayerShareState.StartDeconstructingServerRpc"] {
            send_on(&mut g, 1, ann, name, &[]);
            let reply = name.replace("ServerRpc", "ClientRpc");
            assert_eq!(sent(&g, &reply).into_iter().map(|s| (s.to, s.object)).collect::<Vec<_>>(), vec![(Target::Only(2), ann)], "{name}");
        }
        send_on(&mut g, 1, ann, "PlayerShareState.StartMiningServerRpc", &[json!(101_000_001), json!(1.5), json!(2)]);
        assert_eq!(sent(&g, "PlayerShareState.StartMiningClientRpc").into_iter().map(|s| (s.to, s.args)).collect::<Vec<_>>(), vec![(Target::Only(2), vec![json!(101_000_001), json!(1.5), json!(2)])]);
        send_on(&mut g, 1, ann, "PlayerShareState.StopMiningServerRpc", &[]);
        assert_eq!(sent(&g, "PlayerShareState.StopMiningClientRpc").into_iter().map(|s| s.to).collect::<Vec<_>>(), vec![Target::Except(1)]);
        // An interactive object (a door, a lamp) tells the others near the sender.
        let group = group_with(&g, &["ActionnableInteractiveProxy"], true);
        let (_, net) = place(&mut g, &group, [560.0, 3.0, 495.0]);
        g.net.out.clear();
        send_on(&mut g, 1, net, "ActionnableInteractiveProxy.BroadcastToAllClientsServerRpc", &[]);
        assert_eq!(sent(&g, "ActionnableInteractiveProxy.BroadcastToAllClientsClientRpc").into_iter().map(|s| (s.to, s.object)).collect::<Vec<_>>(), vec![(Target::Only(2), net)]);
    }

    /// The owner-only RPCs (the game's `[Rpc(SendTo.Server)]`, which Netcode drops from anyone
    /// but the object's owner): another client's player shown jumping and mining, an item taken
    /// off its owner, another player's dying consequences, all refused.
    #[test]
    fn owner_only_rpcs_are_refused_from_other_clients() {
        let Some((mut g, ann)) = world() else { return };
        join(&mut g, 2, "Bob");
        g.net.out.clear();
        send_on(&mut g, 2, ann, "PlayerShareState.InputOnJumpServerRpc", &[]);
        send_on(&mut g, 2, ann, "PlayerShareState.StartMiningServerRpc", &[json!(101_000_001), json!(1.5), json!(2)]);
        send_on(&mut g, 2, ann, "PlayerShareState.StopMiningServerRpc", &[]);
        for name in ["PlayerShareState.InputOnJumpClientRpc", "PlayerShareState.StartMiningClientRpc", "PlayerShareState.StopMiningClientRpc"] {
            assert!(sent(&g, name).is_empty(), "{name} was sent for a player that is not the sender");
        }
        let iron = g.create_world_object(g.data.group("Iron").unwrap(), 0, true);
        g.drop_on_floor(iron, [561.0, 3.0, 497.0], [0.0, 0.0, 0.0, 1.0], 0.0, false, false, 0, 0.0, false, 1);
        let net = g.wos[&iron].net.unwrap();
        send_on(&mut g, 2, net, "GroupNetworkContainer.RemoveOwnershipServerRpc", &[]);
        assert_eq!(g.net.objects[&net].owner, 1, "Bob took Ann's item off her");
        send_on(&mut g, 1, net, "GroupNetworkContainer.RemoveOwnershipServerRpc", &[]);
        assert_eq!(g.net.objects[&net].owner, 0, "its owner still can");
    }

    #[test]
    fn chat_messages_go_to_everyone_with_the_sender() {
        let Some((mut g, _)) = world() else { return };
        send_scene(&mut g, 1, "ChatHandler.SendChatMessageServerRpc", &[json!("hello")]);
        assert_eq!(sent(&g, "ChatHandler.ReceiveChatMessageClientRpc").into_iter().map(|s| (s.to, s.args)).collect::<Vec<_>>(), vec![(Target::All, vec![json!(1), json!("hello")])]);
    }

    /// What a client sends into a NetworkList is bounded: a panel type the game has none of
    /// would be a new entry of the constructed-panels list every time (70000 RPCs overflow the
    /// ushort count on the wire), and a linked-groups list longer than the game has groups
    /// would do it at once; every client's and every later joiner's copy of the object breaks.
    #[test]
    fn lists_clients_write_cannot_outgrow_the_wire() {
        let Some((mut g, _)) = world() else { return };
        let container = group_with(&g, &["TextProxy", "LinkedGroupsProxy"], true);
        let building = group_with(&g, &["Panel"], true);
        let (_, net) = place(&mut g, &container, [570.0, 3.0, 500.0]);
        let (_, panel) = place(&mut g, &building, [580.0, 3.0, 500.0]);
        let index = g.net.objects[&panel].behaviours().find(|b| b.class == "Panel").unwrap().index;
        let (handler, _) = g.net.find_class("WorldObjectsHandler").unwrap();
        let constructed = |g: &crate::game::Game| list(g, handler, "WorldObjectsHandler", "_constructedPanelTypes").len();
        let before = constructed(&g);
        for sub_type in [17, 999, -1, i32::MAX] {
            send_rpc(&mut g, 1, panel, index, "Panel.ChangePanelServerRpc", &[json!(sub_type), json!(false), json!(false)]);
        }
        assert_eq!(constructed(&g), before, "no entry for a panel type the game has none of");
        send_on(&mut g, 1, net, "LinkedGroupsProxy.SetLinkedGroupsServerRpc", &[json!((0..70_000).collect::<Vec<i32>>())]);
        assert!(list(&g, net, "LinkedGroupsProxy", "_linkedGroupsHash").len() < 1000);
    }

    /// What the limits remember of a client goes with it: a client id is never used again, so
    /// a flood of short connections would otherwise leave an entry for each, for ever.
    #[test]
    fn a_client_that_left_leaves_nothing_in_the_limits() {
        let mut l = super::Limits::default();
        for client in 1..=3u64 {
            assert!(l.chat(client, 0.0) && l.motion(client, 10, 0.0) && l.spend(client, 1, b"rpc"));
            l.warn(client, "over the rate");
        }
        l.forget(2);
        assert_eq!((l.chat.len(), l.motion.len(), l.used.len(), l.warned.len()), (2, 2, 2, 2));
    }

    /// Story messages: received once (an unknown one not at all), read, and both saved.
    #[test]
    fn messages_are_received_once_read_and_saved() {
        let Some((mut g, _)) = world() else { return };
        let id = g.data.assets.iter().find(|(k, _)| k.starts_with("MessageData:")).and_then(|(_, v)| v["stringId"].as_str()).unwrap().to_string();
        let h = crate::data::stable_hash(&id);
        for _ in 0..2 {
            send_scene(&mut g, 1, "MessagesHandler.AddNewReceivedMessageServerRpc", &[json!(h), json!(true)]);
        }
        send_scene(&mut g, 1, "MessagesHandler.AddNewReceivedMessageServerRpc", &[json!(12345), json!(true)]);
        assert_eq!(sent(&g, "MessagesHandler.AddNewReceivedMessageClientRpc").into_iter().map(|s| s.to).collect::<Vec<_>>(), vec![Target::All]);
        let (handler, _) = g.net.find_class("MessagesHandler").unwrap();
        let unread = [vec![0, 0, 0, 0], h.to_le_bytes().to_vec()].concat();
        assert_eq!(list(&g, handler, "MessagesHandler", "_receivedMessages").last(), Some(&unread));
        send_scene(&mut g, 1, "MessagesHandler.MarkAsReadServerRpc", &[json!([false, h])]);
        let read = [vec![1, 0, 0, 0], h.to_le_bytes().to_vec()].concat();
        assert_eq!(list(&g, handler, "MessagesHandler", "_receivedMessages").last(), Some(&read));
        let g = reload(&mut g);
        let saved: Vec<&Value> = g.save.messages.iter().filter(|m| m["stringId"] == json!(id)).collect();
        assert_eq!(saved, vec![&json!({"stringId": id, "isRead": true})]);
    }

    /// Terra tokens: gained (told to all, counted all-time), spent (not counted), never below 0.
    #[test]
    fn tokens_are_gained_spent_and_counted_all_time() {
        let Some((mut g, _)) = world() else { return };
        let tokens = |g: &crate::game::Game| {
            let (o, _) = g.net.find_class("TokensHandler").unwrap();
            crate::wire::Reader::new(&var(g, o, "TokensHandler", "_tokensNumber")).packed_u64().unwrap()
        };
        send_scene(&mut g, 1, "TokensHandler.GainTokensServerRpc", &[json!(30)]);
        send_scene(&mut g, 1, "TokensHandler.GainTokensServerRpc", &[json!(-12)]);
        assert_eq!(tokens(&g), 18);
        assert_eq!(sent(&g, "TokensHandler.GainTokensClientRpc").into_iter().map(|s| (s.to, s.args)).collect::<Vec<_>>(), vec![(Target::All, vec![json!(30)])]);
        assert_eq!((g.save.world["allTimeTerraTokens"].as_u64(), g.all_time_tokens[&1]), (Some(30), 30));
        send_scene(&mut g, 1, "TokensHandler.GainTokensServerRpc", &[json!(-1000)]);
        assert_eq!(tokens(&g), 0);
        send_scene(&mut g, 1, "TokensHandler.GainTokensServerRpc", &[json!(7)]);
        let saved = g.snapshot();
        assert_eq!(saved.world["terraTokens"], json!(7));
        assert_eq!(saved.players.iter().find(|p| p["name"] == "Ann").unwrap()["totalTerraTokenEarned"], json!(37));
    }

    /// A world object's proxy tells the asking behaviour its id and group, and its details.
    #[test]
    fn world_object_proxies_answer_with_their_object() {
        let Some((mut g, _)) = world() else { return };
        let (id, net) = place(&mut g, "EnergyGenerator1", [570.0, 3.0, 500.0]);
        let proxy = g.net.behaviour_index(net, "WorldObjectAssociatedProxy").unwrap();
        send_rpc(&mut g, 1, net, proxy, "WorldObjectAssociatedProxy.GetWorldObjectIdServerRpc", &[]);
        send_rpc(&mut g, 1, net, proxy, "WorldObjectAssociatedProxy.GetWorldObjectDetailsServerRpc", &[]);
        let generator = hash(&g, "EnergyGenerator1");
        let s = sent(&g, "WorldObjectAssociatedProxy.GetWorldObjectIdClientRpc");
        assert_eq!(s.into_iter().map(|s| (s.to, s.behaviour, s.args)).collect::<Vec<_>>(), vec![(Target::Only(1), proxy, vec![json!(id), json!(generator)])]);
        let d = one(&g, "WorldObjectAssociatedProxy.GetWorldObjectDetailsClientRpc");
        assert_eq!((&d[0], &d[1], d[3].as_array().unwrap().len(), &d[5]), (&json!(id), &json!(generator), 10, &json!(g.wos[&id].linked_inventory)));
        // An object that is no world object: no answer.
        g.net.out.clear();
        let (handler, _) = g.net.find_class("WorldObjectsHandler").unwrap();
        send_rpc(&mut g, 1, handler, 0, "WorldObjectAssociatedProxy.GetWorldObjectIdServerRpc", &[]);
        assert!(sent(&g, "WorldObjectAssociatedProxy.GetWorldObjectIdClientRpc").is_empty());
    }

    /// PlayStation Network RPCs come from consoles only: taken, nothing done.
    #[test]
    fn playstation_rpcs_do_nothing() {
        let Some((mut g, _)) = world() else { return };
        for (name, args) in [
            ("PlaystationNetworkPlayersHandler.PlayerAllowedServerRpc", vec![json!(1)]),
            ("PlaystationNetworkPlayersHandler.PlayerJoinedServerRpc", vec![json!(1)]),
            ("PlaystationNetworkPlayersHandler.SendPlayerJoinedResponseServerRpc", vec![json!(1), json!(true)]),
            ("PlaystationNetworkPlayersHandler.SendPlayerLeftServerRpc", vec![json!("x"), json!(0)]),
        ] {
            assert!(g.rpc(1, 0, 0, name, Args(args)), "{name}");
        }
        assert!(g.net.out.is_empty());
    }

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

    /// A group without a prefab of its own (FusionGenerator1, the wrecks' locked doors) is
    /// spawned in GroupNetworkContainerWithInventory: its proxy tells the linked inventory
    /// (sweep_feed: the game answered, this server did not).
    #[test]
    fn a_container_prefabs_proxy_tells_the_linked_inventory() {
        let Some(mut g) = crate::planets::tests::game() else { return };
        for group in ["FusionGenerator1", "WreckEntryLocked1"] {
            let gi = g.data.group(group).unwrap();
            let (id, net) = g.create_and_instantiate(gi, [565.0, 3.0, 500.0], [0.0, 0.0, 0.0, 1.0], false, true, false);
            let proxy = g.net.objects[&net].behaviours().find(|b| b.class == "InventoryAssociatedProxy").unwrap().index;
            g.net.out.clear();
            super::rpc(&mut g, 1, net, proxy, "InventoryAssociatedProxy.GetInventoryIdServerRpc", &Args(vec![]));
            let rpc = crate::testkit::one(&g, "InventoryAssociatedProxy.GetInventoryIdClientRpc");
            assert_ne!(g.wos[&id].linked_inventory, 0, "{group}");
            assert_eq!(rpc[2], json!(g.wos[&id].linked_inventory), "{group}");
        }
    }

    #[test]
    fn inventory_ids_answer_the_proxy_that_asked() {
        // The truck's storage proxy (its linked inventory) and its equipment's (secondary 0):
        // VehicleEquipment asks for the equipment at spawn; the storage took that answer as its own.
        let Some(mut g) = crate::planets::tests::game() else { return };
        let truck = g.data.group("VehicleTruck").unwrap();
        let (id, net) = g.create_and_instantiate(truck, [565.0, 3.0, 500.0], [0.0, 0.0, 0.0, 1.0], false, true, false);
        let expected = [g.wos[&id].linked_inventory, g.wos[&id].secondary_inventories[0]];
        let proxies: Vec<u16> = g.net.objects[&net].behaviours().filter(|b| b.class == "InventoryAssociatedProxy").map(|b| b.index).collect();
        assert_eq!(proxies.len(), 2);
        for (behaviour, inventory) in proxies.into_iter().zip(expected) {
            g.net.out.clear();
            super::rpc(&mut g, 1, net, behaviour, "InventoryAssociatedProxy.GetInventoryIdServerRpc", &Args(vec![]));
            let mut r = crate::wire::Reader::new(&g.net.out[0].2);
            let (object, to, method) = (r.packed_u64().unwrap(), r.packed_u16().unwrap(), r.packed_u32().unwrap());
            assert_eq!((object, to), (net, behaviour), "the answer goes to the asking proxy");
            assert_eq!(g.net.spec.decode(method, r.rest()).unwrap()[2], json!(inventory));
        }
    }
}
