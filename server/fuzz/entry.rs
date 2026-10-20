//! The fuzz entry point (cfg(fuzzing), or cfg(pcds_corpus) for a replay of corpora): one target,
//! chosen by PCDS_FUZZ_TARGET.

#[cfg(fuzzing)]
use std::sync::OnceLock;

#[cfg(fuzzing)]
static TARGET: OnceLock<String> = OnceLock::new();

/// The tables directory the data-driven targets read: PCDS_FUZZ_DATA, else the installation's in
/// this checkout (run/data/2.103).
pub fn data_dir() -> String {
    std::env::var("PCDS_FUZZ_DATA").unwrap_or_else(|_| concat!(env!("CARGO_MANIFEST_DIR"), "/../../run/data/2.103").into())
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
        "oldsave" => crate::oldsave::fuzz::run(data),
        "savelayout" => crate::save::fuzz::layout(data),
        "manifest" => crate::plugins::manifest::fuzz::run(data),
        "pluginnet" => crate::plugins::net::fuzz::run(data),
        "pluginsign" => crate::plugins::sign::fuzz::run(data),
        "wasmhost" => crate::plugins::fuzz_wasm::run(data),
        "bincodec" => crate::bincodec::fuzz::run(data),
        "adminhttp" => crate::admin::fuzz::run(data),
        "adminroutes" => crate::bank::fuzz_admin::run(data),
        "names" => crate::worlds::fuzz::run(data),
        "msg" => crate::msg::fuzz::run(data),
        "toxic" => crate::toxic::fuzz::run(data),
        "prom" => crate::front::fuzz_prom::run(data),
        "logline" => crate::logging::fuzz::run(data),
        "s3resp" => crate::s3::fuzz::run(data),
        "backup" => crate::cli::fuzz_backup::run(data),
        "doctor" => crate::cli::fuzz_doctor::run(data),
        "loadsave" => crate::game::fuzz::loadsave(data),
        "twin" => crate::game::fuzz::twin(data),
        "tessera" => crate::instances::generate::fuzz::run(data),
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
