//! The admin requests the front hands to the bank (ADMIN frames): the endpoints that need the
//! world. The front answers its own (updates, stopping, the banks, the invite code, the network)
//! and adds them to what the bank answers.
//!
//! A request is answered by looking up (method, path) in `ROUTES`, a table of `routes::Route`
//! lines (the same format the front's table uses), whose handlers are small functions of
//! `(&mut Game, &Request) -> Reply`, one file for a family of endpoints:
//!
//!   world.rs     status, ping, save, pause, map, the plugins' endpoints
//!   players.rs   kick, ban, unban, teleport
//!   settings.rs  rules and config
//!   worlds.rs    the worlds listing, new, remove, file, restore; backups and their S3 copy
//!
//! A Reply is (HTTP status, JSON text). Errors are `{"error": message}`; an answer is a typed
//! struct (status.rs for the status) or the JSON of the module that made it (`dashboard`, `config`).

use serde::Serialize;
use serde_json::Value;

use super::persist::settle;
use super::status::Ctx;
use crate::game::Game;
use crate::ipc;
use crate::routes::{find, route, Route};

mod players;
mod settings;
mod world;
mod worlds;

/// What a handler is asked: the request and what the bank knows of itself (status.rs).
pub(super) struct Request<'a> {
    pub method: &'a str,
    pub path: &'a str,
    pub body: &'a Value,
    pub ctx: &'a Ctx<'a>,
}

/// (HTTP status, JSON text).
pub(super) type Reply = (u16, String);

type Handler = fn(&mut Game, &Request) -> Reply;

/// Every endpoint of the bank, in the order they are tried.
const ROUTES: &[Route<Handler>] = &[
    // The bank's part of the status; the front adds its own (invite code, network, banks, traffic).
    route("GET", "/api/status", world::status),
    // The front's watchdog asks this of a bank that has been silent: any answer is a loop that runs.
    route("GET", "/api/ping", world::ping),
    // The save, and what became of its backup: a backup that failed is the answer's, too.
    route("POST", "/api/save", world::save),
    route("*", "/api/plugins*", world::plugins),
    route("POST", "/api/kick", players::kick),
    route("POST", "/api/ban", players::ban),
    route("POST", "/api/unban", players::unban),
    // chat.webhooks, chat.secret and chat.exec only as "(set)": they may hold tokens.
    route("GET", "/api/rules", settings::rules),
    // A JSON merge patch of the rules: {"speed": {"growth": 2}}; null resets a rule.
    route("POST", "/api/rules", settings::patch_rules),
    // Every setting with its schema entry and value; a change of any, {"speed.growth": 2}.
    route("GET", "/api/config", settings::config),
    route("POST", "/api/config", settings::set_config),
    route("POST", "/api/teleport", players::teleport),
    route("GET", "/api/worlds", worlds::list),
    route("POST", "/api/worlds/new", worlds::new),
    route("POST", "/api/worlds/remove", worlds::remove),
    route("POST", "/api/worlds/file", worlds::file),
    route("POST", "/api/worlds/restore", worlds::restore),
    // A backup now (after a save), copied to S3 when that is set.
    route("POST", "/api/backup", worlds::backup),
    route("POST", "/api/backup/s3", worlds::backup_to_s3),
    // The admin's pause: the world stands still (clock, machines, growth, gauges) until resumed.
    route("POST", "/api/pause", world::pause),
    route("GET", "/api/map", world::map),
];

/// Answers one request.
pub(super) fn request(game: &mut Game, method: &str, path: &str, body: &Value, ctx: &Ctx) -> Reply {
    // Whatever works on the world files waits for the autosave's write first.
    if method == "POST" && (path.starts_with("/api/worlds") || path.starts_with("/api/backup")) {
        settle(game, true);
    }
    match find(ROUTES, method, path) {
        Some(handler) => handler(game, &Request { method, path, body, ctx }),
        None => error(404, "unknown endpoint"),
    }
}

/// The ANSWER frame for an admin request. One past what a frame holds is never sent (the
/// request would end in the front's 10 s timeout, "the game loop did not answer"): its place is
/// an error that says what to do.
pub(super) fn frame(status: u16, body: String) -> String {
    #[derive(Serialize)]
    struct Frame<'a> {
        status: u16,
        body: &'a str,
    }
    let answer = serde_json::to_string(&Frame { status, body: &body }).unwrap_or_default();
    if ipc::fits(answer.as_bytes()) {
        return answer;
    }
    let message = format!("the answer is {} MiB, more than the link to the front carries ({} MiB): for a world's file, copy it from the worlds folder on the server", answer.len() >> 20, ipc::MAX_FRAME >> 20);
    let error = serde_json::to_string(&Error { error: &message }).unwrap_or_default();
    serde_json::to_string(&Frame { status: 500, body: &error }).unwrap_or_default()
}

#[derive(Serialize)]
struct Error<'a> {
    error: &'a str,
}

#[derive(Serialize)]
struct Done {
    ok: bool,
}

/// `{"error": message}` with this status.
pub(super) fn error(status: u16, message: impl AsRef<str>) -> Reply {
    json(status, &Error { error: message.as_ref() })
}

