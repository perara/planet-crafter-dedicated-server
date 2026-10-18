//! Standalone Planet Crafter server: no game process. Accepts invite-code players over Steam
//! networking (as a Steam game server, no Steam user needed) and direct-IP players over Unity
//! Transport, and runs the game's server side, ported from the game (see game.rs), on the
//! world in the save file, with the game's tables for this version (data/<version>/).
//!
//! Usage: pcds serve DATA_DIR SAVE.json
//!        pcds new-save SAVE.json DATA_DIR [--planet ID] [--start LOCATION] [--seed N]
//!                  [--set KEY=VALUE ...]   (a new world, see newworld.rs)
//!        pcds --version [DATA_DIR]
//!        pcds probe HOST:PORT DATA_DIR SCRIPT.json   (a scripted test player)
//!        pcds load HOST:PORT DATA_DIR [PLAYERS] [SECONDS]   (a load test, see load.rs)
//!        pcds extract-raw RAW.json OUT_DIR   (a new game version's tables, see extract.rs)
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
//!   PCDS_WEB          the web GUI page, default web/index.html next to the binary
//!   PCDS_TRACE        log every incoming message and RPC

mod animals;
#[cfg(test)]
mod bench;
mod access;
mod admin;
mod cli;
mod data;
mod deconstruct;
mod energy;
mod equipment;
mod extract;
mod ext;
mod game;
mod gamedata;
mod gauges;
mod genetics;
mod geometry;
mod handlers;
mod idmap;
mod hooks;
mod instances;
mod inventories;
mod load;
mod logistics;
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
mod utp;
mod water;
mod wire;
mod world;

use std::collections::{HashMap, HashSet};
use std::net::{Ipv4Addr, SocketAddr};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use steamworks::networking_sockets::{ListenSocket, NetConnection};
use steamworks::networking_types::{ListenSocketEvent, SendFlags};
use steamworks::{Server, ServerMode, SteamServersConnected};

use data::NetData;
use game::Game;
use gamedata::GameData;
use net::Target;
use rpcspec::Spec;
use save::SaveFile;
use utp::Utp;

const APP_ID: &str = "1284190";
/// The game's Steam transport splits messages above this size (see GameServerSockets.cs).
const CHUNK: usize = 524_288;
/// DisconnectReasonMessage: shown to a kicked or refused player.
const DISCONNECT_REASON: u32 = 8;
/// The most a client's "Split" message may add up to (two of the game's chunks).
const MAX_SPLIT: usize = 1 << 20;
/// Autosave interval bounds, in minutes (larger ones overflow the clock).
const MAX_AUTOSAVE: u64 = 1440;

enum Link {
    /// Steam networking: the connection carries the verified SteamID.
    Steam { steam_id: u64, connection: NetConnection, split: Option<(Vec<u8>, usize)> },
    Ip(utp::Token, SocketAddr),
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
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
pub struct Scenes {
    pub wrecks: Rc<instances::generate::Wrecks>,
    /// Free planet id -> its scene dump. DLC planets have none: the extractor dumps only the
    /// free ones, and nobody can travel to the others (planets.rs).
    pub planets: HashMap<String, PlanetScene>,
}

/// The parts of scenes/<planet>.json the server reads.
pub struct PlanetScene {
    pub water_volumes: Vec<serde_json::Value>,
    pub ore_veins: Vec<serde_json::Value>,
    pub instance_scenes: HashMap<String, Rc<instances::generate::InstanceScene>>,
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
            let names = data.asset(&format!("PlanetData:PlanetData-{}", planet.id)).and_then(|p| data.asset(p["instanceScenesData"].as_str()?)).and_then(|d| d["instanceSceneNames"].as_array());
            if let Some(missing) = names.into_iter().flatten().filter_map(|n| n.as_str()).find(|n| !dump.instance_scenes.contains_key(*n)) {
                return Err(format!("{path}: no world instance scene {missing}"));
            }
            let instance_scenes = dump.instance_scenes.into_iter().map(|(k, v)| (k, Rc::new(v))).collect();
            planets.insert(planet.id.clone(), PlanetScene { water_volumes: dump.water_volumes, ore_veins: dump.ore_veins, instance_scenes });
        }
        Ok(Scenes { wrecks: Rc::new(wrecks), planets })
    };
    load().unwrap_or_else(|e| no_tables(dir, e))
}

