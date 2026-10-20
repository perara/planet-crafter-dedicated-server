//! Saves of the game's older layouts, converted to the current one as the game reads them
//! (JSONExport.LoadFromJson and JsonableGameState.ConvertOldFormatToNew) and writes them back.
//!
//! The game keeps no old reader of its own: it patches the text of the file so the current
//! reader can take it, and the next save is in the current layout. The oldest layout (2022,
//! one planet) has nine sections: unit levels, one player, world objects, inventories, stats,
//! messages, story events, a game mode and colour layers. The patches, in the game's order:
//!
//! * the player's `unlockedGroups` is moved in front of the first `}` of the file;
//! * the first record that starts with the unit levels is cut out and, with the file's first
//!   `planetId` (Prime when there is none), put in as a section of its own after the first: the
//!   planets. A save between the layouts that already holds `terraTokens` there keeps those as
//!   the world state; in the oldest the world state is left empty, and the cut record carried
//!   the unlocked groups, which a planet does not hold: the world state has none afterwards;
//! * `unitBiomassLevel` is `unitPlantsLevel`, the player record gets the id, name and inventory
//!   ids of the one player a single-planet game had, `SpaceMultiplierBiomass` is
//!   `SpaceMultiplierPlants` and `DeparturePlatformHumble` is `DeparturePlatform`;
//! * the settings section of a game mode without dying consequences gets the consequences (and
//!   the gauge drain) the mode stood for; the colour layers are read by no one any more.
//!
//! Every record then goes through its class's fields, as JsonUtility reads and writes it: a
//! field the class does not have (`wear`) is gone, one that is missing has its default.

use serde_json::{json, Map, Value};

use crate::save::{split_outside_strings, SaveFile, IMPORT_HINT};

/// The game modes and what an old one stood for: dying consequences and the gauge drain it set
/// (JsonableGameState.ConvertOldFormatToNew).
fn from_mode(mode: Option<&str>) -> (&'static str, Option<f64>) {
    match mode {
        Some("Chill") => ("NoConsequences", Some(0.800000011920929)),
        Some("Intense") => ("DropAllItems", Some(1.2000000476837158)),
        Some("Hardcore") => ("DeleteSaveFile", Some(1.600000023841858)),
        _ => ("DropSomeItems", None),
    }
}

/// The text of a world in the current layout from a file the server refused as an old one
/// (`SaveFile::load`'s error carries IMPORT_HINT). A file that is current already is returned
/// as it is; one that is no save at all gets the loader's own error. `world_seed` is the seed an
/// old save lacks (the game draws one at every load until it saves), `version` the game version
/// the tables are of.
pub fn import(text: &str, world_seed: u32, version: &str) -> Result<String, String> {
    match SaveFile::parse(text).and_then(|s| crate::save::check_settings(&s.settings)) {
        Ok(()) => Ok(text.to_string()),
        Err(e) if !e.contains(IMPORT_HINT) => Err(e),
        Err(_) => {
            let save = convert(text, world_seed, version)?;
            let out = save.to_text();
            // What is written is what the loader reads back.
            SaveFile::parse(&out).and_then(|s| crate::save::check_settings(&s.settings)).map_err(|e| format!("the converted save does not load: {e}"))?;
            Ok(out)
        }
    }
}

fn find_from(text: &str, from: usize, what: &str) -> Option<usize> {
    text[from..].find(what).map(|i| i + from)
}

/// The first `"key":"[^"]*?"` of the text: its start and end.
fn string_field(text: &str, key: &str, followed_by: Option<char>) -> Option<(usize, usize)> {
    let head = format!("\"{key}\":\"");
    let mut from = 0;
    while let Some(at) = find_from(text, from, &head) {
        let value = at + head.len();
        let close = find_from(text, value, "\"")?;
        let end = close + 1;
        if followed_by.is_none_or(|c| text[end..].starts_with(c)) {
            return Some((at, end + followed_by.map_or(0, char::len_utf8)));
        }
        from = at + 1;
    }
    None
}

