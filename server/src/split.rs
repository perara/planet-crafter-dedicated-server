//! Hot-update spike (docs/hot-update.md): the server as two processes. The front owns the UDP
//! socket and the Unity Transport sessions and stays up; the game process owns the world and
//! the tick. They talk over a unix socket in frames. Every PCDS_SPLIT_SWAP seconds the front
//! starts a new game process and hands the world over to it while the clients stay connected.
//! Direct IP only, no Steam, no admin API, no saving: a measurement rig, not the server.
//!
//!   pcds-server split DATA_DIR SAVE.json
//!     PCDS_LISTEN         default 127.0.0.1:47777 (never the live server's port)
//!     PCDS_SPLIT_SECONDS  run this long, then print the measurements (default 60)
//!     PCDS_SPLIT_SWAP     seconds between handoffs (default 0: none)
//!     PCDS_SPLIT_FAIL     the handoff whose new game dies before it is ready (a rollback)

use std::collections::HashSet;
use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use crate::game::Game;
use crate::idmap::FxHashMap;
use crate::save::SaveFile;
use crate::utp::{Event, Token, Utp};
use crate::wire;

/// Front and game must speak the same frames; a game that says otherwise is not used.
const PROTOCOL: u64 = 1;

// A frame: u32 length of the rest, u8 kind, u64 client (or a number), the body.
const HELLO: u8 = 1; // game -> front: PROTOCOL; body: its game version
const START: u8 = 2; // front -> game: load the save
const RESTORE: u8 = 3; // front -> game: body: a snapshot
const READY: u8 = 4; // game -> front: running
const CONNECTED: u8 = 5; // front -> game: body: the client's IP
const DATA: u8 = 6; // both ways: body: one Netcode batch
const GONE: u8 = 7; // front -> game: the client left; game -> front: drop it
const HANDOFF: u8 = 8; // front -> game: send everything, stop, snapshot
const SNAPSHOT: u8 = 9; // game -> front: body: the snapshot; then it waits for EXIT or RESUME
const EXIT: u8 = 10; // front -> old game: the new one runs
const RESUME: u8 = 11; // front -> old game: the new one failed; go on

type Frame = (u8, u64, Vec<u8>);

struct Link {
    stream: UnixStream,
    inbox: Vec<u8>,
}

impl Link {
    fn send(&mut self, kind: u8, n: u64, body: &[u8]) -> bool {
        let mut f = Vec::with_capacity(13 + body.len());
        f.extend_from_slice(&(9 + body.len() as u32).to_le_bytes());
        f.push(kind);
        f.extend_from_slice(&n.to_le_bytes());
        f.extend_from_slice(body);
        // ponytail: a blocking write; a peer that stops reading stalls the writer. The real
        // front needs a bounded per-game queue and non-blocking writes.
        self.stream.write_all(&f).is_ok()
    }

    /// One read of what is waiting (blocks when nothing is); false once the peer is gone.
    fn fill(&mut self) -> bool {
        let mut buf = [0u8; 1 << 16];
        match self.stream.read(&mut buf) {
            Ok(0) => false,
            Ok(n) => {
                self.inbox.extend_from_slice(&buf[..n]);
                true
            }
            Err(e) => e.kind() == std::io::ErrorKind::Interrupted,
        }
    }

    fn next(&mut self) -> Option<Frame> {
        let n = u32::from_le_bytes(self.inbox.get(..4)?.try_into().ok()?) as usize;
        let f = self.inbox.get(4..4 + n)?;
        let frame = (f[0], u64::from_le_bytes(f[1..9].try_into().ok()?), f[9..].to_vec());
        self.inbox.drain(..4 + n);
        Some(frame)
    }

    /// Waits for one frame; None once the peer is gone.
    fn recv(&mut self) -> Option<Frame> {
        loop {
            if let Some(f) = self.next() {
                return Some(f);
            }
            if !self.fill() {
                return None;
            }
        }
    }

