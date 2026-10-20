//! The admin API's routes. One table says, for each, who answers it, what it takes and what it
//! does; the front's loop and the HTTP threads (admin.rs) both route by it, `docs/admin-api.md`
//! is written from it (a test keeps the two equal), and a test holds the bank's routes to the
//! source (every `("POST", "/api/...")` arm of the bank is a line here, and the other way round).
//!
//! Three answer a request:
//! - the HTTP thread (admin.rs), on its own thread: the web page (no token), the logs and the
//!   chat endpoints, so that a long poll or a slow journal never holds the game loop;
//! - the front's loop: the process's own business (stopping the server, updates and flips, the
//!   world's controls), and while the world is stopped, its status and the worlds' list;
//! - the bank, for everything about the world: the front passes the request on (ADMIN) and the
//!   answer back (ANSWER), and adds its part to `GET /api/status`.
//!
//! Every route but the web page and the health route needs "Authorization: Bearer <token>".

use std::collections::HashMap;
use std::sync::mpsc::{Receiver, Sender};

use serde_json::{json, Value};

use super::{Front, Phase};
use crate::admin;
use crate::ipc;
use crate::msg::{self, AdminAnswer, AdminCall};

/// Who answers a route.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Owner {
    /// The HTTP thread (admin.rs).
    Http(Http),
    /// The front's loop, in any phase (`When::Always`) or only while the world is stopped.
    Loop(Handler, When),
    /// The bank.
    Bank,
}

/// What the HTTP thread does itself.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Http {
    /// The web page of the live bank's release.
    Page,
    /// The journal of the systemd unit.
    Logs,
    /// front/chat_hooks.rs's endpoints (any other `/api/chat...` path is a 404 from there).
    Chat,
}

/// The front's handlers.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Handler {
    Stop,
    UpdateStatus,
    Update,
    WorldStop,
    WorldStart,
    WorldSwitch,
    Status,
    Worlds,
    /// GET /api/health: no token.
    Health,
    /// GET /api/metrics while the world is stopped.
    Metrics,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum When {
    Always,
    WhileStopped,
}

/// What the table says of a route besides its method and path (routes.rs holds those: the bank's
/// table is the same type with a function for its handler).
pub struct Entry {
    pub owner: Owner,
    /// The request body (or the query), as it is read; "" for none. Documentation: docs/admin-api.md
    /// is written from it.
    #[cfg_attr(not(test), allow(dead_code))]
    pub takes: &'static str,
    #[cfg_attr(not(test), allow(dead_code))]
    pub does: &'static str,
}

/// A line of the table; the path is exact or a prefix with a `*` at the end.
pub type Route = crate::routes::Route<Entry>;

impl Route {
    /// Whether a token is needed: not for the web page, which asks for it itself, nor for the
    /// health route, which says only whether the server serves a world.
    pub fn needs_token(&self) -> bool {
        !matches!(self.handler.owner, Owner::Http(Http::Page) | Owner::Loop(Handler::Health, _))
    }
}

const fn route(method: &'static str, path: &'static str, owner: Owner, takes: &'static str, does: &'static str) -> Route {
    crate::routes::route(method, path, Entry { owner, takes, does })
}

const PAGE: Owner = Owner::Http(Http::Page);
const ALWAYS: When = When::Always;
const STOPPED: When = When::WhileStopped;

