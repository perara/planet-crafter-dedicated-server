//! Scene loot, server side: InventoryFromScene.GetGeneratedGroups with InventoryLootHandler,
//! InventoryLootStage and InventoryLootDependingOnGameSettings, for loot containers the server
//! instantiates at runtime (InventoryAssociated.PopulateRandomInventory: wreck safes, golden
//! containers...). Containers placed in a planet's scene are filled by the client that opens
//! them first: it rolls the loot and sends it with GetOrCreateNewInventoryServerRpc, which the
//! core handles.

use serde_json::Value;

use crate::game::Game;
use crate::net::VarValue;

/// InventoryLootStage's chances (percent) per tier, rarest first, and the boosted multiplier.
const TIERS: [(&str, f32); 4] = [("ultraRareItems", 2.5), ("veryRareItems", 8.0), ("rareItems", 25.0), ("unCommonItems", 40.0)];
const BOOSTED_MULTIPLIER: i32 = 9;
const MAX_INSTANCE_LOOT: i32 = 3;

fn components<'a>(game: &'a Game, group: &str, ty: &str) -> Vec<&'a Value> {
    game.data.group_prefabs[group]["components"].as_array().into_iter().flatten().filter(|c| c["type"] == ty).collect()
}

fn int(v: &Value) -> i32 {
    v.as_i64().unwrap_or(0) as i32
}

fn groups(game: &Game, refs: &Value) -> Vec<usize> {
    refs.as_array().into_iter().flatten().filter_map(|r| game.data.group_of_ref(r.as_str()?)).collect()
}

/// InventoryAssociated.PopulateRandomInventory, from WorldObjectsHandler.InstantiateWorldObject:
/// a container-prefab object made at runtime with InventoryFromScene loot gets it.
pub fn populate_random_inventory(game: &mut Game, wo: i32) {
    let Some(w) = game.wos.get(&wo) else { return };
    let (inventory, id) = (w.linked_inventory, game.data.groups[w.group].id.clone());
    let container = game.data.layouts.get(&id).is_some_and(|l| l.prefab.starts_with("GroupNetworkContainer"));
    if w.from_db || inventory == 0 || !container {
        return;
    }
    let Some(scene) = components(game, &id, "InventoryFromScene").first().map(|c| (*c).clone()) else { return };
    let settings = components(game, &id, "InventoryLootDependingOnGameSettings").into_iter().find(|c| c["path"] == scene["path"]).map(|c| c["fields"].clone());
    for group in generated_groups(game, &scene["fields"], settings.as_ref()) {
        game.add_new_item(group, inventory, 0);
    }
}

/// InventoryFromScene.GetGroupQuantity: food comes once per player in the game.
fn quantity(game: &mut Game, group: usize) -> i32 {
    let players = match game.net.find_class("PlayersDataManager").and_then(|(o, _)| game.net.get(o, "PlayersDataManager", "_playersData")) {
        Some(VarValue::List(items)) => items.len() as i32,
        _ => 0,
    };
    let g = &game.data.groups[group];
    if players > 1 && !g.constructible && g.usable_type() == "Eatable" { game.rng.range(1, players + 1) } else { 1 }
}

/// InventoryFromScene.GetGeneratedGroups.
pub fn generated_groups(game: &mut Game, f: &Value, settings: Option<&Value>) -> Vec<usize> {
    let boosted = f["boostedLoot"].as_bool().unwrap_or(false);
    let mut list = Vec::new();
    for g in groups(game, &f["everyTimeGroups"]) {
        for _ in 0..quantity(game, g) {
            list.push(g);
        }
    }
    let chance = int(&f["chanceForEachGroup"]);
    for g in groups(game, &f["someTimesGroups"]) {
        if chance > game.rng.range(0, 100) {
            for _ in 0..quantity(game, g) {
                list.push(g);
            }
        }
    }
    if let Some(table) = f["additiveLootTable"].as_str() {
        let mut n = int(&f["additiveLootNumber"]);
        if game.rng.range(0, 100) > 50 {
            n += 2;
        }
        if game.rng.range(0, 100) > 50 {
            n -= 2;
        }
        for _ in 0..n {
            list.extend(stage_loot(game, table, if boosted { BOOSTED_MULTIPLIER } else { 1 }));
        }
    }
    let mut terra = int(&f["terraStageLootAdded"]);
    if f["hasWorldInstanceLoot"].as_bool() == Some(true) {
        if let Some(rarity) = crate::instances::opened_loot_rarity(game) {
            let level = rarity + if boosted { 2 } else { 0 };
            let n = game.rng.range(1, MAX_INSTANCE_LOOT + 1);
            let table = game.data.singletons["InventoryLootHandler"]["lootWorldInstances"].as_str().unwrap_or("").to_string();
            for _ in 0..n {
                list.extend(stage_loot(game, &table, level));
            }
            terra -= n;
        }
    }
    if terra > 0 {
        if game.rng.range(0, 100) > 50 {
            terra += 2;
        }
        if game.rng.range(0, 100) > 50 {
            terra -= 2;
        }
        for _ in 0..terra {
            list.extend(terra_stage_loot(game, boosted));
        }
    }
    // Blueprints of what everyone has unlocked already are left out.
    list.retain(|g| {
        let group = &game.data.groups[*g];
        let unlocks = (!group.constructible).then(|| group.fields["unlocksGroup"].as_str().and_then(|r| game.data.group_of_ref(r))).flatten();
        !unlocks.is_some_and(|u| crate::unlocks::is_globally_unlocked(game, u))
    });
    let mut size = int(&f["size"]);
    if let Some(s) = settings.filter(|s| s["activateOnRandomizeOres"].as_bool() == Some(true) && game.save.settings["randomizeMineables"].as_bool() == Some(true)) {
        size += int(&s["increaseSize"]);
        let chance = int(&s["chanceForEachGroup"]);
        for g in groups(game, &s["groupsToAddRandomly"]) {
            if chance > game.rng.range(0, 100) {
                list.push(g);
            }
        }
    }
    // Rules: more or less of the same loot, up to the container's size.
    let amount = game.rules.loot.amount;
    if amount != 1.0 && !list.is_empty() {
        let n = (list.len() as f32 * amount).round() as usize;
        list = list.iter().copied().cycle().take(n).collect();
    }
    list.truncate(if size == 0 { 10 } else { size }.max(0) as usize);
    list
}

