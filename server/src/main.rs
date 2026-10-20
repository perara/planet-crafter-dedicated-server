//! Standalone Planet Crafter server: no game process. Accepts invite-code players over Steam
//! networking (as a Steam game server, no Steam user needed) and direct-IP players over Unity
//! Transport, and runs the game's server side, ported from the game (see game.rs), on the
//! world in the save file, with the game's tables for this version (data/<version>/).
//!
//! It runs as two processes (docs/hot-update.md): the front (front/: the sockets, Steam, the
//! admin API, the chat hooks) and a bank (bank/: the world), so a new release can take over
//! the running world without a player noticing.
//!
//! Usage: pcds serve DATA_DIR SAVE.json   (the front; it starts the bank)
//!        pcds bank DATA_DIR SAVE.json    (a bank; only the front starts it)
//!        pcds bank-info                  (the front protocol and snapshot formats, as JSON)
//!        pcds new-save SAVE.json DATA_DIR [--planet ID] [--start LOCATION] [--seed N]
//!                  [--set KEY=VALUE ...]   (a new world, see newworld.rs)
//!        pcds --version [DATA_DIR]
//!        pcds probe HOST:PORT DATA_DIR SCRIPT.json   (a scripted test player)
//!        pcds load HOST:PORT DATA_DIR [PLAYERS] [SECONDS] [ACTION_MS] [NAME]   (a load test, see load.rs)
//!        pcds read-game GAME_DIR OUT_DIR   (a game version's tables, see native/)
//!        pcds COMMAND ...   (installing and running a server: cli/)
//! Environment: a dozen PCDS_* variables, all in config.rs (KNOBS for server.env, PROCESS_ENV for
//! the installation, DEV_HOOKS; docs/env.md is written from
//! them, the README's Settings tables repeat them, and tests keep all of it equal). Gameplay
//! and tuning are rules (rules.rs), not variables.

// Style lints this code base accepts on purpose; each is a matter of taste, not of behavior:
// - collapsible_if, collapsible_match: the nesting mirrors the game's decompiled host code line
//   for line (parity work reads the two side by side), and collapsing needs let chains everywhere.
// - type_complexity, too_many_arguments: tuples and argument lists that are the game's own
//   signatures (World object drops, S3 upload); a named type would only hide them.
// - needless_range_loop, explicit_counter_loop, neg_multiply, wrong_self_convention: parity ports
//   keep the game's index loops and its `x * -1.0` terms; the numbers are bit-for-bit the same.
// - chunks_exact_to_as_chunks: as_chunks is newer than the 1.88 MSRV this crate declares.
#![allow(
    clippy::collapsible_if,
    clippy::collapsible_match,
    clippy::type_complexity,
    clippy::too_many_arguments,
    clippy::needless_range_loop,
    clippy::explicit_counter_loop,
    clippy::neg_multiply,
    clippy::wrong_self_convention,
    clippy::chunks_exact_to_as_chunks,
    clippy::some_filter
)]

#![cfg_attr(fuzzing, no_main)]

/// `section!("name", expr)` is `expr`; the bench (PCDS_BENCH_SECTIONS=1) also adds up the time of each name.
#[cfg(test)]
macro_rules! section {
    ($name:expr, $e:expr) => {{
        let started = crate::bench::start();
        let value = $e;
        crate::bench::stop($name, started);
        value
    }};
}
#[cfg(not(test))]
macro_rules! section {
    ($name:expr, $e:expr) => {
        $e
    };
}

