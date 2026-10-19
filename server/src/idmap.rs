//! Maps keyed by ids (world objects, inventories, network objects, machines) with FxHash, the
//! hash rustc uses: an id hashes in one multiply instead of SipHash's rounds. Iteration order
//! does not change: IndexMap keeps insertion order whatever the hasher.

use std::hash::{BuildHasherDefault, Hash, Hasher};

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
pub type FxHashMap<K, V> = std::collections::HashMap<K, V, BuildHasherDefault<Fx>>;
pub type FxHashSet<K> = std::collections::HashSet<K, BuildHasherDefault<Fx>>;

/// A map in the enumeration order of .NET's Dictionary and HashSet: entries in slot order, and
/// a removed entry's slot taken by the next insertion, the most recently freed first. Insert,
/// lookup and removal are O(1) (an IndexMap's shift_remove moves everything after the entry).
#[derive(Clone)]
pub struct Slots<K, V> {
    slots: Vec<Option<(K, V)>>,
    free: Vec<usize>,
    index: FxHashMap<K, usize>,
}

impl<K, V> Default for Slots<K, V> {
    fn default() -> Self {
        Slots { slots: Vec::new(), free: Vec::new(), index: FxHashMap::default() }
    }
}

impl<K: Copy + Eq + Hash, V> Slots<K, V> {
    pub fn len(&self) -> usize {
        self.index.len()
    }
    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }
    pub fn get(&self, k: &K) -> Option<&V> {
        self.index.get(k).and_then(|i| self.slots[*i].as_ref()).map(|(_, v)| v)
    }
    pub fn get_mut(&mut self, k: &K) -> Option<&mut V> {
        let i = *self.index.get(k)?;
        self.slots[i].as_mut().map(|(_, v)| v)
    }
    pub fn contains_key(&self, k: &K) -> bool {
        self.index.contains_key(k)
    }
    /// A new key takes the most recently freed slot, or goes last; a known one keeps its slot.
    pub fn insert(&mut self, k: K, v: V) -> Option<V> {
        if let Some(i) = self.index.get(&k) {
            return self.slots[*i].replace((k, v)).map(|(_, v)| v);
        }
        let i = match self.free.pop() {
            Some(i) => i,
            None => {
                self.slots.push(None);
                self.slots.len() - 1
            }
        };
        self.slots[i] = Some((k, v));
        self.index.insert(k, i);
        None
    }
    pub fn remove(&mut self, k: &K) -> Option<V> {
        let i = self.index.remove(k)?;
        self.free.push(i);
        self.slots[i].take().map(|(_, v)| v)
    }
    pub fn iter(&self) -> impl Iterator<Item = (&K, &V)> {
        self.slots.iter().flatten().map(|(k, v)| (k, v))
    }
    pub fn keys(&self) -> impl Iterator<Item = &K> {
        self.iter().map(|(k, _)| k)
    }
    pub fn values(&self) -> impl Iterator<Item = &V> {
        self.iter().map(|(_, v)| v)
    }
    pub fn values_mut(&mut self) -> impl Iterator<Item = &mut V> {
        self.slots.iter_mut().flatten().map(|(_, v)| v)
    }
    /// Removes, in slot order, what `keep` refuses.
    pub fn retain(&mut self, mut keep: impl FnMut(&K, &mut V) -> bool) {
        for i in 0..self.slots.len() {
            if let Some((k, v)) = self.slots[i].as_mut()
                && !keep(k, v)
            {
                let k = *k;
                self.remove(&k);
            }
        }
    }
}

impl<K: Copy + Eq + Hash, V> std::ops::Index<&K> for Slots<K, V> {
    type Output = V;
    fn index(&self, k: &K) -> &V {
        self.get(k).expect("no entry for the key")
    }
}

/// As serialized: the slots (null where free) and the free list; the index is rebuilt.
#[derive(serde::Serialize, serde::Deserialize)]
struct SlotsRepr<T> {
    slots: Vec<Option<T>>,
    free: Vec<usize>,
}

impl<K: serde::Serialize, V: serde::Serialize> serde::Serialize for Slots<K, V> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut st = s.serialize_struct("Slots", 2)?;
        st.serialize_field("slots", &self.slots)?;
        st.serialize_field("free", &self.free)?;
        st.end()
    }
}

