//! The fuzz entry point (cfg(fuzzing), or cfg(pcds_corpus) for a replay of corpora): one target,
//! chosen by PCDS_FUZZ_TARGET.

#[cfg(fuzzing)]
use std::sync::OnceLock;

#[cfg(fuzzing)]
static TARGET: OnceLock<String> = OnceLock::new();

/// The tables directory the data-driven targets read.
pub fn data_dir() -> String {
    std::env::var("PCDS_FUZZ_DATA").unwrap_or_else(|_| "/home/per/pcds-handoff/scratch/full".into())
}

#[cfg(fuzzing)]
libfuzzer_sys::fuzz_target!(|data: &[u8]| run(TARGET.get_or_init(|| std::env::var("PCDS_FUZZ_TARGET").unwrap_or_default()), data));

/// One input to the named target.
pub fn run(target: &str, data: &[u8]) {
    match target {
        "wire" => crate::wire::fuzz::run(data),
        "utp" => crate::utp::fuzz::run(data),
        "rpcspec" => crate::rpcspec::fuzz::run(data),
        "transform" => crate::transform::fuzz::run(data),
        "ipc" => crate::ipc::fuzz::run(data),
        "config" => crate::config::fuzz::run(data),
        "chathooks" => crate::chathooks::fuzz::run(data),
        "updater" => crate::updater::fuzz::run(data),
        "access" => crate::access::fuzz::run(data),
        "rules" => crate::rules::fuzz::run(data),
        "save" => crate::save::fuzz::run(data),
        "handoff" => crate::handoff::fuzz::run(data),
        "game" => crate::game::fuzz::run(data),
        "handoffsweep" => crate::handoff::fuzz::sweep(data),
        "savesweep" => crate::game::fuzz::savesweep::run(data),
        "front" => crate::front::fuzz::run(data),
        other => panic!("PCDS_FUZZ_TARGET={other:?}: no such target"),
    }
}

/// RUSTFLAGS="--cfg pcds_corpus" PCDS_FUZZ_TARGET=wire PCDS_CORPUS=fuzz/corpus/wire cargo test replay_corpus
/// (also under Miri): every file of a corpus directory through the target, as a regression run
/// without libFuzzer. Crashes found by fuzzing are unit tests of their own; this replays seeds.
#[cfg(all(test, pcds_corpus))]
#[test]
fn replay_corpus() {
    let (Ok(target), Ok(dir)) = (std::env::var("PCDS_FUZZ_TARGET"), std::env::var("PCDS_CORPUS")) else { return };
    let mut files: Vec<_> = std::fs::read_dir(&dir).expect("PCDS_CORPUS").flatten().filter(|e| e.path().is_file()).map(|e| e.path()).collect();
    files.sort();
    let limit = std::env::var("PCDS_CORPUS_LIMIT").ok().and_then(|v| v.parse().ok()).unwrap_or(usize::MAX);
    for file in files.iter().take(limit) {
        run(&target, &std::fs::read(file).unwrap());
    }
}
