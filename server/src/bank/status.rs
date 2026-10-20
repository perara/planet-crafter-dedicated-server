//! The bank's part of GET /api/status (the front adds its own: invite code, network, banks,
//! traffic), as typed structs: the field names and their order are the API's (golden tests below
//! hold them against the hand-built JSON the status was). The pieces: the tick times of the last
//! minute (`TickTimes`), the backups' and the last save's outcome (persist.rs keeps them), the
//! tables and the installed game, and S3 (s3.rs).

use std::time::{Duration, Instant};

use indexmap::IndexMap;
use serde::Serialize;
use serde_json::{json, Value};

use super::persist::{self, LastSave};
use crate::game::Game;
use crate::{dashboard, meteo, plugins, rules, units};

/// The time of the game's ticks over the last minute, for GET /api/status: a ring of fixed size
/// (one sample is a tick's start and duration), filled in place, so a tick costs one store and no
/// allocation. The ring holds more than a minute of ticks (one every FRAME).
pub struct TickTimes {
    start: Instant,
    ring: Box<[(u32, u32)]>,
    next: usize,
}

/// How long a tick is remembered.
const TICK_WINDOW: Duration = Duration::from_secs(60);
/// Samples held: a minute of 5 ms ticks and a third more.
const TICK_SAMPLES: usize = 16_384;

impl Default for TickTimes {
    fn default() -> Self {
        TickTimes { start: Instant::now(), ring: vec![(u32::MAX, 0); TICK_SAMPLES].into_boxed_slice(), next: 0 }
    }
}

impl TickTimes {
    /// A tick that began at `at` and took `took`: milliseconds since this ring began and
    /// microseconds, as u32 (49 days; 71 minutes).
    pub(super) fn record(&mut self, at: Instant, took: Duration) {
        let ms = at.saturating_duration_since(self.start).as_millis().min(u32::MAX as u128 - 1) as u32;
        self.ring[self.next] = (ms, took.as_micros().min(u32::MAX as u128) as u32);
        self.next = (self.next + 1) % self.ring.len();
    }

    /// p50, p99 and the longest tick of the last TICK_WINDOW before `now`, in microseconds (null
    /// when there is none). Made on request: it sorts a copy.
    pub fn summary(&self, now: Instant) -> TickSummary {
        let since = now.saturating_duration_since(self.start).as_millis().saturating_sub(TICK_WINDOW.as_millis()) as u64;
        let mut micros: Vec<u32> = self.ring.iter().filter(|(at, _)| *at != u32::MAX && u64::from(*at) >= since).map(|(_, took)| *took).collect();
        micros.sort_unstable();
        // The nearest-rank percentile.
        let at = |p: usize| micros.get((micros.len() * p).div_ceil(100).saturating_sub(1)).copied();
        TickSummary { window_seconds: TICK_WINDOW.as_secs(), samples: micros.len(), p50_micros: at(50), p99_micros: at(99), max_micros: micros.last().copied() }
    }
}

/// What the status shows of this bank's loop: loops in the last second, its work time and
/// longest loop.
pub(super) struct Ctx<'a> {
    pub ticks: &'a TickTimes,
    pub save_path: &'a str,
    pub build_id: Option<&'a str>,
    pub data_dir: &'a str,
    pub loop_rate: u32,
    pub busy: (Duration, Duration),
}

/// p50, p99 and the longest tick of the last minute, in microseconds (null when there is none).
#[derive(Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TickSummary {
    pub window_seconds: u64,
    pub samples: usize,
    pub p50_micros: Option<u32>,
    pub p99_micros: Option<u32>,
    pub max_micros: Option<u32>,
}

/// The status' "backup": whether they are on (save.backupsKept), when one was last made since
/// this process started, and the last failure with its time.
#[derive(Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupStatus {
    pub enabled: bool,
    pub last_ok_at: Option<u64>,
    pub error: Option<BackupError>,
}

#[derive(Debug, PartialEq, Serialize)]
pub struct BackupError {
    pub at: u64,
    pub message: String,
}

pub fn backup_status(files: &rules::Save) -> BackupStatus {
    let (ok, failed) = persist::backups();
    BackupStatus { enabled: files.backups_kept > 0, last_ok_at: ok, error: failed.map(|(at, message)| BackupError { at, message }) }
}

