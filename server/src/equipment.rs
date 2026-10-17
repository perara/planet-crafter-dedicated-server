//! SpaceCraft.PlayerEquipment, the server-side parts: what the equipment inventory holds.

use crate::game::Game;

fn equipped<'a>(game: &'a Game, inventory: i32, ty: &str) -> Option<&'a crate::gamedata::Group> {
    let inv = game.inventories.get(&inventory)?;
    inv.items.iter().filter_map(|i| game.wos.get(i)).map(|w| &game.data.groups[w.group]).find(|g| !g.constructible && g.str("equipableType") == ty)
}

/// GetEquipmentTier: the trailing digit of the equipped item's id (1 without one).
fn tier(game: &Game, inventory: i32, ty: &str) -> i16 {
    equipped(game, inventory, ty).map(|g| g.id.chars().last().and_then(|c| c.to_digit(10)).unwrap_or(1) as i16).unwrap_or(0)
}

/// (backpack tier, jetpack tier, skin index) for the player list.
pub fn tiers(game: &Game, equipment: i32) -> (i16, i16, i16) {
    let skin = equipped(game, equipment, "Skin").map(|g| g.fields["value"].as_i64().unwrap_or(0) as i16).unwrap_or(0);
    (tier(game, equipment, "BackpackIncrease"), tier(game, equipment, "Jetpack"), skin)
}

/// GetEquipmentValue for a client's equipment.
pub fn equipment_value(game: &Game, client: u64, ty: &str) -> i64 {
    let Some(inv) = game.clients.get(&client).and_then(|c| c.ready.as_ref()).map(|s| s.equipment) else { return 0 };
    equipped(game, inv, ty).map(|g| g.fields["value"].as_i64().unwrap_or(0)).unwrap_or(0)
}

/// Equipment changes are applied by the owning client (it asks for the inventory size and
/// gauge maxima through RPCs); nothing to do here.
pub fn inventory_changed(_game: &mut Game, _inventory: i32, _wo: i32, _added: bool) {}
