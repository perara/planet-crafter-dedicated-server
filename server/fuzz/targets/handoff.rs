//! A snapshot another bank wrote (or a corrupted one) restores cleanly or is refused, and what
//! is restored snapshots again.
use super::*;
use crate::save::SaveFile;
use std::sync::OnceLock;

type Tables = (NetData, GameData, Spec, crate::Scenes);
static TABLES: OnceLock<Tables> = OnceLock::new();

pub fn run(data: &[u8]) {
    // PCDS_FUZZ_SEED_OUT=file: write a snapshot of the test world (or of the save PCDS_FUZZ_SAVE)
    // there, once, as a seed.
    if let Ok(out) = std::env::var("PCDS_FUZZ_SEED_OUT") {
        if !std::path::Path::new(&out).exists() {
            let save = match std::env::var("PCDS_FUZZ_SAVE") {
                Ok(p) => SaveFile::load(&p).expect("PCDS_FUZZ_SAVE"),
                Err(_) => crate::game::fuzz::save(),
            };
            let mut g = crate::game::fuzz::world(&crate::game::fuzz::tables().0, save);
            let mut bytes = Vec::new();
            snapshot_to(&mut g, &mut bytes).expect("snapshot");
            std::fs::write(&out, bytes).unwrap();
        }
    }
    let (n, d, s, scenes) = TABLES
        .get_or_init(|| {
            let dir = crate::fuzz_entry::data_dir();
            let (n, d, s) = crate::tables(&dir);
            let scenes = crate::scene_tables(&dir, &n, &d);
            (n, d, s, scenes)
        })
        .clone();
    if let Ok(mut g) = restore_bytes(n, d, s, data, scenes) {
        let mut out = Vec::new();
        let _ = snapshot_to(&mut g, &mut out);
        let _ = verify(&g, data);
    }
}

static SITES: std::sync::Mutex<std::collections::BTreeMap<String, (String, usize)>> = std::sync::Mutex::new(std::collections::BTreeMap::new());
static CURRENT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// PCDS_FUZZ_TARGET=handoffsweep PCDS_FUZZ_SWEEP_FILE=snapshot: every byte offset of the snapshot's
/// body (every STEP-th, from PCDS_FUZZ_SWEEP_OFFSET) replaced in turn by boundary values of 4 and
/// 8 bytes (all ones, zero, i32::MIN, NaN and infinity bits ...); each restore must give a Game
/// or an error, and a snapshot of what was restored must not panic. Prints the distinct panic
/// sites. (libFuzzer's mutations rarely land on one number of a snapshot of tens of KB.)
pub fn sweep(_: &[u8]) {
    let file = std::env::var("PCDS_FUZZ_SWEEP_FILE").expect("PCDS_FUZZ_SWEEP_FILE");
    let step: usize = std::env::var("PCDS_FUZZ_SWEEP_STEP").ok().and_then(|v| v.parse().ok()).unwrap_or(1);
    let offset: usize = std::env::var("PCDS_FUZZ_SWEEP_OFFSET").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
    let snapshot = std::fs::read(file).expect("snapshot");
    let tables = TABLES
        .get_or_init(|| {
            let dir = crate::fuzz_entry::data_dir();
            let (n, d, s) = crate::tables(&dir);
            let scenes = crate::scene_tables(&dir, &n, &d);
            (n, d, s, scenes)
        })
        .clone();
    let _ = tables;
    // After the head line.
    let body = snapshot.iter().position(|b| *b == b'\n').map_or(0, |i| i + 1);
    let spans: Vec<(usize, usize)> = (body..snapshot.len().saturating_sub(8)).map(|i| (i, i + 4)).collect();
    eprintln!("sweep: {} offsets", spans.len());
    let values: [&[u8]; 10] = [&[0; 4], &[255; 4], &[0, 0, 0, 128], &[255, 255, 255, 127], &f32::NAN.to_le_bytes(), &f32::INFINITY.to_le_bytes(), &f32::NEG_INFINITY.to_le_bytes(), &[255; 8], &[0, 0, 0, 0, 0, 0, 0, 128], &1e20f32.to_le_bytes()];
    std::panic::set_hook(Box::new(|info| {
        let at = info.location().map(|l| format!("{}:{}", l.file(), l.line())).unwrap_or_default();
        let what = info.payload().downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| info.payload().downcast_ref::<String>().cloned()).unwrap_or_default();
        SITES.lock().unwrap().entry(at).or_insert((what, CURRENT.load(std::sync::atomic::Ordering::Relaxed)));
    }));
    for (n, (start, _end)) in spans.iter().enumerate().skip(offset).step_by(step) {
        CURRENT.store(*start, std::sync::atomic::Ordering::Relaxed);
        for v in values {
            let mut mutated = snapshot[..*start].to_vec();
            mutated.extend_from_slice(v);
            mutated.extend_from_slice(&snapshot[(*start + v.len()).min(snapshot.len())..]);
            let (nd, d, sp, sc) = TABLES.get().unwrap().clone();
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                if let Ok(mut g) = restore_bytes(nd, d, sp, &mutated, sc) {
                    let mut out = Vec::new();
                    let _ = snapshot_to(&mut g, &mut out);
                }
            }));
            let _ = (r, n);
        }
    }
    for (at, (what, byte)) in SITES.lock().unwrap().iter() {
        println!("PANIC {at}: {what} (offset {byte}: {})", String::from_utf8_lossy(&snapshot[(*byte).saturating_sub(60)..(*byte + 20).min(snapshot.len())]).replace('\n', " "));
    }
    println!("sweep done, {} distinct panic sites", SITES.lock().unwrap().len());
}
