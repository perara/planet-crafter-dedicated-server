//! The game's whole runtime state as one versioned JSON document (the snapshot a flip carries
//! from one bank to the next), and a game rebuilt from it that goes on exactly as the old one
//! would have (docs/hot-update.md). The static tables (data/<version>/) are not in it: the new
//! process loads its own.

/// An Instant as the seconds since it, and back as that long before now: the clock goes on.
pub mod instant {
    use std::time::{Duration, Instant};

    pub fn serialize<S: serde::Serializer>(t: &Instant, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_f64(t.elapsed().as_secs_f64())
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Instant, D::Error> {
        let since: f64 = serde::Deserialize::deserialize(d)?;
        // A corrupt snapshot (1e20 s) is an error to refuse, not a panic of Duration::from_secs_f64.
        let ago = Duration::try_from_secs_f64(since.max(0.0)).map_err(serde::de::Error::custom)?;
        Ok(Instant::now().checked_sub(ago).unwrap_or_else(Instant::now))
    }
}

/// A map with non-string keys (JSON object keys are strings), as a list of pairs.
pub mod pairs {
    use std::collections::HashMap;
    use std::hash::Hash;

    pub fn serialize<K: serde::Serialize, V: serde::Serialize, S: serde::Serializer>(m: &HashMap<K, V>, s: S) -> Result<S::Ok, S::Error> {
        s.collect_seq(m.iter())
    }
    pub fn deserialize<'de, K: serde::Deserialize<'de> + Eq + Hash, V: serde::Deserialize<'de>, D: serde::Deserializer<'de>>(d: D) -> Result<HashMap<K, V>, D::Error> {
        let v: Vec<(K, V)> = serde::Deserialize::deserialize(d)?;
        Ok(v.into_iter().collect())
    }
}

use crate::data::NetData;
use crate::game::Game;
use crate::gamedata::GameData;
use crate::rpcspec::Spec;

/// The snapshot's format: one, the current. A release that changes the state's shape or its
/// encoding bumps it; a snapshot of another format is not read (the front refuses a hot flip
/// across formats, and `pcds upgrade --restart` installs the release, the world then loading from
/// the save). The format is the binary codec of bincodec.rs, written and read in parts on several
/// threads.
pub const FORMAT: u32 = 7;

/// The snapshot's first line (msg::SnapshotHead): its format, and the tables it is for (state
/// holds indices into them, so it is only valid on the same game version).
use crate::msg::SnapshotHead as Head;

// ---- the binary codec, in parts -------------------------------------------------------
//
// head line, then: u32 n, n * (u8 kind, u32 entries, u64 length), then the n parts back to back. Part kinds:
// REST (the game with its big collections, the machines and the water left empty, but for the free
// lists), the machines and the water, then chunks of
// CHUNK entries, in order, of the world objects' slots, the inventories' slots and the network
// objects. The parts are encoded and decoded on several threads: they are independent.

const REST: u8 = 0;
const WOS: u8 = 1;
const INVENTORIES: u8 = 2;
const NET_OBJECTS: u8 = 3;
const MACHINES: u8 = 4;
const WATER: u8 = 5;
/// The kinds of part.
pub const KINDS: usize = 6;
/// What flipbench calls the CPU time of each kind of part (names with a dot are inside a step).
pub const WRITE_CPU: [&str; KINDS] = [".write cpu rest", ".write cpu world objects", ".write cpu inventories", ".write cpu network objects", ".write cpu machines", ".write cpu water"];
pub const READ_CPU: [&str; KINDS] = [".read cpu rest", ".read cpu world objects", ".read cpu inventories", ".read cpu network objects", ".read cpu machines", ".read cpu water"];
#[cfg(test)]
pub const NAMES: [&str; KINDS] = ["rest", "world objects", "inventories", "network objects", "machines", "water"];

/// Entries per chunk: about 0.4 ms of work to write and 1 ms to read.
const CHUNK: usize = 2048;

type WoSlots = Vec<Option<(i32, crate::model::WorldObject)>>;
type InventorySlots = Vec<Option<(i32, crate::model::Inventory)>>;

/// The threads a flip may use (PCDS_DEV=flip-threads=N, else the cores this process may run on, at
/// most 8).
fn threads() -> usize {
    crate::config::dev("flip-threads").and_then(|v| v.parse().ok()).unwrap_or_else(|| std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1).min(8)).max(1)
}

