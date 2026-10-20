//! `net.http` against servers that behave badly, all of them on this machine's loopback (no test
//! reaches a real host): a host that is not on the list, a redirect to one that is not, an answer
//! that never ends, a server that trickles, a name that points at the local network, a flood. What
//! each must show is the same as for the plugins that loop: the request ends within its limits,
//! the offence is counted, the plugin is told in a few fixed words, and nothing else was touched.

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pcds_plugin_api::host::{E_BAD, E_DENIED, E_FULL, E_NOT_ALLOWED};
use pcds_plugin_api::{Event, HttpRequest, Verdict, Writer};

use super::net::{Done, Dns, Fetch, Net, Policy};
use super::runtime::{Cost, Fault};
use super::tests::{Mock, boot, fresh_dir, install, module, write_plugin};
use super::*;

// ---- servers on the loopback ----------------------------------------------------------------------

struct Server {
    addr: SocketAddr,
    hits: Arc<AtomicUsize>,
    /// The head of each request received (request line and headers).
    heads: Arc<Mutex<Vec<String>>>,
    /// The most connections open at once.
    peak: Arc<AtomicUsize>,
}

impl Server {
    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.addr.port())
    }
    fn pattern(&self) -> String {
        format!("http://127.0.0.1:{}", self.addr.port())
    }
    fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }
}

/// A server on 127.0.0.1 that hands each request (its head) and connection to `handler`.
fn serve(handler: impl Fn(&mut TcpStream, &str) + Send + Sync + 'static) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (hits, heads, peak, open) = (Arc::new(AtomicUsize::new(0)), Arc::new(Mutex::new(Vec::new())), Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let handler = Arc::new(handler);
    let (h2, heads2, peak2) = (hits.clone(), heads.clone(), peak.clone());
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let (handler, hits, heads, peak, open) = (handler.clone(), h2.clone(), heads2.clone(), peak2.clone(), open.clone());
            std::thread::spawn(move || {
                let now = open.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                let _ = stream.set_read_timeout(Some(Duration::from_secs(20)));
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") && head.len() < 65536 && stream.read(&mut byte).map(|n| n == 1).unwrap_or(false) {
                    head.push(byte[0]);
                }
                let head = String::from_utf8_lossy(&head).into_owned();
                if !head.is_empty() {
                    hits.fetch_add(1, Ordering::SeqCst);
                    heads.lock().unwrap().push(head.clone());
                    // The body, if there is one, is read (and dropped).
                    if let Some(n) = head.lines().find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").and_then(|v| v.trim().parse::<usize>().ok())) {
                        let _ = std::io::copy(&mut (&mut stream).take(n as u64), &mut std::io::sink());
                    }
                    handler(&mut stream, &head);
                }
                open.fetch_sub(1, Ordering::SeqCst);
            });
        }
    });
    Server { addr, hits, heads, peak }
}

