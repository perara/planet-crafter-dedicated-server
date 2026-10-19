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
