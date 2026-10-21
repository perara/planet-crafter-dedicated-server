//! The host boundary of a WebAssembly plugin: a module made from the input calls every host
//! function with pointers, lengths and numbers from the input (inside its memory, on its edge and
//! far outside it), grows and fills its memory and burns fuel. Whatever it does, the call ends
//! with a verdict or a fault (never a panic of the host), within the fuel and the time of the
//! budget, and the memory it holds stays under the limit.
use super::wasm::{Gauge, Wasm, compile, engine};
use super::ctx::{Action, Ctx, Grant, NetCtx, World};
use super::limits::Limits;
use super::runtime::Runtime;
use wasmi::Engine;
use crate::plugins::net::{Pattern, Policy};
use pcds_plugin_api::query::{PlayerInfo, WorldInfo};
use pcds_plugin_api::{Event, Writer};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

static ENGINE: OnceLock<Engine> = OnceLock::new();

struct Still;

impl World for Still {
    fn info(&self) -> WorldInfo {
        WorldInfo { planet: "Prime".into(), players: 2, game_ms: 61_000, ..Default::default() }
    }
    fn players(&self) -> Vec<PlayerInfo> {
        vec![PlayerInfo { client: 1, name: "Ann".into(), ..Default::default() }]
    }
    fn rules(&self) -> String {
        "{\"speed\":{\"growth\":2.0}}".into()
    }
}

struct Cur<'a>(&'a [u8]);

impl Cur<'_> {
    fn u8(&mut self) -> u8 {
        let (b, rest) = self.0.split_first().map_or((0, self.0), |(b, r)| (*b, r));
        self.0 = rest;
        b
    }
    fn u32(&mut self) -> u32 {
        u32::from_le_bytes([self.u8(), self.u8(), self.u8(), self.u8()])
    }
    /// An address or a length: the places a bug is.
    fn n(&mut self, pages: u32) -> u32 {
        let memory = pages.saturating_mul(65536);
        match self.u8() % 16 {
            0 => 0,
            1 => 1,
            2 => 4096,
            3 => 4096 + self.u8() as u32,
            4 => 65535,
            5 => 65536,
            6 => memory,
            7 => memory.wrapping_sub(self.u8() as u32),
            8 => memory.wrapping_add(self.u8() as u32),
            9 => 0x7fff_ffff,
            10 => 0x8000_0000,
            11 => 0xffff_ffff,
            12 => self.u8() as u32,
            13 => 4096 + (self.u8() as u32) * 16,
            _ => self.u32(),
        }
    }
}

fn wat(c: &mut Cur, pages: u32, memory_max: u32) -> String {
    let mut body = String::new();
    let blob: String = c.0.iter().take(600).map(|b| format!("\\{b:02x}")).collect();
    let ops = 1 + c.u8() % 24;
    for _ in 0..ops {
        match c.u8() % 14 {
            0 => body += &format!("(call $log (i32.const {}) (i32.const {}) (i32.const {}))\n", c.u8() as i32 % 5, c.n(pages) as i32, c.n(pages) as i32),
            1 => body += &format!("(drop (call $query (i32.const {}) (i32.const {}) (i32.const {}) (i32.const {}) (i32.const {})))\n", c.u8() % 5, c.n(pages) as i32, c.n(pages) as i32, c.n(pages) as i32, c.n(pages) as i32),
            2 => body += &format!("(drop (call $say (i32.const {}) (i32.const {})))\n", c.n(pages) as i32, c.n(pages) as i32),
            3 => body += &format!("(drop (call $tell (i64.const {}) (i32.const {}) (i32.const {})))\n", c.u8() as i64 % 4, c.n(pages) as i32, c.n(pages) as i32),
            4 => body += &format!("(drop (call $kick (i64.const {}) (i32.const {}) (i32.const {})))\n", c.u8() as i64 % 4, c.n(pages) as i32, c.n(pages) as i32),
            5 => body += &format!("(drop (call $teleport (i64.const {}) (f32.const {}) (f32.const {}) (f32.const {})))\n", c.u8() as i64 % 4, f32::from_bits(c.u32()) as f64 % 1e7, f32::from_bits(c.u32()) as f64 % 1e7, f32::from_bits(c.u32()) as f64 % 1e7),
            6 => body += &format!("(drop (call $patch (i32.const {}) (i32.const {})))\n", c.n(pages) as i32, c.n(pages) as i32),
            7 => body += &format!("(drop (call $http (i32.const {}) (i32.const {})))\n", c.n(pages) as i32, c.n(pages) as i32),
            8 => body += &format!("(drop (memory.grow (i32.const {})))\n", c.u8() as i32 % 80),
            9 => body += &format!("(memory.fill (i32.const {}) (i32.const {}) (i32.const {}))\n", c.n(pages) as i32, c.u8(), c.n(pages) as i32),
            10 => body += &format!("(memory.copy (i32.const {}) (i32.const {}) (i32.const {}))\n", c.n(pages) as i32, c.n(pages) as i32, c.n(pages) as i32),
            11 => body += &format!("(local.set $i (i32.const {})) (loop $l (local.set $i (i32.sub (local.get $i) (i32.const 1))) (br_if $l (local.get $i)))\n", c.u32() >> (c.u8() % 32)),
            12 => body += &format!("(i32.store (i32.const {}) (i32.const {}))\n", c.n(pages) as i32, c.u32() as i32),
            _ => body += &format!("(drop (i32.load (i32.const {})))\n", c.n(pages) as i32),
        }
    }
    let result = c.u8() as i32 % 4 - 1;
    // The state the module hands back: where, how long (packed as pcds_save returns it), and what pcds_load says.
    let saved = ((c.n(pages) as u64) << 32) | c.n(pages) as u64;
    let loaded = c.u8() as i32 % 3 - 1;
    format!(
        r#"(module
          (import "pcds" "log" (func $log (param i32 i32 i32)))
          (import "pcds" "query" (func $query (param i32 i32 i32 i32 i32) (result i32)))
          (import "pcds" "say" (func $say (param i32 i32) (result i32)))
          (import "pcds" "tell" (func $tell (param i64 i32 i32) (result i32)))
          (import "pcds" "kick" (func $kick (param i64 i32 i32) (result i32)))
          (import "pcds" "teleport" (func $teleport (param i64 f32 f32 f32) (result i32)))
          (import "pcds" "rules_patch" (func $patch (param i32 i32) (result i32)))
          (import "pcds" "http_request" (func $http (param i32 i32) (result i64)))
          (memory (export "memory") {pages} {memory_max})
          (data (i32.const 4096) "{blob}")
          (func (export "pcds_abi") (result i32) (i32.const 65538))
          (func (export "pcds_alloc") (param i32) (result i32) (i32.const 32768))
          (func (export "pcds_free") (param i32 i32))
          (func (export "pcds_load") (param i32 i32 i32) (result i32) (i32.const {loaded}))
          (func (export "pcds_save") (result i64) (i64.const {saved}))
          (func (export "pcds_unload") (result i32) (i32.const {loaded}))
          (func (export "pcds_state_version") (result i32) (i32.const 7))
          (func (export "pcds_event") (param $kind i32) (param $ptr i32) (param $len i32) (result i32) (local $i i32)
            {body}
            (i32.const {result})))"#
    )
}

