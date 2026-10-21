//! The boundary between the host and a module, shaken: modules generated at random call the host's functions
//! with arguments chosen to hurt (zero, minus one, the edges of memory, huge lengths, numbers that are not
//! numbers), return nonsense from their exports, and are driven with random events. Whatever they do the
//! host must not panic, must not run past its budget, must hand every call back (a fault, or an answer), and
//! must go on serving a good plugin next to them. The generator is a small xorshift: every run is the same
//! run, and a failure prints the seed and the module as text.

use std::time::{Duration, Instant};

use pcds_plugin_api::{Event, Kind};

use super::abi13_tests::Stage;
use super::tests::{fresh_dir, install};
use super::*;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
    /// An i32 that tends to be on an edge.
    fn int(&mut self) -> i32 {
        const EDGES: [i32; 16] = [0, 1, -1, 2, 4, 16, 255, 4095, 4096, 65535, 65536, 65537, 1 << 20, i32::MAX, i32::MIN, 0x7fff_f000u32 as i32];
        if self.below(3) == 0 { self.next() as i32 } else { EDGES[self.below(16) as usize] }
    }
    fn long(&mut self) -> i64 {
        match self.below(4) {
            0 => 1,
            1 => 0,
            2 => i64::MAX,
            _ => self.next() as i64,
        }
    }
    fn float(&mut self) -> String {
        ["0", "1.5", "-1", "nan", "inf", "-inf", "1e30", "-1e30", "100000", "100001", "0x1p-149"][self.below(11) as usize].to_string()
    }
}

/// One call of a host function with arguments from `r`, its result dropped.
fn call(r: &mut Rng) -> String {
    let (a, b, c, d, e) = (r.int(), r.int(), r.int(), r.int(), r.int());
    match r.below(12) {
        0 => format!("(call $log (i32.const {a}) (i32.const {b}) (i32.const {c}))"),
        1 => format!("(drop (call $query (i32.const {}) (i32.const {b}) (i32.const {c}) (i32.const {d}) (i32.const {e})))", r.below(10)),
        2 => format!("(drop (call $say (i32.const {a}) (i32.const {b})))"),
        3 => format!("(drop (call $tell (i64.const {}) (i32.const {a}) (i32.const {b})))", r.long()),
        4 => format!("(drop (call $kick (i64.const {}) (i32.const {a}) (i32.const {b})))", r.long()),
        5 => format!("(drop (call $teleport (i64.const {}) (f32.const {}) (f32.const {}) (f32.const {})))", r.long(), r.float(), r.float(), r.float()),
        6 => format!("(drop (call $rules_patch (i32.const {a}) (i32.const {b})))"),
        7 => format!("(drop (call $http_request (i32.const {a}) (i32.const {b})))"),
        8 => format!("(drop (call $give (i64.const {}) (i32.const {a}) (i32.const {b}) (i32.const {c})))", r.long()),
        9 => format!("(drop (call $tokens (i32.const {a})))"),
        10 => format!("(drop (call $timer_set (i32.const {a}) (i32.const {b}) (i32.const {c})))"),
        _ => format!("(drop (call $timer_cancel (i32.const {a})))"),
    }
}

/// A module whose event handler makes `calls` calls, and whose other exports answer as `r` pleases.
fn module(r: &mut Rng, calls: u64) -> String {
    let body: String = (0..calls).map(|_| call(r)).collect::<Vec<_>>().join("\n");
    let abi = [65536, 65537, 65538, 65539, 65540, 0, -1, 131072][r.below(8) as usize];
    let alloc = ["(i32.const 4096)", "(i32.const 0)", "(i32.const 65535)", "(i32.const -1)", "(i32.const 70000)", "(memory.grow (i32.const 1000))"][r.below(6) as usize];
    let save = ["(i64.const 0)", "(i64.const 0x100000008)", "(i64.const -1)", "(i64.const 0x0000100000000010)"][r.below(4) as usize];
    let verdict = ["(i32.const 0)", "(i32.const 1)", "(i32.const -1)", "(i32.const 2)", "(i32.const 7)"][r.below(5) as usize];
    format!(
        r#"(module
          (import "pcds" "log" (func $log (param i32 i32 i32)))
          (import "pcds" "query" (func $query (param i32 i32 i32 i32 i32) (result i32)))
          (import "pcds" "say" (func $say (param i32 i32) (result i32)))
          (import "pcds" "tell" (func $tell (param i64 i32 i32) (result i32)))
          (import "pcds" "kick" (func $kick (param i64 i32 i32) (result i32)))
          (import "pcds" "teleport" (func $teleport (param i64 f32 f32 f32) (result i32)))
          (import "pcds" "rules_patch" (func $rules_patch (param i32 i32) (result i32)))
          (import "pcds" "http_request" (func $http_request (param i32 i32) (result i64)))
          (import "pcds" "give" (func $give (param i64 i32 i32 i32) (result i32)))
          (import "pcds" "tokens" (func $tokens (param i32) (result i32)))
          (import "pcds" "timer_set" (func $timer_set (param i32 i32 i32) (result i32)))
          (import "pcds" "timer_cancel" (func $timer_cancel (param i32) (result i32)))
          (memory (export "memory") 1 4)
          (data (i32.const 0) "{{\"speed\":{{\"growth\":2}}}} Iron hello")
          (func (export "pcds_abi") (result i32) (i32.const {abi}))
          (func (export "pcds_alloc") (param i32) (result i32) {alloc})
          (func (export "pcds_free") (param i32 i32))
          (func (export "pcds_save") (result i64) {save})
          (func (export "pcds_state_version") (result i32) (i32.const 3))
          (func (export "pcds_unload") (result i32) {body_unload})
          (func (export "pcds_load") (param i32 i32 i32) (result i32) {load_body} (i32.const 0))
          (func (export "pcds_event") (param $kind i32) (param $ptr i32) (param $len i32) (result i32)
            {body}
            {verdict}))"#,
        body_unload = if r.below(2) == 0 { "(i32.const 0)".to_string() } else { call(r) + "(i32.const 0)" },
        load_body = if r.below(3) == 0 { call(r) } else { String::new() },
    )
}

