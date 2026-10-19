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
            let g = crate::game::fuzz::world(&crate::game::fuzz::tables().0, save);
            let mut bytes = Vec::new();
            snapshot_to(&g, &mut bytes).expect("snapshot");
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
    if let Ok(g) = restore_bytes(n, d, s, data, scenes) {
        let mut out = Vec::new();
        let _ = snapshot_to(&g, &mut out);
        let _ = verify(&g, data);
    }
}

static SITES: std::sync::Mutex<std::collections::BTreeMap<String, (String, usize)>> = std::sync::Mutex::new(std::collections::BTreeMap::new());
static CURRENT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// PCDS_FUZZ_TARGET=handoffsweep PCDS_FUZZ_SWEEP_FILE=snapshot: every number of the snapshot (every
/// STEP-th, from PCDS_FUZZ_SWEEP_OFFSET) replaced in turn by boundary values; each restore must
/// give a Game or an error, and a snapshot of what was restored must not panic. Prints the
/// distinct panic sites. (libFuzzer's mutations rarely land on one number of 36 KB.)
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
    // Where each number starts and ends (outside strings), after the head line.
    let body = snapshot.iter().position(|b| *b == b'\n').map_or(0, |i| i + 1);
    let mut spans = Vec::new();
    let (mut in_string, mut escaped, mut i) = (false, false, body);
    while i < snapshot.len() {
        let b = snapshot[i];
        if in_string {
            if escaped { escaped = false } else if b == b'\\' { escaped = true } else if b == b'"' { in_string = false }
        } else if b == b'"' {
            in_string = true;
        } else if b.is_ascii_digit() || (b == b'-' && snapshot.get(i + 1).is_some_and(u8::is_ascii_digit)) {
            let start = i;
            while i < snapshot.len() && (snapshot[i].is_ascii_digit() || b"+-.eE".contains(&snapshot[i])) {
                i += 1;
            }
            spans.push((start, i));
            continue;
        }
        i += 1;
    }
    eprintln!("sweep: {} numbers", spans.len());
    let values = ["-1", "0", "1", "-2147483649", "2147483648", "4294967296", "1e20", "-1e20", "1e300", "18446744073709551615", "9223372036854775808", "0.5"];
    std::panic::set_hook(Box::new(|info| {
        let at = info.location().map(|l| format!("{}:{}", l.file(), l.line())).unwrap_or_default();
        let what = info.payload().downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| info.payload().downcast_ref::<String>().cloned()).unwrap_or_default();
        SITES.lock().unwrap().entry(at).or_insert((what, CURRENT.load(std::sync::atomic::Ordering::Relaxed)));
    }));
    for (n, (start, end)) in spans.iter().enumerate().skip(offset).step_by(step) {
        CURRENT.store(*start, std::sync::atomic::Ordering::Relaxed);
        for v in values {
            let mut mutated = snapshot[..*start].to_vec();
            mutated.extend_from_slice(v.as_bytes());
            mutated.extend_from_slice(&snapshot[*end..]);
            let (nd, d, sp, sc) = TABLES.get().unwrap().clone();
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                if let Ok(g) = restore_bytes(nd, d, sp, &mutated, sc) {
                    let mut out = Vec::new();
                    let _ = snapshot_to(&g, &mut out);
                }
            }));
            let _ = (r, n);
        }
    }
    for (at, (what, byte)) in SITES.lock().unwrap().iter() {
        println!("PANIC {at}: {what} (number at byte {byte}: {})", String::from_utf8_lossy(&snapshot[(*byte).saturating_sub(60)..(*byte + 20).min(snapshot.len())]).replace('\n', " "));
    }
    println!("sweep done, {} distinct panic sites", SITES.lock().unwrap().len());
}