/// The text of an old save after the game's patches (the part of LoadFromJson before it splits
/// the sections), and whether it was a save from before the planets.
fn patched(text: &str) -> Result<String, String> {
    let mut text = text.trim_start_matches('\u{feff}').replace(['\n', '\r'], "");
    if let Some((a, b)) = string_field(&text, "unlockedGroups", Some(',')) {
        let found = text[a..b].to_string();
        text = text.replace(&found, "");
        let brace = text.find('}').ok_or("no record in the file")?;
        text.insert_str(brace, &format!(",{}", found.replace("\",", "\"")));
    }
    let with_tokens = text.contains("\"terraTokens\"");
    if let Some(start) = text.find("{\"unitOxygenLevel\":") {
        let end = if with_tokens { find_from(&text, start, ",\"terraTokens\"").map(|i| i + ",\"terraTokens\"".len()) } else { find_from(&text, start, "}").map(|i| i + 1) };
        if let Some(end) = end {
            let found = text[start..end].to_string();
            let planet = string_field(&text, "planetId", None).map_or("\"planetId\":\"Prime\"".to_string(), |(a, b)| text[a..b].to_string());
            text = text.replace(&found, if with_tokens { "{\"terraTokens\"" } else { "" });
            let units = if with_tokens { found.replace(",\"terraTokens\"", "") } else { found.replace('}', "") };
            let after_first = text.find('@').map_or(0, |i| i + 1);
            text.insert_str(after_first, &format!("{units},{planet}}}@"));
        }
    }
    Ok(text)
}

/// The record's fields of a class, in the class's order, each as the record has it (when it is
/// of the field's JSON type) or at its default.
fn record(v: &Value, fields: &[(&str, Value)]) -> Value {
    let mut out = Map::new();
    for (name, default) in fields {
        let given = v.get(*name).filter(|g| match default {
            Value::String(_) => g.is_string(),
            Value::Bool(_) => g.is_boolean(),
            Value::Number(n) if n.is_f64() => g.is_number(),
            Value::Number(_) => g.is_i64() || g.is_u64(),
            _ => true,
        });
        out.insert((*name).to_string(), given.unwrap_or(default).clone());
    }
    Value::Object(out)
}

/// The record's fields of a class that it has, in its own order (fields at their default are
/// cut from the text when it is written, so a missing one needs no value).
fn only(v: &Value, fields: &[&str]) -> Value {
    Value::Object(v.as_object().map(|m| m.iter().filter(|(k, _)| fields.contains(&k.as_str())).map(|(k, v)| (k.clone(), v.clone())).collect()).unwrap_or_default())
}

fn list(section: &str, name: &str) -> Result<Vec<Value>, String> {
    let entries = split_outside_strings(section, '|').into_iter().map(str::trim).filter(|s| !s.is_empty());
    entries.map(|s| serde_json::from_str(s).map_err(|e| format!("{name}: {e} in {}", s.chars().take(80).collect::<String>()))).collect()
}

const WORLD_OBJECT: [&str; 19] = ["id", "gId", "liId", "siIds", "liGrps", "pos", "rot", "planet", "liPlanet", "pnls", "color", "text", "grwth", "set", "trtInd", "trtVal", "hunger", "count", "linkedWo"];

