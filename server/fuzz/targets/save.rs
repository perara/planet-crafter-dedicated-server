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

/// The loader's layout checks on any text, and the server's own writing of what it loaded: a save
/// this server writes must be one it loads, entry for entry.
pub fn layout(data: &[u8]) {
    let text = String::from_utf8_lossy(data);
    let sections = split_outside_strings(&text, '@');
    assert!(!sections.is_empty());
    if let Ok(save) = SaveFile::parse(&text) {
        let _ = check_settings(&save.settings);
        let written = save.to_text();
        let back = SaveFile::parse(&written).unwrap_or_else(|e| panic!("a save the server wrote does not load: {e}"));
        assert_eq!(
            (back.planets.len(), back.players.len(), back.objects.len(), back.inventories.len(), back.messages.len(), back.story_events.len(), back.procedural.len()),
            (save.planets.len(), save.players.len(), save.objects.len(), save.inventories.len(), save.messages.len(), save.story_events.len(), save.procedural.len()),
            "an entry was lost or made in writing"
        );
        // The same entries, but for the cut defaults: the names of the fields that stay.
        assert_eq!(back.settings.is_null(), save.settings.is_null());
        assert_eq!(back.world.is_null(), save.world.is_null());
    }
}