// The log's macros (info!, warn!, error!, debug!, trace!) are used by every module after this line.
#[macro_use]
mod logging;
#[cfg(any(fuzzing, pcds_corpus))]
#[path = "../fuzz/entry.rs"]
mod fuzz_entry;
mod animals;
#[cfg(test)]
mod bench;
mod access;
mod admin;
mod bank;
mod bincodec;
mod compact;
mod clock;
mod chathooks;
mod cli;
#[cfg(test)]
mod coverage;
#[cfg(test)]
mod scenario;
#[cfg(test)]
mod timetravel;
#[cfg(test)]
mod testkit;
mod config;
mod dashboard;
mod data;
mod doc;
mod deconstruct;
mod dotnet_sort;
mod energy;
mod equipment;
mod events;
mod ext;
mod facts;
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
mod msg;
mod net;
mod newworld;
mod objects;
mod oldsave;
mod panels;
mod planets;
mod plugins;
mod players;
mod probe;
mod rules;
mod s3;
mod protocol;
mod rockets;
mod routes;
mod rpcspec;
mod save;
mod tables;
mod scene;
mod status;
mod tessera;
mod tokens;
mod toxic;
mod transform;
mod units;
mod unlocks;
mod updater;
mod util;
mod utp;
mod water;
mod wire;
mod worlds;
mod world;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use data::NetData;
use gamedata::GameData;
use rpcspec::Spec;

/// Autosave interval bounds, in minutes (larger ones overflow the clock).
const MAX_AUTOSAVE: u64 = 1440;

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// The game's tables in DATA_DIR (data/<version>/); without them nothing runs.
fn tables(dir: &str) -> (NetData, GameData, Spec) {
    try_tables(dir).unwrap_or_else(|e| no_tables(dir, e))
}

/// tables(), with the reason as an error (`pcds doctor` and `pcds backup verify` say it).
fn try_tables(dir: &str) -> Result<(NetData, GameData, Spec), String> {
    let (net, spec) = try_network_tables(dir)?;
    let data = GameData::load(&format!("{dir}/gamedata.json"))?;
    facts::check(&data, &spec)?;
    Ok((net, data, spec))
}

/// The network tables alone: all a scripted player needs (the extraction joins with them
/// before the game data is written).
fn network_tables(dir: &str) -> (NetData, Spec) {
    try_network_tables(dir).unwrap_or_else(|e| no_tables(dir, e))
}

fn try_network_tables(dir: &str) -> Result<(NetData, Spec), String> {
    check_format(dir)?;
    Ok((NetData::load(&format!("{dir}/netdata.json"))?, Spec::load(&format!("{dir}/rpcs.json"))?))
}

/// Tables another reader wrote (another format, or none) are not loaded: their files may say
/// something else than this server reads them as.
fn check_format(dir: &str) -> Result<(), String> {
    let path = format!("{dir}/tables-format");
    let ours = native::tables::TABLES_FORMAT;
    let format = std::fs::read_to_string(&path).map_err(|e| format!("{path}: {e}; this server reads tables format {ours}"))?;
    if format.trim() != ours.to_string() {
        return Err(format!("they are of tables format {}, this server reads format {ours}", format.trim()));
    }
    Ok(())
}

fn no_tables(dir: &str, e: String) -> ! {
    error!("the game tables in {dir} cannot be used: run `pcds update`, which reads them again (see README) ({e})");
    std::process::exit(1)
}

/// What a world needs besides tables(): the wreck layouts and every planet's scene dump, read at
/// start, so a missing or broken file stops the server there and not in the game. (Default: none,
/// the stand-in a restored game holds until handoff::restore gives it its own.)
#[derive(Default, Clone)]
pub struct Scenes {
    pub wrecks: Arc<instances::generate::Wrecks>,
    /// Planet id -> its scene dump. The free planets' are always there; a DLC planet's is there
    /// when the extraction read it from an install that has the planet (`pcds extract` always
    /// does) and not in tables from before DLC support (planets::ground_missing says so).
    pub planets: Planets,
}

/// The scene dumps by planet. Parsed, a dump is four times the size of its text (8.8 MiB for the
/// six), and the server reads one when a planet's water, ore veins and portals are made: at a start
/// or a travel. So each is kept as compact text (all of them checked when the tables load) and
/// parsed the first time a planet asks, and then kept.
#[derive(Default, Clone)]
pub struct Planets(HashMap<String, Dump>);

#[derive(Clone)]
struct Dump {
    text: Arc<str>,
    parsed: Arc<std::sync::OnceLock<PlanetScene>>,
}