/// /api/status's "tables": the version and build these tables are for, the installed game's
/// build (PCDS_GAME_DIR, which pcds run sets), and a warning when that game has no tables of
/// this server's format (a game update `pcds update` has not read yet).
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Tables<'a> {
    version: &'a str,
    build_id: Option<&'a str>,
    game: Value,
    warning: Value,
}

#[derive(Serialize)]
struct UnitValue {
    value: f64,
    #[serde(rename = "perSecond")]
    per_second: f64,
}

#[derive(Serialize)]
struct World<'a> {
    seed: &'a Value,
    settings: &'a Value,
}

#[derive(Serialize)]
struct Objects {
    world: usize,
    network: usize,
    inventories: usize,
}

/// The whole of GET /api/status from the bank.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status<'a> {
    state: &'static str,
    game_version: &'a str,
    save: Option<String>,
    autosave_minutes: u32,
    tick_rate: u32,
    rules: Vec<String>,
    fps: u32,
    busy_percent: f64,
    longest_loop_micros: u64,
    planet: &'a str,
    terraformation: f64,
    stages: Value,
    game_time: f64,
    units: IndexMap<String, UnitValue>,
    players: Vec<Value>,
    access: &'a crate::access::Lists,
    server_version: &'static str,
    tables: Tables<'a>,
    world: World<'a>,
    objects: Objects,
    held: bool,
    s3: crate::s3::Status<'a>,
    plugins: Value,
    last_save: Option<LastSave>,
    tick: TickSummary,
    backup: BackupStatus,
}

/// The status as this bank answers it, at `now` (the tick times are those before it).
pub(super) fn build<'a>(game: &'a Game, ctx: &Ctx<'a>, now: Instant) -> Status<'a> {
    let mut players: Vec<Value> = game.clients.values().map(|c| dashboard::player(game, c, c.ip.clone().unwrap_or_default())).collect();
    players.sort_by_key(|p| p["id"].as_u64());
    // The game's own f32 values go out as the f64 they widen to, as the JSON values always did.
    let planet_units = game.units.planet(game.planet_hash()).map(|p| p.units.iter().map(|u| (u.kind.unit_type().to_string(), UnitValue { value: f64::from(u.total), per_second: f64::from(u.per_sec) })).collect()).unwrap_or_default();
    Status {
        state: "running",
        game_version: &game.net_data.game_version,
        save: std::path::Path::new(ctx.save_path).file_stem().map(|s| s.to_string_lossy().to_string()),
        autosave_minutes: game.rules.save.autosave_minutes,
        tick_rate: game.net_data.tick_rate,
        rules: rules::changes(&game.rules),
        fps: ctx.loop_rate,
        busy_percent: (ctx.busy.0.as_secs_f64() * 1000.0).round() / 10.0,
        longest_loop_micros: ctx.busy.1.as_micros() as u64,
        planet: game.planet_id(),
        terraformation: units::value(game, units::Kind::Terraformation, 0),
        stages: meteo::stages(game),
        game_time: game.time.secs(),
        units: planet_units,
        players,
        access: &game.access.lists,
        server_version: env!("CARGO_PKG_VERSION"),
        tables: tables_status(game, ctx),
        world: World { seed: &game.save.settings["worldSeed"], settings: &game.save.settings },
        objects: Objects { world: game.wos.len(), network: game.net.objects.len(), inventories: game.inventories.len() },
        held: game.held,
        s3: crate::s3::status(&game.rules.backup),
        plugins: plugins::admin::summary(game),
        last_save: persist::last_save(),
        tick: ctx.ticks.summary(now),
        backup: backup_status(&game.rules.save),
    }
}

/// /api/status's "tables": the version and build these tables are for, the installed game's
/// build (PCDS_GAME_DIR, which pcds run sets), and a warning when that game has no tables of
/// this server's format (a game update `pcds update` has not read yet).
fn tables_status<'a>(game: &'a Game, ctx: &Ctx<'a>) -> Tables<'a> {
    let installed = installed_game(ctx.data_dir);
    let warning = installed["warning"].clone();
    Tables { version: &game.net_data.game_version, build_id: ctx.build_id, game: installed, warning }
}

