//! The whole game on hostile client input: RPCs (well-formed from the table, or raw), raw
//! batches, transforms, variable deltas, ticks and reconnects, on the test world with two
//! players, each execution on a fresh game.
use super::*;
use crate::protocol::{self, CONNECTION_REQUEST, NETWORK_TRANSFORM, NETWORK_VARIABLE_DELTA};
use crate::wire::{self, Writer};
use serde_json::{json, Value};
use std::sync::OnceLock;

pub type Tables = (NetData, GameData, Spec, crate::Scenes);
pub static TABLES: OnceLock<(Tables, Vec<String>)> = OnceLock::new();

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
    fn u64(&mut self) -> u64 {
        (self.u32() as u64) << 32 | self.u32() as u64
    }
    fn pick(&mut self, n: usize) -> usize {
        if n == 0 { 0 } else { self.u32() as usize % n }
    }
    fn bytes(&mut self, max: usize) -> Vec<u8> {
        let n = (self.u8() as usize * max / 255).min(self.0.len());
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        a.to_vec()
    }
    fn done(&self) -> bool {
        self.0.is_empty()
    }
}

pub fn save() -> SaveFile {
    let mut save = SaveFile::empty();
    save.world = json!({"terraTokens": 0});
    save.planets = vec![json!({"planetId": "Prime"})];
    save.objects = vec![json!({"id": 208217122, "gId": "EscapePod", "pos": "558.2,2.47,494.1", "rot": "0,0,0,1", "planet": -1140328421})];
    save.stats = json!({"craftedObjects": 0});
    save.settings = json!({"planetId": "Prime", "dyingConsequencesLabel": "DropSomeItems"});
    save
}

pub fn world(t: &Tables, save: SaveFile) -> Game {
    let (n, d, s, scenes) = t.clone();
    let mut g = Game::new(n, d, s, save, scenes);
    for (client, name) in [(1u64, "Ann"), (2, "Bob")] {
        g.connect(client, None, false, None);
        let data = format!("{}|0", g.net_data.game_version);
        let mut w = Writer::new();
        w.packed_i32(0).u64(0).i32(data.len() as i32).bytes(data.as_bytes());
        g.approve(client, &w.0);
        g.players_rpc(client, "PlayerMainController.PlayerReadyServerRpc", &Args(vec![json!(0), json!(name)]));
    }
    g.net.out.clear();
    g
}

fn value(c: &mut Cur, g: &Game) -> Value {
    let ids: Vec<i64> = match c.u8() % 4 {
        0 => g.wos.keys().map(|k| *k as i64).collect(),
        1 => g.inventories.keys().map(|k| *k as i64).collect(),
        2 => g.net.objects.keys().map(|k| *k as i64).collect(),
        _ => g.data.groups.iter().map(|gr| gr.hash as i64).collect(),
    };
    let any = |c: &mut Cur| ids.get(c.pick(ids.len())).copied().unwrap_or(0);
    match c.u8() % 14 {
        0 => json!(-1),
        1 => json!(0),
        2 => json!(i32::MAX),
        3 => json!(i32::MIN),
        4 => json!(u64::MAX),
        5 => json!(1.0e30),
        6 => json!("x".repeat(c.u8() as usize * 20)),
        7 => {
            let n = c.u8() % 40;
            json!((0..n).map(|_| any(c)).collect::<Vec<_>>())
        }
        8 => json!([f32::from_bits(c.u32()), -1.0e30, 0.5, 2.0]),
        9 => json!(c.u8() % 2 == 0),
        10 => json!(f32::from_bits(c.u32())),
        11 => json!(i64::from(c.u32() as i32)),
        12 => json!(c.u64()),
        _ => json!(any(c)),
    }
}

pub fn tables() -> &'static (Tables, Vec<String>) {
    TABLES.get_or_init(|| {
        let dir = crate::fuzz_entry::data_dir();
        let (n, d, s) = crate::tables(&dir);
        let scenes = crate::scene_tables(&dir, &n, &d);
        let table: Value = serde_json::from_str(&std::fs::read_to_string(format!("{dir}/rpcs.json")).unwrap()).unwrap();
        let mut names: Vec<String> = table.as_object().unwrap().values().filter(|r| r["to"] == "server").filter_map(|r| r["name"].as_str().map(String::from)).collect();
        names.sort();
        ((n, d, s, scenes), names)
    })
}

