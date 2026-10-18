//! `pcds new-save SAVE.json DATA_DIR [--planet ID] [--start LOCATION] [--seed N] [--set KEY=VALUE ...]`:
//! a new world, as the game's new-save screen makes one (SaveFilesCreateNew.CreateNewFile and
//! JSONExport.CreateNewSaveFile) with the Standard settings preset. Only the free starting
//! planets can be chosen.

use std::path::Path;

use serde_json::{json, Value};

use crate::data::{stable_hash, NetData};
use crate::game::{Rng, START_EQUIPMENT_SIZE, START_INVENTORY_SIZE};
use crate::gamedata::GameData;
use crate::model::{Inventory, WorldObject};
use crate::save::SaveFile;

/// DataConfig.GameSettingMode and GameSettingDyingConsequences: the game parses these labels.
const MODES: [&str; 6] = ["Chill", "Standard", "Intense", "Hardcore", "Creative", "Custom"];
const DYING: [&str; 4] = ["NoConsequences", "DropSomeItems", "DropAllItems", "DeleteSaveFile"];

pub struct Options {
    /// None: the first free starting planet.
    pub planet: Option<String>,
    pub start: String,
    pub seed: Option<u32>,
    pub set: Vec<(String, String)>,
}

fn usage() -> ! {
    eprintln!("usage: pcds new-save SAVE.json DATA_DIR [--planet ID] [--start LOCATION] [--seed N] [--set KEY=VALUE ...]");
    std::process::exit(2)
}

pub fn run(args: &[String]) {
    let [path, dir, rest @ ..] = args else { usage() };
    let mut o = Options { planet: None, start: "Standard".into(), seed: None, set: Vec::new() };
    let mut flags = rest.iter();
    while let Some(flag) = flags.next() {
        let Some(value) = flags.next() else { usage() };
        match flag.as_str() {
            "--planet" => o.planet = Some(value.clone()),
            "--start" => o.start = value.clone(),
            "--seed" => o.seed = Some(value.parse().unwrap_or_else(|_| usage())),
            "--set" => o.set.push(value.split_once('=').map(|(k, v)| (k.to_string(), v.to_string())).unwrap_or_else(|| usage())),
            _ => usage(),
        }
    }
    if Path::new(path).exists() {
        eprintln!("{path} exists: not overwriting it");
        std::process::exit(1);
    }
    let (net_data, data, _) = crate::tables(dir);
    let name = Path::new(path).file_stem().unwrap_or_default().to_string_lossy().to_string();
    let save = new_save(&net_data, &data, &name, &o, &mut Rng::seeded()).unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(2)
    });
    if let Err(e) = save.write(path) {
        eprintln!("{path}: {e}");
        std::process::exit(1);
    }
    let s = |k: &str| save.settings[k].as_str().map(str::to_string).unwrap_or_else(|| save.settings[k].to_string());
    println!("new world {path}: planet {}, start {}, world seed {}", s("planetId"), s("gameStartLocation"), s("worldSeed"));
}

/// What a new world can be: the free starting planets with their start locations, the game
/// modes and the dying consequences (the admin page's form).
pub fn choices(net_data: &NetData, data: &GameData) -> Value {
    let planets: Vec<Value> = net_data
        .planets
        .iter()
        .filter(|p| p.starting_planet && p.steam_app_id == 0)
        .map(|p| {
            let spawns = data.asset(&format!("PlanetData:PlanetData-{}", p.id)).and_then(|d| d["spawnPositions"].as_array()).cloned().unwrap_or_default();
            json!({"id": p.id, "starts": spawns.iter().filter(|s| s["positions"].as_array().is_some_and(|a| !a.is_empty())).filter_map(|s| s["id"].as_str()).collect::<Vec<_>>()})
        })
        .collect();
    json!({"planets": planets, "modes": MODES, "dying": DYING})
}

