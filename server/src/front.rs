//! The front: the server's long-lived process, the bootloader of docs/hot-update.md. It owns
//! what a client notices and what must outlive a game process: the UDP socket and every Unity
//! Transport session (utp.rs), the Steam game server login, its listen socket, every Steam
//! connection and the invite code, the client ids, the delayed close of kicked clients, the
//! admin HTTP listener and web page, the chat hooks (chathooks.rs) and the autosave timer.
//! The world runs in a bank process (bank.rs) started from the active bank's binary; the front
//! passes it each client's batches and the bank's batches to the clients, over a unix socket
//! (ipc.rs), and never waits for it.
//!
//! A flip hands the running world to the other bank: the new process loads its tables and
//! says HELLO; the front checks its protocol, game tables and snapshot formats; the old bank
//! stops at a tick boundary and writes its snapshot into a memfd; the new one restores it and
//! says READY (the health check); the front switches routing and the old bank ends. Until
//! READY the old world stays in memory: a new bank that fails is killed and the old one goes
//! on (RESUME). Clients' frames wait in the front meanwhile; it keeps acking, resending and
//! heartbeating, so players see a pause, not a disconnect. A bank that dies outside a flip is
//! started again from the last snapshot or the save, whichever is newer; its players are
//! told to join again (they have seen newer state than either).
//!
//!   pcds serve DATA_DIR SAVE.json   (pcds run starts it from the active bank)
//!     PCDS_BANKS        the banks' folder (run/bin: a/, b/, active); unset: both banks are
//!                       this program (a checkout build), and there is nothing to update

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::{Ipv4Addr, SocketAddr};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use steamworks::networking_sockets::{ListenSocket, NetConnection};
use steamworks::networking_types::{AppNetConnectionEnd, ListenSocketEvent, NetConnectionEnd, NetworkingConfigEntry, NetworkingConfigValue, SendFlags};
use steamworks::{Server, ServerMode, SteamServersConnected};

use crate::admin;
use crate::chathooks::Hooks;
use crate::dashboard::{BYTES_IN, BYTES_OUT};
use crate::ipc::{self, Frame, Link};
use crate::updater;
use crate::utp::{self, Utp};

const APP_ID: &str = "1284190";
/// The game's Steam transport splits messages above this size (see GameServerSockets.cs).
const CHUNK: usize = 524_288;
/// The game's Steam transport's receive buffer (SteamNetworkingSocketsTransport: 2621440).
const MAX_STEAM_MESSAGE: usize = 5 * CHUNK;
/// Steam's send buffer per connection: several joins' worth.
const SEND_BUFFER: i32 = 32 << 20;
/// The most a client's "Split" message may add up to (two of the game's chunks).
const MAX_SPLIT: usize = 1 << 20;
/// A client's frames waiting for the bank (a stalled or flipping world) past this: the client
/// is dropped, not the world.
const MAX_CLIENT_QUEUE: usize = 16 << 20;
/// At a stop, the live bank gets this long to take STOP and save, then as long again to end,
/// before it is killed: 60 s in all, under the supervisor's 20 s for orphans (cli/setup.rs
/// BANKS_GRACE) and systemd's TimeoutStopSec=90 (and Docker's --stop-timeout 90), so
/// systemd's or Docker's kill comes last.
pub const STOP_GRACE: Duration = Duration::from_secs(30);

/// The front's exit code when no bank could start the world at its boot: `pcds run` then
/// starts the other bank's own front (a release whose front or bank cannot run, cli/setup.rs).
pub const BOOT_FAILED: i32 = 75;

/// A new bank's tables and world load in seconds; the restore must be READY within this.
const LOAD_DEADLINE: Duration = Duration::from_secs(120);
const READY_DEADLINE: Duration = Duration::from_secs(5);
/// Kicked: the reason first, the connection closed this much later.
const KICK_DELAY: Duration = Duration::from_millis(300);
/// More crashes than this within CRASH_WINDOW and the front gives up (systemd restarts it).
const MAX_CRASHES: usize = 5;
const CRASH_WINDOW: Duration = Duration::from_secs(600);
const RESTARTED: &str = "The server restarted after an error: join again";
const STOPPED: &str = "The world is stopped: join again later";

enum Conn {
    /// Steam networking: the connection carries the verified SteamID.
    Steam { connection: NetConnection, split: Option<(Vec<u8>, usize)> },
    Ip(utp::Token),
}

struct Bank {
    /// "a" or "b".
    name: String,
    child: Child,
    link: Link,
    /// Its HELLO (Null until it came).
    hello: Value,
    started: Instant,
}

#[derive(PartialEq, Debug, Clone, Copy)]
enum Phase {
    Idle,
    /// The next bank loads its tables; the live one runs on.
    Starting,
    /// HANDOFF sent: the live bank stops and snapshots; client frames wait.
    Snapshotting,
    /// The next bank has the snapshot.
    Restoring,
    /// The live bank died: the next one starts from the last snapshot or the save.
    Recovering,
    /// The admin stopped the world (POST /api/world/stop): no bank; joins are refused; the
    /// admin page and API stay, to start it or another world again.
    Stopped,
}

/// The flip under way.
struct Flip {
    seq: u64,
    reason: String,
    /// HANDOFF sent: the world stands from here.
    stopped: Option<Instant>,
    snapshot: Value,
    /// Its snapshot: the last one (last_snapshot) once the new bank restored it and said READY.
    kept: Option<(OwnedFd, SystemTime)>,
}

enum UpdateDone {
    Installed { seq: u64, bank: String, info: Value },
    Failed { seq: u64, message: String },
    UpToDate { seq: u64, tag: String },
    /// The automatic check left the latest release out (the one rolled back from).
    Skipped { seq: u64, message: String },
    /// A check only (POST /api/update {"check": true}): the latest release, and whether it is
    /// newer than the live bank's.
    Checked { seq: u64, tag: String, newer: bool },
}

struct Updates {
    tx: Sender<UpdateDone>,
    rx: Receiver<UpdateDone>,
    /// The request being worked on (download or flip).
    busy: Option<(u64, String)>,
    seq: u64,
    history: VecDeque<Value>,
    auto: bool,
    every: Duration,
    next_check: Instant,
    latest: Option<String>,
    /// The latest release is later than the live bank's (as the last check found).
    newer: bool,
    checked: Option<u64>,
}

struct Front {
    dir: String,
    save: String,
    banks: Option<PathBuf>,
    utp: Option<Utp>,
    steam: Option<(Server, ListenSocket)>,
    invite_code: Option<String>,
    port: u16,
    conns: HashMap<u64, Conn>,
    ip_clients: HashMap<utp::Token, u64>,
    next_client: u64,
    pending_drop: Vec<(u64, Instant)>,
    /// Kicked or dropped: nothing more from it reaches the bank.
    kicked: HashSet<u64>,
    live: Option<Bank>,
    next: Option<Bank>,
    phase: Phase,
    flip: Option<Flip>,
    /// Frames for the bank while the world stands (and their bytes per client).
    held: Vec<(u8, u64, Vec<u8>, u64)>,
    held_bytes: HashMap<u64, usize>,
    /// The last snapshot a flip carried: a crashed bank starts from it when it is newer than
    /// the save.
    last_snapshot: Option<(OwnedFd, SystemTime)>,
    /// What a recovering bank starts from: the snapshot (else the save).
    recover_from: Option<OwnedFd>,
    crashes: VecDeque<Instant>,
    exiting: Vec<(Child, Instant)>,
    admin: Option<Receiver<admin::Request>>,
    answers: HashMap<u64, (Sender<(u16, String)>, String)>,
    next_request: u64,
    hooks: Hooks,
    web: admin::Web,
    autosave_minutes: u64,
    next_save: Instant,
    updates: Updates,
    started: Instant,
    stop: Arc<AtomicBool>,
    game_version: String,
    flips: u64,
    /// The lock that makes this the one server of its world (released by a switch).
    world_lock: Option<std::fs::File>,
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn ms(d: Duration) -> f64 {
    (d.as_secs_f64() * 10_000.0).round() / 10.0
}

pub fn run(args: &[String]) {
    let [dir, save] = args else {
        eprintln!("usage: pcds serve DATA_DIR SAVE.json (settings: see the source header of main.rs)");
        std::process::exit(2);
    };
    // Settings that are not what they should be stop the start, before anything is opened.
    if let Err(e) = crate::config::check_env(crate::env) {
        eprintln!("{e}");
        std::process::exit(2);
    }
    // One server per world, for this front's life (released when the process ends).
    let world = crate::cli::hold(std::path::Path::new(&format!("{save}.lock")), "another server runs this world").unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(1)
    });
    // SAFETY: set before any other thread exists; the Steam API reads it during init.
    unsafe { std::env::set_var("SteamAppId", APP_ID) };
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    ctrlc::set_handler(move || flag.store(true, Ordering::SeqCst)).expect("signal handler");

    let listen = crate::env("PCDS_LISTEN").unwrap_or_else(|| "0.0.0.0:7777".into());
    let utp = match listen.as_str() {
        "off" => None,
        addr => {
            let addr: SocketAddr = addr.parse().expect("PCDS_LISTEN: host:port");
            let utp = Utp::bind(addr).unwrap_or_else(|e| panic!("UDP {addr}: {e}"));
            println!("direct IP: listening on {addr}");
            Some(utp)
        }
    };
    let port = listen.rsplit(':').next().and_then(|p| p.parse::<u16>().ok()).unwrap_or(0);
    let mut front = Front::new(dir, save, crate::env("PCDS_BANKS").map(PathBuf::from), utp, port, stop);
    front.world_lock = Some(world);
    front.boot();
    front.start_admin();
    front.start_steam();
    front.serve();
}

impl Front {
    fn new(dir: &str, save: &str, banks: Option<PathBuf>, utp: Option<Utp>, port: u16, stop: Arc<AtomicBool>) -> Front {
        let (tx, rx) = std::sync::mpsc::channel();
        let auto = crate::env("PCDS_AUTO_UPDATE").as_deref() == Some("on");
        let every = Duration::from_secs(crate::config::env_number("PCDS_UPDATE_CHECK_HOURS") * 3600);
        let autosave_minutes = crate::autosave_start();
        Front {
            dir: dir.to_string(),
            save: save.to_string(),
            banks,
            utp,
            steam: None,
            invite_code: None,
            port,
            conns: HashMap::new(),
            ip_clients: HashMap::new(),
            next_client: 1,
            pending_drop: Vec::new(),
            kicked: HashSet::new(),
            live: None,
            next: None,
            phase: Phase::Idle,
            flip: None,
            held: Vec::new(),
            held_bytes: HashMap::new(),
            last_snapshot: None,
            recover_from: None,
            crashes: VecDeque::new(),
            exiting: Vec::new(),
            admin: None,
            answers: HashMap::new(),
            next_request: 1,
            hooks: Hooks::default(),
            web: Default::default(),
            autosave_minutes,
            next_save: Instant::now() + Duration::from_secs(autosave_minutes.max(1) * 60),
            // The first check a while after the start: a crash loop must not ask GitHub each time.
            updates: Updates { tx, rx, busy: None, seq: 0, history: VecDeque::new(), auto, every, next_check: Instant::now() + every.min(Duration::from_secs(600)), latest: None, newer: false, checked: None },
            started: Instant::now(),
            stop,
            game_version: String::new(),
            flips: 0,
            world_lock: None,
        }
    }

