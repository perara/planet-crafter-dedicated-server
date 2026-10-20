//! The admin API's answers, route by route, against a file (tests/golden/admin-api.txt): for each
//! request the status and the shape of the JSON that comes back (its keys, nested, and the type of
//! each value). A refactor that moves code between the front, the bank and admin.rs must not
//! change what a client of the API reads; if an answer changes on purpose, the file changes in
//! the same commit (delete it and run this test: it writes the new one and fails once).
//!
//! A real front and bank of this build on a new world, Steam off, on 127.0.0.1. Needs game tables
//! (PCDS_TEST_DATA, else run/data/2.103; PCDS_TEST_DATA=none skips it), as hot_update.rs does.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream, UdpSocket};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

const PCDS: &str = env!("CARGO_BIN_EXE_pcds");
const GOLDEN: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/golden/admin-api.txt");

fn tables() -> Option<String> {
    let dir = match std::env::var("PCDS_TEST_DATA").ok().filter(|v| !v.is_empty()) {
        Some(v) if v == "none" => return None,
        Some(dir) => dir,
        None => concat!(env!("CARGO_MANIFEST_DIR"), "/../run/data/2.103").into(),
    };
    assert!(Path::new(&dir).join("netdata.json").is_file(), "no game tables in {dir}: set PCDS_TEST_DATA to a tables directory, or to none to skip");
    Some(dir)
}

fn free_port() -> u16 {
    loop {
        let tcp = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = tcp.local_addr().unwrap().port();
        if UdpSocket::bind(("127.0.0.1", port)).is_ok() {
            return port;
        }
    }
}

struct Server {
    child: Child,
    admin: u16,
    log: PathBuf,
}