/// Every route of the admin API.
pub const ROUTES: &[Route] = &[
    route("GET", "/", PAGE, "", "The web page (no token)."),
    route("GET", "/index.html", PAGE, "", "The same."),
    route("GET", "/api/logs", Owner::Http(Http::Logs), "", "The server's recent output, from the journal of its systemd unit (`lines`, `source` or a `note` why there are none)."),
    route("GET", "/api/chat/events", Owner::Http(Http::Chat), "query `after=ID&wait=SECONDS`", "The chat events after ID, waiting up to SECONDS (at most 30) for one: `events`, `last`, `dropped`. At most 8 polls wait at once."),
    route("POST", "/api/chat", Owner::Http(Http::Chat), "`{\"text\": \"...\"}`", "Says the text in the game's chat, from the server."),
    route("*", "/api/chat*", Owner::Http(Http::Chat), "", "Any other path under /api/chat: 404."),
    route("POST", "/api/stop", Owner::Loop(Handler::Stop, ALWAYS), "", "Saves the world and stops the server."),
    route("GET", "/api/update", Owner::Loop(Handler::UpdateStatus, ALWAYS), "", "The update state: `state`, `doing`, `seq`, `last`, `history` (the last 20), `auto`, `checkHours`, `latest`, `available`, `checkedAt`, `releases`."),
    route("POST", "/api/update", Owner::Loop(Handler::Update, ALWAYS), "`{\"version\"?: \"v0.6.0\", \"onlyNewer\"?: bool, \"check\"?: bool, \"rollback\"?: bool}`", "Downloads a release into the standby bank and flips to it, only looks up the latest (`check`), or goes back to the other bank (`rollback`). Answers at once with `seq`; the outcome is in GET /api/update's `history`."),
    route("POST", "/api/world/stop", Owner::Loop(Handler::WorldStop, ALWAYS), "", "Tells every player, saves the world and ends its bank; the page and API stay."),
    route("POST", "/api/world/start", Owner::Loop(Handler::WorldStart, ALWAYS), "`{\"world\"?: \"name\"}`", "Starts the world that ran, or another one, in the active bank."),
    route("POST", "/api/world/switch", Owner::Loop(Handler::WorldSwitch, ALWAYS), "`{\"world\": \"name\"}`", "Stops the world that runs, if any, and starts another."),
    route("GET", "/api/health", Owner::Loop(Handler::Health, ALWAYS), "", "Liveness for container probes and uptime monitors, no token: 200 `{\"ok\":true}` while the server serves a world (a hot update does not change that: the old game serves until the new one takes over), 503 `{\"ok\":false}` while there is no game process to serve one (the world is stopped, still starting, or starting again after a crash) or the loop does not answer within 3 s. Nothing else is said."),
    route("GET", "/api/metrics", Owner::Loop(Handler::Metrics, STOPPED), "", "While the world is stopped: the front's numbers alone, in the Prometheus text format."),
    route("GET", "/api/status", Owner::Loop(Handler::Status, STOPPED), "", "While the world is stopped: the front's part of the status, and which world it was."),
    route("GET", "/api/worlds", Owner::Loop(Handler::Worlds, STOPPED), "", "While the world is stopped: the worlds and the backups."),
    route("GET", "/api/status", Owner::Bank, "", "The server's state: world, planet, the DLC planets stated as owned, players, rules changed, loop times, save and backup state, tables, plugins; the front adds the network, the invite code, the process, the banks and the update state."),
    route("GET", "/api/metrics", Owner::Bank, "", "The numbers for a monitor in the Prometheus text format (version 0.0.4, `text/plain`), not JSON: players online and the limit, tick times, loop load, memory of the front and the game, objects, saves, backups, the off-site copies, hand-overs, crashes, the watchdog, the waiting queue, plugins by state and what each costs. The scoped token scope `metrics.read` allows this route alone (docs/monitoring.md)."),
    route("GET", "/api/ping", Owner::Bank, "", "Any answer is a game loop that runs (the front's watchdog asks it)."),
    route("POST", "/api/save", Owner::Bank, "", "Saves the world now (and its backup, when they are on)."),
    route("POST", "/api/kick", Owner::Bank, "`{\"id\": N}` or `{\"name\": \"...\"}`", "Kicks a player."),
    route("POST", "/api/ban", Owner::Bank, "`{\"id\": N}` or `{\"name\": \"...\"}`", "Bans a player (their name, SteamID and IP) and kicks them."),
    route("POST", "/api/unban", Owner::Bank, "`{\"value\": \"...\"}`", "Lifts a ban."),
    route("GET", "/api/rules", Owner::Bank, "", "The rules as they are and the defaults."),
    route("POST", "/api/rules", Owner::Bank, "a JSON merge patch of the rules", "Changes rules; `null` resets one."),
    route("GET", "/api/config", Owner::Bank, "", "Every setting with its type, range, unit, default, value and whether it applies live."),
    route("POST", "/api/config", Owner::Bank, "`{\"speed.growth\": 2, ...}`", "Changes settings by name."),
    route("POST", "/api/teleport", Owner::Bank, "`{\"id\"? or \"name\"?, \"position\": [x, y, z]}`", "Moves a player."),
    route("GET", "/api/worlds", Owner::Bank, "", "The worlds beside the running one, its backups, the choices for a new world, the backup rules and S3 state."),
    route("POST", "/api/worlds/new", Owner::Bank, "`{\"name\", \"planet\"?, \"start\"?, \"seed\"?, \"mode\"?, \"dying\"?}`", "Makes a new world beside the running one."),
    route("POST", "/api/worlds/remove", Owner::Bank, "`{\"name\"}`", "Moves a world into backups/ (never deletes it); not the running one."),
    route("POST", "/api/worlds/file", Owner::Bank, "`{\"name\", \"backup\"?: bool}`", "A world's or a backup's file text."),
    route("POST", "/api/worlds/restore", Owner::Bank, "`{\"backup\", \"name\"}`", "A backup as a new world."),
    route("POST", "/api/backup", Owner::Bank, "", "Saves and makes a backup now, copied to S3 when that is set."),
    route("POST", "/api/backup/s3", Owner::Bank, "", "Copies the newest backup to S3."),
    route("POST", "/api/pause", Owner::Bank, "`{\"paused\": bool}`", "The world stands still until resumed."),
    route("GET", "/api/map", Owner::Bank, "", "The buildings on this planet, for the players' map."),
    route("GET", "/api/plugins", Owner::Bank, "", "Every plugin with its status, capabilities, budgets and counters."),
    route("POST", "/api/plugins/reload", Owner::Bank, "", "Reads the plugins' folder again."),
    route("POST", "/api/plugins/enable", Owner::Bank, "`{\"id\", \"grant\"?: [...], \"digest\"?}`", "Approves a plugin (all it asks for, by default) and reloads; with the `digest` the listing showed, only if the files are still those."),
    route("POST", "/api/plugins/disable", Owner::Bank, "`{\"id\"}`", "Disables a plugin."),
    route("POST", "/api/plugins/config", Owner::Bank, "`{\"id\", \"key\", \"value\"?: \"...\"}`", "Sets a config key the plugin declares (an empty or missing value clears it) and reloads."),
    route("POST", "/api/plugins/trust", Owner::Bank, "`{\"publicKey\", \"name\"?, \"grant\"?: [...], \"force\"?: bool}`", "Trusts a publisher's Ed25519 key: plugins signed by it load without approving each file's digest, with the capabilities granted."),
    route("POST", "/api/plugins/untrust", Owner::Bank, "`{\"id\"}`", "Revokes a trusted key (its id or name) and reloads."),
    route("GET", "/api/plugins/tokens", Owner::Bank, "", "The scoped tokens (never the token itself), the scopes there are and the rate limits."),
    route("POST", "/api/plugins/tokens", Owner::Bank, "`{\"name\", \"scopes\": [...], \"ratePerMinute\"?}`", "Makes a scoped token for an out-of-process plugin; the answer's `token` is shown this once."),
    route("POST", "/api/plugins/tokens/revoke", Owner::Bank, "`{\"id\"}` (the id or the name)", "Revokes a scoped token at once."),
    route("POST", "/api/plugins/events", Owner::Bank, "`{\"after\"?, \"types\"?, \"limit\"?, \"consumer\"?, \"unregister\"?, \"wait\"?: SECONDS}`", "The plugin event stream; with `wait` (at most 25 s; at most 8 polls wait at once) the front holds the request until an event comes or the wait is over. At most 16 consumer names are registered at once (429 for a new one beyond)."),
];

