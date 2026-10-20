//! A bank's game process (docs/hot-update.md): the world and the tick, started by the front
//! from the active bank's binary, on the socket the front gave it (PCDS_IPC_FD). It loads its
//! tables, says HELLO, and then either loads the save (START) or restores the snapshot of the
//! bank before it (RESTORE). It builds and parses the Netcode batches and decides what goes out
//! when; the front only moves them. It saves the world on SAVE (the front's autosave timer), on
//! STOP and when the front is gone. At a flip it stops at a tick boundary, sends what is
//! queued, writes its snapshot into a memfd and waits: EXIT (the new bank runs) or RESUME.
//!
//!   pcds bank DATA_DIR SAVE.json        (started by the front, never by hand)
//!     PCDS_DEV=flip-verify       after a restore, check the state equals the snapshot (tests)
//!     PCDS_DEV=bank-fail=WHAT    tests: "restore" dies on RESTORE, "protocol" says another protocol,
//!                        "slow-hello" holds its HELLO 4 s (the flip waits in its loading phase),
//!                        "slow-ready" holds its READY 2 s after the restore (the flip waits in
//!                        its restoring phase), "die-after-ready" ends 1 s after READY

use std::collections::{HashMap, HashSet};
use std::os::fd::FromRawFd;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::game::Game;
use crate::ipc::{self, Link};
use crate::save::SaveFile;
mod admin;
mod forward;
mod mail;
mod panic;
mod persist;
mod status;
pub(crate) use panic::contained;
use panic::{guarded, taint};
use forward::{forward_chat, forward_ops};
pub use mail::reason_batch;
use mail::{check_loading, post, send};
use status::{Ctx, TickTimes};
use persist::{autosave, final_save, note_backup, persist, persist_later, settle};
use crate::{access, chathooks, config, plugins, rules, save, wire};

/// What this bank says in its HELLO, and `pcds bank-info` prints: its version, the front
/// protocol it speaks, the snapshot format it writes and reads (only that one), and the format
/// of the game tables it loads (and its read-game writes).
pub fn info() -> Value {
    json!({"version": env!("CARGO_PKG_VERSION"), "protocol": ipc::PROTOCOL, "format": crate::handoff::FORMAT, "tablesFormat": crate::native::tables::TABLES_FORMAT})
}

/// The page faults this process (all its threads) has had without reading a disk: a restore into
/// a new process touches every page of the world for the first time.
fn minor_faults() -> u64 {
    // SAFETY: getrusage fills the struct it is given.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) };
    usage.ru_minflt as u64
}

fn fail_for_test(what: &str) -> bool {
    config::dev("bank-fail").is_some_and(|v| v == what)
}