impl<'de, K: serde::Deserialize<'de> + Copy + Eq + Hash, V: serde::Deserialize<'de>> serde::Deserialize<'de> for Slots<K, V> {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let r: SlotsRepr<(K, V)> = serde::Deserialize::deserialize(d)?;
        // What a corrupt snapshot could say and a later insert would trip over: a free slot out of
        // range, occupied, or listed twice (its entry overwritten, or an index out of bounds); a
        // hole nobody lists (never reused); a key in two slots.
        let mut listed = vec![false; r.slots.len()];
        for i in &r.free {
            match (listed.get_mut(*i), r.slots.get(*i)) {
                (Some(seen), Some(None)) if !*seen => *seen = true,
                _ => return Err(serde::de::Error::custom(format!("free slot {i} is out of range, occupied or listed twice"))),
            }
        }
        if r.slots.iter().zip(&listed).any(|(s, l)| s.is_none() && !l) {
            return Err(serde::de::Error::custom("a free slot is not in the free list"));
        }
        let index: FxHashMap<K, usize> = r.slots.iter().enumerate().filter_map(|(i, s)| s.as_ref().map(|(k, _)| (*k, i))).collect();
        if index.len() != r.slots.len() - r.free.len() {
            return Err(serde::de::Error::custom("a key is in two slots"));
        }
        Ok(Slots { slots: r.slots, free: r.free, index })
    }
}

/// A .NET HashSet's order: Slots without values.
#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(bound(deserialize = "K: serde::Deserialize<'de> + Copy + Eq + Hash"))]
pub struct SlotSet<K: Copy + Eq + Hash>(Slots<K, ()>);

impl<K: Copy + Eq + Hash> SlotSet<K> {
    pub fn len(&self) -> usize {
        self.0.len()
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    pub fn contains(&self, k: &K) -> bool {
        self.0.contains_key(k)
    }
    /// True when it was not there.
    pub fn insert(&mut self, k: K) -> bool {
        !self.0.contains_key(&k) && self.0.insert(k, ()).is_none()
    }
    pub fn remove(&mut self, k: &K) -> bool {
        self.0.remove(k).is_some()
    }
    pub fn iter(&self) -> impl Iterator<Item = &K> {
        self.0.keys()
    }
}

fn key<K>(entry: &(K, ())) -> &K {
    &entry.0
}

impl<'a, K: Copy + Eq + Hash> IntoIterator for &'a SlotSet<K> {
    type Item = &'a K;
    type IntoIter = std::iter::Map<std::iter::Flatten<std::slice::Iter<'a, Option<(K, ())>>>, fn(&'a (K, ())) -> &'a K>;
    fn into_iter(self) -> Self::IntoIter {
        self.0.slots.iter().flatten().map(key as fn(&'a (K, ())) -> &'a K)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(json: &str) -> Result<Slots<i32, String>, serde_json::Error> {
        serde_json::from_str(json)
    }

    #[test]
    fn slots_round_trip_and_a_corrupt_free_list_is_refused() {
        let mut s: Slots<i32, String> = Slots::default();
        for k in [5, 6, 7] {
            s.insert(k, format!("v{k}"));
        }
        s.remove(&6);
        let back: Slots<i32, String> = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back.keys().copied().collect::<Vec<_>>(), [5, 7]);
        // A hole is reused by the next key, as it was before the round trip.
        let (mut s, mut back) = (s, back);
        s.insert(9, "n".into());
        back.insert(9, "n".into());
        assert_eq!(s.keys().collect::<Vec<_>>(), back.keys().collect::<Vec<_>>());
        // Out of range, occupied, listed twice, a hole nobody lists, a key twice: each would panic
        // or overwrite an entry at some later insert.
        for bad in [
            r#"{"slots": [[1, "a"]], "free": [5]}"#,
            r#"{"slots": [[1, "a"]], "free": [0]}"#,
            r#"{"slots": [null, [1, "a"]], "free": [0, 0]}"#,
            r#"{"slots": [null, [1, "a"]], "free": []}"#,
            r#"{"slots": [[1, "a"], [1, "b"]], "free": []}"#,
        ] {
            assert!(read(bad).is_err(), "{bad}");
        }
        assert!(read(r#"{"slots": [null, [1, "a"]], "free": [0]}"#).is_ok());
    }
}
