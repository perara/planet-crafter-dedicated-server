//! Tick benchmark on a real save, with a fixed clock and seed, and a fingerprint of everything
//! the server sends and saves (an optimization must leave the fingerprint unchanged):
//!
//!   PCDS_BENCH_SAVE=world.json PCDS_RNG_SEED=1 [PCDS_BENCH_SECONDS=300] [PCDS_BENCH_PLAYERS=4] \
//!     [PCDS_BENCH_DUMP=messages.txt] cargo test --release bench -- --ignored --nocapture
//!
//! PCDS_BENCH_DUMP writes every message (per tick, sorted) to compare two runs line by line.
//!
//! The loop steps like the server's (a tick every 5 ms); players walk in circles, sending a
//! transform every network tick. "cpu at real time" is the share of one core the ticks would
//! use running live.

use std::hash::{Hash, Hasher};
use std::time::{Duration, Instant};

use crate::game::{tests::join, Game};
use crate::protocol::NETWORK_TRANSFORM;
use crate::save::SaveFile;
use crate::wire::Writer;

fn fingerprint(h: &mut impl Hasher, out: &mut Vec<(crate::net::Target, u32, Vec<u8>)>, dump: &mut Option<std::io::BufWriter<std::fs::File>>) -> usize {
    // Within one tick the order of independent messages may follow hash-map order: sorted.
    let mut tick: Vec<String> = out.drain(..).map(|(to, kind, body)| format!("{to:?} {kind} {body:02x?}")).collect();
    tick.sort();
    tick.hash(h);
    if let Some(f) = dump {
        use std::io::Write;
        for line in &tick {
            writeln!(f, "{line}").unwrap();
        }
    }
    tick.len()
}

#[test]
#[ignore]
fn bench() {
    let Some(path) = std::env::var("PCDS_BENCH_SAVE").ok() else { return eprintln!("bench: set PCDS_BENCH_SAVE") };
    let Some(dir) = crate::test_data() else { return };
    let seconds: f64 = std::env::var("PCDS_BENCH_SECONDS").ok().and_then(|v| v.parse().ok()).unwrap_or(300.0);
    let players: u64 = std::env::var("PCDS_BENCH_PLAYERS").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
    let (net_data, data, spec) = crate::tables(dir);
    let loaded = Instant::now();
    let mut g = Game::new(net_data, data, spec, SaveFile::load(&path).unwrap(), dir);
    println!("bench: load {:?}, {} world objects, {} network objects", loaded.elapsed(), g.wos.len(), g.net.objects.len());
    g.sim_time = Some(0.0);
    let mut h = std::collections::hash_map::DefaultHasher::new();
    let mut dump = std::env::var("PCDS_BENCH_DUMP").ok().map(|p| std::io::BufWriter::new(std::fs::File::create(p).unwrap()));
    let objects: Vec<u64> = (1..=players).map(|c| join(&mut g, c, &format!("Bench{c}"))).collect();
    let mut messages = fingerprint(&mut h, &mut g.net.out, &mut dump);
    let (step, net_tick) = (0.005, 1.0 / g.net_data.tick_rate as f64);
    let (mut t, mut next_move, mut ticks, mut busy) = (0.0, 0.0, 0u64, Duration::ZERO);
    while t < seconds {
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
        g.tick();
        busy += started.elapsed();
        ticks += 1;
        messages += fingerprint(&mut h, &mut g.net.out, &mut dump);
        g.kicks.clear();
    }
    let save = g.snapshot().to_text();
    save.hash(&mut h);
    let per_tick = busy / ticks as u32;
    println!(
        "bench: {seconds} s simulated, {players} players, {ticks} ticks: {per_tick:?} per tick, cpu at real time {:.2}% of a core, {messages} messages, save {} bytes, fingerprint {:016x}",
        100.0 * busy.as_secs_f64() / seconds,
        save.len(),
        h.finish()
    );
}
