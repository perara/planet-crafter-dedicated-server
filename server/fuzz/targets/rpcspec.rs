//! rpcspec::Spec::decode for every RPC of the game's table, and Spec::parse on hostile text.
use super::Spec;
use serde_json::Value;
use std::sync::OnceLock;

static SPEC: OnceLock<(Spec, Vec<u32>)> = OnceLock::new();

pub fn run(data: &[u8]) {
    let (spec, hashes) = SPEC.get_or_init(|| {
        let spec = Spec::load(&format!("{}/rpcs.json", crate::fuzz_entry::data_dir())).expect("rpcs.json");
        let mut hashes: Vec<u32> = spec.by_hash.keys().copied().collect();
        hashes.sort();
        (spec, hashes)
    });
    if data.len() < 4 {
        return;
    }
    let pick = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
    let hash = hashes[pick % hashes.len()];
    if let Some(args) = spec.decode(hash, &data[4..]) {
        let name = spec.get(hash).unwrap().name.clone();
        let (h, body) = spec.encode(&name, &args);
        assert_eq!(h, hash);
        // What was read can be written, and what was written reads back (not to the same values:
        // a NaN or infinity was Null, which writes as 0).
        let _ = Value::Null;
        spec.decode(hash, &body).expect("what encode wrote decodes");
    }
    // The table itself is read from a file the extraction wrote, but its parser is cheap to try.
    if let Ok(text) = std::str::from_utf8(&data[4..]) {
        let _ = Spec::parse(text);
    }
}