fn respond(stream: &mut TcpStream, status: &str, headers: &[(&str, &str)], body: &[u8]) {
    let mut out = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n", body.len());
    for (k, v) in headers {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    out.push_str("\r\n");
    let _ = stream.write_all(out.as_bytes());
    let _ = stream.write_all(body);
}

fn ok_server() -> Server {
    serve(|s, _| respond(s, "200 OK", &[("Content-Type", "text/plain"), ("X-Test", "yes")], b"hello"))
}

/// A fake DNS: each name has a list of answers; each lookup takes the next (the last one stays).
#[derive(Default)]
struct FakeDns {
    answers: Mutex<HashMap<String, VecDeque<Vec<IpAddr>>>>,
    lookups: AtomicUsize,
}

impl FakeDns {
    fn with(name: &str, answers: &[&[&str]]) -> Arc<FakeDns> {
        let d = FakeDns::default();
        d.answers.lock().unwrap().insert(name.to_string(), answers.iter().map(|a| a.iter().map(|ip| ip.parse().unwrap()).collect()).collect());
        Arc::new(d)
    }
}

impl Dns for FakeDns {
    fn lookup(&self, host: &str, port: u16) -> std::io::Result<Vec<SocketAddr>> {
        self.lookups.fetch_add(1, Ordering::SeqCst);
        let mut all = self.answers.lock().unwrap();
        let q = all.get_mut(host).ok_or_else(|| std::io::Error::other("no such name"))?;
        let ips = if q.len() > 1 { q.pop_front().unwrap() } else { q.front().cloned().unwrap_or_default() };
        Ok(ips.into_iter().map(|ip| SocketAddr::new(ip, port)).collect())
    }
}

// ---- a plugin that can be told what to ask --------------------------------------------------------

type Seen = Arc<Mutex<Vec<(u64, u32, String, Vec<u8>, Vec<(String, String)>)>>>;

/// A plugin written in Rust for the test: on a chat event it sends the requests it was given and
/// notes the ticket or the code it got; the `HttpDone`s it receives are noted too.
struct Native {
    ctx: Ctx,
    to_send: Arc<Mutex<Vec<Vec<u8>>>>,
    results: Arc<Mutex<Vec<i64>>>,
    seen: Seen,
}

impl Runtime for Native {
    fn ctx(&mut self) -> &mut Ctx {
        &mut self.ctx
    }
    fn start(&mut self, _: Option<&Blob>) -> Result<(), Fault> {
        Ok(())
    }
    fn event(&mut self, event: &Event<'_>, _: &[u8]) -> Result<(Verdict, Cost), Fault> {
        match event {
            Event::Chat { .. } => {
                for request in std::mem::take(&mut *self.to_send.lock().unwrap()) {
                    let code = self.ctx.http_request(&request);
                    self.results.lock().unwrap().push(code);
                }
            }
            Event::HttpDone { ticket, status, error, body, headers } => {
                self.seen.lock().unwrap().push((*ticket, *status, error.to_string(), body.to_vec(), headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()));
            }
            _ => {}
        }
        Ok((Verdict::Continue, Cost::default()))
    }
    fn save(&mut self) -> Result<Option<Blob>, Fault> {
        Ok(None)
    }
    fn restart(&mut self, _: Option<&Blob>) -> Result<(), Fault> {
        Ok(())
    }
}

struct Rig {
    host: Host,
    world: Mock,
    to_send: Arc<Mutex<Vec<Vec<u8>>>>,
    results: Arc<Mutex<Vec<i64>>>,
    seen: Seen,
    dir: PathBuf,
}

/// A host with one plugin, `probe`, that asks for `net.http` (and `net.http.local` if `local`) and
/// lists `allow`; `net_extra` is more of its `[net]` section; `config` more of its manifest.
fn rig_with(allow: &[String], local: bool, net_extra: &str, config: &str, dns: Option<Arc<dyn Dns>>, grant: Option<&[&str]>) -> Rig {
    let dir = fresh_dir("net");
    let mut caps = vec!["event.chat", "net.http"];
    if local {
        caps.push("net.http.local");
    }
    let list = allow.iter().map(|a| format!("\"{a}\"")).collect::<Vec<_>>().join(", ");
    install(&dir, "probe", &module("(i32.const 0)", ""), &caps, &format!("[events]\nsubscribe = [\"chat\"]\n[net]\nallow = [{list}]\n{net_extra}\n{config}"), grant);
    let mut host = boot(&dir);
    if let Some(dns) = dns {
        host.net = Net::with_dns(dns);
    }
    let (to_send, results, seen): (Arc<Mutex<Vec<Vec<u8>>>>, Arc<Mutex<Vec<i64>>>, Seen) = Default::default();
    let p = &mut host.plugins[0];
    assert!(p.running(), "{:?}", p.status);
    let mut ctx = Ctx::new(&p.id, p.grant);
    ctx.net = p.net.clone();
    p.rt = Some(Box::new(Native { ctx, to_send: to_send.clone(), results: results.clone(), seen: seen.clone() }));
    Rig { host, world: Mock::default(), to_send, results, seen, dir }
}

fn rig(allow: &[String], local: bool) -> Rig {
    rig_with(allow, local, "", "", None, None)
}

fn request(method: &str, url: &str, headers: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
    let mut w = Writer::new();
    HttpRequest { method, url, headers: headers.to_vec(), body }.encode(&mut w);
    w.0
}

impl Rig {
    /// Sends the requests in one call (a chat event); the codes and tickets it got.
    fn ask(&mut self, requests: Vec<Vec<u8>>) -> Vec<i64> {
        self.results.lock().unwrap().clear();
        *self.to_send.lock().unwrap() = requests;
        self.host.run(&mut self.world, &Event::Chat { client: 1, name: "Ann", text: "go" }, 0);
        self.results.lock().unwrap().clone()
    }

    /// Delivers answers until `n` have arrived; the answers.
    fn wait(&mut self, n: usize) -> Vec<(u64, u32, String, Vec<u8>, Vec<(String, String)>)> {
        let t = Instant::now();
        while self.seen.lock().unwrap().len() < n {
            self.host.deliver_http(&mut self.world, 0);
            assert!(t.elapsed() < Duration::from_secs(30), "no answer after 30 s: {} of {n}", self.seen.lock().unwrap().len());
            std::thread::sleep(Duration::from_millis(5));
        }
        self.seen.lock().unwrap().clone()
    }

    /// One request, one answer: (status, error, body).
    fn get(&mut self, url: &str) -> (u32, String, Vec<u8>) {
        let before = self.seen.lock().unwrap().len();
        let codes = self.ask(vec![request("GET", url, &[], b"")]);
        assert!(codes[0] > 0, "refused with {}", codes[0]);
        let all = self.wait(before + 1);
        let (_, status, error, body, _) = all[before].clone();
        (status, error, body)
    }

    /// The plugin's runtime is a `Native` again (after a rescan started its module).
    fn natively(&mut self) {
        let p = &mut self.host.plugins[0];
        let mut ctx = Ctx::new(&p.id, p.grant);
        ctx.net = p.net.clone();
        p.rt = Some(Box::new(Native { ctx, to_send: self.to_send.clone(), results: self.results.clone(), seen: self.seen.clone() }));
    }

    fn stats(&self) -> &Stats {
        &self.host.plugins[0].stats
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

// ---- the tests ------------------------------------------------------------------------------------

#[test]
fn a_request_to_a_listed_host_is_answered_as_an_event_to_the_plugin() {
    let server = serve(|s, head| {
        let echo = if head.starts_with("POST /echo") { "posted" } else { "got" };
        respond(s, "201 Created", &[("Content-Type", "text/plain"), ("X-Test", "yes")], echo.as_bytes());
    });
    let mut rig = rig(&[server.pattern()], true);
    let codes = rig.ask(vec![request("post", &server.url("/echo?x=1"), &[("Content-Type", "text/plain"), ("X-Mine", "1")], b"payload")]);
    assert_eq!(codes.len(), 1);
    let ticket = codes[0];
    assert!(ticket > 0, "a ticket: {ticket}");
    assert_eq!(rig.stats().http_requests, 1);
    let done = rig.wait(1);
    let (t, status, error, body, headers) = &done[0];
    assert_eq!((*t as i64, *status, error.as_str(), body.as_slice()), (ticket, 201, "", b"posted".as_slice()));
    assert!(headers.iter().any(|(k, v)| k == "x-test" && v == "yes"), "{headers:?}");
    let head = server.heads.lock().unwrap()[0].clone();
    assert!(head.starts_with("POST /echo?x=1 HTTP/1.1"), "{head}");
    let lower = head.to_ascii_lowercase();
    assert!(lower.contains("x-mine: 1") && lower.contains("content-type: text/plain") && lower.contains("content-length: 7") && lower.contains(&format!("host: 127.0.0.1:{}", server.addr.port())), "{head}");
    assert!(lower.contains("user-agent: pcds/") && lower.contains("(plugin probe)"), "{head}");
    assert_eq!((rig.stats().http_answered, rig.stats().http_failed, rig.stats().http_bytes_out, rig.stats().http_bytes_in), (1, 0, 7, 6));
    // The plugin was called for it (a call counts), and nothing is left in flight.
    assert_eq!(rig.host.plugins[0].net.as_ref().unwrap().in_flight.load(Ordering::SeqCst), 0);
    assert!(!rig.host.net_waiting());
}

#[test]
fn a_host_that_is_not_listed_is_refused_at_once_and_nothing_is_sent() {
    let allowed = ok_server();
    let other = ok_server();
    let mut rig = rig(&[allowed.pattern()], true);
    let codes = rig.ask(vec![
        request("GET", &other.url("/"), &[], b""),
        request("GET", &format!("http://127.0.0.1:{}/", other.addr.port() + 1), &[], b""),
        request("GET", &format!("https://127.0.0.1:{}/", allowed.addr.port()), &[], b""),
        request("GET", &format!("http://localhost:{}/", allowed.addr.port()), &[], b""),
        request("GET", &format!("http://127.0.0.1:{}@evil.example/", allowed.addr.port()), &[], b""),
        request("GET", "file:///etc/passwd", &[], b""),
        request("GET", "ftp://127.0.0.1/", &[], b""),
        request("GET", "http://[::1]:80/", &[], b""),
    ]);
    assert_eq!(codes, vec![E_NOT_ALLOWED as i64, E_NOT_ALLOWED as i64, E_NOT_ALLOWED as i64, E_NOT_ALLOWED as i64, E_BAD as i64, E_BAD as i64, E_BAD as i64, E_NOT_ALLOWED as i64]);
    assert_eq!((other.hits(), allowed.hits()), (0, 0));
    assert_eq!((rig.stats().refused, rig.stats().http_requests), (8, 0));
    assert!(!rig.host.net_waiting());
}

#[test]
fn without_the_capability_nothing_is_asked() {
    let server = ok_server();
    // Asked for, but the admin granted only the chat event.
    let mut rig = rig_with(&[server.pattern()], true, "", "", None, Some(&["event.chat"]));
    assert!(rig.host.plugins[0].net.is_none());
    assert_eq!(rig.ask(vec![request("GET", &server.url("/"), &[], b"")]), vec![E_DENIED as i64]);
    assert_eq!((rig.stats().denied, server.hits()), (1, 0));
}

#[test]
fn an_ip_pattern_is_dead_unless_the_local_capability_is_granted_too() {
    let server = ok_server();
    // Local asked for in the manifest, only net.http granted: the 127.0.0.1 pattern matches nothing.
    let mut rig = rig_with(&[server.pattern()], true, "", "", None, Some(&["event.chat", "net.http"]));
    assert!(!rig.host.plugins[0].net.as_ref().unwrap().policy.local);
    assert_eq!(rig.ask(vec![request("GET", &server.url("/"), &[], b"")]), vec![E_NOT_ALLOWED as i64]);
    assert_eq!(server.hits(), 0);
}

#[test]
fn a_redirect_to_a_host_that_is_not_listed_is_not_followed() {
    let secret_place = ok_server();
    let target = secret_place.url("/internal");
    let listed = serve(move |s, _| respond(s, "302 Found", &[("Location", &target)], b""));
    let mut rig = rig(&[listed.pattern()], true);
    let (status, error, _) = rig.get(&listed.url("/go"));
    assert_eq!((status, error.as_str()), (0, "redirected to a host that is not on the allow list"));
    assert_eq!((listed.hits(), secret_place.hits()), (1, 0));
    assert_eq!(rig.stats().http_failed, 1);
}

#[test]
fn redirects_between_listed_hosts_are_followed_a_few_times_and_without_credentials() {
    let second = ok_server();
    let to_second = second.url("/landed");
    let first = serve(move |s, head| {
        if head.starts_with("GET /loop") {
            respond(s, "302 Found", &[("Location", "/loop")], b"");
        } else {
            respond(s, "307 Temporary Redirect", &[("Location", &to_second)], b"");
        }
    });
    let mut rig = rig(&[first.pattern(), second.pattern()], true);
    let codes = rig.ask(vec![request("GET", &first.url("/start"), &[("Authorization", "Bearer abc"), ("X-Keep", "1")], b"")]);
    assert!(codes[0] > 0);
    let (_, status, error, body, _) = rig.wait(1)[0].clone();
    assert_eq!((status, error.as_str(), body.as_slice()), (200, "", b"hello".as_slice()));
    let first_head = first.heads.lock().unwrap()[0].to_ascii_lowercase();
    let second_head = second.heads.lock().unwrap()[0].to_ascii_lowercase();
    assert!(first_head.contains("authorization: bearer abc"), "the host the plugin named gets its header");
    assert!(!second_head.contains("authorization") && second_head.contains("x-keep: 1"), "another host does not: {second_head}");
    assert!(second_head.starts_with("get /landed"), "{second_head}");
    // A loop ends at the limit.
    let (status, error, _) = rig.get(&first.url("/loop"));
    assert_eq!((status, error.as_str()), (0, "too many redirects"));
    assert_eq!(first.hits(), 1 + limits::HTTP_REDIRECTS + 1);
}

#[test]
fn an_answer_that_never_ends_is_cut_at_the_limit_and_the_server_is_let_go() {
    let sent = Arc::new(AtomicUsize::new(0));
    let sent2 = sent.clone();
    let server = serve(move |s, head| {
        let chunked = head.contains("/chunked");
        let _ = if chunked { s.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n") } else { s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1073741824\r\nConnection: close\r\n\r\n") };
        let block = vec![b'x'; 65536];
        loop {
            let ok = if chunked { s.write_all(b"10000\r\n").and_then(|_| s.write_all(&block)).and_then(|_| s.write_all(b"\r\n")) } else { s.write_all(&block) };
            if ok.is_err() {
                return;
            }
            sent2.fetch_add(65536, Ordering::SeqCst);
        }
    });
    let mut rig = rig_with(&[server.pattern()], true, "max_response_bytes = 20000", "", None, None);
    for path in ["/plain", "/chunked"] {
        sent.store(0, Ordering::SeqCst);
        let t = Instant::now();
        let (status, error, body) = rig.get(&server.url(path));
        assert_eq!((status, error.as_str(), body.len()), (0, "the answer is larger than the limit", 0), "{path}");
        assert!(t.elapsed() < Duration::from_secs(5), "{path}: {:?}", t.elapsed());
        // The client stopped reading: the server did not get to write 1 GiB (the socket buffers hold some).
        std::thread::sleep(Duration::from_millis(200));
        assert!(sent.load(Ordering::SeqCst) < 64 << 20, "{path}: the server wrote {} bytes", sent.load(Ordering::SeqCst));
    }
    // An answer that fits is delivered whole, and its size counts.
    let small = serve(|s, _| respond(s, "200 OK", &[], &[b'y'; 5000]));
    let mut ok = rig_with(&[small.pattern()], true, "max_response_bytes = 5000", "", None, None);
    let (status, error, body) = ok.get(&small.url("/"));
    assert_eq!((status, error.as_str(), body.len()), (200, "", 5000));
    assert_eq!(ok.stats().http_bytes_in, 5000);
}

#[test]
fn a_server_that_trickles_or_never_answers_is_let_go_at_the_timeout() {
    // Headers one byte at a time; a body one byte at a time; accepted and silent.
    let server = serve(|s, head| {
        if head.contains("/silent") {
            std::thread::sleep(Duration::from_secs(8));
        } else if head.contains("/headers") {
            for b in b"HTTP/1.1 200 OK\r\nX-Slow: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".iter() {
                if s.write_all(&[*b]).is_err() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(60));
            }
        } else {
            let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100000\r\nConnection: close\r\n\r\n");
            for _ in 0..100_000 {
                if s.write_all(b"z").is_err() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(60));
            }
        }
    });
    let mut rig = rig_with(&[server.pattern()], true, "timeout_ms = 700", "", None, None);
    for path in ["/silent", "/headers", "/body"] {
        let t = Instant::now();
        let (status, error, body) = rig.get(&server.url(path));
        assert_eq!((status, error.as_str(), body.len()), (0, "timed out", 0), "{path}");
        assert!(t.elapsed() < Duration::from_secs(4), "{path}: {:?}", t.elapsed());
    }
    assert_eq!(rig.stats().http_failed, 3);
}

