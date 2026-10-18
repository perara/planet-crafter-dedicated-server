//! Maps keyed by ids (world objects, inventories, network objects, machines) with FxHash, the
//! hash rustc uses: an id hashes in one multiply instead of SipHash's rounds. Iteration order
//! does not change: IndexMap keeps insertion order whatever the hasher.

use std::hash::{BuildHasherDefault, Hasher};

#[derive(Default, Clone, Copy)]
pub struct Fx(u64);

impl Fx {
    fn add(&mut self, word: u64) {
        self.0 = (self.0.rotate_left(5) ^ word).wrapping_mul(0x517c_c1b7_2722_0a95);
    }
}

impl Hasher for Fx {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        bytes.iter().for_each(|b| self.add(*b as u64));
    }
    fn write_u32(&mut self, i: u32) {
        self.add(i as u64);
    }
    fn write_u64(&mut self, i: u64) {
        self.add(i);
    }
    fn write_usize(&mut self, i: usize) {
        self.add(i as u64);
    }
}

pub type IdMap<K, V> = indexmap::IndexMap<K, V, BuildHasherDefault<Fx>>;
pub type IdSet<K> = indexmap::IndexSet<K, BuildHasherDefault<Fx>>;