    fn fd(&self) -> RawFd {
        self.stream.as_raw_fd()
    }
}

/// Waits until a descriptor is readable or `timeout` passes; which ones are.
fn wait(fds: &[RawFd], timeout: Duration) -> Vec<bool> {
    let mut p: Vec<libc::pollfd> = fds.iter().map(|fd| libc::pollfd { fd: *fd, events: libc::POLLIN, revents: 0 }).collect();
    let ts = libc::timespec { tv_sec: timeout.as_secs() as libc::time_t, tv_nsec: timeout.subsec_nanos() as libc::c_long };
    // SAFETY: valid pollfds and timespec for the call's duration; no signal mask.
    unsafe { libc::ppoll(p.as_mut_ptr(), p.len() as libc::nfds_t, &ts, std::ptr::null()) };
    p.iter().map(|p| p.revents != 0).collect()
}

fn cpu(who: i32) -> Duration {
    // SAFETY: getrusage fills the struct it is given.
    let mut r: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(who, &mut r) };
    let t = |v: libc::timeval| Duration::from_secs(v.tv_sec as u64) + Duration::from_micros(v.tv_usec as u64);
    t(r.ru_utime) + t(r.ru_stime)
}

// ---- the game process --------------------------------------------------------------------

pub fn game(args: &[String]) {
    let (dir, save_path) = (&args[0], &args[1]);
    // The tables load before the hello: the front hands over only once they are in.
    let (net_data, data, spec) = crate::tables(dir);
    let version = net_data.game_version.clone();
    let stream = UnixStream::connect(std::env::var("PCDS_IPC").expect("PCDS_IPC")).expect("the front's socket");
    let mut link = Link { stream, inbox: Vec::new() };
    link.send(HELLO, PROTOCOL, version.as_bytes());
    let mut game = match link.recv() {
        Some((START, _, _)) => Game::new(net_data, data, spec, SaveFile::load(save_path).expect("the save"), dir),
        Some((RESTORE, _, snapshot)) => {
            if std::env::var_os("PCDS_SPLIT_DIE").is_some() {
                std::process::exit(3);
            }
            match crate::handoff::restore(net_data, data, spec, &String::from_utf8_lossy(&snapshot), dir) {
                Ok(g) => g,
                Err(e) => {
                    eprintln!("split-game: restore failed: {e}");
                    std::process::exit(3);
                }
            }
        }
        _ => return,
    };
    link.send(READY, 0, &[]);
    serve(&mut game, &mut link);
}