#[test]
fn a_name_that_points_at_the_local_network_reaches_nothing() {
    let server = ok_server();
    let port = server.addr.port();
    let pattern = format!("http://app.test:{port}");
    for (answer, why) in [
        (vec!["127.0.0.1"], "loopback"),
        (vec!["10.1.2.3"], "private"),
        (vec!["192.168.0.9"], "private"),
        (vec!["169.254.169.254"], "metadata address"),
        (vec!["100.64.0.5"], "shared address space"),
        (vec!["::1"], "ipv6 loopback"),
        (vec!["fd00::5"], "unique local"),
        (vec!["::ffff:127.0.0.1"], "mapped loopback"),
        (vec!["0.0.0.0"], "unspecified"),
        (vec!["8.8.8.8", "127.0.0.1"], "a public answer next to a loopback one"),
        (vec!["127.0.0.1", "8.8.8.8"], "a loopback answer next to a public one"),
    ] {
        let dns = FakeDns::with("app.test", &[&answer]);
        let mut rig = rig_with(std::slice::from_ref(&pattern), false, "", "", Some(dns.clone()), None);
        let (status, error, _) = rig.get(&format!("http://app.test:{port}/"));
        assert_eq!((status, error.as_str()), (0, "the name resolves to an address the plugin may not reach (local, private or reserved)"), "{why}");
        assert_eq!(dns.lookups.load(Ordering::SeqCst), 1, "{why}");
    }
    assert_eq!(server.hits(), 0, "not one connection was made");
    // With the local capability the loopback answer is the way to the server (and 0.0.0.0 still is not).
    let dns = FakeDns::with("app.test", &[&["127.0.0.1"]]);
    let mut rig = rig_with(&[pattern], true, "", "", Some(dns), None);
    assert_eq!(rig.get(&format!("http://app.test:{port}/")).0, 200);
    let head = server.heads.lock().unwrap()[0].to_ascii_lowercase();
    assert!(head.contains(&format!("host: app.test:{port}")), "{head}");
}