impl Planets {
    /// The planet's scene dump; None for a planet the tables have none of.
    pub fn get(&self, planet: &str) -> Option<&PlanetScene> {
        let dump = self.0.get(planet)?;
        Some(dump.parsed.get_or_init(|| read_dump(&dump.text).unwrap_or_else(|e| panic!("scenes/{planet}.json read fine when the tables loaded: {e}"))))
    }

    pub fn contains_key(&self, planet: &str) -> bool {
        self.0.contains_key(planet)
    }

    /// The dumps parsed so far (a planet the world has been on).
    #[cfg(test)]
    pub fn parsed(&self) -> usize {
        self.0.values().filter(|d| d.parsed.get().is_some()).count()
    }

    /// Every dump, parsed.
    #[cfg(test)]
    pub fn iter(&self) -> impl Iterator<Item = (&String, &PlanetScene)> {
        self.0.keys().filter_map(|k| Some((k, self.get(k)?)))
    }

    #[cfg(test)]
    pub fn values(&self) -> impl Iterator<Item = &PlanetScene> {
        self.iter().map(|(_, s)| s)
    }

    #[cfg(test)]
    pub fn insert(&mut self, planet: String, scene: PlanetScene) {
        let parsed = std::sync::OnceLock::new();
        let _ = parsed.set(scene);
        self.0.insert(planet, Dump { text: "".into(), parsed: Arc::new(parsed) });
    }

    #[cfg(test)]
    pub fn remove(&mut self, planet: &str) -> Option<PlanetScene> {
        let scene = self.get(planet)?.clone();
        self.0.remove(planet);
        Some(scene)
    }

    #[cfg(test)]
    pub fn get_mut(&mut self, planet: &str) -> Option<&mut PlanetScene> {
        self.get(planet)?;
        let dump = self.0.get_mut(planet)?;
        Arc::make_mut(&mut dump.parsed).get_mut()
    }
}

#[cfg(test)]
impl std::ops::Index<&str> for Planets {
    type Output = PlanetScene;
    fn index(&self, planet: &str) -> &PlanetScene {
        self.get(planet).unwrap_or_else(|| panic!("no scene dump for {planet}"))
    }
}

/// The parts of scenes/<planet>.json the server reads.
#[derive(Clone)]
pub struct PlanetScene {
    pub water_volumes: Vec<doc::Doc>,
    pub ore_veins: Vec<doc::Doc>,
    /// ToxicArea components (Toxicity only; empty elsewhere).
    pub toxic_areas: Vec<doc::Doc>,
    pub instance_scenes: HashMap<String, Arc<instances::generate::InstanceScene>>,
}

/// What a scene dump's text says that the server runs.
fn read_dump(text: &str) -> Result<PlanetScene, serde_json::Error> {
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Dump {
        water_volumes: Vec<doc::Doc>,
        ore_veins: Vec<doc::Doc>,
        #[serde(default)]
        toxic_areas: Vec<doc::Doc>,
        instance_scenes: HashMap<String, instances::generate::InstanceScene>,
    }
    let dump: Dump = serde_json::from_str(text)?;
    Ok(PlanetScene { water_volumes: dump.water_volumes, ore_veins: dump.ore_veins, toxic_areas: dump.toxic_areas, instance_scenes: dump.instance_scenes.into_iter().map(|(k, v)| (k, Arc::new(v))).collect() })
}