/// `work` on every item, in as many threads as there are items and cores, results in the items'
/// order. The caller's thread works too.
fn in_parallel<I: Sync, O: Send>(items: &[I], work: impl Fn(&I) -> O + Sync) -> Vec<O> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;
    let workers = threads().min(items.len());
    if workers <= 1 {
        return items.iter().map(work).collect();
    }
    let next = AtomicUsize::new(0);
    let results: Vec<Mutex<Option<O>>> = items.iter().map(|_| Mutex::new(None)).collect();
    let run = || {
        loop {
            let i = next.fetch_add(1, Ordering::Relaxed);
            let Some(item) = items.get(i) else { break };
            *results[i].lock().unwrap_or_else(|e| e.into_inner()) = Some(work(item));
        }
    };
    std::thread::scope(|s| {
        for _ in 1..workers {
            s.spawn(run);
        }
        run();
    });
    results.into_iter().map(|r| r.into_inner().unwrap_or_else(|e| e.into_inner()).expect("every item is worked")).collect()
}

/// The CPU time of the calling thread, in ms (the host is shared: wall time is not the code's).
fn thread_cpu_ms() -> f64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: a valid timespec for the call's duration.
    unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
    ts.tv_sec as f64 * 1e3 + ts.tv_nsec as f64 / 1e6
}

enum Task {
    Rest,
    Machines,
    Water,
    Wos(std::ops::Range<usize>),
    Inventories(std::ops::Range<usize>),
    NetObjects(std::ops::Range<usize>),
}

fn chunks(len: usize) -> impl Iterator<Item = std::ops::Range<usize>> {
    (0..len.div_ceil(CHUNK)).map(move |i| i * CHUNK..((i + 1) * CHUNK).min(len))
}

/// The parts of the state, as `kind, entries, bytes`, the first being REST. Leaves the game as
/// it was.
fn encode(g: &mut Game, cpu: &mut [f64; KINDS]) -> Result<Vec<(u8, usize, Vec<u8>)>, String> {
    /// Puts the lent collections back, however the encoding ends.
    struct Lent<'a> {
        g: &'a mut Game,
        wos: Option<WoSlots>,
        inventories: Option<InventorySlots>,
        objects: Option<crate::idmap::IdMap<u64, crate::net::NetObj>>,
        machines: crate::machines::Machines,
        water: crate::water::Water,
    }
    impl Drop for Lent<'_> {
        fn drop(&mut self) {
            self.g.wos.unlend(self.wos.take().unwrap_or_default());
            self.g.inventories.unlend(self.inventories.take().unwrap_or_default());
            self.g.net.objects = self.objects.take().unwrap_or_default();
            self.g.ext.machines = std::mem::take(&mut self.machines);
            self.g.ext.water = std::mem::take(&mut self.water);
        }
    }
    let (wos, inventories, objects) = (g.wos.lend(), g.inventories.lend(), std::mem::take(&mut g.net.objects));
    let (machines, water) = (std::mem::take(&mut g.ext.machines), std::mem::take(&mut g.ext.water));
    let lent = Lent { g, wos: Some(wos), inventories: Some(inventories), objects: Some(objects), machines, water };
    let (wos, inventories, objects) = (lent.wos.as_deref().unwrap_or_default(), lent.inventories.as_deref().unwrap_or_default(), lent.objects.as_ref().unwrap());
    let (game, machines, water): (&Game, &crate::machines::Machines, &crate::water::Water) = (lent.g, &lent.machines, &lent.water);
    let mut tasks = vec![Task::Machines, Task::Rest, Task::Water];
    tasks.extend(chunks(wos.len()).map(Task::Wos));
    tasks.extend(chunks(inventories.len()).map(Task::Inventories));
    tasks.extend(chunks(objects.len()).map(Task::NetObjects));
    let encoded = in_parallel(&tasks, |t| -> Result<(u8, usize, Vec<u8>, f64), String> {
        let started = thread_cpu_ms();
        let (kind, entries, mut out) = match t {
            Task::Rest => (REST, 1, Vec::with_capacity(1 << 20)),
            Task::Machines => (MACHINES, 1, Vec::with_capacity(1 << 20)),
            Task::Water => (WATER, 1, Vec::with_capacity(1 << 18)),
            Task::Wos(r) => (WOS, r.len(), Vec::with_capacity(CHUNK * 224)),
            Task::Inventories(r) => (INVENTORIES, r.len(), Vec::with_capacity(CHUNK * 64)),
            Task::NetObjects(r) => (NET_OBJECTS, r.len(), Vec::with_capacity(CHUNK * 256)),
        };
        match t {
            Task::Rest => crate::bincodec::append(game, &mut out),
            Task::Machines => crate::bincodec::append(machines, &mut out),
            Task::Water => crate::bincodec::append(water, &mut out),
            Task::Wos(r) => crate::bincodec::append(&wos[r.clone()], &mut out),
            Task::Inventories(r) => crate::bincodec::append(&inventories[r.clone()], &mut out),
            Task::NetObjects(r) => crate::bincodec::append(&objects.as_slice().get_range(r.clone()).ok_or("network objects range")?.iter().collect::<Vec<_>>(), &mut out),
        }
        .map_err(|e| e.to_string())?;
        Ok((kind, entries, out, thread_cpu_ms() - started))
    });
    drop(lent);
    let mut parts = Vec::with_capacity(encoded.len());
    for e in encoded {
        let (kind, entries, bytes, ms) = e?;
        cpu[kind as usize] += ms;
        parts.push((kind, entries, bytes));
    }
    Ok(parts)
}

