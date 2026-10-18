//! Standalone Planet Crafter server: no game process. Accepts invite-code players over Steam
//! networking (as a Steam game server, no Steam user needed) and direct-IP players over Unity
//! Transport, and runs the game's server side, ported from the game (see game.rs), on the
//! world in the save file, with the game's tables for this version (data/<version>/).
//!
//! It runs as two processes (docs/hot-update.md): the front (front.rs: the sockets, Steam, the
//! admin API, the chat hooks) and a bank (bank.rs: the world), so a new release can take over
//! the running world without a player noticing.
//!
//! Usage: pcds serve DATA_DIR SAVE.json   (the front; it starts the bank)
//!        pcds bank DATA_DIR SAVE.json    (a bank; only the front starts it)
//!        pcds bank-info                  (the front protocol and snapshot formats, as JSON)
//!        pcds new-save SAVE.json DATA_DIR [--planet ID] [--start LOCATION] [--seed N]
//!                  [--set KEY=VALUE ...]   (a new world, see newworld.rs)
//!        pcds --version [DATA_DIR]
//!        pcds probe HOST:PORT DATA_DIR SCRIPT.json   (a scripted test player)
//!        pcds load HOST:PORT DATA_DIR [PLAYERS] [SECONDS]   (a load test, see load.rs)
//!        pcds read-game GAME_DIR OUT_DIR   (a game version's tables, see native/)
//!        pcds COMMAND ...   (installing and running a server: cli/)
//! Environment:
//!   PCDS_LISTEN       direct-IP address, default 0.0.0.0:7777; "off" disables
//!   PCDS_STEAM        "off" disables invite codes (Steam game server)
//!   PCDS_STEAM_TOKEN  game server login token: a stable invite code (default: anonymous,
//!                     the code changes every start)
//!   PCDS_SESSION      the 4-digit session number at the end of the invite code, default 4242
//!   PCDS_AUTOSAVE     minutes between saves, default 5 (0: only on stop and through the API)
//!   PCDS_BACKUPS      copies of the save kept in backups/ next to it, default 24 (0: none)
//!   PCDS_BACKUP_MINUTES  minutes between those copies, default 60
//!   PCDS_ADMIN        admin API and web GUI address, default 127.0.0.1:7782; "off" disables
//!   PCDS_ADMIN_TOKEN  the admin API's bearer token (the API is off without one)
//!   PCDS_ACCESS       access lists file, default access.json next to the save
//!   PCDS_RULES        server rules file, default rules.json next to the save (rules.rs)
//!   PCDS_WEB          the web GUI page, default web/index.html next to the live bank's binary
//!   PCDS_TRACE        log every incoming message and RPC
//!   PCDS_UNIT         the systemd user unit pcds runs it as (the admin page's log)
//!   PCDS_BANKS        the banks' folder (run/bin; pcds run sets it), see updater.rs
//!   PCDS_AUTO_UPDATE  "on": install new releases and flip to them; PCDS_UPDATE_CHECK_HOURS
//!   PCDS_RELEASES     where releases are looked up, default GitHub's API for this project

mod animals;
#[cfg(test)]
mod bench;
mod access;
mod admin;
mod bank;
mod chathooks;
mod cli;
#[cfg(test)]
mod testkit;
mod config;
mod dashboard;
mod data;
mod deconstruct;
mod energy;
mod equipment;
mod ext;
mod front;
mod game;
mod gamedata;
mod gauges;
mod genetics;
mod geometry;
mod handlers;
mod idmap;
mod handoff;
mod hooks;
mod instances;
mod inventories;
mod ipc;
mod load;
mod logistics;
mod native;
mod machines;
mod meteo;
mod model;
mod net;
mod newworld;
mod objects;
mod panels;
mod planets;
mod players;
mod probe;
mod rules;
mod protocol;
mod rockets;
mod rpcspec;
mod save;
mod scene;
mod status;
mod toxic;
mod transform;
mod units;
mod unlocks;
mod updater;
mod utp;
mod water;
mod wire;
mod world;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use data::NetData;
use gamedata::GameData;
use rpcspec::Spec;

