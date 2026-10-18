//! What the admin page shows beyond the status and the configuration (config.rs): the process's
//! CPU and memory, the network traffic, the players with their state and position, the worlds
//! and backups beside the save, and the buildings for the map; and the request bodies of the
//! admin's actions on players (teleport, a chat message to everyone).

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::UNIX_EPOCH;

use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::game::Game;

/// Batch bytes received from and sent to clients (the transports' own framing not counted).
pub static BYTES_IN: AtomicU64 = AtomicU64::new(0);
pub static BYTES_OUT: AtomicU64 = AtomicU64::new(0);
/// The most buildings the map gets (spread over all of them).
const MAP_POINTS: usize = 4000;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Teleport {
    /// The client id; or the player's name (any case), as "name" or the plugin API's "player".
    pub id: Option<u64>,
    pub name: Option<String>,
    pub player: Option<String>,
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

pub fn traffic() -> Value {
    json!({ "bytesIn": BYTES_IN.load(Ordering::Relaxed), "bytesOut": BYTES_OUT.load(Ordering::Relaxed) })
}

/// CPU time used so far and memory held now, asked of the kernel when the page asks (no
/// sampling in the loop).
pub fn process() -> Value {
    // SAFETY: getrusage only fills the struct it is given.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    let seconds = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1e6;
    let cpu = (unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) } == 0).then(|| seconds(usage.ru_utime) + seconds(usage.ru_stime));
    let rss = std::fs::read_to_string("/proc/self/status").ok().and_then(|s| s.lines().find_map(|l| l.strip_prefix("VmRSS:")?.trim().strip_suffix("kB")?.trim().parse::<u64>().ok()));
    json!({ "cpuSeconds": cpu, "memoryBytes": rss.map(|kb| kb * 1024) })
}

/// The CPU time and memory of another process of this user (a bank), from /proc.
pub fn process_of(pid: u32) -> (f64, u64) {
    // SAFETY: sysconf only reads.
    let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as f64;
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
    // After the command's closing parenthesis: state is field 3, utime and stime 14 and 15.
    let fields: Vec<&str> = stat.rsplit_once(')').map(|(_, rest)| rest.split_whitespace().collect()).unwrap_or_default();
    let cpu = [11, 12].iter().filter_map(|i| fields.get(*i)?.parse::<f64>().ok()).sum::<f64>() / ticks;
    let rss = std::fs::read_to_string(format!("/proc/{pid}/status")).ok().and_then(|s| s.lines().find_map(|l| l.strip_prefix("VmRSS:")?.trim().strip_suffix("kB")?.trim().parse::<u64>().ok()));
    (cpu, rss.unwrap_or(0) * 1024)
}

/// The CPU time of the children this process has reaped (banks that ended).
pub fn reaped_children_cpu() -> f64 {
    // SAFETY: getrusage only fills the struct it is given.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    let seconds = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1e6;
    if unsafe { libc::getrusage(libc::RUSAGE_CHILDREN, &mut usage) } != 0 {
        return 0.0;
    }
    seconds(usage.ru_utime) + seconds(usage.ru_stime)
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

/// The worlds beside the save (every .json there but the access lists and rules) and this
/// world's backups, with their size and modification time (Unix seconds).
pub fn worlds(save_path: &str) -> Value {
    let path = Path::new(save_path);
    let current = path.file_stem().unwrap_or_default().to_string_lossy().to_string();
    let dir = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let list = |dir: &Path, keep: &dyn Fn(&str) -> bool| {
        let mut out: Vec<Value> = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let meta = e.metadata().ok().filter(|m| m.is_file())?;
                let name = e.file_name().to_string_lossy().strip_suffix(".json").filter(|n| keep(n))?.to_string();
                let modified = meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?.as_secs();
                Some(json!({ "name": name, "bytes": meta.len(), "modified": modified }))
            })
            .collect();
        out.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
        out
    };
    let worlds = list(dir, &|n| !matches!(n, "access" | "rules"));
    // save::backup's names: <world>-YYYYMMDD-HHMMSS.
    let backups = list(&dir.join("backups"), &|n| n.strip_prefix(&format!("{current}-")).is_some_and(|t| t.len() == 15));
    json!({ "current": current, "worlds": worlds, "backups": backups })
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

    #[test]
    fn worlds_lists_saves_and_this_worlds_backups() {
        let dir = std::env::temp_dir().join(format!("pcds-worlds-test-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("backups")).unwrap();
        for f in ["world.json", "other.json", "access.json", "rules.json", "world.json.tmp", "backups/world-20261018-120000.json", "backups/other-20261018-120000.json"] {
            std::fs::write(dir.join(f), "{}").unwrap();
        }
        let w = worlds(dir.join("world.json").to_str().unwrap());
        let names = |k: &str| w[k].as_array().unwrap().iter().map(|e| e["name"].as_str().unwrap().to_string()).collect::<Vec<_>>();
        assert_eq!((w["current"].as_str(), names("worlds"), names("backups")), (Some("world"), vec!["other".to_string(), "world".into()], vec!["world-20261018-120000".to_string()]));
        assert_eq!(w["worlds"][0]["bytes"], 2);
        std::fs::remove_dir_all(dir).unwrap();
        assert!(process()["cpuSeconds"].as_f64().is_some() && process()["memoryBytes"].as_u64().unwrap() > 0);
        let (cpu, memory) = process_of(std::process::id());
        assert!(cpu >= 0.0 && memory > 0);
    }
}