/// The route the HTTP thread answers itself for a request: the first line of the table that
/// matches (the table puts exact paths before prefixes).
pub fn find_http(method: &str, path: &str) -> Option<&'static Route> {
    ROUTES.iter().find(|r| r.matches(method, path) && matches!(r.handler.owner, Owner::Http(_)))
}

/// Whether a request needs no token: the route the table matches first says so (the web page and
/// the health route). An unknown path needs one, so a stranger learns nothing about what exists.
pub fn is_open(method: &str, path: &str) -> bool {
    ROUTES.iter().find(|r| r.matches(method, path)).is_some_and(|r| !r.needs_token())
}

/// The front's handler for a request in this phase, if the front answers it itself.
fn front_handler(method: &str, path: &str, stopped: bool) -> Option<Handler> {
    ROUTES.iter().filter(|r| r.matches(method, path)).find_map(|r| match r.handler.owner {
        Owner::Loop(h, When::Always) => Some(h),
        Owner::Loop(h, When::WhileStopped) if stopped => Some(h),
        _ => None,
    })
}

/// A route's answer for an error: `{"error": message}`.
pub(super) fn error(status: u16, message: &str) -> (u16, String) {
    (status, json!({"error": message}).to_string())
}

/// The admin requests the front has taken from the HTTP threads, and those it passed to a bank.
pub(super) struct Admin {
    /// The requests of the HTTP threads (None: no listener).
    rx: Option<Receiver<admin::Request>>,
    /// The requests passed to the bank, by their number: where to reply, and the path.
    answers: HashMap<u64, (Sender<(u16, String)>, String)>,
    next_request: u64,
    /// The web page's file: the live bank's, so a flip brings its page along.
    pub web: admin::Web,
}

