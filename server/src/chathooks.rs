//! Chat hooks: every chat message players send becomes an event for tools outside the game.
//! The game thread only adds it to a ring of the last KEPT events and returns; one worker
//! thread POSTs each event to the rules' chat.webhooks (signed with chat.secret) and runs
//! chat.exec with it, and the admin API's threads read the ring (GET /api/chat/events, a long
//! poll). A hook more than KEPT events behind loses the oldest; they are counted (`dropped`).
//! Replies (a webhook's {"reply": "..."}, the exec program's output, POST /api/chat) come back
//! through a bounded channel and go to the chat at the next tick, from the server (client 0).
//! Chat text is never a command line: the exec program gets it only as JSON on stdin.

use std::collections::VecDeque;
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::{json, Value};

use crate::game::Game;
use crate::net::Target;
use crate::rules::Chat;

/// Events kept for the hooks and the API.
const KEPT: usize = 256;
/// Replies waiting for the next tick; more are refused.
const REPLIES: usize = 64;
/// The longest reply, in characters, and the most of a webhook's answer read.
const MAX_REPLY: usize = 500;
const MAX_ANSWER: u64 = 64 << 10;
/// The longest a GET /api/chat/events may wait.
const MAX_WAIT: u64 = 60;
/// The webhook POST's signature header: "sha256=" and the body's HMAC-SHA256, hex.
const SIGNATURE: &str = "X-PCDS-Signature";

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Event {
    /// 1, 2, 3... since the server started: a gap is events dropped before they were read.
    pub id: u64,
    /// Unix time in milliseconds.
    pub time: u64,
    /// The connection's client id.
    pub client: u64,
    pub name: Option<String>,
    /// Verified (invite-code players) only, as a string: JSON numbers do not hold a SteamID64.
    pub steam_id: Option<String>,
    pub planet: String,
    pub text: String,
    /// With chat.commandPrefix "!": "!tp home" is command "tp" with args ["home"].
    pub command: Option<String>,
    pub args: Vec<String>,
}

struct Ring {
    events: VecDeque<Event>,
    next: u64,
}

/// What the game thread, the hooks' worker and the admin API's threads share.
pub struct Feed {
    ring: Mutex<Ring>,
    added: Condvar,
    /// Events the hooks never got: they were more than KEPT behind.
    dropped: AtomicU64,
    /// The rules' chat section, as the worker reads it for each event.
    config: Mutex<Chat>,
    replies: SyncSender<String>,
}

/// A lock whatever a panicking holder left: the game thread must not panic on it.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Feed {
    fn push(&self, mut event: Event) {
        let mut ring = lock(&self.ring);
        event.id = ring.next;
        ring.next += 1;
        if ring.events.len() == KEPT {
            ring.events.pop_front();
        }
        ring.events.push_back(event);
        self.added.notify_all();
    }

    /// The events after `last`, waiting up to `wait` for one, and the last event's id.
    fn after(&self, last: u64, wait: Duration) -> (Vec<Event>, u64) {
        let deadline = Instant::now() + wait;
        let mut ring = lock(&self.ring);
        while ring.next - 1 <= last {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            ring = self.added.wait_timeout(ring, left).unwrap_or_else(|e| e.into_inner()).0;
        }
        (ring.events.iter().filter(|e| e.id > last).cloned().collect(), ring.next - 1)
    }

    /// A message into the chat at the next tick; false when too many are waiting.
    pub fn say(&self, text: &str) -> bool {
        let text: String = text.trim().chars().take(MAX_REPLY).collect();
        text.is_empty() || self.replies.try_send(text).is_ok()
    }
}

pub struct State {
    pub feed: Arc<Feed>,
    replies: Receiver<String>,
    worker: bool,
}

impl Default for State {
    fn default() -> Self {
        let (tx, replies) = sync_channel(REPLIES);
        let ring = Mutex::new(Ring { events: VecDeque::with_capacity(KEPT), next: 1 });
        let feed = Feed { ring, added: Condvar::new(), dropped: AtomicU64::new(0), config: Mutex::new(Chat::default()), replies: tx };
        State { feed: Arc::new(feed), replies, worker: false }
    }
}

