//! What the admin page shows beyond the status and the configuration (config.rs): the players
//! with their state and position, the worlds and backups beside the save, and the buildings for
//! the map; and the request bodies of the admin's actions on players (teleport, a chat message to
//! everyone). The process's CPU and memory and the network traffic are the front's (front/metrics.rs).


use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::game::Game;

/// The most buildings the map gets (spread over all of them).
const MAP_POINTS: usize = 4000;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Teleport {
    /// The client id; or the player's name (any case).
    pub id: Option<u64>,
    pub name: Option<String>,
    pub position: [f32; 3],
}


/// A request body as its type, or the API's 400 answer.
pub fn body<T: DeserializeOwned>(v: &Value) -> Result<T, (u16, String)> {
    serde_json::from_value(v.clone()).map_err(|e| (400, json!({ "error": e.to_string() }).to_string()))
}

/// To a tenth, so JSON does not carry an f32's float noise.
fn tenth(v: f32) -> f64 {
    (v as f64 * 10.0).round() / 10.0
}

/// A connected client as the page lists it; `via` is its IP or "steam".
pub fn player(game: &Game, c: &crate::game::Client, via: String) -> Value {
    let object = c.player.filter(|_| c.ready.is_some());
    let status = match object {
        None => "joining",
        Some(o) if crate::status::is_dying(game, o) => "dying",
        Some(_) => "ready",
    };
    json!({
        "id": c.id,
        "name": c.ready.as_ref().map(|s| s.name.clone()),
        "steamId": c.verified_steam_id.map(|i| i.to_string()),
        "verified": c.verified_steam_id.is_some(),
        "ip": via,
        "connectedSeconds": c.since.elapsed().as_secs(),
        "status": status,
        "position": object.and_then(|o| game.net.objects.get(&o)).map(|o| o.position.map(tenth)),
        // Oxygen, water, health, toxic.
        "gauges": object.map(|o| crate::gauges::values(game, o).map(tenth)),
        "gaugeMax": object.map(|o| crate::gauges::maxima(game, o).map(tenth)),
    })
}

pub use crate::worlds::overview as worlds;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewWorld {
    pub name: String,
    pub planet: Option<String>,
    pub start: Option<String>,
    pub seed: Option<u32>,
    pub mode: Option<String>,
    pub dying: Option<String>,
}

/// POST /api/worlds/new: a new world file beside the running one (never over an existing one).
pub fn world_new(game: &Game, save_path: &str, w: NewWorld) -> Result<Value, String> {
    let name = crate::worlds::name(&w.name)?;
    let path = crate::worlds::file(save_path, &name);
    if path.exists() {
        return Err(format!("world {name} exists: pick another name"));
    }
    let mut set = Vec::new();
    if let Some(m) = w.mode {
        set.push(("mode".to_string(), m));
    }
    if let Some(d) = w.dying {
        set.push(("dyingConsequencesLabel".to_string(), d));
    }
    let o = crate::newworld::Options { planet: w.planet, start: w.start.unwrap_or_else(|| "Standard".into()), seed: w.seed, set };
    let save = crate::newworld::new_save(&game.net_data, &game.data, &name, &o, &mut crate::game::Rng::seeded())?;
    save.write(&path.to_string_lossy()).map_err(|e| format!("{}: {e}", path.display()))?;
    info!("world {name} created (admin)");
    Ok(json!({"name": name, "planet": save.settings["planetId"], "seed": save.settings["worldSeed"]}))
}