/// The tables tests run on: PCDS_TEST_DATA, else data/2.103. Each user extracts them from their
/// own game, so they may be missing: then None, and the test skips.
#[cfg(test)]
fn test_data() -> Option<&'static str> {
    static DIR: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    let dir = DIR.get_or_init(|| {
        // PCDS_TEST_DATA, else the 2.103 tables `pcds extract` wrote into run/data.
        let dirs = [env("PCDS_TEST_DATA"), Some(concat!(env!("CARGO_MANIFEST_DIR"), "/../run/data/2.103").into()), Some(concat!(env!("CARGO_MANIFEST_DIR"), "/data/2.103").into())];
        dirs.into_iter().flatten().find(|d| std::path::Path::new(d).is_dir())
    });
    if dir.is_none() {
        eprintln!("skipped: no game tables");
    }
    dir.as_deref()
}

/// Logs in as a Steam game server (anonymously, or with a login token for a fixed SteamID)
/// and opens the P2P socket invite codes point to.
fn start_steam(version: &str, session_number: u32) -> Result<(Server, ListenSocket, String), String> {
    // SAFETY: set before any other thread exists; the Steam API reads it during init.
    unsafe { std::env::set_var("SteamAppId", APP_ID) };
    let (steam, client) = Server::init(Ipv4Addr::UNSPECIFIED, 0, 0xFFFF, ServerMode::NoAuthentication, version)
        .map_err(|e| format!("Steam game server init failed ({e}); is steamclient.so in ~/.steam/sdk64?"))?;
    steam.set_product(APP_ID);
    steam.set_mod_dir("Planet Crafter");
    steam.set_game_description("Planet Crafter standalone server");
    steam.set_dedicated_server(true);
    let connected = Arc::new(AtomicBool::new(false));
    let logon = connected.clone();
    let _logon = steam.register_callback(move |_: SteamServersConnected| logon.store(true, Ordering::SeqCst));
    match env("PCDS_STEAM_TOKEN") {
        Some(token) => steam.log_on(&token),
        None => steam.log_on_anonymous(),
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    while !connected.load(Ordering::SeqCst) {
        if Instant::now() > deadline {
            return Err("Steam game server logon timed out".into());
        }
        steam.run_callbacks();
        std::thread::sleep(Duration::from_millis(50));
    }
    drop(client);
    let listen = steam.networking_sockets().create_listen_socket_p2p(0, None).map_err(|_| "P2P listen socket failed")?;
    let steam_id = steam.steam_id().raw();
    let code = format!("{}{:04}", protocol::invite_code(steam_id), session_number);
    println!("steam: game server {steam_id}, invite code {code}");
    Ok((steam, listen, code))
}

/// The pause between frames, as the game's: timers, the game tick and Steam run once per frame.
const FRAME: Duration = Duration::from_millis(5);
/// Other players' movement goes out at most this often (a 60 fps host's rate; clients
/// interpolate movement), everything else at once.
const MOVEMENT_EVERY: Duration = Duration::from_millis(16);

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("serve") => match &args[2..] {
            [dir, save] => serve(dir.clone(), save.clone()),
            _ => {
                eprintln!("usage: pcds serve DATA_DIR SAVE.json (settings: see the source header)");
                std::process::exit(2);
            }
        },
        Some("probe") => probe::run(&args[2..]),
        Some("load") => load::run(&args[2..]),
        Some("extract-raw") => extract::run(&args[2..]),
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

/// The server on the world in SAVE_PATH, with the tables in DIR, until it is stopped.
fn serve(dir: String, save_path: String) {
    let (net_data, data, spec) = tables(&dir);
    let scenes = scene_tables(&dir, &net_data, &data);
    let save = SaveFile::load(&save_path).unwrap_or_else(|e| {
        match e.kind() {
            std::io::ErrorKind::NotFound => eprintln!("no save {save_path}: make a new world with `pcds new-save {save_path} {dir}`"),
            _ => eprintln!("{save_path}: {e}"),
        }
        std::process::exit(1)
    });
    if let Err(e) = game::check_save_format(&save.settings) {
        eprintln!("{save_path}: {e}");
        std::process::exit(1);
    }
    // The save as it was before this server touched it (as every backup: none with PCDS_BACKUPS=0).
    // At most one per 10 minutes: a crash loop must not push the hourly backups out.
    if let Err(e) = save::backup(&save_path, Duration::from_secs(600), backups_kept()) {
        eprintln!("backup failed: {e}");
    }
    let session_number: u32 = env("PCDS_SESSION").map(|s| s.parse().expect("PCDS_SESSION: a number")).unwrap_or(4242);
    let version = net_data.game_version.clone();
    let beside_save = |name: &str| std::path::Path::new(&save_path).with_file_name(name).to_string_lossy().to_string();
    // A rules file that does not hold up stops the start, before the world loads.
    let rules_path = env("PCDS_RULES").unwrap_or_else(|| beside_save("rules.json"));
    let loaded_rules = rules::Rules::load(&rules_path).and_then(|r| r.validate(&data).map(|_| r)).unwrap_or_else(|e| {
        eprintln!("rules: {e}");
        std::process::exit(1)
    });
    let mut game = Game::new(net_data, data, spec, save, scenes);
    rules::start(&mut game, loaded_rules, rules_path);
    game.access = access::Access::load(Some(env("PCDS_ACCESS").unwrap_or_else(|| beside_save("access.json"))));
    let mut autosave_minutes: u64 = env("PCDS_AUTOSAVE").and_then(|s| s.parse().ok()).unwrap_or(5).min(MAX_AUTOSAVE);
    game.session_number = session_number;
    println!("world: {} world objects, {} network objects, planet {}, game {version}", game.wos.len(), game.net.objects.len(), game.planet_id());

    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    ctrlc::set_handler(move || flag.store(true, Ordering::SeqCst)).expect("signal handler");

    let listen = env("PCDS_LISTEN").unwrap_or_else(|| "0.0.0.0:7777".into());
    let mut utp = match listen.as_str() {
        "off" => None,
        addr => {
            let addr: SocketAddr = addr.parse().expect("PCDS_LISTEN: host:port");
            let utp = Utp::bind(addr).unwrap_or_else(|e| panic!("UDP {addr}: {e}"));
            println!("direct IP: listening on {addr}");
            Some(utp)
        }
    };
    let mut invite_code: Option<String> = None;
    let steam = match env("PCDS_STEAM").as_deref() {
        Some("off") => None,
        _ => match start_steam(&version, session_number) {
            Ok((steam, listen, code)) => {
                invite_code = Some(code);
                Some((steam, listen))
            }
            Err(e) if utp.is_some() => {
                eprintln!("steam: {e}; continuing with direct IP only");
                None
            }
            Err(e) => panic!("{e}"),
        },
    };

    let admin = match (env("PCDS_ADMIN").unwrap_or_else(|| "127.0.0.1:7782".into()), env("PCDS_ADMIN_TOKEN")) {
        (addr, Some(token)) if addr != "off" => {
            let web = env("PCDS_WEB").or_else(|| std::env::current_exe().ok().map(|e| e.with_file_name("web").join("index.html").to_string_lossy().to_string()));
            match admin::start(&addr, token, web) {
                Ok(rx) => {
                    println!("admin: http://{addr}/");
                    Some(rx)
                }
                Err(e) => {
                    eprintln!("admin: {addr}: {e}");
                    None
                }
            }
        }
        _ => None,
    };
    let network = match (steam.is_some(), utp.is_some()) {
        (true, true) => "both",
        (true, false) => "steam",
        _ => "ip",
    };
    let port = listen.rsplit(':').next().and_then(|p| p.parse::<u16>().ok()).unwrap_or(0);
    let started = Instant::now();
    let mut loops = (0u32, Instant::now(), 0u32);
    // Time spent working (not waiting) in the current second, its longest loop; and the last
    // second's, for the status.
    let (mut busy, mut busy_last) = ((Duration::ZERO, Duration::ZERO), (Duration::ZERO, Duration::ZERO));

    let mut links: HashMap<u64, Link> = HashMap::new();
    // The client behind each direct-IP connection, from its Connected event to its end.
    let mut ip_clients: HashMap<utp::Token, u64> = HashMap::new();
    let mut approved: HashSet<u64> = HashSet::new();
    // Each client's batch, and the clients whose own packets brought them something this loop.
    let (mut mail, mut urgent): (idmap::FxHashMap<u64, wire::Batch>, Vec<u64>) = Default::default();
    let mut pending_drop: Vec<(u64, Instant)> = Vec::new();
    let mut next_client = 1u64;
    let mut next_save = Instant::now() + Duration::from_secs(autosave_minutes.max(1) * 60);
    let (mut next_frame, mut movement_sent) = (Instant::now(), Instant::now());
    while !stop.load(Ordering::SeqCst) {
        // A frame every 5 ms: the game tick and Steam. A direct-IP packet in between is handled
        // at once, and what it brings goes out at once.
        let woke = Instant::now();
        let frame = woke >= next_frame;
        if frame {
            next_frame = Instant::now() + FRAME;
            loops.0 += 1;
        }
        if loops.1.elapsed() >= Duration::from_secs(1) {
            loops = (0, Instant::now(), loops.0);
            busy_last = std::mem::take(&mut busy);
        }
        // Client ids are given as connections appear, so a connection that also sends or ends
        // within this loop is found.
        let mut joined: Vec<(u64, Link)> = Vec::new();
        let mut incoming: Vec<(u64, Vec<u8>)> = Vec::new();
        let mut gone: Vec<u64> = Vec::new();
        if let Some((steam, listen)) = steam.as_ref().filter(|_| frame) {
            steam.run_callbacks();
            while let Some(event) = listen.try_receive_event() {
                match event {
                    ListenSocketEvent::Connecting(request) => {
                        let _ = request.accept();
                    }
                    ListenSocketEvent::Connected(event) => {
                        let steam_id = event.remote().steam_id().map(|id| id.raw()).unwrap_or(0);
                        joined.push((next_client, Link::Steam { steam_id, connection: event.take_connection(), split: None }));
                        next_client += 1;
                    }
                    ListenSocketEvent::Disconnected(event) => {
                        let peer = event.remote().steam_id().map(|id| id.raw()).unwrap_or(0);
                        println!("{peer}: disconnected ({:?})", event.end_reason());
                        let all = links.iter().map(|(c, l)| (*c, l)).chain(joined.iter().map(|(c, l)| (*c, l)));
                        gone.extend(all.filter(|(_, l)| matches!(l, Link::Steam { steam_id, .. } if *steam_id == peer)).map(|(c, _)| c));
                    }
                }
            }
            for (client, link) in links.iter_mut() {
                if let Link::Steam { connection, split, .. } = link {
                    for message in connection.receive_messages(64).unwrap_or_default() {
                        match reassemble(split, message.data()) {
                            Ok(batch) => incoming.extend(batch.map(|batch| (*client, batch))),
                            Err(e) => {
                                println!("client {client}: dropped ({e})");
                                gone.push(*client);
                                break;
                            }
                        }
                    }
                }
            }
        }
        if let Some(utp) = &mut utp {
            for event in utp.poll() {
                match event {
                    utp::Event::Connected(token, addr) => {
                        ip_clients.insert(token, next_client);
                        joined.push((next_client, Link::Ip(token, addr)));
                        next_client += 1;
                    }
                    utp::Event::Data(token, batch) => incoming.extend(ip_clients.get(&token).map(|c| (*c, batch))),
                    utp::Event::Disconnected(token) => gone.extend(ip_clients.remove(&token)),
                }
            }
        }
        for (client, link) in joined {
            let (verified, trusted, ip) = match &link {
                Link::Steam { steam_id, .. } => {
                    println!("client {client}: connected over Steam as {steam_id}");
                    (Some(*steam_id), false, None)
                }
                Link::Ip(_, addr) => {
                    println!("client {client}: connected from {addr}");
                    // The SteamID a direct-IP client reports is only believed from this machine.
                    (None, addr.ip().is_loopback(), Some(addr.ip().to_string()))
                }
            };
            game.connect(client, verified, trusted, ip);
            links.insert(client, link);
        }
        for client in gone {
            if links.remove(&client).is_some() {
                println!("client {client}: left");
                approved.remove(&client);
                guarded(&mut game, client, &save_path, |game| game.disconnect(client));
            }
        }
        for (client, batch) in incoming {
            // Gone or kicked: nothing more from it reaches the game (the reason still goes out).
            let kicked = pending_drop.iter().any(|(c, _)| *c == client) || game.kicks.iter().any(|(c, _)| *c == client);
            if links.contains_key(&client) && !kicked {
                let before = game.net.out.len();
                guarded(&mut game, client, &save_path, |game| game.receive(client, &batch));
                if game.net.out[before.min(game.net.out.len())..].iter().any(|(to, _, _)| to.reaches(client)) && !urgent.contains(&client) {
                    urgent.push(client);
                }
            }
        }
        if frame && std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| game.tick())).is_err() {
            // The whole world may be half-updated: keep what there is and let systemd restart.
            eprintln!("the game tick panicked: saving and stopping");
            persist(&mut game, &save_path);
            std::process::exit(1);
        }
        if std::mem::take(&mut game.save_requested) {
            persist(&mut game, &save_path);
        }
        // What a client's own packets brought it goes out now; the rest (other players'
        // movement and doings, the world's changes) every MOVEMENT_EVERY.
        post(&mut game, &mut approved, &mut mail);
        let everyone = frame && movement_sent.elapsed() >= MOVEMENT_EVERY;
        let send_now = everyone || !urgent.is_empty();
        if everyone {
            movement_sent = Instant::now();
            let all: Vec<u64> = mail.keys().copied().collect();
            send_mail(&mut mail, &all, &links, utp.as_mut());
        } else if !urgent.is_empty() {
            send_mail(&mut mail, &urgent, &links, utp.as_mut());
        }
        urgent.clear();
        // Kicked or refused: the reason first, the disconnect a moment later.
        for (client, reason) in std::mem::take(&mut game.kicks) {
            if pending_drop.iter().any(|(c, _)| *c == client) {
                continue;
            }
            println!("client {client}: kicked ({reason})");
            // What it was sent before goes first.
            send_mail(&mut mail, &[client], &links, utp.as_mut());
            let units: Vec<u16> = reason.encode_utf16().collect();
            let mut w = wire::Writer::new();
            w.packed_i32(0).u32(units.len() as u32);
            units.iter().for_each(|u| {
                w.u16(*u);
            });
            let batch = wire::batch(&[(DISCONNECT_REASON, w.0)]);
            match links.get(&client) {
                Some(Link::Steam { connection, .. }) => send(connection, &batch),
                Some(Link::Ip(token, _)) => {
                    if let Some(utp) = utp.as_mut() {
                        utp.send(token, &batch);
                    }
                }
                None => {}
            }
            pending_drop.push((client, Instant::now() + Duration::from_millis(300)));
        }
        let now = Instant::now();
        for (client, _) in pending_drop.iter().filter(|(_, due)| *due <= now) {
            if let Some(link) = links.remove(client) {
                match link {
                    Link::Steam { connection, .. } => drop(connection),
                    Link::Ip(token, _) => {
                        ip_clients.remove(&token);
                        if let Some(utp) = utp.as_mut() {
                            utp.disconnect(&token);
                        }
                    }
                }
                approved.remove(client);
                guarded(&mut game, *client, &save_path, |game| game.disconnect(*client));
            }
        }
        pending_drop.retain(|(_, due)| *due > now);
        if let Some(rx) = &admin {
            while let Ok(request) = rx.try_recv() {
                let ctx = Status { invite_code: invite_code.as_deref(), network, port, started, autosave_minutes, loop_rate: loops.2, busy: busy_last, save_path: &save_path };
                let (status, body) = admin_request(&mut game, &request, &ctx, &stop, &mut autosave_minutes, &links);
                let _ = request.reply.send((status, body));
                if request.path == "/api/settings" {
                    next_save = Instant::now() + Duration::from_secs(autosave_minutes.max(1) * 60);
                }
            }
        }
        if let Some(utp) = utp.as_mut().filter(|_| frame || send_now || !pending_drop.is_empty()) {
            utp.flush();
        }
        if autosave_minutes > 0 && Instant::now() >= next_save {
            next_save += Duration::from_secs(autosave_minutes * 60);
            persist(&mut game, &save_path);
        }
        let work = woke.elapsed();
        busy = (busy.0 + work, busy.1.max(work));
        let pause = next_frame.saturating_duration_since(Instant::now());
        match &utp {
            Some(utp) => utp.wait(pause),
            None => std::thread::sleep(pause),
        }
    }
    persist(&mut game, &save_path);
    if let Some(utp) = &mut utp {
        utp.tokens().iter().for_each(|t| utp.disconnect(t));
    }
}