/// JSONExport.LoadFromJson, then SaveToJson: the save as the game would write it after reading
/// `text`.
pub fn convert(text: &str, world_seed: u32, version: &str) -> Result<SaveFile, String> {
    let text = patched(text)?;
    let array = split_outside_strings(&text, '@');
    let section = |i: usize| array.get(i).copied().unwrap_or("");
    let first = |i: usize, name: &str| list(section(i), name).map(|v| v.into_iter().next().unwrap_or(Value::Null));
    if array.len() <= 8 {
        return Err(format!("{} sections and no game settings among them: not an old save this server can convert", array.len().saturating_sub(1)));
    }
    let world = record(
        &first(0, "world state")?,
        &[("terraTokens", json!(0)), ("allTimeTerraTokens", json!(0)), ("unlockedGroups", json!("")), ("openedInstanceSeed", json!(0)), ("openedInstanceTimeLeft", json!(0)), ("logisticsPaused", json!(false))],
    );
    let planets = list(&section(1).replace("unitBiomassLevel", "unitPlantsLevel"), "planets")?
        .iter()
        .map(|p| {
            let mut p = record(p, &[("planetId", json!("")), ("unitOxygenLevel", json!(0.0)), ("unitHeatLevel", json!(0.0)), ("unitPressureLevel", json!(0.0)), ("unitPlantsLevel", json!(0.0)), ("unitInsectsLevel", json!(0.0)), ("unitAnimalsLevel", json!(0.0)), ("unitPurificationLevel", json!(0.0))]);
            if p["planetId"] == "" {
                p["planetId"] = json!("Prime");
            }
            p
        })
        .collect();
    let players = list(&section(2).replace("{\"playerPosition\"", "{\"id\":0,\"name\":\"Convict-1\",\"inventoryId\":1,\"equipmentId\":2,\"playerPosition\""), "players")?
        .iter()
        .map(|p| {
            record(
                p,
                &[
                    ("id", json!(0)),
                    ("name", json!("Convict-1")),
                    ("inventoryId", json!(1)),
                    ("equipmentId", json!(2)),
                    ("playerPosition", json!("0,0,0")),
                    ("playerRotation", json!("0,0,0,1")),
                    ("playerGaugeOxygen", json!(0.0)),
                    ("playerGaugeThirst", json!(0.0)),
                    ("playerGaugeHealth", json!(0.0)),
                    ("playerGaugeToxic", json!(0.0)),
                    ("host", json!(false)),
                    ("planetId", json!("")),
                    ("totalCraftedObjects", json!(0)),
                    ("totalTerraTokenEarned", json!(0)),
                    ("cameraView", json!(0)),
                ],
            )
        })
        .collect();
    let objects = section(3).replace("SpaceMultiplierBiomass", "SpaceMultiplierPlants").replace("DeparturePlatformHumble", "DeparturePlatform");
    let objects = list(&objects, "world objects")?.iter().map(|o| only(o, &WORLD_OBJECT)).collect();
    let inventories = list(section(4), "inventories")?.iter().map(|i| only(i, &["id", "woIds", "size", "demandGrps", "supplyGrps", "priority"])).collect();
    let stats = record(&first(5, "stats")?, &[("craftedObjects", json!(0)), ("totalSaveFileLoad", json!(0)), ("totalSaveFileTime", json!(0))]);
    let messages = list(section(6), "messages")?.iter().map(|m| record(m, &[("stringId", json!("")), ("isRead", json!(false))])).collect();
    let story_events = list(section(7), "story events")?.iter().map(|m| record(m, &[("stringId", json!(""))])).collect();
    let settings = settings(&first(8, "game settings")?, world_seed, version)?;
    // Colour layers (a section some layouts hold before the instances) are skipped.
    let instances = if section(9).starts_with("{\"layerId\"") { section(10) } else { section(9) };
    Ok(SaveFile { world, planets, players, objects, inventories, stats, messages, story_events, settings, procedural: list(instances, "procedural instances")?, pending: None })
}

