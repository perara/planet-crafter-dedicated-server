//! The front's parsers of bytes from outside: the Steam transport's Split<n> reassembly, the
//! chat config a bank sends, the head of a kept snapshot.
use super::chat_hooks::chat_config;
use super::snapshot::never_runs;
use super::transport::steam::{disconnected, parts, reassemble, MAX_SPLIT};
use crate::msg::Hello;
use serde_json::Value;

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
    let hello = serde_json::from_slice::<Hello>(data).unwrap_or_default();
    let _ = never_runs(data, &hello);
    let _ = disconnected(data.len() as i64 - 8, data.len() as u64, &[(1, 2), (3, 4)]);
}

/// The chat feed's endpoints and the webhook signature, on the same bytes (the chathooks target).
pub fn chat(data: &[u8]) {
    use super::chat_hooks::{api, sign, Hooks};
    let text = String::from_utf8_lossy(data).into_owned();
    let (prefix, rest) = text.split_once('\n').unwrap_or(("!", &text));
    let _ = sign(prefix, rest);
    // The API: no long waits (a fuzz input must not sleep).
    let feed = Hooks::default().feed;
    let mut parts = text.splitn(4, '\n');
    let (method, path, query) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""), parts.next().unwrap_or("").replace("wait", "w"));
    let body: Value = serde_json::from_str(parts.next().unwrap_or("")).unwrap_or(Value::Null);
    let _ = api(&feed, method, path, &query, &body);
    let _ = api(&feed, "GET", "/api/chat/events", &query, &body);
    let _ = api(&feed, "POST", "/api/chat", "", &body);
    let _ = feed.say(rest);
}