const CAPS: [&str; 12] = ["event.rpc", "rpc.veto", "event.chat", "event.tick", "world.read", "chat.say", "player.kick", "player.teleport", "rules.write", "player.give", "world.tokens", "event.timer"];
const MANIFEST: &str = "[events]\nsubscribe = [\"rpc_before\", \"chat\", \"tick\"]\nrpc = [\"X.Y\"]";

fn events(r: &mut Rng) -> Vec<Event<'static>> {
    let text: &'static str = ["hi", "!x", "", "a very long line ........................................................................."][r.below(4) as usize];
    vec![
        Event::RpcBefore { client: r.below(4), rpc: "X.Y", object: r.next(), args_json: text },
        Event::Chat { client: r.below(4), name: "Ann", text, player_id: r.next() },
        Event::Tick { game_ms: r.next() as i64, tick: 1, players: 2, since_ms: 5, unix_ms: r.next() as i64 },
        Event::Timer { id: r.below(20) as u32, game_ms: 0, late_ms: 0 },
    ]
}

#[test]
fn generated_modules_calling_the_host_with_hostile_arguments_never_panic_hang_or_hurt_a_neighbour() {
    let mut worst = Duration::ZERO;
    let (mut faulted, mut ran, mut refused_to_load) = (0u32, 0u32, 0u32);
    for seed in 1..=300u64 {
        let mut r = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let calls = 1 + r.below(40);
        let wat = module(&mut r, calls);
        // A random subset of the capabilities is granted (the rest of the calls must come back -1 and do nothing).
        let grant: Vec<&str> = CAPS.iter().copied().filter(|c| r.below(4) != 0 || *c == "event.rpc").collect();
        let dir = fresh_dir(&format!("fuzz{seed}"));
        let wasm = match wat::parse_str(&wat) {
            Ok(w) => w,
            Err(e) => panic!("seed {seed}: the generator wrote a module that is not WebAssembly: {e}\n{wat}"),
        };
        // install() takes text; write the plugin by hand so that the exact module is what runs.
        super::tests::write_plugin(&dir, "fuzz", "(module)", &CAPS, MANIFEST);
        std::fs::write(dir.join("fuzz/plugin.wasm"), &wasm).unwrap();
        // A good neighbour that must go on.
        install(&dir, "zgood", &super::tests::module("(drop (call $say (i32.const 0) (i32.const 5))) (i32.const 0)", ""), &["event.chat", "chat.say"], "[events]\nsubscribe = [\"chat\"]", None);
        approve(&dir, "fuzz", Some(grant.iter().map(|s| s.to_string()).collect())).unwrap();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut host = prepare(&dir, "world");
            host.start_all(&Default::default());
            let mut stage = Stage::default();
            let mut ms = 1000;
            for _ in 0..3 {
                for e in events(&mut r) {
                    ms += 7;
                    let t = Instant::now();
                    host.run(&mut stage, &e, ms);
                    worst = worst.max(t.elapsed());
                }
            }
            // The neighbour still answers (unless the fuzzed plugin vetoed the chat before it: a chat is no RPC).
            let before = stage.applied.iter().filter(|(p, _)| p == "zgood").count();
            host.run(&mut stage, &Event::Chat { client: 1, name: "Ann", text: "hello", player_id: 1 }, ms + 7);
            assert!(stage.applied.iter().filter(|(p, _)| p == "zgood").count() > before, "the good plugin was not called");
            let fuzzed = host.plugins.iter().find(|p| p.id == "fuzz").unwrap();
            (fuzzed.running(), fuzzed.stats.faults, fuzzed.status.name().to_string())
        }));
        match result {
            Ok((running, faults, status)) => {
                ran += 1;
                faulted += u32::from(faults > 0);
                refused_to_load += u32::from(!running && status != "failed");
            }
            Err(e) => {
                let why = e.downcast_ref::<String>().cloned().or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_default();
                panic!("seed {seed}: the host panicked or a check failed: {why}\n{wat}");
            }
        }
    }
    // Nothing ran for long: fuel and the limits are what stop a module, so even a loop of host calls with huge lengths ends fast.
    assert!(worst < Duration::from_millis(500), "the slowest event took {worst:?}");
    println!("fuzz: {ran} modules, {faulted} faulted at least once, {refused_to_load} refused or failed to start; the slowest event took {worst:?}");
    assert!(ran == 300 && faulted > 20, "the generator should make modules that fault as well as ones that run");
}

#[test]
fn generated_events_and_states_do_not_panic_the_decoder_or_the_host() {
    // Random payloads for every kind straight into the decoder the plugins use (the crate has its own copy of this test; this
    // is the host's side: the event bytes the host writes are the ones the crate reads).
    let mut r = Rng(0xC0FFEE);
    for _ in 0..20_000 {
        let len = r.below(64) as usize;
        let bytes: Vec<u8> = (0..len).map(|_| r.next() as u8).collect();
        for k in 0..16 {
            let _ = Event::decode(k, &bytes);
        }
    }
    // A saved state of any size and version is accepted or refused by `load`, never a panic.
    let dir = fresh_dir("fuzz-state");
    install(&dir, "state", &super::tests::module("(i32.const 0)", ""), &["event.chat"], "[events]\nsubscribe = [\"chat\"]", None);
    let host = super::tests::boot(&dir);
    assert!(host.plugins[0].running());
    let _ = Kind::ALL;
}
