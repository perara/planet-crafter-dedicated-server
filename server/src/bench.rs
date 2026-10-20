//! Tick benchmark on a real save, with a fixed clock and seed, and a fingerprint of everything
//! the server sends and saves (an optimization must leave the fingerprint unchanged):
//!
//!   PCDS_BENCH_SAVE=world.json PCDS_RNG_SEED=1 [PCDS_BENCH_SECONDS=300] [PCDS_BENCH_PLAYERS=4] \
//!     [PCDS_BENCH_ACTION_MS=200] [PCDS_BENCH_DUMP=messages.txt] [PCDS_BENCH_START_DAYS=400] cargo test --release bench -- --ignored --nocapture
//!
//! PCDS_BENCH_START_DAYS starts the game's clock that many days in (a server that has run that long: every
//! timer is set at that age): the save hash printed at the end must not depend on it (docs/clock.md).
//!
//! PCDS_BENCH_DUMP writes every message (the joins', then per tick, sorted) and the final save, to compare two
//! runs (two builds, two table sets) line by line.
//!
//! The loop steps like the server's (a tick every 5 ms); players walk in circles, sending a
//! transform every network tick. With PCDS_BENCH_ACTION_MS they also act that often, as the
//! load test's players do (load.rs): craft, drop, pick up, build, eat, drink, mine, chat, sort.
//! "cpu at real time" is the share of one core the ticks would use running live.
//!
//! PCDS_BENCH_SECTIONS=1 also prints the time per subsystem (the `section!` names in the tick; "scene" holds
//! timers, gauges, units, energy and hooks' parts; "receive" and "act" are the bench's players' packets and
//! RPCs). The clock reads add a little to each tick: use it to see where time goes, not for the total.
//!
//! `handoff` (the same variables, plus PCDS_BENCH_HANDOFF=seconds, default half the run) runs
//! the loop twice: straight through, and with the game snapshotted and rebuilt from the
//! snapshot every that many seconds (handoff.rs). The two fingerprints must be equal. It then
//! puts in, one part at a time, what a game loaded from the save alone has, to show which
//! runtime state a handoff must carry (docs/hot-update.md).

use std::hash::{Hash, Hasher};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::data::stable_hash;
use crate::game::{tests::join, Game};
use crate::protocol::{NETWORK_TRANSFORM, RPC};
use crate::save::SaveFile;
use crate::wire::Writer;

/// Counts this thread's allocations (calls, bytes), for the bench's "allocations per tick". With
/// PCDS_BENCH_ALLOC_SITES=N it also looks at every Nth allocation of the ticks: the frame of the
/// server's code it comes from (see `alloc_sites`).
struct Counting;
thread_local!(static ALLOCS: std::cell::Cell<(u64, u64)> = const { std::cell::Cell::new((0, 0)) });
thread_local!(static SAMPLING: std::cell::Cell<(u64, u64, bool)> = const { std::cell::Cell::new((0, 0, false)) });
thread_local!(static SITES: RefCell<HashMap<String, u64>> = RefCell::new(HashMap::new()));

fn sample_site() {
    // The backtrace allocates: not looked at again while it is made.
    SAMPLING.set((SAMPLING.get().0, SAMPLING.get().1, true));
    let trace = std::backtrace::Backtrace::force_capture().to_string();
    let site = trace.lines().filter(|l| l.contains("pcds::") && !l.contains("pcds::bench::")).map(str::trim).take(3).collect::<Vec<_>>().join(" < ");
    SITES.with(|m| *m.borrow_mut().entry(site).or_default() += 1);
    SAMPLING.set((SAMPLING.get().0, SAMPLING.get().1, false));
}

// SAFETY: every call goes to the system allocator; the counters are plain thread-local cells.
unsafe impl std::alloc::GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: std::alloc::Layout) -> *mut u8 {
        let _ = ALLOCS.try_with(|a| a.set((a.get().0 + 1, a.get().1 + l.size() as u64)));
        let _ = SAMPLING.try_with(|s| {
            let (every, n, busy) = s.get();
            if every > 0 && !busy {
                s.set((every, n + 1, busy));
                if (n + 1) % every == 0 {
                    sample_site();
                }
            }
        });
        unsafe { std::alloc::System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: std::alloc::Layout) {
        unsafe { std::alloc::System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: std::alloc::Layout, n: usize) -> *mut u8 {
        let _ = ALLOCS.try_with(|a| a.set((a.get().0 + 1, a.get().1 + n as u64)));
        unsafe { std::alloc::System.realloc(p, l, n) }
    }
}

#[global_allocator]
static COUNTING: Counting = Counting;

/// The allocations this thread has made so far (what the tests of the efficiency budgets count between two points).
pub fn allocations() -> u64 {
    ALLOCS.get().0
}

/// The user instructions this thread has executed, from the kernel's performance counters
/// (perf_event_open): they do not depend on how busy the machine is, which a time does. None
/// where the kernel does not allow the process to count (perf_event_paranoid, a container, a VM
/// without counters). A hybrid CPU has one counter per core type; the thread moves between them.
struct Instr(Vec<i32>);

impl Instr {
    fn open() -> Option<Instr> {
        Instr::open_with(false)
    }

    /// Counting the threads this thread starts from now on as well (read once they have ended).
    fn open_inherit() -> Option<Instr> {
        Instr::open_with(true)
    }

    fn open_with(inherit: bool) -> Option<Instr> {
        // The performance monitoring units: one, or on a hybrid CPU one per core type.
        let pmu = |name: &str| std::fs::read_to_string(format!("/sys/devices/{name}/type")).ok()?.trim().parse::<u64>().ok();
        let units: Vec<u64> = if pmu("cpu_core").is_some() { ["cpu_core", "cpu_atom"].iter().filter_map(|n| pmu(n)).collect() } else { vec![0] };
        let mut fds = Vec::new();
        for unit in units {
            // perf_event_attr: type (HARDWARE), size, config (INSTRUCTIONS, with the unit's number above bit 32 on
            // a hybrid CPU), then the flags word: exclude_kernel (bit 5) and exclude_hv (bit 6).
            let mut attr = [0u64; 16];
            attr[0] = 128 << 32;
            attr[1] = 1 | (unit << 32);
            attr[5] = (1 << 5) | (1 << 6) | (u64::from(inherit) << 1);
            // SAFETY: the kernel reads 128 bytes of a live attribute block; pid 0 is this thread, no cpu, no group.
            let fd = unsafe { libc::syscall(libc::SYS_perf_event_open, attr.as_ptr(), 0, -1, -1, 8) } as i32;
            if fd < 0 {
                return None;
            }
            fds.push(fd);
        }
        Some(Instr(fds))
    }

