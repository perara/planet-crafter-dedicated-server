//! The admin API and the web page over HTTP on 127.0.0.1. The routes, and who answers each, are the
//! table in front/admin_routes.rs (docs/admin-api.md is written from it). A thread in the front
//! serves HTTP: it answers the page, the logs and the chat itself (a long poll must not hold the
//! game loop), and passes the rest to the front's loop, which answers it or passes it to the
//! bank. Every /api call needs "Authorization: Bearer <token>".

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::front::{Feed, Http, Owner};
use crate::tokens::{Checker, Denied};

/// Connections served at once (more are closed unanswered), the bytes of one request (line,
/// headers and body), and how long it may take to arrive: the line and headers within HEAD (an
/// idle connection gives its slot back soon), the whole request within DEADLINE. Chat long
/// polls have a cap of their own (front::chat_hooks MAX_POLLS), so they never take every slot.
const MAX_CONNECTIONS: usize = 64;
/// Of those 64, connections from other machines get at most this many in all, and this many from one
/// address: a client that opens connections and says nothing holds them for HEAD each, and one
/// address (or a few) can no longer hold every slot, so the command line and a proxy on this machine
/// (the loopback) always find one. Over a limit the answer is 503 with Retry-After, at once.
const MAX_REMOTE: usize = 48;
const MAX_PER_ADDRESS: usize = 16;
const MAX_REQUEST: u64 = 64 << 10;
const HEAD: Duration = Duration::from_secs(2);
const DEADLINE: Duration = Duration::from_secs(10);
/// How long the game loop may take to answer: 10 s, but the world controls stop and start the
/// bank inside that loop (a save and a stop of up to 60 s, a load of up to 120 s: front/mod.rs
/// STOP_GRACE and LOAD_DEADLINE), so a switch is answered up to 200 s after, not as failed at 10.
const ANSWER: Duration = Duration::from_secs(10);
const WORLD_ANSWER: Duration = Duration::from_secs(200);
/// The health route: a loop that takes longer than this to say so is not healthy.
const HEALTH_ANSWER: Duration = Duration::from_secs(3);

/// The page's Content-Security-Policy (see `respond_with`).
const PAGE_POLICY: &str = "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; connect-src 'self'; img-src 'self' data: blob:; base-uri 'none'; frame-ancestors 'none'";

pub struct Request {
    pub method: String,
    pub path: String,
    pub body: Value,
    /// (status, JSON body)
    pub reply: Sender<(u16, String)>,
}

/// The longest a POST /api/plugins/events may wait for an event (`"wait"`, in seconds), and how many
/// wait at once (more are answered 503 at once: a wait holds a thread and a connection).
const MAX_STREAM_WAIT: f64 = 25.0;
const MAX_STREAM_POLLS: usize = 8;
/// Without the bank's doorbell (an older bank) a waiting poll asks the bank again this often.
const LOOK_EVERY: Duration = Duration::from_secs(1);
static STREAM_POLLS: AtomicUsize = AtomicUsize::new(0);

/// The plugin event stream's doorbell: the bank rings it (ipc::STREAM) when the stream has a newer
/// event, and the polls waiting here ask the bank again. Only a count of rings is kept; the bank's
/// ring of events is where they are.
static BELL: (Mutex<u64>, Condvar) = (Mutex::new(0), Condvar::new());

/// The bank rang: wake the polls that wait.
pub fn ring_bell() {
    *BELL.0.lock().unwrap_or_else(|e| e.into_inner()) += 1;
    BELL.1.notify_all();
}

fn rings() -> u64 {
    *BELL.0.lock().unwrap_or_else(|e| e.into_inner())
}

/// Waits until the bell has rung since `seen` (a count from `rings`), or `up_to` is over.
fn wait_for_bell(seen: u64, up_to: Duration) {
    let guard = BELL.0.lock().unwrap_or_else(|e| e.into_inner());
    drop(BELL.1.wait_timeout_while(guard, up_to, |n| *n == seen));
}

/// POST /api/plugins/events with `"wait"`: the answer of the bank as soon as it holds an event, or
/// empty when the wait is over. The ring and the consumers' registration are the bank's; here is only
/// the waiting, which the bank's game loop must not do.
fn long_poll(stream: TcpStream, tx: &Sender<Request>, path: String, body: Value, wait: f64) -> std::io::Result<()> {
    struct Poll;
    impl Drop for Poll {
        fn drop(&mut self) {
            STREAM_POLLS.fetch_sub(1, Ordering::SeqCst);
        }
    }
    if STREAM_POLLS.fetch_add(1, Ordering::SeqCst) >= MAX_STREAM_POLLS {
        STREAM_POLLS.fetch_sub(1, Ordering::SeqCst);
        return respond(stream, 503, "application/json", &serde_json::json!({"error": format!("{MAX_STREAM_POLLS} polls of the stream are already waiting: try again, or poll without wait")}).to_string());
    }
    let _poll = Poll;
    let deadline = Instant::now() + Duration::from_secs_f64(wait.min(MAX_STREAM_WAIT));
    loop {
        // The count before the question: a ring while the bank answers is not missed.
        let seen = rings();
        let (reply, answer) = channel();
        if tx.send(Request { method: "POST".into(), path: path.clone(), body: body.clone(), reply }).is_err() {
            return respond(stream, 500, "application/json", "{\"error\":\"server stopping\"}");
        }
        let (status, json) = match answer.recv_timeout(ANSWER) {
            Ok(a) => a,
            Err(_) => return respond(stream, 500, "application/json", "{\"error\":\"the game loop did not answer\"}"),
        };
        let left = deadline.saturating_duration_since(Instant::now());
        let empty = status == 200 && serde_json::from_str::<Value>(&json).is_ok_and(|v| v["events"].as_array().is_some_and(Vec::is_empty));
        if !empty || left.is_zero() {
            return respond(stream, status, "application/json", &json);
        }
        wait_for_bell(seen, left.min(LOOK_EVERY));
    }
}

/// The web page's file: the live bank's, so a flip brings its page along.
pub type Web = Arc<RwLock<Option<String>>>;

/// Starts the admin API; scoped tokens (tokens.rs) may make the requests of their scopes.
pub fn start_with(addr: &str, token: String, web_index: Web, chat: Arc<Feed>, tokens: Arc<Checker>) -> std::io::Result<Receiver<Request>> {
    listen_with(TcpListener::bind(addr)?, token, web_index, chat, tokens)
}