#[test]
fn the_name_is_resolved_again_for_every_connection_and_each_answer_is_checked() {
    let server = serve(|s, _| respond(s, "302 Found", &[("Location", "/second")], b""));
    let port = server.addr.port();
    // The first lookup says loopback (allowed with the local capability), the second says "nothing
    // anyone may reach": the redirect to the same name is stopped by the second answer.
    let dns = FakeDns::with("app.test", &[&["127.0.0.1"], &["0.0.0.0"]]);
    let mut rig = rig_with(&[format!("http://app.test:{port}")], true, "", "", Some(dns.clone()), None);
    let (status, error, _) = rig.get(&format!("http://app.test:{port}/first"));
    assert_eq!((status, error.as_str()), (0, "the name resolves to an address the plugin may not reach (local, private or reserved)"));
    assert_eq!((dns.lookups.load(Ordering::SeqCst), server.hits()), (2, 1));
    // No connection is kept for the next request: it connects (and resolves) afresh.
    let ok = ok_server();
    let dns = FakeDns::with("app.test", &[&["127.0.0.1"]]);
    let mut rig = rig_with(&[format!("http://app.test:{}", ok.addr.port())], true, "", "", Some(dns.clone()), None);
    for _ in 0..3 {
        assert_eq!(rig.get(&format!("http://app.test:{}/", ok.addr.port())).0, 200);
    }
    assert_eq!((dns.lookups.load(Ordering::SeqCst), ok.hits()), (3, 3));
}