struct Status<'a> {
    invite_code: Option<&'a str>,
    network: &'a str,
    port: u16,
    started: Instant,
    autosave_minutes: u64,
    loop_rate: u32,
    /// The last second's work time and longest loop.
    busy: (Duration, Duration),
    save_path: &'a str,
}

/// A client named by the API: by name (any case) or by client id.
fn find_client(game: &Game, player: &str) -> Option<u64> {
    game.clients.values().find(|c| c.ready.as_ref().is_some_and(|s| s.name.eq_ignore_ascii_case(player)) || c.id.to_string() == player).map(|c| c.id)
}

/// Kicks everyone the access lists no longer let in.
fn enforce(game: &mut Game) {
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

fn admin_request(game: &mut Game, r: &admin::Request, ctx: &Status, stop: &AtomicBool, autosave: &mut u64, links: &HashMap<u64, Link>) -> (u16, String) {
    let ok = || (200, "{\"ok\":true}".to_string());
    let field = |k: &str| r.body[k].as_str().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
    match (r.method.as_str(), r.path.as_str()) {
        ("GET", "/api/status") => {
            let players: Vec<serde_json::Value> = game
                .clients
                .values()
                .map(|c| {
                    let ip = match links.get(&c.id) {
                        Some(Link::Steam { .. }) => "steam".to_string(),
                        _ => c.ip.clone().unwrap_or_default(),
                    };
                    serde_json::json!({
                        "id": c.id,
                        "name": c.ready.as_ref().map(|s| s.name.clone()),
                        "steamId": c.verified_steam_id.map(|i| i.to_string()),
                        "verified": c.verified_steam_id.is_some(),
                        "ip": ip,
                        "connectedSeconds": c.since.elapsed().as_secs(),
                    })
                })
                .collect();
            let terraformation = units::value(game, units::Kind::Terraformation, 0);
            let planet_units: serde_json::Map<String, serde_json::Value> = game.units.planet(game.planet_hash()).map(|p| p.units.iter().map(|u| (u.kind.unit_type().to_string(), serde_json::json!({"value": u.total, "perSecond": u.per_sec}))).collect()).unwrap_or_default();
            let body = serde_json::json!({
                "state": "running",
                "standalone": true,
                "gameVersion": game.net_data.game_version,
                "save": std::path::Path::new(ctx.save_path).file_stem().map(|s| s.to_string_lossy().to_string()),
                "network": ctx.network,
                "inviteCode": ctx.invite_code,
                "steam": ctx.invite_code.is_some(),
                "port": ctx.port,
                "uptimeSeconds": ctx.started.elapsed().as_secs(),
                "autosaveMinutes": ctx.autosave_minutes,
                "tickRate": game.net_data.tick_rate,
                "rules": rules::changes(&game.rules),
                "fps": ctx.loop_rate,
                "busyPercent": (ctx.busy.0.as_secs_f64() * 1000.0).round() / 10.0,
                "longestLoopMicros": ctx.busy.1.as_micros() as u64,
                "planet": game.planet_id(),
                "terraformation": terraformation,
                "units": planet_units,
                "players": players,
                "access": game.access.lists,
            });
            (200, body.to_string())
        }
        ("POST", "/api/save") => {
            persist(game, ctx.save_path);
            ok()
        }
        ("POST", "/api/stop") => {
            stop.store(true, Ordering::SeqCst);
            ok()
        }
        ("POST", "/api/kick") => match field("player").and_then(|p| find_client(game, &p)) {
            Some(c) => {
                game.kick(c, "Kicked");
                ok()
            }
            None => (400, "{\"error\":\"player not online\"}".into()),
        },
        ("POST", "/api/ban") => {
            let Some(player) = field("player") else { return (400, "{\"error\":\"'player' is required\"}".into()) };
            match find_client(game, &player) {
                Some(c) => {
                    let client = &game.clients[&c];
                    let name = client.ready.as_ref().map(|s| s.name.clone()).unwrap_or(player);
                    let (steam_id, ip) = (client.verified_steam_id, client.ip.clone());
                    game.access.ban(&name, steam_id, ip.as_deref());
                    game.kick(c, "Banned");
                }
                None => game.access.ban(&player, None, None),
            }
            ok()
        }
        ("POST", "/api/unban") => match field("value") {
            Some(v) => {
                game.access.unban(&v);
                ok()
            }
            None => (400, "{\"error\":\"'value' is required\"}".into()),
        },
        ("POST", "/api/access") => {
            let list = |k: &str, current: &Vec<String>| r.body[k].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect()).unwrap_or_else(|| current.clone());
            let l = &game.access.lists;
            let lists = access::Lists { allowed_ips: list("allowedIPs", &l.allowed_ips), whitelist: list("whitelist", &l.whitelist), banned_names: list("bannedNames", &l.banned_names), banned_ips: list("bannedIPs", &l.banned_ips) };
            game.access.set(lists);
            enforce(game);
            ok()
        }
        ("GET", "/api/rules") => (200, serde_json::json!({"rules": game.rules, "defaults": rules::Rules::default()}).to_string()),
        // A JSON merge patch of the rules: {"speed": {"growth": 2}}; null resets a rule.
        ("POST", "/api/rules") => match game.rules.patched(&r.body).and_then(|new| rules::set(game, new)) {
            Ok(()) => (200, serde_json::json!({"rules": game.rules}).to_string()),
            Err(e) => (400, serde_json::json!({"error": e}).to_string()),
        },
        ("POST", "/api/settings") => {
            if let Some(m) = r.body["autosaveMinutes"].as_u64() {
                *autosave = m.min(MAX_AUTOSAVE);
            }
            ok()
        }
        _ => (404, "{\"error\":\"unknown endpoint\"}".into()),
    }
}