pub fn run(args: &[String]) {
    let [dir, save_path] = args else {
        eprintln!("usage: pcds bank DATA_DIR SAVE.json (the front starts it)");
        std::process::exit(2);
    };
    // Ctrl-C and SIGTERM are the front's: it tells this bank to save and stop.
    // SAFETY: plain signal dispositions, set before any other thread exists.
    unsafe {
        libc::signal(libc::SIGINT, libc::SIG_IGN);
        libc::signal(libc::SIGTERM, libc::SIG_IGN);
    }
    let fd: i32 = crate::env("PCDS_IPC_FD").and_then(|v| v.parse().ok()).unwrap_or_else(|| {
        eprintln!("pcds bank: no PCDS_IPC_FD (the front starts banks)");
        std::process::exit(2)
    });
    // SAFETY: the front put our end of the socket at this descriptor; nothing else owns it.
    let mut link = Link::new(unsafe { UnixStream::from_raw_fd(fd) });
    // The tables load before the hello: the front stops the old bank only once they are in.
    let (net_data, data, spec) = crate::tables(dir);
    let scenes = crate::scene_tables(dir, &net_data, &data);
    let build_id = std::fs::read_to_string(format!("{dir}/buildid")).ok().map(|b| b.trim().to_string());
    // The plugins are read and compiled now, with the tables, while the old bank still plays: after a
    // restore only their start is left (a hot update stops the world for that, not for this).
    let plugins = plugins_dir(save_path);
    let prepared = plugins::prepare(&plugins, &world_name(save_path));
    let mut hello = info();
    hello["gameVersion"] = net_data.game_version.clone().into();
    let protocol = if fail_for_test("protocol") { 999 } else { ipc::PROTOCOL };
    if fail_for_test("slow-hello") {
        println!("bank: holding its hello for 4 s (PCDS_DEV=bank-fail=slow-hello)");
        std::thread::sleep(Duration::from_secs(4));
    }
    link.send(ipc::HELLO, protocol, hello.to_string().as_bytes());
    let (mut verified, mut verify_ms) = (Value::Null, 0.0);
    let mut started = Instant::now();
    let (mut got_at, mut faults_before) = (0, 0);
    let game = match link.recv() {
        Some(f) if f.kind == ipc::START => load(dir, save_path, net_data, data, spec, scenes),
        Some(f) if f.kind == ipc::RESTORE => {
            started = Instant::now();
            got_at = ipc::now_ns();
            faults_before = minor_faults();
            if fail_for_test("restore") {
                eprintln!("bank: dying on the restore (PCDS_DEV=bank-fail=restore)");
                std::process::exit(3);
            }
            let recover = serde_json::from_slice::<Value>(&f.body).is_ok_and(|b| b["recover"] == true);
            let snapshot = f.fd.as_ref().ok_or("no memfd".to_string()).and_then(|fd| ipc::map_memfd(fd).map_err(|e| e.to_string()));
            let restored = snapshot.and_then(|s| crate::handoff::restore_bytes(net_data, data, spec, &s, scenes).map(|g| (g, s)));
            match restored {
                Ok((mut g, snapshot)) => {
                    if config::dev("flip-verify").is_some() {
                        let t = Instant::now();
                        if let Err(e) = crate::handoff::verify(&g, &snapshot) {
                            eprintln!("bank: {e}");
                            std::process::exit(3);
                        }
                        verified = true.into();
                        verify_ms = t.elapsed().as_secs_f64() * 1000.0;
                    }
                    if let Err(e) = reread(&mut g, &access_path(save_path), &rules_path(save_path)) {
                        eprintln!("bank: restore failed: {e}");
                        std::process::exit(3);
                    }
                    if recover {
                        // After a crash: the clients in it are gone (the front dropped them).
                        let clients: Vec<u64> = g.clients.keys().copied().collect();
                        clients.into_iter().for_each(|c| g.disconnect(c));
                    }
                    g
                }
                Err(e) => {
                    eprintln!("bank: restore failed: {e}");
                    std::process::exit(3);
                }
            }
        }
        _ => return,
    };
    let mut game = game;
    plugins::attach(&mut game, prepared);
    // What the start freed (the save's JSON, a snapshot's text, the tables' text) goes back to
    // the system, not to sit in the process.
    // SAFETY: malloc_trim only returns free memory to the system.
    unsafe { libc::malloc_trim(0) };
    // gotAt and readyAt (the monotonic clock, ns) put the restore on the flip's timeline in the front.
    let ready = json!({"restoreMs": started.elapsed().as_secs_f64() * 1000.0 - verify_ms, "verifyMs": verify_ms, "verified": verified, "gotAt": got_at, "readyAt": ipc::now_ns(), "faults": minor_faults() - faults_before});
    if fail_for_test("slow-ready") {
        println!("bank: holding its ready for 2 s (PCDS_DEV=bank-fail=slow-ready)");
        std::thread::sleep(Duration::from_secs(2));
    }
    link.send(ipc::READY, 0, ready.to_string().as_bytes());
    if fail_for_test("die-after-ready") {
        println!("bank: ending 1 s after its ready (PCDS_DEV=bank-fail=die-after-ready)");
        std::thread::spawn(|| {
            std::thread::sleep(Duration::from_secs(1));
            std::process::exit(3)
        });
    }
    Bank { game, link, save_path: save_path.clone(), build_id, data_dir: dir.clone() }.serve();
}