impl Admin {
    pub fn new() -> Admin {
        Admin { rx: None, answers: HashMap::new(), next_request: 1, web: Default::default() }
    }

    /// Starts the HTTP listener (PCDS_ADMIN, with PCDS_ADMIN_TOKEN), serving the chat feed.
    /// Err: the address cannot be listened on, which stops the start (a server nobody can
    /// manage, or whose admin address is another server's, is not a running server).
    pub fn start(&mut self, feed: std::sync::Arc<super::Feed>, save: &str) -> Result<(), String> {
        let addr = crate::env("PCDS_ADMIN").unwrap_or_else(|| "127.0.0.1:7782".into());
        if addr == "off" {
            return Ok(());
        }
        let Some(token) = crate::env("PCDS_ADMIN_TOKEN") else {
            warn!("admin: PCDS_ADMIN_TOKEN is not set: the admin API and web page are off (pcds setup writes one)");
            return Ok(());
        };
        // Scoped tokens (run/plugins/tokens.json, beside the plugins the bank hosts) may make the requests of their scopes.
        let tokens = std::sync::Arc::new(crate::tokens::Checker::new(std::path::PathBuf::from(crate::config::installation_file(&crate::env, save, "plugins"))));
        match admin::start_with(&addr, token, self.web.clone(), feed, tokens) {
            Ok(rx) => {
                info!("admin: http://{addr}/");
                // Plain HTTP: off this machine the token and every action travel in the clear.
                // The container binds 0.0.0.0 inside its own network; its port is published on
                // 127.0.0.1.
                let loopback = addr.rsplit_once(':').and_then(|(h, _)| h.trim_matches(['[', ']']).parse::<std::net::IpAddr>().ok()).is_some_and(|ip| ip.is_loopback());
                if !loopback && crate::env("PCDS_CONTAINER").is_none() {
                    warn!("admin: WARNING: {addr} is reachable from other machines over plain HTTP; bind 127.0.0.1 and use an SSH tunnel or an HTTPS proxy");
                }
                self.rx = Some(rx);
                Ok(())
            }
            Err(e) => Err(format!("admin page {addr}: {e}. Another program (a second pcds server?) may use that port: change PCDS_ADMIN in server.env ('pcds config'), or set it to off")),
        }
    }

    /// The web page to serve from now on: the file of the live bank's release.
    pub fn set_page(&self, file: Option<String>) {
        *self.web.write().unwrap_or_else(|e| e.into_inner()) = file;
    }

    /// A number for a request to a bank (the watchdog's ping takes one too).
    pub fn next_id(&mut self) -> u64 {
        let id = self.next_request;
        self.next_request += 1;
        id
    }

    /// Whether a request to the bank waits for an answer for one of the paths.
    pub fn waits_for(&self, paths: impl Fn(&str) -> bool) -> bool {
        self.answers.values().any(|(_, path)| paths(path))
    }

    /// The requests waiting for the bank are dropped: their HTTP threads answer that the game
    /// loop did not (the bank that had them is gone).
    pub fn forget_answers(&mut self) {
        self.answers.clear();
    }

