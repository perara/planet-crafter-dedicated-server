//! The snapshot codec on its own: any bytes read as the snapshot's types are an error or a value,
//! never a panic, an abort or a huge allocation, and a value reads back from what is written for
//! it, byte for byte (the codec is canonical on what it writes).
use super::*;
use crate::model::{Inventory, WorldObject};
use serde::de::DeserializeOwned;

fn round_trip<T: Serialize + DeserializeOwned>(data: &[u8]) {
    let Ok(value) = from_slice::<T>(data) else { return };
    let mut a = Vec::new();
    append(&value, &mut a).expect("a value read can be written");
    let again = from_slice::<T>(&a).unwrap_or_else(|e| panic!("what the codec wrote does not read back: {e}"));
    let mut b = Vec::new();
    append(&again, &mut b).unwrap();
    assert_eq!(a, b, "writing is not a fixpoint");
}

pub fn run(data: &[u8]) {
    let Some((&which, rest)) = data.split_first() else { return };
    match which % 12 {
        0 => round_trip::<Vec<Option<(i32, WorldObject)>>>(rest),
        1 => round_trip::<Vec<Option<(i32, Inventory)>>>(rest),
        2 => round_trip::<WorldObject>(rest),
        3 => round_trip::<Inventory>(rest),
        4 => round_trip::<crate::idmap::Slots<i32, String>>(rest),
        5 => round_trip::<crate::save::SaveFile>(rest),
        6 => round_trip::<crate::clock::GameTime>(rest),
        7 => round_trip::<Vec<(String, Vec<u8>)>>(rest),
        8 => round_trip::<std::collections::BTreeMap<u64, Vec<(i32, f32)>>>(rest),
        9 => round_trip::<(Option<String>, [f32; 4], Vec<Option<bool>>)>(rest),
        10 => round_trip::<crate::plugins::state::Saved>(rest),
        _ => {
            // As a whole `fill` chunk of world objects.
            let n = rest.first().copied().unwrap_or(0) as usize % 8;
            let mut slots: Vec<std::mem::MaybeUninit<Option<(i32, WorldObject)>>> = (0..n).map(|_| std::mem::MaybeUninit::uninit()).collect();
            if fill::<Option<(i32, WorldObject)>>(rest.get(1..).unwrap_or(&[]), &mut slots).is_ok() {
                for s in slots {
                    // SAFETY: fill returned Ok, so every element is initialised.
                    drop(unsafe { s.assume_init() });
                }
            }
        }
    }
}