/// The game's state at a tick boundary (the bank sent everything queued: net.out is empty),
/// written to `w`. An error leaves the game as it was: it goes on, and the flip is off.
pub fn snapshot_to(g: &mut Game, w: &mut impl std::io::Write) -> Result<(), String> {
    snapshot_timed(g, w, &mut Vec::new())
}

/// snapshot_to, with the milliseconds of its steps appended to `phases` (flipbench); the CPU
/// time the encoding took per kind of part is there too, named with a dot in front.
pub fn snapshot_timed(g: &mut Game, w: &mut impl std::io::Write, phases: &mut Vec<(&'static str, f64)>) -> Result<(), String> {
    let mut laps = Laps::new(phases);
    let head = Head { format: FORMAT.into(), game_version: g.net_data.game_version.clone() };
    let mut cpu = [0.0; KINDS];
    let parts = encode(g, &mut cpu)?;
    laps.lap("serialize");
    for (name, ms) in WRITE_CPU.into_iter().zip(cpu) {
        laps.phases.push((name, ms));
    }
    laps.clock = std::time::Instant::now();
    let io = |e: std::io::Error| e.to_string();
    serde_json::to_writer(&mut *w, &head).map_err(|e| e.to_string())?;
    w.write_all(b"\n").map_err(io)?;
    w.write_all(&(parts.len() as u32).to_le_bytes()).map_err(io)?;
    for (kind, entries, bytes) in &parts {
        w.write_all(&[*kind]).map_err(io)?;
        w.write_all(&(*entries as u32).to_le_bytes()).map_err(io)?;
        w.write_all(&(bytes.len() as u64).to_le_bytes()).map_err(io)?;
    }
    for (_, _, bytes) in &parts {
        w.write_all(bytes).map_err(io)?;
    }
    w.flush().map_err(io)?;
    laps.lap("memfd write");
    Ok(())
}

/// The state in a sealed memfd, as the old bank sends it to the new one; the milliseconds of
/// its steps are appended to `phases` (flipbench).
pub fn snapshot_memfd(g: &mut Game, phases: &mut Vec<(&'static str, f64)>) -> Result<std::os::fd::OwnedFd, String> {
    crate::ipc::memfd("pcds-snapshot", |w| snapshot_timed(g, w, phases))
}

/// The state as this build writes it.
#[cfg(test)]
pub fn snapshot(g: &mut Game) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    snapshot_to(g, &mut out)?;
    Ok(out)
}

/// Bytes per kind of part of a snapshot: where
/// the size goes (flipbench).
#[cfg(test)]
pub fn part_sizes(snapshot: &[u8]) -> [usize; KINDS] {
    let mut sizes = [0; KINDS];
    let Ok((_, body)) = split(snapshot) else { return sizes };
    let n = u32::from_le_bytes(body[..4].try_into().unwrap()) as usize;
    for i in 0..n {
        let entry = &body[4 + i * 13..4 + i * 13 + 13];
        sizes[entry[0] as usize] += u64::from_le_bytes(entry[5..].try_into().unwrap()) as usize;
    }
    sizes
}

/// The game of a format 4 body, without the tables (they are not state); the milliseconds of its
/// steps appended to `phases` (named with a dot in front: they are inside "parse").
fn decode(body: &[u8], phases: &mut Vec<(&'static str, f64)>) -> Result<Game, String> {
    let mut laps = Laps::new(phases);
    use std::mem::MaybeUninit;
    let short = || "the snapshot ends inside its directory".to_string();
    let n = u32::from_le_bytes(body.get(..4).ok_or_else(short)?.try_into().unwrap()) as usize;
    let mut at = n.checked_mul(13).and_then(|d| d.checked_add(4)).ok_or_else(short)?;
    // Kind, entries and bytes of each part.
    let mut parts: Vec<(u8, usize, &[u8])> = Vec::with_capacity(n.min(body.len()));
    for i in 0..n {
        let entry = body.get(4 + i * 13..4 + i * 13 + 13).ok_or_else(short)?;
        let entries = u32::from_le_bytes(entry[1..5].try_into().unwrap()) as usize;
        let len = u64::from_le_bytes(entry[5..].try_into().unwrap()) as usize;
        parts.push((entry[0], entries, body.get(at..at.checked_add(len).ok_or_else(short)?).ok_or("a part ends beyond the snapshot")?));
        at += len;
    }
    if at != body.len() {
        return Err("bytes after the snapshot's last part".into());
    }
    // The world objects and the inventories are read straight into their final Vec, each chunk
    // into its own stretch of it (a chunk cannot ask for more than the directory says).
    let total = |kind: u8| parts.iter().filter(|p| p.0 == kind).map(|p| p.1).sum::<usize>();
    let (mut wos, mut inventories): (WoSlots, InventorySlots) = (Vec::with_capacity(total(WOS)), Vec::with_capacity(total(INVENTORIES)));
    enum Out<'a> {
        Wos(&'a mut [MaybeUninit<Option<(i32, crate::model::WorldObject)>>]),
        Inventories(&'a mut [MaybeUninit<Option<(i32, crate::model::Inventory)>>]),
        Other,
    }
    let (mut wos_free, mut inventories_free) = (wos.spare_capacity_mut(), inventories.spare_capacity_mut());
    type Job<'a> = (u8, &'a [u8], std::sync::Mutex<Option<Out<'a>>>);
    let jobs: Vec<Job> = parts
        .iter()
        .map(|&(kind, entries, bytes)| {
            let out = match kind {
                WOS => {
                    let (head, rest) = std::mem::take(&mut wos_free).split_at_mut(entries);
                    wos_free = rest;
                    Out::Wos(head)
                }
                INVENTORIES => {
                    let (head, rest) = std::mem::take(&mut inventories_free).split_at_mut(entries);
                    inventories_free = rest;
                    Out::Inventories(head)
                }
                _ => Out::Other,
            };
            (kind, bytes, std::sync::Mutex::new(Some(out)))
        })
        .collect();
    enum Done {
        Rest(Box<Game>),
        Filled,
        NetObjects(Vec<(u64, crate::net::NetObj)>),
        Machines(Box<crate::machines::Machines>),
        Water(Box<crate::water::Water>),
    }
    let e = |e: crate::bincodec::Error| e.to_string();
    let done = in_parallel(&jobs, |(kind, bytes, out)| -> Result<(Done, u8, f64), String> {
        let started = thread_cpu_ms();
        let out = out.lock().unwrap_or_else(|e| e.into_inner()).take();
        let done = match (*kind, out) {
            (REST, _) => Done::Rest(Box::new(crate::bincodec::from_slice(bytes).map_err(e)?)),
            (WOS, Some(Out::Wos(out))) => {
                crate::bincodec::fill(bytes, out).map_err(e)?;
                Done::Filled
            }
            (INVENTORIES, Some(Out::Inventories(out))) => {
                crate::bincodec::fill(bytes, out).map_err(e)?;
                Done::Filled
            }
            (NET_OBJECTS, _) => Done::NetObjects(crate::bincodec::from_slice(bytes).map_err(e)?),
            (MACHINES, _) => Done::Machines(Box::new(crate::bincodec::from_slice(bytes).map_err(e)?)),
            (WATER, _) => Done::Water(Box::new(crate::bincodec::from_slice(bytes).map_err(e)?)),
            (k, _) => return Err(format!("a part of kind {k}")),
        };
        Ok((done, *kind, thread_cpu_ms() - started))
    });
    laps.lap(".parts read");
    let (mut game, mut objects, mut cpu, mut machines, mut water) = (None, Vec::new(), [0.0; KINDS], None, None);
    for (d, kind, ms) in done.into_iter().collect::<Result<Vec<_>, _>>()? {
        cpu[kind as usize % KINDS] += ms;
        match d {
            Done::Rest(g) if game.is_none() => game = Some(g),
            Done::Rest(_) => return Err("two REST parts".into()),
            Done::Filled => {}
            Done::NetObjects(mut c) => objects.append(&mut c),
            Done::Machines(m) => machines = Some(m),
            Done::Water(w) => water = Some(w),
        }
    }
    laps.lap(".parts joined");
    for (name, ms) in READ_CPU.into_iter().zip(cpu) {
        laps.phases.push((name, ms));
    }
    laps.clock = std::time::Instant::now();
    drop(jobs);
    // SAFETY: every chunk of both Vecs was filled in full (`fill` is Ok only then), and the
    // chunks' lengths add up to the capacity asked for.
    unsafe {
        wos.set_len(wos.capacity());
        inventories.set_len(inventories.capacity());
    }
    let mut g = game.ok_or("no REST part")?;
    g.ext.machines = *machines.ok_or("no MACHINES part")?;
    g.ext.water = *water.ok_or("no WATER part")?;
    g.wos.fill(wos)?;
    g.inventories.fill(inventories)?;
    laps.lap(".slot indexes");
    g.net.objects = objects.into_iter().collect();
    laps.lap(".net index");
    Ok(*g)
}

/// A game from a snapshot and this process's own tables.
#[cfg(test)]
pub fn restore(net_data: NetData, data: GameData, spec: Spec, snapshot: &[u8], scenes: crate::Scenes) -> Result<Game, String> {
    restore_bytes(net_data, data, spec, snapshot, scenes)
}

fn split(snapshot: &[u8]) -> Result<(Head, &[u8]), String> {
    let at = snapshot.iter().position(|b| *b == b'\n').ok_or("not a snapshot")?;
    let head: Head = serde_json::from_slice(&snapshot[..at]).map_err(|e| e.to_string())?;
    Ok((head, &snapshot[at + 1..]))
}

pub fn restore_bytes(net_data: NetData, data: GameData, spec: Spec, snapshot: &[u8], scenes: crate::Scenes) -> Result<Game, String> {
    restore_timed(net_data, data, spec, snapshot, scenes, &mut Vec::new())
}

/// The milliseconds between laps, appended to a list (flipbench).
struct Laps<'a> {
    phases: &'a mut Vec<(&'static str, f64)>,
    clock: std::time::Instant,
}

impl<'a> Laps<'a> {
    fn new(phases: &'a mut Vec<(&'static str, f64)>) -> Laps<'a> {
        Laps { phases, clock: std::time::Instant::now() }
    }
    fn lap(&mut self, name: &'static str) {
        self.phases.push((name, self.clock.elapsed().as_secs_f64() * 1000.0));
        self.clock = std::time::Instant::now();
    }
}