    /// The requests waiting for the bank are answered with an error.
    pub fn refuse_answers(&mut self, status: u16, message: &str) {
        for (_, (reply, _)) in self.answers.drain() {
            let _ = reply.send((status, json!({"error": message}).to_string()));
        }
    }
}

impl Front {
    /// The admin requests that came in: each answered here, or passed to the bank.
    pub(super) fn admin_requests(&mut self) {
        let Some(rx) = &self.admin.rx else { return };
        let requests: Vec<admin::Request> = rx.try_iter().collect();
        for r in requests {
            let stopped = self.phase == Phase::Stopped;
            match front_handler(&r.method, &r.path, stopped) {
                Some(handler) => {
                    let answer = self.answer_front(handler, &r.body);
                    let _ = r.reply.send(answer);
                    if handler == Handler::Stop {
                        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
                    }
                }
                None if stopped => {
                    let _ = r.reply.send(error(409, "the world is stopped: start it first"));
                }
                None => {
                    let id = self.admin.next_id();
                    let call = AdminCall { method: r.method, path: r.path.clone(), body: r.body };
                    self.admin.answers.insert(id, (r.reply, r.path));
                    self.to_bank(ipc::ADMIN, id, &msg::body(&call), 0);
                }
            }
        }
    }

    /// What the front itself says to a request it answers.
    fn answer_front(&mut self, handler: Handler, body: &Value) -> (u16, String) {
        match handler {
            Handler::Stop => (200, "{\"ok\":true}".into()),
            Handler::UpdateStatus => (200, self.update_status().to_string()),
            Handler::Update => self.update_request(body),
            Handler::WorldStop => self.answer_world_stop(),
            Handler::WorldStart => self.answer_world_start(body),
            Handler::WorldSwitch => self.answer_world_switch(body),
            Handler::Status => (200, self.stopped_status().to_string()),
            Handler::Worlds => (200, crate::worlds::overview(&self.save).to_string()),
            Handler::Health => {
                let ok = self.live.is_some();
                (if ok { 200 } else { 503 }, json!({"ok": ok}).to_string())
            }
            Handler::Metrics => (200, self.metrics_text(None)),
        }
    }

