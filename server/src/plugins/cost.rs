//! What plugins cost. Not run with the tests (they are `#[ignore]`d, and a timing is no test):
//!
//!   make plugin-bench          (or: cargo test --release plugins::cost -- --ignored --nocapture)
//!
//! `PCDS_BENCH_SAVE=world.json` (and the game tables, PCDS_TEST_DATA) adds the tick cost on a real
//! world; without them the parts that need a game are skipped. The numbers are the best of a few
//! rounds, to leave out what a busy machine adds.

use std::hint::black_box;
use std::time::Instant;

use pcds_plugin_api::Event;

use super::tests::{Mock, boot, fresh_dir, install, module};
use super::*;

fn best(rounds: u32, n: u64, mut f: impl FnMut()) -> f64 {
    (0..rounds)
        .map(|_| {
            let t = Instant::now();
            f();
            t.elapsed().as_nanos() as f64 / n as f64
        })
        .fold(f64::MAX, f64::min)
}

fn noop() -> String {
    module("(i32.const 0)", "")
}

fn says() -> String {
    module("(drop (call $say (i32.const 0) (i32.const 5))) (i32.const 0)", "")
}

/// Asks the world about itself at each event: the answer is built and copied into the module's memory.
fn queries() -> String {
    module("(drop (call $query (i32.const 1) (i32.const 0) (i32.const 0) (i32.const 8192) (i32.const 1024))) (i32.const 0)", "")
}

const CHAT: &str = "event.chat";
const TICK_EVERY_TICK: &str = "[events]\nsubscribe = [\"tick\"]\n[budget]\ntick_every_ms = 5";

fn tick_event(ms: i64) -> Event<'static> {
    Event::Tick { game_ms: ms, tick: 0, players: 4, since_ms: 5 }
}

/// Events per second through the host alone: what a plugin costs per event, with the host's whole
/// path in it (the event bytes, the module's allocation, the call, the module's answer, and the
/// actions applied by a mock that only counts them).
#[test]
#[ignore]
fn events_per_second() {
    println!("\nplugin host: cost of one event (host only, no game), wasmi, one thread");
    let chat = Event::Chat { client: 1, name: "Ann", text: "hello everyone" };
    let n = 200_000u64;
    let report = |label: &str, host: &mut Host, event: &Event<'_>| {
        let mut world = Mock::default();
        // The game clock only goes forward, across the rounds too: a plugin is due every 5 ms of it.
        let mut clock = 0i64;
        let ns = best(5, n, || {
            for _ in 0..n {
                clock += 5;
                black_box(host.run(&mut world, event, clock));
                world.applied.clear();
            }
        });
        println!("  {label:<52} {ns:>9.0} ns/event  {:>12.0} events/s", 1e9 / ns);
    };
    let dir = fresh_dir("cost-one");
    install(&dir, "noop", &noop(), &[CHAT, "chat.say"], "[events]\nsubscribe = [\"chat\"]", None);
    report("1 module, does nothing", &mut boot(&dir), &chat);
    let dir = fresh_dir("cost-say");
    install(&dir, "says", &says(), &[CHAT, "chat.say"], "[events]\nsubscribe = [\"chat\"]", None);
    report("1 module, says one line (an action)", &mut boot(&dir), &chat);
    let dir = fresh_dir("cost-query");
    install(&dir, "queries", &queries(), &[CHAT, "world.read"], "[events]\nsubscribe = [\"chat\"]", None);
    report("1 module, reads the world (mock world)", &mut boot(&dir), &chat);
    let dir = fresh_dir("cost-ten");
    for i in 0..10 {
        install(&dir, &format!("noop{i}"), &noop(), &[CHAT], "[events]\nsubscribe = [\"chat\"]", None);
    }
    report("10 modules, each does nothing", &mut boot(&dir), &chat);
    let dir = fresh_dir("cost-ticks");
    for i in 0..10 {
        install(&dir, &format!("tick{i}"), &noop(), &["event.tick"], TICK_EVERY_TICK, None);
    }
    report("10 modules, a tick each (as every tick would)", &mut boot(&dir), &tick_event(0));
    // A command reaches its owner only: the other modules are not asked.
    let dir = fresh_dir("cost-command");
    for i in 0..10 {
        install(&dir, &format!("cmd{i}"), &noop(), &[CHAT], &format!("[[command]]\nname = \"c{i}\""), None);
    }
    let mut host = boot(&dir);
    report("10 modules with a command each, one command", &mut host, &Event::Command { client: 1, name: "Ann", command: "c3", args: vec![], text: "!c3" });
    // The rules-file kind.
    let dir = fresh_dir("cost-data");
    std::fs::create_dir_all(dir.join("rules")).unwrap();
    std::fs::write(dir.join("rules/plugin.toml"), "[plugin]\nid = \"rules\"\nname = \"r\"\nversion = \"1.0.0\"\nkind = \"data\"\nentry = \"rules.toml\"\nabi = 1\ncapabilities = [\"event.chat\", \"chat.say\"]\n").unwrap();
    std::fs::write(dir.join("rules/rules.toml"), "[[command]]\nname = \"hi\"\nreply = \"Hello {name}\"\n").unwrap();
    approve(&dir, "rules", None).unwrap();
    report("1 rules file, a command that replies", &mut boot(&dir), &Event::Command { client: 1, name: "Ann", command: "hi", args: vec![], text: "!hi" });
}