/// InventoryLootStage.GetLoot(boostedMultiplier): a tier by chance, then an item of it.
fn stage_loot(game: &mut Game, stage: &str, multiplier: i32) -> Option<usize> {
    let stage = game.data.asset(stage)?.clone();
    // GetLoot draws again for items the game does not know; every item here is known.
    let roll = game.rng.rangef(0.0, 100.0) / ((multiplier.max(1) + 2) / 3) as f32;
    let tier = TIERS.iter().find(|(_, chance)| roll < *chance).map(|(t, _)| *t).unwrap_or("commonItems");
    let items = stage[tier].as_array()?;
    if items.is_empty() {
        return None;
    }
    let item = &items[game.rng.range(0, items.len() as i32) as usize];
    game.data.group_of_ref(item.as_str()?)
}

/// WorldUnitSystemTerraformation.GetValue: the planets' terraformation multiplied together.
fn system_terraformation(game: &Game) -> f64 {
    let mut total = 0.0f64;
    for p in &game.units.planets {
        let v = p.get(crate::units::Kind::Terraformation).total as f64;
        total = if total < f64::EPSILON { v } else { total * v.max(1.0) };
    }
    total
}

/// InventoryLootHandler.GetLootDepedingOnTerraStage: the loot table of the last stage the
/// system terraformation has reached.
fn terra_stage_loot(game: &mut Game, boosted: bool) -> Option<usize> {
    if game.units.planets.is_empty() {
        return None;
    }
    let value = system_terraformation(game);
    let mut reached = None;
    for s in game.data.singletons["InventoryLootHandler"]["lootTerraStages"].as_array().into_iter().flatten().filter_map(Value::as_str) {
        let start = game.data.asset(s).and_then(|l| l["terraStage"].as_str()).and_then(|t| game.data.asset(t)).and_then(|t| t["startValue"].as_f64());
        if !(value < 0.0 || start.is_none_or(|v| value >= v)) {
            break;
        }
        reached = Some(s.to_string());
    }
    stage_loot(game, &reached?, if boosted { BOOSTED_MULTIPLIER } else { 1 })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn scene_loot_keeps_the_sure_items_and_fits_the_container() {
        let Some(mut g) = crate::planets::tests::game() else { return };
        let f = json!({"size": 3, "everyTimeGroups": ["GroupDataItem:PulsarQuartz", "GroupDataItem:PulsarQuartz"], "chanceForEachGroup": 100,
                       "someTimesGroups": ["GroupDataItem:Minable-Iron"], "terraStageLootAdded": 30, "boostedLoot": true, "additiveLootTable": null,
                       "additiveLootNumber": 0, "hasWorldInstanceLoot": false});
        let quartz = g.data.group("PulsarQuartz").unwrap();
        for _ in 0..20 {
            let loot = generated_groups(&mut g, &f, None);
            assert_eq!(loot.len(), 3, "terra-stage loot fills up to the size");
            assert_eq!(&loot[..2], &[quartz, quartz]);
            assert_eq!(loot[2], g.data.group("Iron").unwrap(), "a 100% chance always drops");
        }
    }

    #[test]
    fn terra_stage_loot_comes_from_the_first_stage_on_a_barren_world() {
        let Some(mut g) = crate::planets::tests::game() else { return };
        let stage0 = g.data.asset("InventoryLootStage:LootStage0").unwrap().clone();
        let known: Vec<usize> = ["commonItems", "unCommonItems", "rareItems", "veryRareItems", "ultraRareItems"].iter().flat_map(|t| groups(&g, &stage0[*t])).collect();
        for _ in 0..50 {
            if let Some(item) = terra_stage_loot(&mut g, true) {
                assert!(known.contains(&item));
            }
        }
    }

    #[test]
    fn runtime_wreck_safes_are_filled_on_instantiation() {
        let Some(mut g) = crate::planets::tests::game() else { return };
        let safe = g.data.group("ProceduralWreckSafe").unwrap();
        let id = g.new_world_object_id();
        g.create_world_object(safe, id, true);
        let planet = g.planet_hash();
        let w = g.wos.get_mut(&id).unwrap();
        w.position = [600.0, 5.0, 500.0];
        w.planet = planet;
        g.instantiate(id, false, 0);
        let inventory = g.wos[&id].linked_inventory;
        assert!(!g.inventories[&inventory].items.is_empty(), "8 terra-stage items at least");
    }
}
