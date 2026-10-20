//! The front's parsers of bytes from outside: the Steam transport's Split<n> reassembly, the
//! chat config a bank sends, the head of a kept snapshot.
use super::transport::steam::{disconnected, parts, reassemble, MAX_SPLIT};
use super::*;

pub fn run(data: &[u8]) {
    // Pieces of one client's stream, cut where the input says.
    let mut split = None;
    let step = 1 + data.first().map_or(0, |b| *b as usize % 16);
    for piece in data.chunks(step) {
        match reassemble(&mut split, piece) {
            Ok(Some(whole)) => assert!(whole.len() <= MAX_SPLIT.max(piece.len())),
            Ok(None) => {}
            Err(_) => split = None,
        }
    }
    let _ = parts(data);
    let _ = chat_config(data);
    let hello = serde_json::from_slice::<Value>(data).unwrap_or(Value::Null);
    let _ = never_runs(data, &hello);
    let _ = disconnected(data.len() as i64 - 8, data.len() as u64, &[(1, 2), (3, 4)]);
}
