//! A bank's game process (docs/hot-update.md): the world and the tick, started by the front
//! from the active bank's binary, on the socket the front gave it (PCDS_IPC_FD). It loads its
//! tables, says HELLO, and then either loads the save (START) or restores the snapshot of the
//! bank before it (RESTORE). It builds and parses the Netcode batches and decides what goes out
//! when; the front only moves them. It saves the world on SAVE (the front's autosave timer), on
//! STOP and when the front is gone. At a flip it stops at a tick boundary, sends what is
//! queued, writes its snapshot into a memfd and waits: EXIT (the new bank runs) or RESUME.
//!
//!   pcds bank DATA_DIR SAVE.json        (started by the front, never by hand)
//!     PCDS_FLIP_VERIFY   after a restore, check the state equals the snapshot (tests)
//!     PCDS_BANK_FAIL     tests: "restore" dies on RESTORE, "protocol" says another protocol,
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
use crate::net::Target;
use crate::save::SaveFile;
use crate::{access, chathooks, config, dashboard, protocol, rules, save, status, units, wire};

/// What this bank says in its HELLO, and `pcds bank-info` prints: its version, the front
/// protocol it speaks, the snapshot format it writes and the ones it reads, and the format of
/// the game tables it loads (and its read-game writes).
pub fn info() -> Value {
    json!({"version": env!("CARGO_PKG_VERSION"), "protocol": ipc::PROTOCOL, "format": crate::handoff::FORMAT, "reads": [crate::handoff::FORMAT - 1, crate::handoff::FORMAT], "tablesFormat": crate::native::tables::TABLES_FORMAT})
}

/// What a client may be sent before it is a player (PlayerReady): the world in its
/// ConnectionApproved and this much more (the world's changes while it loads). A session from a
/// forged address never gets further: its world is all a spoofer can have sent to the victim.
const LOADING_ALLOWANCE: i64 = 8 << 20;

