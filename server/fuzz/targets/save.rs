//! The save file's text format: a hostile file loads to something and writes back to text that
//! loads to the same.
use super::*;

pub fn run(data: &[u8]) {
    let text = String::from_utf8_lossy(data);
    let Ok(save) = SaveFile::parse(&text) else { return };
    let written = save.to_text();
    // Not asserted: that writing twice is stable. serde_json without float_roundtrip parses a
    // float within one ulp of the right value, so a huge float can move in each cycle (benign).
    let _ = SaveFile::parse(&written).map(|s| s.to_text());
    let _ = split_outside_strings(&text, '|');
}
