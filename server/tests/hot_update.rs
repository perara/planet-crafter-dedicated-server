//! Hot updates end to end (docs/hot-update.md), with the pcds program cargo built: a front and
//! two banks of this build on a new world, the load test's players playing through flips, a
//! flip whose new bank dies, a flip refused at its HELLO, a bank killed outside a flip, and
//! the updater against a release server on 127.0.0.1 (a good release, one whose sha256 does
//! not match, one of another front protocol) and a rollback. Needs game tables
//! (PCDS_TEST_DATA, else run/data/2.103; PCDS_TEST_DATA=none skips it, as the unit tests).
//! Steam is off: nothing here touches Steam.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream, UdpSocket};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

const PCDS: &str = env!("CARGO_BIN_EXE_pcds");

/// As the unit tests' (main.rs test_data): PCDS_TEST_DATA=none skips, tables not found fail.
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
    // A port free for both TCP and UDP right now.
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
    log: PathBuf,
    admin: u16,
    udp: u16,
}

impl Server {
    fn api(&self, method: &str, path: &str, body: Value) -> (u16, Value) {
        let body = body.to_string();
        let Ok(mut s) = TcpStream::connect(("127.0.0.1", self.admin)) else { return (0, Value::Null) };
        s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
        write!(s, "{method} {path} HTTP/1.1\r\nAuthorization: Bearer t\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        let mut answer = String::new();
        let _ = s.read_to_string(&mut answer);
        let status = answer.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
        let json = answer.split_once("\r\n\r\n").and_then(|(_, b)| serde_json::from_str(b).ok()).unwrap_or(Value::Null);
        (status, json)
    }

    fn log(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    /// Asks for an update (or a rollback) and waits for its outcome in GET /api/update.
    fn update(&self, body: Value) -> Value {
        let seq = self.update_begin(body);
        self.update_end(seq)
    }

    /// Asks for an update; its number.
    fn update_begin(&self, body: Value) -> u64 {
        let (status, answer) = self.api("POST", "/api/update", body.clone());
        assert_eq!(status, 200, "{body}: {answer}");
        answer["seq"].as_u64().unwrap()
    }

    /// The outcome of update `seq`, waited for.
    fn update_end(&self, seq: u64) -> Value {
        let deadline = Instant::now() + Duration::from_secs(120);
        while Instant::now() < deadline {
            let (_, u) = self.api("GET", "/api/update", json!({}));
            if let Some(done) = u["history"].as_array().and_then(|h| h.iter().find(|e| e["seq"] == seq)) {
                return done.clone();
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!("update {seq}: no outcome within 120 s\n{}", self.log());
    }

    /// The log once it holds `needle` (a failure after 90 s).
    fn wait_log(&self, needle: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(90);
        loop {
            let log = self.log();
            if log.contains(needle) {
                return log;
            }
            assert!(Instant::now() < deadline, "no \"{needle}\" in the log within 90 s\n{log}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// The world objects the live bank says it holds.
    fn objects(&self) -> u64 {
        self.api("GET", "/api/status", json!({})).1["objects"]["world"].as_u64().unwrap_or(0)
    }

    fn live(&self) -> Value {
        self.api("GET", "/api/status", json!({})).1["banks"]["live"].clone()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        // Only what this test started: the front, then its banks (the front's children).
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A banks folder whose two banks are this build.
fn banks(dir: &Path) -> PathBuf {
    let banks = dir.join("bin");
    for b in ["a", "b"] {
        std::fs::create_dir_all(banks.join(b)).unwrap();
        std::os::unix::fs::symlink(PCDS, banks.join(b).join("pcds")).unwrap();
        let info = Command::new(PCDS).arg("bank-info").output().unwrap();
        std::fs::write(banks.join(b).join("bank.json"), info.stdout).unwrap();
    }
    banks
}

fn start(dir: &Path, data: &str, banks: &Path, releases: &str) -> Server {
    start_with(Path::new(PCDS), dir, data, banks, releases, true)
}

/// A front of `program` (a release's or this build's) on the world in `dir`; `verify`:
/// PCDS_DEV=flip-verify, which compares a restored state with its own snapshot (only for banks of
/// one build: a release before this one writes a state this build reads and adds to).
fn start_with(program: &Path, dir: &Path, data: &str, banks: &Path, releases: &str, verify: bool) -> Server {
    start_env(program, dir, data, banks, releases, verify, &[])
}

/// start_with() and more settings of the front's environment. The ports are free when chosen and
/// a front of another test (or of someone else on this host) may take one before this one binds:
/// a front that stops for an address in use is started again on others.
fn start_env(program: &Path, dir: &Path, data: &str, banks: &Path, releases: &str, verify: bool, extra: &[(&str, &str)]) -> Server {
    for _ in 0..5 {
        if let Some(server) = start_once(program, dir, data, banks, releases, verify, extra) {
            return server;
        }
    }
    panic!("the server did not start in 5 tries (ports taken?)");
}

fn start_once(program: &Path, dir: &Path, data: &str, banks: &Path, releases: &str, verify: bool, extra: &[(&str, &str)]) -> Option<Server> {
    let (admin, udp) = (free_port(), free_port());
    let log = dir.join("front.log");
    let out = std::fs::File::create(&log).unwrap();
    // The front finds the banks in bin/ of PCDS_HOME.
    assert_eq!(banks, dir.join("bin"));
    let mut dev = String::from("test-faults");
    if verify {
        dev += ",flip-verify";
    }
    let mut command = Command::new(program);
    for (name, value) in extra {
        match *name {
            "PCDS_DEV" => dev = format!("{dev},{value}"),
            _ => {
                command.env(name, value);
            }
        }
    }
    // No backups: the rule save.backupsKept.
    std::fs::write(dir.join("rules.json"), "{\"save\": {\"backupsKept\": 0}}").unwrap();
    command.env("PCDS_DEV", &dev);
    let child = command
        .arg("serve")
        .arg(data)
        .arg(dir.join("world.json"))
        .env("PCDS_LISTEN", format!("127.0.0.1:{udp}"))
        .env("PCDS_ADMIN", format!("127.0.0.1:{admin}"))
        .env("PCDS_ADMIN_TOKEN", "t")
        .env("PCDS_STEAM", "off")
        .env("PCDS_HOME", dir)
        .env("PCDS_RELEASES", releases)
        .stdout(out.try_clone().unwrap())
        .stderr(out)
        .spawn()
        .unwrap();
    let mut server = Server { child, log, admin, udp };
    let deadline = Instant::now() + Duration::from_secs(60);
    while server.api("GET", "/api/status", json!({})).0 != 200 {
        if server.child.try_wait().unwrap().is_some() {
            let log = server.log();
            if log.to_lowercase().contains("address already in use") || log.contains("admin: 127.0.0.1") {
                return None;
            }
            panic!("the server ended at its start\n{log}");
        }
        // The admin port taken: the front goes on without an admin API.
        if server.log().contains("admin: 127.0.0.1:") {
            return None;
        }
        assert!(Instant::now() < deadline, "the server did not start\n{}", server.log());
        std::thread::sleep(Duration::from_millis(100));
    }
    Some(server)
}

fn load(server: &Server, data: &str, players: u32, seconds: u32) -> Child {
    Command::new(PCDS).args(["load", &format!("127.0.0.1:{}", server.udp), data, &players.to_string(), &seconds.to_string(), "200"]).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap()
}

/// `load` with players named `name`0, `name`1... (two groups of players may not share names).
fn load_named(server: &Server, data: &str, name: &str, players: u32, seconds: u32) -> Child {
    Command::new(PCDS).args(["load", &format!("127.0.0.1:{}", server.udp), data, &players.to_string(), &seconds.to_string(), "200", name]).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap()
}

fn finish(load: Child) -> (bool, String) {
    let out = load.wait_with_output().unwrap();
    (out.status.success(), String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr))
}

/// A release server on 127.0.0.1: GET path -> body, "BASE" in bodies replaced by its address.
fn release_server(routes: Vec<(String, Vec<u8>)>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let routes: Vec<(String, Vec<u8>)> = routes.into_iter().map(|(p, b)| (p, String::from_utf8(b.clone()).map(|t| t.replace("BASE", &base).into_bytes()).unwrap_or(b))).collect();
    std::thread::spawn(move || {
        for mut s in listener.incoming().flatten() {
            let mut reader = BufReader::new(s.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            while reader.read_line(&mut String::new()).is_ok_and(|n| n > 2) {}
            let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
            let (status, body) = routes.iter().find(|(p, _)| *p == path).map(|(_, b)| ("200 OK", b.clone())).unwrap_or(("404 Not Found", b"{}".to_vec()));
            let _ = write!(s, "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
            let _ = s.write_all(&body);
        }
    });
    format!("{base}/releases")
}

/// A release as `make dist` makes it: the tarball pcds-<tag>-linux-x86_64.tar.gz of the folder
/// pcds-<tag>-linux-x86_64/ (pcds, libsteam_api.so, web/), and its sha256 file.
fn release(dir: &Path, tag: &str, program: &Path, tamper: bool) -> Vec<(String, Vec<u8>)> {
    let name = format!("pcds-{tag}-linux-x86_64");
    let root = dir.join(&name);
    std::fs::create_dir_all(root.join("web")).unwrap();
    std::fs::copy(program, root.join("pcds")).unwrap();
    let lib = std::env::var("LD_LIBRARY_PATH").unwrap_or_default().split(':').map(|d| Path::new(d).join("libsteam_api.so")).chain([Path::new(PCDS).with_file_name("libsteam_api.so")]).find(|p| p.is_file());
    if let Some(lib) = lib {
        std::fs::copy(lib, root.join("libsteam_api.so")).unwrap();
    }
    std::fs::write(root.join("web/index.html"), "<html>release</html>").unwrap();
    let tarball = dir.join(format!("{name}.tar.gz"));
    assert!(Command::new("tar").arg("-czf").arg(&tarball).arg("-C").arg(dir).arg(&name).status().unwrap().success());
    let sum = Command::new("sha256sum").arg(&tarball).output().unwrap();
    let mut sum = String::from_utf8_lossy(&sum.stdout).split_whitespace().next().unwrap().to_string();
    if tamper {
        sum = format!("{}{}", if sum.starts_with('0') { '1' } else { '0' }, &sum[1..]);
    }
    let bytes = std::fs::read(&tarball).unwrap();
    std::fs::remove_dir_all(&root).unwrap();
    std::fs::remove_file(&tarball).unwrap();
    let file = format!("{name}.tar.gz");
    let json = json!({"tag_name": tag, "assets": [
        {"name": file, "browser_download_url": format!("BASE/download/{file}")},
        {"name": format!("{file}.sha256"), "browser_download_url": format!("BASE/download/{file}.sha256")},
    ]});
    vec![
        (format!("/releases/tags/{tag}"), json.to_string().into_bytes()),
        (format!("/download/{file}"), bytes),
        (format!("/download/{file}.sha256"), format!("{sum}  {file}\n").into_bytes()),
    ]
}

#[test]
fn the_world_moves_between_banks_with_nobody_disconnected() {
    let Some(data) = tables() else { return };
    let dir = std::env::temp_dir().join(format!("pcds-hot-update-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    assert!(Command::new(PCDS).arg("new-save").arg(dir.join("world.json")).arg(&data).stdout(Stdio::null()).status().unwrap().success());
    let banks = banks(&dir);
    // The releases: a good one (this build), one with a wrong sha256, one of another protocol.
    let fake = dir.join("fake-pcds");
    std::fs::write(&fake, "#!/bin/sh\n[ \"$1\" = bank-info ] && echo '{\"version\":\"9.9.7\",\"protocol\":999,\"format\":1,\"reads\":[0,1]}'\n").unwrap();
    std::fs::set_permissions(&fake, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let mut routes = release(&dir, "v9.9.9", Path::new(PCDS), false);
    routes.extend(release(&dir, "v9.9.8", Path::new(PCDS), true));
    routes.extend(release(&dir, "v9.9.7", &fake, false));
    // The latest is the good one.
    let latest = routes.iter().find(|r| r.0 == "/releases/tags/v9.9.9").unwrap().1.clone();
    routes.push(("/releases/latest".into(), latest));
    let releases = release_server(routes);
    let server = start(&dir, &data, &banks, &releases);
    let first = server.live()["bank"].as_str().unwrap().to_string();

    // 8 players play for 40 s; the world flips under them every few seconds.
    let players = load(&server, &data, 8, 40);
    std::thread::sleep(Duration::from_secs(4));
    let mut flips = Vec::new();
    for i in 0..6 {
        let done = server.update(json!({"rollback": true}));
        assert_eq!(done["ok"], true, "flip {i}: {done}\n{}", server.log());
        assert_eq!(done["verified"], true, "flip {i}: the new bank's state is the snapshot's");
        flips.push(done);
        std::thread::sleep(Duration::from_secs(2));
        if i == 2 {
            // A new bank that dies on its restore: the old one goes on.
            let failed = server.update(json!({"rollback": true, "failForTest": "restore"}));
            assert_eq!(failed["ok"], false);
            assert!(failed["message"].as_str().unwrap().contains("the running bank went on"), "{failed}");
            // One that speaks another protocol: refused at its HELLO, nothing stopped.
            let refused = server.update(json!({"rollback": true, "failForTest": "protocol"}));
            let message = refused["message"].as_str().unwrap();
            assert!(refused["ok"] == false && message.contains("never stopped") && message.contains("front protocol 999") && message.contains("pcds upgrade --restart"), "{refused}");
        }
    }
    let (ok, said) = finish(players);
    assert!(ok, "every player stayed connected through the flips:\n{said}\n{}", server.log());
    assert!(said.contains("load: 8 players"), "{said}");
    // Six flips from the first bank: back in it, and run/bin/active says so.
    assert_eq!(server.live()["bank"], first.as_str());
    assert_eq!(std::fs::read_to_string(banks.join("active")).unwrap().trim(), first);
    let stood: Vec<f64> = flips.iter().map(|f| f["stoodMs"].as_f64().unwrap()).collect();
    println!("hot update: {} flips under 8 players, the world stood {stood:?} ms (with the state check); restores {:?} ms, snapshots {:?} bytes", flips.len(), flips.iter().map(|f| f["restoreMs"].clone()).collect::<Vec<_>>(), flips.iter().map(|f| f["snapshotBytes"].clone()).collect::<Vec<_>>());

    // A bank killed outside a flip: started again from the last snapshot (newer than the
    // save), its players told to join again; then players get in again.
    let players = load(&server, &data, 2, 6);
    std::thread::sleep(Duration::from_secs(3));
    // A ban after the last flip: in access.json, not in that flip's snapshot.
    assert_eq!(server.api("POST", "/api/ban", json!({"name": "Mallory"})).0, 200);
    let pid = server.live()["pid"].as_u64().unwrap();
    // SAFETY: the live bank of the front this test started, by the pid that front reports.
    assert_eq!(unsafe { libc::kill(pid as i32, libc::SIGKILL) }, 0);
    let deadline = Instant::now() + Duration::from_secs(60);
    while server.live()["pid"].as_u64().is_none_or(|p| p == pid) {
        assert!(Instant::now() < deadline, "no new bank\n{}", server.log());
        std::thread::sleep(Duration::from_millis(100));
    }
    let (ok, said) = finish(players);
    assert!(!ok && said.contains("dropped"), "the crash dropped its players: {said}");
    let log = server.log();
    assert!(log.contains("ended (signal: 9") && log.contains("from the snapshot of the last flip") && log.contains("runs the world again"), "{log}");
    let (ok, said) = finish(load(&server, &data, 1, 3));
    assert!(ok, "a player gets in after the restart: {said}");
    let (_, status) = server.api("GET", "/api/status", json!({}));
    assert_eq!(status["access"]["bannedNames"], json!(["Mallory"]), "the ban outlived the restart from the snapshot");

    // Updates from the release server, into the bank that does not run.
    let standby = if server.live()["bank"] == "a" { "b" } else { "a" };
    let bad = server.update(json!({"version": "v9.9.8"}));
    assert!(bad["ok"] == false && bad["message"].as_str().unwrap().contains("does not match its published sha256"), "{bad}");
    let other = server.update(json!({"version": "v9.9.7"}));
    let message = other["message"].as_str().unwrap();
    assert!(other["ok"] == false && message.contains("front protocol 999") && message.contains("disconnects every player") && message.contains("Steam"), "{other}");
    assert!(banks.join(standby).join("pcds").is_symlink(), "a refused release leaves the bank as it was");
    let players = load(&server, &data, 4, 12);
    std::thread::sleep(Duration::from_secs(3));
    let good = server.update(json!({"version": "v9.9.9"}));
    assert_eq!(good["ok"], true, "{good}\n{}", server.log());
    assert!(good["message"].as_str().unwrap().contains("update to v9.9.9"), "{good}");
    assert_eq!(server.live()["bank"], standby);
    let installed: Value = serde_json::from_str(&std::fs::read_to_string(banks.join(standby).join("bank.json")).unwrap()).unwrap();
    assert_eq!(installed["tag"], "v9.9.9");
    assert!(installed["sha256"].as_str().is_some_and(|s| s.len() == 64));
    // And back: the bank before the update still holds its release.
    let back = server.update(json!({"rollback": true}));
    assert_eq!(back["ok"], true, "{back}");
    assert_ne!(server.live()["bank"], standby);
    // Noted: the automatic update does not bring v9.9.9 back.
    let noted: Value = serde_json::from_str(&std::fs::read_to_string(banks.join("rolled-back")).unwrap()).unwrap();
    assert_eq!(noted, json!({"bank": standby, "tag": "v9.9.9"}));
    let (ok, said) = finish(players);
    assert!(ok, "players stayed through the update and the rollback:\n{said}");
    // A check looks the latest release up and installs nothing.
    let live = server.live();
    let checked = server.update(json!({"check": true}));
    assert!(checked["ok"] == true && checked["check"] == true && checked["message"].as_str().unwrap().ends_with("is available"), "{checked}");
    let (_, u) = server.api("GET", "/api/update", json!({}));
    assert!(u["available"] == true && u["latest"].as_str().is_some_and(|t| t.starts_with("v9.9.")), "{u}");
    assert_eq!((server.live()["bank"].clone(), server.live()["pid"].clone()), (live["bank"].clone(), live["pid"].clone()), "no flip");

    // The status shows the banks and the update knobs.
    let (_, status) = server.api("GET", "/api/status", json!({}));
    assert_eq!((status["banks"]["frontProtocol"].as_u64(), status["update"]["auto"].as_bool()), (Some(3), Some(false)));
    assert_eq!(status["banks"]["installed"].as_array().unwrap().len(), 2);

    // Stopped through the API: the bank saves, the front ends.
    let before = std::fs::metadata(dir.join("world.json")).unwrap().modified().unwrap();
    assert_eq!(server.api("POST", "/api/stop", json!({})).0, 200);
    let mut server = server;
    let deadline = Instant::now() + Duration::from_secs(60);
    let status = loop {
        if let Some(s) = server.child.try_wait().unwrap() {
            break s;
        }
        assert!(Instant::now() < deadline, "the front did not end\n{}", server.log());
        std::thread::sleep(Duration::from_millis(100));
    };
    assert!(status.success(), "{status}\n{}", server.log());
    assert!(std::fs::metadata(dir.join("world.json")).unwrap().modified().unwrap() > before, "saved at the stop");
    println!("{}", server.log().lines().filter(|l| l.starts_with("flip ") || l.contains("restarting") || l.starts_with("update")).collect::<Vec<_>>().join("\n"));
    drop(server);

    // Stopped while a bank starts again after a crash: no bank holds the world, so the last
    // flip's snapshot (newer than the save) is kept beside the save, and the next start runs it.
    let server = start(&dir, &data, &banks, &releases);
    assert_eq!(server.update(json!({"rollback": true}))["ok"], true, "{}", server.log());
    let pid = server.live()["pid"].as_u64().unwrap();
    // SAFETY: the live bank of the front this test started, by the pid that front reports.
    assert_eq!(unsafe { libc::kill(pid as i32, libc::SIGKILL) }, 0);
    let deadline = Instant::now() + Duration::from_secs(30);
    while !server.log().contains("restarting bank") {
        assert!(Instant::now() < deadline, "no restart\n{}", server.log());
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(server.api("POST", "/api/stop", json!({})).0, 200);
    let mut server = server;
    let status = server.child.wait().unwrap();
    let kept = dir.join("world.json.snapshot");
    assert!(status.success() && kept.is_file() && server.log().contains("is kept in"), "{status}\n{}", server.log());
    drop(server);
    let server = start(&dir, &data, &banks, &releases);
    assert!(server.log().contains("the world is the one kept in"), "{}", server.log());
    assert_eq!(server.api("POST", "/api/stop", json!({})).0, 200);
    let mut server = server;
    assert!(server.child.wait().unwrap().success());
    let time = |p: &Path| std::fs::metadata(p).unwrap().modified().unwrap();
    assert!(time(&dir.join("world.json")) > time(&kept), "saved at the stop: the next start removes the kept snapshot");
    drop(server);

    // The command line with the server stopped: a release into the standby bank, made the
    // active one for the next start; status shows both banks; a rollback points back.
    let home = dir.join("cli");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::rename(&banks, home.join("bin")).unwrap();
    std::fs::write(home.join("bin/active"), "a\n").unwrap();
    let pcds = |args: &[&str]| {
        let out = Command::new(PCDS).args(args).env("PCDS_HOME", &home).env("PCDS_UNIT", format!("pcds-hot-update-test-{}", std::process::id())).env("PCDS_RELEASES", &releases).output().unwrap();
        (out.status.success(), String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr))
    };
    let (ok, said) = pcds(&["upgrade", "v9.9.9"]);
    assert!(ok && said.contains("v9.9.9 installed in bank b (sha256 checked)"), "{said}");
    assert!(!home.join("bin/rolled-back").exists(), "an update asked for clears the rollback note");
    assert_eq!(std::fs::read_to_string(home.join("bin/active")).unwrap().trim(), "b");
    let (ok, said) = pcds(&["status"]);
    assert!(ok && said.contains("state=stopped") && said.contains("bank b: pcds") && said.contains("(v9.9.9)") && said.contains("[active]"), "{said}");
    let (ok, said) = pcds(&["upgrade", "v9.9.8"]);
    assert!(!ok && said.contains("does not match its published sha256"), "{said}");
    let (ok, said) = pcds(&["upgrade", "--rollback"]);
    assert!(ok && said.contains("bank a"), "{said}");
    assert_eq!(std::fs::read_to_string(home.join("bin/active")).unwrap().trim(), "a");
    let noted: Value = serde_json::from_str(&std::fs::read_to_string(home.join("bin/rolled-back")).unwrap()).unwrap();
    assert_eq!(noted, json!({"bank": "b", "tag": "v9.9.9"}));
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A stop whose save fails (the save's path made a folder): the bank keeps the world as a
/// snapshot beside the save and ends with 1, the front says so and ends with 1, and the next
/// start runs that snapshot.
#[test]
fn a_stop_whose_save_fails_keeps_the_world() {
    let Some(data) = tables() else { return };
    let dir = std::env::temp_dir().join(format!("pcds-savefail-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let save = dir.join("world.json");
    assert!(Command::new(PCDS).arg("new-save").arg(&save).arg(&data).stdout(Stdio::null()).status().unwrap().success());
    let banks = banks(&dir);
    let server = start(&dir, &data, &banks, "http://127.0.0.1:9/releases");
    std::fs::rename(&save, dir.join("world.json.away")).unwrap();
    std::fs::create_dir(&save).unwrap();
    let (status, answer) = server.api("POST", "/api/save", json!({}));
    assert!(status == 500 && answer["error"].as_str().is_some_and(|e| e.contains("world.json")), "{status} {answer}");
    let (_, s) = server.api("GET", "/api/status", json!({}));
    assert_eq!(s["lastSave"]["ok"], false, "{}", s["lastSave"]);
    assert_eq!(server.api("POST", "/api/stop", json!({})).0, 200);
    let mut server = server;
    let status = server.child.wait().unwrap();
    let log = server.log();
    assert!(!status.success() && log.contains("it is kept in") && log.contains("at the stop: the world may not be saved"), "{status}\n{log}");
    drop(server);
    std::fs::remove_dir(&save).unwrap();
    std::fs::rename(dir.join("world.json.away"), &save).unwrap();
    let server = start(&dir, &data, &banks, "http://127.0.0.1:9/releases");
    assert!(server.log().contains("the world is the one kept in"), "{}", server.log());
    assert_eq!(server.api("POST", "/api/stop", json!({})).0, 200);
    let mut server = server;
    assert!(server.child.wait().unwrap().success(), "{}", server.log());
    drop(server);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A kept snapshot on other game tables (a game update after a stop with the world in no bank)
/// is moved aside, and the world starts from the save.
#[test]
fn a_kept_snapshot_that_can_never_run_is_set_aside() {
    let Some(data) = tables() else { return };
    let dir = std::env::temp_dir().join(format!("pcds-unusable-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    assert!(Command::new(PCDS).arg("new-save").arg(dir.join("world.json")).arg(&data).stdout(Stdio::null()).status().unwrap().success());
    std::thread::sleep(Duration::from_millis(20));
    std::fs::write(dir.join("world.json.snapshot"), "{\"format\":1,\"game_version\":\"0.0.1\"}\n{}").unwrap();
    let server = start(&dir, &data, &banks(&dir), "http://127.0.0.1:9/releases");
    assert!(server.log().contains("can never run (it is on game \"0.0.1\"'s tables"), "{}", server.log());
    let names: Vec<String> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().to_string()).collect();
    assert!(!names.contains(&"world.json.snapshot".to_string()) && names.iter().any(|n| n.starts_with("world.json.snapshot.unusable-")), "{names:?}");
    assert_eq!(server.api("POST", "/api/stop", json!({})).0, 200);
    let mut server = server;
    assert!(server.child.wait().unwrap().success(), "{}", server.log());
    drop(server);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// The admin stops the world: players are let go, joins refused, the page stays. Another world
/// runs, is remembered in server.env, and a start runs it.
#[test]
fn the_world_stops_starts_and_another_one_runs() {
    let Some(data) = tables() else { return };
    let dir = std::env::temp_dir().join(format!("pcds-worlds-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    for w in ["world", "other"] {
        assert!(Command::new(PCDS).arg("new-save").arg(dir.join(format!("{w}.json"))).arg(&data).stdout(Stdio::null()).status().unwrap().success());
    }
    std::fs::write(dir.join("server.env"), "PCDS_WORLD=world\n").unwrap();
    let banks = banks(&dir);
    let server = start(&dir, &data, &banks, "http://127.0.0.1:9/releases");
    let players = load(&server, &data, 2, 20);
    std::thread::sleep(Duration::from_secs(3));
    let (status, answer) = server.api("POST", "/api/world/stop", json!({}));
    assert_eq!(status, 200, "{answer}");
    let (ok, said) = finish(players);
    assert!(!ok && said.contains("dropped"), "the stop let its players go: {said}");
    let (_, s) = server.api("GET", "/api/status", json!({}));
    assert_eq!((s["state"].as_str(), s["save"].as_str()), (Some("stopped"), Some("world")), "{s}");
    assert_eq!(server.api("POST", "/api/save", json!({})).0, 409, "nothing to save while stopped");
    // No bank runs: an update or a rollback says so (not "a flip is under way"), the update
    // status stays readable, and the banks say the phase (the dashboard disables its buttons by it).
    for body in [json!({"rollback": true}), json!({"version": "v9.9.9"}), json!({"onlyNewer": true}), json!({"check": true})] {
        let (status, answer) = server.api("POST", "/api/update", body.clone());
        let why = answer["error"].as_str().unwrap_or("");
        assert!(status == 409 && answer["stopped"] == true && why.contains("the world is stopped") && why.contains("--restart") && !why.contains("under way"), "{body}: {status} {answer}");
    }
    assert_eq!(server.api("GET", "/api/update", json!({})).0, 200);
    assert_eq!(server.api("GET", "/api/status", json!({})).1["banks"]["phase"], "stopped");
    let (ok, said) = finish(load(&server, &data, 1, 3));
    assert!(!ok, "a join is refused while stopped: {said}");
    // Another world, from the stopped state; then back, from a running one.
    let (status, answer) = server.api("POST", "/api/world/start", json!({"world": "other"}));
    assert_eq!((status, answer["world"].as_str()), (200, Some("other")), "{answer}\n{}", server.log());
    assert!(std::fs::read_to_string(dir.join("server.env")).unwrap().contains("PCDS_WORLD=other"));
    let (_, s) = server.api("GET", "/api/status", json!({}));
    assert_eq!((s["state"].as_str(), s["save"].as_str()), (Some("running"), Some("other")));
    // Running again: an update works again (a flip between the banks).
    let done = server.update(json!({"rollback": true}));
    assert_eq!(done["ok"], true, "{done}\n{}", server.log());
    let players = load(&server, &data, 2, 12);
    std::thread::sleep(Duration::from_secs(3));
    let (status, answer) = server.api("POST", "/api/world/switch", json!({"world": "world"}));
    assert_eq!((status, answer["world"].as_str()), (200, Some("world")), "{answer}\n{}", server.log());
    let (ok, said) = finish(players);
    assert!(!ok, "the switch let the other world's players go: {said}");
    assert_eq!(server.api("POST", "/api/world/switch", json!({"world": "nowhere"})).0, 400);
    assert_eq!(server.api("POST", "/api/world/switch", json!({"world": "world"})).0, 409, "it runs already");
    let (ok, said) = finish(load(&server, &data, 1, 3));
    assert!(ok, "players join the world that runs now: {said}");
    assert_eq!(server.api("POST", "/api/stop", json!({})).0, 200);
    let mut server = server;
    assert!(server.child.wait().unwrap().success(), "{}", server.log());
    drop(server);
    std::fs::remove_dir_all(&dir).unwrap();
}

// ---- the release before: what a hot update from the last release meets -----------------------
//
// Every flip above is between banks of this build, which write and read the same state. This one
// runs the last release's program, a different binary of the same snapshot format, front protocol
// and tables format (`make gate`, tools/flip-gate.sh, which skips the test when they differ: that
// release is a restart install). PCDS_PREVIOUS_RELEASE is the folder of its tarball (its pcds
// beside libsteam_api.so). The test is ignored, so a plain `cargo test` does not need a download.

/// The program of the release before, from PCDS_PREVIOUS_RELEASE (a failure when it is not given).
fn previous_release() -> PathBuf {
    let dir = std::env::var("PCDS_PREVIOUS_RELEASE").unwrap_or_else(|_| panic!("PCDS_PREVIOUS_RELEASE: the folder of the previous release (its pcds in it): `make gate` fetches it"));
    let program = Path::new(&dir).join("pcds");
    assert!(program.is_file(), "no pcds in PCDS_PREVIOUS_RELEASE={dir}");
    program
}

/// `save` (a new world) with one of every building the game has: every constructible group of
/// the tables, in a field beside the start. A bank that loads it makes every kind of machine
/// there is, so its snapshot holds the state of each (an algae generator among them). Nothing
/// of a real world: the tables' own group names, on a new save. How many were placed.
fn rich_world(save: &Path, data: &str) -> usize {
    let gamedata: Value = serde_json::from_str(&std::fs::read_to_string(format!("{data}/gamedata.json")).unwrap()).unwrap();
    let mut groups: Vec<String> = gamedata["assets"].as_object().unwrap().iter().filter(|(k, _)| k.starts_with("GroupDataConstructible:")).filter_map(|(_, v)| v["id"].as_str().map(str::to_string)).collect();
    groups.sort();
    // The game's save: sections divided by @, the world objects one of them (a gId in each), divided by |.
    let text = std::fs::read_to_string(save).unwrap();
    let mut sections: Vec<String> = text.split('@').map(str::to_string).collect();
    let at = sections.iter().position(|s| s.contains("\"gId\"")).expect("a section of world objects");
    let first: Value = serde_json::from_str(sections[at].split('|').next().unwrap().trim_start_matches('\u{feff}')).unwrap();
    let planet = first["planet"].clone();
    for (i, group) in groups.iter().enumerate() {
        sections[at] += &format!("|{{\"id\":{},\"gId\":\"{group}\",\"pos\":\"{},20,{}\",\"rot\":\"0,0,0,1\",\"planet\":{planet}}}", 400_000_000 + i, 300 + (i % 17) * 30, 300 + (i / 17) * 30);
    }
    std::fs::write(save, sections.join("@")).unwrap();
    groups.len()
}

/// Banks a and b made of two programs (their bank-info as bank.json), a the active one.
fn banks_of(dir: &Path, a: &Path, b: &Path) -> PathBuf {
    let banks = dir.join("bin");
    for (bank, program) in [("a", a), ("b", b)] {
        std::fs::create_dir_all(banks.join(bank)).unwrap();
        std::os::unix::fs::symlink(program, banks.join(bank).join("pcds")).unwrap();
        let info = Command::new(program).arg("bank-info").output().unwrap();
        assert!(info.status.success(), "{} bank-info", program.display());
        std::fs::write(banks.join(bank).join("bank.json"), info.stdout).unwrap();
    }
    std::fs::write(banks.join("active"), "a\n").unwrap();
    banks
}

fn rich_dir(name: &str, data: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pcds-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    assert!(Command::new(PCDS).arg("new-save").arg(dir.join("world.json")).arg(data).stdout(Stdio::null()).status().unwrap().success());
    let placed = rich_world(&dir.join("world.json"), data);
    assert!(placed > 200, "{placed} groups placed");
    dir
}

/// The release before runs a world with one of every building and players; this build's bank
/// takes the world over from it, live: the restore works, the players stay, the world goes on,
/// and the way back works too.
#[test]
#[ignore = "needs the release before: make gate"]
fn a_hot_update_from_the_release_before_works() {
    let Some(data) = tables() else { panic!("PCDS_TEST_DATA=none: the gate needs game tables") };
    let previous = previous_release();
    let dir = rich_dir("previous-release", &data);
    let candidate = PathBuf::from(PCDS);
    let version = |program: &Path| serde_json::from_slice::<Value>(&Command::new(program).arg("bank-info").output().unwrap().stdout).unwrap()["version"].clone();
    let banks = banks_of(&dir, &previous, &candidate);
    let before = Command::new(&previous).arg("bank-info").output().unwrap();
    let before: Value = serde_json::from_slice(&before.stdout).unwrap();
    // The front is the release before too: what runs on a server when its owner updates.
    let server = start_with(&previous, &dir, &data, &banks, "http://127.0.0.1:9/releases", false);
    assert_eq!(server.live()["version"], before["version"], "the release before runs the world");
    let players = load(&server, &data, 6, 30);
    std::thread::sleep(Duration::from_secs(8));
    let (_, status) = server.api("GET", "/api/status", json!({}));
    let objects = status["objects"]["world"].as_u64().unwrap();
    assert!(objects > 250, "one of every building is in the world: {}", status["objects"]);
    let done = server.update(json!({"rollback": true}));
    assert_eq!(done["ok"], true, "the flip from pcds {} into this build failed: {done}\n{}", before["version"], server.log());
    assert_eq!(server.live()["version"], version(&candidate), "this build runs the world");
    let (ok, said) = finish(players);
    assert!(ok, "every player stayed connected through the flip:\n{said}\n{}", server.log());
    let (_, status) = server.api("GET", "/api/status", json!({}));
    assert!(status["objects"]["world"].as_u64().unwrap() >= objects, "no building lost in the flip: {} before, {}", objects, status["objects"]);
    // The world saves from its new bank, and goes on.
    assert_eq!(server.api("POST", "/api/save", json!({})).0, 200, "{}", server.log());
    let log = server.log();
    assert!(!log.contains("restore failed") && !log.contains("panicked"), "{log}");
    // And back, which is a rollback of the update just made: the release before reads this world.
    let back = server.update(json!({"rollback": true}));
    assert_eq!(back["ok"], true, "the way back into pcds {}: {back}\n{}", before["version"], server.log());
    assert_eq!(server.api("POST", "/api/stop", json!({})).0, 200);
    let mut server = server;
    assert!(server.child.wait().unwrap().success(), "{}", server.log());
    println!("gate: pcds {} -> pcds {} and back, {objects} world objects, the world stood {} ms", before["version"], version(&candidate), done["stoodMs"]);
    drop(server);
    std::fs::remove_dir_all(&dir).unwrap();
}

// ---- a flip, and what kills the processes in it ------------------------------------------------
//
// Each test stops the flip in one of its phases (a bank that holds its HELLO or its READY:
// PCDS_DEV=bank-fail=slow-hello and slow-ready, by "failForTest"; a bank frozen with SIGSTOP for the
// phase in which the running bank snapshots), kills a process with SIGKILL there, and checks the
// world: saved or resumed, one bank running or none left, nobody orphaned, run/bin/active right.

fn signal(pid: u64, sig: i32) {
    // SAFETY: a bank of the front this test started, by the pid the front reported or logged.
    unsafe { libc::kill(pid as i32, sig) };
}

fn alive(pid: u64) -> bool {
    // A zombie waits for its parent: it is gone as far as the world is concerned.
    std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|s| s.rsplit(')').next().is_some_and(|rest| !rest.trim_start().starts_with('Z')))
}

/// The pids among `pids` still running after 40 s.
fn survivors(pids: &[u64]) -> Vec<u64> {
    let deadline = Instant::now() + Duration::from_secs(40);
    while Instant::now() < deadline && pids.iter().any(|p| alive(*p)) {
        std::thread::sleep(Duration::from_millis(50));
    }
    pids.iter().copied().filter(|p| alive(*p)).collect()
}

/// The pid of the bank the last flip in the log started.
fn loading_pid(log: &str) -> u64 {
    let line = log.lines().rev().find(|l| l.starts_with("flip to bank")).expect("a flip started");
    line.split("pid ").nth(1).and_then(|r| r.split_whitespace().next()).and_then(|p| p.parse().ok()).expect("its pid")
}

/// The world objects in the save (every one has a gId).
fn saved_objects(dir: &Path) -> usize {
    std::fs::read_to_string(dir.join("world.json")).unwrap().matches("\"gId\"").count()
}

fn world_dir(name: &str, data: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pcds-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    assert!(Command::new(PCDS).arg("new-save").arg(dir.join("world.json")).arg(data).stdout(Stdio::null()).status().unwrap().success());
    dir
}

fn active_bank(banks: &Path) -> String {
    std::fs::read_to_string(banks.join("active")).map(|s| s.trim().to_string()).unwrap_or_else(|_| "a".into())
}

/// The front is killed (SIGKILL) in `phase` of a flip. The banks find it gone and save the
/// world; none is left running; the world has what it had; a new front runs it.
fn front_killed_in(phase: &str) {
    let Some(data) = tables() else { return };
    let dir = world_dir(&format!("front-{phase}"), &data);
    let banks = banks(&dir);
    let releases = "http://127.0.0.1:9/releases";
    let mut server = start(&dir, &data, &banks, releases);
    let players = load(&server, &data, 3, 25);
    std::thread::sleep(Duration::from_secs(5));
    assert_eq!(server.api("POST", "/api/save", json!({})).0, 200);
    let saved = saved_objects(&dir);
    let old = server.live()["pid"].as_u64().unwrap();
    let (mut frozen, new) = (None, 0);
    let new = match phase {
        "starting" => {
            server.update_begin(json!({"rollback": true, "failForTest": "slow-hello"}));
            loading_pid(&server.wait_log("holding its hello"))
        }
        "snapshotting" => {
            signal(old, libc::SIGSTOP);
            frozen = Some(old);
            server.update_begin(json!({"rollback": true}));
            loading_pid(&server.wait_log("is checked; the running bank is told"))
        }
        "restoring" => {
            server.update_begin(json!({"rollback": true, "failForTest": "slow-ready"}));
            loading_pid(&server.wait_log("holding its ready"))
        }
        _ => {
            assert_eq!(server.update(json!({"rollback": true}))["ok"], true, "{}", server.log());
            server.live()["pid"].as_u64().unwrap_or(new)
        }
    };
    let promoted = phase == "promoted";
    let before = std::fs::metadata(dir.join("world.json")).unwrap().modified().unwrap();
    server.child.kill().unwrap();
    server.child.wait().unwrap();
    if let Some(pid) = frozen {
        signal(pid, libc::SIGCONT);
    }
    let left = survivors(&[old, new]);
    assert!(left.is_empty(), "{phase}: banks left running after the front was killed: {left:?}\n{}", server.log());
    assert!(std::fs::metadata(dir.join("world.json")).unwrap().modified().unwrap() > before, "{phase}: a bank saved the world when it found the front gone\n{}", server.log());
    assert!(saved_objects(&dir) >= saved, "{phase}: nothing lost");
    assert_eq!(active_bank(&banks), if promoted { "b" } else { "a" }, "{phase}: run/bin/active");
    let _ = finish(players);
    // A front again, on the saved world.
    drop(server);
    let again = start(&dir, &data, &banks, releases);
    assert!(again.objects() as usize >= saved.min(1), "{phase}: the world runs again");
    assert_eq!(again.api("POST", "/api/stop", json!({})).0, 200);
    let mut again = again;
    assert!(again.child.wait().unwrap().success(), "{}", again.log());
    drop(again);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_front_killed_while_the_new_bank_loads_loses_nothing() {
    front_killed_in("starting");
}

#[test]
fn a_front_killed_while_the_running_bank_snapshots_loses_nothing() {
    front_killed_in("snapshotting");
}

#[test]
fn a_front_killed_while_the_new_bank_restores_loses_nothing() {
    front_killed_in("restoring");
}

#[test]
fn a_front_killed_just_after_a_flip_loses_nothing() {
    front_killed_in("promoted");
}

/// A bank is killed (SIGKILL) in `case`'s phase of a flip: the flip is off and the running bank
/// goes on (players stay), or, when the running bank itself is the one killed, the world
/// starts again from the last save or snapshot and the players are told to join again.
fn bank_killed_in(case: &str) {
    let Some(data) = tables() else { return };
    let dir = world_dir(&format!("bank-{case}"), &data);
    let banks = banks(&dir);
    let server = start(&dir, &data, &banks, "http://127.0.0.1:9/releases");
    let players = load(&server, &data, 3, 25);
    std::thread::sleep(Duration::from_secs(5));
    assert_eq!(server.api("POST", "/api/save", json!({})).0, 200);
    let saved = saved_objects(&dir);
    let before = server.objects();
    let old = server.live()["pid"].as_u64().unwrap();
    let stays = |server: &Server| assert_eq!(server.live()["pid"].as_u64(), Some(old), "the running bank went on\n{}", server.log());
    match case {
        "new-loading" => {
            let seq = server.update_begin(json!({"rollback": true, "failForTest": "slow-hello"}));
            signal(loading_pid(&server.wait_log("holding its hello")), libc::SIGKILL);
            let done = server.update_end(seq);
            assert!(done["ok"] == false && done["message"].as_str().unwrap().contains("the new bank ended"), "{done}");
            stays(&server);
            let (ok, said) = finish(players);
            assert!(ok, "the players stayed: {said}");
        }
        "new-restoring" => {
            let seq = server.update_begin(json!({"rollback": true, "failForTest": "slow-ready"}));
            signal(loading_pid(&server.wait_log("holding its ready")), libc::SIGKILL);
            let done = server.update_end(seq);
            assert!(done["ok"] == false && done["message"].as_str().unwrap().contains("the new bank ended") && done["message"].as_str().unwrap().contains("the running bank went on"), "{done}");
            stays(&server);
            assert!(server.log().contains("bank: resumed"), "{}", server.log());
            let (ok, said) = finish(players);
            assert!(ok, "the players stayed: {said}");
            assert!(server.objects() >= before, "the world went on");
            // A flip works after it.
            assert_eq!(server.update(json!({"rollback": true}))["ok"], true, "{}", server.log());
        }
        "running-snapshotting" => {
            signal(old, libc::SIGSTOP);
            let seq = server.update_begin(json!({"rollback": true}));
            server.wait_log("is checked; the running bank is told");
            signal(old, libc::SIGKILL);
            let done = server.update_end(seq);
            assert!(done["ok"] == false && done["message"].as_str().unwrap().contains("the running bank ended"), "{done}");
            // No bank holds the world: the active bank starts again, from the save (no flip
            // before this one, so no snapshot), and the players are told to join again.
            let deadline = Instant::now() + Duration::from_secs(60);
            while server.live()["pid"].as_u64().is_none_or(|p| p == old) {
                assert!(Instant::now() < deadline, "no new bank\n{}", server.log());
                std::thread::sleep(Duration::from_millis(100));
            }
            let log = server.log();
            assert!(log.contains("restarting bank a") && log.contains("from the save") && log.contains("runs the world again"), "{log}");
            let (ok, said) = finish(players);
            assert!(!ok && said.contains("dropped"), "the crash dropped its players: {said}");
            assert!(server.objects() as usize >= saved.min(1), "the world is back");
            assert_eq!(active_bank(&banks), "a");
            assert!(survivors(&[old]).is_empty());
        }
        _ => {
            // The new bank ends 1 s after its READY: the flip is done, then the world starts
            // again in that bank from the snapshot of the flip.
            let done = server.update(json!({"rollback": true, "failForTest": "die-after-ready"}));
            assert_eq!(done["ok"], true, "{done}\n{}", server.log());
            let log = server.wait_log("runs the world again");
            assert!(log.contains("from the snapshot of the last flip"), "{log}");
            let (ok, said) = finish(players);
            assert!(!ok && said.contains("dropped"), "{said}");
            assert!(server.objects() >= before.min(1), "the world is back");
            assert!(survivors(&[old]).is_empty(), "the bank before ended");
            assert_eq!(active_bank(&banks), "b");
        }
    }
    assert_eq!(server.api("POST", "/api/stop", json!({})).0, 200);
    let mut server = server;
    assert!(server.child.wait().unwrap().success(), "{}", server.log());
    drop(server);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_new_bank_killed_while_it_loads_leaves_the_world_where_it_is() {
    bank_killed_in("new-loading");
}

#[test]
fn a_new_bank_killed_while_it_restores_resumes_the_old_one() {
    bank_killed_in("new-restoring");
}

#[test]
fn a_running_bank_killed_while_it_snapshots_restarts_from_the_save() {
    bank_killed_in("running-snapshotting");
}

#[test]
fn a_new_bank_that_ends_after_its_flip_restarts_from_the_flips_snapshot() {
    bank_killed_in("new-after-ready");
}

/// What a player or the admin does while the world stands for a flip: held, then done.
#[test]
fn requests_and_players_during_a_flip_are_held_and_then_served() {
    let Some(data) = tables() else { return };
    let dir = world_dir("during-flip", &data);
    let banks = banks(&dir);
    let server = start(&dir, &data, &banks, "http://127.0.0.1:9/releases");
    let players = load(&server, &data, 2, 20);
    std::thread::sleep(Duration::from_secs(4));
    let old = server.live()["pid"].as_u64().unwrap();
    let seq = server.update_begin(json!({"rollback": true, "failForTest": "slow-ready"}));
    server.wait_log("holding its ready");
    // The world stands (about 2 s here): a status, a save, a ban and a new player wait for the new bank.
    let newcomer = load_named(&server, &data, "Late", 1, 8);
    let asked = Instant::now();
    let (status, s) = server.api("GET", "/api/status", json!({}));
    assert_eq!(status, 200, "{s}");
    assert_ne!(s["banks"]["live"]["pid"].as_u64(), Some(old), "answered by the bank that took the world");
    assert_eq!(server.api("POST", "/api/save", json!({})).0, 200);
    assert_eq!(server.api("POST", "/api/ban", json!({"name": "Mallory"})).0, 200);
    println!("a status asked during the flip's stand was answered after {:?}", asked.elapsed());
    let done = server.update_end(seq);
    assert_eq!(done["ok"], true, "{done}");
    let (ok, said) = finish(players);
    assert!(ok, "the players stayed: {said}");
    let (ok, said) = finish(newcomer);
    assert!(ok, "the player who came during the flip got in: {said}\n{}", server.log());
    let (_, s) = server.api("GET", "/api/status", json!({}));
    assert_eq!(s["access"]["bannedNames"], json!(["Mallory"]));
    assert_eq!(server.api("POST", "/api/stop", json!({})).0, 200);
    let mut server = server;
    assert!(server.child.wait().unwrap().success(), "{}", server.log());
    drop(server);
    std::fs::remove_dir_all(&dir).unwrap();
}


/// A release server whose releases change while it runs: GET path -> body from `routes`, "BASE"
/// replaced by its address; its releases URL.
fn live_release_server(routes: std::sync::Arc<std::sync::Mutex<Vec<(String, Vec<u8>)>>>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let served = base.clone();
    std::thread::spawn(move || {
        for mut s in listener.incoming().flatten() {
            let mut reader = BufReader::new(s.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            while reader.read_line(&mut String::new()).is_ok_and(|n| n > 2) {}
            let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
            let found = routes.lock().unwrap().iter().find(|(p, _)| *p == path).map(|(_, b)| String::from_utf8(b.clone()).map(|t| t.replace("BASE", &served).into_bytes()).unwrap_or_else(|_| b.clone()));
            let (status, body) = found.map(|b| ("200 OK", b)).unwrap_or(("404 Not Found", b"{}".to_vec()));
            let _ = write!(s, "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
            let _ = s.write_all(&body);
        }
    });
    format!("{base}/releases")
}

/// The rule update.auto (the check every 4 s here: PCDS_DEV=check-seconds=4): a release later than the running one is installed and flipped to under
/// players; the same release is not fetched again; the release rolled back from is left out;
/// a later one is taken. The release's tag is not the program's version (v9.9.9 is this build,
/// which says 0.6.2): the bank that came from it is compared by its tag.
#[test]
fn the_automatic_update_takes_a_new_release_once_and_leaves_the_rolled_back_one() {
    let Some(data) = tables() else { return };
    let dir = world_dir("auto-update", &data);
    let banks = banks(&dir);
    let routes = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let publish = |tag: &str, latest: bool| {
        let mut r = release(&dir, tag, Path::new(PCDS), false);
        if latest {
            let json = r.iter().find(|x| x.0 == format!("/releases/tags/{tag}")).unwrap().1.clone();
            r.push(("/releases/latest".into(), json));
        }
        let mut routes = routes.lock().unwrap();
        routes.retain(|(p, _)| p != "/releases/latest");
        routes.extend(r);
    };
    publish("v9.9.9", true);
    let releases = live_release_server(routes.clone());
    let server = start_env(Path::new(PCDS), &dir, &data, &banks, &releases, true, &[("PCDS_DEV", "check-seconds=4")]);
    // The rule, live: the front hears of it from the bank.
    assert_eq!(server.api("POST", "/api/config", json!({"update.auto": true})).0, 200, "{}", server.log());
    let flips = |server: &Server| server.api("GET", "/api/status", json!({})).1["banks"]["flips"].as_u64().unwrap_or(0);
    let players = load(&server, &data, 3, 60);
    // The first check: v9.9.9 is later than 0.6.2: installed, flipped to.
    let deadline = Instant::now() + Duration::from_secs(60);
    while flips(&server) < 1 {
        assert!(Instant::now() < deadline, "no automatic update\n{}", server.log());
        std::thread::sleep(Duration::from_millis(200));
    }
    let installed: Value = serde_json::from_str(&std::fs::read_to_string(banks.join(active_bank(&banks)).join("bank.json")).unwrap()).unwrap();
    assert_eq!(installed["tag"], "v9.9.9", "{installed}");
    // Several checks later still one flip: the release that runs is not fetched again.
    std::thread::sleep(Duration::from_secs(14));
    assert_eq!(flips(&server), 1, "v9.9.9 installed once\n{}", server.log());
    assert_eq!(server.api("GET", "/api/update", json!({})).1["available"], false);
    // Rolled back: left out by the next checks.
    assert_eq!(server.update(json!({"rollback": true}))["ok"], true, "{}", server.log());
    std::thread::sleep(Duration::from_secs(10));
    assert_eq!(flips(&server), 2, "the release rolled back from is not installed again\n{}", server.log());
    assert!(server.log().contains("the release the server was rolled back from"), "{}", server.log());
    // A later release is taken, and the note is gone.
    publish("v9.9.10", true);
    let deadline = Instant::now() + Duration::from_secs(60);
    while flips(&server) < 3 {
        assert!(Instant::now() < deadline, "no automatic update to the later release\n{}", server.log());
        std::thread::sleep(Duration::from_millis(200));
    }
    let installed: Value = serde_json::from_str(&std::fs::read_to_string(banks.join(active_bank(&banks)).join("bank.json")).unwrap()).unwrap();
    assert_eq!(installed["tag"], "v9.9.10");
    assert!(!banks.join("rolled-back").exists(), "a later release ends the note");
    let (ok, said) = finish(players);
    assert!(ok, "the players stayed through the automatic flips and the rollback:\n{said}\n{}", server.log());
    assert_eq!(server.api("POST", "/api/stop", json!({})).0, 200);
    let mut server = server;
    assert!(server.child.wait().unwrap().success(), "{}", server.log());
    drop(server);
    std::fs::remove_dir_all(&dir).unwrap();
}


/// Two updates at once, a world stop and a shutdown of the world while an update is under way:
/// the second is refused and says why; the first is not disturbed.
#[test]
fn a_second_update_or_a_world_stop_while_an_update_is_under_way_is_refused() {
    let Some(data) = tables() else { return };
    let dir = world_dir("two-updates", &data);
    let banks = banks(&dir);
    let server = start(&dir, &data, &banks, "http://127.0.0.1:9/releases");
    let players = load(&server, &data, 2, 14);
    std::thread::sleep(Duration::from_secs(4));
    let first = server.update_begin(json!({"rollback": true, "failForTest": "slow-hello"}));
    server.wait_log("holding its hello");
    for body in [json!({"rollback": true}), json!({"version": "v9.9.9"}), json!({"check": true})] {
        let (status, answer) = server.api("POST", "/api/update", body.clone());
        assert!(status == 409 && answer["error"].as_str().is_some_and(|e| e.contains("under way")), "{body}: {status} {answer}");
    }
    let (status, answer) = server.api("POST", "/api/world/stop", json!({}));
    assert!(status == 409 && answer["error"].as_str().is_some_and(|e| e.contains("an update is under way")), "{status} {answer}");
    let (status, answer) = server.api("POST", "/api/world/switch", json!({"world": "world"}));
    assert!(status == 409 || status == 400, "{status} {answer}");
    let done = server.update_end(first);
    assert_eq!(done["ok"], true, "the first update was not disturbed: {done}\n{}", server.log());
    let (ok, said) = finish(players);
    assert!(ok, "{said}");
    // A flip of a paused world stays paused (the admin's pause is in the snapshot).
    assert_eq!(server.api("POST", "/api/pause", json!({"paused": true})).0, 200);
    assert_eq!(server.update(json!({"rollback": true}))["ok"], true, "{}", server.log());
    assert_eq!(server.api("GET", "/api/status", json!({})).1["held"], true, "still paused after the flip");
    assert_eq!(server.api("POST", "/api/pause", json!({"paused": false})).0, 200);
    assert_eq!(server.api("POST", "/api/stop", json!({})).0, 200);
    let mut server = server;
    assert!(server.child.wait().unwrap().success(), "{}", server.log());
    drop(server);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A save that fails (its path made a folder) does not stop an update: the flip works, the new bank
/// has the world and says its saves fail; with the path right again the next save works. Nothing
/// of the world is lost: the stop keeps what it cannot save beside the save.
#[test]
fn an_update_with_a_save_that_fails_loses_nothing_and_keeps_failing_honestly() {
    let Some(data) = tables() else { return };
    let dir = world_dir("save-fails", &data);
    let banks = banks(&dir);
    let server = start(&dir, &data, &banks, "http://127.0.0.1:9/releases");
    let players = load(&server, &data, 2, 12);
    std::thread::sleep(Duration::from_secs(4));
    let save = dir.join("world.json");
    std::fs::rename(&save, dir.join("world.json.away")).unwrap();
    std::fs::create_dir(&save).unwrap();
    assert_eq!(server.api("POST", "/api/save", json!({})).0, 500);
    assert_eq!(server.api("GET", "/api/status", json!({})).1["lastSave"]["ok"], false);
    let done = server.update(json!({"rollback": true}));
    assert_eq!(done["ok"], true, "{done}\n{}", server.log());
    let (ok, said) = finish(players);
    assert!(ok, "{said}");
    // The new bank has the world and its saves still fail, in words.
    let (status, answer) = server.api("POST", "/api/save", json!({}));
    assert!(status == 500 && answer["error"].as_str().is_some_and(|e| e.contains("world.json")), "{status} {answer}");
    // The path is right again: it saves.
    std::fs::remove_dir(&save).unwrap();
    std::fs::rename(dir.join("world.json.away"), &save).unwrap();
    assert_eq!(server.api("POST", "/api/save", json!({})).0, 200, "{}", server.log());
    assert_eq!(server.api("GET", "/api/status", json!({})).1["lastSave"]["ok"], true);
    assert_eq!(server.api("POST", "/api/stop", json!({})).0, 200);
    let mut server = server;
    assert!(server.child.wait().unwrap().success(), "{}", server.log());
    drop(server);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A release that reads other game tables, over the feed: refused before anything is installed
/// into a bank that runs, with the command that does it (pcds upgrade --restart).
#[test]
fn a_release_of_another_tables_format_is_refused_live_and_says_how() {
    let Some(data) = tables() else { return };
    let dir = world_dir("tables-format", &data);
    let banks = banks(&dir);
    let tables_format = serde_json::from_slice::<Value>(&Command::new(PCDS).arg("bank-info").output().unwrap().stdout).unwrap()["tablesFormat"].as_u64().unwrap();
    let fake = dir.join("fake-pcds");
    std::fs::write(&fake, format!("#!/bin/sh\n[ \"$1\" = bank-info ] && echo '{{\"version\":\"9.9.6\",\"protocol\":3,\"format\":3,\"reads\":[2,3],\"tablesFormat\":{}}}'\n", tables_format + 1)).unwrap();
    std::fs::set_permissions(&fake, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let releases = release_server(release(&dir, "v9.9.6", &fake, false));
    let server = start(&dir, &data, &banks, &releases);
    let done = server.update(json!({"version": "v9.9.6"}));
    let message = done["message"].as_str().unwrap();
    assert!(done["ok"] == false && message.contains(&format!("game tables of format {}", tables_format + 1)) && message.contains("pcds upgrade --restart v9.9.6") && message.contains("disconnects every player"), "{done}");
    assert_eq!(server.live()["bank"], "a", "nothing flipped");
    assert_eq!(server.api("POST", "/api/stop", json!({})).0, 200);
    let mut server = server;
    assert!(server.child.wait().unwrap().success());
    drop(server);
    std::fs::remove_dir_all(&dir).unwrap();
}


/// What a scripted player sees when it types `lines` in the chat (pcds probe, a real client over
/// the real socket): the whole of what the server sent back, as text.
fn say_in_chat(server: &Server, data: &str, dir: &Path, name: &str, lines: &[&str]) -> String {
    let mut steps = vec![json!({"wait": 600})];
    for l in lines {
        steps.push(json!({"rpc": "ChatHandler.SendChatMessageServerRpc", "on": "ChatHandler", "args": [l]}));
        steps.push(json!({"wait": 250}));
    }
    steps.push(json!({"wait": 600}));
    let script = dir.join(format!("{name}.json"));
    std::fs::write(&script, json!({"name": name, "steps": steps}).to_string()).unwrap();
    let out = Command::new(PCDS).args(["probe", &format!("127.0.0.1:{}", server.udp), data]).arg(&script).output().unwrap();
    format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr))
}

/// A plugin installed with the command line, running in a bank: a player's commands reach it, its
/// state goes through hot updates (the snapshot) and through a restart (the file beside it), and
/// the admin API tells how it does.
#[test]
fn a_plugins_state_goes_through_hot_updates_and_a_restart() {
    let Some(data) = tables() else { return };
    let dir = std::env::temp_dir().join(format!("pcds-plugins-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("home")).unwrap();
    assert!(Command::new(PCDS).arg("new-save").arg(dir.join("world.json")).arg(&data).stdout(Stdio::null()).status().unwrap().success());
    // The plugins folder is beside the rules file (here dir/rules.json): dir/plugins. The command
    // line, with PCDS_HOME = dir, installs and approves there.
    let cli = |args: &[&str]| {
        let out = Command::new(PCDS).arg("plugin").args(args).env("PCDS_HOME", &dir).env("HOME", dir.join("home")).env_remove("PCDS_UNIT").env_remove("PCDS_CONTAINER").output().unwrap();
        assert!(out.status.success(), "plugin {args:?}: {}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    };
    let example = Path::new(env!("CARGO_MANIFEST_DIR")).join("../examples/plugins");
    cli(&["install", example.join("hello-stats").to_str().unwrap()]);
    cli(&["enable", "hello-stats", "--yes"]);
    let banks = banks(&dir);
    let server = start(&dir, &data, &banks, "http://127.0.0.1:9/releases");
    let (status, list) = server.api("GET", "/api/plugins", json!({}));
    assert_eq!((status, list["plugins"][0]["id"].as_str(), list["plugins"][0]["status"].as_str()), (200, Some("hello-stats"), Some("running")), "{list}\n{}", server.log());
    assert!(server.log().contains("plugins: 1 running (hello-stats)"), "{}", server.log());

    // Commands in the chat of a real client: both counters go up.
    let said = say_in_chat(&server, &data, &dir, "First", &["!stats", "!uses"]);
    assert!(said.contains("Server on Prime: 1 players"), "!stats answered the asker: {said}");
    assert!(said.contains("!stats was used 1 times, !uses 1 times on this world"), "{said}");
    let (_, list) = server.api("GET", "/api/plugins", json!({}));
    let calls_before = list["plugins"][0]["stats"]["calls"].as_u64().unwrap();
    assert!(calls_before >= 3 && list["plugins"][0]["stats"]["faults"] == 0, "{list}");

    // A hot update, and another back: the snapshot carries the state; the new bank's plugin goes on.
    for (i, expect) in [(0, 2), (1, 3)] {
        let done = server.update(json!({"rollback": true}));
        assert_eq!((done["ok"].as_bool(), done["verified"].as_bool()), (Some(true), Some(true)), "flip {i}: {done}\n{}", server.log());
        let said = say_in_chat(&server, &data, &dir, &format!("After{i}"), &["!uses"]);
        assert!(said.contains(&format!("!stats was used 1 times, !uses {expect} times on this world")), "after flip {i}: {said}\n{}", server.log());
    }
    // The new bank's plugin is a new instance (its counters began again), running, no fault.
    let (_, list) = server.api("GET", "/api/plugins", json!({}));
    assert_eq!((list["plugins"][0]["status"].as_str(), list["plugins"][0]["stats"]["faults"].as_u64()), (Some("running"), Some(0)), "{list}");

    // A stop saves the world, and the plugin's state beside it; a new server picks it up.
    assert_eq!(server.api("POST", "/api/stop", json!({})).0, 200);
    let mut server = server;
    assert!(server.child.wait().unwrap().success(), "{}", server.log());
    assert!(dir.join("plugins/hello-stats/data/world.state").is_file());
    drop(server);
    let server = start(&dir, &data, &banks, "http://127.0.0.1:9/releases");
    let said = say_in_chat(&server, &data, &dir, "Restarted", &["!uses"]);
    assert!(said.contains("!stats was used 1 times, !uses 4 times on this world"), "{said}\n{}", server.log());

    // A file of the plugin changed on disk: the next reload shows the new files waiting for the admin,
    // while the approved version goes on; approving it again replaces it.
    let wasm = dir.join("plugins/hello-stats/plugin.wasm");
    let mut bytes = std::fs::read(&wasm).unwrap();
    bytes.extend_from_slice(&[0, 6, 4, b'n', b'o', b't', b'e', 0]);
    std::fs::write(&wasm, bytes).unwrap();
    let (status, _) = server.api("POST", "/api/plugins/reload", json!({}));
    let (_, list) = server.api("GET", "/api/plugins", json!({}));
    assert_eq!((status, list["plugins"][0]["status"].as_str()), (200, Some("running")), "{list}");
    assert!(list["plugins"][0]["notice"].as_str().is_some_and(|n| n.contains("wait for approval")), "{list}");
    let said = say_in_chat(&server, &data, &dir, "Waiting", &["!uses"]);
    assert!(said.contains("!stats was used 1 times, !uses 5 times"), "the approved version goes on: {said}");
    let (status, answer) = server.api("POST", "/api/plugins/enable", json!({"id": "hello-stats"}));
    assert_eq!((status, answer["plugin"]["status"].as_str()), (200, Some("running")), "{answer}");
    assert_eq!(server.api("POST", "/api/stop", json!({})).0, 200);
    let mut server = server;
    assert!(server.child.wait().unwrap().success(), "{}", server.log());
    drop(server);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Waits until `GET /api/plugins` satisfies `ok` (the folder scan runs every two seconds and acts
/// on a change it has seen twice), at most `secs`; the listing it last saw.
fn wait_for_plugins(server: &Server, secs: u64, ok: impl Fn(&Value) -> bool) -> Value {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let (_, list) = server.api("GET", "/api/plugins", json!({}));
        if ok(&list) || Instant::now() > deadline {
            assert!(ok(&list), "the plugin listing never became what was waited for: {list}\n{}", server.log());
            return list;
        }
        std::thread::sleep(Duration::from_millis(400));
    }
}

/// Plugins come and go while the server runs, with no reload asked and no restart: a folder dropped
/// in and approved starts by itself; switched off, it stops and its command is gone; changed
/// files wait for approval while the old version goes on, and approved they take over with the
/// state; one is switched off while the world is handed over to the other bank; a removed folder
/// is unloaded; a program reading the stream registers and leaves.
#[test]
fn plugins_load_and_unload_while_the_server_runs() {
    let Some(data) = tables() else { return };
    let dir = std::env::temp_dir().join(format!("pcds-liveplugins-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("home")).unwrap();
    assert!(Command::new(PCDS).arg("new-save").arg(dir.join("world.json")).arg(&data).stdout(Stdio::null()).status().unwrap().success());
    let cli = |args: &[&str]| {
        let out = Command::new(PCDS).arg("plugin").args(args).env("PCDS_HOME", &dir).env("HOME", dir.join("home")).env_remove("PCDS_UNIT").env_remove("PCDS_CONTAINER").output().unwrap();
        assert!(out.status.success(), "plugin {args:?}: {}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    };
    let banks = banks(&dir);
    // There is no plugins folder at all.
    let server = start(&dir, &data, &banks, "http://127.0.0.1:9/releases");
    assert!(!say_in_chat(&server, &data, &dir, "Before", &["!uses"]).contains("was used"));

    // A folder is dropped in, as an admin would copy it, and approved with the server not asked
    // (the command line here has no admin address to ask): the scan finds both.
    let example = Path::new(env!("CARGO_MANIFEST_DIR")).join("../examples/plugins/hello-stats");
    let to = dir.join("plugins/hello-stats");
    std::fs::create_dir_all(&to).unwrap();
    for f in ["plugin.toml", "plugin.wasm"] {
        std::fs::copy(example.join(f), to.join(f)).unwrap();
    }
    let list = wait_for_plugins(&server, 20, |l| l["plugins"][0]["status"] == "pending");
    assert_eq!(list["plugins"][0]["detail"], "discovered, awaiting approval: pcds plugin enable ID");
    assert!(!say_in_chat(&server, &data, &dir, "Waiting", &["!uses"]).contains("was used"), "nothing runs before the approval");
    cli(&["enable", "hello-stats", "--yes"]);
    wait_for_plugins(&server, 20, |l| l["plugins"][0]["status"] == "running");
    let said = say_in_chat(&server, &data, &dir, "Live", &["!stats", "!uses"]);
    assert!(said.contains("!stats was used 1 times, !uses 1 times on this world"), "{said}\n{}", server.log());
    let (_, list) = server.api("GET", "/api/plugins", json!({}));
    assert_eq!(list["runtime"]["instances"], 1);

    // Switched off (the file only; nobody asks the server): the command is gone, the state kept.
    cli(&["disable", "hello-stats"]);
    wait_for_plugins(&server, 20, |l| l["plugins"][0]["status"] == "disabled");
    assert!(!say_in_chat(&server, &data, &dir, "Off", &["!uses"]).contains("was used"));
    let (_, list) = server.api("GET", "/api/plugins", json!({}));
    assert_eq!((list["runtime"]["instances"].as_u64(), list["runtime"]["memoryPages"].as_u64()), (Some(0), Some(0)), "{list}");
    assert!(to.join("data/world.state").is_file(), "its last save was written");
    // On again: a new instance with that state.
    cli(&["enable", "hello-stats", "--yes"]);
    wait_for_plugins(&server, 20, |l| l["plugins"][0]["status"] == "running");
    let said = say_in_chat(&server, &data, &dir, "On", &["!uses"]);
    assert!(said.contains("!stats was used 1 times, !uses 2 times on this world"), "{said}");

    // Changed code: its files wait for approval, the approved version goes on; approved, the new
    // version takes over with the state (the same length of text, so the module stays valid).
    let wasm = to.join("plugin.wasm");
    let bytes = std::fs::read(&wasm).unwrap();
    let at = bytes.windows(16).position(|w| w == b"!stats was used ").expect("the module holds its text");
    let mut changed = bytes.clone();
    changed[at] = b'?';
    std::fs::write(&wasm, changed).unwrap();
    let list = wait_for_plugins(&server, 20, |l| l["plugins"][0]["notice"].is_string());
    assert_eq!(list["plugins"][0]["status"], "running");
    assert!(list["plugins"][0]["notice"].as_str().unwrap().contains("wait for approval"), "{list}");
    assert!(say_in_chat(&server, &data, &dir, "Old", &["!uses"]).contains("!stats was used 1 times, !uses 3 times"), "still the old version");
    let (status, answer) = server.api("POST", "/api/plugins/enable", json!({"id": "hello-stats"}));
    assert_eq!((status, answer["plugin"]["notice"].is_null()), (200, true), "{answer}");
    let said = say_in_chat(&server, &data, &dir, "New", &["!uses"]);
    assert!(said.contains("?stats was used 1 times, !uses 4 times on this world"), "new behaviour, state kept: {said}\n{}", server.log());

    // Switched off while the world is handed over to the other bank: the set that runs afterwards
    // is the folder's, and the plugin is off in the new bank.
    let admin = server.admin;
    let disabler = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(150));
        let body = json!({"id": "hello-stats"}).to_string();
        let mut s = TcpStream::connect(("127.0.0.1", admin)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
        write!(s, "POST /api/plugins/disable HTTP/1.1\r\nAuthorization: Bearer t\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        let mut answer = String::new();
        let _ = s.read_to_string(&mut answer);
        answer
    });
    let done = server.update(json!({"rollback": true}));
    let answered = disabler.join().unwrap();
    assert_eq!((done["ok"].as_bool(), done["verified"].as_bool()), (Some(true), Some(true)), "{done}\n{}", server.log());
    assert!(answered.starts_with("HTTP/1.1 200") || answered.starts_with("HTTP/1.1 409") || answered.starts_with("HTTP/1.1 500"), "{answered}");
    wait_for_plugins(&server, 20, |l| l["plugins"][0]["status"] == "disabled");
    assert!(!say_in_chat(&server, &data, &dir, "AfterFlip", &["!uses"]).contains("was used"));
    cli(&["enable", "hello-stats", "--yes"]);
    wait_for_plugins(&server, 20, |l| l["plugins"][0]["status"] == "running");
    assert!(say_in_chat(&server, &data, &dir, "Again", &["!uses"]).contains("!uses 5 times"), "the state went through the flip and the unload");

    // A program reading the stream registers and leaves.
    let (status, _) = server.api("POST", "/api/plugins/events", json!({"consumer": "tester", "types": ["chat"]}));
    assert_eq!(status, 200);
    let (_, list) = server.api("GET", "/api/plugins", json!({}));
    assert_eq!(list["stream"]["consumers"][0]["name"], "tester");
    server.api("POST", "/api/plugins/events", json!({"consumer": "tester", "unregister": true}));
    let (_, list) = server.api("GET", "/api/plugins", json!({}));
    assert!(list["stream"]["consumers"].as_array().unwrap().is_empty());

    // The folder removed: unloaded, nothing written back, no command.
    std::fs::remove_dir_all(&to).unwrap();
    wait_for_plugins(&server, 20, |l| l["plugins"].as_array().is_some_and(|p| p.is_empty()));
    assert!(!to.exists(), "its folder was not made again for its state");
    assert!(!say_in_chat(&server, &data, &dir, "Gone", &["!uses"]).contains("was used"));
    let log = server.log();
    for line in ["plugin hello-stats: pending", "plugin hello-stats: running", "plugin hello-stats: unloading (disabled)", "plugin hello-stats: disabled", "plugin hello-stats: unloading (its folder was removed)", "plugin hello-stats: removed"] {
        assert!(log.contains(line), "the log lacks {line:?}\n{log}");
    }
    assert_eq!(server.api("POST", "/api/stop", json!({})).0, 200);
    let mut server = server;
    assert!(server.child.wait().unwrap().success(), "{}", server.log());
    drop(server);
    std::fs::remove_dir_all(&dir).unwrap();
}


/// The flip as the live server does it, on grown worlds (tools/grow-world.py), with players
/// playing: the front, two banks, the real sockets. Prints the stand and where it went (the
/// front's timeline) per world.
///
///   PCDS_FLIPBENCH_SAVES=a.json,b.json [PCDS_FLIPBENCH_N=15] [PCDS_FLIPBENCH_PLAYERS=8] \
///     cargo test --release --test hot_update flipbench -- --ignored --nocapture
///
/// Both banks are this build and one server makes N flips in a row. The state check is off.
#[test]
#[ignore]
fn flipbench() {
    let Some(data) = tables() else { return };
    let var = |k: &str, d: usize| std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d);
    let (n, players) = (var("PCDS_FLIPBENCH_N", 15), var("PCDS_FLIPBENCH_PLAYERS", 8) as u32);
    let saves = std::env::var("PCDS_FLIPBENCH_SAVES").expect("flipbench: set PCDS_FLIPBENCH_SAVES");
    for save in saves.split(',') {
        let mut records: Vec<Value> = Vec::new();
        let dir = std::env::temp_dir().join(format!("pcds-flipbench-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::copy(save, dir.join("world.json")).unwrap();
        let banks = banks(&dir);
        std::fs::write(banks.join("active"), "a").unwrap();
        let server = start_with(Path::new(PCDS), &dir, &data, &banks, "http://127.0.0.1:9/releases", false);
        let play = (players > 0).then(|| load(&server, &data, players, (n * 3 + 10) as u32));
        std::thread::sleep(Duration::from_secs(3));
        for i in 0..=n {
            let done = server.update(json!({"rollback": true}));
            assert_eq!(done["ok"], true, "flip {i}: {done}\n{}", server.log());
            // The first flip warms nothing the next ones keep: dropped.
            if i > 0 {
                records.push(done);
            }
            std::thread::sleep(Duration::from_millis(1500));
        }
        if let Some(p) = play {
            drop(finish(p));
        }
        drop(server);
        let _ = std::fs::remove_dir_all(&dir);
        let spread = |f: &dyn Fn(&Value) -> Option<f64>| {
            let mut v: Vec<f64> = records.iter().filter_map(f).collect();
            v.sort_by(|a, b| a.partial_cmp(b).unwrap());
            if v.is_empty() {
                return "-".to_string();
            }
            format!("{:>8.2} {:>8.2} {:>8.2} {:>8.2}", v[v.len() / 2], v[((v.len() - 1) as f64 * 0.95).round() as usize], v[v.len() - 1], v[0])
        };
        println!("flipbench: {save}: {} flips, {players} players (ms: p50 / p95 / max / min); snapshot {} bytes", records.len(), records.last().map(|r| r["snapshotBytes"].to_string()).unwrap_or_default());
        println!("flipbench:   {:<18} {}", "stood", spread(&|r| r["stoodMs"].as_f64()));
        for key in ["handoffMs", "snapshotMs", "snapshotToFrontMs", "relayMs", "restoreToBankMs", "restoreMs", "readyToFrontMs", "releaseMs"] {
            println!("flipbench:   {:<18} {}", key.trim_end_matches("Ms"), spread(&|r| r["timeline"][key].as_f64()));
        }
        println!("flipbench:   {:<18} {}", "held frames", spread(&|r| r["held"].as_f64()));
        println!("flipbench:   {:<18} {}", "restore faults", spread(&|r| r["restoreFaults"].as_f64()));
    }
}