/// The server loop's game half (main.rs), its sends going to the front.
fn serve(game: &mut Game, link: &mut Link) {
    // Who got ConnectionApproved: in the world for a restored game.
    let mut approved: HashSet<u64> = game.clients.values().filter(|c| c.approved).map(|c| c.id).collect();
    let (mut mail, mut urgent): (FxHashMap<u64, wire::Batch>, Vec<u64>) = Default::default();
    let (mut next_frame, mut movement_sent) = (Instant::now(), Instant::now());
    loop {
        let frame = Instant::now() >= next_frame;
        if frame {
            next_frame = Instant::now() + crate::FRAME;
        }
        while let Some((kind, client, body)) = link.next() {
            match kind {
                CONNECTED => {
                    let ip = String::from_utf8_lossy(&body).to_string();
                    let trusted = ip.parse::<IpAddr>().is_ok_and(|a| a.is_loopback());
                    game.connect(client, None, trusted, Some(ip));
                }
                DATA if !game.kicks.iter().any(|(c, _)| *c == client) => {
                    let before = game.net.out.len();
                    game.receive(client, &body);
                    if game.net.out[before.min(game.net.out.len())..].iter().any(|(to, _, _)| to.reaches(client)) && !urgent.contains(&client) {
                        urgent.push(client);
                    }
                }
                GONE => {
                    approved.remove(&client);
                    game.disconnect(client);
                }
                HANDOFF => {
                    // A tick boundary with nothing queued: what is waiting goes out first.
                    crate::post(game, &mut approved, &mut mail);
                    let all: Vec<u64> = mail.keys().copied().collect();
                    send(link, &mut mail, &all);
                    let Ok(text) = crate::handoff::snapshot(game).inspect_err(|e| eprintln!("split-game: no snapshot ({e}); going on")) else {
                        link.send(SNAPSHOT, 0, &[]);
                        continue;
                    };
                    link.send(SNAPSHOT, 0, text.as_bytes());
                    match link.recv() {
                        Some((RESUME, _, _)) => println!("split-game {}: resumed, the new game failed", std::process::id()),
                        _ => std::process::exit(0),
                    }
                }
                _ => {}
            }
        }
        if frame {
            game.tick();
        }
        crate::post(game, &mut approved, &mut mail);
        if frame && movement_sent.elapsed() >= crate::MOVEMENT_EVERY {
            movement_sent = Instant::now();
            let all: Vec<u64> = mail.keys().copied().collect();
            send(link, &mut mail, &all);
        } else if !urgent.is_empty() {
            send(link, &mut mail, &urgent);
        }
        urgent.clear();
        for (client, reason) in std::mem::take(&mut game.kicks) {
            send(link, &mut mail, &[client]);
            let units: Vec<u16> = reason.encode_utf16().collect();
            let mut w = wire::Writer::new();
            w.packed_i32(0).u32(units.len() as u32);
            units.iter().for_each(|u| {
                w.u16(*u);
            });
            link.send(DATA, client, &wire::batch(&[(crate::DISCONNECT_REASON, w.0)]));
            link.send(GONE, client, &[]);
            approved.remove(&client);
            game.disconnect(client);
        }
        let pause = next_frame.saturating_duration_since(Instant::now());
        if wait(&[link.fd()], pause)[0] && !link.fill() {
            return; // the front is gone
        }
    }
}

fn send(link: &mut Link, mail: &mut FxHashMap<u64, wire::Batch>, clients: &[u64]) {
    for c in clients {
        if let Some(b) = mail.remove(c).filter(|b| !b.is_empty()) {
            link.send(DATA, *c, &b.finish());
        }
    }
}

// ---- the front ---------------------------------------------------------------------------

struct Proc {
    child: Child,
    link: Option<Link>,
}

#[derive(PartialEq, Debug)]
enum Phase {
    Idle,
    /// The new game is loading its tables.
    Starting,
    /// The old game was told to stop; the clients' frames wait.
    Snapshotting,
    /// The new game has the snapshot.
    Restoring,
}

