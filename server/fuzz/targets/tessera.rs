//! The wreck solver on seeds: whatever the seed, a generator's tiles are laid out or the attempt
//! fails, in bounded time.
use super::*;
use std::time::{Duration, Instant};

pub fn run(data: &[u8]) {
    let (tables, _) = crate::game::fuzz::tables();
    let w = &tables.3.wrecks;
    let Some((&which, rest)) = data.split_first() else { return };
    let index = which as usize % w.generators.len();
    let seed = i32::from_le_bytes([rest.first().copied().unwrap_or(0), rest.get(1).copied().unwrap_or(0), rest.get(2).copied().unwrap_or(0), rest.get(3).copied().unwrap_or(0)]);
    let Ok(model) = w.model(index) else { return };
    let started = Instant::now();
    let result = crate::tessera::generate(&model, &w.generators[index].fixed, seed);
    assert!(started.elapsed() < Duration::from_secs(150), "generator {index} seed {seed}: {:?}", started.elapsed());
    if let Ok(Some(tiles)) = result {
        for t in &tiles {
            assert!(t.position.iter().chain(&t.rotation).all(|v| v.is_finite()), "generator {index} seed {seed}: a tile off the map");
        }
    }
}
