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
use status::{backup_status, Ctx, TickTimes};
use persist::{autosave, final_save, note_backup, persist, persist_later, settle};
use crate::{access, chathooks, config, dashboard, plugins, protocol, rules, save, wire};

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
                        let request = || admin_request(game, r["method"].as_str().unwrap_or(""), r["path"].as_str().unwrap_or(""), &r["body"], &ctx);
                        let (status, body) = contained("an admin request", request).unwrap_or_else(|| (500, "{\"error\":\"the server failed on this request\"}".to_string()));
                        self.link.send(ipc::ANSWER, f.n, admin_answer(status, body).as_bytes());
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

/// Who an API call names: {"id": N} is that client id exactly; {"name": S} a player's name in
/// any case.
enum Who {
    Id(u64),
    Name(String),
}

fn who(body: &Value) -> Result<Who, (u16, String)> {
    if let Some(id) = body.get("id").filter(|v| !v.is_null()) {
        return id.as_u64().map(Who::Id).ok_or_else(|| (400, "{\"error\":\"'id': a client id\"}".into()));
    }
    let name = body["name"].as_str().map(str::trim).filter(|s| !s.is_empty());
    name.map(|n| Who::Name(n.to_string())).ok_or_else(|| (400, "{\"error\":\"'id' or 'name' is required\"}".into()))
}

