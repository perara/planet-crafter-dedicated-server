//! The front: the server's long-lived process, the bootloader of docs/hot-update.md. It owns
//! what a client notices and what must outlive a game process: the UDP socket and every Unity
//! Transport session (utp.rs), the Steam game server login, its listen socket, every Steam
//! connection and the invite code, the client ids, the delayed close of kicked clients, the
//! admin HTTP listener and web page, the chat hooks (front/chat_hooks.rs) and the autosave timer.
//! The world runs in a bank process (bank/) started from the active bank's binary; the front
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
//!     PCDS_HOME         (pcds run sets it) the installation: server.env, rules.json and
//!                       access.json in it, the banks in bin/ (a/, b/, active); without it both
//!                       banks are this program (a checkout build), and there is nothing to update

//!
//! The front is a thin composition of small modules, each with one job and a documented
//! interface (docs/architecture.md has the map):
//!
//! | module | what it does |
//! |---|---|
//! | `transport` (`transport/steam`) | the clients' connections over direct IP and Steam: one table of client ids, send, kick, close |
//! | `clients` | relays between the transport and the live bank (CONNECTED, DATA, GONE) |
//! | `queue` | the frames held for a bank and the budgets that keep a stalled bank from taking the memory |
//! | `bank_link` | the banks as processes on sockets: start, boot, read, and route each frame kind |
//! | `flip` | the state machine: a flip to another bank, its phases and transitions |
//! | `supervise` | the watchdog, the crash window, restarts and giving a release up |
//! | `snapshot` | the last flip's snapshot, kept for a world that is in no bank |
//! | `updates`, `history` | looking up, downloading and flipping to a release; the history of it |
//! | `world_control` | stop, start and switch the world |
//! | `ops` | the rules the bank sends the front (OPS): the autosave timer, the update schedule, the watchdog |
//! | `status` | the front's part of GET /api/status |
//! | `admin_routes` | the table of the admin API and the front's answers |
//! | `chat_hooks` | the chat feed, its worker (webhooks, exec) and its endpoints |

use std::net::SocketAddr;
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::process::Child;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use crate::ipc;
use crate::utp::Utp;

mod admin_routes;
mod bank_link;
mod chat_hooks;
mod clients;
mod flip;
mod history;
mod metrics;
mod ops;
mod prometheus;
#[cfg(any(fuzzing, pcds_corpus))]
pub use prometheus::fuzz as fuzz_prom;
mod queue;
mod snapshot;
mod status;
mod supervise;
mod transport;
mod updates;
mod world_control;

pub use admin_routes::{find_http, is_open, Http, Owner};
pub use chat_hooks::{api as chat_api, Feed, Hooks};
pub use metrics::{cgroup_memory, flip_memory, process_of};
use admin_routes::Admin;
use bank_link::Bank;
use flip::{Flip, Phase};
use ops::Autosave;
use queue::Queue;
use supervise::{CrashWindow, Watchdog};
use transport::Transport;
use updates::Updates;

/// At a stop, the live bank gets this long to take STOP and save, then as long again to end,
/// before it is killed: 60 s in all, under the supervisor's 20 s for orphans (cli/setup.rs
/// BANKS_GRACE) and systemd's TimeoutStopSec=90 (and Docker's --stop-timeout 90), so
/// systemd's or Docker's kill comes last.
pub const STOP_GRACE: Duration = Duration::from_secs(30);

/// The front's exit code when no bank could start the world at its boot: `pcds run` then
/// starts the other bank's own front (a release whose front or bank cannot run, cli/setup.rs).
pub const BOOT_FAILED: i32 = 75;

/// A new bank's tables and world load in seconds; the restore must be READY within this.
pub const LOAD_DEADLINE: Duration = Duration::from_secs(120);

/// The front's frame while it is quiet (Front::quiet): a request on the admin API waits for it.
const IDLE_FRAME: Duration = Duration::from_millis(20);
/// Kicked: the reason first, the connection closed this much later.
const KICK_DELAY: Duration = Duration::from_millis(300);
/// More crashes than this within CRASH_WINDOW and the front gives up (systemd restarts it).
const MAX_CRASHES: usize = 5;
const CRASH_WINDOW: Duration = Duration::from_secs(600);
const RESTARTED: &str = "The server restarted after an error: join again";
const STOPPED: &str = "The world is stopped: join again later";