#[cfg(test)]
pub fn listen(listener: TcpListener, token: String, web_index: Web, chat: Arc<Feed>) -> std::io::Result<Receiver<Request>> {
    listen_with(listener, token, web_index, chat, Arc::new(Checker::none()))
}

pub fn listen_with(listener: TcpListener, token: String, web_index: Web, chat: Arc<Feed>, tokens: Arc<Checker>) -> std::io::Result<Receiver<Request>> {
    let (tx, rx) = channel();
    let slots = Arc::new(Slots::default());
    std::thread::Builder::new().name("pcds-admin".into()).spawn(move || {
        let mut accepting = Accepting::default();
        loop {
            let stream = match accepting.step(listener.accept().map(|(stream, _)| stream)) {
                Step::Serve(stream) => stream,
                Step::Pause(time) => {
                    std::thread::sleep(time);
                    continue;
                }
            };
            let from = stream.peer_addr().map(|a| a.ip().to_canonical()).unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
            // Only this thread adds, so the counts cannot pass the caps.
            let slot = match slots.admit(from) {
                Ok(slot) => slot,
                Err(Refused::Full) => continue,
                Err(Refused::Busy) => {
                    // Told at once, on this thread: the answer is a few bytes into an empty send buffer.
                    let _ = stream.set_write_timeout(Some(Duration::from_millis(200)));
                    let _ = respond_with(stream, 503, "application/json", "{\"error\":\"too many connections from this address; try again in a few seconds\"}", "Retry-After: 5\r\n");
                    continue;
                }
            };
            let (tx, token, web, chat, tokens) = (tx.clone(), token.clone(), web_index.clone(), chat.clone(), tokens.clone());
            let spawned = std::thread::Builder::new().name("pcds-admin-conn".into()).spawn(move || {
                let _slot = slot;
                let page = web.read().unwrap_or_else(|e| e.into_inner()).clone();
                let _ = serve(stream, &tx, &token, page.as_deref(), &chat, &tokens);
            });
            // No thread for it (the process is at its limit): the connection is closed (the
            // closure, with its slot and stream, is dropped with the error), and the acceptor goes on.
            if let Err(e) = spawned {
                warn!("admin: no thread for a connection ({e}); it was closed, the listener goes on");
                std::thread::sleep(accepting.pause_after_failure());
            }
        }
    })?;
    Ok(rx)
}

/// What the acceptor does with one result of accept().
enum Step {
    Serve(TcpStream),
    Pause(Duration),
}

/// The acceptor's state: the accept errors in a row. An error that does not clear (no file
/// descriptors left: EMFILE, ENFILE; no memory) made the old loop spin at 100% of a core, and
/// logged nothing.
#[derive(Default)]
struct Accepting {
    failures: u32,
}

impl Accepting {
    fn step(&mut self, accepted: std::io::Result<TcpStream>) -> Step {
        match accepted {
            Ok(stream) => {
                self.failures = 0;
                Step::Serve(stream)
            }
            Err(e) => {
                self.failures = self.failures.saturating_add(1);
                // The first, and then one in a while: a minute of a stuck listener is a few lines.
                if self.failures == 1 || self.failures.is_multiple_of(60) {
                    error!("admin: accept failed ({e}), {} in a row; trying again in {:?} (connections wait or are refused until it clears)", self.failures, Self::backoff(self.failures));
                }
                Step::Pause(Self::backoff(self.failures))
            }
        }
    }

    fn pause_after_failure(&mut self) -> Duration {
        self.failures = self.failures.saturating_add(1);
        Self::backoff(self.failures)
    }

    /// 10 ms, doubled for each error in a row, up to a second.
    fn backoff(failures: u32) -> Duration {
        Duration::from_millis(10u64 << failures.saturating_sub(1).min(7)).min(Duration::from_secs(1))
    }
}

/// The connections being served, in all and by where they come from.
#[derive(Default)]
struct Slots {
    open: AtomicUsize,
    /// From other machines.
    remote: AtomicUsize,
    per_address: Mutex<std::collections::HashMap<std::net::IpAddr, usize>>,
}

/// Why a connection is not served.
#[derive(Debug, PartialEq)]
enum Refused {
    /// Every slot is taken: closed unanswered.
    Full,
    /// This address (or the other machines together) has its share: answered 503.
    Busy,
}

/// One connection being served; freed when its thread ends, panic or not.
struct Slot {
    slots: Arc<Slots>,
    from: std::net::IpAddr,
}

impl Slots {
    /// A slot for a connection from `from`: the loopback (this machine: the command line, a proxy)
    /// has all of them; the rest share MAX_REMOTE, and one address has MAX_PER_ADDRESS.
    fn admit(self: &Arc<Self>, from: std::net::IpAddr) -> Result<Slot, Refused> {
        if self.open.load(Ordering::SeqCst) >= MAX_CONNECTIONS {
            return Err(Refused::Full);
        }
        if !from.is_loopback() {
            let mut per = self.per_address.lock().unwrap_or_else(|e| e.into_inner());
            let mine = per.entry(from).or_insert(0);
            if *mine >= MAX_PER_ADDRESS || self.remote.load(Ordering::SeqCst) >= MAX_REMOTE {
                if *mine == 0 {
                    per.remove(&from);
                }
                return Err(Refused::Busy);
            }
            *mine += 1;
            self.remote.fetch_add(1, Ordering::SeqCst);
        }
        self.open.fetch_add(1, Ordering::SeqCst);
        Ok(Slot { slots: self.clone(), from })
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.slots.open.fetch_sub(1, Ordering::SeqCst);
        if !self.from.is_loopback() {
            self.slots.remote.fetch_sub(1, Ordering::SeqCst);
            let mut per = self.slots.per_address.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(n) = per.get_mut(&self.from) {
                *n -= 1;
                if *n == 0 {
                    per.remove(&self.from);
                }
            }
        }
    }
}

/// Reads that all end by one deadline, however slowly the bytes trickle in.
struct Deadline(TcpStream, Instant);

impl Read for Deadline {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let left = self.1.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(std::io::ErrorKind::TimedOut.into());
        }
        self.0.set_read_timeout(Some(left))?;
        self.0.read(buf)
    }
}

/// How long a request's answer is waited for.
fn answer_wait(path: &str) -> Duration {
    if path.starts_with("/api/world/") {
        WORLD_ANSWER
    } else if path == "/api/health" {
        HEALTH_ANSWER
    } else {
        ANSWER
    }
}