/// The world from the save, as a start of the whole server loads it. Exits on what stops a
/// start (no save, an old save format, rules that do not hold up).
fn load(dir: &str, save_path: &str, net_data: crate::data::NetData, data: crate::gamedata::GameData, spec: crate::rpcspec::Spec, scenes: crate::Scenes) -> Game {
    let save = SaveFile::load(save_path).unwrap_or_else(|e| {
        match e.kind() {
            std::io::ErrorKind::NotFound => eprintln!("no save {save_path}: make a new world with `pcds new-save {save_path} {dir}`"),
            _ => eprintln!("{save_path}: {e}"),
        }
        std::process::exit(1)
    });
    if let Err(e) = crate::game::check_save_content(&save, &data) {
        eprintln!("{save_path}: {e}");
        std::process::exit(1);
    }
    // A rules or access file that does not hold up stops the start, before the world loads.
    let rules_path = rules_path(save_path);
    let loaded_rules = rules::Rules::load(&rules_path).and_then(|r| r.validate(&data).map(|_| r)).unwrap_or_else(|e| {
        eprintln!("rules: {e}");
        std::process::exit(1)
    });
    let loaded_access = load_access(&access_path(save_path)).unwrap_or_else(|e| {
        eprintln!("access: {e}");
        std::process::exit(1)
    });
    let folder = std::path::Path::new(save_path).parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(std::path::Path::new("."));
    save::sweep(folder);
    save::sweep(&folder.join("backups"));
    // The save as it was before this server touched it (as every backup: none with save.backupsKept 0).
    // At most one per 10 minutes: a crash loop must not push the hourly backups out.
    note_backup(&save::backup(save_path, Duration::from_secs(600), loaded_rules.save.backups_kept as usize).map_err(|e| format!("{save_path}: {e}")));
    let session_number = config::env_number("PCDS_SESSION") as u32;
    let version = net_data.game_version.clone();
    let mut game = Game::new(net_data, data, spec, save, scenes);
    rules::start(&mut game, loaded_rules, rules_path);
    game.access = loaded_access;
    game.session_number = session_number;
    println!("world: {} world objects, {} network objects, planet {}, game {version}", game.wos.len(), game.net.objects.len(), game.planet_id());
    game
}

fn rules_path(save_path: &str) -> String {
    config::installation_file(&crate::env, save_path, "rules.json")
}

/// The plugins' folder: beside the rules file (run/plugins for an installation, next to the world for a
/// server started by hand).
fn plugins_dir(save_path: &str) -> std::path::PathBuf {
    std::path::Path::new(&rules_path(save_path)).with_file_name("plugins")
}

/// The world's name: its save's file name without the extension.
fn world_name(save_path: &str) -> String {
    std::path::Path::new(save_path).file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "world".into())
}

fn access_path(save_path: &str) -> String {
    config::installation_file(&crate::env, save_path, "access.json")
}

/// The access lists from their file (none: empty lists). Err for a file that is there and does
/// not read: empty lists in its place would let in everyone it bans.
fn load_access(path: &str) -> Result<access::Access, String> {
    match std::fs::read_to_string(path) {
        Ok(text) if !text.trim().is_empty() => {
            serde_json::from_str::<access::Lists>(&text).map_err(|e| e.to_string()).and_then(|l| l.check()).map_err(|e| format!("{path}: {e}"))?;
        }
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("{path}: {e}")),
    }
    Ok(access::Access::load(Some(path.to_string())))
}

/// A restored game's access lists and rules from their files, which every change is written to
/// before it is made: the snapshot's may be older (a ban or a rules change after the last flip,
/// then a crash and a restart from that flip's snapshot).
fn reread(game: &mut Game, access_path: &str, rules_path: &str) -> Result<(), String> {
    game.access = load_access(access_path)?;
    let rules = rules::Rules::load(rules_path).and_then(|r| r.validate(&game.data).map(|_| r))?;
    game.rules_path = Some(rules_path.to_string());
    if rules != game.rules {
        rules::set(game, rules)?;
    }
    enforce(game);
    Ok(())
}

struct Bank {
    game: Game,
    link: Link,
    save_path: String,
    build_id: Option<String>,
    data_dir: String,
}