    fn read(&self) -> u64 {
        self.0
            .iter()
            .map(|fd| {
                let mut count = 0u64;
                // SAFETY: reads 8 bytes into a live u64.
                let n = unsafe { libc::read(*fd, (&mut count as *mut u64).cast(), 8) };
                if n == 8 { count } else { 0 }
            })
            .sum()
    }
}

impl Drop for Instr {
    fn drop(&mut self) {
        // SAFETY: closes descriptors this value opened.
        self.0.iter().for_each(|fd| unsafe {
            libc::close(*fd);
        });
    }
}

static SECTIONS: AtomicBool = AtomicBool::new(false);
thread_local!(static TIMES: RefCell<HashMap<&'static str, Duration>> = RefCell::new(HashMap::new()));

/// `section!`'s clock: only while PCDS_BENCH_SECTIONS is on.
pub fn start() -> Option<Instant> {
    SECTIONS.load(Ordering::Relaxed).then(Instant::now)
}

pub fn stop(name: &'static str, started: Option<Instant>) {
    if let Some(t) = started {
        TIMES.with(|m| *m.borrow_mut().entry(name).or_default() += t.elapsed());
    }
}

fn fingerprint(h: &mut impl Hasher, out: &mut Vec<(crate::net::Target, u32, Vec<u8>)>) -> (usize, u64, Vec<String>) {
    // Within one tick the order of independent messages may follow hash-map order: sorted.
    let mut tick: Vec<String> = out.drain(..).map(|(to, kind, body)| format!("{to:?} {kind} {body:02x?}")).collect();
    tick.sort();
    tick.hash(h);
    let mut own = std::collections::hash_map::DefaultHasher::new();
    tick.hash(&mut own);
    (tick.len(), own.finish(), tick)
}

struct Env {
    path: String,
    dir: &'static str,
    seconds: f64,
    players: u64,
    action_ms: u64,
    /// `bench` only: where every tick's messages and the final save go.
    dump: Option<String>,
    /// The game's clock at the start, in seconds (PCDS_BENCH_START_DAYS: a server that has run that long).
    start: f64,
}

fn env() -> Option<Env> {
    let path = std::env::var("PCDS_BENCH_SAVE").expect("bench: set PCDS_BENCH_SAVE");
    let dir = crate::test_data()?;
    let var = |k: &str| std::env::var(k).ok().and_then(|v| v.parse::<f64>().ok());
    Some(Env { path, dir, seconds: var("PCDS_BENCH_SECONDS").unwrap_or(300.0), players: var("PCDS_BENCH_PLAYERS").unwrap_or(0.0) as u64, action_ms: var("PCDS_BENCH_ACTION_MS").unwrap_or(0.0) as u64, dump: std::env::var("PCDS_BENCH_DUMP").ok(), start: var("PCDS_BENCH_START_DAYS").unwrap_or(0.0) * 86_400.0 })
}

fn load(e: &Env) -> Game {
    let (net_data, data, spec) = crate::tables(e.dir);
    let scenes = crate::scene_tables(e.dir, &net_data, &data);
    Game::with_clock(net_data, data, spec, SaveFile::load(&e.path).unwrap(), scenes, Some(e.start))
}

/// A bench player's own bookkeeping (what load.rs's bot learns from the server's answers).
#[derive(Default)]
struct Player {
    dropped: Vec<i32>,
    builds: u32,
    mining: bool,
    rng: u64,
}

/// An RPC from a client, on the scene singleton of its class or its own player's behaviour.
fn call(g: &mut Game, client: u64, player: u64, name: &str, args: &[Value]) {
    let class = name.split('.').next().unwrap_or("");
    let Some((object, behaviour)) = g.net.behaviour_index(player, class).map(|b| (player, b)).or_else(|| g.net.find_class(class)) else { return };
    let (method, body) = g.net.spec.encode(name, args);
    let mut w = Writer::new();
    w.packed_u64(client).packed_u64(object).packed_u16(behaviour).packed_u32(method).bytes(&body);
    g.receive(client, &crate::wire::batch(&[(RPC, w.0)]));
}

/// One of load.rs's actions, chosen the same way.
fn act(g: &mut Game, client: u64, player: u64, p: &mut Player, seq: u64) {
    let (iron, container) = (stable_hash("Iron"), stable_hash("Container1"));
    let Some(inventory) = g.clients.get(&client).and_then(|c| c.ready.as_ref()).map(|s| s.inventory) else { return };
    let items: Vec<i32> = g.inventories.get(&inventory).map(|i| i.items.clone()).unwrap_or_default();
    let near = |r: f32| json!([560.0 + r, 3.5, 500.0 + r]);
    if p.mining {
        p.mining = false;
        return call(g, client, player, "PlayerShareState.StopMiningServerRpc", &[]);
    }
    p.rng ^= p.rng << 13;
    p.rng ^= p.rng >> 7;
    p.rng ^= p.rng << 17;
    match p.rng % 100 {
        0..=24 if items.len() < 10 => call(g, client, player, "InventoriesHandler.AddNewItemToInventoryServerRpc", &[json!(iron), json!(inventory)]),
        0..=24 => call(g, client, player, "InventoriesHandler.RemoveItemFromInventoryServerRpc", &[json!(items[0]), json!(inventory), json!(true)]),
        // Every other drop despawns after 10 s if nobody picks it up first (a scene timer).
        25..=39 if !items.is_empty() => {
            call(g, client, player, "WorldObjectsHandler.DropOnFloorServerRpc", &[json!(items[0]), near(1.5), json!(0.6), json!(true), json!(false), json!(0), json!(if seq % 2 == 0 { 10.0 } else { 0.0 })]);
            p.dropped.push(items[0]);
        }
        40..=54 if !p.dropped.is_empty() => {
            let wo = p.dropped.remove(0);
            call(g, client, player, "InventoriesHandler.AddWorldObjectToInventoryServerRpc", &[json!(wo), json!(iron), json!(inventory), json!(true)]);
        }
        55..=59 if p.builds < 20 => {
            p.builds += 1;
            let at = near(3.0 + p.builds as f32 * 2.5 + client as f32 * 0.7);
            call(g, client, player, "WorldObjectsHandler.CreateAndInstantiateWorldObjectServerRpc", &[json!(container), at, json!([0.0, 0.0, 0.0, 1.0]), json!(true), json!(false), json!(true), json!(true)]);
        }
        60..=64 => call(g, client, player, "PlayerGaugesHandler.EatServerRpc", &[json!(5)]),
        65..=69 => call(g, client, player, "PlayerGaugesHandler.DrinkServerRpc", &[json!(5)]),
        70..=79 => {
            p.mining = true;
            call(g, client, player, "PlayerShareState.StartMiningServerRpc", &[json!(0), json!(1.0), json!(0)]);
        }
        80..=89 => call(g, client, player, "ChatHandler.SendChatMessageServerRpc", &[json!(format!("bench {client} {seq}"))]),
        _ => call(g, client, player, "InventoriesHandler.SortInventoryServerRpc", &[json!(inventory)]),
    }
}

struct Run {
    fingerprint: u64,
    /// Each tick's own hash, to find the first tick two runs differ in.
    ticks: Vec<u64>,
    messages: usize,
    busy: Duration,
    /// Allocation calls and bytes inside the timed part of the ticks.
    allocs: (u64, u64),
    /// User instructions inside the timed part (None: this process may not count them).
    instructions: Option<u64>,
    /// Each tick's time in the timed part (the players' packets and actions that tick, then the tick), in ns.
    tick_ns: Vec<u32>,
    save: usize,
    /// The hash of the final save's text alone (no ticks in it: equal across a clock that starts late).
    save_hash: u64,
    /// The sorted messages of the tick asked for.
    dumped: Vec<String>,
    /// With PCDS_BENCH_SECTIONS: the slowest ticks (index, ns) and what each section took in them.
    worst: Vec<(usize, u32, Vec<(&'static str, Duration)>)>,
    /// The game at the end.
    game: Game,
}

/// The bench loop; `swap` may replace the game after a tick, every `swap_every` seconds.
fn run(mut g: Game, e: &Env, swap_every: f64, mut swap: impl FnMut(Game) -> Game, dump: Option<usize>) -> Run {
    g.sim_time = Some(e.start);
    // The tests' check of the explosives' count looks at every world object: not in a timing.
    crate::meteo::explode::CHECK_LIVE.store(false, std::sync::atomic::Ordering::Relaxed);
    // More than the default 16 players, when asked for.
    if e.players as usize > g.rules.max_players() {
        g.rules.limits.max_players = Some(e.players as usize);
    }
    let mut h = std::collections::hash_map::DefaultHasher::new();
    let objects: Vec<u64> = (1..=e.players).map(|c| join(&mut g, c, &format!("Bench{c}"))).collect();
    let mut players: Vec<Player> = (1..=e.players).map(|c| Player { rng: 0x9e37_79b9_7f4a_7c15 ^ c.wrapping_mul(0x2545_f491_4f6c_dd1d), ..Default::default() }).collect();
    let (mut messages, _, joined) = fingerprint(&mut h, &mut g.net.out);
    let (step, net_tick, act_every) = (0.005, 1.0 / g.net_data.tick_rate as f64, e.action_ms as f64 / 1000.0);
    let (mut t, mut next_move, mut next_act, mut next_swap, mut busy, mut seq) = (0.0, 0.0, act_every, swap_every, Duration::ZERO, 0);
    let mut allocs = (0u64, 0u64);
    let counter = Instr::open();
    let mut instructions = 0u64;
    let mut tick_ns = Vec::with_capacity((e.seconds / step) as usize + 1);
    let (mut ticks, mut dumped) = (Vec::new(), Vec::new());
    let mut worst: Vec<(usize, u32, Vec<(&'static str, Duration)>)> = Vec::new();
    let sample_every: u64 = std::env::var("PCDS_BENCH_ALLOC_SITES").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
    let mut file = e.dump.as_ref().map(|p| std::io::BufWriter::new(std::fs::File::create(p).unwrap()));
    if let Some(f) = &mut file {
        use std::io::Write;
        joined.iter().for_each(|line| writeln!(f, "join {line}").unwrap());
    }
    while t < e.seconds {
        t += step;
        g.sim_time = Some(e.start + t);
        let sections_before = SECTIONS.load(Ordering::Relaxed).then(|| TIMES.with(|m| m.borrow().clone()));
        let started = Instant::now();
        let before = ALLOCS.get();
        let instr_before = counter.as_ref().map_or(0, Instr::read);
        if t >= next_move {
            next_move += net_tick;
            for (i, object) in objects.iter().enumerate() {
                let a = (t / 20.0 + i as f64) as f32;
                let mut w = Writer::new();
                // Position x and z (flags 2 and 8) of the player's own object.
                w.packed_u64(*object).packed_i32(0).packed_u32(2 | 8).packed_i32(1);
                for f in [560.0 + 10.0 * a.cos(), 500.0 + 10.0 * a.sin()] {
                    w.bytes(&f32::to_le_bytes(f));
                }
                section!("receive", g.receive(i as u64 + 1, &crate::wire::batch(&[(NETWORK_TRANSFORM, w.0)])));
            }
        }
        if e.action_ms > 0 && t >= next_act {
            next_act += act_every;
            seq += 1;
            for (i, object) in objects.iter().enumerate() {
                section!("act", act(&mut g, i as u64 + 1, *object, &mut players[i], seq));
            }
        }
        SAMPLING.set((sample_every, SAMPLING.get().1, false));
        section!("tick", g.tick());
        SAMPLING.set((0, SAMPLING.get().1, false));
        busy += started.elapsed();
        let this_tick = started.elapsed().as_nanos().min(u32::MAX as u128) as u32;
        tick_ns.push(this_tick);
        if let Some(sections_before) = sections_before
            && (worst.len() < 5 || worst.iter().any(|w| w.1 < this_tick))
        {
            let mut parts: Vec<(&'static str, Duration)> = TIMES.with(|m| m.borrow().iter().map(|(k, v)| (*k, *v - sections_before.get(k).copied().unwrap_or_default())).filter(|(_, d)| *d > Duration::from_micros(30)).collect());
            parts.sort_by_key(|p| std::cmp::Reverse(p.1));
            worst.push((tick_ns.len() - 1, this_tick, parts));
            worst.sort_by_key(|w| std::cmp::Reverse(w.1));
            worst.truncate(5);
        }
        let after = ALLOCS.get();
        allocs = (allocs.0 + after.0 - before.0, allocs.1 + after.1 - before.1);
        instructions += counter.as_ref().map_or(0, Instr::read) - instr_before;
        let (n, own, tick) = fingerprint(&mut h, &mut g.net.out);
        messages += n;
        if let Some(f) = &mut file {
            use std::io::Write;
            tick.iter().for_each(|line| writeln!(f, "{line}").unwrap());
        }
        if dump == Some(ticks.len()) {
            dumped = tick;
        }
        ticks.push(own);
        g.kicks.clear();
        // A tick boundary: everything queued went out, as after the server loop's send.
        if t >= next_swap {
            next_swap += swap_every;
            g = swap(g);
        }
    }
    let save = g.snapshot().to_text();
    save.hash(&mut h);
    let save_hash = {
        let mut own = std::collections::hash_map::DefaultHasher::new();
        save.hash(&mut own);
        own.finish()
    };
    if let Some(f) = &mut file {
        use std::io::Write;
        writeln!(f, "save {save}").unwrap();
    }
    Run { fingerprint: h.finish(), ticks, messages, busy, allocs, instructions: counter.map(|_| instructions), tick_ns, save: save.len(), save_hash, dumped, worst, game: g }
}

/// The load test's players (three, acting every 200 ms) for 20 s on the test world, played
/// straight and with the game handed off (snapshot, restore) every 5 s: everything sent and
/// the final save are the same. The bench and handoff runs below do this on a real save.
#[test]
fn a_played_session_ends_the_same_through_handoffs() {
    let Some(start) = crate::game::test_game(crate::game::tests::save()) else { return };
    let e = Env { path: String::new(), dir: crate::test_data().unwrap(), seconds: 20.0, players: 3, action_ms: 200, dump: None, start: 0.0 };
    // A copy of a game as a new bank makes it (one start for both runs: the same random state).
    let copy = |g: &mut Game| {
        let (net_data, data, spec, scenes) = crate::game::test_tables().unwrap();
        crate::handoff::restore(net_data, data, spec, &crate::handoff::snapshot(g).unwrap(), scenes).unwrap()
    };
    let mut start = start;
    let a = run(copy(&mut start), &e, f64::INFINITY, |g| g, None);
    let b = run(copy(&mut start), &e, 5.0, |mut old| copy(&mut old), None);
    // They played: built, crafted, chatted.
    let containers = a.game.wos.values().filter(|w| a.game.data.groups[w.group].id == "Container1").count();
    assert!(containers >= 3, "{containers} containers built");
    assert!(a.game.inventories.values().any(|i| i.items.len() >= 2), "crafted items in a backpack");
    assert_eq!(a.fingerprint, b.fingerprint, "{}", divergence(&a, &b, |d| (run(copy(&mut start), &e, f64::INFINITY, |g| g, d), run(copy(&mut start), &e, 5.0, |mut old| copy(&mut old), d))));
}

#[test]
#[ignore]
fn bench() {
    let Some(e) = env() else { return };
    let loaded = Instant::now();
    let g = load(&e);
    println!("bench: load {:?}, {} world objects, {} network objects", loaded.elapsed(), g.wos.len(), g.net.objects.len());
    SECTIONS.store(std::env::var("PCDS_BENCH_SECTIONS").is_ok_and(|v| v == "1"), Ordering::Relaxed);
    let r = run(g, &e, f64::INFINITY, |g| g, None);
    let per_tick = r.busy / r.ticks.len() as u32;
    println!(
        "bench: {} s simulated, {} players, {} ticks: {per_tick:?} per tick, cpu at real time {:.2}% of a core, {} messages, save {} bytes, fingerprint {:016x}, save hash {:016x}",
        e.seconds,
        e.players,
        r.ticks.len(),
        100.0 * r.busy.as_secs_f64() / e.seconds,
        r.messages,
        r.save,
        r.fingerprint,
        r.save_hash
    );
    if let Some(i) = r.instructions {
        println!("bench: {:.0} user instructions per tick in the timed part", i as f64 / r.ticks.len() as f64);
    }
    println!("bench: {:.2} allocations ({:.0} bytes) per tick, {:.2} us per tick in the timed part", r.allocs.0 as f64 / r.ticks.len() as f64, r.allocs.1 as f64 / r.ticks.len() as f64, r.busy.as_secs_f64() * 1e6 / r.ticks.len() as f64);
    let mut sorted = r.tick_ns.clone();
    sorted.sort_unstable();
    let at = |p: f64| sorted[((sorted.len() - 1) as f64 * p) as usize] as f64 / 1e3;
    println!("bench: per tick p50 {:.2} us, p99 {:.2} us, p99.9 {:.2} us, max {:.2} us", at(0.5), at(0.99), at(0.999), at(1.0));
    let mut times: Vec<_> = TIMES.with(|m| m.borrow().iter().map(|(k, v)| (*k, *v)).collect());
    times.sort_by_key(|t| std::cmp::Reverse(t.1));
    let mut sites: Vec<(String, u64)> = SITES.with(|m| m.borrow().iter().map(|(k, v)| (k.clone(), *v)).collect());
    sites.sort_by_key(|s| std::cmp::Reverse(s.1));
    let every: u64 = std::env::var("PCDS_BENCH_ALLOC_SITES").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
    for (site, n) in sites.iter().take(25) {
        println!("bench: allocs {:7.2} per tick: {site}", (*n * every) as f64 / r.ticks.len() as f64);
    }
    for (index, ns, parts) in &r.worst {
        println!("bench: slow tick {index} ({:.2} ms): {}", *ns as f64 / 1e6, parts.iter().map(|(k, d)| format!("{k} {:.2}", d.as_secs_f64() * 1e3)).collect::<Vec<_>>().join(", "));
    }
    for (name, t) in times {
        println!("bench: section {name:12} {:9.1} ms, {:8.3} us per tick, {:5.1}% of the busy time", t.as_secs_f64() * 1e3, t.as_secs_f64() * 1e6 / r.ticks.len() as f64, 100.0 * t.as_secs_f64() / r.busy.as_secs_f64());
    }
}

/// The first tick two runs differ in, with the messages only one of them sent then.
fn divergence(a: &Run, b: &Run, rerun: impl FnOnce(Option<usize>) -> (Run, Run)) -> String {
    let Some(first) = a.ticks.iter().zip(&b.ticks).position(|(x, y)| x != y) else {
        return format!("every tick equal, only the final save differs ({} vs {} bytes)", a.save, b.save);
    };
    let (x, y) = rerun(Some(first));
    let only = |p: &Run, q: &Run| p.dumped.iter().filter(|m| !q.dumped.contains(m)).map(|m| m.chars().take(160).collect::<String>()).collect::<Vec<_>>();
    format!("first differing tick {first} (t={:.3} s): straight only {:?}; handed-off only {:?}", (first + 1) as f64 * 0.005, only(&x, &y), only(&y, &x))
}

/// JSON paths where two documents differ: into `ext`, `net`, `units` and `save` one level, the
/// rest by top-level field.
fn differing(a: &Value, b: &Value) -> Vec<(String, String)> {
    let null = Value::Null;
    let mut out = Vec::new();
    let keys = |x: &Value, y: &Value| -> Vec<String> {
        let (x, y) = (x.as_object().cloned().unwrap_or_default(), y.as_object().cloned().unwrap_or_default());
        x.keys().chain(y.keys().filter(|k| !x.contains_key(*k))).cloned().collect()
    };
    for k in keys(a, b) {
        let (x, y) = (a.get(&k).unwrap_or(&null), b.get(&k).unwrap_or(&null));
        if x == y {
            continue;
        }
        if ["ext", "net", "units", "save"].contains(&k.as_str()) {
            for j in keys(x, y) {
                let (p, q) = (x.get(&j).unwrap_or(&null), y.get(&j).unwrap_or(&null));
                if p != q {
                    out.push((format!("{k}.{j}"), example(p, q)));
                }
            }
        } else {
            out.push((k, example(x, y)));
        }
    }
    out
}

/// A short example of a difference: the first differing leaf.
fn example(a: &Value, b: &Value) -> String {
    let null = Value::Null;
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            let k = x.keys().chain(y.keys()).find(|k| x.get(*k) != y.get(*k));
            k.map(|k| format!("{k}: {}", example(x.get(k).unwrap_or(&null), y.get(k).unwrap_or(&null)))).unwrap_or_default()
        }
        (Value::Array(x), Value::Array(y)) if x.len() == y.len() => {
            let i = (0..x.len()).find(|i| x[*i] != y[*i]).unwrap_or(0);
            format!("[{i}] {}", example(&x[i], &y[i]))
        }
        _ => format!("live {} / from save {}", short(a), short(b)),
    }
}

fn short(v: &Value) -> String {
    let s = v.to_string();
    if s.len() > 60 { format!("{}... ({} chars)", &s[..s.char_indices().nth(60).map(|(i, _)| i).unwrap_or(s.len())], s.len()) } else { s }
}

/// An idle world (no players: machines, growth, meteo, rockets) played PCDS_BENCH_SECONDS straight,
/// and with the server stopped and started from its save text after half of it (the game's clock
/// starts again at 0, as after a restart). What a restart changes is what the save does not hold:
/// the partial cycle of every machine, the phase of every timer, the random state. The planet's
/// unit levels at the end are printed for both.
///
///   PCDS_BENCH_SAVE=world.json PCDS_RNG_SEED=1 PCDS_BENCH_SECONDS=3600 cargo test --release bench::restart -- --ignored --nocapture
#[test]
#[ignore]
fn restart() {
    let Some(e) = env() else { return };
    let levels = |g: &mut Game| {
        let save = g.snapshot();
        save.planets.iter().map(|p| format!("{} {}", p["planetId"].as_str().unwrap_or("?"), ["unitOxygenLevel", "unitHeatLevel", "unitPressureLevel", "unitPlantsLevel", "unitInsectsLevel", "unitAnimalsLevel"].map(|k| format!("{k}={}", p[k])).join(" "))).collect::<Vec<_>>()
    };
    let play = |g: &mut Game, from: f64, to: f64, origin: f64| {
        let mut t = from;
        while t < to {
            t += 0.005;
            g.sim_time = Some(t - origin);
            g.tick();
            g.net.out.clear();
        }
    };
    let (net_data, data, spec) = crate::tables(e.dir);
    let scenes = crate::scene_tables(e.dir, &net_data, &data);
    let mut straight = load(&e);
    play(&mut straight, 0.0, e.seconds, 0.0);
    let mut first = load(&e);
    play(&mut first, 0.0, e.seconds / 2.0, 0.0);
    println!("restart: at the start {:?}", levels(&mut load(&e)));
    println!("restart: at half {:?}", levels(&mut first));
    let text = first.save_text();
    let mut second = Game::with_clock(net_data, data, spec, SaveFile::from_text(&text), scenes, Some(0.0));
    play(&mut second, e.seconds / 2.0, e.seconds, e.seconds / 2.0);
    println!("restart: straight {:?}", levels(&mut straight));
    println!("restart: restarted at {} s {:?}", e.seconds / 2.0, levels(&mut second));
}

#[test]
#[ignore]
fn handoff() {
    let Some(mut e) = env() else { return };
    e.dump = None;
    let every: f64 = std::env::var("PCDS_BENCH_HANDOFF").ok().and_then(|v| v.parse().ok()).unwrap_or(e.seconds / 2.0);
    let tables = || crate::tables(e.dir);
    let straight = |dump| run(load(&e), &e, f64::INFINITY, |g| g, dump);
    let a = straight(None);
    println!("handoff: straight run {} ticks, {} messages, fingerprint {:016x}", a.ticks.len(), a.messages, a.fingerprint);

    // The handoff itself: snapshot, a new game from it (its tables loaded beforehand), go on.
    let stats = std::cell::RefCell::new(Vec::<(usize, Duration, Duration)>::new());
    let handed = |dump| {
        stats.borrow_mut().clear();
        run(load(&e), &e, every, |mut old| {
            // The new bank has its tables loaded before the handoff starts.
            let (net_data, data, spec) = tables();
            let scenes = crate::scene_tables(e.dir, &net_data, &data);
            let started = Instant::now();
            let text = crate::handoff::snapshot(&mut old).unwrap_or_else(|err| panic!("snapshot: {err}"));
            let serialized = started.elapsed();
            drop(old);
            let started = Instant::now();
            let new = crate::handoff::restore(net_data, data, spec, &text, scenes).unwrap_or_else(|err| panic!("restore: {err}"));
            stats.borrow_mut().push((text.len(), serialized, started.elapsed()));
            new
        }, dump)
    };
    let b = handed(None);
    {
        let s = stats.borrow();
        let spread = |f: &dyn Fn(&(usize, Duration, Duration)) -> Duration| {
            let mut v: Vec<Duration> = s.iter().map(f).collect();
            v.sort();
            format!("p50 {:?} max {:?}", v.get(v.len() / 2).copied().unwrap_or_default(), v.last().copied().unwrap_or_default())
        };
        println!(
            "handoff: {} handoffs, every {every} s: snapshot up to {} bytes; serialize {}, restore {}, the two together (the stand) {}",
            s.len(),
            s.iter().map(|x| x.0).max().unwrap_or(0),
            spread(&|x| x.1),
            spread(&|x| x.2),
            spread(&|x| x.1 + x.2)
        );
    }
    let equal = a.fingerprint == b.fingerprint;
    println!("handoff: handed-off run {} messages, fingerprint {:016x}: {}", b.messages, b.fingerprint, if equal { "EQUAL" } else { "DIFFERENT" });
    if !equal {
        println!("handoff: {}", divergence(&a, &b, |d| (straight(d), handed(d))));
    }

    // What the save alone does not rebuild: a game loaded from the save written at the first
    // handoff time (no one connected) against the live game then.
    let (mut live_doc, mut cold_doc) = (Value::Null, Value::Null);
    run(load(&e), &e, every, |mut live| {
        if live_doc.is_null() {
            let (net_data, data, spec) = tables();
            let scenes = crate::scene_tables(e.dir, &net_data, &data);
            let mut cold = Game::new(net_data, data, spec, SaveFile::from_text(&live.snapshot().to_text()), scenes);
            cold.sim_time = live.sim_time;
            live_doc = serde_json::to_value(&live).unwrap();
            cold_doc = serde_json::to_value(&cold).unwrap();
        }
        live
    }, None);
    // Where the snapshot's bytes go (the pause grows with them).
    let mut sizes: Vec<(String, usize)> = live_doc.as_object().unwrap().iter().flat_map(|(k, v)| match (k.as_str(), v.as_object()) {
        ("ext" | "net", Some(o)) => o.iter().map(|(j, w)| (format!("{k}.{j}"), w.to_string().len())).collect::<Vec<_>>(),
        _ => vec![(k.clone(), v.to_string().len())],
    }).collect();
    sizes.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
    println!("handoff: snapshot bytes by part: {}", sizes.iter().take(8).map(|(k, n)| format!("{k} {n}")).collect::<Vec<_>>().join(", "));
    let parts = differing(&live_doc, &cold_doc);
    println!("handoff: {} parts differ between the live game and one loaded from its save:", parts.len());
    // Each part from the save-loaded game instead of the live one: does the output change?
    for (part, example) in &parts {
        let mut doc = live_doc.clone();
        let pointer = format!("/{}", part.replace('.', "/"));
        let cold = cold_doc.pointer(&pointer).cloned().unwrap_or(Value::Null);
        let Some(slot) = doc.pointer_mut(&pointer) else { continue };
        *slot = cold;
        let quiet = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut first = true;
            run(load(&e), &e, every, |g| {
                if !std::mem::take(&mut first) {
                    return g;
                }
                let (net_data, data, spec) = tables();
                let scenes = crate::scene_tables(e.dir, &net_data, &data);
                // A snapshot of the changed state: the document read as a game, written as a bank does.
                let mut changed: Game = serde_json::from_value(doc.clone()).unwrap_or_else(|err| panic!("state: {err}"));
                changed.net_data.game_version = net_data.game_version.clone();
                let text = crate::handoff::snapshot(&mut changed).unwrap_or_else(|err| panic!("snapshot: {err}"));
                crate::handoff::restore(net_data, data, spec, &text, scenes).unwrap_or_else(|err| panic!("restore: {err}"))
            }, None)
        }));
        std::panic::set_hook(quiet);
        let verdict = match outcome {
            Ok(r) if r.fingerprint == a.fingerprint => "output unchanged".to_string(),
            Ok(r) => format!("OUTPUT DIFFERS from tick {:?}", a.ticks.iter().zip(&r.ticks).position(|(x, y)| x != y)),
            Err(e) => format!("PANICS ({})", e.downcast_ref::<String>().cloned().or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_default().chars().take(120).collect::<String>()),
        };
        println!("handoff:   {part}: {verdict}; e.g. {example}");
    }
    assert!(equal, "the handed-off run must send and save exactly what the straight run does");
}

/// Long-uptime soak: two players act every second for PCDS_SOAK_SECONDS of game time (steps of
/// PCDS_SOAK_STEP s); every PCDS_SOAK_REPORT s the size of each part of the state, and the
/// network objects by what they stand for, are printed: what grows without end shows.
/// PCDS_BENCH_SAVE=world.json cargo test soak_growth -- --ignored --nocapture
#[test]
#[ignore]
fn soak_growth() {
    let Some(mut e) = env() else { return };
    let var = |k: &str, d: f64| std::env::var(k).ok().and_then(|v| v.parse::<f64>().ok()).unwrap_or(d);
    let (seconds, step, report) = (var("PCDS_SOAK_SECONDS", 3600.0), var("PCDS_SOAK_STEP", 0.05), var("PCDS_SOAK_REPORT", 900.0));
    e.players = 2;
    let mut g = load(&e);
    g.sim_time = Some(0.0);
    let objects: Vec<u64> = (1..=e.players).map(|c| join(&mut g, c, &format!("Soak{c}"))).collect();
    let mut players: Vec<Player> = (1..=e.players).map(|c| Player { rng: 0x9e37_79b9_7f4a_7c15 ^ c.wrapping_mul(0x2545_f491_4f6c_dd1d), ..Default::default() }).collect();
    let (mut t, mut next_act, mut next_report, mut seq) = (0.0, 1.0, 0.0, 0);
    let mut first: Option<std::collections::BTreeMap<String, usize>> = None;
    while t < seconds {
        t += step;
        g.sim_time = Some(e.start + t);
        if t >= next_act {
            next_act += 1.0;
            seq += 1;
            for (i, object) in objects.iter().enumerate() {
                act(&mut g, i as u64 + 1, *object, &mut players[i], seq);
            }
        }
        g.tick();
        g.net.out.clear();
        g.kicks.clear();
        if t >= next_report {
            next_report += report;
            let doc = serde_json::to_value(&g).unwrap();
            let mut sizes = std::collections::BTreeMap::new();
            for (k, v) in doc.as_object().unwrap() {
                match (k.as_str(), v.as_object()) {
                    ("ext" | "net", Some(o)) => o.iter().for_each(|(j, w)| {
                        sizes.insert(format!("{k}.{j}"), w.to_string().len());
                    }),
                    _ => {
                        sizes.insert(k.clone(), v.to_string().len());
                    }
                }
            }
            let base = first.get_or_insert_with(|| sizes.clone()).clone();
            let grown: Vec<String> = sizes.iter().filter(|(k, n)| **n > base.get(*k).copied().unwrap_or(0) * 12 / 10 + 200).map(|(k, n)| format!("{k} {}->{n}", base.get(k).copied().unwrap_or(0))).collect();
            // A network object whose world object is gone, or points at another object.
            let strays = g.net.objects.values().filter(|o| !o.scene).filter(|o| o.wo.is_some_and(|w| g.wos.get(&w).is_none_or(|w| w.net != Some(o.id)))).count();
            // World objects in no inventory and at no place: nothing reaches them.
            let mut orphans: std::collections::BTreeMap<&str, usize> = Default::default();
            for w in g.wos.values().filter(|w| !w.is_placed() && !g.is_held(w.id) && !crate::model::is_from_scene(w.id)) {
                *orphans.entry(g.data.groups[w.group].id.as_str()).or_default() += 1;
            }
            println!("soak orphans: {orphans:?}");
            // Load then save twice: the second load is the fixpoint (the first recomputes what the game does at a load).
            let (net_data, data, spec) = crate::tables(e.dir);
            let scenes = crate::scene_tables(e.dir, &net_data, &data);
            let first = g.snapshot().to_text();
            let second = Game::new(net_data.clone(), data.clone(), spec.clone(), SaveFile::from_text(&first), scenes.clone()).snapshot().to_text();
            let third = Game::new(net_data, data, spec, SaveFile::from_text(&second), scenes).snapshot().to_text();
            let differs = |a: &str, b: &str| a.split(['\n']).zip(b.split(['\n'])).filter(|(x, y)| x != y).map(|(x, y)| format!("{} / {}", x.chars().take(240).collect::<String>(), y.chars().take(240).collect::<String>())).take(3).collect::<Vec<_>>();
            println!("soak save fixpoint: live vs loaded {} lines differ, loaded vs loaded-again {} lines differ {:?}", first.split('\n').zip(second.split('\n')).filter(|(x, y)| x != y).count(), second.split('\n').zip(third.split('\n')).filter(|(x, y)| x != y).count(), differs(&second, &third));
            println!("soak t={t:.0}s: wos {} inv {} net {} strays {strays} total {} bytes; grown: {}", g.wos.len(), g.inventories.len(), g.net.objects.len(), sizes.values().sum::<usize>(), grown.join(", "));
        }
    }
}

/// The CPU time of this thread (a loaded host stretches wall time, not this).
#[derive(Clone, Copy)]
struct Cpu(Duration);

impl Cpu {
    fn now() -> Cpu {
        let mut t = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        // SAFETY: clock_gettime fills the timespec it is given.
        unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut t) };
        Cpu(Duration::new(t.tv_sec as u64, t.tv_nsec as u32))
    }
    fn elapsed(&self) -> Duration {
        Cpu::now().0 - self.0
    }
}