/// What an answer is: the metrics are text, everything else JSON.
fn content_type(path: &str, status: u16) -> &'static str {
    if path == "/api/metrics" && status == 200 { "text/plain; version=0.0.4; charset=utf-8" } else { "application/json" }
}

/// Equal strings, in time that does not depend on where they differ.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0, |d, (x, y)| d | (x ^ y)) == 0
}

fn respond(stream: TcpStream, status: u16, content_type: &str, body: &str) -> std::io::Result<()> {
    respond_with(stream, status, content_type, body, "")
}

/// `extra`: more header lines, each ending in CRLF.
fn respond_with(mut stream: TcpStream, status: u16, content_type: &str, body: &str, extra: &str) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        411 => "Length Required",
        413 => "Content Too Large",
        429 => "Too Many Requests",
        503 => "Service Unavailable",
        _ => "Error",
    };
    // The page holds the token once logged in: no other site may frame it (clickjacking). The page
    // itself may talk to this server and nobody else (a script that got in could not send the token
    // anywhere), load nothing from elsewhere and take no <base>; its script and style are inline.
    let policy = if content_type.starts_with("text/html") { PAGE_POLICY } else { "frame-ancestors 'none'" };
    let frames = format!("X-Frame-Options: DENY\r\nContent-Security-Policy: {policy}\r\nX-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\nCache-Control: no-store\r\n");
    write!(stream, "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n{extra}{frames}Connection: close\r\n\r\n{body}", body.len())
}

fn serve(stream: TcpStream, tx: &Sender<Request>, token: &str, web: Option<&str>, chat: &Feed, tokens: &Checker) -> std::io::Result<()> {
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let started = Instant::now();
    let mut reader = BufReader::new(Deadline(stream.try_clone()?, started + HEAD)).take(MAX_REQUEST);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("");
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let (path, query) = (path.to_string(), query.to_string());
    let (mut auth, mut length, mut chunked) = (None::<String>, None::<Option<usize>>, false);
    loop {
        let mut header = String::new();
        // The end of the head is an empty line (CRLF, or LF alone): one with only blanks on it is a folded line
        // (a proxy may read the headers after it as part of the one before), refused below.
        if reader.read_line(&mut header)? == 0 || header == "\r\n" || header == "\n" {
            break;
        }
        // The head is read as one reader reads it or not at all: what a proxy in front may read
        // another way (a line folded onto the one before, a name with a space, two lengths, two
        // tokens) is refused, so no request means one thing here and another there.
        let bad = |what: &str| respond(stream.try_clone()?, 400, "application/json", &serde_json::json!({ "error": what }).to_string());
        if !header.ends_with('\n') {
            return respond(stream, 431, "application/json", "{\"error\":\"the head of a request is at most 64 KiB\"}");
        }
        if header.starts_with([' ', '\t']) || header.trim().is_empty() {
            return bad("a header folded over lines");
        }
        let Some((k, v)) = header.split_once(':') else { return bad("a header line without a colon") };
        if k.is_empty() || !k.bytes().all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)) {
            return bad("a header name is letters, digits and the usual signs, with no space before the colon");
        }
        match k.to_ascii_lowercase().as_str() {
            "authorization" if auth.replace(v.trim().to_string()).is_some() => return bad("two Authorization headers"),
            // Digits only: usize::from_str also takes "+5", which a proxy reads as no length at all.
            "content-length" if length.replace(Some(v.trim()).filter(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())).and_then(|n| n.parse().ok())).is_some() => return bad("two Content-Length headers"),
            "transfer-encoding" => chunked = true,
            _ => {}
        }
    }
    let (auth, length) = (auth.unwrap_or_default(), length.unwrap_or(Some(0)));
    reader.get_mut().get_mut().1 = started + DEADLINE;
    // A chunked body would read as empty, and {} answered as done: refused instead.
    if chunked {
        return respond(stream, 411, "application/json", "{\"error\":\"send the body with a Content-Length\"}");
    }
    let Some(length) = length else { return respond(stream, 400, "application/json", "{\"error\":\"bad Content-Length\"}") };
    // What the head left of MAX_REQUEST: a body that fits the limit alone but not after its head
    // would end in the read below, and the connection would close unanswered.
    if length as u64 > reader.limit() {
        let answered = respond(stream.try_clone()?, 413, "application/json", &format!("{{\"error\":\"a request (head and body) is at most {} KiB\"}}", MAX_REQUEST >> 10));
        // A lingering close: read (up to 1 MiB, 2 s) what the client still sends, so the kernel
        // does not reset the connection over unread bytes before the answer is read.
        let _ = stream.shutdown(std::net::Shutdown::Write);
        let _ = std::io::copy(&mut Deadline(stream, Instant::now() + Duration::from_secs(2)).take(1 << 20), &mut std::io::sink());
        return answered;
    }
    // Past MAX_REQUEST the reader ends: headers and body together over it fail here.
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body)?;
    // The routes this thread answers itself are in front/admin_routes.rs.
    let route = crate::front::find_http(&method, &path);
    // The page needs no token (it asks for it), nor does the health route; every other route does.
    if route.is_some_and(|r| r.handler.owner == Owner::Http(Http::Page)) {
        return match web.and_then(|p| std::fs::read_to_string(p).ok()) {
            Some(html) => respond(stream, 200, "text/html; charset=utf-8", &html),
            None => respond(stream, 404, "text/plain", "no web GUI installed"),
        };
    }
    if !crate::front::is_open(&method, &path) && !same(&auth, &format!("Bearer {token}")) {
        // Not the admin token: a scoped token may make the requests of its scopes (tokens.rs).
        match tokens.authorize(&auth, &method, &path) {
            Ok(_) => {}
            Err(Denied::Unknown) => return respond(stream, 401, "application/json", "{\"error\":\"missing or wrong token\"}"),
            Err(Denied::Scope(needs)) => return respond(stream, 403, "application/json", &serde_json::json!({"error": "this token may not make this request", "needs": needs}).to_string()),
            Err(Denied::Rate(wait)) => return respond_with(stream, 429, "application/json", &serde_json::json!({"error": "this token is over its rate; try again later", "retryAfterSeconds": wait}).to_string(), &format!("Retry-After: {wait}\r\n")),
        }
    }
    if route.map(|r| r.handler.owner) == Some(Owner::Http(Http::Logs)) {
        return respond(stream, 200, "application/json", &logs().to_string());
    }
    let body: Value = if body.iter().all(u8::is_ascii_whitespace) { Value::Object(Default::default()) } else {
        match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(e) => return respond(stream, 400, "application/json", &serde_json::json!({ "error": e.to_string() }).to_string()),
        }
    };
    if route.map(|r| r.handler.owner) == Some(Owner::Http(Http::Chat)) {
        let (status, json) = crate::front::chat_api(chat, &method, &path, &query, &body);
        return respond(stream, status, "application/json", &json);
    }
    if method == "POST" && path == "/api/plugins/events" && body["wait"].as_f64().is_some_and(|w| w > 0.0) {
        let wait = body["wait"].as_f64().unwrap_or(0.0);
        return long_poll(stream, tx, path, body, wait);
    }
    let (reply, answer) = channel();
    let (wait, asked) = (answer_wait(&path), path.clone());
    if tx.send(Request { method, path, body, reply }).is_err() {
        return respond(stream, 500, "application/json", "{\"error\":\"server stopping\"}");
    }
    match answer.recv_timeout(wait) {
        Ok((status, json)) => respond(stream, status, content_type(&asked, status), &json),
        Err(_) if asked == "/api/health" => respond(stream, 503, "application/json", "{\"ok\":false}"),
        Err(_) => respond(stream, 500, "application/json", "{\"error\":\"the game loop did not answer\"}"),
    }
}