    /// The program of a bank: its folder's pcds, or this program without banks.
    fn exe(&self, bank: &str) -> PathBuf {
        match &self.banks {
            Some(dir) => dir.join(bank).join("pcds"),
            None => std::env::current_exe().expect("this program"),
        }
    }

    fn web_page(&self, bank: &str) -> Option<String> {
        if let Some(web) = crate::env("PCDS_WEB") {
            return Some(web);
        }
        let dir = self.exe(bank);
        Some(dir.with_file_name("web").join("index.html").to_string_lossy().to_string())
    }

    /// A bank process on a socket of its own (its fd 3).
    fn spawn(&self, bank: &str, fail: Option<&str>) -> Result<Bank, String> {
        let (ours, theirs) = UnixStream::pair().map_err(|e| format!("socketpair: {e}"))?;
        ours.set_nonblocking(true).map_err(|e| e.to_string())?;
        let exe = self.exe(bank);
        let mut command = Command::new(&exe);
        command.arg("bank").arg(&self.dir).arg(&self.save).env("PCDS_IPC_FD", "3").env("PCDS_BANK", bank);
        if let Some(f) = fail {
            command.env("PCDS_BANK_FAIL", f);
        }
        let fd = theirs.as_raw_fd();
        // SAFETY: between fork and exec only async-signal-safe calls: dup2 and fcntl. fd 3 then
        // is the socket, without close-on-exec.
        unsafe {
            command.pre_exec(move || {
                if fd == 3 {
                    libc::fcntl(3, libc::F_SETFD, 0);
                } else if libc::dup2(fd, 3) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn().map_err(|e| format!("{}: {e}", exe.display()))?;
        Ok(Bank { name: bank.to_string(), child, link: Link::new(ours), hello: Value::Null, started: Instant::now() })
    }

    /// The first bank, waited for (nobody is connected yet): the active one, else the other one
    /// when the active one cannot start (a broken update), as a bootloader falls back.
    fn boot(&mut self) {
        let first = self.banks.as_deref().map(updater::active).unwrap_or_else(|| "a".into());
        let mut kept = self.kept_snapshot().unwrap_or_else(|e| {
            eprintln!("{e}");
            std::process::exit(1)
        });
        let mut tried = Vec::new();
        for bank in [first.clone(), updater::other(&first).to_string()] {
            if self.banks.as_ref().is_some_and(|b| !b.join(&bank).join("pcds").is_file()) {
                continue;
            }
            let snapshot = match kept.as_ref().map(|(fd, _)| fd.try_clone()).transpose() {
                Ok(fd) => fd,
                Err(e) => {
                    eprintln!("{}: {e}", snapshot_path(&self.save));
                    std::process::exit(1);
                }
            };
            match self.boot_bank(&bank, snapshot, &mut kept) {
                Ok(b) => {
                    println!("bank {bank} (pid {}, pcds {}) running the world after {:?}", b.child.id(), b.hello["version"].as_str().unwrap_or("?"), b.started.elapsed());
                    if let Some((fd, at)) = kept {
                        println!("the world is the one kept in {} at the last stop (the last flip's snapshot, newer than the save)", snapshot_path(&self.save));
                        self.last_snapshot = Some((fd, at));
                    }
                    if bank != first {
                        let reason = tried.join("; ");
                        eprintln!("bank {first} could not start the world ({reason}); running bank {bank}, which ran before");
                        if let Some(dir) = &self.banks
                            && let Err(e) = updater::note_fallback(dir, &first, &bank, &format!("could not load the world: {reason}"))
                        {
                            eprintln!("banks: {e}");
                        }
                    }
                    self.game_version = b.hello["gameVersion"].as_str().unwrap_or("").to_string();
                    *self.web.write().unwrap_or_else(|e| e.into_inner()) = self.web_page(&bank);
                    if let Some(dir) = &self.banks
                        && let Err(e) = updater::set_active(dir, &bank)
                    {
                        eprintln!("banks: {e}");
                    }
                    self.live = Some(b);
                    return;
                }
                Err(e) => tried.push(format!("bank {bank}: {e}")),
            }
            if self.banks.is_none() {
                break;
            }
        }
        eprintln!("no bank could start the world: {}", tried.join("; "));
        if kept.is_some() {
            eprintln!("they were given the world kept in {} at the last stop; move it away to start from the save instead", snapshot_path(&self.save));
        }
        std::process::exit(BOOT_FAILED);
    }

    /// The snapshot keep_snapshot() left beside the save, when it is newer than the save (an
    /// older one is removed: the save has its world).
    fn kept_snapshot(&self) -> Result<Option<(OwnedFd, SystemTime)>, String> {
        let path = snapshot_path(&self.save);
        let Ok(at) = std::fs::metadata(&path).and_then(|m| m.modified()) else { return Ok(None) };
        if at <= modified(&self.save) {
            std::fs::remove_file(&path).map_err(|e| format!("{path}: {e}"))?;
            println!("removed {path}: the save is newer");
            return Ok(None);
        }
        let bytes = std::fs::read(&path).map_err(|e| format!("{path}: {e}"))?;
        let fd = ipc::memfd("pcds-snapshot", |w| std::io::Write::write_all(w, &bytes).map_err(|e| e.to_string())).map_err(|e| format!("{path}: {e}"))?;
        Ok(Some((fd, at)))
    }

    /// The world is in no bank (the live one died, the next one is not running it yet): the
    /// last flip's snapshot, when it is newer than the save, is kept beside the save, with the
    /// snapshot's time; the next start runs it.
    fn keep_snapshot(&self) {
        let Some((fd, at)) = &self.last_snapshot else { return };
        if *at <= modified(&self.save) {
            return;
        }
        let path = snapshot_path(&self.save);
        let kept = ipc::read_memfd(fd)
            .map_err(|e| e.to_string())
            .and_then(|bytes| crate::cli::replace_file(std::path::Path::new(&path), &bytes, 0o600))
            .and_then(|_| std::fs::File::options().write(true).open(&path).and_then(|f| f.set_modified(*at)).map_err(|e| e.to_string()));
        match kept {
            Ok(()) => eprintln!("the world is in no bank: the last flip's snapshot (newer than the save) is kept in {path}; the next start runs it"),
            Err(e) => eprintln!("the world is in no bank, and the last flip's snapshot could not be kept in {path} ({e}): the next start runs the save, which is older"),
        }
    }

    /// A bank started on the world: the kept snapshot's, else the save's. A kept snapshot this
    /// bank can never run (another game's tables, a format it does not read) is moved aside,
    /// and `kept` emptied: the world starts from the save.
    fn boot_bank(&self, bank: &str, mut snapshot: Option<OwnedFd>, kept: &mut Option<(OwnedFd, SystemTime)>) -> Result<Bank, String> {
        let mut b = self.spawn(bank, None)?;
        let deadline = Instant::now() + LOAD_DEADLINE;
        let hello = wait_frame(&mut b, deadline).ok_or("it ended before its hello (see above)")?;
        b.hello = hello_of(&hello).inspect_err(|_| {
            let _ = b.child.kill();
            let _ = b.child.wait();
        })?;
        if let Some(why) = snapshot.as_ref().and_then(|fd| ipc::read_memfd(fd).ok()).and_then(|s| never_runs(&s, &b.hello)) {
            let path = snapshot_path(&self.save);
            let aside = format!("{path}.unusable-{}", crate::save::stamp(SystemTime::now()));
            match std::fs::rename(&path, &aside) {
                Ok(()) => eprintln!("{path} can never run ({why}): moved to {aside}; the world starts from the save, which is older"),
                Err(e) => eprintln!("{path} can never run ({why}), and could not be moved aside ({e}); the world starts from the save, which is older"),
            }
            (snapshot, *kept) = (None, None);
        }
        match snapshot {
            Some(fd) => b.link.queue(ipc::RESTORE, 0, b"{\"recover\":true}", Some(fd), 0),
            None => b.link.queue(ipc::START, 0, &[], None, 0),
        }
        let ready = wait_frame(&mut b, deadline).ok_or("it ended while loading the world (see above)")?;
        if ready.kind != ipc::READY {
            let _ = b.child.kill();
            let _ = b.child.wait();
            return Err(format!("frame {} instead of READY", ready.kind));
        }
        Ok(b)
    }

    fn start_admin(&mut self) {
        let addr = crate::env("PCDS_ADMIN").unwrap_or_else(|| "127.0.0.1:7782".into());
        if addr == "off" {
            return;
        }
        let Some(token) = crate::env("PCDS_ADMIN_TOKEN") else {
            eprintln!("admin: PCDS_ADMIN_TOKEN is not set: the admin API and web page are off (pcds setup writes one)");
            return;
        };
        match admin::start(&addr, token, self.web.clone(), self.hooks.feed.clone()) {
            Ok(rx) => {
                println!("admin: http://{addr}/");
                // Plain HTTP: off this machine the token and every action travel in the clear.
                // The container binds 0.0.0.0 inside its own network; its port is published on
                // 127.0.0.1.
                let loopback = addr.rsplit_once(':').and_then(|(h, _)| h.trim_matches(['[', ']']).parse::<std::net::IpAddr>().ok()).is_some_and(|ip| ip.is_loopback());
                if !loopback && crate::env("PCDS_CONTAINER").is_none() {
                    eprintln!("admin: WARNING: {addr} is reachable from other machines over plain HTTP; bind 127.0.0.1 and use an SSH tunnel or an HTTPS proxy");
                }
                self.admin = Some(rx);
            }
            Err(e) => eprintln!("admin: {addr}: {e}"),
        }
    }

    /// Logs in as a Steam game server (anonymously, or with a login token for a fixed SteamID)
    /// and opens the P2P socket invite codes point to.
    fn start_steam(&mut self) {
        if crate::env("PCDS_STEAM").as_deref() == Some("off") {
            return;
        }
        let session = crate::config::env_number("PCDS_SESSION") as u32;
        match steam_login(&self.game_version, session) {
            Ok((steam, listen, code)) => {
                self.invite_code = Some(code);
                self.steam = Some((steam, listen));
            }
            Err(e) if self.utp.is_some() => eprintln!("steam: {e}; continuing with direct IP only"),
            Err(e) => {
                eprintln!("{e}");
                self.shutdown(1);
            }
        }
    }

    fn network(&self) -> &'static str {
        match (self.steam.is_some(), self.utp.is_some()) {
            (true, true) => "both",
            (true, false) => "steam",
            _ => "ip",
        }
    }

    fn serve(&mut self) {
        let mut next_frame = Instant::now();
        while !self.stop.load(Ordering::SeqCst) {
            let frame = Instant::now() >= next_frame;
            if frame {
                next_frame = Instant::now() + crate::FRAME;
                self.poll_steam();
            }
            let mut sent = self.poll_utp();
            sent |= self.read_banks();
            self.admin_requests();
            for reply in self.hooks.replies() {
                self.to_bank(ipc::SAY, 0, reply.as_bytes(), 0);
            }
            if self.autosave_minutes > 0 && Instant::now() >= self.next_save && self.live.is_some() {
                self.next_save += Duration::from_secs(self.autosave_minutes * 60);
                self.to_bank(ipc::SAVE, 0, &[], 0);
            }
            self.updates();
            self.supervise();
            sent |= self.drops();
            self.flush_banks();
            if let Some(utp) = self.utp.as_mut().filter(|_| frame || sent) {
                utp.flush();
            }
            let mut fds = Vec::new();
            let mut out = Vec::new();
            if let Some(u) = &self.utp {
                fds.push(u.fd());
                out.push(false);
            }
            for b in [&self.live, &self.next].into_iter().flatten() {
                fds.push(b.link.fd());
                out.push(b.link.pending());
            }
            ipc::wait(&fds, &out, next_frame.saturating_duration_since(Instant::now()));
        }
        self.shutdown(0);
    }

    // ---- clients ------------------------------------------------------------------------

    fn poll_steam(&mut self) {
        let Some((steam, listen)) = &self.steam else { return };
        steam.run_callbacks();
        let (mut joined, mut gone, mut incoming) = (Vec::new(), Vec::new(), Vec::new());
        while let Some(event) = listen.try_receive_event() {
            match event {
                ListenSocketEvent::Connecting(request) => {
                    let _ = request.accept();
                }
                ListenSocketEvent::Connected(event) => {
                    let steam_id = event.remote().steam_id().map(|id| id.raw()).unwrap_or(0);
                    let client = self.next_client;
                    self.next_client += 1;
                    // Its disconnect names the connection by this (not by SteamID: the same
                    // player may have an old connection still closing).
                    let connection = event.take_connection();
                    let _ = connection.set_connection_user_data(client as i64);
                    joined.push((client, steam_id, connection));
                }
                ListenSocketEvent::Disconnected(event) => {
                    let peer = event.remote().steam_id().map(|id| id.raw()).unwrap_or(0);
                    println!("{peer}: disconnected ({:?})", event.end_reason());
                    let fresh: Vec<(u64, u64)> = joined.iter().map(|(c, s, _)| (*c, *s)).collect();
                    gone.extend(disconnected(event.user_data(), peer, &fresh));
                }
            }
        }
        for (client, steam_id, connection) in joined {
            println!("client {client}: connected over Steam as {steam_id}");
            self.conns.insert(client, Conn::Steam { connection, split: None });
            self.to_bank(ipc::CONNECTED, client, json!({"steamId": steam_id, "trusted": false}).to_string().as_bytes(), 0);
        }
        for (client, conn) in self.conns.iter_mut() {
            if let Conn::Steam { connection, split, .. } = conn {
                for message in connection.receive_messages(64).unwrap_or_default() {
                    match reassemble(split, message.data()) {
                        Ok(batch) => incoming.extend(batch.map(|b| (*client, b))),
                        Err(e) => {
                            println!("client {client}: dropped ({e})");
                            gone.push(*client);
                            break;
                        }
                    }
                }
            }
        }
        for client in gone {
            self.left(client);
        }
        for (client, batch) in incoming {
            self.from_client(client, batch);
        }
    }

    /// Whether anything went out to a client.
    fn poll_utp(&mut self) -> bool {
        let Some(utp) = self.utp.as_mut() else { return false };
        let events = utp.poll();
        // A handshake or a disconnect answers at the next flush.
        let any = !events.is_empty();
        for event in events {
            match event {
                utp::Event::Connected(token, addr) => {
                    let client = self.next_client;
                    self.next_client += 1;
                    println!("client {client}: connected from {addr}");
                    self.ip_clients.insert(token, client);
                    self.conns.insert(client, Conn::Ip(token));
                    let c = json!({"ip": addr.ip().to_string(), "trusted": trusted(addr.ip(), crate::env("PCDS_TRUST_LOOPBACK").as_deref() == Some("1"))});
                    self.to_bank(ipc::CONNECTED, client, c.to_string().as_bytes(), 0);
                }
                utp::Event::Data(token, batch) => {
                    if let Some(c) = self.ip_clients.get(&token).copied() {
                        self.from_client(c, batch);
                    }
                }
                utp::Event::Disconnected(token) => {
                    if let Some(c) = self.ip_clients.remove(&token) {
                        self.left(c);
                    }
                }
            }
        }
        any
    }

    fn from_client(&mut self, client: u64, batch: Vec<u8>) {
        // Gone or kicked: nothing more from it reaches the game (the reason still goes out).
        if !self.conns.contains_key(&client) || self.kicked.contains(&client) {
            return;
        }
        BYTES_IN.fetch_add(batch.len() as u64, Ordering::Relaxed);
        self.to_bank(ipc::DATA, client, &batch, client);
    }

    /// The connection ended from the client's side (or was dropped).
    fn left(&mut self, client: u64) {
        if self.conns.remove(&client).is_some() {
            println!("client {client}: left");
            self.ip_clients.retain(|_, c| *c != client);
            self.kicked.remove(&client);
            self.pending_drop.retain(|(c, _)| *c != client);
            self.to_bank(ipc::GONE, client, &[], 0);
        }
    }

    /// Closes a connection from this side.
    fn close(&mut self, client: u64) {
        match self.conns.remove(&client) {
            // Lingering: what was sent (a kick's reason) is delivered before the close.
            Some(Conn::Steam { connection, .. }) => {
                connection.close(NetConnectionEnd::App(AppNetConnectionEnd::generic_normal()), Some("closed by the server"), true);
            }
            Some(Conn::Ip(token)) => {
                self.ip_clients.remove(&token);
                if let Some(utp) = self.utp.as_mut() {
                    utp.close(&token);
                }
            }
            None => return,
        }
        self.kicked.remove(&client);
        self.to_bank(ipc::GONE, client, &[], 0);
    }

    fn to_client(&mut self, client: u64, batch: &[u8]) -> bool {
        BYTES_OUT.fetch_add(batch.len() as u64, Ordering::Relaxed);
        match self.conns.get(&client) {
            Some(Conn::Steam { connection, .. }) => {
                // A batch it got only part of would be garbage to it: the client goes, loudly.
                if let Err(e) = send(connection, batch) {
                    eprintln!("client {client}: {e}; dropped");
                    self.kicked.insert(client);
                    self.close(client);
                }
                false
            }
            Some(Conn::Ip(token)) => {
                if let Some(utp) = self.utp.as_mut() {
                    utp.send(token, batch);
                }
                true
            }
            None => false,
        }
    }

    /// Kicked clients whose reason has had its moment: closed. Whether any was.
    fn drops(&mut self) -> bool {
        let now = Instant::now();
        let due: Vec<u64> = self.pending_drop.iter().filter(|(_, at)| *at <= now).map(|(c, _)| *c).collect();
        self.pending_drop.retain(|(_, at)| *at > now);
        for c in &due {
            self.close(*c);
        }
        !due.is_empty() || !self.pending_drop.is_empty()
    }

    // ---- the banks ------------------------------------------------------------------------

    /// A frame for the live bank, or held while the world stands. `client`: a client's own
    /// frame, counted against its bound.
    fn to_bank(&mut self, kind: u8, n: u64, body: &[u8], client: u64) {
        // No world: a joining player is told so and let go; nothing else has a game to go to.
        if self.phase == Phase::Stopped {
            if kind == ipc::CONNECTED {
                self.to_client(n, &crate::bank::reason_batch(STOPPED));
                if self.kicked.insert(n) {
                    self.pending_drop.push((n, Instant::now() + KICK_DELAY));
                }
            }
            return;
        }
        let holding = matches!(self.phase, Phase::Snapshotting | Phase::Restoring | Phase::Recovering);
        let queued = if client == 0 {
            0
        } else {
            self.held_bytes.get(&client).copied().unwrap_or(0) + self.live.as_ref().and_then(|b| b.link.queued.get(&client).copied()).unwrap_or(0)
        };
        if client != 0 && queued + body.len() > MAX_CLIENT_QUEUE {
            eprintln!("client {client}: over {} MiB waiting for the game; dropped", MAX_CLIENT_QUEUE >> 20);
            self.kicked.insert(client);
            self.close(client);
            return;
        }
        match (&mut self.live, holding) {
            (Some(b), false) => b.link.queue(kind, n, body, None, client),
            _ => {
                if client != 0 {
                    *self.held_bytes.entry(client).or_default() += body.len() + 13;
                }
                self.held.push((kind, n, body.to_vec(), client));
            }
        }
    }

    /// The held frames to the live bank.
    fn release_held(&mut self) -> usize {
        let held = std::mem::take(&mut self.held);
        self.held_bytes.clear();
        let n = held.len();
        if let Some(b) = self.live.as_mut() {
            for (kind, num, body, client) in held {
                b.link.queue(kind, num, &body, None, client);
            }
        }
        n
    }

    /// What the banks' sockets take now (a bank that is gone shows at the next read).
    fn flush_banks(&mut self) {
        for b in [self.live.as_mut(), self.next.as_mut()].into_iter().flatten() {
            let _ = b.link.flush();
        }
    }

    /// Reads both banks. Whether anything went out to a client.
    fn read_banks(&mut self) -> bool {
        let mut sent = false;
        // Up to 4 MiB a loop from each: the clients' sessions must not wait on a flood.
        // A bank whose bytes are no frames (ipc::MAX_FRAME) counts as one that ended.
        let live_alive = self.live.as_mut().is_none_or(|b| b.link.fill(4 << 20));
        while let Some(f) = self.live.as_mut().and_then(|b| b.link.next()) {
            sent |= self.on_live(f);
        }
        if !live_alive || self.live.as_ref().is_some_and(|b| b.link.broken.is_some()) {
            self.live_died();
        }
        let next_alive = self.next.as_mut().is_none_or(|b| b.link.fill(4 << 20));
        while let Some(f) = self.next.as_mut().and_then(|b| b.link.next()) {
            self.on_next(f);
        }
        // READY may have made it the live one, with frames after READY in its inbox.
        while let Some(f) = self.live.as_mut().and_then(|b| b.link.next()) {
            sent |= self.on_live(f);
        }
        if self.live.as_ref().is_some_and(|b| b.link.broken.is_some()) {
            self.live_died();
        }
        if (!next_alive || self.next.as_ref().is_some_and(|b| b.link.broken.is_some())) && self.next.is_some() {
            let status = self.next.as_mut().map(end).unwrap_or_default();
            match self.phase {
                Phase::Recovering => {
                    let b = self.next.take().unwrap();
                    eprintln!("bank {} ended while starting ({status})", b.name);
                    self.recover(&b.name);
                }
                _ => self.fail_flip(&format!("the new bank ended ({status})")),
            }
        }
        sent
    }

    fn on_live(&mut self, f: Frame) -> bool {
        match f.kind {
            ipc::DATA => return self.to_client(f.n, &f.body),
            ipc::KICK => {
                if self.conns.contains_key(&f.n) && self.kicked.insert(f.n) {
                    self.pending_drop.push((f.n, Instant::now() + KICK_DELAY));
                }
            }
            ipc::ANSWER => self.answer(f.n, &f.body),
            ipc::CHAT => match serde_json::from_slice(&f.body) {
                Ok(event) => self.hooks.feed.push(event),
                Err(e) => eprintln!("chat: an event the front cannot read ({e})"),
            },
            ipc::CHATCFG => match chat_config(&f.body) {
                Ok(chat) => self.hooks.configure(chat),
                Err(e) => eprintln!("chat: {e}; the hooks keep their settings"),
            },
            // A SNAPSHOT answers the HANDOFF of one flip: one of an earlier flip (it came after
            // that flip failed and the bank was told to go on) is not this flip's world.
            ipc::SNAPSHOT if !self.flip.as_ref().is_some_and(|flip| flip.seq == f.n) || self.phase != Phase::Snapshotting => {
                eprintln!("bank: a snapshot for flip {} dropped (the flip under way: {:?}, {:?})", f.n, self.flip.as_ref().map(|flip| flip.seq), self.phase);
            }
            ipc::SNAPSHOT => match f.fd {
                Some(fd) => {
                    if let Some(flip) = self.flip.as_mut() {
                        flip.snapshot = serde_json::from_slice(&f.body).unwrap_or_default();
                        flip.kept = fd.try_clone().ok().map(|d| (d, SystemTime::now()));
                    }
                    if let Some(n) = self.next.as_mut() {
                        n.link.queue(ipc::RESTORE, 0, b"{\"recover\":false}", Some(fd), 0);
                    }
                    self.phase = Phase::Restoring;
                }
                // It could not write one and went on.
                None => self.fail_flip("the running bank could not write a snapshot"),
            },
            _ => {}
        }
        false
    }

    fn on_next(&mut self, f: Frame) {
        match f.kind {
            ipc::HELLO => {
                let hello = hello_of(&f);
                if let (Some(b), Ok(hello)) = (self.next.as_mut(), &hello) {
                    b.hello = hello.clone();
                }
                if self.phase == Phase::Recovering {
                    // A bank of another protocol cannot be given the world: the front must be
                    // started again, from that bank's program.
                    if let Err(e) = &hello {
                        let name = self.next.as_ref().map(|b| b.name.clone()).unwrap_or_default();
                        eprintln!("bank {name} cannot run the world again: {e}; stopping");
                        self.shutdown(1);
                    }
                    let start = match self.recover_from.take() {
                        Some(fd) => (ipc::RESTORE, Some(fd)),
                        None => (ipc::START, None),
                    };
                    let body: &[u8] = if start.0 == ipc::RESTORE { b"{\"recover\":true}" } else { b"" };
                    if let Some(b) = self.next.as_mut() {
                        b.link.queue(start.0, 0, body, start.1, 0);
                    }
                    return;
                }
                if self.phase != Phase::Starting {
                    return;
                }
                let hello = match hello {
                    Ok(hello) => hello,
                    Err(e) => return self.fail_flip(&e),
                };
                let live = self.live.as_ref().map(|b| b.hello.clone()).unwrap_or_default();
                let mut info = hello.clone();
                info["protocol"] = f.n.into();
                if let Err(e) = updater::compatible(&info, &live) {
                    return self.fail_flip(&e);
                }
                if hello["gameVersion"] != live["gameVersion"] {
                    return self.fail_flip(&format!("it has game {}'s tables, the world runs on {}'s", hello["gameVersion"], live["gameVersion"]));
                }
                // Quiesce: from here the world stands.
                if let Some(flip) = self.flip.as_mut() {
                    flip.stopped = Some(Instant::now());
                }
                let seq = self.flip.as_ref().map_or(0, |flip| flip.seq);
                if let Some(b) = self.live.as_mut() {
                    b.link.queue(ipc::HANDOFF, seq, &[], None, 0);
                    let _ = b.link.flush();
                }
                self.phase = Phase::Snapshotting;
            }
            ipc::READY if matches!(self.phase, Phase::Restoring | Phase::Recovering) => self.promote(&f.body),
            _ => {}
        }
    }

    /// The next bank runs: it becomes the live one, the old one ends.
    fn promote(&mut self, ready: &[u8]) {
        let Some(new) = self.next.take() else { return };
        let ready: Value = serde_json::from_slice(ready).unwrap_or_default();
        let old = self.live.take();
        let recovered = self.phase == Phase::Recovering;
        self.phase = Phase::Idle;
        let (from, to) = (old.as_ref().map(|b| b.name.clone()).unwrap_or_else(|| "-".into()), new.name.clone());
        if let Some(mut b) = old {
            b.link.queue(ipc::EXIT, 0, &[], None, 0);
            let _ = b.link.flush();
            self.exiting.push((b.child, Instant::now()));
        }
        self.live = Some(new);
        let held = self.release_held();
        if let Some(dir) = &self.banks
            && let Err(e) = updater::set_active(dir, &to)
        {
            eprintln!("banks: {e}");
        }
        *self.web.write().unwrap_or_else(|e| e.into_inner()) = self.web_page(&to);
        if recovered {
            println!("bank {to} (pid {}) runs the world again", self.live.as_ref().map_or(0, |b| b.child.id()));
            return;
        }
        let Some(mut flip) = self.flip.take() else { return };
        // Restored, so a crash restarts from it (one that failed to restore must not be).
        if let Some(kept) = flip.kept.take() {
            self.last_snapshot = Some(kept);
        }
        let stood = flip.stopped.map(|t| t.elapsed()).unwrap_or_default();
        self.flips += 1;
        let verified = match ready["verified"].as_bool() {
            Some(true) => format!("; the restored state is the snapshot's, checked in {:.1} ms (in the stand)", ready["verifyMs"].as_f64().unwrap_or(0.0)),
            _ => String::new(),
        };
        let message = format!(
            "flip {from} -> {to} ({}): the world stood {:.1} ms (snapshot {} bytes written in {:.1} ms, restored in {:.1} ms; {held} client frames held){verified}",
            flip.reason,
            ms(stood),
            flip.snapshot["bytes"],
            flip.snapshot["ms"].as_f64().unwrap_or(0.0),
            ready["restoreMs"].as_f64().unwrap_or(0.0)
        );
        println!("{message}");
        // Rolled back: the automatic update must not bring back what the owner left.
        if flip.reason == "rollback"
            && let Some(dir) = &self.banks
            && let Err(e) = updater::set_rolled_back(dir, &from)
        {
            eprintln!("banks: the rollback from bank {from} could not be noted ({e}): the automatic update may install its release again");
        }
        let version = self.live.as_ref().map(|b| b.hello["version"].clone()).unwrap_or_default();
        self.record(flip.seq, true, &message, json!({"from": from, "to": to, "stoodMs": ms(stood), "snapshotBytes": flip.snapshot["bytes"], "snapshotMs": flip.snapshot["ms"], "restoreMs": ready["restoreMs"], "held": held, "verified": ready["verified"], "version": version}));
    }

    /// A flip that did not work: the new bank is killed, the old one goes on with the frames
    /// held for the new one.
    fn fail_flip(&mut self, why: &str) {
        if let Some(mut b) = self.next.take() {
            let _ = b.child.kill();
            self.exiting.push((b.child, Instant::now()));
        }
        let phase = std::mem::replace(&mut self.phase, Phase::Idle);
        let flip = self.flip.take();
        let stood = flip.as_ref().and_then(|f| f.stopped).map(|t| t.elapsed());
        let message = match (&mut self.live, phase) {
            (Some(b), Phase::Snapshotting | Phase::Restoring) => {
                b.link.queue(ipc::RESUME, 0, &[], None, 0);
                self.release_held();
                format!("flip failed ({why}); the running bank went on, the world stood {:.1} ms", ms(stood.unwrap_or_default()))
            }
            (Some(_), _) => format!("flip refused ({why}); the running bank never stopped"),
            (None, _) => format!("flip failed ({why})"),
        };
        eprintln!("{message}");
        if let Some(f) = flip {
            self.record(f.seq, false, &message, json!({}));
        }
        if self.live.is_none() {
            let name = self.banks.as_deref().map(updater::active).unwrap_or_else(|| "a".into());
            self.recover(&name);
        }
    }

    /// Starts a flip to `bank`.
    fn flip_to(&mut self, bank: &str, reason: &str, seq: u64, fail: Option<&str>) -> Result<(), String> {
        if self.phase != Phase::Idle || self.live.is_none() {
            return Err("a flip is under way".into());
        }
        let b = self.spawn(bank, fail)?;
        println!("flip to bank {bank} ({reason}): pid {} loading", b.child.id());
        self.next = Some(b);
        self.phase = Phase::Starting;
        self.flip = Some(Flip { seq, reason: reason.to_string(), stopped: None, snapshot: Value::Null, kept: None });
        self.updates.busy = Some((seq, format!("flipping to bank {bank}")));
        Ok(())
    }

    /// The live bank ended.
    fn live_died(&mut self) {
        let Some(mut b) = self.live.take() else { return };
        let status = end(&mut b);
        let name = b.name.clone();
        eprintln!("bank {name} (pid {}) ended ({status})", b.child.id());
        match self.phase {
            // The snapshot is with the new bank already: it takes over as if nothing happened.
            Phase::Restoring => eprintln!("the new bank has its snapshot: it takes over"),
            Phase::Starting | Phase::Snapshotting => {
                self.fail_flip("the running bank ended");
            }
            _ => self.recover(&name),
        }
    }

    /// A bank died outside a flip: the world starts again in that bank, from the last snapshot
    /// when it is newer than the save, else the save. Its players were ahead of both: they are
    /// told to join again. The Steam login and the invite code stay.
    fn recover(&mut self, bank: &str) {
        let now = Instant::now();
        self.crashes.push_back(now);
        while self.crashes.front().is_some_and(|t| now - *t > CRASH_WINDOW) {
            self.crashes.pop_front();
        }
        if self.crashes.len() > MAX_CRASHES {
            eprintln!("the game ended {} times within {} minutes: stopping (systemd starts the server again)", self.crashes.len(), CRASH_WINDOW.as_secs() / 60);
            self.shutdown(1);
        }
        let clients: Vec<u64> = self.conns.keys().copied().collect();
        let reason = crate::bank::reason_batch(RESTARTED);
        for c in &clients {
            self.to_client(*c, &reason);
            if self.kicked.insert(*c) {
                self.pending_drop.push((*c, now + KICK_DELAY));
            }
        }
        self.held.clear();
        self.held_bytes.clear();
        self.answers.clear();
        // The snapshot stays held: should this bank die too before it saves, the next one
        // starts from it again.
        let save_time = modified(&self.save);
        self.recover_from = None;
        let from = match &self.last_snapshot {
            Some((fd, at)) if *at > save_time => match fd.try_clone() {
                Ok(fd) => {
                    self.recover_from = Some(fd);
                    format!("the snapshot of the last flip ({} s old, newer than the save)", at.elapsed().map(|d| d.as_secs()).unwrap_or(0))
                }
                Err(e) => format!("the save: the snapshot of the last flip, newer, could not be passed on ({e})"),
            },
            _ => "the save".to_string(),
        };
        match self.spawn(bank, None) {
            Ok(b) => {
                eprintln!("restarting bank {bank} (pid {}) from {from}; {} players were told to join again", b.child.id(), clients.len());
                self.next = Some(b);
                self.phase = Phase::Recovering;
            }
            Err(e) => {
                eprintln!("cannot restart bank {bank}: {e}");
                self.shutdown(1);
            }
        }
    }

    /// Timeouts and the old banks' ends.
    fn supervise(&mut self) {
        if let Some(b) = &self.next {
            let late = match self.phase {
                Phase::Starting | Phase::Recovering => b.started.elapsed() > LOAD_DEADLINE,
                Phase::Snapshotting | Phase::Restoring => self.flip.as_ref().and_then(|f| f.stopped).is_some_and(|t| t.elapsed() > READY_DEADLINE),
                Phase::Idle | Phase::Stopped => false,
            };
            if late {
                match self.phase {
                    Phase::Recovering => {
                        let mut b = self.next.take().unwrap();
                        let _ = b.child.kill();
                        let _ = b.child.wait();
                        self.recover(&b.name);
                    }
                    _ => self.fail_flip(&format!("not ready within {:?}", if self.phase == Phase::Starting { LOAD_DEADLINE } else { READY_DEADLINE })),
                }
            }
        }
        // The old banks: EXIT given; killed if they linger.
        self.exiting.retain_mut(|(child, since)| match child.try_wait() {
            Ok(Some(_)) => false,
            _ if since.elapsed() > Duration::from_secs(10) => {
                let _ = child.kill();
                let _ = child.wait();
                false
            }
            _ => true,
        });
    }

    // ---- the admin API --------------------------------------------------------------------

    fn admin_requests(&mut self) {
        let Some(rx) = &self.admin else { return };
        let requests: Vec<admin::Request> = rx.try_iter().collect();
        for r in requests {
            match (r.method.as_str(), r.path.as_str()) {
                ("POST", "/api/stop") => {
                    let _ = r.reply.send((200, "{\"ok\":true}".into()));
                    self.stop.store(true, Ordering::SeqCst);
                }
                ("GET", "/api/update") => {
                    let _ = r.reply.send((200, self.update_status().to_string()));
                }
                ("POST", "/api/update") => {
                    let answer = self.update_request(&r.body);
                    let _ = r.reply.send(answer);
                }
                ("POST", "/api/world/stop") => {
                    let answer = match self.stop_world(STOPPED) {
                        Ok(()) => (200, json!({"ok": true, "world": self.world_name()}).to_string()),
                        Err(e) if self.phase == Phase::Stopped => (500, json!({"error": e, "stopped": true}).to_string()),
                        Err(e) => (409, json!({"error": e}).to_string()),
                    };
                    let _ = r.reply.send(answer);
                }
                ("POST", "/api/world/start") => {
                    let name = r.body["world"].as_str().map(str::to_string);
                    let answer = match self.start_world(name.as_deref()) {
                        Ok(w) => (200, json!({"ok": true, "world": w}).to_string()),
                        Err(e) => (409, json!({"error": e}).to_string()),
                    };
                    let _ = r.reply.send(answer);
                }
                // Another world: the players told and let go, this one saved, that one started.
                ("POST", "/api/world/switch") => {
                    let Some(name) = r.body["world"].as_str().map(str::to_string) else {
                        let _ = r.reply.send((400, "{\"error\":\"'world' is required\"}".into()));
                        continue;
                    };
                    let answer = if name == self.world_name() && self.phase != Phase::Stopped {
                        (409, json!({"error": format!("{name} is the world that runs")}).to_string())
                    } else if !std::path::Path::new(&self.save).with_file_name(format!("{name}.json")).is_file() || crate::dashboard::world_name(&name).is_err() {
                        (400, json!({"error": format!("no world {name}")}).to_string())
                    } else {
                        let stopped = if self.phase == Phase::Stopped { Ok(()) } else { self.stop_world(&format!("The server moves to another world ({name}): join again")) };
                        match stopped.and_then(|()| self.start_world(Some(&name))) {
                            Ok(w) => (200, json!({"ok": true, "world": w}).to_string()),
                            Err(e) => (500, json!({"error": e, "stopped": self.phase == Phase::Stopped}).to_string()),
                        }
                    };
                    let _ = r.reply.send(answer);
                }
                ("GET", "/api/status") if self.phase == Phase::Stopped => {
                    let _ = r.reply.send((200, self.stopped_status().to_string()));
                }
                ("GET", "/api/worlds") if self.phase == Phase::Stopped => {
                    let _ = r.reply.send((200, crate::dashboard::worlds(&self.save).to_string()));
                }
                _ if self.phase == Phase::Stopped => {
                    let _ = r.reply.send((409, "{\"error\":\"the world is stopped: start it first\"}".into()));
                }
                _ => {
                    let id = self.next_request;
                    self.next_request += 1;
                    let body = json!({"method": r.method, "path": r.path, "body": r.body, "autosaveMinutes": self.autosave_minutes});
                    self.answers.insert(id, (r.reply, r.path));
                    self.to_bank(ipc::ADMIN, id, body.to_string().as_bytes(), 0);
                }
            }
        }
    }

    fn answer(&mut self, id: u64, body: &[u8]) {
        let Some((reply, path)) = self.answers.remove(&id) else { return };
        let a: Value = serde_json::from_slice(body).unwrap_or_default();
        if let Some(m) = a["autosaveMinutes"].as_u64()
            && m != self.autosave_minutes
        {
            self.autosave_minutes = m;
            self.next_save = Instant::now() + Duration::from_secs(m.max(1) * 60);
        }
        let status = a["status"].as_u64().unwrap_or(500) as u16;
        let mut text = a["body"].as_str().unwrap_or("{}").to_string();
        if path == "/api/status"
            && let Ok(mut s) = serde_json::from_str::<Value>(&text)
        {
            self.overlay(&mut s);
            text = s.to_string();
        }
        let _ = reply.send((status, text));
    }

    /// The front's part of GET /api/status.
    fn overlay(&self, s: &mut Value) {
        let steam: HashSet<u64> = self.conns.iter().filter(|(_, c)| matches!(c, Conn::Steam { .. })).map(|(c, _)| *c).collect();
        for p in s["players"].as_array_mut().into_iter().flatten() {
            if p["id"].as_u64().is_some_and(|id| steam.contains(&id)) {
                p["ip"] = "steam".into();
            }
        }
        let banks: Vec<u32> = [&self.live, &self.next].into_iter().flatten().map(|b| b.child.id()).collect();
        let (mut cpu, mut memory) = (crate::dashboard::reaped_children_cpu(), 0);
        let me = crate::dashboard::process();
        cpu += me["cpuSeconds"].as_f64().unwrap_or(0.0);
        memory += me["memoryBytes"].as_u64().unwrap_or(0);
        for pid in banks {
            let (c, m) = crate::dashboard::process_of(pid);
            cpu += c;
            memory += m;
        }
        s["network"] = self.network().into();
        s["inviteCode"] = self.invite_code.clone().into();
        s["steam"] = self.invite_code.is_some().into();
        s["port"] = self.port.into();
        s["uptimeSeconds"] = self.started.elapsed().as_secs().into();
        s["autosaveMinutes"] = self.autosave_minutes.into();
        s["process"] = json!({"cpuSeconds": cpu, "memoryBytes": memory});
        s["traffic"] = crate::dashboard::traffic();
        s["banks"] = self.banks_status();
        s["worldControl"] = true.into();
        s["update"] = self.update_status();
    }

    fn banks_status(&self) -> Value {
        let bank = |b: &Bank| json!({"bank": b.name, "pid": b.child.id(), "version": b.hello["version"], "protocol": ipc::PROTOCOL, "format": b.hello["format"], "runningSeconds": b.started.elapsed().as_secs()});
        let mut v = match &self.banks {
            Some(dir) => updater::describe(dir),
            None => json!({"dir": null, "active": null, "installed": []}),
        };
        v["live"] = self.live.as_ref().map(bank).unwrap_or(Value::Null);
        v["next"] = self.next.as_ref().map(bank).unwrap_or(Value::Null);
        v["phase"] = format!("{:?}", self.phase).to_lowercase().into();
        v["flips"] = self.flips.into();
        v["crashes"] = self.crashes.len().into();
        v["frontProtocol"] = ipc::PROTOCOL.into();
        v["frontVersion"] = env!("CARGO_PKG_VERSION").into();
        v
    }

    // ---- updates --------------------------------------------------------------------------

    fn update_status(&self) -> Value {
        let u = &self.updates;
        json!({
            "state": if u.busy.is_some() { "busy" } else { "idle" },
            "doing": u.busy.as_ref().map(|(_, what)| what.clone()),
            "seq": u.seq,
            "last": u.history.back(),
            "history": u.history,
            "auto": u.auto,
            "checkHours": u.every.as_secs() / 3600,
            "latest": u.latest,
            "available": u.newer,
            "checkedAt": u.checked,
            "releases": self.banks.as_ref().map(|_| updater::releases_url()),
        })
    }

    fn record(&mut self, seq: u64, ok: bool, message: &str, mut extra: Value) {
        extra["seq"] = seq.into();
        extra["ok"] = ok.into();
        extra["message"] = message.into();
        extra["at"] = now_secs().into();
        self.updates.history.push_back(extra);
        while self.updates.history.len() > 20 {
            self.updates.history.pop_front();
        }
        if self.updates.busy.as_ref().is_some_and(|(s, _)| *s == seq) {
            self.updates.busy = None;
        }
    }

    /// POST /api/update: {"version": "v0.6.0"} (null: the latest; "onlyNewer": true installs
    /// it only if it is later than the live bank's), {"check": true}: only look up the latest,
    /// or {"rollback": true}: back to the other bank. The answer comes at once with the
    /// request's "seq"; GET /api/update has its outcome in "history" when done.
    fn update_request(&mut self, body: &Value) -> (u16, String) {
        if self.updates.busy.is_some() || self.phase != Phase::Idle {
            return (409, json!({"error": "an update or a flip is under way", "doing": self.updates.busy.as_ref().map(|b| b.1.clone())}).to_string());
        }
        let Some(live) = &self.live else { return (503, "{\"error\":\"the game is starting\"}".into()) };
        let target = updater::other(&live.name).to_string();
        // The e2e tests' fault injection; off unless the front was started with PCDS_TEST_FAULTS=1.
        let fail = body["failForTest"].as_str().filter(|_| crate::env("PCDS_TEST_FAULTS").as_deref() == Some("1")).map(str::to_string);
        self.updates.seq += 1;
        let seq = self.updates.seq;
        if body["rollback"] == true {
            if let Some(dir) = &self.banks {
                let Some(info) = updater::info(dir, &target) else { return (400, json!({"error": format!("bank {target} holds no release to go back to")}).to_string()) };
                if let Err(e) = updater::compatible(&info, &live.hello) {
                    return (400, json!({"error": e}).to_string());
                }
            }
            return match self.flip_to(&target, "rollback", seq, fail.as_deref()) {
                Ok(()) => (200, json!({"ok": true, "seq": seq, "bank": target}).to_string()),
                Err(e) => (500, json!({"error": e}).to_string()),
            };
        }
        let Some(dir) = self.banks.clone() else {
            return (400, "{\"error\":\"no banks: this server runs from a build, not from pcds run (PCDS_BANKS); rollback flips between two copies of it\"}".into());
        };
        if body["check"] == true {
            self.check(seq);
            return (200, json!({"ok": true, "seq": seq}).to_string());
        }
        let version = body["version"].as_str().filter(|v| *v != "latest").map(str::to_string);
        self.download(seq, dir, target, version, body["onlyNewer"] == true, false);
        (200, json!({"ok": true, "seq": seq}).to_string())
    }

    /// A download into the standby bank, on a thread of its own (it takes seconds); the loop
    /// flips when it is in. `only_newer`: install only a later release than the live bank's.
    /// `auto`: the automatic check, which also leaves out the release rolled back from; an
    /// update asked for clears that note.
    fn download(&mut self, seq: u64, dir: PathBuf, bank: String, version: Option<String>, only_newer: bool, auto: bool) {
        let live = self.live.as_ref().map(|b| b.hello.clone()).unwrap_or_default();
        let tx = self.updates.tx.clone();
        self.updates.busy = Some((seq, format!("downloading {} into bank {bank}", version.as_deref().unwrap_or("the latest release"))));
        answer_on_thread(tx, seq, move || {
            let release = updater::lookup(&updater::releases_url(), version.as_deref())?;
            if only_newer && !updater::newer(&release.tag, live["version"].as_str().unwrap_or("")) {
                return Ok(UpdateDone::UpToDate { seq, tag: release.tag });
            }
            if auto && let Some(why) = updater::auto_skips(&dir, &release.tag)? {
                return Ok(UpdateDone::Skipped { seq, message: why });
            }
            let info = updater::install(&release, &dir, &bank, Some(&live))?;
            if !auto {
                updater::clear_rolled_back(&dir)?;
            }
            Ok(UpdateDone::Installed { seq, bank, info })
        });
    }

    /// The latest release looked up on a thread of its own; nothing is installed.
    fn check(&mut self, seq: u64) {
        let live = self.live.as_ref().map(|b| b.hello["version"].as_str().unwrap_or("").to_string()).unwrap_or_default();
        let tx = self.updates.tx.clone();
        self.updates.busy = Some((seq, "checking for a new release".into()));
        answer_on_thread(tx, seq, move || updater::lookup(&updater::releases_url(), None).map(|release| UpdateDone::Checked { seq, newer: updater::newer(&release.tag, &live), tag: release.tag }));
    }

    fn updates(&mut self) {
        while let Ok(done) = self.updates.rx.try_recv() {
            self.updates.checked = Some(now_secs());
            match done {
                UpdateDone::Installed { seq, bank, info } => {
                    let tag = info["tag"].as_str().unwrap_or("?").to_string();
                    self.updates.latest = Some(tag.clone());
                    self.updates.newer = false;
                    println!("update: {tag} installed in bank {bank} (sha256 {} checked)", info["sha256"].as_str().unwrap_or(""));
                    if let Err(e) = self.flip_to(&bank, &format!("update to {tag}"), seq, None) {
                        self.record(seq, false, &format!("{tag} is in bank {bank}, but the flip did not start: {e}"), json!({}));
                    }
                }
                UpdateDone::Failed { seq, message } => {
                    eprintln!("update: {message}");
                    self.record(seq, false, &message, json!({}));
                }
                UpdateDone::UpToDate { seq, tag } => {
                    self.updates.latest = Some(tag.clone());
                    self.updates.newer = false;
                    self.record(seq, true, &format!("up to date ({tag} is the latest release)"), json!({}));
                }
                UpdateDone::Skipped { seq, message } => {
                    println!("update: {message}");
                    self.record(seq, true, &message, json!({}));
                }
                UpdateDone::Checked { seq, tag, newer } => {
                    self.updates.latest = Some(tag.clone());
                    self.updates.newer = newer;
                    let message = if newer { format!("{tag} is available") } else { format!("up to date ({tag} is the latest release)") };
                    self.record(seq, true, &message, json!({"check": true}));
                }
            }
        }
        if self.updates.auto && Instant::now() >= self.updates.next_check {
            self.updates.next_check = Instant::now() + self.updates.every;
            if let (Some(dir), Some(live), None, Phase::Idle) = (self.banks.clone(), &self.live, &self.updates.busy, self.phase) {
                let bank = updater::other(&live.name).to_string();
                self.updates.seq += 1;
                let seq = self.updates.seq;
                println!("update: checking for a new release (PCDS_AUTO_UPDATE=on)");
                self.download(seq, dir, bank, None, true, true);
            }
        }
    }

    // ---- the end --------------------------------------------------------------------------

    /// Saves the world, ends the bank, disconnects everyone, exits.
    /// The live bank told to save and stop, and waited for (within STOP_GRACE twice). Err: it
    /// ended without saving (it says where the world is kept), or was killed before it saved.
    fn stop_live(&mut self) -> Result<(), String> {
        let Some(mut b) = self.live.take() else { return Ok(()) };
        b.link.queue(ipc::STOP, 0, &[], None, 0);
        let deadline = Instant::now() + STOP_GRACE;
        loop {
            if b.link.flush().is_err() || Instant::now() > deadline {
                break;
            }
            if let Ok(Some(_)) = b.child.try_wait() {
                break;
            }
            // Its last batches still drain, so its writes never block its save.
            if ipc::wait(&[b.link.fd()], &[b.link.pending()], Duration::from_millis(50))[0] && !b.link.fill(usize::MAX) {
                break;
            }
            while b.link.next().is_some() {}
        }
        match b.child.try_wait() {
            Ok(Some(_)) => {}
            _ if b.link.broken.is_some() => {
                end(&mut b);
            }
            _ => {
                let deadline = Instant::now() + STOP_GRACE;
                while Instant::now() < deadline && matches!(b.child.try_wait(), Ok(None)) {
                    std::thread::sleep(Duration::from_millis(50));
                }
                let _ = b.child.kill();
            }
        }
        match b.child.wait() {
            Ok(s) if !s.success() => Err(format!("bank {} ended ({s}) at the stop: the world may not be saved (see above)", b.name)),
            _ => Ok(()),
        }
    }

    // ---- the world: stop, start, another one ------------------------------------------------

    /// Every player told `why` and let go, the world saved and its bank ended: Stopped.
    fn stop_world(&mut self, why: &str) -> Result<(), String> {
        if let Some((_, doing)) = &self.updates.busy {
            return Err(format!("an update is under way ({doing}): wait for it"));
        }
        if self.phase != Phase::Idle || self.live.is_none() {
            return Err(format!("the world cannot stop now ({})", if self.live.is_none() { "it is not running".to_string() } else { format!("{:?}", self.phase).to_lowercase() }));
        }
        let reason = crate::bank::reason_batch(why);
        let now = Instant::now();
        for c in self.conns.keys().copied().collect::<Vec<_>>() {
            self.to_client(c, &reason);
            if self.kicked.insert(c) {
                self.pending_drop.push((c, now + KICK_DELAY));
            }
        }
        if let Some(utp) = self.utp.as_mut() {
            utp.flush();
        }
        self.held.clear();
        self.held_bytes.clear();
        for (_, (reply, _)) in self.answers.drain() {
            let _ = reply.send((503, "{\"error\":\"the world stopped\"}".into()));
        }
        let saved = self.stop_live();
        // The save has the world now (or the bank kept it beside the save): the last flip's
        // snapshot is older.
        self.last_snapshot = None;
        self.recover_from = None;
        self.phase = Phase::Stopped;
        println!("world {}: stopped by the admin", self.world_name());
        saved
    }

    fn world_name(&self) -> String {
        std::path::Path::new(&self.save).file_stem().unwrap_or_default().to_string_lossy().to_string()
    }

    /// The world (`name`, else the one it ran) started in the active bank: Idle. A world other
    /// than the last one takes this server's lock of it and is remembered in server.env
    /// (PCDS_WORLD), so the next start runs it too; but only once it runs: one that does not
    /// start leaves the server as it was (stopped, on the world it stopped), and server.env
    /// naming the world that last ran, so a restart does not fail on it.
    fn start_world(&mut self, name: Option<&str>) -> Result<String, String> {
        if self.phase != Phase::Stopped {
            return Err("the world runs: stop it first, or run another one".into());
        }
        let mut moved = None;
        if let Some(name) = name.filter(|n| *n != self.world_name()) {
            let name = crate::dashboard::world_name(name)?;
            let save = std::path::Path::new(&self.save).with_file_name(format!("{name}.json")).to_string_lossy().to_string();
            if !std::path::Path::new(&save).is_file() {
                return Err(format!("no world {name}"));
            }
            let lock = crate::cli::hold(std::path::Path::new(&format!("{save}.lock")), "another server runs that world")?;
            moved = Some((std::mem::replace(&mut self.save, save), self.world_lock.replace(lock), name));
        }
        let started = self.start_bank();
        match (started, moved) {
            (Err(e), Some((save, lock, name))) => {
                (self.save, self.world_lock) = (save, lock);
                Err(format!("world {name} did not start: {e}; the server stays stopped on world {}: start it, or run another world", self.world_name()))
            }
            (Ok(world), Some((_, _, name))) => {
                if let Some(env) = crate::env("PCDS_ENV_FILE")
                    && let Err(e) = crate::cli::set_env_file(std::path::Path::new(&env), "PCDS_WORLD", &name)
                {
                    eprintln!("world {name}: not remembered for the next start ({e})");
                }
                Ok(world)
            }
            (started, None) => started,
        }
    }

    /// The world of `self.save` booted in the active bank: Idle.
    fn start_bank(&mut self) -> Result<String, String> {
        let bank = self.banks.as_deref().map(updater::active).unwrap_or_else(|| "a".into());
        let mut kept = self.kept_snapshot()?;
        let snapshot = kept.as_ref().map(|(fd, _)| fd.try_clone()).transpose().map_err(|e| e.to_string())?;
        let b = self.boot_bank(&bank, snapshot, &mut kept)?;
        if let Some((fd, at)) = kept {
            self.last_snapshot = Some((fd, at));
        }
        self.game_version = b.hello["gameVersion"].as_str().unwrap_or("").to_string();
        *self.web.write().unwrap_or_else(|e| e.into_inner()) = self.web_page(&bank);
        self.live = Some(b);
        self.phase = Phase::Idle;
        self.next_save = Instant::now() + Duration::from_secs(self.autosave_minutes.max(1) * 60);
        let name = self.world_name();
        println!("world {name}: running (bank {bank}), started by the admin");
        Ok(name)
    }

    /// GET /api/status while the world is stopped: the front's part, and which world it was.
    fn stopped_status(&self) -> Value {
        let mut s = json!({"state": "stopped", "save": self.world_name(), "players": [], "serverVersion": env!("CARGO_PKG_VERSION"), "gameVersion": self.game_version});
        self.overlay(&mut s);
        s
    }

    fn shutdown(&mut self, mut code: i32) -> ! {
        if self.live.is_some() && matches!(self.phase, Phase::Snapshotting | Phase::Restoring | Phase::Starting) {
            self.fail_flip("the server is stopping");
        }
        if let Some(mut b) = self.next.take() {
            let _ = b.child.kill();
            let _ = b.child.wait();
        }
        // No live bank to save the world (it died; the next one was starting from the snapshot).
        if self.live.is_none() {
            self.keep_snapshot();
        }
        if let Err(e) = self.stop_live() {
            eprintln!("{e}");
            code = code.max(1);
        }
        for (mut child, _) in self.exiting.drain(..) {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(utp) = &mut self.utp {
            utp.tokens().iter().for_each(|t| utp.disconnect(t));
        }
        std::process::exit(code)
    }
}

/// The client a Steam disconnect is about: the one whose connection carries its id; one that
/// closed before its id was set (connected and gone within one poll) is the one of that
/// SteamID that connected in this poll.
fn disconnected(user_data: i64, peer: u64, joined_now: &[(u64, u64)]) -> Vec<u64> {
    match u64::try_from(user_data).ok().filter(|c| *c > 0) {
        Some(client) => vec![client],
        None => joined_now.iter().filter(|(_, s)| *s == peer).map(|(c, _)| *c).collect(),
    }
}

/// Whether the SteamID a direct-IP client reports is believed: only from this machine, and
/// only when asked (PCDS_TRUST_LOOPBACK=1: tests, the load and probe tools). Behind a UDP
/// relay (playit.gg, frp, ssh -R) every player comes from 127.0.0.1.
fn trusted(ip: std::net::IpAddr, opted_in: bool) -> bool {
    opted_in && utp::is_loopback(ip)
}

/// An update's work on a thread of its own, whose outcome always comes: a panic in it is a
/// Failed one, so the update is never busy for ever.
fn answer_on_thread(tx: Sender<UpdateDone>, seq: u64, work: impl FnOnce() -> Result<UpdateDone, String> + Send + 'static) {
    std::thread::spawn(move || {
        let done = std::panic::catch_unwind(std::panic::AssertUnwindSafe(work)).unwrap_or_else(|_| Err("the update's thread panicked (see above)".into()));
        let _ = tx.send(done.unwrap_or_else(|message| UpdateDone::Failed { seq, message }));
    });
}

/// Where the front keeps the last flip's snapshot when it stops with the world in no bank.
pub fn snapshot_path(save: &str) -> String {
    format!("{save}.snapshot")
}

fn modified(path: &str) -> SystemTime {
    std::fs::metadata(path).and_then(|m| m.modified()).unwrap_or(UNIX_EPOCH)
}

/// The hooks' settings a bank sent (CHATCFG). Err names the field only: a value may be a
/// webhook URL or the secret, and serde's messages quote values.
fn chat_config(body: &[u8]) -> Result<crate::rules::Chat, String> {
    let v: Value = serde_json::from_slice(body).map_err(|_| "chat config unreadable (not JSON)".to_string())?;
    serde_json::from_value(v.clone()).map_err(|_| {
        let fields = v.as_object().into_iter().flatten();
        match fields.into_iter().find(|(k, x)| serde_json::from_value::<crate::rules::Chat>(json!({ k.as_str(): x })).is_err()) {
            Some((k, _)) => format!("chat config unreadable at chat.{k}"),
            None => "chat config unreadable".to_string(),
        }
    })
}

/// Why a snapshot can never run on the bank of this HELLO: it is on another game's tables, in
/// a format the bank does not read, or no snapshot at all. None: it may (any other error is the
/// bank's to tell).
fn never_runs(snapshot: &[u8], hello: &Value) -> Option<String> {
    let head = snapshot.split(|b| *b == b'\n').next().and_then(|h| serde_json::from_slice::<Value>(h).ok());
    let Some(head) = head.filter(|h| h["format"].is_u64() && h["game_version"].is_string()) else { return Some("not a snapshot".into()) };
    if head["game_version"] != hello["gameVersion"] {
        return Some(format!("it is on game {}'s tables, the bank has {}'s", head["game_version"], hello["gameVersion"]));
    }
    if !hello["reads"].as_array().is_some_and(|r| r.contains(&head["format"])) {
        return Some(format!("snapshot format {}, the bank reads {}", head["format"], hello["reads"]));
    }
    None
}

/// A bank's HELLO, refused unless it speaks this front's protocol (at the boot, a flip and a
/// restart after a crash alike: a world never goes to a bank that reads the frames otherwise).
fn hello_of(f: &Frame) -> Result<Value, String> {
    if f.kind != ipc::HELLO {
        return Err(format!("frame {} instead of HELLO", f.kind));
    }
    let mut info: Value = serde_json::from_slice(&f.body).map_err(|e| format!("its HELLO is not JSON ({e})"))?;
    info["protocol"] = f.n.into();
    if f.n != ipc::PROTOCOL {
        // The same words as an update refused for its protocol.
        updater::compatible(&info, &Value::Null)?;
    }
    info.as_object_mut().map(|o| o.remove("protocol"));
    Ok(info)
}

/// A bank whose socket closed (it is ending) or broke (it is killed): waited for; how it ended.
fn end(b: &mut Bank) -> String {
    if let Some(why) = &b.link.broken {
        eprintln!("bank {} (pid {}): {why}; killed", b.name, b.child.id());
        let _ = b.child.kill();
    }
    let status = b.child.wait().map(|s| s.to_string()).unwrap_or_default();
    match &b.link.broken {
        Some(why) => format!("{status}, after {why}"),
        None => status,
    }
}

/// A bank's next frame, read until `deadline` or its end.
fn wait_frame(b: &mut Bank, deadline: Instant) -> Option<Frame> {
    loop {
        let _ = b.link.flush();
        if let Some(f) = b.link.next() {
            return Some(f);
        }
        if Instant::now() > deadline {
            let _ = b.child.kill();
            let _ = b.child.wait();
            return None;
        }
        ipc::wait(&[b.link.fd()], &[b.link.pending()], Duration::from_millis(100));
        if !b.link.fill(usize::MAX) {
            let status = end(b);
            eprintln!("bank {} ended ({status})", b.name);
            return None;
        }
    }
}

fn steam_login(version: &str, session_number: u32) -> Result<(Server, ListenSocket, String), String> {
    let (steam, client) = Server::init(Ipv4Addr::UNSPECIFIED, 0, 0xFFFF, ServerMode::NoAuthentication, version)
        .map_err(|e| format!("Steam game server init failed ({e}); is steamclient.so in ~/.steam/sdk64?"))?;
    steam.set_product(APP_ID);
    steam.set_mod_dir("Planet Crafter");
    steam.set_game_description("Planet Crafter standalone server");
    steam.set_dedicated_server(true);
    let connected = Arc::new(AtomicBool::new(false));
    let logon = connected.clone();
    let _logon = steam.register_callback(move |_: SteamServersConnected| logon.store(true, Ordering::SeqCst));
    match crate::env("PCDS_STEAM_TOKEN") {
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
    // Room for the largest join queued at once (the default is 512 KiB); connections take it
    // from the listen socket.
    let buffer = NetworkingConfigEntry::new_int32(NetworkingConfigValue::SendBufferSize, SEND_BUFFER);
    let listen = steam.networking_sockets().create_listen_socket_p2p(0, [buffer]).map_err(|_| "P2P listen socket failed")?;
    let steam_id = steam.steam_id().raw();
    let code = format!("{}{:04}", crate::protocol::invite_code(steam_id), session_number);
    println!("steam: game server {steam_id}, invite code {code}");
    Ok((steam, listen, code))
}

/// The game's Steam transport frames batches over 512 KiB as "Split<n>" plus n chunks. Its
/// receive buffer holds MAX_STEAM_MESSAGE, and it reads n as one digit: bigger batches go as
/// several, split between messages. Err when Steam refuses a send (the client then has part
/// of a batch) or one message is too big for the game.
fn send(connection: &NetConnection, batch: &[u8]) -> Result<(), String> {
    for part in steam_parts(batch)? {
        let refused = |e| format!("Steam refused a send of {} bytes ({e:?})", part.len());
        if part.len() <= CHUNK {
            connection.send_message(&part, SendFlags::RELIABLE).map_err(refused)?;
            continue;
        }
        let chunks: Vec<&[u8]> = part.chunks(CHUNK).collect();
        connection.send_message(format!("Split{}", chunks.len()).as_bytes(), SendFlags::RELIABLE).map_err(refused)?;
        for c in chunks {
            connection.send_message(c, SendFlags::RELIABLE).map_err(refused)?;
        }
    }
    Ok(())
}

/// A batch as batches the game's Steam transport takes (each at most MAX_STEAM_MESSAGE).
fn steam_parts(batch: &[u8]) -> Result<Vec<Vec<u8>>, String> {
    if batch.len() <= MAX_STEAM_MESSAGE {
        return Ok(vec![batch.to_vec()]);
    }
    let (mut parts, mut part, mut size) = (Vec::new(), crate::wire::Batch::default(), 16);
    for (kind, body) in crate::wire::messages(batch) {
        // The message's kind and length are at most 5 bytes each.
        let n = body.len() + 10;
        if 16 + n > MAX_STEAM_MESSAGE {
            return Err(format!("a message of {} bytes (type {kind}), and the game's Steam transport takes at most {MAX_STEAM_MESSAGE} at once: this world can be joined over direct IP only", body.len()));
        }
        if size + n > MAX_STEAM_MESSAGE {
            parts.push(std::mem::take(&mut part).finish());
            size = 16;
        }
        part.push(kind, body);
        size += n;
    }
    if !part.is_empty() {
        parts.push(part.finish());
    }
    Ok(parts)
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

    /// A bank on one end of a socket pair (a sleeping process stands in for its program).
    fn fake_bank(name: &str) -> (Bank, Link) {
        let (ours, theirs) = UnixStream::pair().unwrap();
        ours.set_nonblocking(true).unwrap();
        let child = Command::new("sleep").arg("30").spawn().unwrap();
        (Bank { name: name.into(), child, link: Link::new(ours), hello: Value::Null, started: Instant::now() }, Link::new(theirs))
    }

    fn test_front() -> Front {
        Front::new("data", "/nonexistent/world.json", None, None, 0, Arc::new(AtomicBool::new(false)))
    }

    fn snapshot_fd(text: &str) -> OwnedFd {
        ipc::memfd("test", |w| std::io::Write::write_all(w, text.as_bytes()).map_err(|e| e.to_string())).unwrap()
    }

    /// A world that does not start leaves the server stopped on the world it stopped, with its
    /// lock, and server.env naming that world (a restart would fail on the other).
    #[test]
    fn a_world_that_does_not_start_is_not_remembered() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("pcds-start-world-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("banks/a")).unwrap();
        let program = dir.join("banks/a/pcds");
        std::fs::write(&program, "#!/bin/sh\nexit 1\n").unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        let (a, b, env) = (dir.join("a.json"), dir.join("b.json"), dir.join("server.env"));
        std::fs::write(&a, "{}").unwrap();
        std::fs::write(&b, "{}").unwrap();
        std::fs::write(&env, "PCDS_WORLD=a\n").unwrap();
        // SAFETY: only start_world reads it, and no other test calls that.
        unsafe { std::env::set_var("PCDS_ENV_FILE", &env) };
        let mut front = Front::new("data", &a.to_string_lossy(), Some(dir.join("banks")), None, 0, Arc::new(AtomicBool::new(false)));
        front.phase = Phase::Stopped;
        front.world_lock = Some(crate::cli::hold(&dir.join("a.json.lock"), "busy").unwrap());
        let e = front.start_world(Some("b")).unwrap_err();
        assert!(e.contains("world b did not start") && e.contains("stays stopped on world a"), "{e}");
        assert_eq!((front.world_name(), front.phase), ("a".to_string(), Phase::Stopped));
        assert_eq!(std::fs::read_to_string(&env).unwrap(), "PCDS_WORLD=a\n");
        // The lock of a is still this server's; b's is free again.
        assert!(crate::cli::hold(&dir.join("a.json.lock"), "busy").is_err());
        assert!(crate::cli::hold(&dir.join("b.json.lock"), "busy").is_ok());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A panic in an update's thread is that update's failure, not a busy update for ever.
    #[test]
    fn a_panicking_update_thread_fails_its_update() {
        let (tx, rx) = std::sync::mpsc::channel();
        answer_on_thread(tx, 3, || panic!("test panic"));
        assert!(matches!(rx.recv_timeout(Duration::from_secs(10)), Ok(UpdateDone::Failed { seq: 3, message }) if message.contains("panicked")));
    }

    /// A kept snapshot of other tables, of a format the bank does not read, or none at all can
    /// never run; one the bank may run goes to it (its own errors stop the start).
    #[test]
    fn a_snapshot_that_can_never_run_is_told_apart() {
        let hello = json!({"gameVersion": "2.103", "reads": [0, 1]});
        assert_eq!(never_runs(b"{\"format\":1,\"game_version\":\"2.103\"}\n{\"broken", &hello), None);
        for bad in [&b"{\"format\":1,\"game_version\":\"0.0.1\"}\n{}"[..], b"{\"format\":7,\"game_version\":\"2.103\"}\n{}", b"garbage"] {
            assert!(never_runs(bad, &hello).is_some(), "{}", String::from_utf8_lossy(bad));
        }
    }

    /// Flip 1 timed out and the old bank was told to go on; its late snapshot comes while
    /// flip 2 waits for its own: dropped, and flip 2 takes only the snapshot of its HANDOFF.
    #[test]
    fn a_flip_takes_only_the_snapshot_of_its_own_handoff() {
        let mut front = test_front();
        let (live, mut old) = fake_bank("a");
        let (next, mut new) = fake_bank("b");
        front.live = Some(live);
        front.next = Some(next);
        front.phase = Phase::Starting;
        front.flip = Some(Flip { seq: 2, reason: "test".into(), stopped: None, snapshot: Value::Null, kept: None });
        front.on_next(Frame { kind: ipc::HELLO, n: ipc::PROTOCOL, body: br#"{"format":1,"reads":[0,1]}"#.to_vec(), fd: None });
        assert_eq!(front.phase, Phase::Snapshotting);
        let handoff = old.recv().unwrap();
        assert_eq!((handoff.kind, handoff.n), (ipc::HANDOFF, 2), "HANDOFF carries the flip");
        old.send_fd(ipc::SNAPSHOT, 1, b"{}", Some(snapshot_fd("flip 1")));
        old.send(ipc::SNAPSHOT, 1, &[]);
        front.read_banks();
        assert_eq!(front.phase, Phase::Snapshotting, "flip 1's snapshots are not flip 2's");
        assert!(front.last_snapshot.is_none() && front.flip.is_some());
        old.send_fd(ipc::SNAPSHOT, 2, b"{}", Some(snapshot_fd("flip 2")));
        front.read_banks();
        assert_eq!(front.phase, Phase::Restoring);
        front.flush_banks();
        let restore = new.recv().unwrap();
        assert_eq!((restore.kind, ipc::read_memfd(restore.fd.as_ref().unwrap()).unwrap()), (ipc::RESTORE, b"flip 2".to_vec()));
        // A crash restarts from it only once the new bank restored it.
        assert!(front.last_snapshot.is_none());
        front.on_next(Frame { kind: ipc::READY, n: 0, body: b"{}".to_vec(), fd: None });
        assert_eq!(front.last_snapshot.as_ref().map(|(fd, _)| ipc::read_memfd(fd).unwrap()), Some(b"flip 2".to_vec()));
        let exiting = front.exiting.drain(..).map(|(c, _)| c);
        for mut child in [front.live.take(), front.next.take()].into_iter().flatten().map(|b| b.child).chain(exiting) {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    #[test]
    fn a_bank_of_another_protocol_is_refused_on_every_path() {
        let hello = |kind, n, body: &str| hello_of(&Frame { kind, n, body: body.as_bytes().to_vec(), fd: None });
        assert_eq!(hello(ipc::HELLO, ipc::PROTOCOL, r#"{"version":"1"}"#).unwrap()["version"], "1");
        assert!(hello(ipc::HELLO, 999, r#"{"version":"1"}"#).unwrap_err().contains("front protocol 999"));
        assert!(hello(ipc::READY, ipc::PROTOCOL, "{}").is_err());
        assert!(hello(ipc::HELLO, ipc::PROTOCOL, "not json").is_err());
    }

    #[test]
    fn a_chat_config_error_never_shows_a_value() {
        assert!(chat_config(br#"{"webhooks": ["https://example.com/hook"], "retries": 1}"#).is_ok());
        for (body, field) in [(r#"{"webhooks": "https://hooks.example/t0psecret"}"#, "chat.webhooks"), (r#"{"secret": ["t0psecret"]}"#, "chat.secret"), (r#"{"t0psecret": 1}"#, "chat.t0psecret")] {
            let e = chat_config(body.as_bytes()).unwrap_err();
            assert_eq!(e, format!("chat config unreadable at {field}"));
        }
        assert_eq!(chat_config(b"{\"secret\": \"t0psecret").unwrap_err(), "chat config unreadable (not JSON)");
    }

    #[test]
    fn a_steam_disconnect_names_its_connection_not_the_steam_id() {
        // Client 3 is the same player's new connection: the old one's close is client 2's.
        assert_eq!(disconnected(2, 76561198000000042, &[(3, 76561198000000042)]), [2]);
        assert_eq!(disconnected(-1, 76561198000000042, &[(3, 76561198000000042), (4, 1)]), [3]);
        assert!(disconnected(-1, 5, &[]).is_empty());
    }

    #[test]
    fn loopback_is_trusted_only_when_asked() {
        let local = "127.0.0.1".parse().unwrap();
        assert!(!trusted(local, false), "a relay's players all come from 127.0.0.1");
        assert!(trusted(local, true) && trusted("::ffff:127.0.0.1".parse().unwrap(), true));
        assert!(!trusted("10.0.0.2".parse().unwrap(), true));
    }

    /// Batches past the game's 2.5 MiB go as several, split between messages, so "Split" never
    /// needs two digits; one message over it is an error, not garbage.
    #[test]
    fn steam_batches_fit_the_games_transport() {
        let small = crate::wire::batch(&[(1, vec![7; 100])]);
        assert_eq!(steam_parts(&small).unwrap(), [small.clone()]);
        let big = crate::wire::batch(&[(1, vec![1; 1 << 20]), (2, vec![2; 1 << 20]), (3, vec![3; 1 << 20]), (4, vec![4; 10])]);
        let parts = steam_parts(&big).unwrap();
        assert_eq!(parts.len(), 2);
        assert!(parts.iter().all(|p| p.len() <= MAX_STEAM_MESSAGE && p.len().div_ceil(CHUNK) <= 9));
        let back: Vec<(u32, Vec<u8>)> = parts.iter().flat_map(|p| crate::wire::messages(p).into_iter().map(|(k, b)| (k, b.to_vec())).collect::<Vec<_>>()).collect();
        assert_eq!(back, crate::wire::messages(&big).into_iter().map(|(k, b)| (k, b.to_vec())).collect::<Vec<_>>());
        let world = crate::wire::batch(&[(1, vec![0; 3 << 20])]);
        assert!(steam_parts(&world).unwrap_err().contains("direct IP"));
    }

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
}