fn backups_kept() -> usize {
    env("PCDS_BACKUPS").and_then(|s| s.parse().ok()).unwrap_or(24)
}

/// One client's share of the game. A panic there (the hook printed it) kicks that client and
/// saves at once instead of ending the server for everyone.
fn guarded(game: &mut Game, client: u64, save_path: &str, work: impl FnOnce(&mut Game)) {
    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| work(game))).is_err() {
        eprintln!("client {client}: the server panicked on it; kicked, saving");
        game.kick(client, "Server error");
        persist(game, save_path);
    }
}

fn persist(game: &mut Game, path: &str) {
    let every: u64 = env("PCDS_BACKUP_MINUTES").and_then(|s| s.parse().ok()).unwrap_or(60);
    if let Err(e) = save::backup(path, Duration::from_secs(every.saturating_mul(60)), backups_kept()) {
        eprintln!("backup failed: {e}");
    }
    match game.snapshot().write(path) {
        Ok(()) => println!("saved {path}"),
        Err(e) => eprintln!("save failed: {e}"),
    }
}

/// One batch per client per tick, in queue order. A client gets messages queued after its
/// ConnectionApproved: everything before is in the world it was sent.
/// Sorts the queued messages into each approved client's batch, in queue order.
fn post(game: &mut Game, approved: &mut HashSet<u64>, mail: &mut idmap::FxHashMap<u64, wire::Batch>) {
    for (to, kind, body) in game.net.out.drain(..) {
        if kind == protocol::CONNECTION_APPROVED {
            if let Target::Only(c) = to {
                approved.insert(c);
                mail.entry(c).or_default().push(kind, &body);
            }
            continue;
        }
        for c in approved.iter().filter(|c| to.reaches(**c)) {
            mail.entry(*c).or_default().push(kind, &body);
        }
    }
}