fn fail_for_test(what: &str) -> bool {
    std::env::var("PCDS_BANK_FAIL").is_ok_and(|v| v == what)
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
    let mut hello = info();
    hello["gameVersion"] = net_data.game_version.clone().into();
    let protocol = if fail_for_test("protocol") { 999 } else { ipc::PROTOCOL };
    if fail_for_test("slow-hello") {
        println!("bank: holding its hello for 4 s (PCDS_BANK_FAIL=slow-hello)");
        std::thread::sleep(Duration::from_secs(4));
    }
    link.send(ipc::HELLO, protocol, hello.to_string().as_bytes());
    let (mut verified, mut verify_ms) = (Value::Null, 0.0);
    let mut started = Instant::now();
    let game = match link.recv() {
        Some(f) if f.kind == ipc::START => load(dir, save_path, net_data, data, spec, scenes),
        Some(f) if f.kind == ipc::RESTORE => {
            started = Instant::now();
            if fail_for_test("restore") {
                eprintln!("bank: dying on the restore (PCDS_BANK_FAIL=restore)");
                std::process::exit(3);
            }
            let recover = serde_json::from_slice::<Value>(&f.body).is_ok_and(|b| b["recover"] == true);
            let snapshot = f.fd.as_ref().ok_or("no memfd".to_string()).and_then(|fd| ipc::read_memfd(fd).map_err(|e| e.to_string()));
            let restored = snapshot.and_then(|s| crate::handoff::restore_bytes(net_data, data, spec, &s, scenes).map(|g| (g, s)));
            match restored {
                Ok((mut g, snapshot)) => {
                    if std::env::var_os("PCDS_FLIP_VERIFY").is_some() {
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
    let ready = json!({"restoreMs": started.elapsed().as_secs_f64() * 1000.0 - verify_ms, "verifyMs": verify_ms, "verified": verified});
    if fail_for_test("slow-ready") {
        println!("bank: holding its ready for 2 s (PCDS_BANK_FAIL=slow-ready)");
        std::thread::sleep(Duration::from_secs(2));
    }
    link.send(ipc::READY, 0, ready.to_string().as_bytes());
    if fail_for_test("die-after-ready") {
        println!("bank: ending 1 s after its ready (PCDS_BANK_FAIL=die-after-ready)");
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
    if let Err(e) = crate::game::check_save_format(&save.settings) {
        eprintln!("{save_path}: {e}");
        std::process::exit(1);
    }
    let folder = std::path::Path::new(save_path).parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(std::path::Path::new("."));
    save::sweep(folder);
    save::sweep(&folder.join("backups"));
    // The save as it was before this server touched it (as every backup: none with PCDS_BACKUPS=0).
    // At most one per 10 minutes: a crash loop must not push the hourly backups out.
    if let Err(e) = save::backup(save_path, Duration::from_secs(600), backups_kept()) {
        eprintln!("backup failed: {e}");
    }
    let session_number = config::env_number("PCDS_SESSION") as u32;
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
    let version = net_data.game_version.clone();
    let mut game = Game::new(net_data, data, spec, save, scenes);
    rules::start(&mut game, loaded_rules, rules_path);
    game.access = loaded_access;
    game.session_number = session_number;
    println!("world: {} world objects, {} network objects, planet {}, game {version}", game.wos.len(), game.net.objects.len(), game.planet_id());
    game
}

fn beside_save(save_path: &str, name: &str) -> String {
    std::path::Path::new(save_path).with_file_name(name).to_string_lossy().to_string()
}

fn rules_path(save_path: &str) -> String {
    crate::env("PCDS_RULES").unwrap_or_else(|| beside_save(save_path, "rules.json"))
}

fn access_path(save_path: &str) -> String {
    crate::env("PCDS_ACCESS").unwrap_or_else(|| beside_save(save_path, "access.json"))
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

/// What the status shows of this bank's loop: loops in the last second, its work time and
/// longest loop.
struct Ctx<'a> {
    save_path: &'a str,
    build_id: Option<&'a str>,
    data_dir: &'a str,
    loop_rate: u32,
    busy: (Duration, Duration),
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
        // The hooks' settings, once at the start (a restored game's too).
        game.ext.chat.changed = true;
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
                        let mut autosave = r["autosaveMinutes"].as_u64().unwrap_or_else(crate::autosave_start);
                        let ctx = Ctx { save_path: &self.save_path, build_id: self.build_id.as_deref(), data_dir: &self.data_dir, loop_rate: loops.2, busy: busy_last };
                        let request = || admin_request(game, r["method"].as_str().unwrap_or(""), r["path"].as_str().unwrap_or(""), &r["body"], &ctx, &mut autosave);
                        let (status, body) = contained("an admin request", request).unwrap_or_else(|| (500, "{\"error\":\"the server failed on this request\"}".to_string()));
                        self.link.send(ipc::ANSWER, f.n, admin_answer(status, body, autosave).as_bytes());
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
                        let t = Instant::now();
                        match ipc::memfd("pcds-snapshot", |w| crate::handoff::snapshot_to(game, w)) {
                            Ok(fd) => {
                                let len = std::fs::File::from(fd.try_clone().expect("dup")).metadata().map(|m| m.len()).unwrap_or(0);
                                let head = json!({"bytes": len, "ms": t.elapsed().as_secs_f64() * 1000.0});
                                self.link.send_fd(ipc::SNAPSHOT, f.n, head.to_string().as_bytes(), Some(fd));
                                // The world stays in memory until the new bank runs.
                                match self.link.recv() {
                                    Some(f) if f.kind == ipc::RESUME => println!("bank: resumed, the new bank failed"),
                                    Some(f) if f.kind == ipc::EXIT => std::process::exit(0),
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
            if frame && std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| game.tick())).is_err() {
                // The whole world may be half-updated: keep what there is; the front starts the
                // bank again from the save.
                eprintln!("the game tick panicked: saving and stopping");
                let _ = persist(game, &self.save_path);
                std::process::exit(1);
            }
            if std::mem::take(&mut game.save_requested) {
                let _ = persist(game, &self.save_path);
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

/// The chat events for the front's hooks, and the hooks' settings when they changed.
fn forward_chat(game: &mut Game, link: &mut Link) {
    for event in game.ext.chat.outbox.drain(..) {
        link.send(ipc::CHAT, 0, serde_json::to_string(&event).unwrap_or_default().as_bytes());
    }
    if std::mem::take(&mut game.ext.chat.changed) {
        link.send(ipc::CHATCFG, 0, serde_json::to_string(&game.rules.chat).unwrap_or_default().as_bytes());
    }
}

/// DisconnectReasonMessage: what a kicked or refused player is shown.
pub fn reason_batch(reason: &str) -> Vec<u8> {
    let units: Vec<u16> = reason.encode_utf16().collect();
    let mut w = wire::Writer::new();
    w.packed_i32(0).u32(units.len() as u32);
    units.iter().for_each(|u| {
        w.u16(*u);
    });
    wire::batch(&[(crate::DISCONNECT_REASON, w.0)])
}

/// Kicks a client that is no player yet and was sent more than its allowance; forgets the
/// ones that became players or left.
fn check_loading(game: &mut Game, loading: &mut HashMap<u64, i64>) {
    loading.retain(|c, left| match game.clients.get(c) {
        Some(client) if client.ready.is_none() => {
            if *left < 0 {
                println!("client {c}: sent {} MiB more than its world while not a player; dropped", LOADING_ALLOWANCE >> 20);
                game.kicks.push((*c, "MENU_Network_Error_Timeout".into()));
            }
            *left >= 0
        }
        _ => false,
    });
}

/// `loading`: what each client that is no player yet may still be sent, counted down.
fn send(link: &mut Link, mail: &mut crate::idmap::FxHashMap<u64, wire::Batch>, clients: &[u64], loading: &mut HashMap<u64, i64>) {
    for c in clients {
        if let Some(b) = mail.remove(c).filter(|b| !b.is_empty()) {
            let batch = b.finish();
            // Over the frame limit the front would end the link: that client goes instead (it
            // cannot go on without the batch). A join batch is ~14 bytes per world object.
            if !ipc::fits(&batch) {
                eprintln!("client {c}: a batch of {} bytes is over the {} bytes a frame holds; it is dropped", batch.len(), ipc::MAX_FRAME);
                link.send(ipc::DATA, *c, &reason_batch("The world is too large for this server to send"));
                link.send(ipc::KICK, *c, &[]);
                continue;
            }
            if let Some(left) = loading.get_mut(c) {
                *left -= batch.len() as i64;
            }
            link.send(ipc::DATA, *c, &batch);
        }
    }
}

/// Who an API call names: {"id": N} is that client id exactly; {"name": S} (or the plugin
/// API's {"player": S}) a player's name in any case.
enum Who {
    Id(u64),
    Name(String),
}

fn who(body: &Value) -> Result<Who, (u16, String)> {
    if let Some(id) = body.get("id").filter(|v| !v.is_null()) {
        return id.as_u64().map(Who::Id).ok_or_else(|| (400, "{\"error\":\"'id': a client id\"}".into()));
    }
    let name = ["name", "player"].iter().find_map(|k| body[*k].as_str().map(str::trim).filter(|s| !s.is_empty()));
    name.map(|n| Who::Name(n.to_string())).ok_or_else(|| (400, "{\"error\":\"'id' or 'name' is required\"}".into()))
}

/// The online client named; None: nobody online is. Two players with that name (any case) are
/// refused: the caller must give the id.
fn find_client(game: &Game, who: &Who) -> Result<Option<u64>, (u16, String)> {
    match who {
        Who::Id(id) => Ok(game.clients.contains_key(id).then_some(*id)),
        Who::Name(name) => {
            let found: Vec<u64> = game.clients.values().filter(|c| c.ready.as_ref().is_some_and(|s| s.name.eq_ignore_ascii_case(name))).map(|c| c.id).collect();
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

fn admin_request(game: &mut Game, method: &str, path: &str, body: &Value, ctx: &Ctx, autosave: &mut u64) -> (u16, String) {
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
    match (method, path) {
        // The bank's part; the front adds its own (invite code, network, banks, traffic).
        ("GET", "/api/status") => {
            let mut players: Vec<Value> = game.clients.values().map(|c| dashboard::player(game, c, c.ip.clone().unwrap_or_default())).collect();
            players.sort_by_key(|p| p["id"].as_u64());
            let terraformation = units::value(game, units::Kind::Terraformation, 0);
            let planet_units: serde_json::Map<String, Value> = game.units.planet(game.planet_hash()).map(|p| p.units.iter().map(|u| (u.kind.unit_type().to_string(), json!({"value": u.total, "perSecond": u.per_sec}))).collect()).unwrap_or_default();
            let body = json!({
                "state": "running",
                "gameVersion": game.net_data.game_version,
                "save": std::path::Path::new(ctx.save_path).file_stem().map(|s| s.to_string_lossy().to_string()),
                "autosaveMinutes": *autosave,
                "tickRate": game.net_data.tick_rate,
                "rules": rules::changes(&game.rules),
                "fps": ctx.loop_rate,
                "busyPercent": (ctx.busy.0.as_secs_f64() * 1000.0).round() / 10.0,
                "longestLoopMicros": ctx.busy.1.as_micros() as u64,
                "planet": game.planet_id(),
                "terraformation": terraformation,
                "stages": crate::meteo::stages(game),
                "gameTime": game.time,
                "units": planet_units,
                "players": players,
                "access": game.access.lists,
                "serverVersion": env!("CARGO_PKG_VERSION"),
                "tables": tables_status(game, ctx),
                "world": {"seed": game.save.settings["worldSeed"], "settings": game.save.settings},
                "objects": {"world": game.wos.len(), "network": game.net.objects.len(), "inventories": game.inventories.len()},
                "held": game.held,
                "s3": crate::s3::status(&game.rules.backup),
                "lastSave": LAST_SAVE.with_borrow(Value::clone),
            });
            (200, body.to_string())
        }
        ("POST", "/api/save") => kept(persist(game, ctx.save_path)),
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
        ("POST", "/api/access") => {
            // The lists given replace those; the others stay. A misspelt list or an entry that
            // is not a string is refused, never dropped.
            let mut lists = serde_json::to_value(&game.access.lists).unwrap_or_default();
            match body.as_object() {
                Some(given) => {
                    for (k, v) in given {
                        lists[k] = v.clone();
                    }
                }
                None => return (400, "{\"error\":\"a JSON object of lists\"}".into()),
            }
            let lists = match serde_json::from_value::<access::Lists>(lists).map_err(|e| e.to_string()).and_then(|l| l.check().map(|_| l)) {
                Ok(l) => l,
                Err(e) => return (400, json!({"error": e}).to_string()),
            };
            if let Err(e) = game.access.set(lists) {
                return (500, json!({"error": e}).to_string());
            }
            enforce(game);
            ok()
        }
        // chat.webhooks, chat.secret and chat.exec only as "(set)": they may hold tokens.
        ("GET", "/api/rules") => (200, json!({"rules": rules::shown(&game.rules), "defaults": rules::Rules::default()}).to_string()),
        // A JSON merge patch of the rules: {"speed": {"growth": 2}}; null resets a rule.
        ("POST", "/api/rules") => match game.rules.patched(body).and_then(|new| rules::set(game, new)) {
            Ok(()) => (200, json!({"rules": rules::shown(&game.rules)}).to_string()),
            Err(e) => (400, json!({"error": e}).to_string()),
        },
        ("POST", "/api/settings") => {
            // Another key (the plugin API's own) is not this server's to refuse; a given
            // autosaveMinutes that is no whole number must not be taken as done.
            match body.get("autosaveMinutes").map(|m| m.as_u64()) {
                Some(None) => return (400, "{\"error\":\"'autosaveMinutes': a whole number of minutes\"}".into()),
                Some(Some(m)) => *autosave = m.min(crate::MAX_AUTOSAVE),
                None => {}
            }
            ok()
        }
        // Every setting with its schema entry and value; a change of any, {"speed.growth": 2}.
        ("GET", "/api/config") => (200, config::get(game, *autosave).to_string()),
        ("POST", "/api/config") => match config::set(game, autosave, body) {
            Ok(()) => (200, config::get(game, *autosave).to_string()),
            Err(e) => (400, e.to_string()),
        },
        ("POST", "/api/teleport") => match dashboard::body::<dashboard::Teleport>(body) {
            Ok(t) if !t.position.iter().all(|v| v.is_finite()) => (400, "{\"error\":\"position: three finite numbers\"}".into()),
            Ok(t) => {
                let named = match t.id {
                    Some(id) => Who::Id(id),
                    None => match t.name.or(t.player) {
                        Some(n) => Who::Name(n),
                        None => return (400, "{\"error\":\"'id' or 'name' is required\"}".into()),
                    },
                };
                match find_client(game, &named) {
                    Ok(Some(c)) if status::teleport(game, c, t.position) => {
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
            w["backupsKept"] = backups_kept().into();
            w["s3"] = crate::s3::status(&game.rules.backup);
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
            if backups_kept() == 0 {
                return (400, "{\"error\":\"backups are off (PCDS_BACKUPS=0)\"}".into());
            }
            if let Err(e) = game.snapshot().write(ctx.save_path) {
                return (500, json!({"error": format!("save failed: {e}")}).to_string());
            }
            if let Err(e) = save::backup(ctx.save_path, Duration::ZERO, backups_kept()) {
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
fn admin_answer(status: u16, body: String, autosave: u64) -> String {
    let answer = json!({"status": status, "body": body, "autosaveMinutes": autosave}).to_string();
    if ipc::fits(answer.as_bytes()) {
        return answer;
    }
    let error = json!({"error": format!("the answer is {} MiB, more than the link to the front carries ({} MiB): for a world's file, copy it from the worlds folder on the server", answer.len() >> 20, ipc::MAX_FRAME >> 20)});
    json!({"status": 500, "body": error.to_string(), "autosaveMinutes": autosave}).to_string()
}

/// /api/status's "tables": the version and build these tables are for, the installed game's
/// build (PCDS_GAME_DIR, which pcds run sets), and a warning when that game has no tables of
/// this server's format (a game update `pcds update` has not read yet).
fn tables_status(game: &Game, ctx: &Ctx) -> Value {
    let installed = installed_game(ctx.data_dir);
    let warning = installed["warning"].clone();
    json!({"version": game.net_data.game_version, "buildId": ctx.build_id, "game": installed, "warning": warning})
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

fn backups_kept() -> usize {
    config::env_number("PCDS_BACKUPS") as usize
}

/// One client's share of the game. A panic there (the hook printed it) kicks that client and
/// saves at once instead of ending the server for everyone.
fn guarded(game: &mut Game, client: u64, save_path: &str, work: impl FnOnce(&mut Game)) {
    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| work(game))).is_err() {
        eprintln!("client {client}: the server panicked on it; kicked, saving");
        game.kick(client, "Server error");
        let _ = persist(game, save_path);
    }
}

/// The admin's or a chat hook's work: a panic there (the hook printed it) is logged and the bank
/// goes on (there is no client to kick, and the world is the one the clients have). Some(what
/// the work returned) when it finished.
pub(crate) fn contained<T>(what: &str, work: impl FnOnce() -> T) -> Option<T> {
    let done = std::panic::catch_unwind(std::panic::AssertUnwindSafe(work));
    if done.is_err() {
        eprintln!("{what}: the server panicked on it; going on");
    }
    done.ok()
}

thread_local! {
    /// The last save's outcome, for GET /api/status: {"ok", "at" (Unix seconds), "error"}. The
    /// game loop's thread saves and answers the API.
    static LAST_SAVE: std::cell::RefCell<Value> = const { std::cell::RefCell::new(Value::Null) };
}

fn persist(game: &mut Game, path: &str) -> Result<(), String> {
    let every = config::env_number("PCDS_BACKUP_MINUTES");
    if let Err(e) = save::backup(path, Duration::from_secs(every.saturating_mul(60)), backups_kept()) {
        eprintln!("backup failed: {e}");
    }
    let saved = game.snapshot().write(path).map_err(|e| format!("{path}: {e}"));
    let at = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    LAST_SAVE.set(json!({"ok": saved.is_ok(), "at": at, "error": saved.as_ref().err()}));
    match &saved {
        Ok(()) => println!("saved {path}"),
        Err(e) => eprintln!("save failed: {e}"),
    }
    // The newest backup off-site (on a thread), when S3 is set and it is not there yet.
    if let Err(e) = crate::s3::after_backup(&game.rules.backup, path, false) {
        eprintln!("backup: {e}");
    }
    saved
}

/// The front's autosave. A world that stood still since the last one (rules:
/// world.pauseWhenEmpty) is that save already: no write, and no backup that only repeats the
/// one before. A save that failed is tried again at the next one.
fn autosave(game: &mut Game, path: &str) {
    if std::mem::take(&mut game.advanced) && persist(game, path).is_err() {
        game.advanced = true;
    }
}

/// The save before the bank ends, and the code it ends with. When the save fails (a full disk,
/// a folder made read-only) the world is kept as a snapshot beside it, where the front keeps
/// one (front::kept_snapshot: the next start runs it), else in the temporary folder; the code
/// is 1, and the front says so.
fn final_save(game: &mut Game, path: &str) -> i32 {
    let Err(e) = persist(game, path) else { return 0 };
    let beside = crate::front::snapshot_path(path);
    let name = std::path::Path::new(&beside).file_name().unwrap_or_default().to_string_lossy().to_string();
    let elsewhere = std::env::temp_dir().join(format!("pcds-{}-{name}", std::process::id())).to_string_lossy().to_string();
    let mut bytes = Vec::new();
    let kept = crate::handoff::snapshot_to(game, &mut bytes).and_then(|()| {
        let write = |to: &str| save::write_atomic(to, &bytes).map_err(|e| format!("{to}: {e}"));
        write(&beside).map(|()| None).or_else(|b| write(&elsewhere).map(|()| Some(b)))
    });
    match kept {
        Ok(None) => eprintln!("the world could not be saved ({e}): it is kept in {beside}, and the next start runs it"),
        Ok(Some(b)) => eprintln!("the world could not be saved ({e}) nor kept beside it ({b}): it is kept in {elsewhere}; move it to {beside} for the next start to run it"),
        Err(k) => eprintln!("the world could not be saved ({e}) nor kept ({k}): what it did since the last save is lost"),
    }
    1
}

/// Sorts the queued messages into each approved client's batch, in queue order. A client gets
/// messages queued after its ConnectionApproved: everything before is in the world it was sent.
fn post(game: &mut Game, approved: &mut HashSet<u64>, mail: &mut crate::idmap::FxHashMap<u64, wire::Batch>, loading: &mut HashMap<u64, i64>) {
    for (to, kind, body) in game.net.out.drain(..) {
        if kind == protocol::CONNECTION_APPROVED {
            if let Target::Only(c) = to {
                approved.insert(c);
                loading.insert(c, body.len() as i64 + LOADING_ALLOWANCE);
                mail.entry(c).or_default().push(kind, &body);
            }
            continue;
        }
        for c in approved.iter().filter(|c| to.reaches(**c)) {
            mail.entry(*c).or_default().push(kind, &body);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game;

    /// A panic in one client's work kicks it and saves; the next loop goes on.
    #[test]
    fn a_panicking_client_is_kicked_and_the_world_saved() {
        let Some(mut game) = game::test_game(game::tests::save()) else { return };
        let dir = std::env::temp_dir().join(format!("pcds-guarded-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("world.json").to_string_lossy().to_string();
        guarded(&mut game, 7, &path, |_| panic!("test panic"));
        assert_eq!(game.kicks.iter().map(|(c, _)| *c).collect::<Vec<_>>(), [7]);
        assert!(!SaveFile::load(&path).unwrap().objects.is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A client that is no player yet gets its world and LOADING_ALLOWANCE more, then is
    /// dropped; a player has no such bound.
    #[test]
    fn a_client_that_never_becomes_a_player_is_sent_its_world_and_a_bound() {
        let Some(mut game) = game::test_game(game::tests::save()) else { return };
        let (ours, mut theirs) = UnixStream::pair().unwrap();
        std::thread::spawn(move || std::io::copy(&mut theirs, &mut std::io::sink()));
        let mut link = Link::new(ours);
        let (mut approved, mut mail, mut loading) = (HashSet::new(), Default::default(), HashMap::new());
        game::tests::approve(&mut game, 1);
        game::tests::join(&mut game, 2, "Ann");
        post(&mut game, &mut approved, &mut mail, &mut loading);
        assert!(loading[&1] > LOADING_ALLOWANCE && loading[&2] > LOADING_ALLOWANCE, "both got their world");
        send(&mut link, &mut mail, &[1, 2], &mut loading);
        check_loading(&mut game, &mut loading);
        assert!(!loading.contains_key(&2), "a player is not counted");
        assert!(game.kicks.is_empty() && loading[&1] > 0 && loading[&1] < LOADING_ALLOWANCE);
        // The world's changes while it loads, past the allowance.
        let chunk = vec![0u8; 1 << 20];
        for _ in 0..=(LOADING_ALLOWANCE >> 20) {
            game.out_to(Target::All, (protocol::CLIENT_RPC, chunk.clone()));
            post(&mut game, &mut approved, &mut mail, &mut loading);
            send(&mut link, &mut mail, &[1, 2], &mut loading);
            check_loading(&mut game, &mut loading);
        }
        assert_eq!(game.kicks.iter().map(|(c, _)| *c).collect::<Vec<_>>(), [1]);
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
        let ctx = Ctx { save_path: &path, build_id: None, data_dir: "", loop_rate: 0, busy: Default::default() };
        let (status, answer) = admin_request(&mut game, "POST", "/api/save", &json!({}), &ctx, &mut 5);
        assert!(status == 500 && answer.contains("world.json"), "{status} {answer}");
        let (_, s) = admin_request(&mut game, "GET", "/api/status", &json!({}), &ctx, &mut 5);
        let last = serde_json::from_str::<Value>(&s).unwrap()["lastSave"].clone();
        assert!(last["ok"] == false && last["at"].as_u64() > Some(0) && last["error"].as_str().is_some_and(|e| e.contains("world.json")), "{last}");
        game.advanced = true;
        autosave(&mut game, &path);
        assert!(game.advanced, "the next autosave tries again");
        assert_eq!(final_save(&mut game, &path), 1);
        let kept = std::fs::read(crate::front::snapshot_path(&path)).unwrap();
        let tables = crate::test_data().unwrap();
        let (n, d, s) = crate::tables(tables);
        let scenes = crate::scene_tables(tables, &n, &d);
        let back = crate::handoff::restore_bytes(n, d, s, &kept, scenes).unwrap();
        assert_eq!(back.wos.len(), game.wos.len());
        // Written: the status says so.
        std::fs::remove_dir(&path).unwrap();
        autosave(&mut game, &path);
        assert!(!game.advanced);
        let (_, s) = admin_request(&mut game, "GET", "/api/status", &json!({}), &ctx, &mut 5);
        assert_eq!(serde_json::from_str::<Value>(&s).unwrap()["lastSave"]["ok"], true);
        assert_eq!(final_save(&mut game, &path), 0);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A batch over the frame limit never reaches the socket (the front would end the link and
    /// the bank with it): its client is told why and dropped; the link goes on.
    #[test]
    fn a_batch_over_the_frame_limit_drops_its_client_not_the_bank() {
        let (a, b) = UnixStream::pair().unwrap();
        a.set_nonblocking(true).unwrap();
        let (mut front, mut bank) = (Link::new(a), Link::new(b));
        let mut mail: crate::idmap::FxHashMap<u64, wire::Batch> = Default::default();
        mail.entry(4).or_default().push(protocol::RPC, &vec![0; ipc::MAX_FRAME]);
        mail.entry(5).or_default().push(protocol::RPC, b"small");
        send(&mut bank, &mut mail, &[4, 5], &mut HashMap::new());
        // Any frame over the limit, whoever sends it: not sent.
        bank.send(ipc::DATA, 6, &vec![0; ipc::MAX_FRAME]);
        bank.send(ipc::GONE, 0, &[]);
        let mut frames = Vec::new();
        while frames.last() != Some(&(ipc::GONE, 0)) {
            assert!(front.fill(usize::MAX));
            while let Some(f) = front.next() {
                frames.push((f.kind, f.n));
            }
        }
        assert!(front.broken.is_none());
        assert_eq!(frames, [(ipc::DATA, 4), (ipc::KICK, 4), (ipc::DATA, 5), (ipc::GONE, 0)]);
    }

    /// An admin API call as the bank answers it: (status, JSON answer).
    fn call(game: &mut Game, method: &str, path: &str, body: Value, autosave: &mut u64) -> (u16, Value) {
        let ctx = Ctx { save_path: "", build_id: None, data_dir: "", loop_rate: 0, busy: Default::default() };
        let (status, answer) = admin_request(game, method, path, &body, &ctx, autosave);
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
        let mut autosave = 5;
        let ctx = Ctx { save_path: &save_path, build_id: None, data_dir: "", loop_rate: 0, busy: Default::default() };
        let mut call = |game: &mut Game, method: &str, path: &str, body: Value| {
            let (status, answer) = admin_request(game, method, path, &body, &ctx, &mut autosave);
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

    /// A ban and a rules change written after a snapshot are in the game restored from it.
    /// An answer too large for a frame is an error the caller can read, not a lost answer.
    #[test]
    fn an_answer_past_a_frame_is_an_error() {
        let small: Value = serde_json::from_str(&admin_answer(200, "{}".into(), 5)).unwrap();
        assert_eq!((small["status"].as_u64(), small["body"].as_str()), (Some(200), Some("{}")));
        let big: Value = serde_json::from_str(&admin_answer(200, "x".repeat(ipc::MAX_FRAME), 5)).unwrap();
        assert_eq!((big["status"].as_u64(), big["autosaveMinutes"].as_u64()), (Some(500), Some(5)));
        assert!(big["body"].as_str().unwrap().contains("copy it from the worlds folder"), "{big}");
    }

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
        let post = |game: &mut Game, body: Value| call(game, "POST", "/api/rules", body, &mut 5).0;
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
        let mut autosave = 5;
        let set = json!({"chat": {"webhooks": ["https://example.com/hook/t0ken"], "secret": "s3cret", "exec": ["on-chat", "--key=k3y"]}});
        let (status, answer) = call(&mut game, "POST", "/api/rules", set, &mut autosave);
        let (_, read) = call(&mut game, "GET", "/api/rules", json!({}), &mut autosave);
        assert_eq!(status, 200);
        for text in [answer.to_string(), read.to_string()] {
            assert!(!text.contains("t0ken") && !text.contains("s3cret") && !text.contains("k3y"), "{text}");
        }
        assert_eq!(read["rules"]["chat"]["secret"], "(set)");
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

    #[test]
    fn autosave_minutes_are_clamped() {
        let Some(mut game) = game::test_game(game::tests::save()) else { return };
        let mut autosave = 5;
        call(&mut game, "POST", "/api/settings", json!({"autosaveMinutes": 160_000_000_000_000_000u64}), &mut autosave);
        assert_eq!(autosave, crate::MAX_AUTOSAVE);
        // Not a whole number: refused, and the interval stays.
        for bad in [json!(2.5), json!(-1), json!("5"), json!(null)] {
            let (status, answer) = call(&mut game, "POST", "/api/settings", json!({"autosaveMinutes": bad}), &mut autosave);
            assert!(status == 400 && answer["error"].as_str().unwrap().contains("whole number"), "{bad}: {status} {answer}");
        }
        assert_eq!(autosave, crate::MAX_AUTOSAVE);
    }

    #[test]
    fn unbanning_what_is_not_banned_is_said() {
        let Some(mut game) = game::test_game(game::tests::save()) else { return };
        let mut autosave = 5;
        game.access.ban("Mallory", None, Some("10.9.9.9")).unwrap();
        let (status, answer) = call(&mut game, "POST", "/api/unban", json!({"value": "Mellory"}), &mut autosave);
        assert!(status == 400 && answer["error"].as_str().unwrap().contains("Mellory is not banned"), "{status} {answer}");
        assert_eq!(game.access.lists.banned_names, ["Mallory"]);
        assert_eq!(call(&mut game, "POST", "/api/unban", json!({"value": " mallory "}), &mut autosave).0, 200);
        assert_eq!(call(&mut game, "POST", "/api/unban", json!({"value": "10.9.9.9"}), &mut autosave).0, 200);
        assert!(game.access.lists.banned_names.is_empty() && game.access.lists.banned_ips.is_empty());
    }

    #[test]
    fn the_config_endpoint_round_trips() {
        let Some(mut game) = game::test_game(game::tests::save()) else { return };
        let mut autosave = 5;
        let (status, config) = call(&mut game, "GET", "/api/config", json!({}), &mut autosave);
        assert_eq!(status, 200);
        let knobs = config["knobs"].as_array().unwrap();
        let knob = |key: &str| knobs.iter().find(|k| k["key"] == key).unwrap_or_else(|| panic!("{key}")).clone();
        assert_eq!((knob("speed.growth")["type"].as_str(), knob("speed.growth")["default"].as_f64(), knob("speed.growth")["live"].as_bool()), (Some("float"), Some(1.0), Some(true)));
        assert_eq!(knob("PCDS_ADMIN_TOKEN")["live"], false);
        assert!(knob("PCDS_LISTEN")["readOnly"].is_string());
        assert_eq!((knob("PCDS_AUTO_UPDATE")["default"].as_str(), knob("PCDS_UPDATE_CHECK_HOURS")["default"].as_i64()), (Some("off"), Some(24)));
        // Every value the page can change, posted back as read: accepted, nothing changes.
        let writable: serde_json::Map<String, Value> = knobs.iter().filter(|k| k["readOnly"].is_null()).map(|k| (k["key"].as_str().unwrap().to_string(), k["value"].clone())).collect();
        assert!(writable.contains_key("access.bannedNames") && writable.contains_key("server.autosaveMinutes"));
        assert_eq!(call(&mut game, "POST", "/api/config", json!(writable), &mut autosave).1, config);
        // A change of each kind of setting, read back.
        let change = json!({"speed.growth": 2.0, "death.consequences": "DropAllItems", "limits.buildings": {"Drill1": 3}, "access.whitelist": ["Ann"], "server.autosaveMinutes": 10});
        let (status, after) = call(&mut game, "POST", "/api/config", change.clone(), &mut autosave);
        assert_eq!(status, 200, "{after}");
        for (key, value) in change.as_object().unwrap() {
            let got = after["knobs"].as_array().unwrap().iter().find(|k| k["key"] == key.as_str()).unwrap()["value"].clone();
            assert_eq!(&got, value, "{key}");
        }
        assert_eq!((game.rules.speed.growth, game.access.lists.whitelist.clone(), autosave), (2.0, vec!["Ann".to_string()], 10));
        assert_eq!(call(&mut game, "GET", "/api/config", json!({}), &mut autosave).1, after);
        // A map is replaced, not merged; null puts back the default.
        call(&mut game, "POST", "/api/config", json!({"limits.buildings": {"Vegetube1": 1}}), &mut autosave);
        assert_eq!(game.rules.limits.buildings.keys().collect::<Vec<_>>(), ["Vegetube1"]);
        let resets: serde_json::Map<String, Value> = writable.keys().map(|k| (k.clone(), Value::Null)).collect();
        assert_eq!(call(&mut game, "POST", "/api/config", json!(resets), &mut autosave).0, 200);
        assert_eq!((game.rules.clone(), game.access.lists.whitelist.len(), autosave), (rules::Rules::default(), 0, crate::autosave_start()));
        // Refused, with the setting named, and nothing of the change applied.
        for (bad, key) in [
            (json!({"speed.growth": 3, "gauges.foodDrain": -1}), "gauges.foodDrain"),
            (json!({"speed.growth": 3, "limits.buildings": {"Iron": 1}}), "limits.buildings"),
            (json!({"speed.growth": 3, "PCDS_LISTEN": "0.0.0.0:1"}), "PCDS_LISTEN"),
            (json!({"speed.growth": 3, "speed.grwoth": 1}), "speed.grwoth"),
            (json!({"speed.growth": 3, "access.whitelist": "Ann"}), "access.whitelist"),
            (json!({"speed.growth": 3, "server.autosaveMinutes": 100000}), "server.autosaveMinutes"),
            (json!({"access.whitelist": ["Bob"], "limits.buildings": {"Iron": 1}}), "limits.buildings"),
        ] {
            let (status, answer) = call(&mut game, "POST", "/api/config", bad.clone(), &mut autosave);
            assert_eq!((status, answer["key"].as_str()), (400, Some(key)), "{bad}: {answer}");
            assert_eq!(game.rules.speed.growth, 1.0, "{bad}");
            assert!(game.access.lists.whitelist.is_empty(), "{bad}: the access lists are kept as they were");
        }
    }

    #[test]
    fn players_list_teleport_and_chat_through_the_api() {
        let Some(mut game) = game::test_game(game::tests::save()) else { return };
        let mut autosave = 5;
        let object = game::tests::join(&mut game, 1, "Ann");
        game.net.objects.get_mut(&object).unwrap().position = [560.0, 3.0, 500.0];
        game.connect(2, None, false, Some("10.0.0.2".into()));
        let (status, s) = call(&mut game, "GET", "/api/status", json!({}), &mut autosave);
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
        assert_eq!(call(&mut game, "POST", "/api/teleport", json!({"player": "ann", "position": [10.0, 20.5, -30.0]}), &mut autosave).0, 200);
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
        assert_eq!(call(&mut game, "POST", "/api/teleport", json!({"id": 1, "position": [1, 2, 3]}), &mut autosave).0, 200);
        for bad in [json!({"id": 2, "position": [0, 0, 0]}), json!({"player": "Bob", "position": [0, 0, 0]}), json!({"player": "Ann"}), json!({"player": "Ann", "position": [0, 0]}), json!({"player": "Ann", "position": [0, 0, 0], "x": 1})] {
            assert_eq!(call(&mut game, "POST", "/api/teleport", bad.clone(), &mut autosave).0, 400, "{bad}");
        }
        // The map: this planet's buildings.
        let (status, map) = call(&mut game, "GET", "/api/map", json!({}), &mut autosave);
        assert_eq!((status, map["planet"].as_str(), map["buildings"].is_array()), (200, Some("Prime"), true));
        assert!(map["buildings"].as_array().unwrap().iter().all(|b| b.as_array().is_some_and(|p| p.len() == 3)), "x, y, z");
    }

    /// An id names that client only, never a player whose name is the same digits; a name two
    /// players share is refused.
    #[test]
    fn players_are_named_by_id_or_by_a_name_only_one_has() {
        let Some(mut game) = game::test_game(game::tests::save()) else { return };
        let mut autosave = 5;
        game::tests::join(&mut game, 1, "4");
        game::tests::join(&mut game, 4, "Ann");
        assert_eq!(call(&mut game, "POST", "/api/kick", json!({"id": 4}), &mut autosave).0, 200);
        assert_eq!(game.kicks.iter().map(|(c, _)| *c).collect::<Vec<_>>(), [4], "client 4, not the player named 4");
        game.kicks.clear();
        assert_eq!(call(&mut game, "POST", "/api/kick", json!({"name": "4"}), &mut autosave).0, 200);
        assert_eq!(game.kicks.iter().map(|(c, _)| *c).collect::<Vec<_>>(), [1]);
        // The game refuses a second player of one name; two of them is a rename away.
        game::tests::join(&mut game, 5, "Bob");
        game.clients.get_mut(&5).unwrap().ready.as_mut().unwrap().name = "ann".into();
        let (status, answer) = call(&mut game, "POST", "/api/kick", json!({"name": "ANN"}), &mut autosave);
        assert_eq!(status, 400, "{answer}");
        assert!(answer["error"].as_str().unwrap().contains("client id"));
        assert_eq!(call(&mut game, "POST", "/api/kick", json!({}), &mut autosave).0, 400);
        assert_eq!(call(&mut game, "POST", "/api/ban", json!({"id": 99}), &mut autosave).0, 400, "an id names an online client");
    }

    /// A ban that cannot be written answers 500 (it would be lost at the next start).
    #[test]
    fn a_ban_that_cannot_be_kept_answers_500() {
        let Some(mut game) = game::test_game(game::tests::save()) else { return };
        game.access = access::Access::load(Some("/nonexistent-dir-pcds/access.json".into()));
        let (status, answer) = call(&mut game, "POST", "/api/ban", json!({"name": "Mallory"}), &mut 5);
        assert_eq!(status, 500, "{answer}");
        assert!(game.access.lists.banned_names.is_empty());
    }
}
