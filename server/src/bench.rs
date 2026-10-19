//! Tick benchmark on a real save, with a fixed clock and seed, and a fingerprint of everything
//! the server sends and saves (an optimization must leave the fingerprint unchanged):
//!
//!   PCDS_BENCH_SAVE=world.json PCDS_RNG_SEED=1 [PCDS_BENCH_SECONDS=300] [PCDS_BENCH_PLAYERS=4] \
//!     [PCDS_BENCH_ACTION_MS=200] [PCDS_BENCH_DUMP=messages.txt] cargo test --release bench -- --ignored --nocapture
//!
//! PCDS_BENCH_DUMP writes every message (the joins', then per tick, sorted) and the final save, to compare two
//! runs (two builds, two table sets) line by line.
//!
//! The loop steps like the server's (a tick every 5 ms); players walk in circles, sending a
//! transform every network tick. With PCDS_BENCH_ACTION_MS they also act that often, as the
//! load test's players do (load.rs): craft, drop, pick up, build, eat, drink, mine, chat, sort.
//! "cpu at real time" is the share of one core the ticks would use running live.
//!
//! `handoff` (the same variables, plus PCDS_BENCH_HANDOFF=seconds, default half the run) runs
//! the loop twice: straight through, and with the game snapshotted and rebuilt from the
//! snapshot every that many seconds (handoff.rs). The two fingerprints must be equal. It then
//! puts in, one part at a time, what a game loaded from the save alone has, to show which
//! runtime state a handoff must carry (docs/hot-update.md).

use std::hash::{Hash, Hasher};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::data::stable_hash;
use crate::game::{tests::join, Game};
use crate::protocol::{NETWORK_TRANSFORM, RPC};
use crate::save::SaveFile;
use crate::wire::Writer;

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
}

fn env() -> Option<Env> {
    let path = std::env::var("PCDS_BENCH_SAVE").expect("bench: set PCDS_BENCH_SAVE");
    let dir = crate::test_data()?;
    let var = |k: &str| std::env::var(k).ok().and_then(|v| v.parse::<f64>().ok());
    Some(Env { path, dir, seconds: var("PCDS_BENCH_SECONDS").unwrap_or(300.0), players: var("PCDS_BENCH_PLAYERS").unwrap_or(0.0) as u64, action_ms: var("PCDS_BENCH_ACTION_MS").unwrap_or(0.0) as u64, dump: std::env::var("PCDS_BENCH_DUMP").ok() })
}

fn load(e: &Env) -> Game {
    let (net_data, data, spec) = crate::tables(e.dir);
    let scenes = crate::scene_tables(e.dir, &net_data, &data);
    Game::with_clock(net_data, data, spec, SaveFile::load(&e.path).unwrap(), scenes, Some(0.0))
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
    save: usize,
    /// The sorted messages of the tick asked for.
    dumped: Vec<String>,
    /// The game at the end.
    game: Game,
}

/// The bench loop; `swap` may replace the game after a tick, every `swap_every` seconds.
fn run(mut g: Game, e: &Env, swap_every: f64, mut swap: impl FnMut(Game) -> Game, dump: Option<usize>) -> Run {
    g.sim_time = Some(0.0);
    let mut h = std::collections::hash_map::DefaultHasher::new();
    let objects: Vec<u64> = (1..=e.players).map(|c| join(&mut g, c, &format!("Bench{c}"))).collect();
    let mut players: Vec<Player> = (1..=e.players).map(|c| Player { rng: 0x9e37_79b9_7f4a_7c15 ^ c.wrapping_mul(0x2545_f491_4f6c_dd1d), ..Default::default() }).collect();
    let (mut messages, _, joined) = fingerprint(&mut h, &mut g.net.out);
    let (step, net_tick, act_every) = (0.005, 1.0 / g.net_data.tick_rate as f64, e.action_ms as f64 / 1000.0);
    let (mut t, mut next_move, mut next_act, mut next_swap, mut busy, mut seq) = (0.0, 0.0, act_every, swap_every, Duration::ZERO, 0);
    let (mut ticks, mut dumped) = (Vec::new(), Vec::new());
    let mut file = e.dump.as_ref().map(|p| std::io::BufWriter::new(std::fs::File::create(p).unwrap()));
    if let Some(f) = &mut file {
        use std::io::Write;
        joined.iter().for_each(|line| writeln!(f, "join {line}").unwrap());
    }
    while t < e.seconds {
        t += step;
        g.sim_time = Some(t);
        let started = Instant::now();
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
                g.receive(i as u64 + 1, &crate::wire::batch(&[(NETWORK_TRANSFORM, w.0)]));
            }
        }
        if e.action_ms > 0 && t >= next_act {
            next_act += act_every;
            seq += 1;
            for (i, object) in objects.iter().enumerate() {
                act(&mut g, i as u64 + 1, *object, &mut players[i], seq);
            }
        }
        g.tick();
        busy += started.elapsed();
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
    if let Some(f) = &mut file {
        use std::io::Write;
        writeln!(f, "save {save}").unwrap();
    }
    Run { fingerprint: h.finish(), ticks, messages, busy, save: save.len(), dumped, game: g }
}