impl Bank {
    /// The server loop's game half, its sends going to the front.
    fn serve(mut self) {
        let game = &mut self.game;
        // Who got ConnectionApproved: in the world for a restored game.
        let mut approved: HashSet<u64> = game.clients.values().filter(|c| c.approved).map(|c| c.id).collect();
        // Kicked, until the front says the connection is closed.
        let mut kicked: HashSet<u64> = HashSet::new();
        // Bytes approved clients that are no players yet may still be sent.
        let mut loading: HashMap<u64, i64> = HashMap::new();
        let (mut mail, mut urgent): (crate::idmap::FxHashMap<u64, wire::Batch>, Vec<u64>) = Default::default();
        let (mut next_frame, mut movement_sent) = (Instant::now(), Instant::now());
        let mut loops = (0u32, Instant::now(), 0u32);
        let (mut busy, mut busy_last) = ((Duration::ZERO, Duration::ZERO), (Duration::ZERO, Duration::ZERO));
        let mut ticks = TickTimes::default();
        // The hooks' settings, once at the start (a restored game's too).
        game.ext.chat.changed = true;
        let mut last_ops = Value::Null;
        loop {
            let woke = Instant::now();
            let frame = woke >= next_frame;
            if frame {
                next_frame = Instant::now() + crate::FRAME;
                loops.0 += 1;
            }
            if loops.1.elapsed() >= Duration::from_secs(1) {
                loops = (0, Instant::now(), loops.0);
                busy_last = std::mem::take(&mut busy);
            }
            while let Some(f) = self.link.next() {
                match f.kind {
                    ipc::CONNECTED => {
                        let c: Value = serde_json::from_slice(&f.body).unwrap_or_default();
                        guarded(game, f.n, &self.save_path, |game| game.connect(f.n, c["steamId"].as_u64(), c["trusted"] == true, c["ip"].as_str().map(str::to_string)));
                    }
                    // Kicked this loop too (game.kicks goes out after it): nothing more counts.
                    ipc::DATA if !kicked.contains(&f.n) && !game.kicks.iter().any(|(c, _)| *c == f.n) && game.clients.contains_key(&f.n) => {
                        let before = game.net.out.len();
                        guarded(game, f.n, &self.save_path, |game| game.receive(f.n, &f.body));
                        if game.net.out[before.min(game.net.out.len())..].iter().any(|(to, _, _)| to.reaches(f.n)) && !urgent.contains(&f.n) {
                            urgent.push(f.n);
                        }
                    }
                    ipc::GONE => {
                        approved.remove(&f.n);
                        kicked.remove(&f.n);
                        if game.clients.contains_key(&f.n) {
                            println!("client {}: left", f.n);
                            guarded(game, f.n, &self.save_path, |game| game.disconnect(f.n));
                        }
                    }
                    ipc::ADMIN => {
                        let r: Value = serde_json::from_slice(&f.body).unwrap_or_default();
                        let ctx = Ctx { ticks: &ticks, save_path: &self.save_path, build_id: self.build_id.as_deref(), data_dir: &self.data_dir, loop_rate: loops.2, busy: busy_last };
                        let request = || admin::request(game, r["method"].as_str().unwrap_or(""), r["path"].as_str().unwrap_or(""), &r["body"], &ctx);
                        let (status, body) = contained("an admin request", request).unwrap_or_else(|| (500, "{\"error\":\"the server failed on this request\"}".to_string()));
                        self.link.send(ipc::ANSWER, f.n, admin::frame(status, body).as_bytes());
                    }
                    ipc::SAVE => autosave(game, &self.save_path),
                    ipc::STOP => std::process::exit(final_save(game, &self.save_path)),
                    ipc::SAY => {
                        contained("a chat hook's reply", || chathooks::say(game, &String::from_utf8_lossy(&f.body)));
                    }
                    ipc::HANDOFF => {
                        // A tick boundary with nothing queued: what is waiting goes out first.
                        post(game, &mut approved, &mut mail, &mut loading);
                        let all: Vec<u64> = mail.keys().copied().collect();
                        send(&mut self.link, &mut mail, &all, &mut loading);
                        forward_chat(game, &mut self.link);
                        let (t, got_at) = (Instant::now(), ipc::now_ns());
                        // The plugins' states go into the snapshot (within their budget altogether).
                        contained("the plugins' state", || plugins::checkpoint(game, Some(plugins::limits::FLIP_BUDGET)));
                        match crate::handoff::snapshot_memfd(game, &mut Vec::new()) {
                            Ok(fd) => {
                                let len = std::fs::File::from(fd.try_clone().expect("dup")).metadata().map(|m| m.len()).unwrap_or(0);
                                let head = json!({"bytes": len, "ms": t.elapsed().as_secs_f64() * 1000.0, "gotAt": got_at, "doneAt": ipc::now_ns()});
                                self.link.send_fd(ipc::SNAPSHOT, f.n, head.to_string().as_bytes(), Some(fd));
                                // The world stays in memory until the new bank runs.
                                match self.link.recv() {
                                    Some(f) if f.kind == ipc::RESUME => println!("bank: resumed, the new bank failed"),
                                    Some(f) if f.kind == ipc::EXIT => {
                                        settle(game, true);
                                        std::process::exit(0)
                                    }
                                    _ => std::process::exit(final_save(game, &self.save_path)),
                                }
                            }
                            Err(e) => {
                                eprintln!("bank: no snapshot ({e}); going on");
                                self.link.send(ipc::SNAPSHOT, f.n, &[]);
                            }
                        }
                    }
                    _ => {}
                }
            }
            let tick_started = Instant::now();
            let ticked = !frame || std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| game.tick())).is_ok();
            if frame {
                ticks.record(tick_started, tick_started.elapsed());
            }
            if !ticked {
                // The whole world may be half-updated: keep what there is; the front starts the
                // bank again from the save.
                eprintln!("the game tick panicked: saving and stopping");
                taint();
                let _ = persist(game, &self.save_path);
                std::process::exit(1);
            }
            // Plugins appear, change and go while the world runs: the folder is looked at between ticks.
            if frame {
                contained("the plugin folder's scan", || plugins::poll(game));
            }
            settle(game, false);
            if std::mem::take(&mut game.save_requested) && !persist_later(game, &self.save_path) {
                game.save_requested = true;
            }
            // What a client's own packets brought it goes out now; the rest (other players'
            // movement and doings, the world's changes) every MOVEMENT_EVERY.
            post(game, &mut approved, &mut mail, &mut loading);
            if frame && movement_sent.elapsed() >= crate::MOVEMENT_EVERY {
                movement_sent = Instant::now();
                let all: Vec<u64> = mail.keys().copied().collect();
                send(&mut self.link, &mut mail, &all, &mut loading);
            } else if !urgent.is_empty() {
                send(&mut self.link, &mut mail, &urgent, &mut loading);
            }
            urgent.clear();
            check_loading(game, &mut loading);
            // Kicked or refused: what it was sent first, then the reason; the front closes the
            // connection a moment later and says GONE.
            for (client, reason) in std::mem::take(&mut game.kicks) {
                if !kicked.insert(client) {
                    continue;
                }
                println!("client {client}: kicked ({reason})");
                send(&mut self.link, &mut mail, &[client], &mut HashMap::new());
                self.link.send(ipc::DATA, client, &reason_batch(&reason));
                self.link.send(ipc::KICK, client, &[]);
            }
            forward_chat(game, &mut self.link);
            forward_ops(game, &mut self.link, &mut last_ops);
            let work = woke.elapsed();
            busy = (busy.0 + work, busy.1.max(work));
            let pause = next_frame.saturating_duration_since(Instant::now());
            if ipc::wait(&[self.link.fd()], &[], pause)[0] && !self.link.fill(usize::MAX) {
                println!("bank: the front is gone; saving and stopping");
                std::process::exit(final_save(game, &self.save_path));
            }
        }
    }
}