/// How long journalctl may take: a journal that is huge or stuck must not keep a connection (and
/// with enough of them every slot) for as long as it likes; the page asks again every 5 s.
const LOGS_WAIT: Duration = Duration::from_secs(6);

/// The server's recent output, from the journal of the systemd user unit pcds runs it as
/// (PCDS_UNIT). Read on the HTTP thread: journalctl takes longer than a game tick.
fn logs() -> Value {
    let Some(unit) = std::env::var("PCDS_UNIT").ok().filter(|u| !u.is_empty()) else {
        return serde_json::json!({ "lines": [], "note": "Not running as a pcds systemd unit: the log is the server's own output (docker logs, the terminal)." });
    };
    journal("journalctl", &unit, LOGS_WAIT)
}

fn journal(program: &str, unit: &str, wait: Duration) -> Value {
    let unit = format!("{unit}.service");
    let spawned = std::process::Command::new(program)
        .args(["--user", "-u", &unit, "-n", "300", "-o", "short-iso", "--no-pager"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn();
    let mut child = match spawned {
        Ok(child) => child,
        Err(e) => return serde_json::json!({ "lines": [], "note": format!("journalctl: {e}") }),
    };
    // Read as it writes (a full pipe would stop it), while this thread watches the clock.
    let reader = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut bytes);
            }
            bytes
        })
    };
    let (out, err) = (reader(child.stdout.take().map(|p| Box::new(p) as Box<dyn Read + Send>)), reader(child.stderr.take().map(|p| Box::new(p) as Box<dyn Read + Send>)));
    let deadline = Instant::now() + wait;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return serde_json::json!({ "lines": [], "note": format!("journalctl gave no answer within {} s (a very large or stuck journal): run `journalctl --user -u {unit}` on the server", wait.as_secs()) });
            }
        }
    };
    let (out, err) = (out.join().unwrap_or_default(), err.join().unwrap_or_default());
    if status.success() {
        serde_json::json!({ "lines": String::from_utf8_lossy(&out).lines().collect::<Vec<_>>(), "source": format!("journalctl --user -u {unit}") })
    } else {
        serde_json::json!({ "lines": [], "note": String::from_utf8_lossy(&err).trim() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ask(addr: std::net::SocketAddr, request: &[u8]) -> String {
        let mut s = TcpStream::connect(addr).unwrap();
        let _ = s.write_all(request);
        let mut answer = String::new();
        let _ = s.read_to_string(&mut answer);
        answer
    }

    /// An accept that keeps failing (EMFILE) is waited out with a growing pause, not spun on, and
    /// the first good connection after it is served as ever.
    #[test]
    fn an_acceptor_that_keeps_failing_backs_off_and_recovers() {
        let mut accepting = Accepting::default();
        let emfile = || Err::<TcpStream, _>(std::io::Error::from_raw_os_error(libc::EMFILE));
        let pauses: Vec<Duration> = (0..12)
            .map(|_| match accepting.step(emfile()) {
                Step::Pause(p) => p,
                Step::Serve(_) => panic!("an error is not a connection"),
            })
            .collect();
        let ms: Vec<u128> = pauses.iter().map(|p| p.as_millis()).collect();
        assert_eq!(ms, [10, 20, 40, 80, 160, 320, 640, 1000, 1000, 1000, 1000, 1000]);
        // A listener stuck for a minute is asked about once a second, not a million times.
        assert!(pauses.iter().sum::<Duration>() > Duration::from_secs(5));
        // A connection that comes ends the run of errors.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (stream, _) = listener.accept().unwrap();
        assert!(matches!(accepting.step(Ok(stream)), Step::Serve(_)));
        assert!(matches!(accepting.step(emfile()), Step::Pause(p) if p == Duration::from_millis(10)));
        // A thread that could not be made is one more failure in the run.
        assert_eq!(accepting.pause_after_failure(), Duration::from_millis(20));
        drop(client);
    }

    /// Stopping, starting and switching a world are answered when done, not failed at 10 s.
    #[test]
    fn world_controls_are_waited_for_as_long_as_they_may_take() {
        let longest = crate::front::STOP_GRACE * 2 + crate::front::LOAD_DEADLINE;
        for path in ["/api/world/stop", "/api/world/start", "/api/world/switch"] {
            assert!(answer_wait(path) >= longest, "{path}");
        }
        for path in ["/api/status", "/api/worlds", "/api/worlds/file", "/api/save"] {
            assert_eq!(answer_wait(path), ANSWER, "{path}");
        }
    }

    /// A journalctl that never answers is killed at the limit, with a note that says what to do.
    #[test]
    fn a_stuck_journal_does_not_hold_the_connection() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("pcds-journal-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let program = dir.join("journalctl");
        std::fs::write(&program, "#!/bin/sh\nsleep 20\n").unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        let started = Instant::now();
        let answer = journal(program.to_str().unwrap(), "pcds", Duration::from_millis(300));
        assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
        assert!(answer["note"].as_str().unwrap().contains("no answer within"), "{answer}");
        // One that answers is read whole.
        std::fs::write(&program, "#!/bin/sh\necho one; echo two\n").unwrap();
        let answer = journal(program.to_str().unwrap(), "pcds", Duration::from_secs(5));
        assert_eq!(answer["lines"], serde_json::json!(["one", "two"]));
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A scoped token opens the requests of its scopes and nothing else; a revoked or unknown one,
    /// nothing; one over its rate waits; the answers never carry a token.
    #[test]
    fn a_scoped_token_makes_the_requests_of_its_scopes_and_no_others() {
        let dir = std::env::temp_dir().join(format!("pcds-admin-tokens-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (_, relay) = crate::tokens::create(&dir, "relay", &["events.read".to_string(), "chat.send".to_string()], Some(5)).unwrap();
        let (_, quick) = crate::tokens::create(&dir, "quick", &["chat.send".to_string()], Some(2)).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let hooks = crate::front::Hooks::default();
        let rx = listen_with(listener, "admin-secret".into(), Default::default(), hooks.feed.clone(), Arc::new(Checker::new(dir.clone()))).unwrap();
        let req = |token: &str, method: &str, path: &str| format!("{method} {path} HTTP/1.1\r\nAuthorization: Bearer {token}\r\nContent-Length: 13\r\n\r\n{{\"text\":\"hi\"}}");
        // The stream endpoint reaches the game loop, as with the admin token.
        let client = std::thread::spawn({
            let r = req(&relay, "POST", "/api/plugins/events");
            move || ask(addr, r.as_bytes())
        });
        let request = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!((request.method.as_str(), request.path.as_str()), ("POST", "/api/plugins/events"));
        request.reply.send((200, "{\"events\":[]}".into())).unwrap();
        assert!(client.join().unwrap().starts_with("HTTP/1.1 200"));
        // A scope it does not have, an endpoint no scope covers, the endpoints that manage tokens:
        // refused before the game loop hears of them.
        for (method, path) in [("POST", "/api/kick"), ("GET", "/api/status"), ("POST", "/api/stop"), ("GET", "/api/logs"), ("POST", "/api/rules"), ("GET", "/api/plugins/tokens"), ("POST", "/api/plugins/tokens"), ("POST", "/api/plugins/tokens/revoke"), ("POST", "/api/plugins/enable"), ("POST", "/api/plugins/trust")] {
            let answer = ask(addr, req(&relay, method, path).as_bytes());
            assert!(answer.starts_with("HTTP/1.1 403"), "{method} {path}: {answer}");
            assert!(!answer.contains(&relay), "a token is never echoed");
        }
        assert!(ask(addr, req(&relay, "POST", "/api/kick").as_bytes()).contains("player.kick"), "the answer says which scope is missing");
        assert!(rx.try_recv().is_err());
        // The chat endpoints are answered in the front, with the scope checked first.
        assert!(ask(addr, req(&relay, "POST", "/api/chat").as_bytes()).starts_with("HTTP/1.1 200"), "chat.send says something");
        assert!(ask(addr, req(&relay, "GET", "/api/chat/events").as_bytes()).starts_with("HTTP/1.1 403"));
        // Unknown tokens and the admin token pass the old way.
        assert!(ask(addr, req("pcdst_00", "POST", "/api/chat").as_bytes()).starts_with("HTTP/1.1 401"));
        assert!(ask(addr, req("nothing", "POST", "/api/chat").as_bytes()).starts_with("HTTP/1.1 401"));
        // The rate: two a minute, then 429 with the time to wait, and the other token is untouched.
        for _ in 0..2 {
            assert!(!ask(addr, req(&quick, "POST", "/api/chat").as_bytes()).starts_with("HTTP/1.1 4"));
        }
        let limited = ask(addr, req(&quick, "POST", "/api/chat").as_bytes());
        assert!(limited.starts_with("HTTP/1.1 429") && limited.contains("Retry-After: "), "{limited}");
        assert!(!ask(addr, req(&relay, "POST", "/api/chat").as_bytes()).starts_with("HTTP/1.1 4"));
        // Revoked: refused on the next request.
        std::thread::sleep(Duration::from_millis(5));
        crate::tokens::revoke(&dir, "relay").unwrap();
        assert!(ask(addr, req(&relay, "POST", "/api/plugins/events").as_bytes()).starts_with("HTTP/1.1 401"));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A poll of the stream with a wait is held in the front until the bank rings (or the wait is
    /// over), looking every second when it does not ring; the bank's side is one ask each time.
    #[test]
    fn a_long_poll_of_the_stream_waits_for_the_bell_and_looks_every_second_without_it() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let rx = listen(listener, "secret".into(), Default::default(), crate::front::Hooks::default().feed).unwrap();
        let post = |body: &str| format!("POST /api/plugins/events HTTP/1.1\r\nAuthorization: Bearer secret\r\nContent-Length: {}\r\n\r\n{body}", body.len());
        let empty = || (200u16, "{\"epoch\":1,\"events\":[],\"last\":5,\"dropped\":0}".to_string());
        let one = || (200u16, "{\"epoch\":1,\"events\":[{\"id\":6,\"type\":\"chat\"}],\"last\":6,\"dropped\":0}".to_string());
        // Without a wait: one ask, answered as it comes (empty or not).
        let client = std::thread::spawn({ let r = post("{\"after\":5}"); move || ask(addr, r.as_bytes()) });
        rx.recv_timeout(Duration::from_secs(5)).unwrap().reply.send(empty()).unwrap();
        assert!(client.join().unwrap().contains("\"events\":[]"));
        // With a wait and a ring: the second ask comes at once after the bell, long before a second.
        let started = Instant::now();
        let client = std::thread::spawn({ let r = post("{\"after\":5,\"wait\":10}"); move || ask(addr, r.as_bytes()) });
        rx.recv_timeout(Duration::from_secs(5)).unwrap().reply.send(empty()).unwrap();
        assert!(rx.recv_timeout(Duration::from_millis(300)).is_err(), "nothing rang: it waits");
        ring_bell();
        let second = rx.recv_timeout(Duration::from_millis(500)).expect("the bell makes it ask the bank again");
        assert_eq!(second.body["after"], 5, "the same question");
        second.reply.send(one()).unwrap();
        let answer = client.join().unwrap();
        assert!(answer.starts_with("HTTP/1.1 200") && answer.contains("\"id\":6"), "{answer}");
        assert!(started.elapsed() < Duration::from_secs(3), "{:?}", started.elapsed());
        // With a wait and no ring: it looks again after a second.
        let started = Instant::now();
        let client = std::thread::spawn({ let r = post("{\"wait\":10}"); move || ask(addr, r.as_bytes()) });
        rx.recv_timeout(Duration::from_secs(5)).unwrap().reply.send(empty()).unwrap();
        let second = rx.recv_timeout(Duration::from_secs(4)).unwrap();
        assert!(started.elapsed() >= Duration::from_millis(900), "{:?}", started.elapsed());
        second.reply.send(one()).unwrap();
        assert!(client.join().unwrap().contains("\"id\":6"));
        // With a wait that runs out: the empty answer, after about the wait, and never past the cap.
        let started = Instant::now();
        let client = std::thread::spawn({ let r = post("{\"wait\":1.2}"); move || ask(addr, r.as_bytes()) });
        while !client.is_finished() {
            if let Ok(r) = rx.recv_timeout(Duration::from_millis(50)) {
                r.reply.send(empty()).unwrap();
            }
        }
        assert!(client.join().unwrap().contains("\"events\":[]") && (1.1..3.0).contains(&started.elapsed().as_secs_f64()), "{:?}", started.elapsed());
        // An error of the bank is the answer, at once.
        let client = std::thread::spawn({ let r = post("{\"wait\":10}"); move || ask(addr, r.as_bytes()) });
        rx.recv_timeout(Duration::from_secs(5)).unwrap().reply.send((400, "{\"error\":\"no\"}".into())).unwrap();
        assert!(client.join().unwrap().starts_with("HTTP/1.1 400"));
        // Only MAX_STREAM_POLLS wait at once.
        let waiting: Vec<_> = (0..MAX_STREAM_POLLS).map(|_| std::thread::spawn({ let r = post("{\"wait\":2}"); move || ask(addr, r.as_bytes()) })).collect();
        for _ in 0..MAX_STREAM_POLLS {
            rx.recv_timeout(Duration::from_secs(5)).unwrap().reply.send(empty()).unwrap();
        }
        let refused = ask(addr, post("{\"wait\":2}").as_bytes());
        assert!(refused.starts_with("HTTP/1.1 503"), "{refused}");
        while waiting.iter().any(|w| !w.is_finished()) {
            if let Ok(r) = rx.recv_timeout(Duration::from_millis(50)) {
                r.reply.send(empty()).unwrap();
            }
        }
        assert_eq!(STREAM_POLLS.load(Ordering::SeqCst), 0, "the slots are given back");
    }

    #[test]
    fn token_size_and_connection_limits() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let rx = listen(listener, "secret".into(), Default::default(), crate::front::Hooks::default().feed).unwrap();
        let wrong = b"GET /api/status HTTP/1.1\r\nAuthorization: Bearer secreT\r\n\r\n";
        assert!(ask(addr, wrong).starts_with("HTTP/1.1 401"));

        let client = std::thread::spawn(move || ask(addr, b"GET /api/status HTTP/1.1\r\nAuthorization: Bearer secret\r\n\r\n"));
        let request = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!((request.method.as_str(), request.path.as_str()), ("GET", "/api/status"));
        request.reply.send((200, "{}".into())).unwrap();
        assert!(client.join().unwrap().starts_with("HTTP/1.1 200"));

        // The logs are answered here, never by the game loop (nothing answers it in this test).
        let logs = ask(addr, b"GET /api/logs HTTP/1.1\r\nAuthorization: Bearer secret\r\n\r\n");
        let body: Value = serde_json::from_str(logs.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert!(logs.starts_with("HTTP/1.1 200") && body["lines"].is_array(), "{logs}");
        assert!(ask(addr, b"GET /api/logs HTTP/1.1\r\n\r\n").starts_with("HTTP/1.1 401"));
        assert!(rx.try_recv().is_err());

        // A body past the request limit, or a length that is no number: answered, never reaches
        // the game loop.
        let big = [b"POST /api/save HTTP/1.1\r\nAuthorization: Bearer secret\r\nContent-Length: 100000\r\n\r\n".as_slice(), &[b' '; 100_000]].concat();
        assert!(ask(addr, &big).starts_with("HTTP/1.1 413"));
        // A length within the limit alone, but not after the head: refused the same way.
        let close = [b"POST /api/save HTTP/1.1\r\nAuthorization: Bearer secret\r\nContent-Length: 65500\r\n\r\n".as_slice(), &[b' '; 65_500]].concat();
        assert!(ask(addr, &close).starts_with("HTTP/1.1 413"));
        assert!(ask(addr, b"POST /api/save HTTP/1.1\r\nAuthorization: Bearer secret\r\nContent-Length: ten\r\n\r\n").starts_with("HTTP/1.1 400"));
        assert!(rx.try_recv().is_err());
        // No other site may frame the page.
        let page = ask(addr, b"GET / HTTP/1.1\r\n\r\n");
        assert!(page.contains("X-Frame-Options: DENY\r\n") && page.contains("frame-ancestors 'none'"), "{page}");

        // MAX_CONNECTIONS idle connections hold every slot: the next is closed unanswered.
        let idle: Vec<TcpStream> = (0..MAX_CONNECTIONS).map(|_| TcpStream::connect(addr).unwrap()).collect();
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(ask(addr, wrong), "");
        drop(idle);
        let served = (0..50).any(|_| {
            std::thread::sleep(Duration::from_millis(20));
            ask(addr, wrong).starts_with("HTTP/1.1 401")
        });
        assert!(served, "slots are freed when connections end");
    }

    /// Other machines have a share of the slots and one address a part of it; this machine (the
    /// loopback) has the rest, so the command line always finds one. Slots come back when a
    /// connection ends.
    #[test]
    fn other_machines_have_their_share_and_the_loopback_the_rest() {
        let slots = Arc::new(Slots::default());
        let ip = |s: &str| -> std::net::IpAddr { s.parse().unwrap() };
        let mut held: Vec<Slot> = (0..MAX_PER_ADDRESS).map(|_| slots.admit(ip("192.0.2.1")).unwrap()).collect();
        assert_eq!(slots.admit(ip("192.0.2.1")).err(), Some(Refused::Busy), "one address has MAX_PER_ADDRESS");
        // Other addresses fit until the machines together have MAX_REMOTE.
        for other in ["198.51.100.1", "198.51.100.2"] {
            held.extend((0..MAX_PER_ADDRESS).map(|_| slots.admit(ip(other)).unwrap()));
        }
        assert_eq!(held.len(), MAX_REMOTE);
        assert_eq!(slots.admit(ip("203.0.113.9")).err(), Some(Refused::Busy), "the other machines together are at their share");
        assert!(slots.per_address.lock().unwrap().get(&ip("203.0.113.9")).is_none(), "a refusal leaves no count behind");
        // This machine has the rest, up to every slot.
        let local: Vec<Slot> = (0..MAX_CONNECTIONS - MAX_REMOTE).map(|_| slots.admit(ip("127.0.0.1")).unwrap()).collect();
        assert_eq!(slots.admit(ip("::1")).err(), Some(Refused::Full), "every slot is taken: closed unanswered");
        assert_eq!(slots.admit(ip("203.0.113.9")).err(), Some(Refused::Full));
        // Slots come back as connections end, address by address.
        drop(local);
        held.truncate(MAX_PER_ADDRESS);
        assert_eq!(slots.open.load(Ordering::SeqCst), MAX_PER_ADDRESS);
        assert_eq!(slots.remote.load(Ordering::SeqCst), MAX_PER_ADDRESS);
        assert_eq!(slots.per_address.lock().unwrap().len(), 1);
        assert!(slots.admit(ip("198.51.100.1")).is_ok() && slots.admit(ip("::1")).is_ok());
        drop(held);
        assert_eq!((slots.open.load(Ordering::SeqCst), slots.remote.load(Ordering::SeqCst), slots.per_address.lock().unwrap().len()), (0, 0, 0));
    }

    /// The address of this machine on the network (what a connection to it comes from, so it is not the
    /// loopback), when there is one.
    fn lan_ip() -> Option<std::net::IpAddr> {
        let probe = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
        probe.connect("192.0.2.1:9").ok()?;
        probe.local_addr().ok().map(|a| a.ip()).filter(|ip| !ip.is_loopback() && !ip.is_unspecified())
    }

    /// A machine that opens connections and says nothing holds its share and no more: the next
    /// connection from it is told 503 at once, while this machine's command line is served; and a
    /// head that does not come within two seconds ends the connection.
    #[test]
    fn a_silent_machine_holds_its_share_and_this_machine_is_still_served() {
        let Some(lan) = lan_ip() else { return };
        let listener = TcpListener::bind("0.0.0.0:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let rx = listen(listener, "secret".into(), Default::default(), crate::front::Hooks::default().feed).unwrap();
        let remote = std::net::SocketAddr::new(lan, port);
        let idle: Vec<TcpStream> = (0..MAX_PER_ADDRESS).map(|_| TcpStream::connect(remote).unwrap()).collect();
        std::thread::sleep(Duration::from_millis(200));
        let started = Instant::now();
        let refused = ask(remote, b"GET /api/health HTTP/1.1\r\n\r\n");
        assert!(refused.starts_with("HTTP/1.1 503") && refused.contains("Retry-After: 5") && refused.contains("too many connections"), "{refused}");
        assert!(started.elapsed() < Duration::from_secs(1), "told at once, not after the head's wait: {:?}", started.elapsed());
        // This machine is served meanwhile.
        let wrong = b"GET /api/status HTTP/1.1\r\nAuthorization: Bearer nope\r\n\r\n";
        assert!(ask(std::net::SocketAddr::new(std::net::Ipv4Addr::LOCALHOST.into(), port), wrong).starts_with("HTTP/1.1 401"));
        // The silent ones are let go when their head does not come (HEAD): the machine is served again.
        let began = Instant::now();
        let mut first = idle.into_iter().next().unwrap();
        let mut buf = [0u8; 16];
        assert_eq!(std::io::Read::read(&mut first, &mut buf).unwrap_or(0), 0, "closed without an answer");
        assert!(began.elapsed() >= Duration::from_millis(1500) && began.elapsed() < Duration::from_secs(4), "{:?}", began.elapsed());
        assert!((0..50).any(|_| {
            std::thread::sleep(Duration::from_millis(100));
            ask(remote, wrong).starts_with("HTTP/1.1 401")
        }));
        assert!(rx.try_recv().is_err());
    }

    /// The health route needs no token and is answered by the game loop in 3 s or it is 503; the
    /// metrics are text, need a token, and a token of the scope metrics.read is enough for them.
    #[test]
    fn health_needs_no_token_and_metrics_are_text_for_a_token_of_their_scope() {
        let dir = std::env::temp_dir().join(format!("pcds-admin-metrics-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (_, scoped) = crate::tokens::create(&dir, "prometheus", &["metrics.read".to_string()], None).unwrap();
        let (_, chat) = crate::tokens::create(&dir, "relay", &["chat.send".to_string()], None).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let rx = listen_with(listener, "secret".into(), Default::default(), crate::front::Hooks::default().feed, Arc::new(Checker::new(dir.clone()))).unwrap();
        // Healthy: no token, JSON.
        let client = std::thread::spawn(move || ask(addr, b"GET /api/health HTTP/1.1\r\n\r\n"));
        let request = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!((request.method.as_str(), request.path.as_str()), ("GET", "/api/health"));
        request.reply.send((200, "{\"ok\":true}".into())).unwrap();
        let answer = client.join().unwrap();
        assert!(answer.starts_with("HTTP/1.1 200") && answer.contains("Content-Type: application/json") && answer.ends_with("{\"ok\":true}"), "{answer}");
        // Not healthy: 503 from the loop; and a loop that does not answer in time is 503 too.
        let client = std::thread::spawn(move || ask(addr, b"GET /api/health HTTP/1.1\r\n\r\n"));
        rx.recv_timeout(Duration::from_secs(5)).unwrap().reply.send((503, "{\"ok\":false}".into())).unwrap();
        assert!(client.join().unwrap().starts_with("HTTP/1.1 503"));
        let started = Instant::now();
        let client = std::thread::spawn(move || ask(addr, b"GET /api/health HTTP/1.1\r\n\r\n"));
        let _silent = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let answer = client.join().unwrap();
        assert!(answer.starts_with("HTTP/1.1 503") && answer.ends_with("{\"ok\":false}") && started.elapsed() < Duration::from_secs(8), "{answer}");
        // Health is the one path other than the page that needs no token; the same path with another method does.
        assert!(ask(addr, b"POST /api/health HTTP/1.1\r\n\r\n").starts_with("HTTP/1.1 401"));
        // Metrics: no token is 401, a scope that is not theirs 403, the admin token or metrics.read pass and the answer is text.
        assert!(ask(addr, b"GET /api/metrics HTTP/1.1\r\n\r\n").starts_with("HTTP/1.1 401"));
        assert!(ask(addr, format!("GET /api/metrics HTTP/1.1\r\nAuthorization: Bearer {chat}\r\n\r\n").as_bytes()).starts_with("HTTP/1.1 403"));
        for token in ["secret", scoped.as_str()] {
            let request = format!("GET /api/metrics HTTP/1.1\r\nAuthorization: Bearer {token}\r\n\r\n");
            let client = std::thread::spawn(move || ask(addr, request.as_bytes()));
            rx.recv_timeout(Duration::from_secs(5)).unwrap().reply.send((200, "# HELP pcds_x x\n# TYPE pcds_x gauge\npcds_x 1\n".into())).unwrap();
            let answer = client.join().unwrap();
            assert!(answer.starts_with("HTTP/1.1 200") && answer.contains("Content-Type: text/plain; version=0.0.4; charset=utf-8\r\n") && answer.ends_with("pcds_x 1\n"), "{answer}");
        }
        // An error from the game stays JSON.
        let request = b"GET /api/metrics HTTP/1.1\r\nAuthorization: Bearer secret\r\n\r\n";
        let client = std::thread::spawn(move || ask(addr, request));
        rx.recv_timeout(Duration::from_secs(5)).unwrap().reply.send((500, "{\"error\":\"x\"}".into())).unwrap();
        assert!(client.join().unwrap().contains("Content-Type: application/json"));
        // A scoped token reaches nothing else.
        assert!(ask(addr, format!("GET /api/status HTTP/1.1\r\nAuthorization: Bearer {scoped}\r\n\r\n").as_bytes()).starts_with("HTTP/1.1 403"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A head that a proxy in front of the admin port could read another way is refused, so that no
    /// request means one thing there and another here: a header folded onto the line before (the
    /// continuation was read as a header of its own, `Authorization` among them), a name with a
    /// space before the colon, two lengths, two tokens, a head over the limit.
    #[test]
    fn a_head_that_can_be_read_two_ways_is_refused_not_guessed() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let rx = listen(listener, "secret".into(), Default::default(), crate::front::Hooks::default().feed).unwrap();
        let auth = "Authorization: Bearer secret\r\n";
        for (what, request) in [
            ("a folded line", format!("GET /api/ping HTTP/1.1\r\nX: a\r\n {auth}\r\n")),
            ("a folded line after a tab", format!("GET /api/ping HTTP/1.1\r\nX: a\r\n\t{auth}\r\n")),
            ("a space before the colon", "GET /api/ping HTTP/1.1\r\nAuthorization : Bearer secret\r\n\r\n".to_string()),
            ("two tokens", format!("GET /api/ping HTTP/1.1\r\nAuthorization: Bearer nope\r\n{auth}\r\n")),
            ("two lengths", format!("POST /api/save HTTP/1.1\r\n{auth}Content-Length: 5\r\ncontent-length: 0\r\n\r\n")),
            ("a line without a colon", format!("GET /api/ping HTTP/1.1\r\n{auth}garbage\r\n\r\n")),
            ("an empty name", format!("GET /api/ping HTTP/1.1\r\n{auth}: x\r\n\r\n")),
            // A line of blanks ends no head: a proxy reads it as a folded line and the headers after it as part of the one before.
            ("a line of blanks after the token", format!("GET /api/ping HTTP/1.1\r\n{auth} \r\n\r\n")),
            ("a line of blanks and a tab", format!("GET /api/ping HTTP/1.1\r\n{auth}\t \r\n\r\n")),
            ("a length with a sign", format!("POST /api/save HTTP/1.1\r\n{auth}Content-Length: +2\r\n\r\n{{}}")),
        ] {
            let answer = ask(addr, request.as_bytes());
            assert!(answer.starts_with("HTTP/1.1 400"), "{what}: {answer}");
        }
        let answer = ask(addr, format!("GET /api/ping HTTP/1.1\r\n{auth}X: {}\r\n\r\n", "a".repeat(70_000)).as_bytes());
        assert!(answer.starts_with("HTTP/1.1 431"), "{answer:.60}");
        assert!(rx.try_recv().is_err(), "none of them reached the game loop");
        // A head as clients write it still goes through (any case, LF-only lines, a body).
        let client = std::thread::spawn(move || ask(addr, b"POST /api/save HTTP/1.1\nauthorization:   Bearer secret  \nCONTENT-LENGTH: 2\nX-Other: a:b\n\n{}"));
        let request = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        request.reply.send((200, "{}".into())).unwrap();
        assert!(client.join().unwrap().starts_with("HTTP/1.1 200"));
    }

    /// The page may talk to this server and nobody else, load nothing from elsewhere and not be kept
    /// by a cache; the API's answers carry the framing and caching rules but not the page's policy.
    #[test]
    fn the_page_has_a_policy_of_its_own_and_the_api_does_not_need_it() {
        let dir = std::env::temp_dir().join(format!("pcds-admin-page-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("index.html");
        std::fs::write(&file, "<!doctype html><title>x</title>").unwrap();
        let web: Web = Arc::new(RwLock::new(Some(file.display().to_string())));
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let _rx = listen(listener, "secret".into(), web, crate::front::Hooks::default().feed).unwrap();
        let page = ask(addr, b"GET / HTTP/1.1\r\n\r\n");
        assert!(page.starts_with("HTTP/1.1 200") && page.contains("Content-Type: text/html"), "{page}");
        for header in ["default-src 'none'", "connect-src 'self'", "base-uri 'none'", "frame-ancestors 'none'", "X-Frame-Options: DENY\r\n", "X-Content-Type-Options: nosniff\r\n", "Referrer-Policy: no-referrer\r\n", "Cache-Control: no-store\r\n"] {
            assert!(page.contains(header), "{header}: {page}");
        }
        let api = ask(addr, b"GET /api/status HTTP/1.1\r\n\r\n");
        assert!(api.contains("Cache-Control: no-store\r\n") && api.contains("Content-Security-Policy: frame-ancestors 'none'\r\n") && !api.contains("script-src"), "{api}");
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[cfg(any(fuzzing, pcds_corpus))]
#[path = "../fuzz/targets/adminhttp.rs"]
pub mod fuzz;