/// restore_bytes, with the milliseconds each step took appended to `phases` (flipbench).
pub fn restore_timed(net_data: NetData, data: GameData, spec: Spec, snapshot: &[u8], scenes: crate::Scenes, phases: &mut Vec<(&'static str, f64)>) -> Result<Game, String> {
    let mut laps = Laps::new(phases);
    let (head, state) = split(snapshot)?;
    if head.game_version != net_data.game_version {
        return Err(format!("a snapshot on game {}'s tables, these are {}'s", head.game_version, net_data.game_version));
    }
    if head.format != u64::from(FORMAT) {
        return Err(format!("a snapshot of format {}: this build reads format {FORMAT} only", head.format));
    }
    laps.lap("head");
    let mut g = decode(state, laps.phases)?;
    laps.lap("parse");
    g.net_data = net_data;
    g.data = data;
    g.net.spec = spec;
    g.scenes = scenes;
    g.rebuild_holders();
    crate::meteo::explode::recount(&mut g);
    laps.lap("rebuild holders");
    crate::machines::refresh_from_tables(&mut g);
    laps.lap("refresh tables");
    // The tables are not quite static: Game::new scales the groups' generation by the
    // world's settings.
    g.change_generation_for_groups();
    laps.lap("generation");
    Ok(g)
}

/// Whether a restored game holds exactly the state of the snapshot it came from: both written
/// as text and read back, compared without the two clocks (seconds since an Instant, which go
/// on) and without the order of sets and maps (a HashSet's order differs between processes).
/// Err names the first part that differs. PCDS_DEV=flip-verify runs it at every flip (tests).
pub fn verify(g: &Game, snapshot: &[u8]) -> Result<(), String> {
    use serde_json::Value;
    fn canon(v: &mut Value) {
        match v {
            Value::Array(a) => {
                a.iter_mut().for_each(canon);
                a.sort_by_cached_key(|x| x.to_string());
            }
            Value::Object(o) => {
                o.values_mut().for_each(canon);
                o.sort_keys();
            }
            _ => {}
        }
    }
    let state = |v: &mut Value| {
        if let Some(o) = v.as_object_mut() {
            o.remove("started");
        }
        for c in v["clients"].as_object_mut().into_iter().flat_map(|c| c.values_mut()) {
            c.as_object_mut().map(|c| c.remove("since"));
        }
        canon(v);
    };
    let (head, body) = split(snapshot)?;
    if head.format != u64::from(FORMAT) {
        return Err(format!("a snapshot of format {}", head.format));
    }
    // Through the text, as the restored game below: a float is its shortest text, not its f64.
    let mut sent: Value = serde_json::from_str(&serde_json::to_string(&decode(body, &mut Vec::new())?).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    let mut restored: Value = serde_json::from_str(&serde_json::to_string(g).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    state(&mut sent);
    state(&mut restored);
    if sent == restored {
        return Ok(());
    }
    let keys = sent.as_object().map(|o| o.keys().cloned().collect::<Vec<_>>()).unwrap_or_default();
    let part = keys.iter().find(|k| sent[k.as_str()] != restored[k.as_str()]).cloned().unwrap_or_default();
    Err(format!("the restored state differs from the snapshot in {part}"))
}

#[cfg(test)]
mod tests {
    use crate::game::{test_game, tests::{join, save}};

    /// A snapshot whose clock is 1e20 seconds old is refused with an error; it panicked.
    #[test]
    fn a_corrupt_clock_is_an_error_not_a_panic() {
        #[derive(serde::Deserialize)]
        struct Clock(#[serde(with = "super::instant")] #[allow(dead_code)] std::time::Instant);
        assert!(serde_json::from_str::<Clock>("1e20").is_err());
        assert!(serde_json::from_str::<Clock>("1e300").is_err());
        assert!(serde_json::from_str::<Clock>("12.5").is_ok());
        assert!(serde_json::from_str::<Clock>("-3.0").is_ok(), "a clock before its start counts as now");
    }

    /// A snapshot with its head line's text changed.
    fn with_head(snapshot: &[u8], from: &str, to: &str) -> Vec<u8> {
        let at = snapshot.iter().position(|b| *b == b'\n').unwrap();
        let head = String::from_utf8(snapshot[..at].to_vec()).unwrap();
        assert!(head.contains(from), "{head} has no {from}");
        [head.replacen(from, to, 1).as_bytes(), &snapshot[at..]].concat()
    }

    #[test]
    fn a_snapshot_restores_only_in_its_format_and_on_its_tables() {
        let Some(mut g) = test_game(save()) else { return };
        let player = join(&mut g, 1, "Ann");
        let text = super::snapshot(&mut g).unwrap();
        let dir = crate::test_data().unwrap();
        let tables = || {
            let (n, d, s) = crate::tables(dir);
            let scenes = crate::scene_tables(dir, &n, &d);
            (n, d, s, scenes)
        };
        let (n, d, s, scenes) = tables();
        let back = super::restore(n, d, s, &text, scenes).unwrap();
        assert_eq!(back.player_object(1), Some(player));
        super::verify(&back, &text).unwrap();
        let (n, d, s, scenes) = tables();
        let mut other = super::restore(n, d, s, &text, scenes).unwrap();
        other.tmp_inventory_id += 1;
        assert!(super::verify(&other, &text).unwrap_err().contains("tmp_inventory_id"));
        for (from, to) in [(&*format!("\"format\":{}", super::FORMAT), "\"format\":99"), (&*format!("\"game_version\":\"{}\"", g.net_data.game_version), "\"game_version\":\"0.1\"")] {
            let (n, d, s, scenes) = tables();
            assert!(super::restore(n, d, s, &with_head(&text, from, to), scenes).is_err(), "{to} is refused");
        }
        // A cut or damaged body is an error, never a panic.
        for cut in [text.len() / 2, text.len() - 1, text.len() - 100] {
            let (n, d, s, scenes) = tables();
            assert!(super::restore(n, d, s, &text[..cut], scenes).is_err(), "cut at {cut}");
        }
    }

    /// The binary codec carries what JSON could not: a position that is not a number comes
    /// back as it was.
    #[test]
    fn a_position_that_is_not_a_number_crosses_a_flip() {
        let Some(mut g) = test_game(save()) else { return };
        let player = join(&mut g, 1, "Ann");
        g.net.objects.get_mut(&player).unwrap().position[1] = f32::NAN;
        let pod = *g.wos.keys().next().unwrap();
        g.wos.get_mut(&pod).unwrap().rotation[0] = f32::INFINITY;
        let (n, d, s, scenes) = crate::game::test_tables().unwrap();
        let text = super::snapshot(&mut g).unwrap();
        let h = super::restore(n, d, s, &text, scenes).unwrap();
        assert!(h.net.objects[&player].position[1].is_nan());
        assert_eq!(h.wos[&pod].rotation[0], f32::INFINITY);
    }

    /// The parts run on several threads and come back in order.
    #[test]
    fn work_in_parallel_comes_back_in_order() {
        let items: Vec<u64> = (0..1000).collect();
        let out = super::in_parallel(&items, |i| {
            std::thread::sleep(std::time::Duration::from_micros(i % 7));
            i * 2
        });
        assert_eq!(out, items.iter().map(|i| i * 2).collect::<Vec<_>>());
        assert_eq!(super::in_parallel(&[] as &[u8], |_| 1), Vec::<i32>::new());
        assert_eq!(super::in_parallel(&[5], |i| *i), vec![5]);
    }

    /// Nothing is a snapshot: errors, not panics, and no allocation of what a length asks for.
    #[test]
    fn garbage_is_not_a_snapshot() {
        for body in [&[][..], &[1, 0, 0, 0][..], &[1, 0, 0, 0, 0, 255, 255, 255, 255, 255, 255, 255, 255][..], &[0xff; 64][..], &[1, 0, 0, 0, 9, 1, 0, 0, 0, 0, 0, 0, 0, 7][..]] {
            assert!(super::decode(body, &mut Vec::new()).is_err());
        }
    }
}

#[cfg(any(fuzzing, pcds_corpus))]
#[path = "../fuzz/targets/handoff.rs"]
pub mod fuzz;