#[test]
fn a_flood_is_limited_per_call_per_plugin_and_for_the_whole_server() {
    let ok = ok_server();
    let mut rig = rig(&[ok.pattern()], true);
    // One call asks for fifty: two are alive at once at most, the rest are told "full".
    let codes = rig.ask((0..50).map(|_| request("GET", &ok.url("/"), &[], b"")).collect());
    let tickets = codes.iter().filter(|c| **c > 0).count();
    assert_eq!(tickets, limits::HTTP_IN_FLIGHT as usize, "{codes:?}");
    assert!(codes.iter().filter(|c| **c <= 0).all(|c| *c == E_FULL as i64), "{codes:?}");
    rig.wait(tickets);
    // Over the rate: once the answers are in, a second can start more, but ten a second is the end.
    let mut accepted = tickets;
    let started = Instant::now();
    for _ in 0..30 {
        let codes = rig.ask(vec![request("GET", &ok.url("/"), &[], b"")]);
        if codes[0] > 0 {
            accepted += 1;
            rig.wait(accepted);
        }
    }
    assert!(accepted <= limits::HTTP_PER_SECOND as usize * (started.elapsed().as_secs() as usize + 1), "{accepted} requests in {:?}", started.elapsed());
    assert!(rig.stats().refused >= 48);

    // The server as a whole: a hanging server, forty requests offered to the workers at once.
    let hang = serve(|s, _| while s.read(&mut [0u8; 1]).map(|n| n > 0).unwrap_or(false) {});
    let policy = Arc::new(Policy { patterns: vec![super::net::Pattern::parse(&hang.pattern(), true).unwrap()], local: true, timeout: Duration::from_millis(300), max_response: 1000 });
    let mut net = Net::default();
    let counter: Arc<std::sync::atomic::AtomicU32> = Arc::default();
    let fetch = |n| Fetch { ticket: n, method: "GET".into(), url: hang.url("/"), headers: vec![], body: vec![], shown: hang.pattern() };
    for n in 0..40 {
        net.submit("probe", fetch(n), policy.clone(), counter.clone());
    }
    let early = net.take(100);
    let busy = early.iter().filter(|d| d.error == "the server is busy with other requests").count();
    assert!(busy >= 40 - limits::HTTP_QUEUE - limits::HTTP_WORKERS, "{busy} of 40 turned away at once");
    let mut all: Vec<Done> = early;
    let t = Instant::now();
    while all.len() < 40 {
        all.extend(net.take(100));
        assert!(t.elapsed() < Duration::from_secs(30));
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(all.iter().all(|d| d.status == 0 && (d.error == "timed out" || d.error.contains("busy"))), "{:?}", all.iter().map(|d| d.error.clone()).collect::<Vec<_>>());
    assert_eq!(counter.load(Ordering::SeqCst), 0, "everything that was alive is counted out");
    // The server counts a connection until it notices the client hung up, so the margin is wide; 36 were let in.
    assert!(hang.peak.load(Ordering::SeqCst) <= 2 * limits::HTTP_WORKERS, "{} connections at once", hang.peak.load(Ordering::SeqCst));
}

#[test]
fn config_values_go_into_the_request_and_nowhere_else() {
    let server = ok_server();
    let secret = "SECRET-WEBHOOK-7391";
    let manifest = "[[config]]\nkey = \"hook\"\ndescription = \"the path of the hook\"\n[[config]]\nkey = \"region\"\nsecret = false\n";
    let mut rig = rig_with(&[server.pattern()], true, "", manifest, None, None);
    // The admin sets the values; the host reads them at the next scan.
    config::set(&rig.dir, "probe", "hook", Some(secret)).unwrap();
    config::set(&rig.dir, "probe", "region", Some("eu-1")).unwrap();
    rig.host.rescan(&mut Default::default(), false);
    rig.natively();
    let codes = rig.ask(vec![request("POST", &server.url("/hook/${config.hook}?r=${config.region}"), &[("Authorization", "Bot ${config.hook}")], b"{}")]);
    assert!(codes[0] > 0, "{codes:?}");
    let (_, status, error, body, headers) = rig.wait(1)[0].clone();
    assert_eq!((status, error.as_str()), (200, ""));
    let head = server.heads.lock().unwrap()[0].clone();
    assert!(head.starts_with(&format!("POST /hook/{secret}?r=eu-1 ")) && head.contains(&format!("Bot {secret}")), "{head}");
    // What the plugin gets, what the host lists and logs, and what is printed: not the value.
    let p = &rig.host.plugins[0];
    let listing = admin::plugin_json(p).to_string();
    assert!(!listing.contains(secret) && listing.contains("eu-1") && listing.contains("\"hook\""), "{listing}");
    let line = net::log_line(&Done { plugin: "probe".into(), method: "POST".into(), shown: format!("http://127.0.0.1:{}", server.addr.port()), status: 200, millis: 3, ..Default::default() });
    assert!(!line.contains(secret) && !line.contains("/hook") && line.contains("POST http://127.0.0.1"), "{line}");
    let action = format!("{:?}", Action::Http(Fetch { ticket: 1, method: "GET".into(), url: format!("https://x.example/{secret}"), headers: vec![("Authorization".into(), secret.into())], body: vec![], shown: "https://x.example".into() }));
    assert!(!action.contains(secret), "{action}");
    assert!(!format!("{body:?}{headers:?}{error}").contains(secret));
    // A value nobody set is an error the plugin hears, not an empty string in the URL.
    config::set(&rig.dir, "probe", "hook", None).unwrap();
    rig.host.rescan(&mut Default::default(), false);
    rig.natively();
    assert_eq!(rig.ask(vec![request("GET", &server.url("/hook/${config.hook}"), &[], b"")]), vec![E_BAD as i64]);
    // A change of a value restarts the plugin with the new one (a plugin is its files, its grant and its values).
    assert_ne!(listing, admin::plugin_json(&rig.host.plugins[0]).to_string());
}

#[test]
fn an_https_request_to_a_server_that_does_not_speak_tls_fails_cleanly() {
    let server = serve(|s, _| respond(s, "200 OK", &[], b"plain"));
    let mut rig = rig(&[format!("https://127.0.0.1:{}", server.addr.port())], true);
    let (status, error, _) = rig.get(&format!("https://127.0.0.1:{}/", server.addr.port()));
    assert_eq!(status, 0);
    assert!(!error.is_empty() && !error.contains("127.0.0.1"), "{error}");
}

#[test]
fn a_plugin_removed_with_a_request_in_flight_leaves_nothing_behind() {
    let hang = serve(|_, _| std::thread::sleep(Duration::from_secs(2)));
    let mut rig = rig_with(&[hang.pattern()], true, "timeout_ms = 500", "", None, None);
    let in_flight = rig.host.plugins[0].net.as_ref().unwrap().in_flight.clone();
    assert!(rig.ask(vec![request("GET", &hang.url("/"), &[], b"")])[0] > 0);
    assert_eq!(in_flight.load(Ordering::SeqCst), 1);
    assert!(rig.host.net_waiting());
    std::fs::remove_dir_all(rig.dir.join("probe")).unwrap();
    let mut saved = Default::default();
    rig.host.rescan(&mut saved, false);
    assert!(rig.host.plugins.is_empty());
    // The answer arrives for nobody and is dropped; the count goes back to zero.
    let t = Instant::now();
    while rig.host.net_waiting() {
        rig.host.deliver_http(&mut rig.world, 0);
        assert!(t.elapsed() < Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(in_flight.load(Ordering::SeqCst), 0);
    assert!(rig.seen.lock().unwrap().is_empty());
}

#[test]
fn a_module_asks_for_a_request_and_hears_its_answer_through_the_abi() {
    let server = ok_server();
    let bytes = request("GET", &server.url("/abi"), &[("X-From", "wasm")], b"");
    let data: String = bytes.iter().map(|b| format!("\\{b:02x}")).collect();
    let imports = "(import \"pcds\" \"http_request\" (func $http (param i32 i32) (result i64)))";
    // Chat (4): ask. HttpDone (12): the payload begins with the ticket (8 bytes) and the status
    // (4); the module says "hello" when the status is 200.
    let body = format!("(if (i32.eq (local.get $kind) (i32.const 4)) (then (drop (call $http (i32.const 512) (i32.const {})))))\n(if (i32.eq (local.get $kind) (i32.const 12)) (then (if (i32.eq (i32.load (i32.add (local.get $ptr) (i32.const 8))) (i32.const 200)) (then (drop (call $say (i32.const 0) (i32.const 5)))))))\n(i32.const 0)", bytes.len());
    let wat = module(&body, &format!("(data (i32.const 512) \"{data}\")")).replacen("(module", &format!("(module {imports}"), 1);
    let dir = fresh_dir("net-abi");
    install(&dir, "caller", &wat, &["event.chat", "net.http", "net.http.local", "chat.say"], &format!("[events]\nsubscribe = [\"chat\"]\n[net]\nallow = [\"{}\"]\ntimeout_ms = 3000", server.pattern()), None);
    let mut host = boot(&dir);
    assert!(host.plugins[0].running(), "{:?}", host.plugins[0].status);
    let mut world = Mock::default();
    host.run(&mut world, &Event::Chat { client: 1, name: "Ann", text: "go" }, 0);
    assert_eq!(host.plugins[0].stats.http_requests, 1);
    let t = Instant::now();
    while world.applied.is_empty() {
        host.deliver_http(&mut world, 0);
        assert!(t.elapsed() < Duration::from_secs(10), "no answer");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(world.applied, vec![("caller".to_string(), Action::Say("hello".into()))]);
    let head = server.heads.lock().unwrap()[0].to_ascii_lowercase();
    assert!(head.starts_with("get /abi http/1.1") && head.contains("x-from: wasm"), "{head}");
    assert_eq!((host.plugins[0].stats.http_answered, host.plugins[0].stats.faults), (1, 0));
    // A request longer than the limit is refused with a code, not trapped.
    let big = module("(if (i32.eq (local.get $kind) (i32.const 4)) (then (call $http (i32.const 0) (i32.const 2000000)) drop)) (i32.const 0)", "").replacen("(module", &format!("(module {imports}"), 1).replace("(memory (export \"memory\") 1)", "(memory (export \"memory\") 40)");
    let dir2 = fresh_dir("net-abi2");
    install(&dir2, "big", &big, &["event.chat", "net.http"], "[events]\nsubscribe = [\"chat\"]\n[net]\nallow = [\"https://discord.com\"]", None);
    let mut host2 = boot(&dir2);
    host2.run(&mut world, &Event::Chat { client: 1, name: "Ann", text: "go" }, 0);
    assert_eq!((host2.plugins[0].stats.faults, host2.plugins[0].stats.refused), (0, 1));
}

/// A resolver that checks nothing: with it the connector's re-check is the only thing in the way.
#[derive(Debug)]
struct Lax(SocketAddr);

impl ureq::unversioned::resolver::Resolver for Lax {
    fn resolve(&self, _: &ureq::http::Uri, _: &ureq::config::Config, _: ureq::unversioned::transport::NextTimeout) -> Result<ureq::unversioned::resolver::ResolvedSocketAddrs, ureq::Error> {
        let mut out: ureq::unversioned::resolver::ResolvedSocketAddrs = ureq::unversioned::resolver::ArrayVec::from_fn(|_| self.0);
        out.push(self.0);
        Ok(out)
    }
}

#[test]
fn the_connector_refuses_an_address_the_resolver_let_through() {
    let server = ok_server();
    let policy = Arc::new(Policy { patterns: vec![super::net::Pattern::parse(&server.pattern(), true).unwrap()], local: false, timeout: Duration::from_secs(5), max_response: 1000 });
    let agent = net::agent_with(&policy, Lax(server.addr), Instant::now() + Duration::from_secs(5), "probe");
    let err = agent.get(server.url("/")).call().unwrap_err();
    assert!(matches!(&err, ureq::Error::Other(o) if o.to_string().contains("may not reach")), "{err}");
    assert_eq!(server.hits(), 0, "not a connection was made");
    // With the local capability the very same agent connects.
    let local = Arc::new(Policy { local: true, ..(*policy).clone() });
    let agent = net::agent_with(&local, Lax(server.addr), Instant::now() + Duration::from_secs(5), "probe");
    assert_eq!(agent.get(server.url("/")).call().unwrap().status().as_u16(), 200);
}

/// The Discord example, as compiled: its module is the one in examples/plugins, with the host of its
/// URL swapped for a server on the loopback (same length, so the module is the same module), and the
/// manifest pointed at it.
#[test]
fn the_discord_example_batches_its_lines_retries_a_failure_and_keeps_the_webhook_out_of_sight() {
    let from = Path::new(env!("CARGO_MANIFEST_DIR")).join("../examples/plugins/discord-notify");
    let attempts = Arc::new(AtomicUsize::new(0));
    let counted = attempts.clone();
    // The first request is answered 500, the next 204.
    let server = serve(move |s, _| {
        if counted.fetch_add(1, Ordering::SeqCst) == 0 { respond(s, "500 Internal Server Error", &[], b"oops") } else { respond(s, "204 No Content", &[], b"") }
    });
    let mut wasm = std::fs::read(from.join("plugin.wasm")).unwrap_or_else(|e| panic!("{e}: scripts/build-plugin-examples builds the module"));
    let old = b"https://discord.com/api/webhooks/${config.webhook}".to_vec();
    let start = format!("http://127.0.0.1:{}/", server.addr.port());
    let tail = "/${config.webhook}";
    let fill = old.len() - start.len() - tail.len();
    let new = format!("{start}{}{tail}", "w".repeat(fill));
    assert_eq!(new.len(), old.len());
    let at: Vec<usize> = wasm.windows(old.len()).enumerate().filter(|(_, w)| *w == old.as_slice()).map(|(i, _)| i).collect();
    assert_eq!(at.len(), 1, "the URL is in the module once");
    wasm[at[0]..at[0] + old.len()].copy_from_slice(new.as_bytes());
    let dir = fresh_dir("net-discord");
    let folder = dir.join("discord-notify");
    std::fs::create_dir_all(&folder).unwrap();
    let manifest = std::fs::read_to_string(from.join("plugin.toml")).unwrap();
    let manifest = manifest.replace("allow = [\"https://discord.com\"]", &format!("allow = [\"{}\"]", server.pattern())).replace("\"net.http\"]", "\"net.http\", \"net.http.local\"]");
    std::fs::write(folder.join("plugin.toml"), manifest).unwrap();
    std::fs::write(folder.join("plugin.wasm"), wasm).unwrap();
    config::set(&dir, "discord-notify", "webhook", Some("123456/SECRET-TOKEN")).unwrap();
    approve(&dir, "discord-notify", None).unwrap();
    let mut host = boot(&dir);
    assert!(host.plugins[0].running(), "{:?} {:?}", host.plugins[0].status, host.plugins[0].warnings);
    let mut world = Mock::default();
    let tick = |ms: i64| Event::Tick { game_ms: ms, tick: 1, players: 1, since_ms: 5000 };
    let run = |host: &mut Host, world: &mut Mock, e: &Event<'_>| host.run(world, e, if let Event::Tick { game_ms, .. } = e { *game_ms } else { 5000 });
    run(&mut host, &mut world, &Event::PlayerJoin { client: 1, name: "Ann", steam_id: "" });
    run(&mut host, &mut world, &Event::PlayerLeave { client: 1, name: "Bo \"B\"" });
    run(&mut host, &mut world, &Event::Command { client: 1, name: "Ann", command: "report", args: vec!["the", "door"], text: "!report the door is stuck" });
    assert_eq!(world.applied.len(), 1, "!report answered the asker");
    assert_eq!(server.hits(), 0, "nothing is sent before the tick");
    let pump = |host: &mut Host, world: &mut Mock, hits: usize| {
        let t = Instant::now();
        while server.hits() < hits || host.net_waiting() {
            host.deliver_http(world, 5000);
            assert!(t.elapsed() < Duration::from_secs(10), "no answer");
            std::thread::sleep(Duration::from_millis(5));
        }
    };
    run(&mut host, &mut world, &tick(5000));
    pump(&mut host, &mut world, 1);
    let head = server.heads.lock().unwrap()[0].clone();
    assert!(head.starts_with(&format!("POST /{}/123456/SECRET-TOKEN ", "w".repeat(fill))), "{head}");
    assert!(head.to_ascii_lowercase().contains("content-type: application/json"), "{head}");
    // The 500 left the lines for the next tick; that one's 204 takes them.
    run(&mut host, &mut world, &tick(10_000));
    pump(&mut host, &mut world, 2);
    run(&mut host, &mut world, &tick(15_000));
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(server.hits(), 2, "nothing left to send");
    let p = &host.plugins[0];
    assert_eq!((p.stats.http_requests, p.stats.http_answered, p.stats.http_failed, p.stats.faults), (2, 2, 0, 0));
    // What the server got the second time is the same message: joins, leaves, the report, escaped.
    let mut saved = Default::default();
    host.checkpoint(&mut saved, None);
    let blob = &saved.states["discord-notify"];
    assert_eq!((blob.version, &blob.bytes[..4]), (1, &[0, 0, 0, 0][..]), "the buffer is empty and part of the state");
    // The webhook is in no listing.
    let listing = admin::plugin_json(&host.plugins[0]).to_string();
    assert!(!listing.contains("SECRET-TOKEN") && listing.contains("\"webhook\""), "{listing}");
}

#[test]
fn check_says_where_a_plugin_may_reach_runs_it_on_answers_and_sends_nothing() {
    let report = check::check_folder(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../examples/plugins/discord-notify"), None);
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    let facts = report.facts.join("\n");
    for expected in ["may send HTTP requests to: https://discord.com", "config webhook: the end of the webhook's URL", "(a secret)", "signed by the key d6d9b10b298c53f9fe73f4c9d8570e69, which you do not trust", "http_done sample (an answer of 200): ok", "http_done sample (an answer of 500): ok", "http_done sample (a request that failed): ok"] {
        assert!(facts.contains(expected), "{expected:?} not in\n{facts}");
    }
    // A module that calls http_request without asking for it is an error; one that asks for it and never calls is a warning.
    let dir = fresh_dir("net-check");
    let imports = "(import \"pcds\" \"http_request\" (func $http (param i32 i32) (result i64)))";
    write_plugin(&dir, "caller", &module("(drop (call $http (i32.const 0) (i32.const 0))) (i32.const 0)", "").replacen("(module", &format!("(module {imports}"), 1), &["event.chat"], "[events]\nsubscribe = [\"chat\"]");
    let report = check::check_folder(&dir.join("caller"), None);
    assert!(report.errors.iter().any(|e| e.contains("needs net.http, which the manifest does not ask for")), "{:?}", report.errors);
    write_plugin(&dir, "idle", &module("(i32.const 0)", ""), &["event.chat", "net.http", "chat.say", "player.kick", "rules.write", "world.read"], "[events]\nsubscribe = [\"chat\"]\n[net]\nallow = [\"https://*.example.org\", \"http://plain.example.org\"]");
    let report = check::check_folder(&dir.join("idle"), None);
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    let warnings = report.warnings.join("\n");
    assert!(warnings.contains("asks for net.http, but the module imports nothing that needs it") && warnings.contains("a wildcard") && warnings.contains("plain http://"), "{warnings}");
    // `http_done` is not something to subscribe to.
    write_plugin(&dir, "sub", &module("(i32.const 0)", ""), &["net.http"], "[events]\nsubscribe = [\"http_done\"]\n[net]\nallow = [\"https://discord.com\"]");
    assert!(check::check_folder(&dir.join("sub"), None).errors.iter().any(|e| e.contains("not an event to subscribe to")));
}