/// Resident memory of this process, in KiB.
fn rss_kib() -> u64 {
    std::fs::read_to_string("/proc/self/statm").ok().and_then(|s| s.split_whitespace().nth(1)?.parse::<u64>().ok()).unwrap_or(0) * 4
}

/// Bytes malloc has handed out and not had back (glibc's mallinfo2): what the program holds,
/// not what the process keeps resident after a free.
fn heap_bytes() -> u64 {
    // SAFETY: mallinfo2 only reads malloc's counters.
    let m = unsafe { libc::mallinfo2() };
    (m.uordblks + m.hblkhd) as u64
}

/// The memory each of the tables takes (PCDS_TEST_DATA), and what a start's load leaves of
/// them: `cargo test --release tables_memory -- --ignored --nocapture`.
#[test]
#[ignore]
fn tables_memory() {
    let Some(dir) = crate::test_data() else { return };
    let mib = |b: u64| format!("{:.1} MiB", b as f64 / 1048576.0);
    let (start, t) = (heap_bytes(), Instant::now());
    let net_data = crate::data::NetData::load(&format!("{dir}/netdata.json")).unwrap();
    let after_net = heap_bytes();
    let spec = crate::rpcspec::Spec::load(&format!("{dir}/rpcs.json")).unwrap();
    let after_spec = heap_bytes();
    let t_game = Instant::now();
    let data = crate::gamedata::GameData::load(&format!("{dir}/gamedata.json")).unwrap();
    let after_game = heap_bytes();
    let t_scenes = Instant::now();
    let scenes = crate::scene_tables(dir, &net_data, &data);
    let after_scenes = heap_bytes();
    println!(
        "tables: netdata {} ({:?} with rpcs), rpcs {}, gamedata {} ({:?}), scenes {} ({:?}); total {}, rss {} MiB",
        mib(after_net - start),
        t_game.duration_since(t),
        mib(after_spec - after_net),
        mib(after_game - after_spec),
        t_scenes.duration_since(t_game),
        mib(after_scenes - after_game),
        t_scenes.elapsed(),
        mib(after_scenes - start),
        rss_kib() >> 10
    );
    drop((spec, scenes));
    // What each part of the game data holds: the heap before and after it is dropped.
    let mut data = data;
    let mut parts = Vec::new();
    let mut measure = |name: &str, drop_it: &mut dyn FnMut()| {
        let before = heap_bytes();
        drop_it();
        parts.push(format!("{name} {}", mib(before.saturating_sub(heap_bytes()))));
    };
    measure("layouts", &mut || data.layouts = Default::default());
    measure("assets", &mut || data.assets = Default::default());
    measure("group_prefabs", &mut || data.group_prefabs = Default::default());
    measure("panels", &mut || data.panels = Default::default());
    measure("singletons", &mut || data.singletons = Default::default());
    measure("group fields", &mut || data.groups.iter_mut().for_each(|g| g.fields = Default::default()));
    println!("tables: gamedata by part: {}; left {}", parts.join(", "), mib(heap_bytes().saturating_sub(start)));
}