/// POST /api/worlds/remove: moved into backups/ as <name>-removed-<time>.json, never deleted;
/// the running world is refused.
pub fn world_remove(save_path: &str, name: &str) -> Result<Value, String> {
    let name = crate::worlds::name(name)?;
    if name == crate::worlds::stem(save_path) {
        return Err(format!("the server runs {name}: switch to another world first"));
    }
    let from = crate::worlds::file(save_path, &name);
    if !from.is_file() {
        return Err(format!("no world {name}"));
    }
    let backups = crate::worlds::backups(save_path);
    std::fs::create_dir_all(&backups).map_err(|e| e.to_string())?;
    let to = backups.join(format!("{}.json", crate::worlds::kept_stem(&name, "removed", std::time::SystemTime::now())));
    if to.exists() {
        return Err("removed a moment ago: try again in a second".into());
    }
    std::fs::rename(&from, &to).map_err(|e| format!("{}: {e}", to.display()))?;
    info!("world {name} removed (admin): kept as {}", to.display());
    Ok(json!({"kept": to.file_name().map(|f| f.to_string_lossy().to_string())}))
}

/// A world's or a backup's file, by name: worlds/<name>.json or backups/<name>.json.
fn file_of(save_path: &str, name: &str, backup: bool) -> Result<std::path::PathBuf, String> {
    let name = crate::worlds::name(name)?;
    let path = if backup { crate::worlds::backups(save_path).join(format!("{name}.json")) } else { crate::worlds::file(save_path, &name) };
    if !path.is_file() {
        return Err(format!("no {} {name}", if backup { "backup" } else { "world" }));
    }
    Ok(path)
}

/// GET /api/worlds/file?name=N[&backup=1]: the file's text, for the page to save.
pub fn world_file(save_path: &str, name: &str, backup: bool) -> Result<Value, String> {
    let path = file_of(save_path, name, backup)?;
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(json!({"name": format!("{name}.json"), "text": text}))
}

/// POST /api/worlds/restore {"backup": B, "name": N}: a backup as a new world N.
pub fn world_restore(save_path: &str, backup: &str, name: &str) -> Result<Value, String> {
    let from = file_of(save_path, backup, true)?;
    let name = crate::worlds::name(name)?;
    let to = crate::worlds::file(save_path, &name);
    if to.exists() {
        return Err(format!("world {name} exists: pick another name"));
    }
    // A backup this server cannot read (an old layout, cut short) is not made a world.
    crate::save::SaveFile::load(&from.to_string_lossy()).map_err(|e| format!("backup {backup}: {e}"))?;
    let text = std::fs::read(&from).map_err(|e| format!("{}: {e}", from.display()))?;
    crate::save::write_atomic(&to.to_string_lossy(), &text).map_err(|e| format!("{}: {e}", to.display()))?;
    info!("backup {backup} restored as world {name} (admin)");
    Ok(json!({"name": name}))
}

/// The buildings on this planet as [x, y, z], for the players' map (y: where a teleport by the
/// map lands near them).
pub fn map(game: &Game) -> Value {
    let planet = game.planet_hash();
    let points: Vec<[f64; 3]> = game.wos.values().filter(|w| w.planet_hash() == planet && game.data.groups[w.group].constructible).map(|w| w.position.map(tenth)).collect();
    let step = points.len().div_ceil(MAP_POINTS).max(1);
    json!({ "planet": game.planet_id(), "buildings": points.into_iter().step_by(step).collect::<Vec<_>>() })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A backup in a layout this server does not read is not restored as a world.
    #[test]
    fn a_backup_the_server_cannot_read_is_not_restored() {
        let dir = std::env::temp_dir().join(format!("pcds-restore-test-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("backups")).unwrap();
        let save = dir.join("world.json").to_string_lossy().to_string();
        std::fs::write(dir.join("backups/world-20261018-120000.json"), crate::save::SaveFile::old_format_text()).unwrap();
        std::fs::write(dir.join("backups/world-20261018-130000.json"), crate::save::SaveFile::tiny_text("w")).unwrap();
        let err = world_restore(&save, "world-20261018-120000", "old").unwrap_err();
        assert!(err.contains("old save format") && err.contains("pcds world import"), "{err}");
        assert!(!dir.join("old.json").exists());
        assert!(world_restore(&save, "world-20261018-130000", "good").is_ok() && dir.join("good.json").is_file());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