/// JSONExport.CreateNewSaveFile: world state, player stats and the planet's state at their
/// defaults, no players, inventories 1 and 2 for the first one, the starting pod at a random
/// spawn position of the start location (SpawnLocationHandler.GetPositionDependingOnSpawnSetting)
/// and the settings.
pub fn new_save(net_data: &NetData, data: &GameData, name: &str, o: &Options, rng: &mut Rng) -> Result<SaveFile, String> {
    let planets: Vec<&str> = net_data.planets.iter().filter(|p| p.starting_planet && p.steam_app_id == 0).map(|p| p.id.as_str()).collect();
    let planet = o.planet.as_deref().or(planets.first().copied()).ok_or("no free starting planet in the game tables")?;
    if !planets.contains(&planet) {
        return Err(format!("--planet {planet}: a new world starts on {}", planets.join(", ")));
    }
    let spawns = data.asset(&format!("PlanetData:PlanetData-{planet}")).and_then(|p| p["spawnPositions"].as_array()).cloned().unwrap_or_default();
    let positions = spawns.iter().find(|s| s["id"] == o.start.as_str()).and_then(|s| s["positions"].as_array()).filter(|p| !p.is_empty()).ok_or_else(|| {
        let ids: Vec<&str> = spawns.iter().filter_map(|s| s["id"].as_str()).collect();
        format!("--start {}: {planet} has {}", o.start, ids.join(", "))
    })?;
    let spawn = &positions[rng.range(0, positions.len() as i32) as usize];
    let pod_group = crate::scene::pod_group(data, planet).ok_or("no EscapePod group in the game tables")?;
    // WorldObjectsIdHandler.GetNewWorldObjectIdForDb.
    let mut pod = WorldObject::new(20 * 10_000_000 + rng.range(1_000_000, 9_999_999), pod_group, data);
    let f = |v: &Value, i: usize| v[i].as_f64().unwrap_or(0.0) as f32;
    pod.position = [0, 1, 2].map(|i| f(&spawn["position"], i));
    pod.rotation = [0, 1, 2, 3].map(|i| f(&spawn["rotation"], i));
    pod.planet = stable_hash(planet);

    let mut settings = data.asset("JsonableGameState:GameSettingsPresetStandard").cloned().ok_or("no GameSettingsPresetStandard in the game tables")?;
    // The preset's runtime fields as a new save has them. ConvertEnumToStrings writes the enums
    // as labels and CleanSaveStringOfVariables drops them; CreateNewFile names the save and
    // randomizes the seed; modded, version and hasPlayedIntro are set when the game saves.
    let map = settings.as_object_mut().ok_or("GameSettingsPresetStandard is not an object")?;
    let mode = map.shift_remove("gameMode").unwrap_or(json!("Standard"));
    let dying = map.shift_remove("gameDyingConsequences").unwrap_or(json!("DropSomeItems"));
    settings["mode"] = mode;
    settings["dyingConsequencesLabel"] = dying;
    settings["startLocationLabel"] = json!(o.start);
    settings["gameStartLocation"] = json!(o.start);
    settings["saveDisplayName"] = json!(name);
    settings["planetId"] = json!(planet);
    settings["worldSeed"] = json!(o.seed.unwrap_or_else(|| rng.range(0, i32::MAX) as u32));
    settings["version"] = json!(net_data.game_version);
    settings["modded"] = json!(false);
    settings["hasPlayedIntro"] = json!(false);
    settings["preInterplanetarySave"] = json!(false);
    for (key, value) in &o.set {
        let parsed = match &settings[key.as_str()] {
            Value::String(_) => Some(json!(value)).filter(|_| match key.as_str() {
                "mode" => MODES.contains(&value.as_str()),
                "dyingConsequencesLabel" => DYING.contains(&value.as_str()),
                _ => true,
            }),
            Value::Bool(_) => value.parse().ok().map(Value::Bool),
            Value::Number(n) if n.is_f64() => value.parse::<f64>().ok().filter(|v| v.is_finite()).map(Value::from),
            Value::Number(_) => value.parse::<i64>().ok().map(Value::from),
            _ => {
                let keys: Vec<&str> = settings.as_object().into_iter().flat_map(|m| m.keys()).map(String::as_str).collect();
                return Err(format!("--set {key}: not a game setting; settings: {}", keys.join(", ")));
            }
        };
        settings[key.as_str()] = parsed.ok_or_else(|| format!("--set {key}={value}: not a valid value (now {})", settings[key.as_str()]))?;
    }
    Ok(SaveFile {
        world: json!({"terraTokens": 0, "allTimeTerraTokens": 0, "unlockedGroups": "", "openedInstanceSeed": 0, "openedInstanceTimeLeft": 0, "logisticsPaused": false}),
        planets: vec![json!({"planetId": planet, "unitOxygenLevel": 0.0, "unitHeatLevel": 0.0, "unitPressureLevel": 0.0, "unitPlantsLevel": 0.0, "unitInsectsLevel": 0.0, "unitAnimalsLevel": 0.0, "unitPurificationLevel": 0.0})],
        objects: vec![pod.to_json(data)],
        inventories: vec![Inventory::new(1, START_INVENTORY_SIZE).to_json(data), Inventory::new(2, START_EQUIPMENT_SIZE).to_json(data)],
        stats: json!({"craftedObjects": 0, "totalSaveFileLoad": 0, "totalSaveFileTime": 0}),
        settings,
        ..SaveFile::empty()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_save_has_the_pod_the_inventories_and_the_settings() {
        let Some(dir) = crate::test_data() else { return };
        let (net_data, data, _) = crate::tables(dir);
        let set = vec![("freeCraft".to_string(), "true".to_string()), ("modifierGaugeDrain".to_string(), "0.5".to_string())];
        let o = Options { planet: Some("Prime".into()), start: "Standard".into(), seed: Some(7), set };
        let save = new_save(&net_data, &data, "My World", &o, &mut Rng::seeded()).unwrap();
        let text = save.to_text();
        let pod = &save.objects[0];
        assert_eq!((pod["gId"].as_str(), pod["planet"].as_i64()), (Some("EscapePod"), Some(-1140328421)));
        assert!(text.contains(&format!("{{\"id\":{},\"gId\":\"EscapePod\",\"pos\":\"", pod["id"])), "{text}");
        let at = WorldObject::from_json(pod, &data, 0).unwrap().position;
        let standard = data.asset("PlanetData:PlanetData-Prime").unwrap()["spawnPositions"][0]["positions"].as_array().unwrap();
        assert!(standard.iter().any(|p| [0, 1, 2].map(|i| p["position"][i].as_f64().unwrap() as f32) == at), "{at:?}");
        assert!(text.contains("\r{\"id\":1,\"woIds\":\"\",\"size\":12}|\n{\"id\":2,\"woIds\":\"\",\"size\":4}\r@"));
        let s = &save.settings;
        assert_eq!((s["saveDisplayName"].as_str(), s["planetId"].as_str(), s["worldSeed"].as_u64(), s["version"].as_str()), (Some("My World"), Some("Prime"), Some(7), Some("2.103")));
        assert_eq!((s["mode"].as_str(), s["dyingConsequencesLabel"].as_str(), s["startLocationLabel"].as_str()), (Some("Standard"), Some("DropSomeItems"), Some("Standard")));
        assert_eq!((s["hasPlayedIntro"].as_bool(), s["modded"].as_bool(), s["freeCraft"].as_bool(), s["modifierGaugeDrain"].as_f64()), (Some(false), Some(false), Some(true), Some(0.5)));
        assert!(text.contains("\"hasPlayedIntro\":false,\"gameStartLocation\":\"Standard\"}"), "the enum fields are dropped");
        // DLC planets and unknown settings are refused.
        let o = Options { planet: Some("Humble".into()), start: "Standard".into(), seed: None, set: vec![] };
        assert_eq!(new_save(&net_data, &data, "x", &o, &mut Rng::seeded()).err().unwrap(), "--planet Humble: a new world starts on Prime, Selenea");
        let o = Options { planet: None, start: "Standard".into(), seed: None, set: vec![("mode".into(), "Easy".into())] };
        assert!(new_save(&net_data, &data, "x", &o, &mut Rng::seeded()).is_err());
    }
}