/// A fresh game costs a deep copy of the tables (25 ms), so one game serves RESET inputs; the
/// inputs since the last reset are dumped to $PCDS_FUZZ_HISTORY (default ./history.bin) when
/// a panic ends the run, so a crash can be replayed from them (run with PCDS_FUZZ_RESET=1).
const RESET: u32 = 40;

thread_local! {
    static STATE: std::cell::RefCell<Option<(Game, u32, Vec<Vec<u8>>)>> = const { std::cell::RefCell::new(None) };
}

pub fn run(data: &[u8]) {
    let (tables, names) = tables();
    static HOOK: std::sync::Once = std::sync::Once::new();
    HOOK.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let pid = std::process::id();
            // Where it panicked, for a catalogue of distinct sites (PCDS_FUZZ_LOG, default panics.log).
            let at = info.location().map(|l| format!("{}:{}", l.file(), l.line())).unwrap_or_default();
            let what = info.payload().downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| info.payload().downcast_ref::<String>().cloned()).unwrap_or_default();
            let log = std::env::var("PCDS_FUZZ_LOG").unwrap_or_else(|_| "panics.log".into());
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&log) {
                use std::io::Write;
                let _ = writeln!(f, "{at} | {what} | history-{pid}.bin");
            }
            STATE.with(|s| {
                if let Ok(s) = s.try_borrow() {
                    if let Some((_, _, history)) = s.as_ref() {
                        // Length-prefixed inputs, oldest first (PCDS_FUZZ_REPLAY=file runs them again).
                        let bytes: Vec<u8> = history.iter().flat_map(|h| (h.len() as u32).to_le_bytes().into_iter().chain(h.iter().copied())).collect();
                        let dir = std::path::Path::new(&log).parent().map(|d| d.to_path_buf()).unwrap_or_default();
                        let _ = std::fs::write(dir.join(format!("history-{pid}.bin")), bytes);
                    }
                }
            });
            previous(info);
        }));
    });
    // A history from a crash: run its inputs in order, on the same game.
    if let Ok(path) = std::env::var("PCDS_FUZZ_REPLAY") {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            let bytes = std::fs::read(path).expect("PCDS_FUZZ_REPLAY");
            let mut at = 0;
            while at + 4 <= bytes.len() {
                let n = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
                at += 4;
                run_one(tables, names, &bytes[at..at + n], u32::MAX);
                at += n;
            }
        });
        return;
    }
    let reset = std::env::var("PCDS_FUZZ_RESET").ok().and_then(|v| v.parse().ok()).unwrap_or(RESET);
    run_one(tables, names, data, reset);
}

fn run_one(tables: &Tables, names: &[String], data: &[u8], reset: u32) {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        if s.as_ref().is_none_or(|(_, n, _)| *n >= reset) {
            *s = Some((world(tables, save()), 0, Vec::new()));
        }
        let (g, n, history) = s.as_mut().unwrap();
        *n += 1;
        history.push(data.to_vec());
        step(g, names, data);
        // Whatever the clients did must leave a world that hot-updates: its snapshot is written,
        // read back, and the restored game is the same one (a NaN becomes null in JSON and fails here).
        if *n % 20 == 0 {
            let mut out = Vec::new();
            if let Err(e) = crate::handoff::snapshot_to(g, &mut out) {
                panic!("a client made a world that cannot be snapshotted: {e}");
            }
            let (nd, d, sp, sc) = tables.clone();
            match crate::handoff::restore_bytes(nd, d, sp, &out, sc) {
                Ok(back) => {
                    if let Err(e) = crate::handoff::verify(&back, &out) {
                        panic!("a restored world differs from its snapshot: {e}");
                    }
                }
                Err(e) => panic!("a client made a world whose snapshot cannot be restored: {e}"),
            }
        }
    });
}

fn finite_world(g: &Game) -> Option<String> {
    let bad = |v: &[f32]| v.iter().any(|x| !x.is_finite());
    g.net.objects.values().find(|o| bad(&o.position) || bad(&o.rotation)).map(|o| format!("network object {} ({:?}) at {:?} {:?}", o.id, o.behaviours.iter().map(|b| b.class.as_str()).collect::<Vec<_>>(), o.position, o.rotation))
        .or_else(|| g.wos.values().find(|w| bad(&w.position) || bad(&w.rotation)).map(|w| format!("world object {} at {:?} {:?}", w.id, w.position, w.rotation)))
}