/// How long it takes to get a plugin going: reading and compiling its module, and starting an
/// instance (which a hot update does for each plugin, and a crash does again).
#[test]
#[ignore]
fn start_cost() {
    let engine = wasm::engine();
    for (label, path) in [("hello-stats (44 KB, Rust)", std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../examples/plugins/hello-stats/plugin.wasm"))] {
        let bytes = std::fs::read(&path).unwrap();
        let t = Instant::now();
        let mut module = None;
        for _ in 0..50 {
            module = Some(wasm::compile(&engine, &bytes).unwrap().0);
        }
        let compile = t.elapsed() / 50;
        let module = module.unwrap();
        let limits = limits::Limits::resolve(&Default::default(), &Default::default());
        let t = Instant::now();
        for _ in 0..200 {
            black_box(wasm::Wasm::new(&engine, &module, Ctx::new("t", Grant::default()), limits, Default::default()).unwrap());
        }
        let instantiate = t.elapsed() / 200;
        println!("\nplugin start: {label}: compile and validate {compile:?}, new instance {instantiate:?}");
    }
}

/// An event nobody listens to costs a load and a test.
#[test]
#[ignore]
fn zero_plugins_cost_nothing() {
    let Some((mut g, _)) = crate::testkit::world() else { return println!("no game tables: skipped") };
    assert_eq!(g.ext.plugins.interest, 0);
    let n = 100_000_000u64;
    let empty = best(5, n, || {
        for i in 0..n {
            black_box(i);
        }
    });
    let tick = best(5, n, || {
        for i in 0..n {
            black_box(i);
            crate::events::tick(black_box(&mut g));
        }
    });
    let rpc = best(5, n / 10, || {
        for i in 0..n / 10 {
            black_box(i);
            black_box(crate::events::rpc_before(black_box(&mut g), 1, 2, "ChatHandler.SendChatMessageServerRpc", &[]));
        }
    });
    println!("\nzero plugins: an empty loop {empty:.2} ns; events::tick {tick:.2} ns; events::rpc_before {rpc:.2} ns per call (the difference to the empty loop is what nobody listening costs)");
}

/// The game's tick, with 0, 1 and 10 plugins that are called at every tick, on the bench world.
#[test]
#[ignore]
fn tick_cost_on_a_real_world() {
    let (Some(path), Some(dir_tables)) = (std::env::var("PCDS_BENCH_SAVE").ok(), crate::test_data()) else { return println!("PCDS_BENCH_SAVE or the game tables are missing: skipped") };
    let make = |plugins: &[(&str, String, &[&str])]| {
        let (net_data, data, spec) = crate::tables(dir_tables);
        let scenes = crate::scene_tables(dir_tables, &net_data, &data);
        let mut g = crate::game::Game::with_clock(net_data, data, spec, crate::save::SaveFile::load(&path).unwrap(), scenes, Some(0.0));
        for c in 1..=4 {
            crate::game::tests::join(&mut g, c, &format!("P{c}"));
        }
        let dir = fresh_dir("cost-tick");
        for (i, (_, wat, caps)) in plugins.iter().enumerate() {
            install(&dir, &format!("p{i:02}"), wat, caps, TICK_EVERY_TICK, None);
        }
        attach(&mut g, prepare(&dir, "world"));
        g
    };
    let ticks = 20_000u64;
    let run = |g: &mut crate::game::Game| {
        // The clock steps like the bench's: a tick every 5 ms of game time.
        let mut t = g.sim_time.unwrap_or(0.0);
        g.net.out.clear();
        best(5, ticks, || {
            for _ in 0..ticks {
                t += 0.005;
                g.sim_time = Some(t);
                g.tick();
                g.net.out.clear();
            }
        })
    };
    println!("\ntick cost on {path} with 4 players; {ticks} ticks per round, best of 5 (a tick is 5 ms)");
    let tick_caps: &[&str] = &["event.tick"];
    let query_caps: &[&str] = &["event.tick", "world.read"];
    let cases: Vec<(&str, Vec<(&str, String, &[&str])>)> = vec![
        ("0 plugins", vec![]),
        ("1 plugin that does nothing, called at every tick", vec![("a", noop(), tick_caps)]),
        ("10 plugins that do nothing, called at every tick", (0..10).map(|_| ("a", noop(), tick_caps)).collect()),
        ("1 plugin that reads the world at every tick", vec![("a", queries(), query_caps)]),
        ("10 plugins that read the world at every tick", (0..10).map(|_| ("a", queries(), query_caps)).collect()),
    ];
    let mut base = None;
    for (label, plugins) in cases {
        let mut g = make(&plugins);
        let ns = run(&mut g);
        let base_ns = *base.get_or_insert(ns);
        println!("  {label:<52} {:>9.1} us/tick  ({:+.2} us, {:.3}% of the 5 ms tick)", ns / 1000.0, (ns - base_ns) / 1000.0, (ns - base_ns) / 5e6 * 100.0);
    }
}

/// What the timer scan costs when nothing changed: a few `stat`s.
#[test]
#[ignore]
fn idle_scan_cost() {
    let dir = fresh_dir("cost-scan");
    for i in 0..10 {
        install(&dir, &format!("p{i}"), &noop(), &["event.tick"], TICK_EVERY_TICK, None);
    }
    let host = boot(&dir);
    let n = 20_000u64;
    let ns = best(5, n, || {
        for _ in 0..n {
            black_box(host.signature());
        }
    });
    println!("\nidle scan of a folder with 10 plugins: {:.1} us (every {} s)", ns / 1000.0, POLL_EVERY.as_secs());
}
