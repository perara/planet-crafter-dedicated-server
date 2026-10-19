//! Admin API and web GUI over HTTP on 127.0.0.1, the same endpoints as the plugin server's
//! (so its web GUI works unchanged): GET /api/status, POST /api/save, /api/stop, /api/kick,
//! /api/ban, /api/unban, /api/access, /api/settings; and this server's own: GET and POST
//! /api/rules and /api/config, POST /api/chat and /api/teleport, GET /api/chat/events,
//! /api/worlds, /api/map and /api/logs, GET and POST /api/update (see admin_request in bank.rs
//! and front.rs). Every /api call needs "Authorization: Bearer <token>". A thread in the front
//! serves HTTP; the front's loop answers requests or passes them to the bank, but for the logs
//! and the chat (chathooks.rs): a long poll must not hold the loop.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::chathooks::Feed;

/// Connections served at once (more are closed unanswered), the bytes of one request (line,
/// headers and body), and how long it may take to arrive: the line and headers within HEAD (an
/// idle connection gives its slot back soon), the whole request within DEADLINE. Chat long
/// polls have a cap of their own (chathooks::MAX_POLLS), so they never take every slot.
const MAX_CONNECTIONS: usize = 64;
const MAX_REQUEST: u64 = 64 << 10;
const HEAD: Duration = Duration::from_secs(3);
const DEADLINE: Duration = Duration::from_secs(10);
/// How long the game loop may take to answer: 10 s, but the world controls stop and start the
/// bank inside that loop (a save and a stop of up to 60 s, a load of up to 120 s: front.rs
/// STOP_GRACE and LOAD_DEADLINE), so a switch is answered up to 200 s after, not as failed at 10.
const ANSWER: Duration = Duration::from_secs(10);
const WORLD_ANSWER: Duration = Duration::from_secs(200);

pub struct Request {
    pub method: String,
    pub path: String,
    pub body: Value,
    /// (status, JSON body)
    pub reply: Sender<(u16, String)>,
}

/// The web page's file: the live bank's, so a flip brings its page along.
pub type Web = Arc<RwLock<Option<String>>>;

pub fn start(addr: &str, token: String, web_index: Web, chat: Arc<Feed>) -> std::io::Result<Receiver<Request>> {
    listen(TcpListener::bind(addr)?, token, web_index, chat)
}

pub fn listen(listener: TcpListener, token: String, web_index: Web, chat: Arc<Feed>) -> std::io::Result<Receiver<Request>> {
    let (tx, rx) = channel();
    let open = Arc::new(AtomicUsize::new(0));
    std::thread::Builder::new().name("pcds-admin".into()).spawn(move || {
        for stream in listener.incoming().flatten() {
            // Only this thread adds, so the count cannot pass the cap.
            if open.load(Ordering::SeqCst) >= MAX_CONNECTIONS {
                continue;
            }
            open.fetch_add(1, Ordering::SeqCst);
            let (slot, tx, token, web, chat) = (Slot(open.clone()), tx.clone(), token.clone(), web_index.clone(), chat.clone());
            std::thread::spawn(move || {
                let _slot = slot;
                let page = web.read().unwrap_or_else(|e| e.into_inner()).clone();
                let _ = serve(stream, &tx, &token, page.as_deref(), &chat);
            });
        }
    })?;
    Ok(rx)
}

/// One connection being served; freed when its thread ends, panic or not.
struct Slot(Arc<AtomicUsize>);

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
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
    if path.starts_with("/api/world/") { WORLD_ANSWER } else { ANSWER }
}

/// Equal strings, in time that does not depend on where they differ.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0, |d, (x, y)| d | (x ^ y)) == 0
}

fn respond(mut stream: TcpStream, status: u16, content_type: &str, body: &str) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        409 => "Conflict",
        411 => "Length Required",
        413 => "Content Too Large",
        503 => "Service Unavailable",
        _ => "Error",
    };
    // The page holds the token once logged in: no other site may frame it (clickjacking).
    let frames = "X-Frame-Options: DENY\r\nContent-Security-Policy: frame-ancestors 'none'\r\n";
    write!(stream, "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n{frames}Connection: close\r\n\r\n{body}", body.len())
}

fn serve(stream: TcpStream, tx: &Sender<Request>, token: &str, web: Option<&str>, chat: &Feed) -> std::io::Result<()> {
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
    let (mut auth, mut length, mut chunked) = (String::new(), Some(0usize), false);
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header)? == 0 || header.trim().is_empty() {
            break;
        }
        if let Some((k, v)) = header.split_once(':') {
            match k.trim().to_ascii_lowercase().as_str() {
                "authorization" => auth = v.trim().to_string(),
                "content-length" => length = v.trim().parse().ok(),
                "transfer-encoding" => chunked = true,
                _ => {}
            }
        }
    }
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
    if method == "GET" && (path == "/" || path == "/index.html") {
        return match web.and_then(|p| std::fs::read_to_string(p).ok()) {
            Some(html) => respond(stream, 200, "text/html; charset=utf-8", &html),
            None => respond(stream, 404, "text/plain", "no web GUI installed"),
        };
    }
    if !same(&auth, &format!("Bearer {token}")) {
        return respond(stream, 401, "application/json", "{\"error\":\"missing or wrong token\"}");
    }
    if method == "GET" && path == "/api/logs" {
        return respond(stream, 200, "application/json", &logs().to_string());
    }
    let body: Value = if body.iter().all(u8::is_ascii_whitespace) { Value::Object(Default::default()) } else {
        match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(e) => return respond(stream, 400, "application/json", &serde_json::json!({ "error": e.to_string() }).to_string()),
        }
    };
    if path.starts_with("/api/chat") {
        let (status, json) = crate::chathooks::api(chat, &method, &path, &query, &body);
        return respond(stream, status, "application/json", &json);
    }
    let (reply, answer) = channel();
    let wait = answer_wait(&path);
    if tx.send(Request { method, path, body, reply }).is_err() {
        return respond(stream, 500, "application/json", "{\"error\":\"server stopping\"}");
    }
    match answer.recv_timeout(wait) {
        Ok((status, json)) => respond(stream, status, "application/json", &json),
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

    #[test]
    fn token_size_and_connection_limits() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let rx = listen(listener, "secret".into(), Default::default(), crate::chathooks::Hooks::default().feed).unwrap();
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
}