/// "!tp home" with prefix "!": ("tp", ["home"]). None: no prefix, or not a command.
fn command(prefix: Option<&str>, text: &str) -> Option<(String, Vec<String>)> {
    let rest = text.trim_start().strip_prefix(prefix.filter(|p| !p.is_empty())?)?;
    if rest.starts_with(char::is_whitespace) {
        return None;
    }
    let mut words = rest.split_whitespace();
    let name = words.next()?.to_string();
    Some((name, words.map(str::to_string).collect()))
}

/// ChatHandler.SendChatMessageServerRpc: the message as an event. Never waits on a hook.
pub fn chat(game: &mut Game, client: u64, text: &str) {
    let c = game.clients.get(&client);
    let (command, args) = command(game.rules.chat.command_prefix.as_deref(), text).unzip();
    let event = Event {
        id: 0,
        time: SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0),
        client,
        name: c.and_then(|c| c.ready.as_ref()).map(|s| s.name.clone()),
        steam_id: c.and_then(|c| c.verified_steam_id).map(|id| id.to_string()),
        planet: game.planet_id().to_string(),
        text: text.to_string(),
        command,
        args: args.unwrap_or_default(),
    };
    game.ext.chat.feed.push(event);
}

/// The replies that came in, to everyone, as the server's messages are.
pub fn tick(game: &mut Game) {
    while let Ok(text) = game.ext.chat.replies.try_recv() {
        game.net.rpc_scene(Target::All, "ChatHandler.ReceiveChatMessageClientRpc", &[json!(0), json!(text)]);
    }
}

/// The rules' chat section changed: the hooks use it from their next event on.
pub fn configure(game: &mut Game) {
    let chat = game.rules.chat.clone();
    let hooks = !chat.webhooks.is_empty() || !chat.exec.is_empty();
    *lock(&game.ext.chat.feed.config) = chat;
    if hooks && !game.ext.chat.worker {
        // The hooks get what comes from now on.
        let (feed, last) = (game.ext.chat.feed.clone(), lock(&game.ext.chat.feed.ring).next - 1);
        game.ext.chat.worker = std::thread::Builder::new().name("pcds-chathooks".into()).spawn(move || deliver(&feed, last)).is_ok();
    }
}

/// The admin API's chat endpoints, served on its own threads (a long poll must not hold the
/// game loop): GET /api/chat/events?after=ID&wait=SECONDS, POST /api/chat {"text": "..."}.
pub fn api(feed: &Feed, method: &str, path: &str, query: &str, body: &Value) -> (u16, String) {
    match (method, path) {
        ("GET", "/api/chat/events") => {
            let q = |k: &str| query.split('&').find_map(|kv| kv.strip_prefix(k)?.strip_prefix('=')?.parse::<u64>().ok());
            let (events, last) = feed.after(q("after").unwrap_or(0), Duration::from_secs(q("wait").unwrap_or(0).min(MAX_WAIT)));
            (200, json!({"events": events, "last": last, "dropped": feed.dropped.load(Ordering::Relaxed)}).to_string())
        }
        ("POST", "/api/chat") => match body["text"].as_str().filter(|t| !t.trim().is_empty()) {
            Some(text) if feed.say(text) => (200, "{\"ok\":true}".into()),
            Some(_) => (503, "{\"error\":\"too many chat messages waiting\"}".into()),
            None => (400, "{\"error\":\"'text' is required\"}".into()),
        },
        _ => (404, "{\"error\":\"unknown endpoint\"}".into()),
    }
}

/// The hooks' next events after `last`; those dropped before the hooks got them are counted.
fn next_for_hooks(feed: &Feed, last: u64, wait: Duration) -> Vec<Event> {
    let events = feed.after(last, wait).0;
    let missed = events.first().map_or(0, |e| e.id - last - 1);
    if missed > 0 {
        feed.dropped.fetch_add(missed, Ordering::Relaxed);
        eprintln!("chat hooks: {missed} events dropped, the hooks were {KEPT} behind");
    }
    events
}

