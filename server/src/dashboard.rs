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

/// The memory limit of the cgroup this process runs in (cgroup v2: a container's `--memory`, a
/// Kubernetes memory limit) and what counts against it now (anonymous memory and shared
/// memory, the snapshot's memfd among it; page cache is reclaimable and left out). None
/// without a limit or without cgroup v2 (a host: nothing is checked).
pub fn cgroup_memory() -> Option<(u64, u64)> {
    let limit = std::fs::read_to_string("/sys/fs/cgroup/memory.max").ok()?.trim().parse().ok()?;
    let stat = std::fs::read_to_string("/sys/fs/cgroup/memory.stat").ok()?;
    let field = |name: &str| stat.lines().find_map(|l| l.strip_prefix(name)?.trim().parse::<u64>().ok());
    Some((limit, field("anon ")? + field("shmem ")?))
}

/// Whether a flip fits in the memory the cgroup leaves. The new bank and the world's snapshot
/// are in memory while the old bank still holds the world: measured on a world of 100000
/// objects (bank 321 MB, snapshot 166 MB, the new bank peaking at 488 MB as it restores),
/// about twice the live bank's memory beyond what is in use. A flip that does not fit gets a
/// bank killed by the kernel (a crash restart, players dropped), so it is refused first.
pub fn flip_memory(live_bank: u64, cgroup: Option<(u64, u64)>) -> Result<(), String> {
    let Some((limit, used)) = cgroup else { return Ok(()) };
    let need = live_bank * 2;
    let free = limit.saturating_sub(used);
    if free >= need {
        return Ok(());
    }
    let mb = |b: u64| b.div_ceil(1 << 20);
    Err(format!(
        "a hot update needs about {} MB of memory beyond what runs (the new bank and the world's snapshot are in memory while the old bank still holds the world); this container's limit is {} MB with {} MB in use, {} MB free. Raise the memory limit to at least {} MB (docker run --memory, Compose mem_limit, a Kubernetes memory limit), or restart instead (pcds upgrade --restart VERSION), which needs no extra memory and disconnects every player",
        mb(need),
        mb(limit),
        mb(used),
        mb(free),
        mb(used + need)
    ))
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

/// A world's name: letters, digits, '.', '_' and '-', up to 64, starting with a letter or digit
/// (worlds/<name>.json); the access and rules files are not worlds.
pub fn world_name(name: &str) -> Result<String, String> {
    let ok = name.len() <= 64 && name.bytes().next().is_some_and(|b| b.is_ascii_alphanumeric()) && name.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b));
    if !ok || matches!(name, "access" | "rules") {
        return Err(format!("world names are letters, digits, '.', '_' and '-' (up to 64), not access or rules: {name:?}"));
    }
    Ok(name.to_string())
}

fn worlds_dir(save_path: &str) -> &Path {
    Path::new(save_path).parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."))
}

fn running(save_path: &str) -> String {
    Path::new(save_path).file_stem().unwrap_or_default().to_string_lossy().to_string()
}

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
    let name = world_name(&w.name)?;
    let path = worlds_dir(save_path).join(format!("{name}.json"));
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
    println!("world {name} created (admin)");
    Ok(json!({"name": name, "planet": save.settings["planetId"], "seed": save.settings["worldSeed"]}))
}

/// POST /api/worlds/remove: moved into backups/ as <name>-removed-<time>.json, never deleted;
/// the running world is refused.
pub fn world_remove(save_path: &str, name: &str) -> Result<Value, String> {
    let name = world_name(name)?;
    if name == running(save_path) {
        return Err(format!("the server runs {name}: switch to another world first"));
    }
    let dir = worlds_dir(save_path);
    let from = dir.join(format!("{name}.json"));
    if !from.is_file() {
        return Err(format!("no world {name}"));
    }
    std::fs::create_dir_all(dir.join("backups")).map_err(|e| e.to_string())?;
    let to = dir.join("backups").join(format!("{name}-removed-{}.json", crate::save::stamp(std::time::SystemTime::now())));
    if to.exists() {
        return Err("removed a moment ago: try again in a second".into());
    }
    std::fs::rename(&from, &to).map_err(|e| format!("{}: {e}", to.display()))?;
    println!("world {name} removed (admin): kept as {}", to.display());
    Ok(json!({"kept": to.file_name().map(|f| f.to_string_lossy().to_string())}))
}

/// A world's or a backup's file, by name: worlds/<name>.json or backups/<name>.json.
fn file_of(save_path: &str, name: &str, backup: bool) -> Result<std::path::PathBuf, String> {
    let name = world_name(name)?;
    let dir = worlds_dir(save_path);
    let path = if backup { dir.join("backups").join(format!("{name}.json")) } else { dir.join(format!("{name}.json")) };
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
    let name = world_name(name)?;
    let to = worlds_dir(save_path).join(format!("{name}.json"));
    if to.exists() {
        return Err(format!("world {name} exists: pick another name"));
    }
    let text = std::fs::read(&from).map_err(|e| format!("{}: {e}", from.display()))?;
    crate::save::write_atomic(&to.to_string_lossy(), &text).map_err(|e| format!("{}: {e}", to.display()))?;
    println!("backup {backup} restored as world {name} (admin)");
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

    #[test]
    fn a_flip_must_fit_in_the_memory_the_container_leaves() {
        const MB: u64 = 1 << 20;
        // No limit (a host): nothing is checked.
        assert!(flip_memory(10_000 * MB, None).is_ok());
        // The measured world: bank 321 MB; 1 GB with 355 MB in use fits, with the last
        // snapshot (166 MB) also held it does not, nor does 512 MB.
        assert!(flip_memory(321 * MB, Some((1024 * MB, 355 * MB))).is_ok());
        let e = flip_memory(321 * MB, Some((1024 * MB, 521 * MB))).unwrap_err();
        assert!(e.contains("about 642 MB") && e.contains("limit is 1024 MB") && e.contains("at least 1163 MB") && e.contains("pcds upgrade --restart"), "{e}");
        assert!(flip_memory(321 * MB, Some((512 * MB, 355 * MB))).is_err());
        // Already over the limit counts as no room.
        assert!(flip_memory(MB, Some((MB, 2 * MB))).is_err());
        // This test's own cgroup, whatever it is: reads or says nothing, never fails.
        let _ = cgroup_memory();
    }
}