/// scenes/<planet>.json: read and checked now, so that a broken file stops the start; kept as
/// text (the parse is dropped).
fn planet_scene(dir: &str, planet: &str, data: &GameData) -> Result<Dump, String> {
    let path = format!("{dir}/scenes/{planet}.json");
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{path}: {e}"))?;
    let dump = read_dump(&text).map_err(|e| format!("{path}: {e}"))?;
    let asset = format!("PlanetData:PlanetData-{planet}");
    let planet_data = data.asset(&asset).ok_or(format!("gamedata.json: no {asset}"))?;
    // A planet without portals has no instance data (null); one with it lists its scenes.
    let names: Vec<&str> = match &planet_data["instanceScenesData"] {
        doc::Doc::Null => Vec::new(),
        r => {
            let names = r.as_str().and_then(|r| data.asset(r)).and_then(|d| d["instanceSceneNames"].as_array());
            let names = names.ok_or(format!("gamedata.json: {asset}.instanceScenesData {r} has no instanceSceneNames"))?;
            names.iter().map(|n| n.as_str().ok_or(format!("gamedata.json: {asset}: an instance scene name that is not text"))).collect::<Result<_, _>>()?
        }
    };
    if let Some(missing) = names.iter().find(|n| !dump.instance_scenes.contains_key(**n)) {
        return Err(format!("{path}: no world instance scene {missing}"));
    }
    check_scene(&dump.water_volumes, &dump.ore_veins, &dump.toxic_areas, data).map_err(|e| format!("{path}: {e}"))?;
    Ok(Dump { text: Arc::from(&*util::compact_json(&text)), parsed: Default::default() })
}

/// wrecks.json and scenes/<planet>.json of every planet in DATA_DIR, with every world instance
/// scene the planet's portals open. A free planet's file must be there; a DLC planet's may be
/// missing (tables read before the extraction knew the DLC planets).
fn scene_tables(dir: &str, net: &NetData, data: &GameData) -> Scenes {
    try_scene_tables(dir, net, data).unwrap_or_else(|e| no_tables(dir, e))
}

/// scene_tables(), with the reason as an error.
fn try_scene_tables(dir: &str, net: &NetData, data: &GameData) -> Result<Scenes, String> {
    let wrecks = std::fs::read_to_string(format!("{dir}/wrecks.json")).map_err(|e| format!("{dir}/wrecks.json: {e}"))?;
    let wrecks = instances::generate::Wrecks::parse(&wrecks).map_err(|e| format!("{dir}/wrecks.json: {e}"))?;
    let mut planets = HashMap::new();
    for planet in &net.planets {
        if planet.steam_app_id != 0 && !std::path::Path::new(&format!("{dir}/scenes/{}.json", planet.id)).is_file() {
            continue;
        }
        planets.insert(planet.id.clone(), planet_scene(dir, &planet.id, data)?);
    }
    Ok(Scenes { wrecks: Arc::new(wrecks), planets: Planets(planets) })
}

/// What water.rs, veins.rs and toxic.rs read from a scene dump is complete: story interpolations
/// with their world object, offset and easing; ore vein groups that name groups; toxic areas
/// with a place, a time and scene objects of known groups.
fn check_scene(water: &[doc::Doc], veins: &[doc::Doc], toxic: &[doc::Doc], data: &GameData) -> Result<(), String> {
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
    for (i, a) in toxic.iter().enumerate() {
        let f = &a["fields"];
        let place = a["position"].as_array().is_some_and(|p| p.len() == 3 && p.iter().all(|x| x.is_number()));
        let objects = f["_toxicObjects"].as_array().filter(|o| !o.is_empty());
        if !place || !f["_toxicTimeInSeconds"].is_number() || objects.is_none() {
            return Err(format!("toxicAreas[{i}]: needs a position, _toxicTimeInSeconds and _toxicObjects"));
        }
        for o in objects.into_iter().flatten() {
            if !o["worldObjectId"].is_i64() || o["group"].as_str().and_then(|r| data.group_of_ref(r)).is_none() {
                return Err(format!("toxicAreas[{i}]: scene object {o} has no id or a group that is not in gamedata.json"));
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
        Some("bank-info") => println!("{}", msg::json(&bank::info())),
        Some("probe") => probe::run(&args[2..]),
        Some("load") => load::run(&args[2..]),
        Some("read-game") => native::tables::run(&args[2..]),
        Some("tables-diff") => native::diff::run(&args[2..]),
        Some("native-layout") => native::tables::layout(&args[2..]),
        Some("native-il") => native::tables::il(&args[2..]),
        Some("native-classes") => native::scenes::classes(&args[2..]),
        Some("native-fields") => native::scenes::fields_of(&args[2..]),
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

