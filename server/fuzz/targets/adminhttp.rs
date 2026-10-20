//! The hand-written HTTP reader of the admin API: raw bytes in on a socket. It answers with an
//! HTTP response or closes; it passes a request to the game loop only with the right token in
//! the head (or for a route that needs none); it never panics, and every answer is well formed.
use super::*;
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::mpsc::channel;
use std::time::Duration;

const TOKEN: &str = "fuzz-admin-token-0123456789";

pub fn run(data: &[u8]) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = channel::<Request>();
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<(String, String, Value)>::new()));
    let responder = {
        let seen = seen.clone();
        std::thread::spawn(move || {
            while let Ok(r) = rx.recv() {
                seen.lock().unwrap().push((r.method.clone(), r.path.clone(), r.body.clone()));
                let _ = r.reply.send((200, "{\"ok\":true}".into()));
            }
        })
    };
    let server = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let hooks = crate::front::Hooks::default();
        let tokens = Checker::new(std::path::PathBuf::from("/nonexistent"));
        let _ = serve(stream, &tx, TOKEN, Some("/nonexistent/page.html"), &hooks.feed, &tokens);
    });
    let mut client = TcpStream::connect(addr).unwrap();
    client.set_read_timeout(Some(Duration::from_secs(15))).unwrap();
    // The write may fail when the server has already answered and closed.
    let _ = client.write_all(data);
    let _ = client.shutdown(Shutdown::Write);
    let mut answer = Vec::new();
    let _ = client.read_to_end(&mut answer);
    server.join().expect("the connection's thread panicked");
    responder.join().unwrap();
    if !answer.is_empty() {
        let text = String::from_utf8_lossy(&answer);
        let (head, body) = text.split_once("\r\n\r\n").expect("an answer has a head");
        assert!(head.starts_with("HTTP/1.1 "), "{head}");
        let length: usize = head.lines().find_map(|l| l.strip_prefix("Content-Length: ")).expect("a Content-Length").parse().unwrap();
        assert_eq!(length, body.len(), "the Content-Length of the answer is the body's");
        assert!(head.contains("X-Content-Type-Options: nosniff") && head.contains("Cache-Control: no-store"));
    }
    let forwarded = seen.lock().unwrap();
    if !forwarded.is_empty() {
        // A request reached the game loop: its head carried the token (the page needs none and is never passed on).
        let raw = String::from_utf8_lossy(data);
        assert!(raw.contains(&format!("Bearer {TOKEN}")), "a request without the token reached the game loop: {forwarded:?}");
        // Exactly one Authorization header said so (the head ends at the first empty line).
        let mut lines = raw.split('\n');
        lines.next();
        let head: Vec<&str> = lines.take_while(|l| !l.trim_end_matches('\r').is_empty()).collect();
        let heads = head.iter().filter(|l| l.split(':').next().is_some_and(|name| name.eq_ignore_ascii_case("authorization"))).count();
        assert_eq!(heads, 1, "{raw:?}");
    }
}
