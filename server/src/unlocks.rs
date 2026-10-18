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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::testkit::*;

    #[test]
    fn groups_unlocked_globally_are_everyones_and_saved() {
        let Some((mut g, _)) = world() else { return };
        let drone = hash(&g, "DroneStation1");
        send_scene(&mut g, 1, "UnlockedGroupsHandler.UnlockGroupGloballyServerRpc", &[json!(drone), json!(true), json!(true)]);
        send_scene(&mut g, 1, "UnlockedGroupsHandler.UnlockGroupGloballyServerRpc", &[json!(drone), json!(true), json!(true)]);
        assert_eq!(unlocked(&g).iter().filter(|h| **h == drone).count(), 1, "once");
        let popups = sent(&g, "UnlockedGroupsHandler.UnlockGroupGloballyClientRpc");
        assert_eq!(popups.iter().map(|s| (s.to, s.args.clone())).collect::<Vec<_>>(), vec![(Target::All, vec![json!(drone), json!(true)])]);
        // Without the popup: unlocked, nobody told.
        let teleporter = hash(&g, "Teleporter1");
        g.net.out.clear();
        send_scene(&mut g, 1, "UnlockedGroupsHandler.UnlockGroupGloballyServerRpc", &[json!(teleporter), json!(false), json!(false)]);
        assert!(sent(&g, "UnlockedGroupsHandler.UnlockGroupGloballyClientRpc").is_empty());
        let mut g = reload(&mut g);
        for group in ["DroneStation1", "Teleporter1"] {
            assert!(is_globally_unlocked(&g, g.data.group(group).unwrap()), "{group} after a reload");
        }
        assert!(g.snapshot().world["unlockedGroups"].as_str().unwrap().contains("DroneStation1"));
    }

    /// A blueprint chip decodes into the group it names, else a random locked group of the
    /// lowest tier left; the chip is used up. With everything unlocked, the client hears so.
    #[test]
    fn blueprint_chips_decode_into_a_locked_group() {
        let Some((mut g, _)) = world() else { return };
        let (bag, _) = backpack(&g, 1);
        let chip_group = blueprint_group(&g).expect("the blueprint chip group");
        let chip_id = g.data.groups[chip_group].id.clone();
        let chip = give(&mut g, &chip_id, bag);
        let autocrafter = g.data.group("AutoCrafter1").unwrap();
        g.wos.get_mut(&chip).unwrap().linked_groups = Some(vec![autocrafter]);
        send_scene(&mut g, 1, "UnlockedGroupsHandler.DecodeBlueprintServerRpc", &[json!(chip)]);
        assert!(is_globally_unlocked(&g, autocrafter));
        assert!(!g.wos.contains_key(&chip) && !g.inventories[&bag].items.contains(&chip), "used up");
        assert_eq!(one(&g, "UnlockedGroupsHandler.PlayAudioOnDecodeClientRpc"), Vec::<serde_json::Value>::new());
        assert_eq!(one(&g, "UnlockedGroupsHandler.UnlockGroupGloballyClientRpc"), vec![json!(g.data.groups[autocrafter].hash), json!(true)]);

        // An unlinked chip: a random group of the first tier that still has a locked one.
        let before = unlocked(&g).len();
        let chip = give(&mut g, &chip_id, bag);
        send_scene(&mut g, 1, "UnlockedGroupsHandler.DecodeBlueprintServerRpc", &[json!(chip)]);
        assert_eq!(unlocked(&g).len(), before + 1);
        // Unknown chips: nothing.
        g.net.out.clear();
        send_scene(&mut g, 1, "UnlockedGroupsHandler.DecodeBlueprintServerRpc", &[json!(-3)]);
        assert!(g.net.out.is_empty());
        // Every tier's groups unlocked: no more chip to unlock.
        let data = g.data.asset(g.data.singletons["UnlockingHandler"]["unlockingData"].as_str().unwrap()).unwrap().clone();
        for tier in 1..=10 {
            for r in data[format!("tier{tier}GroupToUnlock")].as_array().into_iter().flatten() {
                if let Some(group) = r.as_str().and_then(|r| g.data.group_of_ref(r)) {
                    let h = g.data.groups[group].hash;
                    unlock_globally(&mut g, h, false, false);
                }
            }
        }
        let chip = give(&mut g, &chip_id, bag);
        g.net.out.clear();
        send_scene(&mut g, 1, "UnlockedGroupsHandler.DecodeBlueprintServerRpc", &[json!(chip)]);
        assert_eq!(sent(&g, "UnlockedGroupsHandler.NoMoreChipToUnlockClientRpc").len(), 1);
        assert!(g.wos.contains_key(&chip), "kept");
    }
}