/// The load test's players (three, acting every 200 ms) for 20 s on the test world, played
/// straight and with the game handed off (snapshot, restore) every 5 s: everything sent and
/// the final save are the same. The bench and handoff runs below do this on a real save.
#[test]
fn a_played_session_ends_the_same_through_handoffs() {
    let Some(start) = crate::game::test_game(crate::game::tests::save()) else { return };
    let e = Env { path: String::new(), dir: crate::test_data().unwrap(), seconds: 20.0, players: 3, action_ms: 200, dump: None };
    // A copy of a game as a new bank makes it (one start for both runs: the same random state).
    let copy = |g: &Game| {
        let (net_data, data, spec, scenes) = crate::game::test_tables().unwrap();
        crate::handoff::restore(net_data, data, spec, &crate::handoff::snapshot(g).unwrap(), scenes).unwrap()
    };
    let a = run(copy(&start), &e, f64::INFINITY, |g| g, None);
    let b = run(copy(&start), &e, 5.0, |old| copy(&old), None);
    // They played: built, crafted, chatted.
    let containers = a.game.wos.values().filter(|w| a.game.data.groups[w.group].id == "Container1").count();
    assert!(containers >= 3, "{containers} containers built");
    assert!(a.game.inventories.values().any(|i| i.items.len() >= 2), "crafted items in a backpack");
    assert_eq!(a.fingerprint, b.fingerprint, "{}", divergence(&a, &b, |d| (run(copy(&start), &e, f64::INFINITY, |g| g, d), run(copy(&start), &e, 5.0, |old| copy(&old), d))));
}

#[test]
#[ignore]
fn bench() {
    let Some(e) = env() else { return };
    let loaded = Instant::now();
    let g = load(&e);
    println!("bench: load {:?}, {} world objects, {} network objects", loaded.elapsed(), g.wos.len(), g.net.objects.len());
    let r = run(g, &e, f64::INFINITY, |g| g, None);
    let per_tick = r.busy / r.ticks.len() as u32;
    println!(
        "bench: {} s simulated, {} players, {} ticks: {per_tick:?} per tick, cpu at real time {:.2}% of a core, {} messages, save {} bytes, fingerprint {:016x}",
        e.seconds,
        e.players,
        r.ticks.len(),
        100.0 * r.busy.as_secs_f64() / e.seconds,
        r.messages,
        r.save,
        r.fingerprint
    );
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
        run(load(&e), &e, every, |old| {
            // The new bank has its tables loaded before the handoff starts.
            let (net_data, data, spec) = tables();
            let scenes = crate::scene_tables(e.dir, &net_data, &data);
            let started = Instant::now();
            let text = crate::handoff::snapshot(&old).unwrap_or_else(|err| panic!("snapshot: {err}"));
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
        // A snapshot: the live one's head line, then the changed state.
        let head = crate::handoff::snapshot(&load(&e)).unwrap();
        let text = format!("{}\n{doc}", head.split_once('\n').unwrap().0);
        let quiet = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut first = true;
            run(load(&e), &e, every, |g| {
                if !std::mem::take(&mut first) {
                    return g;
                }
                let (net_data, data, spec) = tables();
                {
                let scenes = crate::scene_tables(e.dir, &net_data, &data);
                crate::handoff::restore(net_data, data, spec, &text, scenes)
            }.unwrap_or_else(|err| panic!("restore: {err}"))
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

/// Resident memory of this process, in KiB.
fn rss_kib() -> u64 {
    std::fs::read_to_string("/proc/self/statm").ok().and_then(|s| s.split_whitespace().nth(1)?.parse::<u64>().ok()).unwrap_or(0) * 4
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
    let t = Instant::now();
    let save = SaveFile::load(&e.path).unwrap();
    println!("scale: tables {} MiB; read+parse save {:?}, {} objects", tables_rss >> 10, t.elapsed(), save.objects.len());
    let t = Instant::now();
    let mut g = Game::with_clock(net_data, data, spec, save, scenes, Some(0.0));
    println!("scale: Game::new {:?}, {} world objects, {} network objects, rss {} MiB (+{} MiB)", t.elapsed(), g.wos.len(), g.net.objects.len(), rss_kib() >> 10, (rss_kib() - tables_rss) >> 10);
    let t = Instant::now();
    let snapshot = g.snapshot();
    let took = t.elapsed();
    let peak = rss_kib();
    let t = Instant::now();
    let text = snapshot.to_text();
    println!("scale: snapshot (JSON trees) {took:?}, to_text {:?}, {} bytes, rss {} MiB", t.elapsed(), text.len(), peak >> 10);
    drop(snapshot);
    let before = rss_kib();
    let t = Instant::now();
    let direct = g.save_text();
    println!("scale: save_text (no trees) {:?}, rss {} MiB (+{} MiB)", t.elapsed(), rss_kib() >> 10, rss_kib().saturating_sub(before) >> 10);
    assert!(direct == text, "save_text is the snapshot's text");
    let snapshot = g.snapshot();
    let dir = std::env::temp_dir().join(format!("pcds-scale-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("world.json").to_string_lossy().to_string();
    let t = Instant::now();
    snapshot.write(&path).unwrap();
    println!("scale: write (to_text + write_atomic) {:?}", t.elapsed());
    let t = Instant::now();
    crate::save::backup(&path, Duration::ZERO, 24).unwrap();
    println!("scale: backup {:?}", t.elapsed());
    let t = Instant::now();
    let back = SaveFile::load(&path).unwrap();
    println!("scale: reload {:?} ({} objects)", t.elapsed(), back.objects.len());
    let t = Instant::now();
    let h = crate::handoff::snapshot(&g).unwrap();
    println!("scale: handoff snapshot {:?}, {} bytes", t.elapsed(), h.len());
    std::fs::remove_dir_all(dir).ok();
}