    /// The bank's ANSWER to a request: replied to the HTTP thread that waits for it, with the
    /// front's part added to the status.
    pub(super) fn answer(&mut self, id: u64, body: &[u8]) {
        let Some((reply, path)) = self.admin.answers.remove(&id) else { return };
        let a: AdminAnswer = msg::parse(body).unwrap_or_default();
        let status = a.status.unwrap_or(500) as u16;
        let mut text = a.body.unwrap_or_else(|| "{}".to_string());
        if path == "/api/metrics" && status == 200 {
            text = self.metrics_text(serde_json::from_str(&text).ok());
        }
        if path == "/api/status"
            && let Ok(mut s) = serde_json::from_str::<Value>(&text)
        {
            self.overlay(&mut s);
            text = s.to_string();
        }
        let _ = reply.send((status, text));
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::test_front;
    use super::*;
    use std::collections::BTreeSet;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    /// docs/admin-api.md, from the table.
    fn markdown() -> String {
        let mut out = String::from(
            "# The admin API\n\nGenerated from the table in `server/src/front/admin_routes.rs` (a test keeps the two equal; the text below\nis what it expects). Every route but the web page and `GET /api/health` needs `Authorization: Bearer <token>` (docs/hot-update.md, README).\nThe answers are JSON (`GET /api/metrics` is Prometheus text); an error is `{\"error\": \"...\"}` with the status. Who answers: the HTTP thread (admin.rs: its own\nthread, never held by the game loop), the front's loop (the process: updates, stopping, the world's controls), or the bank\n(the world; the front passes the request on and adds its part to `GET /api/status`).\n\n| Route | Answered by | Takes | Does |\n|---|---|---|---|\n",
        );
        for r in ROUTES {
            let by = match r.handler.owner {
                Owner::Http(_) => "HTTP thread",
                Owner::Loop(_, When::Always) => "front",
                Owner::Loop(_, When::WhileStopped) => "front, while the world is stopped",
                Owner::Bank => "bank",
            };
            let method = if r.method == "*" { "any" } else { r.method };
            out += &format!("| `{method} {}` | {by} | {} | {} |\n", r.path, if r.handler.takes.is_empty() { "nothing" } else { r.handler.takes }, r.handler.does);
        }
        out
    }

    #[test]
    fn docs_admin_api_is_written_from_the_table() {
        let file = Path::new(env!("CARGO_MANIFEST_DIR")).join("../docs/admin-api.md");
        let text = std::fs::read_to_string(&file).unwrap_or_default();
        assert!(text == markdown(), "{} is not what the table says; it should be:\n\n{}", file.display(), markdown());
    }

    fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                rust_files(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }

    /// Every `("POST", "/api/...")` arm of the bank, the plugins and the chat feed (not the command
    /// line, which calls them) is a line of the table, and every line of theirs is an arm: the table is
    /// the API, not a list of it.
    #[test]
    fn the_table_lists_the_routes_the_bank_and_the_feed_answer_and_no_others() {
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        rust_files(&src, &mut files);
        let mut found = BTreeSet::new();
        for file in files.iter().filter(|f| (!f.starts_with(src.join("front")) || f.ends_with("chat_hooks.rs")) && !f.starts_with(src.join("cli")) && !f.ends_with("coverage.rs") && !f.ends_with("tokens.rs") && !f.ends_with("src/admin.rs")) {
            let text = std::fs::read_to_string(file).unwrap();
            for method in ["GET", "POST"] {
                let pattern = format!("(\"{method}\", \"/api/");
                for (at, _) in text.match_indices(&pattern) {
                    let path = text[at + pattern.len() - 5..].split('"').next().unwrap();
                    found.insert(format!("{method} {path}"));
                }
            }
        }
        let table: BTreeSet<String> = ROUTES.iter().filter(|r| r.handler.owner == Owner::Bank || (r.handler.owner == Owner::Http(Http::Chat) && r.method != "*")).map(|r| format!("{} {}", r.method, r.path)).collect();
        assert_eq!(found, table, "left: the arms in the source; right: the table (front/admin_routes.rs)");
    }

    /// A world control with a 'world' that is no name is refused, not read as "the world that ran".
    #[test]
    fn a_start_with_a_world_that_is_no_name_is_refused() {
        let mut front = test_front();
        front.phase = Phase::Stopped;
        let (tx, rx) = std::sync::mpsc::channel();
        front.admin.rx = Some(rx);
        let (reply, answer) = std::sync::mpsc::channel();
        tx.send(admin::Request { method: "POST".into(), path: "/api/world/start".into(), body: json!({"world": 5}), reply }).unwrap();
        front.admin_requests();
        let (status, body) = answer.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(status == 400 && body.contains("a world's name"), "{status} {body}");
    }

    /// The front answers a request itself, or passes it on, as the table says in each phase.
    #[test]
    fn the_table_decides_who_answers_in_each_phase() {
        use Handler::*;
        assert_eq!(front_handler("POST", "/api/stop", false), Some(Stop));
        assert_eq!(front_handler("POST", "/api/stop", true), Some(Stop));
        assert_eq!(front_handler("GET", "/api/update", true), Some(UpdateStatus));
        assert_eq!(front_handler("POST", "/api/world/switch", false), Some(WorldSwitch));
        // The status and the worlds are the bank's while it runs, the front's while it does not.
        assert_eq!(front_handler("GET", "/api/status", false), None);
        assert_eq!(front_handler("GET", "/api/status", true), Some(Status));
        assert_eq!(front_handler("GET", "/api/worlds", false), None);
        assert_eq!(front_handler("GET", "/api/worlds", true), Some(Worlds));
        // The rest is the bank's; the HTTP thread's routes never reach the front.
        for (m, p) in [("POST", "/api/save"), ("GET", "/api/config"), ("POST", "/api/worlds/new"), ("GET", "/api/nothing"), ("GET", "/api/logs"), ("POST", "/api/chat")] {
            assert_eq!(front_handler(m, p, false), None, "{m} {p}");
            assert_eq!(front_handler(m, p, true), None, "{m} {p}");
        }
        // The HTTP thread's routes: the page without a token, the chat by prefix.
        let page = find_http("GET", "/").unwrap();
        assert!(page.handler.owner == Owner::Http(Http::Page) && !page.needs_token());
        assert_eq!(find_http("GET", "/index.html").unwrap().handler.owner, Owner::Http(Http::Page));
        assert_eq!(find_http("GET", "/api/logs").unwrap().handler.owner, Owner::Http(Http::Logs));
        for (m, p) in [("GET", "/api/chat/events"), ("POST", "/api/chat"), ("DELETE", "/api/chatty")] {
            let r = find_http(m, p).unwrap();
            assert!(r.handler.owner == Owner::Http(Http::Chat) && r.needs_token(), "{m} {p}");
        }
        assert!(find_http("GET", "/api/status").is_none() && find_http("POST", "/").is_none());
    }

    /// What the web page can be checked for without a browser: every request it makes is a route of
    /// the table, its narrow-screen grids may shrink (a grid column of `1fr` is as wide as its widest
    /// content, which made three pages scroll sideways on a phone), and the colours of both themes
    /// read at 4.5:1 on their backgrounds.
    #[test]
    fn the_web_page_asks_for_routes_that_exist_and_reads_on_every_screen() {
        let page = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/web/index.html")).unwrap();
        let mut calls = 0;
        for call in page.split("api('").skip(1) {
            let Some((method, rest)) = call.split_once("', ") else { continue };
            let Some(path) = rest.strip_prefix(['\'', '`']) else { continue };
            let path = path.split(['\'', '`', '?', '$']).next().unwrap();
            calls += 1;
            assert!(ROUTES.iter().any(|r| r.matches(method, path)), "the page calls {method} {path}: no such route");
        }
        assert!(calls > 30, "{calls} calls found: the scan lost the page's api() calls");
        for query in page.lines().filter(|l| l.contains("@media (max-width")) {
            assert!(!query.contains("grid-template-columns: 1fr"), "use minmax(0, 1fr) in a narrow-screen grid: {query}");
        }
        // Colour tokens by theme: the light ones in :root, the dark ones in :root[data-theme=dark].
        let block = |start: &str| -> std::collections::HashMap<String, (u8, u8, u8)> {
            let from = page.find(start).unwrap();
            let body = &page[from..from + page[from..].find('}').unwrap()];
            let mut tokens = std::collections::HashMap::new();
            for decl in body.split(';') {
                let Some((name, value)) = decl.rsplit_once(": ") else { continue };
                let (name, value) = (name.split_whitespace().last().unwrap_or(""), value.trim());
                if let Some(hex) = value.strip_prefix('#').filter(|h| h.len() == 6) {
                    let n = u32::from_str_radix(hex, 16).unwrap();
                    tokens.insert(name.to_string(), ((n >> 16) as u8, (n >> 8) as u8, n as u8));
                }
            }
            tokens
        };
        let luminance = |(r, g, b): (u8, u8, u8)| {
            let c = |v: u8| { let v = f64::from(v) / 255.0; if v <= 0.03928 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) } };
            0.2126 * c(r) + 0.7152 * c(g) + 0.0722 * c(b)
        };
        let ratio = |a, b| { let (x, y) = (luminance(a), luminance(b)); (x.max(y) + 0.05) / (x.min(y) + 0.05) };
        for (theme, start) in [("light", ":root {"), ("dark", ":root[data-theme=dark] {")] {
            let t = block(start);
            for (fg, bg) in [("--text", "--panel"), ("--muted", "--panel"), ("--muted", "--bg"), ("--accent", "--panel"), ("--accent", "--accent-soft"), ("--accent2", "--panel"), ("--danger", "--panel"), ("--danger", "--danger-soft"), ("--warn", "--panel"), ("--warn", "--warn-soft"), ("--on-accent", "--accent"), ("--on-danger", "--danger")] {
                let (a, b) = (t.get(fg).copied().unwrap_or_else(|| panic!("{theme}: no {fg}")), t.get(bg).copied().unwrap_or_else(|| panic!("{theme}: no {bg}")));
                assert!(ratio(a, b) >= 4.5, "{theme}: {fg} on {bg} is {:.2}:1", ratio(a, b));
            }
        }
    }

    /// Nothing is in the table twice, and the front's routes are not shadowed by an earlier one.
    #[test]
    fn the_table_has_each_route_once() {
        let mut seen = std::collections::HashSet::new();
        for r in ROUTES {
            assert!(seen.insert((r.method, r.path, format!("{:?}", r.handler.owner))), "{} {} twice", r.method, r.path);
            assert!(!r.handler.does.is_empty(), "{} {} says nothing", r.method, r.path);
        }
        for r in ROUTES.iter().filter(|r| matches!(r.handler.owner, Owner::Loop(..))) {
            let handler = front_handler(r.method, r.path, true);
            assert!(matches!(r.handler.owner, Owner::Loop(h, _) if Some(h) == handler), "{} {} is answered by {handler:?}", r.method, r.path);
        }
    }

    /// Every route of the table, every method and the paths that could be mistaken for it, with a token of
    /// each scope and a token that was never made: only the six requests the scopes name pass, and each
    /// of them lands on the route that is meant (the first the table matches), so a scope cannot reach
    /// a route by a path the token check reads one way and the table another.
    #[test]
    fn a_scoped_token_reaches_only_the_six_routes_of_its_scopes_and_each_lands_where_it_is_meant() {
        use crate::tokens::{create, scope_for, Checker, Denied, SCOPES};
        let dir = std::env::temp_dir().join(format!("pcds-routes-scopes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let tokens: Vec<(&str, String)> = SCOPES.iter().map(|(scope, _)| (*scope, create(&dir, &scope.replace('.', "-"), &[scope.to_string()], Some(6000)).unwrap().1)).collect();
        let check = Checker::new(dir.clone());
        let mut paths: Vec<(String, String)> = Vec::new();
        for r in ROUTES {
            let base = r.path.trim_end_matches('*');
            for method in ["GET", "POST", "PUT", "DELETE", "HEAD", "get", "post"] {
                for path in [base.to_string(), format!("{base}/"), format!("{base}x"), format!("{base}/x"), format!("{base}%2f"), format!("{base}?a=b"), base.to_uppercase(), format!("/{base}")] {
                    paths.push((method.to_string(), path));
                }
            }
        }
        let mut passed = std::collections::BTreeSet::new();
        for (method, path) in &paths {
            for (scope, token) in &tokens {
                let answer = check.authorize(&format!("Bearer {token}"), method, path);
                match answer {
                    Ok(_) => {
                        assert_eq!(scope_for(method, path), Some(*scope), "{scope} passed {method} {path}");
                        passed.insert((method.clone(), path.clone(), scope.to_string()));
                    }
                    Err(Denied::Scope(needs)) => assert_ne!(needs, Some(*scope), "{scope} refused {method} {path} for a scope it has"),
                    Err(e) => panic!("{scope} {method} {path}: {e:?}"),
                }
            }
            assert_eq!(check.authorize("Bearer pcdst_nothing", method, path), Err(Denied::Unknown));
        }
        let want: Vec<(&str, &str, &str, Owner)> = vec![
            ("POST", "/api/plugins/events", "events.read", Owner::Bank),
            ("GET", "/api/chat/events", "chat.read", Owner::Http(Http::Chat)),
            ("POST", "/api/chat", "chat.send", Owner::Http(Http::Chat)),
            ("POST", "/api/kick", "player.kick", Owner::Bank),
            ("POST", "/api/teleport", "player.teleport", Owner::Bank),
            ("GET", "/api/metrics", "metrics.read", Owner::Loop(Handler::Metrics, STOPPED)),
        ];
        for (method, path, scope, owner) in &want {
            assert!(passed.contains(&(method.to_string(), path.to_string(), scope.to_string())), "{scope} must pass {method} {path}");
            let first = ROUTES.iter().find(|r| r.matches(method, path)).unwrap();
            assert_eq!(first.handler.owner, *owner, "{method} {path} lands on {:?}", first.handler.owner);
        }
        assert_eq!(passed.len(), want.len(), "only those six: {passed:?}");
        let _ = std::fs::remove_dir_all(dir);
    }
}
