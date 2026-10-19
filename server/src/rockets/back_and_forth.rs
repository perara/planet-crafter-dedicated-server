//! MachineRocketBackAndForth with its Trade and InterplanetaryExchange kinds, server side. The
//! platform's rocket leaves at growth 1, climbs one growth step every `updateGrowthEvery`
//! seconds (divided by the optimizers' speed multiplier), does its task at step 98 (sell and
//! buy, or deliver and fetch), lands from step 99 (MachineRocketLand) and is back on site at
//! growth 0. GrowthProxy carries the growth (the world object's) to the clients.

use serde_json::json;

use crate::game::{Args, Game};
use crate::net::{enc, Target, VarValue};

/// MachineRocketBackAndForth._yAxisOfSky: the returning rocket starts this high above its pad.
const SKY: f32 = 500.0;
/// MachineRocketLand._distanceToDecelerate.
const DECELERATE: f32 = 25.0;
/// MachineRocketBackAndForthInterplanetaryExchange.RelaunchRocket's wait.
const RELAUNCH_AFTER: f32 = 5.0;

#[derive(Clone, Copy, PartialEq, Debug, serde::Serialize, serde::Deserialize)]
pub enum Kind {
    Trade,
    Exchange,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct Machine {
    kind: Kind,
    /// The network object (the component lives and dies with it).
    net: u64,
    update_every: f32,
    /// MachineRocketLand.speed.
    landing_speed: f32,
    /// _speedMultiplier.
    speed_multiplier: f32,
    /// The running UpdateGrowth coroutine: its wait (fixed when it starts) and next wake-up.
    period: f32,
    next_update: Option<f32>,
    removed_from_logistics: bool,
    /// MachineRocketLand while it runs: the rocket's height above its landing spot.
    landing: Option<f32>,
    /// The GameObject is active (its world object is on the current planet).
    active: bool,
    /// RelaunchRocket coroutine (exchange).
    relaunch_at: Option<f32>,
}

fn kind_of(game: &Game, group: usize) -> Option<(Kind, f32)> {
    let id = &game.data.groups[group].id;
    [(Kind::Trade, "MachineRocketBackAndForthTrade"), (Kind::Exchange, "MachineRocketBackAndForthInterplanetaryExchange")]
        .into_iter()
        .find_map(|(kind, ty)| game.data.prefab_components(id, ty).first().map(|c| (kind, c["updateGrowthEvery"].as_f64().unwrap_or(6.0) as f32)))
}

fn machine(game: &mut Game, wo: i32) -> Option<&mut Machine> {
    game.ext.rockets.machines.get_mut(&wo)
}

fn growth(game: &Game, wo: i32) -> f32 {
    game.wos.get(&wo).map(|w| w.growth).unwrap_or(0.0)
}

fn on_current_planet(game: &Game, wo: i32) -> bool {
    game.wos.get(&wo).is_some_and(|w| w.planet_hash() == game.planet_hash())
}

fn inventory_full(game: &Game, wo: i32) -> bool {
    game.wos.get(&wo).and_then(|w| game.inventories.get(&w.linked_inventory)).is_some_and(|i| i.is_full())
}

/// GrowthProxy.SetGrowth on the server: 0 and forced values go to every client
/// (ResetCurrentGrowthServerRpc run by the server itself).
fn set_growth(game: &mut Game, wo: i32, value: f32, force: bool) {
    let Some(w) = game.wos.get_mut(&wo) else { return };
    w.growth = value;
    if let (true, Some(net)) = (value == 0.0 || force, w.net) {
        game.net.rpc(Target::All, net, "GrowthProxy.SetCurrentGrowthClientRpc", &[json!(value)]);
    }
}

/// StartGrowth: (re)starts the UpdateGrowth coroutine.
fn start_growth(game: &mut Game, wo: i32) {
    let now = game.time;
    if let Some(m) = machine(game, wo) {
        m.period = m.update_every / m.speed_multiplier;
        m.next_update = Some(now + m.period);
    }
}

fn rocket_has_landed(game: &mut Game, wo: i32) {
    set_growth(game, wo, 0.0, true);
}

/// The world object got its network object: Awake, the GrowthProxy init callback, OnEnable,
/// then the GameObject deactivated if it stands on another planet.
pub fn spawned(game: &mut Game, wo: i32) {
    let Some((group, net)) = game.wos.get(&wo).and_then(|w| Some((w.group, w.net?))) else { return };
    let Some((kind, update_every)) = kind_of(game, group) else { return };
    let landing_speed = super::component_f32(game, group, "MachineRocketLand", "speed", 20.0);
    let m = Machine { kind, net, update_every, landing_speed, speed_multiplier: 1.0, period: update_every, next_update: None, removed_from_logistics: false, landing: None, active: true, relaunch_at: None };
    game.ext.rockets.machines.insert(wo, m);
    if growth(game, wo) >= 100.0 {
        set_growth(game, wo, 0.0, false);
    }
    if kind == Kind::Trade {
        game.set_on("TokensHandler", "_isTradingActivated", enc::bool(true));
    }
    on_enable(game, wo);
    if !on_current_planet(game, wo) {
        machine(game, wo).unwrap().active = false;
        on_disable(game, wo);
    }
}

/// OnEnable: the rocket on its pad or hidden up there (either way no landing runs), the
/// coroutine restarted, and an automatic send if the inventory is full.
fn on_enable(game: &mut Game, wo: i32) {
    if let Some(m) = machine(game, wo) {
        m.landing = None;
    }
    start_growth(game, wo);
    inventory_modified(game, wo);
}

/// OnDisable: a landing rocket counts as landed.
fn on_disable(game: &mut Game, wo: i32) {
    if let Some(m) = machine(game, wo) {
        m.landing = None;
    }
    if growth(game, wo) == 100.0 {
        rocket_has_landed(game, wo);
    }
}

/// OnRocketBackAndForthInventoryModified: setting 1 sends the rocket once the inventory is full.
fn inventory_modified(game: &mut Game, wo: i32) {
    if game.wos.get(&wo).is_some_and(|w| w.setting == 1) && inventory_full(game, wo) {
        send_rocket(game, wo, None);
    }
}

pub fn inventory_changed(game: &mut Game, inventory: i32) {
    let owner = game.ext.rockets.machines.keys().copied().find(|m| game.wos.get(m).is_some_and(|w| w.linked_inventory == inventory));
    if let Some(wo) = owner {
        inventory_modified(game, wo);
    }
}

/// SendRocketBackAndForthFromProxy on the server.
fn send(game: &mut Game, wo: i32) -> bool {
    let Some(kind) = machine(game, wo).map(|m| m.kind) else { return false };
    let can_be_sent = kind != Kind::Exchange || game.wos.get(&wo).is_some_and(|w| w.planet_linked != 0);
    if !can_be_sent || growth(game, wo) != 0.0 {
        return false;
    }
    // ProcessRocketSent, the ignition, camera shake and window closing are client side.
    set_growth(game, wo, 1.0, true);
    start_growth(game, wo);
    true
}

/// ConstructibleProxy.SendInSpaceRocketBackAndForthServerRpc, from a client or (None) from the
/// server itself (SendBackAndForthRocket), whose error reply reaches nobody.
fn send_rocket(game: &mut Game, wo: i32, client: Option<u64>) {
    let Some(net) = machine(game, wo).map(|m| m.net) else { return };
    if send(game, wo) {
        game.net.rpc(Target::All, net, "ConstructibleProxy.SendInSpaceRocketBackAndForthClientRpc", &[]);
    } else if let Some(client) = client {
        game.net.rpc(Target::Only(client), net, "ConstructibleProxy.SendInSpaceRocketBackAndForthDisplayErrorClientRpc", &[]);
    }
}

/// AddToRocketBackAndForthSpeedMultiplier (MachineOptimizer's fuses).
pub fn add_speed_multiplier(game: &mut Game, wo: i32, delta: f32) {
    let Some(m) = machine(game, wo) else { return };
    m.speed_multiplier += delta;
    let (net, multiplier) = (m.net, m.speed_multiplier);
    game.net.rpc(Target::All, net, "ConstructibleProxy.SetRocketBackAndForthSpeedMultiplierClientRpc", &[json!(multiplier)]);
    let g = growth(game, wo);
    set_growth(game, wo, g, true);
    start_growth(game, wo);
}

/// The ConstructibleProxy and GrowthProxy RPCs of a back-and-forth platform.
pub fn rpc(game: &mut Game, client: u64, object: u64, name: &str, a: &Args) -> bool {
    let wo = super::wo_of(game, object);
    let mine = wo.is_some_and(|w| game.ext.rockets.machines.contains_key(&w));
    let wo = wo.unwrap_or(0);
    match name {
        // Without the component the game throws: no reply.
        "ConstructibleProxy.SendInSpaceRocketBackAndForthServerRpc" => {
            if mine {
                send_rocket(game, wo, Some(client));
            }
        }
        "ConstructibleProxy.RetrieveRocketBackAndForthSpeedMultiplierServerRpc" => {
            if let Some(multiplier) = machine(game, wo).map(|m| m.speed_multiplier) {
                game.net.rpc(Target::Only(client), object, "ConstructibleProxy.SetRocketBackAndForthSpeedMultiplierClientRpc", &[json!(multiplier)]);
            }
        }
        "GrowthProxy.GetCurrentGrowthServerRpc" if mine => {
            let g = growth(game, wo);
            game.net.rpc(Target::Only(client), object, "GrowthProxy.SetCurrentGrowthClientRpc", &[json!(g)]);
        }
        "GrowthProxy.ResetCurrentGrowthServerRpc" if mine => {
            let value = a.f(0);
            game.wos.get_mut(&wo).unwrap().growth = value;
            game.net.rpc(Target::All, object, "GrowthProxy.SetCurrentGrowthClientRpc", &[json!(value)]);
        }
        _ => return false,
    }
    true
}

/// One frame of MachineRocketLand.UpdatePosition, along the vertical: the rocket's next height
/// above its target, or None once it has landed.
pub fn landing_step(height: f32, speed: f32, dt: f32) -> Option<f32> {
    let slow = (height.abs() / DECELERATE).clamp(0.0, 1.0);
    let step = speed * slow * dt;
    let next = if height.abs() <= step { 0.0 } else { height - step.copysign(height) };
    if height < 0.1 || (next - height) * (next - height) < 1.192_092_9e-7 { None } else { Some(next) }
}

pub fn tick(game: &mut Game, dt: f32) {
    let now = game.time;
    for wo in game.ext.rockets.machines.keys().copied().collect::<Vec<_>>() {
        let Some(m) = game.ext.rockets.machines.get(&wo) else { continue };
        // The GameObject went (destroyed, or despawned with its planet): its coroutines too.
        if !game.wos.get(&wo).is_some_and(|w| w.net == Some(m.net)) || !game.net.objects.contains_key(&m.net) {
            game.ext.rockets.machines.shift_remove(&wo);
            continue;
        }
        let active = on_current_planet(game, wo);
        if active != m.active {
            machine(game, wo).unwrap().active = active;
            if active { on_enable(game, wo) } else { on_disable(game, wo) }
        }
        // Update (MachineRocketLand), then the coroutines, as Unity orders a frame.
        if let Some(m) = machine(game, wo).filter(|_| dt > 0.0) {
            if let Some(height) = m.landing {
                m.landing = landing_step(height, m.landing_speed, dt);
                if m.landing.is_none() {
                    rocket_has_landed(game, wo);
                }
            }
        }
        if machine(game, wo).and_then(|m| m.relaunch_at).is_some_and(|t| now >= t) {
            machine(game, wo).unwrap().relaunch_at = None;
            if inventory_full(game, wo) && game.wos.get(&wo).is_some_and(|w| w.setting == 1) {
                send_rocket(game, wo, None);
            }
        }
        if let Some(m) = machine(game, wo).filter(|m| m.next_update.is_some_and(|t| now >= t)) {
            m.next_update = Some(now + m.period);
            update_growth(game, wo);
        }
    }
}

/// One wake-up of the UpdateGrowth coroutine on the server.
fn update_growth(game: &mut Game, wo: i32) {
    let inventory = game.wos[&wo].linked_inventory;
    if !game.inventories.contains_key(&inventory) {
        return;
    }
    // ponytail: LogisticManager.GetIsInited() is taken as true (the logistics port keeps no such
    // flag); it only delays the first steps after a load.
    let mut num = growth(game, wo);
    if num == 0.0 {
        let m = machine(game, wo).unwrap();
        m.next_update = None;
        if m.removed_from_logistics {
            m.removed_from_logistics = false;
            // LogisticManager.SetInventoryStatusInLogistics.
            crate::logistics::set_dirty(game, inventory);
            process_rocket_landed(game, wo);
        }
        return;
    }
    if num == 98.0 {
        process_rocket_task(game, wo);
        set_growth(game, wo, 99.0, false);
    } else if num == 99.0 {
        let active = machine(game, wo).is_some_and(|m| m.active);
        if active {
            machine(game, wo).unwrap().landing = Some(SKY);
        } else {
            rocket_has_landed(game, wo);
            num = 0.0;
        }
    }
    if num > 0.0 && num < 100.0 {
        set_growth(game, wo, num + 1.0, false);
    }
    if num > 0.0 && machine(game, wo).is_some_and(|m| !m.removed_from_logistics) {
        machine(game, wo).unwrap().removed_from_logistics = true;
        // LogisticManager.RemoveInventoryFromLogistics.
        crate::logistics::remove_inventory(game, inventory);
    }
}

fn process_rocket_task(game: &mut Game, wo: i32) {
    match machine(game, wo).map(|m| m.kind) {
        Some(Kind::Trade) => trade(game, wo),
        Some(Kind::Exchange) => exchange(game, wo),
        None => {}
    }
}

/// ProcessRocketLanded: a full exchange rocket set to automatic goes again 5 s later.
fn process_rocket_landed(game: &mut Game, wo: i32) {
    let now = game.time;
    let again = inventory_full(game, wo) && game.wos.get(&wo).is_some_and(|w| w.setting == 1);
    if let Some(m) = machine(game, wo).filter(|m| m.kind == Kind::Exchange && again) {
        m.relaunch_at = Some(now + RELAUNCH_AFTER);
    }
}

fn tokens(game: &Game) -> u64 {
    match game.net.find_class("TokensHandler").and_then(|(o, _)| game.net.get(o, "TokensHandler", "_tokensNumber")) {
        Some(VarValue::Plain(b)) => crate::wire::Reader::new(b).packed_u64().unwrap_or(0),
        _ => 0,
    }
}

fn trade_value(game: &Game, group: usize) -> i32 {
    game.data.groups[group].fields["tradeValue"].as_i64().unwrap_or(0) as i32
}

/// LinkedGroupsProxy.SetLinkedGroups(null) and SettingProxy.SetSetting from the server.
fn clear_linked_groups(game: &mut Game, wo: i32) {
    let Some(w) = game.wos.get_mut(&wo) else { return };
    w.linked_groups = Some(Vec::new());
    if let Some(net) = w.net {
        crate::handlers::set_linked_groups(game, net, &[]);
    }
}

fn set_setting(game: &mut Game, wo: i32, setting: i32) {
    let Some(w) = game.wos.get_mut(&wo) else { return };
    w.setting = setting;
    if let Some(net) = w.net {
        game.net.set(net, "SettingProxy", "_setting", enc::int(setting));
    }
}

/// InventoriesHandler.RemoveItemsFromInventory(groups, destroy: true) called by the server: the
/// last item of each group.
fn remove_items_of_groups(game: &mut Game, inventory: i32, groups: &[usize]) {
    let mut removed = Vec::new();
    for g in groups {
        let Some(inv) = game.inventories.get(&inventory) else { return };
        if let Some(at) = inv.items.iter().rposition(|i| game.wos.get(i).is_some_and(|w| w.group == *g)) {
            let item = game.inventories.get_mut(&inventory).unwrap().items.remove(at);
            game.release(item);
            removed.push(item);
        }
    }
    game.destroy_world_objects(&removed, false);
    for item in &removed {
        game.propagate(inventory, *item, false);
    }
    game.dirty_inventory(inventory, 0, false);
}

/// MachineRocketBackAndForthTrade.ProcessRocketTask: everything destructible inside is sold,
/// then the ordered groups (the linked groups) are bought while the tokens last.
fn trade(game: &mut Game, wo: i32) {
    let inventory = game.wos[&wo].linked_inventory;
    let items = game.inventories.get(&inventory).map(|i| i.items.clone()).unwrap_or_default();
    let (mut value, mut sold) = (0i32, Vec::new());
    for item in items {
        let Some(group) = game.wos.get(&item).map(|w| w.group) else { continue };
        let g = &game.data.groups[group];
        if g.constructible || !g.bool("cantBeDestroyed") {
            value = value.wrapping_add(trade_value(game, group));
            sold.push(group);
        }
    }
    remove_items_of_groups(game, inventory, &sold);
    crate::handlers::gain_tokens(game, value);
    for group in game.wos[&wo].linked_groups.clone().unwrap_or_default() {
        let price = trade_value(game, group);
        if price as u64 > tokens(game) {
            break;
        }
        // The server's AddItemToInventory callback runs at once: LoseTokens.
        if game.add_new_item(group, inventory, 0).0 {
            let left = tokens(game).wrapping_sub(price as u64);
            game.set_on("TokensHandler", "_tokensNumber", enc::ulong(left));
        }
    }
    clear_linked_groups(game, wo);
    if game.inventories.get(&inventory).is_some_and(|i| !i.items.is_empty()) {
        set_setting(game, wo, 0);
    }
}

/// InventoriesHandler.TransferAllSameGroup called by the server.
fn transfer_all_same_group(game: &mut Game, from: i32, to: i32, group: usize) {
    if !game.inventories.contains_key(&to) {
        return;
    }
    let Some(items) = game.inventories.get(&from).map(|i| i.items.clone()) else { return };
    let time = game.time;
    let mut moved = Vec::new();
    for item in items.into_iter().rev() {
        let fits = game.wos.get(&item).is_some_and(|w| w.group == group && !w.is_locked_in_inventory(time));
        if fits && game.inventory_add(to, item, true) {
            game.inventory_remove(from, item);
            moved.push(item);
        }
    }
    if moved.is_empty() {
        return;
    }
    for item in moved {
        game.propagate(from, item, false);
        game.propagate(to, item, true);
    }
    game.dirty_inventory(from, 0, true);
    game.dirty_inventory(to, 0, true);
}

/// MachineRocketBackAndForthInterplanetaryExchange.ProcessRocketTask: the cargo goes to the
/// linked planet's delivery depots (by their selected group), the order (the linked groups)
/// comes from that planet's interplanetary logistics.
fn exchange(game: &mut Game, wo: i32) {
    let Some(group) = game.wos.get(&wo).map(|w| w.group) else { return };
    let (inventory, planet) = (game.wos[&wo].linked_inventory, game.wos[&wo].planet_linked);
    let depot = game.data.prefab_components(&game.data.groups[group].id, "MachineRocketBackAndForthInterplanetaryExchange").first().and_then(|c| c["_interplanetaryDeliveryDepot"].as_str()).and_then(|r| game.data.group_of_ref(r));
    let has_cargo = |game: &Game| game.inventories.get(&inventory).is_some_and(|i| !i.items.is_empty());
    if let (true, Some(depot)) = (has_cargo(game), depot) {
        let depots: Vec<(i32, usize)> = game
            .wos
            .values()
            .filter(|d| d.group == depot && (planet == 0 || d.planet_hash() == planet))
            .filter_map(|d| Some((d.linked_inventory, *d.linked_groups.as_ref()?.first()?)))
            .collect();
        for (depot_inventory, selected) in depots {
            transfer_all_same_group(game, inventory, depot_inventory, selected);
            if !has_cargo(game) {
                break;
            }
        }
    }
    let mut order: Vec<(usize, i32)> = Vec::new();
    for g in game.wos[&wo].linked_groups.clone().unwrap_or_default() {
        match order.iter_mut().find(|(o, _)| *o == g) {
            Some((_, n)) => *n += 1,
            None => order.push((g, 1)),
        }
    }
    clear_linked_groups(game, wo);
    for (g, n) in order {
        super::interplanetary::transfer_groups(game, g, n, planet, inventory);
    }
    // The game then tests the linked groups it has just cleared, so it never resets the setting.
}