/// The installed game's build and whether it has tables; hashed again only when the game's
/// code file changes (stat on each call). ponytail: tables read after the hash are seen at
/// the next restart, which `pcds update` does once it has read them.
fn installed_game(data_dir: &str) -> Value {
    crate::env("PCDS_GAME_DIR").map_or(Value::Null, |game| game_at(std::path::Path::new(&game), data_dir))
}

fn game_at(dir: &std::path::Path, data_dir: &str) -> Value {
    use std::sync::Mutex;
    static SEEN: Mutex<Option<((std::time::SystemTime, u64), Value)>> = Mutex::new(None);
    let code = dir.join("Planet Crafter_Data/Managed/Assembly-CSharp.dll");
    let key = match std::fs::metadata(&code).and_then(|m| Ok((m.modified()?, m.len()))) {
        Ok(key) => key,
        Err(e) => return json!({"warning": format!("the game is gone ({}: {e}): pcds update", code.display())}),
    };
    let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((k, v)) = seen.as_ref()
        && *k == key
    {
        return v.clone();
    }
    let data = std::path::Path::new(data_dir).parent().unwrap_or(std::path::Path::new(data_dir));
    let v = match crate::cli::build_id(dir) {
        Ok(build) => {
            let missing = crate::cli::find_tables(data, &build, crate::native::tables::TABLES_FORMAT).is_err();
            json!({"buildId": build, "hasTables": !missing, "warning": missing.then(|| format!("game build {build} has no tables: run pcds update"))})
        }
        Err(e) => json!({"warning": e}),
    };
    *seen = Some((key, v.clone()));
    v
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::game;

    fn summary(t: &TickTimes, now: Instant) -> Value {
        serde_json::to_value(t.summary(now)).unwrap()
    }

    /// The status' tick times: p50, p99 and the longest of the last minute, from a ring of fixed size
    /// that a tick fills in place.
    #[test]
    fn tick_times_are_a_percentile_of_the_last_minute_in_a_fixed_ring() {
        let mut t = TickTimes::default();
        let (start, capacity) = (t.start, t.ring.len());
        assert_eq!(summary(&t, start)["samples"], 0);
        assert!(summary(&t, start)["p99Micros"].is_null());
        // 1000 ticks of 1..=1000 us over the first 5 seconds.
        for i in 1..=1000u64 {
            t.record(start + Duration::from_millis(i * 5), Duration::from_micros(i));
        }
        let s = summary(&t, start + Duration::from_secs(5));
        assert_eq!((s["samples"].as_u64(), s["p50Micros"].as_u64(), s["p99Micros"].as_u64(), s["maxMicros"].as_u64()), (Some(1000), Some(500), Some(990), Some(1000)), "{s}");
        // A slow tick is the maximum; p99 moves by one rank only (one tick in 1001).
        t.record(start + Duration::from_secs(6), Duration::from_millis(80));
        let s = summary(&t, start + Duration::from_secs(6));
        assert_eq!((s["maxMicros"].as_u64(), s["p99Micros"].as_u64()), (Some(80_000), Some(991)), "{s}");
        // A minute on, those ticks are gone, and a single new one is the whole picture.
        t.record(start + Duration::from_secs(70), Duration::from_micros(7));
        let s = summary(&t, start + Duration::from_secs(71));
        assert_eq!((s["samples"].as_u64(), s["maxMicros"].as_u64(), s["p50Micros"].as_u64()), (Some(1), Some(7), Some(7)), "{s}");
        // Past its size the ring overwrites the oldest, in the same memory.
        for i in 0..capacity * 2 {
            t.record(start + Duration::from_secs(80) + Duration::from_micros(i as u64), Duration::from_micros(3));
        }
        assert_eq!(t.ring.len(), capacity);
        assert_eq!(summary(&t, start + Duration::from_secs(81))["samples"], capacity);
    }

    /// The status before it had types: the JSON built by hand, field by field. The typed status must
    /// print the same bytes (the API's names, their order, numbers as the f64 of the game's f32).
    fn old_summary(t: &TickTimes, now: Instant) -> Value {
        let since = now.saturating_duration_since(t.start).as_millis().saturating_sub(TICK_WINDOW.as_millis()) as u64;
        let mut micros: Vec<u32> = t.ring.iter().filter(|(at, _)| *at != u32::MAX && u64::from(*at) >= since).map(|(_, took)| *took).collect();
        micros.sort_unstable();
        let at = |p: usize| micros.get((micros.len() * p).div_ceil(100).saturating_sub(1)).copied();
        json!({"windowSeconds": TICK_WINDOW.as_secs(), "samples": micros.len(), "p50Micros": at(50), "p99Micros": at(99), "maxMicros": micros.last()})
    }

    fn old_backup(files: &rules::Save) -> Value {
        let (ok, failed) = persist::backups();
        json!({"enabled": files.backups_kept > 0, "lastOkAt": ok, "error": failed.as_ref().map(|(at, message)| json!({"at": at, "message": message}))})
    }

    fn old_last_save() -> Value {
        match persist::last_save() {
            Some(l) => json!({"ok": l.ok, "at": l.at, "error": l.error.as_ref()}),
            None => Value::Null,
        }
    }

    fn old_tables(game: &Game, ctx: &Ctx) -> Value {
        let installed = installed_game(ctx.data_dir);
        let warning = installed["warning"].clone();
        json!({"version": game.net_data.game_version, "buildId": ctx.build_id, "game": installed, "warning": warning})
    }

    fn old_status(game: &Game, ctx: &Ctx, now: Instant) -> String {
        let mut players: Vec<Value> = game.clients.values().map(|c| dashboard::player(game, c, c.ip.clone().unwrap_or_default())).collect();
        players.sort_by_key(|p| p["id"].as_u64());
        let terraformation = units::value(game, units::Kind::Terraformation, 0);
        let planet_units: serde_json::Map<String, Value> = game.units.planet(game.planet_hash()).map(|p| p.units.iter().map(|u| (u.kind.unit_type().to_string(), json!({"value": u.total, "perSecond": u.per_sec}))).collect()).unwrap_or_default();
        let body = json!({
            "state": "running",
            "gameVersion": game.net_data.game_version,
            "save": std::path::Path::new(ctx.save_path).file_stem().map(|s| s.to_string_lossy().to_string()),
            "autosaveMinutes": game.rules.save.autosave_minutes,
            "tickRate": game.net_data.tick_rate,
            "rules": rules::changes(&game.rules),
            "fps": ctx.loop_rate,
            "busyPercent": (ctx.busy.0.as_secs_f64() * 1000.0).round() / 10.0,
            "longestLoopMicros": ctx.busy.1.as_micros() as u64,
            "planet": game.planet_id(),
            "terraformation": terraformation,
            "stages": meteo::stages(game),
            "gameTime": game.time.secs(),
            "units": planet_units,
            "players": players,
            "access": game.access.lists,
            "serverVersion": env!("CARGO_PKG_VERSION"),
            "tables": old_tables(game, ctx),
            "world": {"seed": game.save.settings["worldSeed"], "settings": game.save.settings},
            "objects": {"world": game.wos.len(), "network": game.net.objects.len(), "inventories": game.inventories.len()},
            "held": game.held,
            "s3": serde_json::to_value(crate::s3::status(&game.rules.backup)).unwrap(),
            "plugins": plugins::admin::summary(game),
            "lastSave": old_last_save(),
            "tick": old_summary(ctx.ticks, now),
            "backup": old_backup(&game.rules.save),
        });
        body.to_string()
    }

    #[test]
    fn the_tick_summary_prints_what_the_json_macro_did() {
        let mut t = TickTimes::default();
        let start = t.start;
        let same = |t: &TickTimes, now: Instant| assert_eq!(serde_json::to_string(&t.summary(now)).unwrap(), old_summary(t, now).to_string());
        same(&t, start);
        for i in 1..=1000u64 {
            t.record(start + Duration::from_millis(i * 5), Duration::from_micros(i * 3 % 977));
        }
        same(&t, start + Duration::from_secs(5));
        same(&t, start + Duration::from_secs(66));
        same(&t, start + Duration::from_secs(600));
        assert_eq!(serde_json::to_string(&TickTimes::default().summary(start)).unwrap(), r#"{"windowSeconds":60,"samples":0,"p50Micros":null,"p99Micros":null,"maxMicros":null}"#);
    }

    #[test]
    fn the_backup_status_prints_what_the_json_macro_did() {
        let on = rules::Save { backups_kept: 3, ..Default::default() };
        let off = rules::Save { backups_kept: 0, ..Default::default() };
        let same = |files: &rules::Save| assert_eq!(serde_json::to_string(&backup_status(files)).unwrap(), old_backup(files).to_string());
        same(&on);
        same(&off);
        persist::note_backup(&Ok(true));
        same(&on);
        persist::note_backup(&Err("backup \"x\" failed".to_string()));
        same(&on);
        assert!(serde_json::to_string(&backup_status(&on)).unwrap().contains(r#""error":{"at":"#));
        persist::note_backup(&Ok(true));
        assert!(serde_json::to_string(&backup_status(&on)).unwrap().ends_with(r#""error":null}"#));
    }

    #[test]
    fn the_whole_status_prints_what_the_json_macro_did() {
        let Some(mut game) = game::test_game(game::tests::save()) else { return };
        game::tests::join(&mut game, 1, "Ann");
        game::tests::join(&mut game, 2, "Bob");
        for _ in 0..30 {
            game.tick();
        }
        game.time = crate::clock::GameTime::at(0.1);
        game.held = true;
        persist::note_backup(&Ok(true));
        let mut ticks = TickTimes::default();
        let start = ticks.start;
        for i in 1..=50u64 {
            ticks.record(start + Duration::from_millis(i * 5), Duration::from_micros(i * 7));
        }
        let now = start + Duration::from_secs(1);
        for (save_path, build_id, busy) in [("", None, Default::default()), ("/tmp/some/world one.json", Some("123456"), (Duration::from_micros(123_456), Duration::from_micros(4_321)))] {
            let ctx = Ctx { ticks: &ticks, save_path, build_id, data_dir: "", loop_rate: 33, busy };
            let new = serde_json::to_string(&build(&game, &ctx, now)).unwrap();
            assert_eq!(new, old_status(&game, &ctx, now));
        }
        // Fields in the API's order.
        let v: Value = serde_json::from_str(&serde_json::to_string(&build(&game, &Ctx { ticks: &ticks, save_path: "w.json", build_id: None, data_dir: "", loop_rate: 0, busy: Default::default() }, now)).unwrap()).unwrap();
        let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(keys, ["state", "gameVersion", "save", "autosaveMinutes", "tickRate", "rules", "fps", "busyPercent", "longestLoopMicros", "planet", "terraformation", "stages", "gameTime", "units", "players", "access", "serverVersion", "tables", "world", "objects", "held", "s3", "plugins", "lastSave", "tick", "backup"]);
        assert_eq!(v["players"].as_array().map(Vec::len), Some(2));
    }

    /// The installed game's build, and a warning once it has no tables (a game update).
    #[test]
    fn status_warns_of_a_game_without_tables() {
        let dir = std::env::temp_dir().join(format!("pcds-game-status-{}", std::process::id()));
        let code = dir.join("game/Planet Crafter_Data/Managed/Assembly-CSharp.dll");
        std::fs::create_dir_all(code.parent().unwrap()).unwrap();
        std::fs::write(&code, "build 1").unwrap();
        let build = crate::cli::build_id(&dir.join("game")).unwrap();
        let tables = dir.join("data/2.103");
        std::fs::create_dir_all(&tables).unwrap();
        std::fs::write(tables.join("buildid"), &build).unwrap();
        std::fs::write(tables.join("tables-format"), crate::native::tables::TABLES_FORMAT.to_string()).unwrap();
        let data = tables.to_str().unwrap();
        assert_eq!(game_at(&dir.join("game"), data), json!({"buildId": build, "hasTables": true, "warning": null}));
        // The game updated (another size: a new hash at once).
        std::fs::write(&code, "build 2, longer").unwrap();
        let v = game_at(&dir.join("game"), data);
        let build = crate::cli::build_id(&dir.join("game")).unwrap();
        assert_eq!(v["warning"], format!("game build {build} has no tables: run pcds update"));
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(game_at(&dir.join("game"), data)["warning"].as_str().unwrap().contains("the game is gone"));
    }
}