pub fn front(args: &[String]) {
    let (dir, save) = (args[0].clone(), args[1].clone());
    let var = |k: &str, d: f64| std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d);
    let listen: SocketAddr = std::env::var("PCDS_LISTEN").unwrap_or_else(|_| "127.0.0.1:47777".into()).parse().expect("PCDS_LISTEN");
    let (seconds, swap_every, fail_at) = (var("PCDS_SPLIT_SECONDS", 60.0), var("PCDS_SPLIT_SWAP", 0.0), var("PCDS_SPLIT_FAIL", 0.0) as u32);
    let mut utp = Utp::bind(listen).unwrap_or_else(|e| panic!("UDP {listen}: {e}"));
    let path = std::env::temp_dir().join(format!("pcds-split-{}.sock", std::process::id()));
    let listener = UnixListener::bind(&path).expect("the game socket");
    let spawn = |die: bool| {
        let mut c = Command::new(std::env::current_exe().unwrap());
        c.args(["split-game", &dir, &save]).env("PCDS_IPC", &path);
        if die {
            c.env("PCDS_SPLIT_DIE", "1");
        }
        Proc { child: c.spawn().expect("a game process"), link: None }
    };
    // The first game, waited for: nobody is connected yet.
    let started = Instant::now();
    let mut live = spawn(false);
    let mut l = Link { stream: listener.accept().expect("the first game").0, inbox: Vec::new() };
    assert!(matches!(l.recv(), Some((HELLO, PROTOCOL, _))), "the first game speaks another protocol");
    l.send(START, 0, &[]);
    assert!(matches!(l.recv(), Some((READY, _, _))), "the first game did not start");
    live.link = Some(l);
    listener.set_nonblocking(true).unwrap();
    println!("split: front on {listen}, game {} ready after {:?}", live.child.id(), started.elapsed());

    let (mut clients, mut tokens): (FxHashMap<u64, Token>, FxHashMap<Token, u64>) = Default::default();
    let mut next_client = 1u64;
    let (mut phase, mut next, mut held): (Phase, Option<Proc>, Vec<Frame>) = (Phase::Idle, None, Vec::new());
    let version = live_version(&dir);
    let (mut since_swap, mut t0, mut handoffs, mut pauses, mut failed) = (Instant::now(), Instant::now(), 0u32, Vec::<Duration>::new(), 0u32);
    let (mut frames_in, mut frames_out, mut snapshot_len) = (0u64, 0u64, 0usize);
    while started.elapsed().as_secs_f64() < seconds {
        let mut fds = vec![utp.fd(), live.link.as_ref().unwrap().fd()];
        fds.extend(next.as_ref().and_then(|n| n.link.as_ref()).map(Link::fd));
        let readable = wait(&fds, Duration::from_millis(5));
        for event in utp.poll() {
            let frame = match event {
                Event::Connected(token, addr) => {
                    let c = next_client;
                    next_client += 1;
                    clients.insert(c, token);
                    tokens.insert(token, c);
                    (CONNECTED, c, addr.ip().to_string().into_bytes())
                }
                Event::Data(token, batch) => match tokens.get(&token) {
                    Some(c) => (DATA, *c, batch),
                    None => continue,
                },
                Event::Disconnected(token) => match tokens.remove(&token) {
                    Some(c) => {
                        clients.remove(&c);
                        (GONE, c, Vec::new())
                    }
                    None => continue,
                },
            };
            frames_in += 1;
            if matches!(phase, Phase::Snapshotting | Phase::Restoring) {
                held.push(frame);
            } else {
                live.link.as_mut().unwrap().send(frame.0, frame.1, &frame.2);
            }
        }
        if readable[1] && !live.link.as_mut().unwrap().fill() {
            panic!("the game process died"); // ponytail: the real front restarts it from the last snapshot or save
        }
        while let Some((kind, c, body)) = live.link.as_mut().unwrap().next() {
            match kind {
                DATA => {
                    frames_out += 1;
                    if let Some(token) = clients.get(&c) {
                        utp.send(token, &body);
                    }
                }
                GONE => {
                    if let Some(token) = clients.remove(&c) {
                        tokens.remove(&token);
                        utp.flush();
                        utp.disconnect(&token);
                    }
                }
                SNAPSHOT if phase == Phase::Snapshotting && body.is_empty() => {
                    // The old game could not write one and went on (a RESUME is ignored there).
                    failed += fail(&mut phase, &mut next, &mut live, &mut held, t0, "the old game wrote no snapshot");
                }
                SNAPSHOT if phase == Phase::Snapshotting => {
                    snapshot_len = body.len();
                    next.as_mut().unwrap().link.as_mut().unwrap().send(RESTORE, 0, &body);
                    phase = Phase::Restoring;
                }
                _ => {}
            }
        }
        // The next game: its connection, its hello, its ready.
        if let Some(n) = next.as_mut().filter(|n| n.link.is_none()) {
            if let Ok((s, _)) = listener.accept() {
                s.set_nonblocking(false).unwrap();
                n.link = Some(Link { stream: s, inbox: Vec::new() });
            }
        }
        if readable.get(2) == Some(&true) && !next.as_mut().unwrap().link.as_mut().unwrap().fill() {
            failed += fail(&mut phase, &mut next, &mut live, &mut held, t0, "the new game exited");
        }
        while let Some((kind, n, body)) = next.as_mut().and_then(|p| p.link.as_mut()).and_then(Link::next) {
            match kind {
                HELLO if n != PROTOCOL => failed += fail(&mut phase, &mut next, &mut live, &mut held, t0, &format!("it speaks protocol {n}, not {PROTOCOL}")),
                HELLO if body != version => failed += fail(&mut phase, &mut next, &mut live, &mut held, t0, "it has other game tables"),
                HELLO => {
                    t0 = Instant::now();
                    live.link.as_mut().unwrap().send(HANDOFF, 0, &[]);
                    phase = Phase::Snapshotting;
                }
                READY => {
                    let pause = t0.elapsed();
                    let mut old = std::mem::replace(&mut live, next.take().unwrap());
                    old.link.as_mut().unwrap().send(EXIT, 0, &[]);
                    let _ = old.child.wait();
                    let l = live.link.as_mut().unwrap();
                    let n = held.len();
                    held.drain(..).for_each(|(k, c, b)| {
                        l.send(k, c, &b);
                    });
                    pauses.push(pause);
                    println!("split: handoff {handoffs}: game {} -> {}, the world stood {pause:?}, snapshot {snapshot_len} bytes, {n} client frames held", old.child.id(), live.child.id());
                    phase = Phase::Idle;
                }
                _ => {}
            }
        }
        if phase == Phase::Restoring && t0.elapsed() > Duration::from_secs(5) {
            failed += fail(&mut phase, &mut next, &mut live, &mut held, t0, "not ready within 5 s");
        }
        if swap_every > 0.0 && phase == Phase::Idle && since_swap.elapsed().as_secs_f64() >= swap_every {
            since_swap = Instant::now();
            handoffs += 1;
            next = Some(spawn(handoffs == fail_at));
            phase = Phase::Starting;
        }
        utp.flush();
    }
    let _ = live.child.kill();
    let _ = live.child.wait();
    let _ = std::fs::remove_file(&path);
    let max = pauses.iter().max().copied().unwrap_or_default();
    println!(
        "split: {seconds} s: {frames_in} frames to the game, {frames_out} batches back; {} handoffs (the world stood at most {max:?}), {failed} failed and rolled back; cpu: front {:?}, games {:?}",
        pauses.len(),
        cpu(libc::RUSAGE_SELF),
        cpu(libc::RUSAGE_CHILDREN)
    );
}