/// Kicks everyone the access lists no longer let in.
pub fn enforce(game: &mut Game) {
    let refused: Vec<u64> = game
        .clients
        .values()
        .filter(|c| {
            let ip_refused = c.verified_steam_id.is_none() && c.ip.as_deref().is_some_and(|ip| !game.access.ip_allowed(ip));
            let name_refused = c.ready.as_ref().is_some_and(|s| !game.access.name_allowed(&s.name, c.verified_steam_id));
            ip_refused || name_refused
        })
        .map(|c| c.id)
        .collect();
    for c in refused {
        game.kick(c, access::REFUSED);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game;

    /// A ban and a rules change written after a snapshot are in the game restored from it.
    #[test]
    fn a_restored_game_takes_access_and_rules_from_their_files() {
        let Some(mut game) = game::test_game(game::tests::save()) else { return };
        let dir = std::env::temp_dir().join(format!("pcds-bank-reread-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (access, rules) = (dir.join("access.json").to_string_lossy().to_string(), dir.join("rules.json").to_string_lossy().to_string());
        std::fs::write(&access, r#"{"bannedNames": ["Mallory"]}"#).unwrap();
        std::fs::write(&rules, r#"{"speed": {"growth": 2}}"#).unwrap();
        reread(&mut game, &access, &rules).unwrap();
        assert_eq!((game.access.lists.banned_names.clone(), game.rules.speed.growth), (vec!["Mallory".to_string()], 2.0));
        assert!(!game.access.name_allowed("mallory", None));
        // A file that does not read is an error, not empty lists.
        std::fs::write(&access, r#"{"bannedNames": "#).unwrap();
        assert!(reread(&mut game, &access, &rules).unwrap_err().contains("access.json"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
