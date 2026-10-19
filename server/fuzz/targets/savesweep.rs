//! (a part of the game target's file, see game.rs) Hostile saves: a real save with each number and each
//! string value replaced in turn by awkward ones, loaded into a Game, ticked, and saved again.
//! PCDS_FUZZ_TARGET=savesweep PCDS_FUZZ_SWEEP_FILE=save.json [PCDS_FUZZ_SWEEP_STEP/OFFSET]
use super::*;

static SITES: std::sync::Mutex<std::collections::BTreeMap<String, (String, usize)>> = std::sync::Mutex::new(std::collections::BTreeMap::new());
static CURRENT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

pub fn run(_: &[u8]) {
    let file = std::env::var("PCDS_FUZZ_SWEEP_FILE").expect("PCDS_FUZZ_SWEEP_FILE");
    let step: usize = std::env::var("PCDS_FUZZ_SWEEP_STEP").ok().and_then(|v| v.parse().ok()).unwrap_or(1);
    let offset: usize = std::env::var("PCDS_FUZZ_SWEEP_OFFSET").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
    let text = std::fs::read_to_string(file).expect("save");
    let (tables, _) = tables();
    // Spans of numbers and of string contents (outside nothing else: the whole text is JSON lines).
    let bytes = text.as_bytes();
    let mut spans: Vec<(usize, usize, bool)> = Vec::new();
    let (mut i, mut in_string, mut escaped, mut s_start) = (0, false, false, 0);
    while i < bytes.len() {
        let b = bytes[i];
        if in_string {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
                spans.push((s_start, i, true));
            }
        } else if b == b'"' {
            in_string = true;
            s_start = i + 1;
        } else if (b.is_ascii_digit() || b == b'-') && !bytes.get(i.wrapping_sub(1)).is_some_and(|p| p.is_ascii_alphanumeric()) {
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_digit() || b"+-.eE".contains(&bytes[i])) {
                i += 1;
            }
            spans.push((start, i, false));
            continue;
        }
        i += 1;
    }
    eprintln!("savesweep: {} values", spans.len());
    let numbers = ["-1", "0", "2147483648", "-2147483649", "4294967296", "1e20", "1e300", "18446744073709551615", "0.5", "null", "true", "[]", "{}"];
    let strings = ["", "NaN", "inf", "-inf", "1e39", "-1", "1,2", "1,2,3,4,5,6,7,8", "99999999999999999999", ",", "a,b,c", "0,0,0", "\\\\u0000", "9999999,9999999"];
    std::panic::set_hook(Box::new(|info| {
        let at = info.location().map(|l| format!("{}:{}", l.file(), l.line())).unwrap_or_default();
        let what = info.payload().downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| info.payload().downcast_ref::<String>().cloned()).unwrap_or_default();
        SITES.lock().unwrap().entry(at).or_insert((what, CURRENT.load(std::sync::atomic::Ordering::Relaxed)));
    }));
    let mut runs = 0;
    for (start, end, is_string) in spans.iter().skip(offset).step_by(step) {
        CURRENT.store(*start, std::sync::atomic::Ordering::Relaxed);
        for v in if *is_string { &strings[..] } else { &numbers[..] } {
            let mut mutated = text[..*start].to_string();
            mutated.push_str(v);
            mutated.push_str(&text[*end..]);
            let save = SaveFile::from_text(&mutated);
            let tables = tables.clone();
            runs += 1;
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
                let (n, d, s, scenes) = tables;
                let mut g = Game::new(n, d, s, save, scenes);
                g.time += 1.0;
                g.tick();
                let _ = g.snapshot().to_text();
            }));
        }
    }
    for (at, (what, byte)) in SITES.lock().unwrap().iter() {
        println!("PANIC {at}: {what} (value at byte {byte}: {})", text[byte.saturating_sub(70)..(*byte + 30).min(text.len())].replace('\n', " ").replace('\r', " "));
    }
    println!("savesweep done: {runs} loads, {} distinct panic sites", SITES.lock().unwrap().len());
}