/// The online client named; None: nobody online is. Two players with that name (any case) are
/// refused: the caller must give the id.
fn find_client(game: &Game, who: &Who) -> Result<Option<u64>, (u16, String)> {
    match who {
        Who::Id(id) => Ok(game.clients.contains_key(id).then_some(*id)),
        Who::Name(name) => {
            let found: Vec<u64> = game.clients.values().filter(|c| c.ready.as_ref().is_some_and(|s| crate::access::same_name(&s.name, name))).map(|c| c.id).collect();
            match found[..] {
                [] => Ok(None),
                [one] => Ok(Some(one)),
                _ => Err((400, json!({"error": format!("{} players online are named {name}: give the client id", found.len())}).to_string())),
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

fn admin_request(game: &mut Game, method: &str, path: &str, body: &Value, ctx: &Ctx) -> (u16, String) {
    let ok = || (200, "{\"ok\":true}".to_string());
    let kept = |r: Result<(), String>| match r {
        Ok(()) => (200, "{\"ok\":true}".to_string()),
        Err(e) => (500, json!({"error": e}).to_string()),
    };
    let field = |k: &str| body[k].as_str().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
    // A request's outcome: its JSON, or 400 with what is wrong.
    let answer = |r: Result<Value, String>| match r {
        Ok(v) => (200, v.to_string()),
        Err(e) => (400, json!({"error": e}).to_string()),
    };
    // Whatever works on the world files waits for the autosave's write first.
    if method == "POST" && (path.starts_with("/api/worlds") || path.starts_with("/api/backup")) {
        settle(game, true);
    }
    match (method, path) {
        // The bank's part; the front adds its own (invite code, network, banks, traffic).
        ("GET", "/api/status") => (200, serde_json::to_string(&status::build(game, ctx, Instant::now())).unwrap_or_default()),
        // The front's watchdog asks this of a bank that has been silent: any answer is a loop that runs.
        ("GET", "/api/ping") => ok(),
        // The save, and what became of its backup: a backup that failed is the answer's, too.
        ("POST", "/api/save") => match persist(game, ctx.save_path) {
            Ok(()) => (200, json!({"ok": true, "backup": backup_status(&game.rules.save)}).to_string()),
            Err(e) => (500, json!({"error": e}).to_string()),
        },
        (_, p) if p.starts_with("/api/plugins") => plugins::admin::request(game, method, path, body),
        ("POST", "/api/kick") => match who(body).and_then(|w| find_client(game, &w)) {
            Ok(Some(c)) => {
                game.kick(c, "Kicked");
                ok()
            }
            Ok(None) => (400, "{\"error\":\"player not online\"}".into()),
            Err(e) => e,
        },
        ("POST", "/api/ban") => {
            let named = match who(body) {
                Ok(w) => w,
                Err(e) => return e,
            };
            match find_client(game, &named) {
                Ok(Some(c)) => {
                    let client = &game.clients[&c];
                    let name = client.ready.as_ref().map(|s| s.name.clone());
                    let name = match (name, &named) {
                        (Some(n), _) => n,
                        (None, Who::Name(n)) => n.clone(),
                        (None, Who::Id(_)) => return (400, "{\"error\":\"that client has no name yet: kick it instead\"}".into()),
                    };
                    let (steam_id, ip) = (client.verified_steam_id, client.ip.clone());
                    if let Err(e) = game.access.ban(&name, steam_id, ip.as_deref()) {
                        return (500, json!({"error": e}).to_string());
                    }
                    game.kick(c, "Banned");
                    ok()
                }
                Ok(None) => match named {
                    Who::Name(n) => kept(game.access.ban(&n, None, None)),
                    Who::Id(_) => (400, "{\"error\":\"player not online: ban by name\"}".into()),
                },
                Err(e) => e,
            }
        }
        ("POST", "/api/unban") => match field("value") {
            Some(v) => match game.access.unban(&v) {
                Ok(true) => ok(),
                Ok(false) => (400, json!({"error": format!("{v} is not banned (the bans: GET /api/status, access; a range is unbanned by its own entry)")}).to_string()),
                Err(e) => (500, json!({"error": e}).to_string()),
            },
            None => (400, "{\"error\":\"'value' is required\"}".into()),
        },
        // chat.webhooks, chat.secret and chat.exec only as "(set)": they may hold tokens.
        ("GET", "/api/rules") => (200, json!({"rules": rules::shown(&game.rules), "defaults": rules::Rules::default()}).to_string()),
        // A JSON merge patch of the rules: {"speed": {"growth": 2}}; null resets a rule.
        ("POST", "/api/rules") => match game.rules.patched(body).and_then(|new| rules::set(game, new)) {
            Ok(()) => (200, json!({"rules": rules::shown(&game.rules)}).to_string()),
            Err(e) => (400, json!({"error": e}).to_string()),
        },
        // Every setting with its schema entry and value; a change of any, {"speed.growth": 2}.
        ("GET", "/api/config") => (200, config::get(game).to_string()),
        ("POST", "/api/config") => match config::set(game, body) {
            Ok(()) => (200, config::get(game).to_string()),
            Err(e) => (400, e.to_string()),
        },
        ("POST", "/api/teleport") => match dashboard::body::<dashboard::Teleport>(body) {
            Ok(t) if !t.position.iter().all(|v| v.is_finite()) => (400, "{\"error\":\"position: three finite numbers\"}".into()),
            Ok(t) => {
                let named = match t.id {
                    Some(id) => Who::Id(id),
                    None => match t.name {
                        Some(n) => Who::Name(n),
                        None => return (400, "{\"error\":\"'id' or 'name' is required\"}".into()),
                    },
                };
                match find_client(game, &named) {
                    Ok(Some(c)) if crate::status::teleport(game, c, t.position) => {
                        println!("client {c}: teleported to {:?} (admin)", t.position);
                        ok()
                    }
                    Err(e) => e,
                    _ => (400, "{\"error\":\"player not online and ready\"}".into()),
                }
            }
            Err(e) => e,
        },
        ("GET", "/api/worlds") => {
            let mut w = dashboard::worlds(ctx.save_path);
            w["choices"] = crate::newworld::choices(&game.net_data, &game.data);
            w["backupsKept"] = game.rules.save.backups_kept.into();
            w["s3"] = serde_json::to_value(crate::s3::status(&game.rules.backup)).unwrap_or_default();
            (200, w.to_string())
        }
        ("POST", "/api/worlds/new") => match dashboard::body::<dashboard::NewWorld>(body) {
            Ok(w) => answer(dashboard::world_new(game, ctx.save_path, w)),
            Err(e) => e,
        },
        ("POST", "/api/worlds/remove") => match field("name") {
            Some(n) => answer(dashboard::world_remove(ctx.save_path, &n)),
            None => (400, "{\"error\":\"'name' is required\"}".into()),
        },
        ("POST", "/api/worlds/file") => match field("name") {
            Some(n) => answer(dashboard::world_file(ctx.save_path, &n, body["backup"] == true)),
            None => (400, "{\"error\":\"'name' is required\"}".into()),
        },
        ("POST", "/api/worlds/restore") => match (field("backup"), field("name")) {
            (Some(b), Some(n)) => answer(dashboard::world_restore(ctx.save_path, &b, &n)),
            _ => (400, "{\"error\":\"'backup' and 'name' are required\"}".into()),
        },
        // A backup now (after a save), copied to S3 when that is set.
        ("POST", "/api/backup") => {
            if game.rules.save.backups_kept == 0 {
                return (400, "{\"error\":\"backups are off (the rule save.backupsKept is 0)\"}".into());
            }
            if let Err(e) = save::write_atomic(ctx.save_path, game.save_text().as_bytes()) {
                return (500, json!({"error": format!("save failed: {e}")}).to_string());
            }
            let made = save::backup(ctx.save_path, Duration::ZERO, game.rules.save.backups_kept as usize).map_err(|e| format!("{}: {e}", ctx.save_path));
            note_backup(&made);
            if let Err(e) = made {
                return (500, json!({"error": format!("backup failed: {e}")}).to_string());
            }
            let made = crate::s3::newest_backup(ctx.save_path).and_then(|p| p.file_stem().map(|f| f.to_string_lossy().to_string()));
            let s3 = crate::s3::after_backup(&game.rules.backup, ctx.save_path, false);
            (200, json!({"backup": made, "s3": s3.as_ref().ok().cloned().flatten(), "s3Error": s3.err()}).to_string())
        }
        ("POST", "/api/backup/s3") => answer(crate::s3::after_backup(&game.rules.backup, ctx.save_path, true).map(|k| json!({"key": k}))),
        // The admin's pause: the world stands still (clock, machines, growth, gauges) until resumed.
        ("POST", "/api/pause") => match body["paused"].as_bool() {
            Some(p) => {
                if game.held != p {
                    game.held = p;
                    println!("world {} by the admin", if p { "paused" } else { "resumed" });
                    game.ext.chat.replies.push_back(if p { "The world is paused by the server admin." } else { "The world goes on." }.to_string());
                }
                (200, json!({"paused": p}).to_string())
            }
            None => (400, "{\"error\":\"'paused': true or false\"}".into()),
        },
        ("GET", "/api/map") => (200, dashboard::map(game).to_string()),
        _ => (404, "{\"error\":\"unknown endpoint\"}".into()),
    }
}

/// The ANSWER frame for an admin request. One past what a frame holds is never sent (the
/// request would end in the front's 10 s timeout, "the game loop did not answer"): its place is
/// an error that says what to do.
fn admin_answer(status: u16, body: String) -> String {
    let answer = json!({"status": status, "body": body}).to_string();
    if ipc::fits(answer.as_bytes()) {
        return answer;
    }
    let error = json!({"error": format!("the answer is {} MiB, more than the link to the front carries ({} MiB): for a world's file, copy it from the worlds folder on the server", answer.len() >> 20, ipc::MAX_FRAME >> 20)});
    json!({"status": 500, "body": error.to_string()}).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game;

    /// An admin API call as the bank answers it: (status, JSON answer).
    fn call(game: &mut Game, method: &str, path: &str, body: Value) -> (u16, Value) {
        let ctx = Ctx { ticks: &Default::default(), save_path: "", build_id: None, data_dir: "", loop_rate: 0, busy: Default::default() };
        let (status, answer) = admin_request(game, method, path, &body, &ctx);
        (status, serde_json::from_str(&answer).unwrap())
    }

    /// Worlds made, read, restored and removed through the API, a backup on demand, and the
    /// admin's pause holding the clock.
    #[test]
    fn worlds_backups_and_the_pause_through_the_api() {
        let Some(mut game) = game::test_game(game::tests::save()) else { return };
        let dir = std::env::temp_dir().join(format!("pcds-bank-worlds-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let save_path = dir.join("world.json").to_string_lossy().to_string();
        game.snapshot().write(&save_path).unwrap();
        let ctx = Ctx { ticks: &Default::default(), save_path: &save_path, build_id: None, data_dir: "", loop_rate: 0, busy: Default::default() };
        let call = |game: &mut Game, method: &str, path: &str, body: Value| {
            let (status, answer) = admin_request(game, method, path, &body, &ctx);
            (status, serde_json::from_str::<Value>(&answer).unwrap())
        };
        let (status, w) = call(&mut game, "GET", "/api/worlds", json!({}));
        assert_eq!((status, w["current"].as_str()), (200, Some("world")));
        let planet = w["choices"]["planets"][0]["id"].as_str().unwrap().to_string();
        let (status, made) = call(&mut game, "POST", "/api/worlds/new", json!({"name": "second", "planet": planet, "seed": 7, "mode": "Chill"}));
        assert_eq!((status, made["seed"].as_u64()), (200, Some(7)), "{made}");
        let made = crate::save::SaveFile::load(&dir.join("second.json").to_string_lossy()).unwrap();
        assert_eq!((made.settings["planetId"].as_str(), made.settings["mode"].as_str()), (Some(planet.as_str()), Some("Chill")));
        for bad in [json!({"name": "second"}), json!({"name": "../x"}), json!({"name": "rules"}), json!({"name": "x", "mode": "Easy"}), json!({"name": "x", "planet": "Nowhere"})] {
            assert_eq!(call(&mut game, "POST", "/api/worlds/new", bad.clone()).0, 400, "{bad}");
        }
        let (status, file) = call(&mut game, "POST", "/api/worlds/file", json!({"name": "second"}));
        assert_eq!((status, file["text"].as_str().map(|t| t.contains("planetId"))), (200, Some(true)));
        // A backup now, then that backup as a new world.
        let (status, b) = call(&mut game, "POST", "/api/backup", json!({}));
        let backup = b["backup"].as_str().unwrap_or_else(|| panic!("{status} {b}")).to_string();
        assert!(backup.starts_with("world-") && b["s3"].is_null() && b["s3Error"].is_null(), "S3 off: {b}");
        assert_eq!(call(&mut game, "POST", "/api/worlds/restore", json!({"backup": backup, "name": "third"})).0, 200);
        assert!(dir.join("third.json").is_file());
        assert_eq!(call(&mut game, "POST", "/api/backup/s3", json!({})).0, 400, "S3 off");
        // Removed into backups/, never the running world.
        assert_eq!(call(&mut game, "POST", "/api/worlds/remove", json!({"name": "world"})).0, 400);
        assert_eq!(call(&mut game, "POST", "/api/worlds/remove", json!({"name": "second"})).0, 200);
        assert!(!dir.join("second.json").exists() && std::fs::read_dir(dir.join("backups")).unwrap().flatten().any(|e| e.file_name().to_string_lossy().starts_with("second-removed-")));
        // Paused: the clock holds with a player in; resumed: it goes on.
        game::tests::join(&mut game, 1, "Ann");
        assert_eq!(call(&mut game, "POST", "/api/pause", json!({"paused": true})).0, 200);
        let before = game.time;
        game.sim_time = Some(game.sim_time.unwrap_or(0.0) + 5.0);
        game.tick();
        assert_eq!(game.time, before, "paused");
        assert_eq!(call(&mut game, "GET", "/api/status", json!({})).1["held"], true);
        assert_eq!(call(&mut game, "POST", "/api/pause", json!({"paused": false})).0, 200);
        game.sim_time = Some(game.sim_time.unwrap() + 5.0);
        game.tick();
        assert!(game.time > before, "resumed");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// An answer too large for a frame is an error the caller can read, not a lost answer.
    #[test]
    fn an_answer_past_a_frame_is_an_error() {
        let small: Value = serde_json::from_str(&admin_answer(200, "{}".into())).unwrap();
        assert_eq!((small["status"].as_u64(), small["body"].as_str()), (Some(200), Some("{}")));
        let big: Value = serde_json::from_str(&admin_answer(200, "x".repeat(ipc::MAX_FRAME))).unwrap();
        assert_eq!(big["status"].as_u64(), Some(500));
        assert!(big["body"].as_str().unwrap().contains("copy it from the worlds folder"), "{big}");
    }

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

    #[test]
    fn rules_change_through_the_api_and_bad_ones_are_refused() {
        let Some(mut game) = game::test_game(game::tests::save()) else { return };
        let post = |game: &mut Game, body: Value| call(game, "POST", "/api/rules", body).0;
        assert_eq!(post(&mut game, json!({"speed": {"growth": 2}})), 200);
        assert_eq!(game.rules.speed.growth, 2.0);
        assert_eq!(post(&mut game, json!({"speed": {"growth": -1}})), 400);
        assert_eq!(post(&mut game, json!({"sped": {}})), 400);
        assert_eq!(game.rules.speed.growth, 2.0, "a refused change keeps the rules");
    }

    /// The hooks' targets and secret never come back from the rules endpoints.
    #[test]
    fn the_rules_endpoints_never_show_hook_secrets() {
        let Some(mut game) = game::test_game(game::tests::save()) else { return };
        let set = json!({"chat": {"webhooks": ["https://example.com/hook/t0ken"], "secret": "s3cret", "exec": ["on-chat", "--key=k3y"]}});
        let (status, answer) = call(&mut game, "POST", "/api/rules", set);
        let (_, read) = call(&mut game, "GET", "/api/rules", json!({}));
        assert_eq!(status, 200);
        for text in [answer.to_string(), read.to_string()] {
            assert!(!text.contains("t0ken") && !text.contains("s3cret") && !text.contains("k3y"), "{text}");
        }
        assert_eq!(read["rules"]["chat"]["secret"], "(set)");
    }

    #[test]
    fn autosave_minutes_stay_within_their_range() {
        let Some(mut game) = game::test_game(game::tests::save()) else { return };
        let (status, _) = call(&mut game, "POST", "/api/config", json!({"save.autosaveMinutes": 7}));
        assert_eq!((status, game.rules.save.autosave_minutes), (200, 7));
        // Out of range or not a whole number: refused, and the interval stays.
        for bad in [json!(160_000_000_000_000_000u64), json!(2.5), json!(-1), json!("5")] {
            let (status, answer) = call(&mut game, "POST", "/api/config", json!({"save.autosaveMinutes": bad}));
            assert_eq!(status, 400, "{bad}: {answer}");
        }
        assert_eq!(game.rules.save.autosave_minutes, 7);
    }

    #[test]
    fn unbanning_what_is_not_banned_is_said() {
        let Some(mut game) = game::test_game(game::tests::save()) else { return };
        game.access.ban("Mallory", None, Some("10.9.9.9")).unwrap();
        let (status, answer) = call(&mut game, "POST", "/api/unban", json!({"value": "Mellory"}));
        assert!(status == 400 && answer["error"].as_str().unwrap().contains("Mellory is not banned"), "{status} {answer}");
        assert_eq!(game.access.lists.banned_names, ["Mallory"]);
        assert_eq!(call(&mut game, "POST", "/api/unban", json!({"value": " mallory "})).0, 200);
        assert_eq!(call(&mut game, "POST", "/api/unban", json!({"value": "10.9.9.9"})).0, 200);
        assert!(game.access.lists.banned_names.is_empty() && game.access.lists.banned_ips.is_empty());
    }

    #[test]
    fn the_config_endpoint_round_trips() {
        let Some(mut game) = game::test_game(game::tests::save()) else { return };
        let (status, config) = call(&mut game, "GET", "/api/config", json!({}));
        assert_eq!(status, 200);
        let knobs = config["knobs"].as_array().unwrap();
        let knob = |key: &str| knobs.iter().find(|k| k["key"] == key).unwrap_or_else(|| panic!("{key}")).clone();
        assert_eq!((knob("speed.growth")["type"].as_str(), knob("speed.growth")["default"].as_f64(), knob("speed.growth")["live"].as_bool()), (Some("float"), Some(1.0), Some(true)));
        assert_eq!(knob("PCDS_ADMIN_TOKEN")["live"], false);
        assert!(knob("PCDS_LISTEN")["readOnly"].is_string());
        assert_eq!((knob("update.auto")["default"].as_bool(), knob("update.checkHours")["default"].as_i64()), (Some(false), Some(24)));
        // Every value the page can change, posted back as read: accepted, nothing changes.
        let writable: serde_json::Map<String, Value> = knobs.iter().filter(|k| k["readOnly"].is_null()).map(|k| (k["key"].as_str().unwrap().to_string(), k["value"].clone())).collect();
        assert!(writable.contains_key("access.bannedNames") && writable.contains_key("save.autosaveMinutes"));
        assert_eq!(call(&mut game, "POST", "/api/config", json!(writable)).1, config);
        // A change of each kind of setting, read back.
        let change = json!({"speed.growth": 2.0, "death.consequences": "DropAllItems", "limits.buildings": {"Drill1": 3}, "access.whitelist": ["Ann"], "save.autosaveMinutes": 10});
        let (status, after) = call(&mut game, "POST", "/api/config", change.clone());
        assert_eq!(status, 200, "{after}");
        for (key, value) in change.as_object().unwrap() {
            let got = after["knobs"].as_array().unwrap().iter().find(|k| k["key"] == key.as_str()).unwrap()["value"].clone();
            assert_eq!(&got, value, "{key}");
        }
        assert_eq!((game.rules.speed.growth, game.access.lists.whitelist.clone(), game.rules.save.autosave_minutes), (2.0, vec!["Ann".to_string()], 10));
        assert_eq!(call(&mut game, "GET", "/api/config", json!({})).1, after);
        // A map is replaced, not merged; null puts back the default.
        call(&mut game, "POST", "/api/config", json!({"limits.buildings": {"Vegetube1": 1}}));
        assert_eq!(game.rules.limits.buildings.keys().collect::<Vec<_>>(), ["Vegetube1"]);
        let resets: serde_json::Map<String, Value> = writable.keys().map(|k| (k.clone(), Value::Null)).collect();
        assert_eq!(call(&mut game, "POST", "/api/config", json!(resets)).0, 200);
        assert_eq!((game.rules.clone(), game.access.lists.whitelist.len()), (rules::Rules::default(), 0));
        // Refused, with the setting named, and nothing of the change applied.
        for (bad, key) in [
            (json!({"speed.growth": 3, "gauges.foodDrain": -1}), "gauges.foodDrain"),
            (json!({"speed.growth": 3, "limits.buildings": {"Iron": 1}}), "limits.buildings"),
            (json!({"speed.growth": 3, "PCDS_LISTEN": "0.0.0.0:1"}), "PCDS_LISTEN"),
            (json!({"speed.growth": 3, "speed.grwoth": 1}), "speed.grwoth"),
            (json!({"speed.growth": 3, "access.whitelist": "Ann"}), "access.whitelist"),
            (json!({"speed.growth": 3, "save.autosaveMinutes": 100000}), "save.autosaveMinutes"),
            (json!({"access.whitelist": ["Bob"], "limits.buildings": {"Iron": 1}}), "limits.buildings"),
        ] {
            let (status, answer) = call(&mut game, "POST", "/api/config", bad.clone());
            assert_eq!((status, answer["key"].as_str()), (400, Some(key)), "{bad}: {answer}");
            assert_eq!(game.rules.speed.growth, 1.0, "{bad}");
            assert!(game.access.lists.whitelist.is_empty(), "{bad}: the access lists are kept as they were");
        }
    }

    #[test]
    fn players_list_teleport_and_chat_through_the_api() {
        let Some(mut game) = game::test_game(game::tests::save()) else { return };
        let object = game::tests::join(&mut game, 1, "Ann");
        game.net.objects.get_mut(&object).unwrap().position = [560.0, 3.0, 500.0];
        game.connect(2, None, false, Some("10.0.0.2".into()));
        let (status, s) = call(&mut game, "GET", "/api/status", json!({}));
        assert_eq!(status, 200);
        let players = s["players"].as_array().unwrap();
        assert_eq!(players.iter().map(|p| (p["id"].as_u64().unwrap(), p["status"].as_str().unwrap())).collect::<Vec<_>>(), [(1, "ready"), (2, "joining")]);
        assert_eq!((players[0]["name"].as_str(), players[0]["position"].clone(), players[0]["gauges"].as_array().map(Vec::len)), (Some("Ann"), json!([560.0, 3.0, 500.0]), Some(4)));
        assert!(players[0]["gaugeMax"].as_array().is_some_and(|m| m.len() == 4 && m.iter().all(|v| v.as_f64().unwrap() > 0.0)), "{}", players[0]);
        assert_eq!((players[1]["ip"].as_str(), players[1]["position"].is_null()), (Some("10.0.0.2"), true));
        let stages = s["stages"]["list"].as_array().unwrap();
        assert!(stages.len() > 3 && stages.windows(2).all(|w| w[0]["start"].as_f64() <= w[1]["start"].as_f64()), "stages in order: {stages:?}");
        assert!(stages.iter().any(|st| st["key"] == s["stages"]["current"]), "the current stage is one of them");
        for key in ["serverVersion", "tables", "world", "objects", "units", "gameTime"] {
            assert!(!s[key].is_null(), "{key}");
        }
        // Teleport: the game's respawn move, to that player only.
        let before = game.net.out.len();
        assert_eq!(call(&mut game, "POST", "/api/teleport", json!({"name": "ann", "position": [10.0, 20.5, -30.0]})).0, 200);
        let sent: Vec<_> = game.net.out[before..].iter().filter(|(_, kind, _)| *kind == protocol::CLIENT_RPC).collect();
        assert_eq!(sent.len(), 1);
        let (to, _, body) = sent[0];
        assert!(to.reaches(1) && !to.reaches(2));
        let mut r = wire::Reader::new(body);
        assert_eq!(r.packed_u64(), Some(object));
        r.packed_u16();
        let method = r.packed_u32().unwrap();
        assert_eq!(game.net.spec.get(method).unwrap().name, "PlayerStatus.DieAndRespawnClientRpc");
        assert_eq!(game.net.spec.decode(method, r.rest()).unwrap(), vec![json!(false), json!(0), json!([10.0, 20.5, -30.0])]);
        assert_eq!(call(&mut game, "POST", "/api/teleport", json!({"id": 1, "position": [1, 2, 3]})).0, 200);
        for bad in [json!({"id": 2, "position": [0, 0, 0]}), json!({"name": "Bob", "position": [0, 0, 0]}), json!({"name": "Ann"}), json!({"name": "Ann", "position": [0, 0]}), json!({"name": "Ann", "position": [0, 0, 0], "x": 1})] {
            assert_eq!(call(&mut game, "POST", "/api/teleport", bad.clone()).0, 400, "{bad}");
        }
        // The map: this planet's buildings.
        let (status, map) = call(&mut game, "GET", "/api/map", json!({}));
        assert_eq!((status, map["planet"].as_str(), map["buildings"].is_array()), (200, Some("Prime"), true));
        assert!(map["buildings"].as_array().unwrap().iter().all(|b| b.as_array().is_some_and(|p| p.len() == 3)), "x, y, z");
    }

    /// An id names that client only, never a player whose name is the same digits; a name two
    /// players share is refused.
    #[test]
    fn players_are_named_by_id_or_by_a_name_only_one_has() {
        let Some(mut game) = game::test_game(game::tests::save()) else { return };
        game::tests::join(&mut game, 1, "4");
        game::tests::join(&mut game, 4, "Ann");
        assert_eq!(call(&mut game, "POST", "/api/kick", json!({"id": 4})).0, 200);
        assert_eq!(game.kicks.iter().map(|(c, _)| *c).collect::<Vec<_>>(), [4], "client 4, not the player named 4");
        game.kicks.clear();
        assert_eq!(call(&mut game, "POST", "/api/kick", json!({"name": "4"})).0, 200);
        assert_eq!(game.kicks.iter().map(|(c, _)| *c).collect::<Vec<_>>(), [1]);
        // The game refuses a second player of one name; two of them is a rename away.
        game::tests::join(&mut game, 5, "Bob");
        game.clients.get_mut(&5).unwrap().ready.as_mut().unwrap().name = "ann".into();
        let (status, answer) = call(&mut game, "POST", "/api/kick", json!({"name": "ANN"}));
        assert_eq!(status, 400, "{answer}");
        assert!(answer["error"].as_str().unwrap().contains("client id"));
        assert_eq!(call(&mut game, "POST", "/api/kick", json!({})).0, 400);
        assert_eq!(call(&mut game, "POST", "/api/ban", json!({"id": 99})).0, 400, "an id names an online client");
    }

    /// A ban that cannot be written answers 500 (it would be lost at the next start).
    #[test]
    fn a_ban_that_cannot_be_kept_answers_500() {
        let Some(mut game) = game::test_game(game::tests::save()) else { return };
        game.access = access::Access::load(Some("/nonexistent-dir-pcds/access.json".into()));
        let (status, answer) = call(&mut game, "POST", "/api/ban", json!({"name": "Mallory"}));
        assert_eq!(status, 500, "{answer}");
        assert!(game.access.lists.banned_names.is_empty());
    }

    /// A backup that fails is no secret kept in the log: /api/status and the answer of POST /api/save
    /// say so (the save itself still goes through), until a backup is made.
    #[test]
    fn a_backup_that_fails_is_in_the_status_and_in_the_answer_of_a_save() {
        let Some(mut game) = game::test_game(game::tests::save()) else { return };
        let dir = std::env::temp_dir().join(format!("pcds-backup-status-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("world.json").to_string_lossy().to_string();
        let ctx = Ctx { ticks: &Default::default(), save_path: &path, build_id: None, data_dir: "", loop_rate: 0, busy: Default::default() };
        std::fs::write(&path, "{}").unwrap();
        // backups/ is a file: no backup can be made.
        std::fs::write(dir.join("backups"), "in the way").unwrap();
        let ask = |game: &mut Game, method: &str, path: &str| {
            let (status, answer) = admin_request(game, method, path, &json!({}), &ctx);
            (status, serde_json::from_str::<Value>(&answer).unwrap())
        };
        let (status, saved) = ask(&mut game, "POST", "/api/save");
        assert_eq!(status, 200, "the save itself went through: {saved}");
        assert!(saved["backup"]["error"]["message"].as_str().is_some_and(|m| m.contains("world.json")) && saved["backup"]["error"]["at"].as_u64() > Some(0) && saved["backup"]["lastOkAt"].is_null(), "{saved}");
        assert_eq!(SaveFile::load(&path).map(|s| !s.objects.is_empty()).ok(), Some(true));
        let (_, status) = ask(&mut game, "GET", "/api/status");
        assert_eq!(status["backup"], saved["backup"]);
        assert_eq!(status["backup"]["enabled"], true);
        // Mended: the next save makes the backup, and the failure is gone.
        std::fs::remove_file(dir.join("backups")).unwrap();
        let (_, saved) = ask(&mut game, "POST", "/api/save");
        assert!(saved["backup"]["error"].is_null() && saved["backup"]["lastOkAt"].as_u64() > Some(0), "{saved}");
        assert_eq!(std::fs::read_dir(dir.join("backups")).unwrap().count(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A save that cannot be written (here a folder where the save goes): /api/save says so, as
    /// the status does; the autosave tries again; the last save keeps the world as a snapshot the
    /// next start runs, and the bank ends with 1.
    #[test]
    fn a_failed_save_is_reported_retried_and_kept() {
        let Some(mut game) = game::test_game(game::tests::save()) else { return };
        let dir = std::env::temp_dir().join(format!("pcds-savefail-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("world.json")).unwrap();
        let path = dir.join("world.json").to_string_lossy().to_string();
        let ctx = Ctx { ticks: &Default::default(), save_path: &path, build_id: None, data_dir: "", loop_rate: 0, busy: Default::default() };
        let (status, answer) = admin_request(&mut game, "POST", "/api/save", &json!({}), &ctx);
        assert!(status == 500 && answer.contains("world.json"), "{status} {answer}");
        let (_, s) = admin_request(&mut game, "GET", "/api/status", &json!({}), &ctx);
        let last = serde_json::from_str::<Value>(&s).unwrap()["lastSave"].clone();
        assert!(last["ok"] == false && last["at"].as_u64() > Some(0) && last["error"].as_str().is_some_and(|e| e.contains("world.json")), "{last}");
        // The autosave writes on a thread: its failure comes back as a retry at the next one.
        game.advanced = true;
        autosave(&mut game, &path);
        settle(&mut game, true);
        assert!(game.advanced, "the next autosave tries again");
        let (_, s) = admin_request(&mut game, "GET", "/api/status", &json!({}), &ctx);
        assert_eq!(serde_json::from_str::<Value>(&s).unwrap()["lastSave"]["ok"], false);
        assert_eq!(final_save(&mut game, &path), 1);
        let kept = std::fs::read(crate::front::snapshot_path(&path)).unwrap();
        let tables = crate::test_data().unwrap();
        let (n, d, s) = crate::tables(tables);
        let scenes = crate::scene_tables(tables, &n, &d);
        let back = crate::handoff::restore_bytes(n, d, s, &kept, scenes).unwrap();
        assert_eq!(back.wos.len(), game.wos.len());
        // Written: the status says so.
        std::fs::remove_dir(&path).unwrap();
        game.advanced = true;
        autosave(&mut game, &path);
        settle(&mut game, true);
        assert!(!game.advanced);
        let (_, s) = admin_request(&mut game, "GET", "/api/status", &json!({}), &ctx);
        assert_eq!(serde_json::from_str::<Value>(&s).unwrap()["lastSave"]["ok"], true);
        assert_eq!(final_save(&mut game, &path), 0);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// The status carries the tick times.
    #[test]
    fn the_status_carries_the_tick_times() {
        let mut t = TickTimes::default();
        let start = Instant::now();
        t.record(start, Duration::from_micros(40));
        let Some(mut game) = game::test_game(game::tests::save()) else { return };
        let ctx = Ctx { ticks: &t, save_path: "", build_id: None, data_dir: "", loop_rate: 0, busy: Default::default() };
        let (_, status) = admin_request(&mut game, "GET", "/api/status", &json!({}), &ctx);
        let status: Value = serde_json::from_str(&status).unwrap();
        assert_eq!(status["tick"]["windowSeconds"], 60);
        assert!(status["tick"]["p50Micros"].is_number() || status["tick"]["samples"] == 0);
    }
}