/// DisconnectReasonMessage: shown to a kicked or refused player.
const DISCONNECT_REASON: u32 = 8;
/// Autosave interval bounds, in minutes (larger ones overflow the clock).
const MAX_AUTOSAVE: u64 = 1440;

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// The autosave interval a start begins with: PCDS_AUTOSAVE, else 5 minutes.
fn autosave_start() -> u64 {
    env("PCDS_AUTOSAVE").and_then(|s| s.parse().ok()).unwrap_or(5).min(MAX_AUTOSAVE)
}

/// The game's tables in DATA_DIR (data/<version>/); without them nothing runs.
fn tables(dir: &str) -> (NetData, GameData, Spec) {
    let (net, spec) = network_tables(dir);
    let data = GameData::load(&format!("{dir}/gamedata.json")).unwrap_or_else(|e| no_tables(dir, e));
    (net, data, spec)
}

/// The network tables alone: all a scripted player needs (the extraction joins with them
/// before the game data is written).
fn network_tables(dir: &str) -> (NetData, Spec) {
    let load = || Ok::<_, String>((NetData::load(&format!("{dir}/netdata.json"))?, Spec::load(&format!("{dir}/rpcs.json"))?));
    load().unwrap_or_else(|e| no_tables(dir, e))
}

fn no_tables(dir: &str, e: String) -> ! {
    eprintln!("no game tables in {dir}: run `pcds extract` (see README) ({e})");
    std::process::exit(1)
}

/// What a world needs besides tables(): the wreck layouts and every free planet's scene dump,
/// read at start, so a missing or broken file stops the server there and not in the game.
/// (Default: none, the stand-in a restored game holds until handoff::restore gives it its own.)
#[derive(Default, Clone)]
pub struct Scenes {
    pub wrecks: Arc<instances::generate::Wrecks>,
    /// Free planet id -> its scene dump. DLC planets have none: the extraction reads only the
    /// free ones, and nobody can travel to the others (planets.rs).
    pub planets: HashMap<String, PlanetScene>,
}

/// The parts of scenes/<planet>.json the server reads.
#[derive(Clone)]
pub struct PlanetScene {
    pub water_volumes: Vec<serde_json::Value>,
    pub ore_veins: Vec<serde_json::Value>,
    pub instance_scenes: HashMap<String, Arc<instances::generate::InstanceScene>>,
}

/// wrecks.json and scenes/<planet>.json of every free planet in DATA_DIR, with every world
/// instance scene the planet's portals open.
fn scene_tables(dir: &str, net: &NetData, data: &GameData) -> Scenes {
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Dump {
        water_volumes: Vec<serde_json::Value>,
        ore_veins: Vec<serde_json::Value>,
        instance_scenes: HashMap<String, instances::generate::InstanceScene>,
    }
    let read = |path: &str| std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"));
    let load = || -> Result<Scenes, String> {
        let wrecks = instances::generate::Wrecks::parse(&read(&format!("{dir}/wrecks.json"))?).map_err(|e| format!("{dir}/wrecks.json: {e}"))?;
        let mut planets = HashMap::new();
        for planet in net.planets.iter().filter(|p| p.steam_app_id == 0) {
            let path = format!("{dir}/scenes/{}.json", planet.id);
            let dump: Dump = serde_json::from_str(&read(&path)?).map_err(|e| format!("{path}: {e}"))?;
            let asset = format!("PlanetData:PlanetData-{}", planet.id);
            let planet_data = data.asset(&asset).ok_or(format!("gamedata.json: no {asset}"))?;
            // A planet without portals has no instance data (null); one with it lists its scenes.
            let names: Vec<&str> = match &planet_data["instanceScenesData"] {
                serde_json::Value::Null => Vec::new(),
                r => {
                    let names = r.as_str().and_then(|r| data.asset(r)).and_then(|d| d["instanceSceneNames"].as_array());
                    let names = names.ok_or(format!("gamedata.json: {asset}.instanceScenesData {r} has no instanceSceneNames"))?;
                    names.iter().map(|n| n.as_str().ok_or(format!("gamedata.json: {asset}: an instance scene name that is not text"))).collect::<Result<_, _>>()?
                }
            };
            if let Some(missing) = names.iter().find(|n| !dump.instance_scenes.contains_key(**n)) {
                return Err(format!("{path}: no world instance scene {missing}"));
            }
            check_scene(&dump.water_volumes, &dump.ore_veins, data).map_err(|e| format!("{path}: {e}"))?;
            let instance_scenes = dump.instance_scenes.into_iter().map(|(k, v)| (k, Arc::new(v))).collect();
            planets.insert(planet.id.clone(), PlanetScene { water_volumes: dump.water_volumes, ore_veins: dump.ore_veins, instance_scenes });
        }
        Ok(Scenes { wrecks: Arc::new(wrecks), planets })
    };
    load().unwrap_or_else(|e| no_tables(dir, e))
}

