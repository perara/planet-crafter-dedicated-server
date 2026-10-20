//! Chat commands and the Event JSON (the feed and its endpoints go with the front's parsers).
use super::*;

pub fn run(data: &[u8]) {
    let text = String::from_utf8_lossy(data).into_owned();
    // Prefix and text from the same input.
    let (prefix, rest) = text.split_once('\n').unwrap_or(("!", &text));
    let _ = command(Some(prefix), rest);
    let _ = command(Some("!"), &text);
    let _ = command(None, &text);
    if let Ok(e) = serde_json::from_slice::<Event>(data) {
        let again = serde_json::to_string(&e).unwrap();
        assert_eq!(serde_json::from_str::<Event>(&again).unwrap(), e);
    }
    crate::front::fuzz::chat(data);
}