/// The worker: every event to every webhook, then the exec program, one after the other.
fn deliver(feed: &Feed, mut last: u64) {
    let mut agent: Option<(f32, ureq::Agent)> = None;
    loop {
        for event in next_for_hooks(feed, last, Duration::from_secs(3600)) {
            last = event.id;
            let chat = lock(&feed.config).clone();
            let timeout = Duration::from_secs_f32(chat.timeout_seconds);
            let body = serde_json::to_string(&event).unwrap_or_default();
            if agent.as_ref().is_none_or(|(t, _)| *t != chat.timeout_seconds) {
                agent = Some((chat.timeout_seconds, http_agent(timeout)));
            }
            let http = &agent.as_ref().unwrap().1;
            for (i, url) in chat.webhooks.iter().enumerate() {
                // Never the URL in the log: it may hold a token (as Discord's do).
                match post(http, url, &body, chat.secret.as_deref(), chat.retries) {
                    Ok(Some(reply)) => {
                        feed.say(&reply);
                    }
                    Ok(None) => {}
                    Err(e) => eprintln!("chat hooks: webhook {} failed: {e}", i + 1),
                }
            }
            if !chat.exec.is_empty() {
                match exec(&chat.exec, &body, timeout) {
                    Ok(reply) => {
                        feed.say(&reply);
                    }
                    Err(e) => eprintln!("chat hooks: exec failed: {e}"),
                }
            }
        }
    }
}

/// Webhook targets are exactly what the admin set: no proxy from the environment, no redirects.
fn http_agent(timeout: Duration) -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .http_status_as_error(false)
        .max_redirects(0)
        .proxy(None)
        .user_agent(concat!("pcds/", env!("CARGO_PKG_VERSION")))
        .build();
    ureq::Agent::new_with_config(config)
}

/// HMAC-SHA256 of the body with the secret, hex.
fn sign(secret: &str, body: &str) -> String {
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, secret.as_bytes());
    ring::hmac::sign(&key, body.as_bytes()).as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

/// POSTs the event, tried again `retries` times (1 s, 2 s... apart) without an answer or on
/// 429 and 5xx. The answer's "reply", if it is JSON with one.
fn post(agent: &ureq::Agent, url: &str, body: &str, secret: Option<&str>, retries: u32) -> Result<Option<String>, String> {
    let mut attempt = 0;
    loop {
        let mut request = agent.post(url).header("Content-Type", "application/json");
        if let Some(secret) = secret {
            request = request.header(SIGNATURE, format!("sha256={}", sign(secret, body)));
        }
        let failed = match request.send(body) {
            Ok(mut answer) if answer.status().is_success() => {
                let text = answer.body_mut().with_config().limit(MAX_ANSWER).read_to_string().unwrap_or_default();
                let reply = serde_json::from_str::<Value>(&text).ok().and_then(|v| v["reply"].as_str().map(str::to_string));
                return Ok(reply);
            }
            Ok(answer) => {
                let status = answer.status().as_u16();
                if status != 429 && status < 500 {
                    return Err(format!("HTTP {status}"));
                }
                format!("HTTP {status}")
            }
            Err(ureq::Error::BadUri(_)) => return Err("bad URL".into()),
            Err(e) => e.to_string(),
        };
        if attempt >= retries {
            return Err(format!("{failed}, after {} tries", attempt + 1));
        }
        attempt += 1;
        std::thread::sleep(Duration::from_secs(attempt as u64));
    }
}

