//! Admin API and web GUI over HTTP on 127.0.0.1, the same endpoints as the plugin server's
//! (so its web GUI works unchanged): GET /api/status, POST /api/save, /api/stop, /api/kick,
//! /api/ban, /api/unban, /api/access, /api/settings. Every /api call needs
//! "Authorization: Bearer <token>". A thread serves HTTP; the game loop answers requests.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::Duration;

use serde_json::Value;

pub struct Request {
    pub method: String,
    pub path: String,
    pub body: Value,
    /// (status, JSON body)
    pub reply: Sender<(u16, String)>,
}

pub fn start(addr: &str, token: String, web_index: Option<String>) -> std::io::Result<Receiver<Request>> {
    let listener = TcpListener::bind(addr)?;
    let (tx, rx) = channel();
    std::thread::Builder::new().name("pcds-admin".into()).spawn(move || {
        for stream in listener.incoming().flatten() {
            let (tx, token, web) = (tx.clone(), token.clone(), web_index.clone());
            std::thread::spawn(move || {
                let _ = serve(stream, &tx, &token, web.as_deref());
            });
        }
    })?;
    Ok(rx)
}

fn respond(mut stream: TcpStream, status: u16, content_type: &str, body: &str) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        _ => "Error",
    };
    write!(stream, "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
}

fn serve(stream: TcpStream, tx: &Sender<Request>, token: &str, web: Option<&str>) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let (method, path) = (parts.next().unwrap_or("").to_string(), parts.next().unwrap_or("").split('?').next().unwrap_or("").to_string());
    let (mut auth, mut length) = (String::new(), 0usize);
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header)? == 0 || header.trim().is_empty() {
            break;
        }
        if let Some((k, v)) = header.split_once(':') {
            match k.trim().to_ascii_lowercase().as_str() {
                "authorization" => auth = v.trim().to_string(),
                "content-length" => length = v.trim().parse().unwrap_or(0),
                _ => {}
            }
        }
    }
    let mut body = vec![0u8; length.min(1 << 20)];
    reader.read_exact(&mut body)?;
    if method == "GET" && (path == "/" || path == "/index.html") {
        return match web.and_then(|p| std::fs::read_to_string(p).ok()) {
            Some(html) => respond(stream, 200, "text/html; charset=utf-8", &html),
            None => respond(stream, 404, "text/plain", "no web GUI installed"),
        };
    }
    if auth != format!("Bearer {token}") {
        return respond(stream, 401, "application/json", "{\"error\":\"missing or wrong token\"}");
    }
    let body: Value = if body.iter().all(u8::is_ascii_whitespace) { Value::Object(Default::default()) } else {
        match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(e) => return respond(stream, 400, "application/json", &serde_json::json!({ "error": e.to_string() }).to_string()),
        }
    };
    let (reply, answer) = channel();
    if tx.send(Request { method, path, body, reply }).is_err() {
        return respond(stream, 500, "application/json", "{\"error\":\"server stopping\"}");
    }
    match answer.recv_timeout(Duration::from_secs(10)) {
        Ok((status, json)) => respond(stream, status, "application/json", &json),
        Err(_) => respond(stream, 500, "application/json", "{\"error\":\"the game loop did not answer\"}"),
    }
}
