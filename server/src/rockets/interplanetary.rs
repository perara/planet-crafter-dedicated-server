//! LogisticInterplanetaryManager, server side: the inventories of every instantiated building
//! whose group takes part in interplanetary logistics, counted per item group and planet in
//! _itemsInPlanetaryLogisticsGroups (what an exchange platform can order from each planet), and
//! TransferGroups, which fetches an exchange rocket's order from another planet.

use std::collections::HashMap;

use indexmap::IndexMap;

use crate::game::Game;
use crate::model::WorldObject;
use crate::net::{ListEvent, VarValue};

const CLASS: &str = "LogisticInterplanetaryManager";
const LIST: &str = "_itemsInPlanetaryLogisticsGroups";

#[derive(Default, serde::Serialize, serde::Deserialize)]
pub struct State {
    /// _worldObjectsWithInventoriesInPlanetaryLogistics: watched inventory -> its building.
    inventories: IndexMap<i32, i32>,
    /// The group of each item in a watched inventory, for items destroyed before their removal
    /// is propagated.
    #[serde(with = "crate::handoff::pairs")]
    items: HashMap<(i32, i32), usize>,
    reset: bool,
}

/// The list starts empty on a fresh server; the scene data carries the dumped world's counts.
/// Setting it as a list also lets the core keep it without knowing its 12-byte items.
pub fn reset_list(game: &mut Game) {
    if std::mem::replace(&mut game.ext.rockets.interplanetary.reset, true) {
        return;
    }
    if let Some((object, _)) = game.net.find_class(CLASS) {
        game.net.objects.get_mut(&object).unwrap().init_list(CLASS, LIST, Vec::new());
    }
}

/// AddWorldObject, at the end of InstantiateWorldObject.
pub fn on_instantiated(game: &mut Game, wo: i32) {
    reset_list(game);
    let Some(w) = game.wos.get(&wo) else { return };
    let ty = game.data.groups[w.group].str("logisticInterplanetaryType");
    let mut watched = Vec::new();
    if ty == "EnabledOnAllInventories" {
        watched.push(w.linked_inventory);
    }
    if ty == "EnabledOnAllInventories" || ty == "EnabledOnSecondaryInventories" {
        watched.extend(&w.secondary_inventories);
    }
    for inventory in watched {
        let Some(items) = game.inventories.get(&inventory).filter(|_| inventory != 0).map(|i| i.items.clone()) else { continue };
        game.ext.rockets.interplanetary.inventories.insert(inventory, wo);
        for item in items {
            modified(game, inventory, item, true);
        }
    }
}

/// RemoveWorldObject: stops watching (the counts stay as they are, as in the game).
pub fn on_destroyed(game: &mut Game, wo: &WorldObject) {
    let s = &mut game.ext.rockets.interplanetary;
    for inventory in std::iter::once(&wo.linked_inventory).chain(&wo.secondary_inventories) {
        if s.inventories.get(inventory) == Some(&wo.id) {
            s.inventories.shift_remove(inventory);
            s.items.retain(|(i, _), _| i != inventory);
        }
    }
}

pub fn inventory_changed(game: &mut Game, inventory: i32, wo: i32, added: bool) {
    if game.ext.rockets.interplanetary.inventories.contains_key(&inventory) {
        modified(game, inventory, wo, added);
    }
}

/// Inventory.inventoryContentGrowthFinished on a watched inventory (an item grown inside it).
pub fn inventory_growth_finished(game: &mut Game, inventory: i32, wo: i32) {
    let Some(&owner) = game.ext.rockets.interplanetary.inventories.get(&inventory) else { return };
    let Some(group) = game.wos.get(&wo).map(|w| w.group) else { return };
    let g = &game.data.groups[group];
    if !g.constructible && g.str("displayInLogisticType") == "Display" {
        let planet = game.wos.get(&owner).map(|w| w.planet_hash()).unwrap_or(0);
        count(game, true, g.hash, planet);
    }
}

/// WorldObjectsHandlerPlanetaryLogisticsWatcher.OnInventoryModified.
fn modified(game: &mut Game, inventory: i32, item: i32, added: bool) {
    let s = &mut game.ext.rockets.interplanetary;
    let Some(&owner) = s.inventories.get(&inventory) else { return };
    let known = if added { None } else { s.items.remove(&(inventory, item)) };
    let Some(group) = known.or_else(|| game.wos.get(&item).map(|w| w.group)) else { return };
    if added {
        game.ext.rockets.interplanetary.items.insert((inventory, item), group);
    }
    let g = &game.data.groups[group];
    let ready = game.wos.get(&item).is_none_or(|w| !w.is_placed() || w.growth == 100.0);
    if !g.constructible && g.str("displayInLogisticType") == "Display" && ready {
        let planet = game.wos.get(&owner).map(|w| w.planet_hash()).unwrap_or(0);
        count(game, added, g.hash, planet);
    }
}