/// Runs the program (no shell) with the event's JSON and a newline on stdin; its output,
/// trimmed, is the reply. Killed past the timeout.
fn exec(argv: &[String], body: &str, timeout: Duration) -> Result<String, String> {
    let mut child = Command::new(&argv[0]).args(&argv[1..]).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().map_err(|e| e.to_string())?;
    let (pid, stdin, input) = (child.id(), child.stdin.take(), format!("{body}\n"));
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        // Closed after the write: the program sees the end of its input.
        if let Some(mut s) = stdin {
            let _ = s.write_all(input.as_bytes());
        }
        let _ = tx.send(child.wait_with_output());
    });
    match rx.recv_timeout(timeout) {
        Ok(Ok(out)) if out.status.success() => Ok(String::from_utf8_lossy(&out.stdout).into_owned()),
        Ok(Ok(out)) => Err(out.status.to_string()),
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => {
            // SAFETY: a plain signal to our own child, not yet reaped (its waiter has not returned).
            unsafe { libc::kill(pid as i32, libc::SIGKILL) };
            Err(format!("no exit within {timeout:?}: killed"))
        }
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::game::tests::{join, save};
    use crate::game::{test_game, Args};
    use std::io::{BufRead, BufReader, Read};
    use std::net::{TcpListener, TcpStream};

    fn send_chat(g: &mut Game, client: u64, text: &str) {
        assert!(crate::handlers::rpc(g, client, 0, 0, "ChatHandler.SendChatMessageServerRpc", &Args(vec![json!(text)])));
    }

    /// The chat messages queued for the clients: (from, text).
    fn chat_sent(g: &Game) -> Vec<(u64, String)> {
        g.net.out.iter().filter(|(_, kind, _)| *kind == crate::protocol::CLIENT_RPC).filter_map(|(_, _, body)| {
            let mut r = crate::wire::Reader::new(body);
            r.packed_u64()?;
            r.packed_u16()?;
            let method = r.packed_u32()?;
            (g.net.spec.get(method)?.name == "ChatHandler.ReceiveChatMessageClientRpc").then_some(())?;
            let args = g.net.spec.decode(method, r.rest())?;
            Some((args[0].as_u64()?, args[1].as_str()?.to_string()))
        }).collect()
    }

    /// Ticks until a chat message from the server is queued (5 s at most).
    fn wait_for_reply(g: &mut Game) -> Vec<String> {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            tick(g);
            let replies: Vec<String> = chat_sent(g).into_iter().filter(|(from, _)| *from == 0).map(|(_, t)| t).collect();
            if !replies.is_empty() {
                return replies;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        Vec::new()
    }

    /// One HTTP request read off a test listener: (headers, lowercase names; body).
    pub fn read_request(stream: &mut TcpStream) -> (Vec<(String, String)>, String) {
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let (mut headers, mut line) = (Vec::new(), String::new());
        reader.read_line(&mut line).unwrap();
        loop {
            line.clear();
            if reader.read_line(&mut line).unwrap() == 0 || line.trim().is_empty() {
                break;
            }
            if let Some((k, v)) = line.split_once(':') {
                headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
            }
        }
        let length = headers.iter().find(|(k, _)| k == "content-length").map_or(0, |(_, v)| v.parse().unwrap());
        let mut body = vec![0; length];
        reader.read_exact(&mut body).unwrap();
        (headers, String::from_utf8(body).unwrap())
    }

    fn answer(stream: &mut TcpStream, body: &str) {
        let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
    }

    /// As the admin API changes them: checked, then live.
    fn hooked(g: &mut Game, chat: Chat) {
        let rules = crate::rules::Rules { chat, ..g.rules.clone() };
        crate::rules::set(g, rules).unwrap();
    }

    #[test]
    fn a_chat_rpc_becomes_an_event_and_still_reaches_everyone() {
        let Some(mut g) = test_game(save()) else { return };
        join(&mut g, 1, "Ann");
        g.rules.chat.command_prefix = Some("!".into());
        g.net.out.clear();
        send_chat(&mut g, 1, "hello there");
        send_chat(&mut g, 1, "!tp home base");
        let (events, last) = g.ext.chat.feed.after(0, Duration::ZERO);
        assert_eq!(last, 2);
        let e = &events[0];
        assert_eq!((e.id, e.client, e.name.as_deref(), e.steam_id.as_deref(), e.planet.as_str(), e.text.as_str()), (1, 1, Some("Ann"), None, "Prime", "hello there"));
        assert!(e.command.is_none() && e.time > 1_700_000_000_000);
        assert_eq!((events[1].command.as_deref(), events[1].args.clone()), (Some("tp"), vec!["home".to_string(), "base".into()]));
        let json = serde_json::to_value(e).unwrap();
        assert_eq!(json.as_object().unwrap().keys().collect::<Vec<_>>(), ["id", "time", "client", "name", "steamId", "planet", "text", "command", "args"]);
        assert_eq!(chat_sent(&g), [(1, "hello there".to_string()), (1, "!tp home base".into())], "the game's broadcast is unchanged");
    }

    #[test]
    fn command_prefixes_parse() {
        let words = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(command(Some("!"), "!tp  home  base "), Some(("tp".into(), words(&["home", "base"]))));
        assert_eq!(command(Some("!"), "  !help"), Some(("help".into(), vec![])));
        assert_eq!(command(Some("//"), "//kit iron"), Some(("kit".into(), words(&["iron"]))));
        for (prefix, text) in [(Some("!"), "hello !tp"), (Some("!"), "!"), (Some("!"), "! tp"), (None, "!tp"), (Some(""), "tp")] {
            assert_eq!(command(prefix, text), None, "{prefix:?} {text:?}");
        }
    }

    #[test]
    fn a_full_ring_drops_the_oldest_and_counts_them() {
        let state = State::default();
        let event = |n: usize| Event { id: 0, time: 0, client: 1, name: None, steam_id: None, planet: "Prime".into(), text: n.to_string(), command: None, args: vec![] };
        (1..=KEPT + 10).for_each(|n| state.feed.push(event(n)));
        let events = next_for_hooks(&state.feed, 0, Duration::ZERO);
        assert_eq!((events.len(), events[0].id, events[0].text.as_str()), (KEPT, 11, "11"));
        assert_eq!(state.feed.dropped.load(Ordering::Relaxed), 10);
        assert!(next_for_hooks(&state.feed, (KEPT + 10) as u64, Duration::ZERO).is_empty());
        assert_eq!(state.feed.dropped.load(Ordering::Relaxed), 10, "nothing missed when up to date");
    }

    #[test]
    fn hmac_matches_rfc_4231() {
        assert_eq!(sign("Jefe", "what do ya want for nothing?"), "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843");
    }

    #[test]
    fn a_webhook_gets_the_event_signed_and_its_reply_reaches_the_chat() {
        let Some(mut g) = test_game(save()) else { return };
        join(&mut g, 1, "Ann");
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/hook", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request(&mut stream);
            answer(&mut stream, "{\"reply\":\"pong\"}");
            request
        });
        hooked(&mut g, Chat { webhooks: vec![url], secret: Some("s3cret".into()), ..Chat::default() });
        g.net.out.clear();
        send_chat(&mut g, 1, "ping");
        assert_eq!(wait_for_reply(&mut g), ["pong"]);
        let (headers, body) = server.join().unwrap();
        let header = |k: &str| headers.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, b"s3cret");
        let tag = header("x-pcds-signature").and_then(|s| s.strip_prefix("sha256=").map(str::to_string)).unwrap();
        let tag: Vec<u8> = (0..tag.len() / 2).map(|i| u8::from_str_radix(&tag[2 * i..2 * i + 2], 16).unwrap()).collect();
        assert!(ring::hmac::verify(&key, body.as_bytes(), &tag).is_ok(), "the signature verifies");
        assert_eq!(header("content-type").as_deref(), Some("application/json"));
        let event: Value = serde_json::from_str(&body).unwrap();
        assert_eq!((event["id"].as_u64(), event["name"].as_str(), event["text"].as_str()), (Some(1), Some("Ann"), Some("ping")));
    }

    #[test]
    fn the_tick_is_not_delayed_by_a_slow_webhook() {
        let Some(mut g) = test_game(save()) else { return };
        join(&mut g, 1, "Ann");
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let (got, request) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                read_request(&mut stream);
                let _ = got.send(());
                std::thread::sleep(Duration::from_secs(3));
                answer(&mut stream, "{}");
            }
        });
        hooked(&mut g, Chat { webhooks: vec![url], timeout_seconds: 10.0, ..Chat::default() });
        send_chat(&mut g, 1, "first");
        request.recv_timeout(Duration::from_secs(5)).expect("the webhook is called");
        // The worker now waits on the listener's 3 s: chat and ticks go on regardless.
        let start = Instant::now();
        for n in 0..20 {
            send_chat(&mut g, 1, &format!("message {n}"));
            g.tick();
        }
        assert!(start.elapsed() < Duration::from_millis(1000), "{:?} for 20 messages and ticks", start.elapsed());
        assert_eq!(g.ext.chat.feed.after(0, Duration::ZERO).1, 21);
    }

    #[test]
    fn the_exec_hook_gets_the_event_as_json_on_stdin_and_its_output_is_the_reply() {
        let Some(mut g) = test_game(save()) else { return };
        join(&mut g, 1, "Ann");
        let marker = std::env::temp_dir().join(format!("pcds-chathooks-test-{}", std::process::id()));
        let text = format!("$(touch {}); `touch {}`", marker.display(), marker.display());
        hooked(&mut g, Chat { exec: vec!["cat".into()], ..Chat::default() });
        g.net.out.clear();
        send_chat(&mut g, 1, &text);
        let replies = wait_for_reply(&mut g);
        let echoed: Value = serde_json::from_str(&replies[0]).unwrap();
        assert_eq!(echoed["text"].as_str(), Some(text.as_str()));
        assert!(!marker.exists(), "chat text is never run");
        // Past the timeout the program is killed.
        assert!(exec(&["sleep".into(), "5".into()], "{}", Duration::from_millis(200)).unwrap_err().contains("killed"));
    }

    fn ask(addr: std::net::SocketAddr, request: &str) -> Value {
        let mut s = TcpStream::connect(addr).unwrap();
        s.write_all(request.as_bytes()).unwrap();
        let mut answer = String::new();
        s.read_to_string(&mut answer).unwrap();
        serde_json::from_str(answer.split_once("\r\n\r\n").unwrap().1).unwrap()
    }

    #[test]
    fn the_admin_api_long_polls_events_and_says_into_the_chat() {
        let Some(mut g) = test_game(save()) else { return };
        join(&mut g, 1, "Ann");
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let _rx = crate::admin::listen(listener, "t".into(), None, g.ext.chat.feed.clone()).unwrap();
        let poll = std::thread::spawn(move || {
            let start = Instant::now();
            (ask(addr, "GET /api/chat/events?after=0&wait=20 HTTP/1.1\r\nAuthorization: Bearer t\r\n\r\n"), start.elapsed())
        });
        std::thread::sleep(Duration::from_millis(200));
        send_chat(&mut g, 1, "anyone there?");
        let (answer, waited) = poll.join().unwrap();
        assert!(waited < Duration::from_secs(10), "answered when the event came, not at the end of the wait: {waited:?}");
        assert_eq!((answer["events"][0]["text"].as_str(), answer["last"].as_u64(), answer["dropped"].as_u64()), (Some("anyone there?"), Some(1), Some(0)));
        let none = ask(addr, "GET /api/chat/events?after=1 HTTP/1.1\r\nAuthorization: Bearer t\r\n\r\n");
        assert_eq!(none["events"], json!([]));

        let say = "{\"text\":\"Server restarts in 5 minutes\"}";
        let ok = ask(addr, &format!("POST /api/chat HTTP/1.1\r\nAuthorization: Bearer t\r\nContent-Length: {}\r\n\r\n{say}", say.len()));
        assert_eq!(ok["ok"], json!(true));
        g.net.out.clear();
        tick(&mut g);
        assert_eq!(chat_sent(&g), [(0, "Server restarts in 5 minutes".to_string())]);
        assert!(g.ext.chat.feed.after(1, Duration::ZERO).0.is_empty(), "the server's own messages are not events");
    }
}