/// The game settings as the game reads them (a JsonableGameState at its defaults, overwritten
/// by the record, ConvertOldFormatToNew, ConvertStringsToEnum) and writes them (the version and
/// whether it is modded are set by the save; the enums are not written).
fn settings(v: &Value, world_seed: u32, version: &str) -> Result<Value, String> {
    let mut s = record(
        v,
        &[
            ("saveDisplayName", json!("")),
            ("planetId", json!("")),
            ("unlockedSpaceTrading", json!(false)),
            ("unlockedOreExtrators", json!(false)),
            ("unlockedTeleporters", json!(false)),
            ("unlockedDrones", json!(false)),
            ("unlockedAutocrafter", json!(false)),
            ("unlockedEverything", json!(false)),
            ("freeCraft", json!(false)),
            ("preInterplanetarySave", json!(false)),
            ("randomizeMineables", json!(false)),
            ("modifierTerraformationPace", json!(1.0)),
            ("modifierPowerConsumption", json!(1.0)),
            ("modifierGaugeDrain", json!(1.0)),
            ("modifierMeteoOccurence", json!(1.0)),
            ("modifierMultiplayerTerraformationFactor", json!(0.0)),
            ("modded", json!(false)),
            ("version", json!("")),
            ("mode", json!("")),
            ("dyingConsequencesLabel", json!("")),
            ("startLocationLabel", json!("")),
            ("worldSeed", json!(0)),
            ("hasPlayedIntro", json!(false)),
        ],
    );
    // A label that was not there is null in the game; the record's own is kept.
    if v.get("dyingConsequencesLabel").is_none_or(Value::is_null) {
        let (dying, drain) = from_mode(v["mode"].as_str());
        s["dyingConsequencesLabel"] = json!(dying);
        if let Some(drain) = drain {
            s["modifierGaugeDrain"] = json!(drain);
        }
        s["startLocationLabel"] = json!("");
    }
    // Enum.Parse of the two labels: an unknown one is a save the game cannot read.
    let label = |k: &str, known: &[&str]| s[k].as_str().filter(|l| known.contains(l)).map(str::to_string).ok_or_else(|| format!("game settings: {k} {} is none the game knows", s[k]));
    label("mode", &crate::save::MODES)?;
    label("dyingConsequencesLabel", &crate::save::DYING)?;
    if s["planetId"] == "" {
        s["planetId"] = json!("Prime");
    }
    if s["worldSeed"] == 0 {
        s["worldSeed"] = json!(world_seed);
    }
    // The save sets these; preInterplanetarySave is on while the old save loads and off once it has.
    s["modded"] = json!(false);
    s["version"] = json!(version);
    s["preInterplanetarySave"] = json!(false);
    s["gameStartLocation"] = s["startLocationLabel"].clone();
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn imported(text: &str) -> SaveFile {
        SaveFile::parse(&import(text, 77, "2.103").unwrap_or_else(|e| panic!("{e}"))).unwrap()
    }

    /// The oldest layout, converted as the game converts it (checked against the game itself:
    /// server/tests/parity/EXPECTED.md "Old saves").
    #[test]
    fn the_oldest_layout_becomes_the_current_one() {
        let text = SaveFile::old_format_text();
        let save = imported(&text);
        // The world state is empty, the player's unlocked groups went with the cut record (the game's).
        assert_eq!(save.world, json!({"terraTokens": 0, "allTimeTerraTokens": 0, "unlockedGroups": "", "openedInstanceSeed": 0, "openedInstanceTimeLeft": 0, "logisticsPaused": false}));
        // The units are the planet Prime's; the biomass is the plants.
        assert_eq!(save.planets, vec![json!({"planetId": "Prime", "unitOxygenLevel": 15.5, "unitHeatLevel": 1.0, "unitPressureLevel": 2.0, "unitPlantsLevel": 3.0, "unitInsectsLevel": 0.0, "unitAnimalsLevel": 0.0, "unitPurificationLevel": 0.0})]);
        // The one player has the id, name and inventories of the first.
        assert_eq!(
            save.players,
            vec![json!({"id": 0, "name": "Convict-1", "inventoryId": 1, "equipmentId": 2, "playerPosition": "1,2,3", "playerRotation": "0,0,0,1", "playerGaugeOxygen": 370.0, "playerGaugeThirst": 99.0, "playerGaugeHealth": 98.0,
                "playerGaugeToxic": 0.0, "host": false, "planetId": "", "totalCraftedObjects": 0, "totalTerraTokenEarned": 0, "cameraView": 0})]
        );
        // "wear" is no field of a world object any more; the rest stays (fields at their default are not written).
        assert_eq!(save.objects[0], json!({"id": 208716012, "gId": "EscapePod", "pos": "4,5,6", "rot": "0,0,0,1"}));
        assert_eq!(save.objects[1], json!({"id": 107604566, "gId": "ice"}));
        assert_eq!(save.inventories, vec![json!({"id": 1, "woIds": "208716012", "size": 35}), json!({"id": 2, "woIds": "", "size": 12})]);
        assert_eq!(save.stats, json!({"craftedObjects": 765, "totalSaveFileLoad": 0, "totalSaveFileTime": 0}));
        assert_eq!((save.messages.len(), save.story_events.len(), save.procedural.len()), (1, 1, 0), "the colour layers are read by no one");
        let s = &save.settings;
        assert_eq!((s["planetId"].as_str(), s["mode"].as_str(), s["dyingConsequencesLabel"].as_str(), s["startLocationLabel"].as_str(), s["gameStartLocation"].as_str()), (Some("Prime"), Some("Standard"), Some("DropSomeItems"), Some(""), Some("")));
        assert_eq!((s["worldSeed"].as_u64(), s["version"].as_str(), s["modded"].as_bool(), s["hasPlayedIntro"].as_bool(), s["preInterplanetarySave"].as_bool()), (Some(77), Some("2.103"), Some(false), Some(false), Some(false)));
        assert_eq!((s["modifierGaugeDrain"].as_f64(), s["modifierTerraformationPace"].as_f64(), s["modifierMultiplayerTerraformationFactor"].as_f64()), (Some(1.0), Some(1.0), Some(0.0)));
        // What was converted is a save: converting it again changes nothing.
        let once = import(&text, 77, "2.103").unwrap();
        assert_eq!(import(&once, 5, "x").unwrap(), once);
        assert!(crate::save::check_settings(&SaveFile::parse(&once).unwrap().settings).is_ok());
    }

    /// A game mode stood for dying consequences and a gauge drain (ConvertOldFormatToNew).
    #[test]
    fn an_old_game_mode_becomes_its_dying_consequences_and_gauge_drain() {
        for (mode, dying, drain) in [("Chill", "NoConsequences", 0.800000011920929), ("Standard", "DropSomeItems", 1.0), ("Intense", "DropAllItems", 1.2000000476837158), ("Hardcore", "DeleteSaveFile", 1.600000023841858), ("Creative", "DropSomeItems", 1.0)] {
            let text = SaveFile::old_format_text().replace("\"mode\":\"Standard\"", &format!("\"mode\":\"{mode}\""));
            let s = imported(&text).settings;
            assert_eq!((s["mode"].as_str(), s["dyingConsequencesLabel"].as_str(), s["modifierGaugeDrain"].as_f64()), (Some(mode), Some(dying), Some(drain)), "{mode}");
        }
        // A mode the game does not know cannot be read by it either.
        let text = SaveFile::old_format_text().replace("\"mode\":\"Standard\"", "\"mode\":\"Easy\"");
        assert!(import(&text, 1, "").unwrap_err().contains("mode"), "refused");
        // Settings of the current layout with only a mode (a game between the layouts) are converted alike.
        let mut save = SaveFile::parse(&SaveFile::tiny_text("w")).unwrap();
        save.settings = json!({"mode": "Hardcore", "hasPlayedIntro": true, "freeCraft": true});
        let s = imported(&save.to_text()).settings;
        assert_eq!((s["dyingConsequencesLabel"].as_str(), s["modifierGaugeDrain"].as_f64(), s["hasPlayedIntro"].as_bool(), s["freeCraft"].as_bool(), s["planetId"].as_str()), (Some("DeleteSaveFile"), Some(1.600000023841858), Some(true), Some(true), Some("Prime")));
    }

    /// The layout between the two: the first section holds terraTokens too and keeps them as the
    /// world state, with the groups when they are its last field.
    #[test]
    fn a_save_between_the_layouts_keeps_its_tokens_and_groups() {
        let text = SaveFile::old_format_text()
            .replacen(
                "\"unitBiomassLevel\":3.0}",
                "\"unitBiomassLevel\":3.0,\"terraTokens\":5,\"allTimeTerraTokens\":9,\"unlockedGroups\":\"Pod4x,Jetpack2\"}",
                1,
            )
            .replacen("\"unlockedGroups\":\"Pod4x,BootsSpeed1\",", "", 1);
        let save = imported(&text);
        assert_eq!(save.world, json!({"terraTokens": 5, "allTimeTerraTokens": 9, "unlockedGroups": "Pod4x,Jetpack2", "openedInstanceSeed": 0, "openedInstanceTimeLeft": 0, "logisticsPaused": false}));
        assert_eq!(save.planets[0]["unitOxygenLevel"], json!(15.5));
        assert_eq!(save.planets[0]["planetId"], json!("Prime"));
        // With the planet named in the file, that is the planet.
        let named = text.replacen("\"mode\":\"Standard\"", "\"planetId\":\"Selenea\",\"mode\":\"Standard\"", 1);
        assert_eq!(imported(&named).planets[0]["planetId"], json!("Selenea"));
    }

    #[test]
    fn groups_renamed_since_are_renamed() {
        let text = SaveFile::old_format_text().replace("\"gId\":\"ice\"", "\"gId\":\"SpaceMultiplierBiomass\"").replace("\"gId\":\"EscapePod\"", "\"gId\":\"BlueprintDeparturePlatformHumble\"");
        let save = imported(&text);
        assert_eq!((save.objects[0]["gId"].as_str(), save.objects[1]["gId"].as_str()), (Some("BlueprintDeparturePlatform"), Some("SpaceMultiplierPlants")));
    }

    /// A save of the current layout is not touched; a file that is no save, or an old save cut
    /// short, is refused with the reason, not converted into a smaller world.
    #[test]
    fn only_old_saves_are_converted() {
        let current = SaveFile::tiny_text("w");
        assert_eq!(import(&current, 1, "x").unwrap(), current);
        for garbage in ["", "hello", "{}", "@@@@@@@@@"] {
            assert!(import(garbage, 1, "x").is_err(), "{garbage:?}");
        }
        let old = SaveFile::old_format_text();
        let mut cuts = vec![];
        for n in 1..9 {
            // Cut after n whole sections (eight, up to the game mode, are an old save without layers), or in the middle of the next.
            let at = old.match_indices("\n@").nth(n - 1).unwrap().0 + 2;
            if n < 8 {
                cuts.push(old[..at].to_string());
            }
            cuts.push(old[..at + 20].to_string());
        }
        for cut in cuts {
            let e = import(&cut, 1, "x").err().unwrap_or_else(|| panic!("converted: {cut}"));
            assert!(!e.is_empty() && !e.contains('\n'), "{e}");
        }
    }

    /// `tests/parity/mkold.py` makes the old layouts from a current save (the parity run uses the
    /// sweep's start save); here a small one, back through the import: the objects, the
    /// inventories (the player's two become 1 and 2), the units and the mode come out as they went in.
    #[test]
    fn the_script_that_makes_old_saves_makes_ones_the_import_reads() {
        let script = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/parity/mkold.py");
        if std::process::Command::new("python3").arg("--version").output().is_err() {
            return;
        }
        let mut cur = SaveFile::empty();
        cur.world = json!({"terraTokens": 5, "allTimeTerraTokens": 7, "unlockedGroups": "Pod4x,Jetpack2", "openedInstanceSeed": 0, "openedInstanceTimeLeft": 0, "logisticsPaused": false});
        cur.planets = vec![json!({"planetId": "Prime", "unitOxygenLevel": 11.5, "unitHeatLevel": 2.0, "unitPressureLevel": 3.0, "unitPlantsLevel": 4.0, "unitInsectsLevel": 0.0, "unitAnimalsLevel": 0.0, "unitPurificationLevel": -1.0})];
        cur.players = vec![json!({"id": 76561197999990100u64, "name": "A", "inventoryId": 3, "equipmentId": 4, "playerPosition": "1,2,3", "playerRotation": "0,0,0,1", "playerGaugeOxygen": 300.0, "playerGaugeThirst": 90.0, "playerGaugeHealth": 80.0, "host": true})];
        cur.objects = vec![json!({"id": 20, "gId": "ice"}), json!({"id": 21, "gId": "Iron"}), json!({"id": 22, "gId": "Container1", "liId": 9, "pos": "5,6,7", "rot": "0,0,0,1", "planet": -1140328421}), json!({"id": 23, "gId": "EscapePod", "pos": "8,9,10", "rot": "0,0,0,1", "planet": -1140328421})];
        cur.inventories = vec![json!({"id": 1, "woIds": "", "size": 4}), json!({"id": 3, "woIds": "20,21", "size": 10}), json!({"id": 4, "woIds": "", "size": 4}), json!({"id": 9, "woIds": "", "size": 5})];
        cur.stats = json!({"craftedObjects": 12, "totalSaveFileLoad": 0, "totalSaveFileTime": 0});
        cur.messages = vec![json!({"stringId": "Message_Help1", "isRead": true})];
        cur.story_events = vec![json!({"stringId": "StoryEvent-FirstRain"})];
        cur.settings = json!({"saveDisplayName": "x", "planetId": "Prime", "mode": "Intense", "dyingConsequencesLabel": "DropAllItems", "worldSeed": 7});
        let dir = std::env::temp_dir().join(format!("pcds-mkold-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (from, to) = (dir.join("cur.json"), dir.join("old.json"));
        std::fs::write(&from, cur.to_text()).unwrap();
        for tokens in [false, true] {
            let mut cmd = std::process::Command::new("python3");
            cmd.args(["-I", script, &from.to_string_lossy(), &to.to_string_lossy()]);
            if tokens {
                cmd.arg("--tokens");
            }
            let made = cmd.output().unwrap();
            assert!(made.status.success(), "{}", String::from_utf8_lossy(&made.stderr));
            let old = std::fs::read_to_string(&to).unwrap();
            assert!(SaveFile::parse(&old).err().unwrap().contains("old save format"), "{old}");
            let save = imported(&old);
            let ids = |v: &[Value]| v.iter().map(|o| o["id"].as_i64().unwrap()).collect::<Vec<_>>();
            assert_eq!(ids(&save.objects), [20, 21, 22, 23]);
            assert_eq!(save.objects[2]["liId"], json!(9));
            assert_eq!(ids(&save.inventories), [1, 2, 9]);
            assert_eq!(save.inventories[0]["woIds"], json!("20,21"), "the player's bag is 1");
            assert_eq!(save.planets, vec![json!({"planetId": "Prime", "unitOxygenLevel": 11.5, "unitHeatLevel": 2.0, "unitPressureLevel": 3.0, "unitPlantsLevel": 4.0, "unitInsectsLevel": 0.0, "unitAnimalsLevel": 0.0, "unitPurificationLevel": 0.0})]);
            assert_eq!((save.players[0]["playerPosition"].as_str(), save.players[0]["playerGaugeThirst"].as_f64(), save.stats["craftedObjects"].as_u64()), (Some("1,2,3"), Some(90.0), Some(12)));
            assert_eq!((save.settings["dyingConsequencesLabel"].as_str(), save.settings["modifierGaugeDrain"].as_f64()), (Some("DropAllItems"), Some(1.2000000476837158)));
            // The tokens layout keeps the world state with its groups; the oldest loses them (the game's).
            assert_eq!((save.world["terraTokens"].as_u64(), save.world["unlockedGroups"].as_str()), if tokens { (Some(5), Some("Pod4x,Jetpack2")) } else { (Some(0), Some("")) });
        }
        std::fs::remove_dir_all(dir).ok();
    }
}