/// Group.GroupPerPlanetCountData: hash, planet hash, count (memcpy).
fn item(hash: i32, planet: i32, count: i32) -> Vec<u8> {
    [hash.to_le_bytes(), planet.to_le_bytes(), count.to_le_bytes()].concat()
}

fn field(item: &[u8], at: usize) -> i32 {
    item.get(at..at + 4).map(|b| i32::from_le_bytes(b.try_into().unwrap())).unwrap_or(0)
}

/// LogisticInterplanetaryManager.OnInventoryModified: one more or one less of a group.
fn count(game: &mut Game, added: bool, hash: i32, planet: i32) {
    let Some((object, _)) = game.net.find_class(CLASS) else { return };
    let Some(VarValue::List(items)) = game.net.get(object, CLASS, LIST) else { return };
    let event = count_event(items, added, hash, planet);
    if let Some(event) = event {
        game.net.list_events(object, CLASS, LIST, vec![event]);
    }
}

fn count_event(items: &[Vec<u8>], added: bool, hash: i32, planet: i32) -> Option<ListEvent> {
    let at = items.iter().position(|i| field(i, 0) == hash && field(i, 4) == planet);
    Some(match (at, added) {
        (None, true) => ListEvent::Add(item(hash, planet, 1)),
        (None, false) => return None,
        (Some(i), true) => ListEvent::Value(i as i32, item(hash, planet, field(&items[i], 8) + 1)),
        (Some(i), false) => match field(&items[i], 8) - 1 {
            0 => ListEvent::RemoveAt(i as i32),
            n => ListEvent::Value(i as i32, item(hash, planet, n)),
        },
    })
}

/// TransferGroupsServerRpc: `number` items of a group from the watched inventories of a planet
/// into an inventory, the last items first.
pub fn transfer_groups(game: &mut Game, group: usize, mut number: i32, source_planet: i32, destination: i32) {
    if !game.inventories.contains_key(&destination) {
        return;
    }
    let watched: Vec<(i32, i32)> = game.ext.rockets.interplanetary.inventories.iter().map(|(i, w)| (*i, *w)).collect();
    for (inventory, owner) in watched {
        if game.wos.get(&owner).map(|w| w.planet_hash()) == Some(source_planet) {
            let items = game.inventories.get(&inventory).map(|i| i.items.clone()).unwrap_or_default();
            for wo in items.into_iter().rev() {
                if let Some(placed) = game.wos.get(&wo).filter(|w| w.group == group).map(|w| w.is_placed()) {
                    if placed {
                        add_world_object_to_inventory(game, wo, destination);
                    } else {
                        game.transfer_one(inventory, destination, wo, 0);
                    }
                    number -= 1;
                }
                if number == 0 {
                    break;
                }
            }
        }
        if number == 0 {
            break;
        }
    }
}

/// InventoriesHandler.AddWorldObjectToInventory(grabbed: true) called by the server, for an item
/// that grew in place (a grower's produce).
fn add_world_object_to_inventory(game: &mut Game, wo: i32, inventory: i32) {
    if !game.inventory_add(inventory, wo, true) {
        return;
    }
    game.propagate(inventory, wo, true);
    let w = game.wos.get_mut(&wo).unwrap();
    let was_unsaved = std::mem::replace(&mut w.dont_save, false);
    game.update_save_state(wo);
    if was_unsaved {
        crate::scene::on_world_object_from_scene_retrieved(game, wo);
    }
    crate::machines::grabbed(game, wo);
    game.destroy_go_on_all_clients(wo, 0.0);
    game.dirty_inventory(inventory, 0, false);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_per_group_and_planet() {
        let mut items: Vec<Vec<u8>> = Vec::new();
        let apply = |items: &mut Vec<Vec<u8>>, e: Option<ListEvent>| match e {
            Some(ListEvent::Add(i)) => items.push(i),
            Some(ListEvent::Value(at, i)) => items[at as usize] = i,
            Some(ListEvent::RemoveAt(at)) => {
                items.remove(at as usize);
            }
            _ => {}
        };
        for (added, hash, planet) in [(true, 7, 1), (true, 7, 1), (true, 7, 2), (false, 7, 1), (false, 9, 1)] {
            let e = count_event(&items, added, hash, planet);
            apply(&mut items, e);
        }
        assert_eq!(items, vec![item(7, 1, 1), item(7, 2, 1)]);
        let e = count_event(&items, false, 7, 1);
        assert!(matches!(e, Some(ListEvent::RemoveAt(0))));
    }
}
