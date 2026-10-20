//! The facts about one game version that the server's code holds in source: the groups it looks
//! up by name, the RPCs a loading client may send, the classes of a rocket platform, the
//! animals' prefabs. They are checked against the tables when the server loads them, so a game
//! update that renames or drops one stops the start with its name, instead of a panic or a
//! silent miss when the world first needs it.
//!
//! A group looked up by name (`data.group("..")`) goes in REQUIRED_GROUPS (a test fails for one
//! that does not); the other lists are the constants of their own modules.

use crate::gamedata::GameData;
use crate::rpcspec::Spec;

/// Groups the code looks up by name outside the tests (the space multipliers, which it looks up
/// by the ids of rockets::SPACE_MULTIPLIERS, are checked too).
pub const REQUIRED_GROUPS: &[&str] = &[
    // animals/spawn.rs: the animals' DNA, and the traits on it
    "DNASequence",
    crate::genetics::TRAIT_GROUP,
    // plugins/cookbook_tests.rs: fills a backpack
    "Cobalt",
    // machines/veins.rs
    "GenerationGroupVein",
    // instances.rs
    "PortalGenerator1",
    "ProceduralWreckSafe",
    // scene.rs: the starting pod of a planet without an EscapePod<planet> of its own
    "EscapePod",
];

/// What of the above the tables lack, as "what NAME" lines; Err names every one.
pub fn check(data: &GameData, spec: &Spec) -> Result<(), String> {
    let mut missing = Vec::new();
    let space = crate::rockets::SPACE_MULTIPLIERS.iter().map(|(_, id)| *id);
    missing.extend(REQUIRED_GROUPS.iter().copied().chain(space).filter(|id| data.group(id).is_none()).map(|id| format!("group {id}")));
    missing.extend(crate::game::LOADING_RPCS.iter().filter(|name| spec.hash(name).is_none()).map(|name| format!("RPC {name}")));
    for class in crate::rockets::PLATFORM_CLASSES {
        if !data.group_prefabs.any_component_of_type(class) {
            missing.push(format!("rocket platform class {class}"));
        }
    }
    // The animals' network prefab hashes are not in the tables: each animal the tables list
    // needs one in animals/spawn.rs.
    if data.asset("AnimalList:AnimalList").is_none() {
        missing.push("asset AnimalList:AnimalList".into());
    }
    let hashes = crate::animals::spawn::PREFABS;
    missing.extend(crate::animals::spawn::animal_list(data).into_iter().filter(|(_, prefab)| !hashes.iter().any(|h| h.0 == prefab)).map(|(_, prefab)| format!("prefab hash of animal {prefab}")));
    if missing.is_empty() {
        return Ok(());
    }
    Err(format!("the tables lack what this server release needs from the game: {} (the game was updated: a newer server release may be needed)", missing.join(", ")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The test tables have all of it.
    #[test]
    fn the_test_tables_have_every_fact() {
        let Some((_, data, spec, _)) = crate::game::test_tables() else { return };
        check(&data, &spec).unwrap();
    }

    /// Tables without a fact are refused with its name: no tables lack everything, real ones
    /// but the RPCs lack the RPCs.
    #[test]
    fn a_missing_fact_is_named() {
        let Some((_, data, spec, _)) = crate::game::test_tables() else { return };
        let e = check(&GameData::default(), &spec).unwrap_err();
        for says in ["group DNASequence", "group GeneticTrait", "group EscapePod", "group SpaceMultiplierOxygen", "rocket platform class MachineRocketBackAndForthTrade", "rocket platform class MachineRocketBackAndForthInterplanetaryExchange", "asset AnimalList:AnimalList"] {
            assert!(e.contains(says), "{says} in {e}");
        }
        assert!(!e.contains("RPC "), "{e}");
        let e = check(&data, &Spec::default()).unwrap_err();
        assert!(e.contains("RPC PlayerMainController.PlayerReadyServerRpc") && e.contains("RPC ProceduralInstancesHandler.LoadInstanceFromSceneServerRpc"), "{e}");
        assert!(!e.contains("group ") && !e.contains("prefab hash"), "{e}");
    }

    /// An animal the tables list has a prefab hash, and one without is named.
    #[test]
    fn an_animal_without_a_prefab_hash_is_named() {
        let Some((_, data, spec, _)) = crate::game::test_tables() else { return };
        let mut data = data;
        let mut assets = (*data.assets).clone();
        let mut list = (**assets.get("AnimalList:AnimalList").unwrap()).clone();
        list["animals"].as_array_mut().unwrap().push(serde_json::json!({"id": 99, "prefab": "GameObject:Dragon"}));
        assets.insert("AnimalList:AnimalList".into(), std::sync::Arc::new(list));
        data.assets = std::sync::Arc::new(assets);
        let e = check(&data, &spec).unwrap_err();
        assert!(e.contains("prefab hash of animal Dragon") && !e.contains("Sharky"), "{e}");
    }

    /// Every group the non-test source looks up by a literal name is in REQUIRED_GROUPS.
    #[test]
    fn every_group_looked_up_by_name_is_required() {
        fn walk(dir: &std::path::Path, found: &mut Vec<(String, String)>) {
            for entry in std::fs::read_dir(dir).unwrap().flatten() {
                let (path, name) = (entry.path(), entry.file_name().to_string_lossy().to_string());
                if path.is_dir() {
                    walk(&path, found);
                } else if name.ends_with(".rs") && !["bench.rs", "coverage.rs", "scenario.rs", "testkit.rs", "timetravel.rs", "tests.rs", "play_tests.rs", "facts.rs"].contains(&name.as_str()) {
                    let text = std::fs::read_to_string(&path).unwrap();
                    let lines: Vec<&str> = text.lines().collect();
                    let end = (0..lines.len()).find(|&i| lines[i] == "#[cfg(test)]" && lines.get(i + 1).is_some_and(|l| l.contains("mod ") && l.contains("tests"))).unwrap_or(lines.len());
                    for (i, line) in lines[..end].iter().enumerate() {
                        for (at, _) in line.match_indices(".group(\"") {
                            let id = line[at + 8..].split('"').next().unwrap_or_default();
                            found.push((format!("{}:{}", path.display(), i + 1), id.to_string()));
                        }
                    }
                }
            }
        }
        let mut found = Vec::new();
        walk(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"), &mut found);
        assert!(found.len() >= 5, "{found:?}");
        for (at, id) in found {
            assert!(REQUIRED_GROUPS.contains(&id.as_str()), "{at} looks up group {id}: add it to facts::REQUIRED_GROUPS");
        }
    }
}
