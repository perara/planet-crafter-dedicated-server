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
/// headers and body), and how long it may take to arrive.
const MAX_CONNECTIONS: usize = 16;
const MAX_REQUEST: u64 = 64 << 10;
const DEADLINE: Duration = Duration::from_secs(10);

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
    let mut reader = BufReader::new(Deadline(stream.try_clone()?, Instant::now() + DEADLINE)).take(MAX_REQUEST);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("");
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let (path, query) = (path.to_string(), query.to_string());
    let (mut auth, mut length) = (String::new(), Some(0usize));
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header)? == 0 || header.trim().is_empty() {
            break;
        }
        if let Some((k, v)) = header.split_once(':') {
            match k.trim().to_ascii_lowercase().as_str() {
                "authorization" => auth = v.trim().to_string(),
                "content-length" => length = v.trim().parse().ok(),
                _ => {}
            }
        }
    }
    let Some(length) = length else { return respond(stream, 400, "application/json", "{\"error\":\"bad Content-Length\"}") };
    if length as u64 > MAX_REQUEST {
        return respond(stream, 413, "application/json", &format!("{{\"error\":\"a request body is at most {} KiB\"}}", MAX_REQUEST >> 10));
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
    if tx.send(Request { method, path, body, reply }).is_err() {
        return respond(stream, 500, "application/json", "{\"error\":\"server stopping\"}");
    }
    match answer.recv_timeout(Duration::from_secs(10)) {
        Ok((status, json)) => respond(stream, status, "application/json", &json),
        Err(_) => respond(stream, 500, "application/json", "{\"error\":\"the game loop did not answer\"}"),
    }
}

/// The server's recent output, from the journal of the systemd user unit pcds runs it as
/// (PCDS_UNIT). Read on the HTTP thread: journalctl takes longer than a game tick.
fn logs() -> Value {
    let Some(unit) = std::env::var("PCDS_UNIT").ok().filter(|u| !u.is_empty()) else {
        return serde_json::json!({ "lines": [], "note": "Not running as a pcds systemd unit: the log is the server's own output (docker logs, the terminal)." });
    };
    let unit = format!("{unit}.service");
    match std::process::Command::new("journalctl").args(["--user", "-u", &unit, "-n", "300", "-o", "short-iso", "--no-pager"]).output() {
        Ok(out) if out.status.success() => serde_json::json!({ "lines": String::from_utf8_lossy(&out.stdout).lines().collect::<Vec<_>>(), "source": format!("journalctl --user -u {unit}") }),
        Ok(out) => serde_json::json!({ "lines": [], "note": String::from_utf8_lossy(&out.stderr).trim() }),
        Err(e) => serde_json::json!({ "lines": [], "note": format!("journalctl: {e}") }),
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