/// Sends these clients their batches.
fn send_mail(mail: &mut idmap::FxHashMap<u64, wire::Batch>, clients: &[u64], links: &HashMap<u64, Link>, mut utp: Option<&mut Utp>) {
    for client in clients {
        let Some(b) = mail.remove(client).filter(|b| !b.is_empty()) else { continue };
        let batch = b.finish();
        match links.get(client) {
            Some(Link::Steam { connection, .. }) => send(connection, &batch),
            Some(Link::Ip(token, _)) => {
                if let Some(utp) = utp.as_deref_mut() {
                    utp.send(token, &batch);
                }
            }
            None => {}
        }
    }
}

/// The game's Steam transport frames batches over 512 KiB as "Split<n>" plus n chunks.
fn send(connection: &NetConnection, batch: &[u8]) {
    if batch.len() <= CHUNK {
        let _ = connection.send_message(batch, SendFlags::RELIABLE);
        return;
    }
    let chunks: Vec<&[u8]> = batch.chunks(CHUNK).collect();
    let _ = connection.send_message(format!("Split{}", chunks.len()).as_bytes(), SendFlags::RELIABLE);
    chunks.iter().for_each(|c| {
        let _ = connection.send_message(c, SendFlags::RELIABLE);
    });
}

/// A client's messages back into batches. Err for a split the game never sends (a count other
/// than 1..=9, or over MAX_SPLIT in all): the caller drops the client.
fn reassemble(split: &mut Option<(Vec<u8>, usize)>, data: &[u8]) -> Result<Option<Vec<u8>>, &'static str> {
    if let Some((buffer, left)) = split {
        if buffer.len() + data.len() > MAX_SPLIT {
            return Err("split message over 1 MiB");
        }
        buffer.extend_from_slice(data);
        *left -= 1;
        if *left == 0 {
            return Ok(split.take().map(|(buffer, _)| buffer));
        }
        return Ok(None);
    }
    if data.len() == 6 && &data[..5] == b"Split" {
        let n = char::from(data[5]).to_digit(10).filter(|n| (1..=9).contains(n)).ok_or("bad split count")?;
        *split = Some((Vec::new(), n as usize));
        return Ok(None);
    }
    Ok(Some(data.to_vec()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steam_splits_are_bounded() {
        let mut split = None;
        assert_eq!(reassemble(&mut split, b"batch"), Ok(Some(b"batch".to_vec())));
        for bad in [b"Split0", b"Split/", b"Split:"] {
            assert!(reassemble(&mut None, bad).is_err());
        }
        assert_eq!(reassemble(&mut split, b"Split2"), Ok(None));
        assert_eq!(reassemble(&mut split, b"ab"), Ok(None));
        assert_eq!(reassemble(&mut split, b"cd"), Ok(Some(b"abcd".to_vec())));
        assert!(split.is_none());
        reassemble(&mut split, b"Split3").unwrap();
        reassemble(&mut split, &vec![0; MAX_SPLIT]).unwrap();
        assert!(reassemble(&mut split, b"x").is_err());
    }

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

    #[test]
    fn rules_change_through_the_api_and_bad_ones_are_refused() {
        let Some(mut game) = game::test_game(game::tests::save()) else { return };
        let post = |game: &mut Game, body: serde_json::Value| {
            let request = admin::Request { method: "POST".into(), path: "/api/rules".into(), body, reply: std::sync::mpsc::channel().0 };
            let ctx = Status { invite_code: None, network: "ip", port: 0, started: Instant::now(), autosave_minutes: 5, loop_rate: 0, busy: Default::default(), save_path: "" };
            admin_request(game, &request, &ctx, &AtomicBool::new(false), &mut 5, &HashMap::new()).0
        };
        assert_eq!(post(&mut game, serde_json::json!({"speed": {"growth": 2}})), 200);
        assert_eq!(game.rules.speed.growth, 2.0);
        assert_eq!(post(&mut game, serde_json::json!({"speed": {"growth": -1}})), 400);
        assert_eq!(post(&mut game, serde_json::json!({"sped": {}})), 400);
        assert_eq!(game.rules.speed.growth, 2.0, "a refused change keeps the rules");
    }

    #[test]
    fn autosave_minutes_are_clamped() {
        let Some(mut game) = game::test_game(game::tests::save()) else { return };
        let request = admin::Request { method: "POST".into(), path: "/api/settings".into(), body: serde_json::json!({"autosaveMinutes": 160_000_000_000_000_000u64}), reply: std::sync::mpsc::channel().0 };
        let ctx = Status { invite_code: None, network: "ip", port: 0, started: Instant::now(), autosave_minutes: 5, loop_rate: 0, busy: Default::default(), save_path: "" };
        let mut autosave = 5;
        admin_request(&mut game, &request, &ctx, &AtomicBool::new(false), &mut autosave, &HashMap::new());
        assert_eq!(autosave, MAX_AUTOSAVE);
        let _ = Instant::now() + Duration::from_secs(autosave * 60);
    }

    /// The startup copy is made even when a spaced backup was just taken.
    #[test]
    fn the_original_save_is_kept() {
        let dir = std::env::temp_dir().join(format!("pcds-original-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("world.json");
        std::fs::write(&path, "original").unwrap();
        save::backup(path.to_str().unwrap(), Duration::from_secs(3600), 24).unwrap();
        std::thread::sleep(Duration::from_millis(1100));
        save::backup(path.to_str().unwrap(), Duration::ZERO, 24).unwrap();
        assert_eq!(std::fs::read_dir(dir.join("backups")).unwrap().count(), 2);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