struct Front {
    dir: String,
    save: String,
    banks: Option<PathBuf>,
    /// The clients' connections.
    net: Transport,
    /// Frames for the bank while the world stands, and the budgets of what waits for it.
    queue: Queue,
    live: Option<Bank>,
    next: Option<Bank>,
    phase: Phase,
    flip: Option<Flip>,
    /// The last snapshot a flip carried: a crashed bank starts from it when it is newer than
    /// the save.
    last_snapshot: Option<(OwnedFd, SystemTime)>,
    /// What a recovering bank starts from: the snapshot (else the save).
    recover_from: Option<OwnedFd>,
    crashes: CrashWindow,
    /// The bank a flip (an update, a rollback) made the live one in this front's life; None
    /// when the live bank is the one the front started with. A release that ends the game over
    /// and over is given up (give_up) only when it came by a flip.
    flipped: Option<String>,
    watch: Watchdog,
    exiting: Vec<(Child, Instant)>,
    /// Writes of the banks folder's files that a flip made (they flush to disk, which takes as
    /// long as the disk does): off the front's thread, joined at shutdown.
    persisting: Vec<std::thread::JoinHandle<()>>,
    admin: Admin,
    hooks: Hooks,
    autosave: Autosave,
    updates: Updates,
    started: Instant,
    stop: Arc<AtomicBool>,
    game_version: String,
    flips: u64,
    /// Counters for GET /api/metrics.
    totals: metrics::Totals,
    /// The lock that makes this the one server of its world (released by a switch).
    world_lock: Option<std::fs::File>,
}

/// The banks' folder: bin/ of PCDS_HOME, which pcds run gives the front. Without one (a front
/// started by hand, the tests') both banks are this program.
fn banks_dir() -> Option<PathBuf> {
    crate::env("PCDS_HOME").map(|home| PathBuf::from(home).join("bin")).filter(|dir| dir.is_dir())
}

fn ms(d: Duration) -> f64 {
    (d.as_secs_f64() * 10_000.0).round() / 10.0
}

pub fn run(args: &[String]) {
    let [dir, save] = args else {
        error!("usage: pcds serve DATA_DIR SAVE.json (settings are environment variables: docs/env.md; 'pcds run' is what the service runs)");
        std::process::exit(2);
    };
    crate::logging::start("front", &crate::config::installation_file(&crate::env, save, "rules.json"));
    // Settings that are not what they should be stop the start, before anything is opened.
    if let Err(e) = crate::config::check_env(crate::env) {
        error!("{e}");
        std::process::exit(2);
    }
    // One server per world, for this front's life (released when the process ends).
    let world = crate::util::hold(std::path::Path::new(&format!("{save}.lock")), "another server runs this world").unwrap_or_else(|e| {
        error!("{e}");
        std::process::exit(1)
    });
    // SAFETY: set before any other thread exists; the Steam API reads it during init.
    unsafe { std::env::set_var("SteamAppId", transport::steam::APP_ID) };
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    ctrlc::set_handler(move || flag.store(true, Ordering::SeqCst)).expect("signal handler");

    let listen = crate::env("PCDS_LISTEN").unwrap_or_else(|| "0.0.0.0:7777".into());
    let utp = match listen.as_str() {
        "off" => None,
        addr => {
            let addr: SocketAddr = addr.parse().expect("PCDS_LISTEN: host:port");
            let utp = Utp::bind(addr).unwrap_or_else(|e| {
                error!("direct IP {addr}: {e}. Another program (a second pcds server?) may use that port: change PCDS_LISTEN in server.env ('pcds config'), or set it to off");
                std::process::exit(1)
            });
            info!("direct IP: listening on {addr}");
            Some(utp)
        }
    };
    let port = listen.rsplit(':').next().and_then(|p| p.parse::<u16>().ok()).unwrap_or(0);
    let mut front = Front::new(dir, save, banks_dir(), utp, port, stop);
    front.world_lock = Some(world);
    front.boot();
    front.start_admin();
    front.start_steam();
    front.serve();
}

impl Front {
    fn new(dir: &str, save: &str, banks: Option<PathBuf>, utp: Option<Utp>, port: u16, stop: Arc<AtomicBool>) -> Front {
        // The rules' values (save, update) until the bank says what the rules file holds (OPS).
        let rules = crate::rules::Rules::default();
        Front {
            dir: dir.to_string(),
            save: save.to_string(),
            banks,
            net: Transport::new(utp, port),
            queue: Queue::new(),
            live: None,
            next: None,
            phase: Phase::Idle,
            flip: None,
            last_snapshot: None,
            recover_from: None,
            crashes: CrashWindow::default(),
            flipped: None,
            watch: Watchdog::new(u64::from(rules.limits.bank_watchdog_seconds)),
            exiting: Vec::new(),
            persisting: Vec::new(),
            admin: Admin::new(),
            hooks: Hooks::default(),
            autosave: Autosave::new(u64::from(rules.save.autosave_minutes)),
            updates: Updates::new(),
            started: Instant::now(),
            stop,
            game_version: String::new(),
            flips: 0,
            totals: Default::default(),
            world_lock: None,
        }
    }

    fn start_admin(&mut self) {
        if let Err(e) = self.admin.start(self.hooks.feed.clone(), &self.save) {
            error!("{e}");
            self.shutdown(1);
        }
    }