/// The new game failed: the old one goes on, with what was held for the new one. Returns 1.
fn fail(phase: &mut Phase, next: &mut Option<Proc>, live: &mut Proc, held: &mut Vec<Frame>, began: Instant, why: &str) -> u32 {
    if let Some(mut n) = next.take() {
        let _ = n.child.kill();
        let _ = n.child.wait();
    }
    let l = live.link.as_mut().unwrap();
    if *phase != Phase::Starting {
        l.send(RESUME, 0, &[]);
        held.drain(..).for_each(|(k, c, b)| {
            l.send(k, c, &b);
        });
        println!("split: handoff failed ({why}); the old game resumed, the world stood {:?}", began.elapsed());
    } else {
        println!("split: handoff refused ({why}); the old game never stopped");
    }
    *phase = Phase::Idle;
    1
}

/// The game version the front's tables are for: a game on other tables is not handed the world.
fn live_version(dir: &str) -> Vec<u8> {
    crate::network_tables(dir).0.game_version.into_bytes()
}

/// The unix socket's cost alone, between two processes, in the frames above:
///   cargo test --release split::tests::ipc -- --ignored --nocapture
#[cfg(test)]
mod tests {
    use super::*;

    /// The other process: echoes every frame until the socket closes; then reports its CPU.
    #[test]
    #[ignore]
    fn ipc_echo() {
        let Some(path) = std::env::var_os("PCDS_IPC_ECHO") else { return };
        let mut l = Link { stream: UnixStream::connect(path).unwrap(), inbox: Vec::new() };
        let start = cpu(libc::RUSAGE_SELF);
        loop {
            // As the game waits: in ppoll until its next frame, woken by the front's frame.
            if !wait(&[l.fd()], Duration::from_millis(5))[0] {
                continue;
            }
            if !l.fill() {
                break;
            }
            while let Some((kind, n, body)) = l.next() {
                if kind == EXIT {
                    l.send(EXIT, (cpu(libc::RUSAGE_SELF) - start).as_nanos() as u64, &[]);
                    return;
                }
                l.send(kind, n, &body);
            }
        }
    }