/// What water.rs and veins.rs read from a scene dump is complete: story interpolations with
/// their world object, offset and easing; ore vein groups that name groups.
fn check_scene(water: &[serde_json::Value], veins: &[serde_json::Value], data: &GameData) -> Result<(), String> {
    for (i, v) in water.iter().enumerate() {
        let s = &v["storyInterpolation"];
        let offset = s["offset"].as_array().is_some_and(|o| o.len() == 3 && o.iter().all(|x| x.is_number()));
        if !s.is_null() && !(s["worldObject"].is_i64() && offset && s["ease"].is_boolean()) {
            return Err(format!("waterVolumes[{i}].storyInterpolation: {s}"));
        }
    }
    for (i, v) in veins.iter().enumerate() {
        for r in v["fields"]["groups"].as_array().into_iter().flatten() {
            if r.as_str().and_then(|r| data.group_of_ref(r)).is_none() {
                return Err(format!("oreVeins[{i}]: group {r} is not in gamedata.json"));
            }
        }
    }
    Ok(())
}

/// The tables tests run on: PCDS_TEST_DATA, else run/data/2.103 or data/2.103. Each user
/// extracts them from their own game, so a checkout has none (CI): PCDS_TEST_DATA=none says so,
/// and the tests that need them skip. Tables that cannot be found otherwise fail the test.
#[cfg(test)]
fn test_data() -> Option<&'static str> {
    static DIR: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    DIR.get_or_init(|| match env("PCDS_TEST_DATA").as_deref() {
        Some("none") => None,
        Some(dir) if std::path::Path::new(dir).join("netdata.json").is_file() => Some(dir.to_string()),
        Some(dir) => panic!("PCDS_TEST_DATA={dir}: no game tables there"),
        None => {
            let dirs = [concat!(env!("CARGO_MANIFEST_DIR"), "/../run/data/2.103"), concat!(env!("CARGO_MANIFEST_DIR"), "/data/2.103")];
            let found = dirs.iter().find(|d| std::path::Path::new(d).join("netdata.json").is_file());
            Some(found.unwrap_or_else(|| panic!("no game tables in {dirs:?}: set PCDS_TEST_DATA to a tables directory, or to none to skip the tests that need them")).to_string())
        }
    })
    .as_deref()
}

/// The pause between frames, as the game's: timers, the game tick and Steam run once per frame.
const FRAME: Duration = Duration::from_millis(5);
/// Other players' movement goes out at most this often (a 60 fps host's rate; clients
/// interpolate movement), everything else at once.
const MOVEMENT_EVERY: Duration = Duration::from_millis(16);

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("serve") => front::run(&args[2..]),
        Some("bank") => bank::run(&args[2..]),
        Some("bank-info") => println!("{}", bank::info()),
        Some("probe") => probe::run(&args[2..]),
        Some("load") => load::run(&args[2..]),
        Some("read-game") => native::tables::run(&args[2..]),
        Some("tables-diff") => native::diff::run(&args[2..]),
        Some("native-layout") => native::tables::layout(&args[2..]),
        Some("native-il") => native::tables::il(&args[2..]),
        Some("native-wrecks") => native::wrecks::debug(&args[2..]),
        Some("native-reach") => native::wrecks::reach(&args[2..]),
        Some("new-save") => newworld::run(&args[2..]),
        Some("--version") => {
            println!("pcds {}", env!("CARGO_PKG_VERSION"));
            if let Some(dir) = args.get(2) {
                println!("game tables {}", tables(dir).0.game_version);
            }
        }
        _ => cli::run(&args[1..]),
    }
}