fn step(g: &mut Game, names: &[String], data: &[u8]) {
    let trace = std::env::var_os("PCDS_FUZZ_TRACE").is_some();
    let mut c = Cur(data);
    let mut ops = 0;
    while !c.done() && ops < 48 {
        ops += 1;
        let client = 1 + c.u8() as u64 % 2;
        match c.u8() % 8 {
            // A well-formed RPC from the table, with arguments from the pools above.
            0..=2 => {
                let name = names[c.pick(names.len())].clone();
                let n = c.u8() % 9;
                let args: Vec<Value> = (0..n).map(|_| value(&mut c, g)).collect();
                let class = name.split('.').next().unwrap_or("");
                let carriers: Vec<u64> = g.net.objects.keys().copied().filter(|o| g.net.carries(*o, class)).collect();
                let all: Vec<u64> = g.net.objects.keys().copied().collect();
                let pool = if carriers.is_empty() || c.u8() % 5 == 0 { &all } else { &carriers };
                let object = pool[c.pick(pool.len())];
                let behaviour = (c.u8() % 6) as u16;
                let (hash, mut body) = g.net.spec.encode(&name, &args);
                if c.u8() % 8 == 0 {
                    let cut = c.pick(body.len() + 1);
                    body.truncate(cut);
                }
                let mut w = Writer::new();
                w.packed_u64(client).packed_u64(object).packed_u16(behaviour).packed_u32(hash).bytes(&body);
                if trace {
                    eprintln!("OP client {client} {name} on object {object} behaviour {behaviour} args {}", Value::Array(args.clone()));
                }
                g.incoming_rpc(client, &w.0);
            }
            // An RPC with a table hash and raw argument bytes.
            3 => {
                let name = &names[c.pick(names.len())];
                let hash = g.net.spec.hash(name).unwrap_or(0);
                let all: Vec<u64> = g.net.objects.keys().copied().collect();
                let object = all[c.pick(all.len())];
                let mut w = Writer::new();
                w.packed_u64(client).packed_u64(object).packed_u16((c.u8() % 6) as u16).packed_u32(hash).bytes(&c.bytes(300));
                if trace {
                    eprintln!("OP client {client} raw rpc {name} on object {object}: {:02x?}", &w.0);
                }
                g.incoming_rpc(client, &w.0);
            }
            // A batch of raw messages.
            4 => {
                let kinds = [CONNECTION_REQUEST, NETWORK_TRANSFORM, NETWORK_VARIABLE_DELTA, protocol::RPC, 0, 99];
                let kind = kinds[c.pick(kinds.len())];
                let mut batch = wire::batch(&[(kind, c.bytes(400))]);
                if c.u8() % 4 == 0 {
                    let cut = c.pick(batch.len() + 1);
                    batch.truncate(cut);
                }
                if trace {
                    eprintln!("OP client {client} raw batch kind {kind}: {:02x?}", &batch);
                }
                g.receive(client, &batch);
            }
            // A transform of one of the objects.
            5 => {
                let all: Vec<u64> = g.net.objects.keys().copied().collect();
                let object = all[c.pick(all.len())];
                let mut w = Writer::new();
                let flags = c.u32() & 0x1000_7FFF;
                w.packed_u64(object).packed_i32(0).packed_u32(flags).packed_i32(7);
                for _ in 0..4 {
                    let f = if c.u8() % 3 == 0 { f32::from_bits(c.u32()) } else { (c.u32() as i32 as f32) / 1000.0 };
                    w.f32(f);
                }
                if trace {
                    eprintln!("OP client {client} transform of object {object}: {:02x?}", &w.0);
                }
                g.receive(client, &wire::batch(&[(NETWORK_TRANSFORM, w.0)]));
            }
            // A variable delta on an object.
            6 => {
                let all: Vec<u64> = g.net.objects.keys().copied().collect();
                let object = all[c.pick(all.len())];
                let mut w = Writer::new();
                w.packed_u64(object).packed_u16((c.u8() % 6) as u16).i32(0).bytes(&c.bytes(120));
                if trace {
                    eprintln!("OP client {client} variable delta on object {object}: {:02x?}", &w.0);
                }
                g.receive(client, &wire::batch(&[(NETWORK_VARIABLE_DELTA, w.0)]));
            }
            // Time passes; one client leaves and comes back.
            _ => {
                g.time += c.u8() as f32 / 16.0;
                g.tick();
                if c.u8() % 4 == 0 {
                    g.disconnect(2);
                    g.connect(2, None, false, Some("127.0.0.1".into()));
                }
            }
        }
        g.net.out.clear();
        if trace {
            if let Some(what) = finite_world(g) {
                panic!("after op {ops}: {what}");
            }
        }
    }
}

#[path = "savesweep.rs"]
pub mod savesweep;