    fn percentile(v: &mut [Duration], p: f64) -> Duration {
        v.sort();
        v[((v.len() - 1) as f64 * p).round() as usize]
    }

    #[test]
    #[ignore]
    fn ipc() {
        let path = std::env::temp_dir().join(format!("pcds-ipc-{}.sock", std::process::id()));
        let listener = UnixListener::bind(&path).unwrap();
        let mut child = Command::new(std::env::current_exe().unwrap()).args(["split::tests::ipc_echo", "--ignored", "--exact", "--nocapture"]).env("PCDS_IPC_ECHO", &path).stdout(std::process::Stdio::null()).spawn().unwrap();
        let mut l = Link { stream: listener.accept().unwrap().0, inbox: Vec::new() };
        std::fs::remove_file(&path).unwrap();
        // A client's transform batch is about 40 bytes, an answer batch a few hundred.
        let body = vec![7u8; 120];
        let round = |l: &mut Link| {
            let t = Instant::now();
            l.send(DATA, 1, &body);
            loop {
                if let Some(_) = l.next() {
                    return t.elapsed();
                }
                wait(&[l.fd()], Duration::from_millis(5));
                assert!(l.fill());
            }
        };
        let mut burst = Vec::new();
        let c0 = cpu(libc::RUSAGE_SELF);
        for _ in 0..20_000 {
            burst.push(round(&mut l));
        }
        let burst_cpu = cpu(libc::RUSAGE_SELF) - c0;
        // At the load test's pace and above: a round trip every 1 ms (8 players make ~700
        // frames a second in all), each side asleep in between as the server loops are.
        let mut paced = Vec::new();
        let c1 = cpu(libc::RUSAGE_SELF);
        let started = Instant::now();
        for i in 0..10_000u32 {
            let due = started + Duration::from_millis(i as u64);
            std::thread::sleep(due.saturating_duration_since(Instant::now()));
            paced.push(round(&mut l));
        }
        let paced_cpu = cpu(libc::RUSAGE_SELF) - c1;
        // A snapshot the size of the live world's (2-4 MB) through the same socket.
        let snapshot = vec![b'x'; 4 << 20];
        let t = Instant::now();
        l.send(SNAPSHOT, 0, &snapshot);
        while l.next().is_none() {
            assert!(l.fill());
        }
        let bulk = t.elapsed();
        l.send(EXIT, 0, &[]);
        let child_cpu = loop {
            match l.recv() {
                Some((EXIT, ns, _)) => break Duration::from_nanos(ns),
                Some(_) => continue,
                None => panic!("echo process gone"),
            }
        };
        child.wait().unwrap();
        println!("ipc: back to back, 20000 round trips of a 120-byte frame: p50 {:?}, p99 {:?}, max {:?}; this side {:?} cpu per round trip", percentile(&mut burst, 0.5), percentile(&mut burst, 0.99), percentile(&mut burst, 1.0), burst_cpu / 20_000);
        println!("ipc: paced, a round trip every ms for 10 s: p50 {:?}, p99 {:?}, max {:?}; this side {:?} cpu per round trip (with the timer wakeup)", percentile(&mut paced, 0.5), percentile(&mut paced, 0.99), percentile(&mut paced, 1.0), paced_cpu / 10_000);
        println!("ipc: 4 MiB snapshot there and back: {bulk:?}; the echo side's cpu over everything: {child_cpu:?} for {} frames ({:?} per frame)", 30_001, child_cpu / 30_001);
    }
}