pub fn run(data: &[u8]) {
    let engine = ENGINE.get_or_init(engine);
    let mut c = Cur(data);
    let pages = [1u32, 2, 16][c.u8() as usize % 3];
    let limit_pages = [1u32, 64, 1024][c.u8() as usize % 3];
    let fuel = [10_000u64, 1_000_000, 20_000_000][c.u8() as usize % 3];
    let Ok(bytes) = wat::parse_str(wat(&mut c, pages, 1024)) else { return };
    let Ok((module, _)) = compile(engine, &bytes) else { return };
    let mut ctx = Ctx::new("fuzz", Grant::from_names(pcds_plugin_api::caps::ALL.iter().map(|(n, _)| *n)));
    let policy = Policy { patterns: ["https://discord.com", "http://127.0.0.1:9"].iter().map(|p| Pattern::parse(p, true).unwrap()).collect(), local: true, timeout: Duration::from_secs(1), max_response: 1 << 16 };
    ctx.net = Some(NetCtx { policy: Arc::new(policy), config: Arc::new([("hook".to_string(), "abc".to_string())].into()), in_flight: Arc::default() });
    let limits = Limits { fuel_per_event: fuel, fuel_per_save: fuel, memory_pages: limit_pages, tick_every_ms: 1000 };
    let gauge = Arc::new(Gauge::default());
    let started = Instant::now();
    let Ok(mut w) = Wasm::new(engine, &module, ctx, limits, gauge.clone()) else { return };
    let _ = w.start(Some(&crate::plugins::state::Blob { version: 3, bytes: data.iter().take(2048).copied().collect() }));
    for ev in [Event::Chat { client: 1, name: "Ann", text: "hi", player_id: 1 }, Event::Tick { game_ms: 5, tick: 1, players: 1, since_ms: 5, unix_ms: 1_700_000_000_000 }] {
        let mut out = Writer::new();
        ev.encode(&mut out);
        w.ctx().begin(&Still, 0);
        let r = w.event(&ev, &out.0);
        let actions = w.ctx().end();
        assert!(actions.len() <= crate::plugins::limits::ACTIONS_PER_CALL);
        for a in &actions {
            match a {
                Action::Say(t) | Action::Tell(_, t) | Action::Kick(_, t) => assert!(t.chars().count() <= crate::plugins::limits::SAY_CHARS),
                Action::Teleport(_, p) => assert!(p.iter().all(|v| v.is_finite() && v.abs() <= 100_000.0)),
                Action::Http(f) => assert!(f.url.starts_with("https://discord.com") || f.url.starts_with("http://127.0.0.1:9")),
                Action::RulesPatch(v) => assert!(v.is_object()),
                // Bounded and checked by the host (ctx.rs); the fuzz asks only that they come out.
                Action::Give(..) | Action::Tokens(_) => {}
            }
        }
        if let Ok((_, cost)) = r {
            assert!(cost.fuel <= fuel, "{} fuel used of {fuel}", cost.fuel);
        }
        let (_, held) = gauge.live();
        assert!(held <= limit_pages as usize, "{held} pages held, limit {limit_pages}");
    }
    if let Ok(Some(blob)) = w.save() {
        assert!(blob.bytes.len() <= crate::plugins::limits::MAX_STATE_BYTES);
    }
    let _ = w.unload();
    let _ = w.restart(None);
    assert!(started.elapsed() < Duration::from_secs(20), "{:?}", started.elapsed());
}