/// `{"ok": true}`.
pub(super) fn ok() -> Reply {
    json(200, &Done { ok: true })
}

/// A typed answer as JSON text.
pub(super) fn json<T: Serialize>(status: u16, answer: &T) -> Reply {
    match serde_json::to_string(answer) {
        Ok(text) => (status, text),
        Err(e) => error(500, format!("the answer could not be written: {e}")),
    }
}

/// Ok, or 500 with what went wrong (a change that could not be kept).
pub(super) fn kept(result: Result<(), String>) -> Reply {
    match result {
        Ok(()) => ok(),
        Err(e) => error(500, e),
    }
}

/// A request's outcome: its JSON, or 400 with what is wrong.
pub(super) fn answer<T: Serialize>(result: Result<T, String>) -> Reply {
    match result {
        Ok(v) => json(200, &v),
        Err(e) => error(400, e),
    }
}

/// A string of the body, trimmed; None when it is missing, not a string or empty.
pub(super) fn field(body: &Value, key: &str) -> Option<String> {
    body[key].as_str().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string)
}

/// An admin API call as the bank answers it: (status, JSON answer).
#[cfg(test)]
pub(super) fn call(game: &mut Game, method: &str, path: &str, body: Value) -> (u16, Value) {
    let ctx = Ctx { ticks: &Default::default(), save_path: "", build_id: None, data_dir: "", loop_rate: 0, busy: Default::default() };
    let (status, answer) = request(game, method, path, &body, &ctx);
    (status, serde_json::from_str(&answer).unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// An answer too large for a frame is an error the caller can read, not a lost answer.
    #[test]
    fn an_answer_past_a_frame_is_an_error() {
        let small: Value = serde_json::from_str(&frame(200, "{}".into())).unwrap();
        assert_eq!((small["status"].as_u64(), small["body"].as_str()), (Some(200), Some("{}")));
        let big: Value = serde_json::from_str(&frame(200, "x".repeat(ipc::MAX_FRAME))).unwrap();
        assert_eq!(big["status"].as_u64(), Some(500));
        assert!(big["body"].as_str().unwrap().contains("copy it from the worlds folder"), "{big}");
    }

    /// The router answers an unknown path or a known path with the wrong method with the 404 it
    /// always did, and every route of the table is the first to match its own path (none is
    /// shadowed by an earlier one, none is there twice).
    #[test]
    fn unknown_endpoints_are_404_and_no_route_is_shadowed() {
        for (i, r) in ROUTES.iter().enumerate() {
            let (method, probe) = (if r.method == "*" { "GET" } else { r.method }, r.path.trim_end_matches('*'));
            assert_eq!(ROUTES.iter().position(|x| x.matches(method, probe)), Some(i), "{} {}", r.method, r.path);
        }
        assert_eq!(error(404, "unknown endpoint"), (404, r#"{"error":"unknown endpoint"}"#.to_string()));
        let Some(mut game) = crate::game::test_game(crate::game::tests::save()) else { return };
        assert_eq!(call(&mut game, "GET", "/api/nothing", json!({})).0, 404);
        assert_eq!(call(&mut game, "GET", "/api/kick", json!({})).0, 404);
        assert_eq!(call(&mut game, "DELETE", "/api/status", json!({})).0, 404);
    }

    /// The replies the handlers build by hand before they were typed: the same bytes for the
    /// messages the endpoints give (quotes and non-ASCII are escaped as the json! macro did).
    #[test]
    fn replies_are_the_bytes_the_json_macro_made() {
        let old_error = |e: &str| json!({"error": e}).to_string();
        for message in ["player not online", "'id' or 'name' is required", "'id': a client id", "that client has no name yet: kick it instead", "backups are off (the rule save.backupsKept is 0)", "2 players online are named \"Ann\": give the client id", "tab\there \\ and \u{e5}", "'paused': true or false", "position: three finite numbers", "'backup' and 'name' are required", "player not online and ready", "player not online: ban by name", "'value' is required", "'name' is required", "unknown endpoint", "too many\nlines"] {
            assert_eq!(error(400, message).1, old_error(message), "{message}");
            let owned = message.to_string();
            assert_eq!(error(500, owned).1, old_error(message));
        }
        assert_eq!(ok(), (200, "{\"ok\":true}".to_string()));
        assert_eq!(kept(Ok(())), ok());
        assert_eq!(kept(Err("disk \"full\"".into())), (500, json!({"error": "disk \"full\""}).to_string()));
        assert_eq!(answer(Ok(json!({"a": [1, 2.5, null], "b": "é"}))), (200, json!({"a": [1, 2.5, null], "b": "é"}).to_string()));
        assert_eq!(answer::<Value>(Err("bad".into())), (400, old_error("bad")));
        let frame_old = |status: u16, body: &str| json!({"status": status, "body": body}).to_string();
        for (status, body) in [(200, "{}"), (400, "{\"error\":\"x\\\"y\"}"), (500, ""), (200, "ünï\ncode")] {
            assert_eq!(frame(status, body.to_string()), frame_old(status, body));
        }
    }
}
