//! SpaceCraft.UnlockedGroupsHandler and UnlockingHandler, server side: globally unlocked groups
//! (blueprints decoded, story and message rewards). Unlocks by terraformation level are worked
//! out by every client from the units.

use serde_json::json;

use crate::game::{Args, Game};
use crate::net::{enc, Target, VarValue};

const CLASS: &str = "UnlockedGroupsHandler";

fn unlocked(game: &Game) -> Vec<i32> {
    match game.net.find_class(CLASS).and_then(|(o, _)| game.net.get(o, CLASS, "_unlockedGroups")) {
        Some(VarValue::List(items)) => items.iter().filter_map(|b| crate::wire::Reader::new(b).packed_i32()).collect(),
        _ => Vec::new(),
    }
}

pub fn is_globally_unlocked(game: &Game, group: usize) -> bool {
    unlocked(game).contains(&game.data.groups[group].hash)
}

/// UnlockingHandler.bluePrintChipGroupData.
pub fn blueprint_group(game: &Game) -> Option<usize> {
    game.data.singletons["UnlockingHandler"]["bluePrintChipGroupData"].as_str().and_then(|r| game.data.group_of_ref(r))
}

/// UnlockGroupGloballyServerRpc.
pub fn unlock_globally(game: &mut Game, hash: i32, immediate: bool, popup: bool) {
    if unlocked(game).contains(&hash) {
        return;
    }
    game.list_events_on(CLASS, "_unlockedGroups", vec![crate::net::ListEvent::Add(enc::int(hash))]);
    if popup {
        game.net.rpc_scene(Target::All, "UnlockedGroupsHandler.UnlockGroupGloballyClientRpc", &[json!(hash), json!(immediate)]);
    }
}

/// GameSettingsHandler.AddUnlockedGroups (SessionController, first planet loaded): the world
/// settings that start with blueprints unlocked. unlockedEverything and freeCraft are read by
/// the clients from the settings; the server has nothing to unlock for them.
pub fn add_unlocked_groups(game: &mut Game) {
    let s = &game.data.singletons["GameSettingsHandler"];
    let refs = |key: &str| -> Vec<serde_json::Value> { s[key].as_array().cloned().unwrap_or_else(|| vec![s[key].clone()]) };
    let mut groups = Vec::new();
    for (setting, key) in [("unlockedOreExtrators", "groupsExtractors"), ("unlockedDrones", "groupDrones"), ("unlockedAutocrafter", "groupAutocrafter"), ("unlockedTeleporters", "groupTeleporters"), ("unlockedSpaceTrading", "groupSpaceTrading")] {
        if game.save.settings[setting].as_bool() == Some(true) {
            groups.extend(refs(key).iter().filter_map(|r| game.data.group_of_ref(r.as_str()?)));
        }
    }
    for g in groups {
        let hash = game.data.groups[g].hash;
        unlock_globally(game, hash, false, false);
    }
}

/// UnlockingHandler.GetUnlockableGroup: a random still-locked group of the lowest tier left.
fn unlockable_group(game: &mut Game) -> Option<usize> {
    let data = game.data.asset(game.data.singletons["UnlockingHandler"]["unlockingData"].as_str()?)?.clone();
    for tier in 1..=10 {
        let list: Vec<Option<usize>> = data[format!("tier{tier}GroupToUnlock")].as_array().into_iter().flatten().map(|r| r.as_str().and_then(|r| game.data.group_of_ref(r))).collect();
        for _ in 0..500 {
            if list.is_empty() {
                break;
            }
            if let Some(g) = list[game.rng.range(0, list.len() as i32) as usize] {
                if !is_globally_unlocked(game, g) {
                    return Some(g);
                }
            }
        }
    }
    None
}

pub fn rpc(game: &mut Game, client: u64, name: &str, a: &Args) -> bool {
    match name {
        "UnlockedGroupsHandler.UnlockGroupGloballyServerRpc" => unlock_globally(game, a.i(0), a.b(1), a.b(2)),
        "UnlockedGroupsHandler.DecodeBlueprintServerRpc" => {
            let Some(chip) = game.wos.get(&a.i(0)).cloned() else { return true };
            let target = chip.linked_groups.as_ref().and_then(|l| l.first().copied()).or_else(|| {
                let unlocks = game.data.groups[chip.group].str("unlocksGroup").to_string();
                game.data.group_of_ref(&unlocks)
            });
            let target = match target {
                Some(t) => Some(t),
                None => unlockable_group(game),
            };
            match target {
                Some(g) => {
                    game.net.rpc_scene(Target::Only(client), "UnlockedGroupsHandler.PlayAudioOnDecodeClientRpc", &[]);
                    game.destroy_world_object(chip.id, false);
                    let hash = game.data.groups[g].hash;
                    unlock_globally(game, hash, true, true);
                }
                None => game.net.rpc_scene(Target::Only(client), "UnlockedGroupsHandler.NoMoreChipToUnlockClientRpc", &[]),
            }
        }
        _ => return false,
    }
    true
}