impl Server {
    /// The status and the body: JSON when it is, else its text.
    fn ask(&self, method: &str, path: &str, body: Value, token: bool) -> (u16, String, Option<Value>) {
        let body = body.to_string();
        let mut s = TcpStream::connect(("127.0.0.1", self.admin)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
        let auth = if token { "Authorization: Bearer t\r\n" } else { "" };
        write!(s, "{method} {path} HTTP/1.1\r\n{auth}Content-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        let mut answer = String::new();
        let _ = s.read_to_string(&mut answer);
        let status = answer.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
        let text = answer.split_once("\r\n\r\n").map(|(_, b)| b.to_string()).unwrap_or_default();
        let json = serde_json::from_str(&text).ok();
        (status, text, json)
    }

    fn log(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn start(dir: &Path, data: &str) -> Server {
    for _ in 0..5 {
        let (admin, udp) = (free_port(), free_port());
        let log = dir.join("front.log");
        let out = std::fs::File::create(&log).unwrap();
        // No backups (the rule save.backupsKept), and a web page for the first release's bank.
        std::fs::write(dir.join("rules.json"), "{\"save\": {\"backupsKept\": 0}}").unwrap();
        let child = Command::new(PCDS)
            .arg("serve")
            .arg(data)
            .arg(dir.join("world.json"))
            .env_remove("PCDS_GAME_DIR")
            .env("PCDS_LISTEN", format!("127.0.0.1:{udp}"))
            .env("PCDS_ADMIN", format!("127.0.0.1:{admin}"))
            .env("PCDS_ADMIN_TOKEN", "t")
            .env("PCDS_STEAM", "off")
            .env("PCDS_HOME", dir)
            .env("PCDS_RELEASES", "http://127.0.0.1:9/releases")
            .stdout(out.try_clone().unwrap())
            .stderr(out)
            .spawn()
            .unwrap();
        let mut server = Server { child, admin, log };
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if TcpStream::connect(("127.0.0.1", admin)).is_ok() && server.ask("GET", "/api/status", json!({}), true).0 == 200 {
                return server;
            }
            if server.child.try_wait().unwrap().is_some() {
                assert!(server.log().to_lowercase().contains("address already in use"), "the server ended at its start\n{}", server.log());
                break;
            }
            assert!(Instant::now() < deadline, "the server did not start\n{}", server.log());
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    panic!("the server did not start in 5 tries (ports taken?)");
}

/// The keys of a JSON value, nested, with the type of each leaf: `{a:num,b:{c:str},d:[bool]}`.
/// Keys are in the order the server wrote them; an array shows its first element.
fn shape(v: &Value) -> String {
    match v {
        Value::Null => "null".into(),
        Value::Bool(_) => "bool".into(),
        Value::Number(_) => "num".into(),
        Value::String(_) => "str".into(),
        Value::Array(a) => format!("[{}]", a.first().map(shape).unwrap_or_default()),
        Value::Object(o) => format!("{{{}}}", o.iter().map(|(k, v)| format!("{k}:{}", shape(v))).collect::<Vec<_>>().join(",")),
    }
}

#[test]
fn every_route_answers_with_the_keys_it_always_did() {
    let Some(data) = tables() else { return };
    let dir = std::env::temp_dir().join(format!("pcds-golden-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    assert!(Command::new(PCDS).arg("new-save").arg(dir.join("world.json")).arg(&data).stdout(Stdio::null()).status().unwrap().success());
    std::fs::write(dir.join("server.env"), "PCDS_WORLD=world\n").unwrap();
    let banks = dir.join("bin");
    for b in ["a", "b"] {
        std::fs::create_dir_all(banks.join(b).join("web")).unwrap();
        std::os::unix::fs::symlink(PCDS, banks.join(b).join("pcds")).unwrap();
        std::fs::write(banks.join(b).join("web/index.html"), "<html>page</html>").unwrap();
        let info = Command::new(PCDS).arg("bank-info").output().unwrap();
        std::fs::write(banks.join(b).join("bank.json"), info.stdout).unwrap();
    }
    let server = start(&dir, &data);
    let mut got: Vec<String> = Vec::new();
    let mut record = |what: &str, (status, text, json): (u16, String, Option<Value>)| {
        let body = match json {
            Some(v) => shape(&v),
            None => format!("text({})", text.len().min(1)),
        };
        got.push(format!("{what} -> {status} {body}"));
    };
    let ask = |method: &str, path: &str, body: Value| server.ask(method, path, body, true);

    // The page needs no token, the API does; a path nobody answers is a 404 from the bank.
    record("GET / (no token)", server.ask("GET", "/", json!({}), false));
    record("GET /api/status (no token)", server.ask("GET", "/api/status", json!({}), false));
    record("GET /api/nothing", ask("GET", "/api/nothing", json!({})));

    // The world runs: the bank answers, the front adds its part to the status.
    for (method, path, body) in [
        ("GET", "/api/status", json!({})),
        ("GET", "/api/ping", json!({})),
        ("GET", "/api/update", json!({})),
        ("GET", "/api/rules", json!({})),
        ("POST", "/api/rules", json!({})),
        ("GET", "/api/config", json!({})),
        ("POST", "/api/config", json!({})),
        ("GET", "/api/worlds", json!({})),
        ("GET", "/api/map", json!({})),
        ("GET", "/api/plugins", json!({})),
        ("POST", "/api/plugins/reload", json!({})),
        ("POST", "/api/plugins/enable", json!({})),
        ("POST", "/api/plugins/disable", json!({})),
        ("POST", "/api/plugins/events", json!({})),
        ("POST", "/api/pause", json!({"paused": false})),
        ("POST", "/api/pause", json!({})),
        ("POST", "/api/kick", json!({"id": 99})),
        ("POST", "/api/kick", json!({})),
        ("POST", "/api/ban", json!({"name": "Nobody"})),
        ("POST", "/api/unban", json!({"value": "Nobody"})),
        ("POST", "/api/unban", json!({})),
        ("POST", "/api/teleport", json!({"id": 99, "position": [0, 0, 0]})),
        ("POST", "/api/teleport", json!({})),
        ("POST", "/api/worlds/new", json!({"name": "golden"})),
        ("POST", "/api/worlds/new", json!({"name": "golden"})),
        ("POST", "/api/worlds/file", json!({"name": "golden"})),
        ("POST", "/api/worlds/file", json!({})),
        ("POST", "/api/worlds/restore", json!({"backup": "nothing", "name": "restored"})),
        ("POST", "/api/worlds/restore", json!({})),
        ("POST", "/api/worlds/remove", json!({"name": "golden"})),
        ("POST", "/api/worlds/remove", json!({"name": "world"})),
        ("POST", "/api/worlds/remove", json!({})),
        ("POST", "/api/backup", json!({})),
        ("POST", "/api/backup/s3", json!({})),
        ("POST", "/api/save", json!({})),
        // The HTTP thread's: the logs (no unit here: a note), the chat.
        ("GET", "/api/logs", json!({})),
        ("GET", "/api/chat/events?after=0", json!({})),
        ("POST", "/api/chat", json!({"text": "hello"})),
        ("POST", "/api/chat", json!({})),
        ("GET", "/api/chat/elsewhere", json!({})),
        // The front's: updates (a check against a release server that is not there is an outcome, not an answer).
        ("POST", "/api/update", json!({"check": true})),
    ] {
        record(&format!("{method} {path} {body}"), ask(method, path, body));
    }
    // The check's outcome is in the history; the update state with an entry.
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline && ask("GET", "/api/update", json!({})).2.is_some_and(|u| u["history"].as_array().is_none_or(|h| h.is_empty())) {
        std::thread::sleep(Duration::from_millis(100));
    }
    record("GET /api/update (after a check)", ask("GET", "/api/update", json!({})));

    // The world controls, and the front alone while the world is stopped.
    for (method, path, body) in [
        ("POST", "/api/world/start", json!({})),
        ("POST", "/api/world/switch", json!({})),
        ("POST", "/api/world/switch", json!({"world": "nowhere"})),
        ("POST", "/api/world/switch", json!({"world": "world"})),
        ("POST", "/api/world/stop", json!({})),
        ("POST", "/api/world/stop", json!({})),
        ("GET", "/api/status", json!({})),
        ("GET", "/api/worlds", json!({})),
        ("GET", "/api/update", json!({})),
        ("POST", "/api/update", json!({"check": true})),
        ("POST", "/api/save", json!({})),
        ("GET", "/api/map", json!({})),
        ("POST", "/api/world/start", json!({"world": 5})),
        ("POST", "/api/world/start", json!({"world": "nowhere"})),
        ("POST", "/api/world/switch", json!({})),
        ("POST", "/api/world/start", json!({})),
        ("GET", "/api/status", json!({})),
    ] {
        record(&format!("{method} {path} {body}"), ask(method, path, body));
    }
    record("POST /api/stop", ask("POST", "/api/stop", json!({})));
    let mut server = server;
    assert!(server.child.wait().unwrap().success(), "{}", server.log());
    drop(server);
    std::fs::remove_dir_all(&dir).unwrap();

    let got = got.join("\n") + "\n";
    match std::fs::read_to_string(GOLDEN) {
        Ok(want) => assert!(want == got, "the API answers differently from {GOLDEN}:\n{}", diff(&want, &got)),
        Err(_) => {
            std::fs::create_dir_all(Path::new(GOLDEN).parent().unwrap()).unwrap();
            std::fs::write(GOLDEN, &got).unwrap();
            panic!("{GOLDEN} did not exist: written; read it, and run the test again");
        }
    }
}

/// The lines that differ, side by side.
fn diff(want: &str, got: &str) -> String {
    let (w, g): (Vec<&str>, Vec<&str>) = (want.lines().collect(), got.lines().collect());
    (0..w.len().max(g.len())).filter(|i| w.get(*i) != g.get(*i)).map(|i| format!("{i}:\n  file: {}\n  now:  {}", w.get(i).unwrap_or(&""), g.get(i).unwrap_or(&""))).collect::<Vec<_>>().join("\n")
}
