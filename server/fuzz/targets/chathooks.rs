//! Chat commands, the admin API's chat endpoints and the Event JSON.
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