    fn start_steam(&mut self) {
        if let Err(e) = self.net.start_steam(&self.game_version) {
            error!("{e}");
            self.shutdown(1);
        }
    }

    /// Nobody connected and nothing under way: only a new connection, a bank frame or an admin
    /// request can come, and none of them needs the 5 ms frame.
    fn quiet(&self) -> bool {
        self.net.conns.is_empty() && self.next.is_none() && self.exiting.is_empty() && self.net.pending_drop.is_empty() && self.queue.held.is_empty() && matches!(self.phase, Phase::Idle | Phase::Stopped)
    }

    fn serve(&mut self) {
        let mut frame_at = Instant::now().checked_sub(IDLE_FRAME).unwrap_or_else(Instant::now);
        while !self.stop.load(Ordering::SeqCst) {
            let every = if self.quiet() { IDLE_FRAME } else { crate::FRAME };
            let frame = Instant::now() >= frame_at + every;
            if frame {
                frame_at = Instant::now();
                self.poll_steam();
            }
            let mut sent = self.poll_utp();
            sent |= self.read_banks();
            self.admin_requests();
            for reply in self.hooks.replies() {
                self.to_bank(ipc::SAY, 0, reply.as_bytes(), 0);
            }
            if self.autosave.due(Instant::now(), self.live.is_some()) {
                self.to_bank(ipc::SAVE, 0, &[], 0);
            }
            self.updates();
            self.supervise();
            sent |= self.drops();
            self.flush_banks();
            self.release_memory();
            if self.phase == Phase::Restoring
                && let (Some(flip), Some(n)) = (self.flip.as_mut(), self.next.as_ref())
                && flip.relay_at == 0
                && !n.link.pending()
            {
                flip.relay_at = ipc::now_ns();
            }
            self.net.flush(frame || sent);
            let mut fds = Vec::new();
            let mut out = Vec::new();
            if let Some(fd) = self.net.fd() {
                fds.push(fd);
                out.push(false);
            }
            for b in [&self.live, &self.next].into_iter().flatten() {
                fds.push(b.link.fd());
                out.push(b.link.pending());
            }
            let every = if self.quiet() { IDLE_FRAME } else { crate::FRAME };
            ipc::wait(&fds, &out, (frame_at + every).saturating_duration_since(Instant::now()));
        }
        self.shutdown(0);
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
            error!("{e}");
            code = code.max(1);
        }
        for (mut child, _) in self.exiting.drain(..) {
            let _ = child.kill();
            let _ = child.wait();
        }
        for t in self.persisting.drain(..) {
            let _ = t.join();
        }
        self.net.disconnect_all();
        std::process::exit(code)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::Link;
    use std::os::unix::net::UnixStream;
    use std::process::Command;

    /// A bank on one end of a socket pair (a sleeping process stands in for its program).
    pub(super) fn fake_bank(name: &str) -> (Bank, Link) {
        let (ours, theirs) = UnixStream::pair().unwrap();
        ours.set_nonblocking(true).unwrap();
        let child = Command::new("sleep").arg("30").spawn().unwrap();
        (Bank { name: name.into(), child, link: Link::new(ours), hello: None, started: Instant::now(), heard: Instant::now() }, Link::new(theirs))
    }

    pub(super) fn test_front() -> Front {
        Front::new("data", "/nonexistent/world.json", None, None, 0, Arc::new(AtomicBool::new(false)))
    }

    pub(super) fn snapshot_fd(text: &str) -> OwnedFd {
        ipc::memfd("test", |w| std::io::Write::write_all(w, text.as_bytes()).map_err(|e| e.to_string())).unwrap()
    }

    /// The slow frame is for a front with nobody connected and nothing under way only.
    #[test]
    fn the_front_is_quiet_only_with_nobody_connected_and_nothing_under_way() {
        let mut front = test_front();
        assert!(front.quiet());
        front.net.conns.insert(1, transport::Conn::Ip([0; 8]));
        assert!(!front.quiet());
        front.net.conns.clear();
        front.net.pending_drop.push((1, Instant::now()));
        assert!(!front.quiet());
        front.net.pending_drop.clear();
        front.queue.held.push((ipc::DATA, 1, vec![], 1));
        assert!(!front.quiet());
        front.queue.held.clear();
        for phase in [Phase::Starting, Phase::Snapshotting, Phase::Restoring, Phase::Recovering] {
            front.phase = phase;
            assert!(!front.quiet());
        }
        front.phase = Phase::Stopped;
        assert!(front.quiet());
        let (bank, _theirs) = fake_bank("b");
        front.next = Some(bank);
        assert!(!front.quiet());
    }
}

#[cfg(any(fuzzing, pcds_corpus))]
#[path = "../../fuzz/targets/front.rs"]
pub mod fuzz;