/// Load, save and memory of a world of any size (PCDS_BENCH_SAVE): the times of each step of a
/// start and of an autosave, and the memory they leave. `cargo test --release scale -- --ignored --nocapture`.
#[test]
#[ignore]
fn scale() {
    let Some(e) = env() else { return };
    let (net_data, data, spec) = crate::tables(e.dir);
    let scenes = crate::scene_tables(e.dir, &net_data, &data);
    let tables_rss = rss_kib();
    let t = Cpu::now();
    let save = SaveFile::load(&e.path).unwrap();
    println!("scale: heap after tables {} MiB; tables {} MiB rss; read+parse save {:?}, {} objects", heap_bytes() >> 20, tables_rss >> 10, t.elapsed(), save.objects.len());
    let t = Cpu::now();
    let mut g = Game::with_clock(net_data, data, spec, save, scenes, Some(0.0));
    println!("scale: Game::new {:?}, {} world objects, {} network objects, rss {} MiB (+{} MiB), heap {} MiB", t.elapsed(), g.wos.len(), g.net.objects.len(), rss_kib() >> 10, (rss_kib() - tables_rss) >> 10, heap_bytes() >> 20);
    {
        // A player joining: ConnectionApproved with the whole world, then PlayerReady.
        let t = Cpu::now();
        join(&mut g, 90, "Scale1");
        let first = t.elapsed();
        let bytes: usize = g.net.out.iter().filter(|(to, _, _)| *to == crate::net::Target::Only(90)).map(|(_, _, b)| b.len()).sum();
        g.net.out.clear();
        let t = Cpu::now();
        join(&mut g, 91, "Scale2");
        println!("scale: a player joins in {first:?} (the second {:?}), {} KiB sent to it", t.elapsed(), bytes >> 10);
        g.disconnect(90);
        g.disconnect(91);
        g.net.out.clear();
    }
    let (before, t) = (rss_kib(), Cpu::now());
    // SAFETY: malloc_trim only returns free memory to the system.
    unsafe { libc::malloc_trim(0) };
    println!("scale: malloc_trim(0) after the load {:?}: rss {} MiB -> {} MiB, malloc in use {:.1} MiB", t.elapsed(), before >> 10, rss_kib() >> 10, heap_bytes() as f64 / 1048576.0);
    let t = Cpu::now();
    let snapshot = g.snapshot();
    let took = t.elapsed();
    let peak = rss_kib();
    let t = Cpu::now();
    let text = snapshot.to_text();
    println!("scale: snapshot (JSON trees) {took:?}, to_text {:?}, {} bytes, rss {} MiB", t.elapsed(), text.len(), peak >> 10);
    drop(snapshot);
    let before = rss_kib();
    let direct = g.save_text();
    assert!(direct == text, "save_text is the snapshot's text");
    // What the game thread stands for (wall time: the text is written by several threads) and what all threads spend.
    let process_cpu = || {
        let mut t = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        // SAFETY: clock_gettime fills the timespec it is given.
        unsafe { libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, &mut t) };
        Duration::new(t.tv_sec as u64, t.tv_nsec as u32)
    };
    let (mut walls, cpu0) = (Vec::new(), process_cpu());
    for _ in 0..9 {
        let t = Instant::now();
        let again = g.save_text();
        walls.push(t.elapsed());
        assert!(again.len() == direct.len());
    }
    walls.sort();
    println!("scale: save_text (no trees) wall min {:?} median {:?}, all threads' cpu {:?} each, rss {} MiB (+{} MiB)", walls[0], walls[4], (process_cpu() - cpu0) / 9, rss_kib() >> 10, rss_kib().saturating_sub(before) >> 10);
    let snapshot = g.snapshot();
    let dir = std::env::temp_dir().join(format!("pcds-scale-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("world.json").to_string_lossy().to_string();
    let t = Cpu::now();
    snapshot.write(&path).unwrap();
    println!("scale: write (to_text + write_atomic) {:?}", t.elapsed());
    let t = Cpu::now();
    crate::save::backup(&path, Duration::ZERO, 24).unwrap();
    println!("scale: backup {:?}", t.elapsed());
    let t = Cpu::now();
    let back = SaveFile::load(&path).unwrap();
    println!("scale: reload {:?} ({} objects)", t.elapsed(), back.objects.len());
    let t = Cpu::now();
    let h = crate::handoff::snapshot(&mut g).unwrap();
    println!("scale: handoff snapshot {:?}, {} bytes", t.elapsed(), h.len());
    {
        let (mut vars, mut behaviours, mut name_bytes, mut value_bytes, mut list_items) = (0usize, 0usize, 0usize, 0usize, 0usize);
        for o in g.net.objects.values() {
            behaviours += o.behaviours.len();
            name_bytes += o.behaviours.iter().map(|b| b.class.len()).sum::<usize>();
            for b in &o.behaviours {
                vars += b.vars.len();
                for v in &b.vars {
                    name_bytes += v.name.len() + v.ty.len();
                    match &v.value {
                        crate::net::VarValue::Plain(x) => value_bytes += x.len(),
                        crate::net::VarValue::List(l) => list_items += l.len(),
                    }
                }
            }
        }
        let n = g.net.objects.len().max(1);
        println!("scale: per network object: {:.1} behaviours, {:.1} variables, {:.0} bytes of names and types, {:.0} bytes of plain values, {:.1} list items", behaviours as f64 / n as f64, vars as f64 / n as f64, name_bytes as f64 / n as f64, value_bytes as f64 / n as f64, list_items as f64 / n as f64);
    }
    println!("scale: prefabs parsed (of all): {:?}", g.data.group_prefabs.parsed());
    // What each part of the game holds: the heap before and after it is dropped (last: the rest
    // of the game is not whole after it).
    let mut parts = Vec::new();
    let mut measure = |name: &str, drop_it: &mut dyn FnMut()| {
        let before = heap_bytes();
        drop_it();
        parts.push(format!("{name} {:.1}", before.saturating_sub(heap_bytes()) as f64 / 1048576.0));
    };
    let (wos, net, invs, holders) = (g.wos.len(), g.net.objects.len(), g.inventories.len(), g.holders.len());
    measure("net.objects", &mut || drop(std::mem::take(&mut g.net.objects)));
    measure("wos", &mut || drop(std::mem::take(&mut g.wos)));
    measure("inventories", &mut || drop(std::mem::take(&mut g.inventories)));
    measure("holders", &mut || g.holders = Default::default());
    measure("constructed+pickables", &mut || {
        g.constructed = Default::default();
        g.pickables = Default::default();
    });
    measure("timers", &mut || g.timers = Default::default());
    measure("ext.machines", &mut || g.ext.machines = Default::default());
    measure("ext.logistics", &mut || g.ext.logistics = Default::default());
    measure("ext.water", &mut || g.ext.water = Default::default());
    measure("ext.meteo", &mut || g.ext.meteo = Default::default());
    measure("ext.energy", &mut || g.ext.energy = Default::default());
    measure("ext.planets", &mut || g.ext.planets = Default::default());
    measure("ext.animals", &mut || g.ext.animals = Default::default());
    measure("ext.toxic", &mut || g.ext.toxic = Default::default());
    measure("ext.rockets", &mut || g.ext.rockets = Default::default());
    measure("ext.gauges", &mut || g.ext.gauges = Default::default());
    measure("ext.status", &mut || g.ext.status = Default::default());
    measure("ext.chat", &mut || g.ext.chat = Default::default());
    measure("ext.rest", &mut || g.ext = Default::default());
    measure("save", &mut || g.save = SaveFile::empty());
    measure("saved_players", &mut || g.saved_players = Vec::new());
    measure("net_wos", &mut || g.net_wos = Default::default());
    measure("units", &mut || g.units = Default::default());
    println!("scale: heap MiB by part ({wos} world objects, {net} network objects, {invs} inventories, {holders} holders): {}", parts.join(", "));
    std::fs::remove_dir_all(dir).ok();
}

/// The flip's stand on worlds of several sizes, phase by phase (docs/hot-update.md, "Flip
/// speed"). The old bank snapshots into a memfd, the descriptor crosses a unix socket as the
/// front passes it on, the new bank reads it and restores (its tables loaded beforehand), and
/// the restored game is the next flip's old bank: N flips in a row.
///
///   tools/grow-world.py SAVE OUT N        a save grown to about N world objects
///   PCDS_FLIPBENCH_SAVES=a.json,b.json [PCDS_FLIPBENCH_N=20] [PCDS_FLIPBENCH_PLAYERS=8] \
///     cargo test --release flipbench -- --ignored --nocapture
///
/// Prints, per save, p50 / p95 / max (ms) of: serialize, memfd write, fd pass, memfd read, each
/// restore step, and the stand (the sum). The verify check is not run (PCDS_DEV=flip-verify is a
/// test aid). The front's own part (held frames, READY) is in its log line and record.
#[test]
#[ignore]
fn flipbench() {
    use std::collections::BTreeMap;
    let Some(dir) = crate::test_data() else { return };
    let var = |k: &str, d: usize| std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d);
    let (rounds, players) = (var("PCDS_FLIPBENCH_N", 20), var("PCDS_FLIPBENCH_PLAYERS", 8) as u64);
    let saves = std::env::var("PCDS_FLIPBENCH_SAVES").or_else(|_| std::env::var("PCDS_BENCH_SAVE")).expect("flipbench: set PCDS_FLIPBENCH_SAVES");
    for path in saves.split(',') {
        let e = Env { path: path.to_string(), dir, seconds: 0.0, players, action_ms: 0, dump: None, start: 0.0 };
        let loading = Instant::now();
        let mut g = load(&e);
        let loaded = loading.elapsed();
        g.sim_time = Some(0.0);
        for c in 1..=players {
            join(&mut g, c, &format!("Bench{c}"));
        }
        g.net.out.clear();
        // Some play: a second of ticks, so that timers and queues hold state.
        for i in 1..=200 {
            g.sim_time = Some(i as f64 * 0.005);
            g.tick();
            g.net.out.clear();
        }
        // What a new bank does before the stand, and what it holds: its tables.
        {
            let rss = || std::fs::read_to_string("/proc/self/status").ok().and_then(|t| t.lines().find(|l| l.starts_with("VmRSS:")).and_then(|l| l.split_whitespace().nth(1)?.parse::<f64>().ok())).unwrap_or(0.0) / 1024.0;
            let (before, started) = (rss(), Instant::now());
            let (net_data, data, spec) = crate::tables(dir);
            let tables = started.elapsed();
            let scenes = crate::scene_tables(dir, &net_data, &data);
            println!("flipbench:   a new bank loads the tables in {tables:?}, the scenes in {:?}; its memory grows by {:.0} MiB (this process holds {:.0} MiB with the world loaded)", started.elapsed() - tables, rss() - before, before);
            drop((net_data, data, spec, scenes));
        }
        let mut samples: BTreeMap<&'static str, Vec<f64>> = BTreeMap::new();
        let (mut bytes, mut sizes) = (0, [0; crate::handoff::KINDS]);
        let (front_end, bank_end) = std::os::unix::net::UnixStream::pair().unwrap();
        let (mut front, mut bank) = (crate::ipc::Link::new(front_end), crate::ipc::Link::new(bank_end));
        let counter = Instr::open_inherit();
        for round in 0..rounds + 1 {
            let (net_data, data, spec) = crate::tables(dir);
            let scenes = crate::scene_tables(dir, &net_data, &data);
            let mut phases: Vec<(&'static str, f64)> = Vec::new();
            let instr_before = counter.as_ref().map_or(0, Instr::read);
            let fd = crate::handoff::snapshot_memfd(&mut g, &mut phases).unwrap();
            // The front passes the descriptor on: the old bank's link to the front's, then the front's to the new bank's.
            let t = Instant::now();
            bank.send_fd(crate::ipc::SNAPSHOT, 1, b"{}", Some(fd));
            let f = front.recv().unwrap();
            front.queue(crate::ipc::RESTORE, 0, b"{}", f.fd, 0);
            front.flush().unwrap();
            let f = bank.recv().unwrap();
            phases.push(("fd pass", t.elapsed().as_secs_f64() * 1000.0));
            let t = Instant::now();
            let text = crate::ipc::map_memfd(f.fd.as_ref().unwrap()).unwrap();
            phases.push(("memfd map", t.elapsed().as_secs_f64() * 1000.0));
            bytes = text.len();
            sizes = crate::handoff::part_sizes(&text);
            let new = crate::handoff::restore_timed(net_data, data, spec, &text, scenes, &mut phases).unwrap_or_else(|err| panic!("restore: {err}"));
            // What the stand cost in user instructions, all its threads (they end before this): millions.
            if let Some(c) = &counter {
                phases.push((".stand instructions (M)", (c.read() - instr_before) as f64 / 1e6));
            }
            // The steps inside a step (their names start with a dot) are not added twice.
            let stand: f64 = phases.iter().filter(|p| !p.0.starts_with('.')).map(|p| p.1).sum();
            phases.push(("stand", stand));
            drop(text);
            // The old bank ends (EXIT): its world is freed after the stand, not in it.
            let old = std::mem::replace(&mut g, new);
            drop(old);
            // Round 0 warms the allocator and the page cache.
            if round > 0 {
                for (k, v) in phases {
                    samples.entry(k).or_default().push(v);
                }
            }
            g.net.out.clear();
            g.sim_time = Some(1.0 + round as f64 * 0.005);
            g.tick();
            g.net.out.clear();
        }
        println!("flipbench: {path}: {} world objects, {} network objects, {} inventories; loaded in {loaded:?}; snapshot {bytes} bytes; {rounds} flips (ms: p50 / p95 / max / min)", g.wos.len(), g.net.objects.len(), g.inventories.len());
        println!("flipbench:   bytes: {}", crate::handoff::NAMES.iter().zip(sizes).map(|(n, b)| format!("{n} {b}")).collect::<Vec<_>>().join(", "));
        let order = ["serialize", ".write cpu rest", ".write cpu world objects", ".write cpu inventories", ".write cpu network objects", ".write cpu machines", ".write cpu water", "memfd write", "fd pass", "memfd map", "head", ".parts read", ".read cpu rest", ".read cpu world objects", ".read cpu inventories", ".read cpu network objects", ".read cpu machines", ".read cpu water", ".parts joined", ".slot indexes", ".net index", "parse", "rebuild holders", "refresh tables", "generation", "stand", ".stand instructions (M)"];
        for name in order {
            let Some(v) = samples.get_mut(name) else { continue };
            v.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let at = |q: f64| v[((v.len() - 1) as f64 * q).round() as usize];
            println!("flipbench:   {name:<16} {:>8.2} {:>8.2} {:>8.2} {:>8.2}", at(0.5), at(0.95), v[v.len() - 1], v[0]);
        }
    }
}

/// How much of a big world changes while a pre-copy would be under way (docs/hot-update.md, "Flip
/// speed"): the same players act (PCDS_BENCH_ACTION_MS, default 200) over PCDS_FLIPBENCH_PLAYERS
/// (default 8); the entries of the world objects, inventories and network objects are digested at
/// two moments `delta` seconds apart (0.25, 1 and 5), and the entries and the 2048-entry chunks
/// that differ are counted.
///
///   PCDS_FLIPBENCH_SAVES=a.json cargo test --release --bin pcds bench::dirtybench -- --ignored --nocapture
#[test]
#[ignore]
fn dirtybench() {
    use std::hash::{Hash, Hasher};
    let Some(dir) = crate::test_data() else { return };
    let players = std::env::var("PCDS_FLIPBENCH_PLAYERS").ok().and_then(|v| v.parse().ok()).unwrap_or(8u64);
    let action_ms = std::env::var("PCDS_BENCH_ACTION_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(200u64);
    let saves = std::env::var("PCDS_FLIPBENCH_SAVES").expect("dirtybench: set PCDS_FLIPBENCH_SAVES");
    /// Each entry's digest, keyed by its id, per collection.
    fn digests(g: &Game) -> [Vec<(u64, u64)>; 3] {
        let digest = |v: &[u8]| {
            let mut h = std::collections::hash_map::DefaultHasher::new();
            v.hash(&mut h);
            h.finish()
        };
        let wos = g.wos.slots().iter().enumerate().map(|(i, s)| (i as u64, s.as_ref().map(|(k, v)| digest(&crate::bincodec::to_vec(&(k, v)).unwrap())).unwrap_or(0))).collect();
        let inventories = g.inventories.slots().iter().enumerate().map(|(i, s)| (i as u64, s.as_ref().map(|(k, v)| digest(&crate::bincodec::to_vec(&(k, v)).unwrap())).unwrap_or(0))).collect();
        let net = g.net.objects.iter().map(|(k, v)| (*k, digest(&crate::bincodec::to_vec(v).unwrap()))).collect();
        [wos, inventories, net]
    }
    for path in saves.split(',') {
        for delta in [0.25, 1.0, 5.0] {
            let e = Env { path: path.to_string(), dir, seconds: 2.0 * delta + 0.01, players, action_ms, dump: None, start: 0.0 };
            let taken = std::cell::RefCell::new(Vec::<[Vec<(u64, u64)>; 3]>::new());
            run(load(&e), &e, delta, |g| {
                taken.borrow_mut().push(digests(&g));
                g
            }, None);
            let taken = taken.into_inner();
            let [a, b] = [&taken[0], &taken[1]];
            let mut line = format!("dirtybench: {path}: {players} players, {delta} s apart:");
            for (name, (x, y)) in ["world objects", "inventories", "network objects"].iter().zip(a.iter().zip(b.iter())) {
                let y: std::collections::HashMap<u64, u64> = y.iter().copied().collect();
                let changed: Vec<usize> = x.iter().enumerate().filter(|(_, (k, d))| y.get(k) != Some(d)).map(|(i, _)| i).collect();
                let new = y.len().saturating_sub(x.len());
                let chunks: std::collections::BTreeSet<usize> = changed.iter().map(|i| i / 2048).collect();
                line += &format!(" {name} {}/{} entries (+{new} new), {}/{} chunks;", changed.len(), x.len(), chunks.len(), x.len().div_ceil(2048));
            }
            println!("{line}");
        }
    }
}

/// The efficiency guard (docs/performance.md): budgets on what a tick, a snapshot and a join cost,
/// counted in things that do not depend on how busy the machine is (allocations, bytes, user
/// instructions), so that a change that makes the server dearer fails a test.
mod budget {
    use super::*;
    use crate::game::tests::{save, join};

    /// About `buildings` buildings on the test world, in the proportions of a base: containers that
    /// supply and demand iron and magnesium, water collectors and autocrafters running on the energy
    /// two generators make, a drone station with three drones. Unseeded: the same every time.
    pub(super) fn world(buildings: usize) -> Option<Game> {
        use crate::testkit::{give, place};
        let mut g = crate::game::test_game(save())?;
        let at = |i: usize| [500.0 + (i % 50) as f32 * 3.0, 3.5, 450.0 + (i / 50) as f32 * 3.0];
        let (iron, magnesium) = (g.data.group("Iron").unwrap(), g.data.group("Magnesium").unwrap());
        for i in 0..2 {
            place(&mut g, "EnergyGenerator6", [540.0 + i as f32 * 8.0, 3.5, 540.0]);
        }
        let (station, _) = place(&mut g, "DroneStation1", [560.0, 3.5, 560.0]);
        let station_inventory = g.wos[&station].linked_inventory;
        for _ in 0..3 {
            give(&mut g, "Drone1", station_inventory);
        }
        let target = g.data.group("Rod-iridium").unwrap();
        for i in 0..buildings {
            match i % 8 {
                // Supply and demand: half of the containers.
                0..=3 => {
                    let (wo, _) = place(&mut g, "Container1", at(i));
                    let inventory = g.wos[&wo].linked_inventory;
                    for _ in 0..6 {
                        give(&mut g, if i % 2 == 0 { "Iron" } else { "Magnesium" }, inventory);
                    }
                    let (supply, demand) = if i % 2 == 0 { (vec![iron], vec![magnesium]) } else { (vec![magnesium], vec![iron]) };
                    let inv = g.inventories.get_mut(&inventory).unwrap();
                    inv.logistic.supply = supply;
                    inv.logistic.demand = demand;
                    crate::logistics::set_dirty(&mut g, inventory);
                }
                4..=6 => {
                    let (wo, _) = place(&mut g, "WaterCollector1", at(i));
                    g.ext.machines.energy.insert(wo, true);
                }
                _ => {
                    let (wo, _) = place(&mut g, "AutoCrafter1", at(i));
                    g.wos.get_mut(&wo).unwrap().linked_groups = Some(vec![target]);
                    g.ext.machines.energy.insert(wo, true);
                }
            }
        }
        g.net.out.clear();
        Some(g)
    }

    /// What `seconds` of the world with `players` do, per tick: (allocations, user instructions
    /// when this process may count them), and the world's size.
    pub(super) fn tick_cost(buildings: usize, players: u64, action_ms: u64, seconds: f64) -> Option<(f64, Option<f64>, usize)> {
        let g = world(buildings)?;
        let objects = g.wos.len();
        let e = Env { path: String::new(), dir: crate::test_data().unwrap(), seconds, players, action_ms, dump: None, start: 0.0 };
        let r = run(g, &e, f64::INFINITY, |g| g, None);
        let ticks = r.ticks.len() as f64;
        Some((r.allocs.0 as f64 / ticks, r.instructions.map(|i| i as f64 / ticks), objects))
    }

    /// The tick's cost on the guard world, measured in what does not depend on the machine's load.
    /// `allocations` per tick and (where the kernel lets the process count) user `instructions` per
    /// tick. A budget is about one and a half times what the build measures; one that is passed is a
    /// change that made the server dearer: find it (tools/perf, docs/performance.md) or, if the cost is
    /// the point of the change, raise the budget there and here.
    #[test]
    fn the_tick_stays_within_its_budget() {
        // (buildings, players, they act every N ms, allocations per tick, instructions per tick)
        let cases: [(usize, u64, u64, f64, f64); 5] = [(0, 0, 0, 2.0, 6_000.0), (400, 0, 0, 30.0, 32_000.0), (400, 4, 0, 38.0, 38_000.0), (400, 4, 200, 45.0, 45_000.0), (1600, 4, 200, 120.0, 130_000.0)];
        for (buildings, players, action_ms, max_allocs, max_instr) in cases {
            let Some((allocs, instr, objects)) = tick_cost(buildings, players, action_ms, 20.0) else { return };
            let what = format!("{buildings} buildings ({objects} world objects), {players} players acting every {action_ms} ms");
            assert!(allocs <= max_allocs, "{what}: {allocs:.1} allocations per tick, the budget is {max_allocs} (docs/performance.md)");
            match instr {
                Some(i) => assert!(i <= max_instr, "{what}: {i:.0} instructions per tick, the budget is {max_instr} (docs/performance.md)"),
                None => eprintln!("SKIPPED the instruction budget of {what}: this process may not count instructions (perf_event_paranoid, a container or a VM without counters); the allocations were checked"),
            }
        }
    }

    /// Four times the buildings is about four times the tick, not sixteen: nothing in a tick may
    /// look at every building for every building.
    #[test]
    fn a_tick_costs_about_the_same_per_building_at_four_times_the_size() {
        let Some((_, Some(small), n_small)) = tick_cost(400, 0, 0, 20.0) else { return };
        let (_, Some(large), n_large) = tick_cost(1600, 0, 0, 20.0).unwrap() else { return };
        let (per_small, per_large) = (small / n_small as f64, large / n_large as f64);
        assert!(per_large <= per_small * 2.0, "{per_small:.1} instructions per tick and world object at {n_small}, {per_large:.1} at {n_large}: something in the tick grew faster than the world");
    }

    /// The bytes of the world as the server keeps and moves them: a hot update's snapshot, the save
    /// text and what a joining player is sent, per world object.
    #[test]
    fn the_snapshot_the_save_and_the_join_stay_within_their_budget() {
        let Some(mut g) = world(400) else { return };
        let objects = g.wos.len();
        let snapshot = crate::handoff::snapshot(&mut g).unwrap().len();
        let save = g.save_text().len();
        join(&mut g, 90, "Joiner");
        let joined: usize = g.net.out.iter().filter(|(to, _, _)| *to == crate::net::Target::Only(90)).map(|(_, _, b)| b.len()).sum();
        let per = |bytes: usize| bytes as f64 / objects as f64;
        assert!(per(snapshot) <= 300.0, "a snapshot is {:.0} bytes per world object, the budget is 300 (docs/performance.md)", per(snapshot));
        assert!(per(save) <= 100.0, "a save is {:.0} bytes per world object, the budget is 100 (docs/performance.md)", per(save));
        assert!(per(joined) <= 24.0, "a join sends {:.0} bytes per world object, the budget is 24 (docs/performance.md)", per(joined));
    }

    /// What a hot update's world stand costs per world object, in user instructions (the snapshot
    /// and the restore, all the threads they use): None where this process may not count them.
    fn stand_instructions(buildings: usize) -> Option<f64> {
        let mut g = world(buildings)?;
        let (net_data, data, spec, scenes) = crate::game::test_tables()?;
        let Some(counter) = Instr::open_inherit() else {
            eprintln!("SKIPPED the instruction budget of the hot update's stand: this process may not count instructions");
            return None;
        };
        let objects = g.wos.len();
        let before = counter.read();
        let text = crate::handoff::snapshot(&mut g).unwrap();
        let restored = crate::handoff::restore(net_data, data, spec, &text, scenes).unwrap();
        assert_eq!(restored.wos.len(), objects);
        Some((counter.read() - before) as f64 / objects as f64)
    }

    /// What a hot update's world stand costs in instructions: the largest cost the players feel.
    #[test]
    fn the_stand_of_a_hot_update_stays_within_its_budget() {
        let Some(per) = stand_instructions(1600) else { return };
        assert!(per <= 14_000.0, "a snapshot and its restore cost {per:.0} instructions per world object, the budget is 14000 (docs/performance.md)");
    }

    #[test]
    #[ignore]
    fn numbers() {
        for (buildings, players, action_ms) in [(0, 0, 0), (400, 0, 0), (400, 4, 0), (400, 4, 200), (1600, 0, 0), (1600, 4, 200)] {
            let Some((allocs, instr, objects)) = tick_cost(buildings, players, action_ms, 60.0) else { return };
            println!("budget: {buildings} buildings ({objects} world objects), {players} players acting every {action_ms} ms: {allocs:.2} allocations, {} instructions per tick", instr.map_or("-".into(), |i| format!("{i:.0}")));
        }
        let mut g = world(400).unwrap();
        let (snapshot, save) = (crate::handoff::snapshot(&mut g).unwrap().len(), g.save_text().len());
        join(&mut g, 90, "Joiner");
        let joined: usize = g.net.out.iter().filter(|(to, _, _)| *to == crate::net::Target::Only(90)).map(|(_, _, b)| b.len()).sum();
        println!("budget: {} world objects: snapshot {snapshot} bytes, save {save} bytes, join {joined} bytes", g.wos.len());
        println!("budget: a hot update's stand: {:?} instructions per world object", stand_instructions(1600));
    }
}
