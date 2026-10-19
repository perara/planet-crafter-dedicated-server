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
        let (status, answer) = self.api("POST", "/api/update", body.clone());
        assert_eq!(status, 200, "{body}: {answer}");
        let seq = answer["seq"].as_u64().unwrap();
        let deadline = Instant::now() + Duration::from_secs(120);
        while Instant::now() < deadline {
            let (_, u) = self.api("GET", "/api/update", json!({}));
            if let Some(done) = u["history"].as_array().and_then(|h| h.iter().find(|e| e["seq"] == seq)) {
                return done.clone();
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!("{body}: no outcome within 120 s\n{}", self.log());
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
/// PCDS_FLIP_VERIFY, which compares a restored state with its own snapshot (only for banks of
/// one build: a release before this one writes a state this build reads and adds to).
fn start_with(program: &Path, dir: &Path, data: &str, banks: &Path, releases: &str, verify: bool) -> Server {
    let (admin, udp) = (free_port(), free_port());
    let log = dir.join("front.log");
    let out = std::fs::File::create(&log).unwrap();
    let mut command = Command::new(program);
    if verify {
        command.env("PCDS_FLIP_VERIFY", "1");
    }
    let child = command
        .arg("serve")
        .arg(data)
        .arg(dir.join("world.json"))
        .env("PCDS_LISTEN", format!("127.0.0.1:{udp}"))
        .env("PCDS_ADMIN", format!("127.0.0.1:{admin}"))
        .env("PCDS_ADMIN_TOKEN", "t")
        .env("PCDS_STEAM", "off")
        .env("PCDS_BACKUPS", "0")
        .env("PCDS_BANKS", banks)
        .env("PCDS_RELEASES", releases)
        .env("PCDS_TEST_FAULTS", "1")
        .env("PCDS_ENV_FILE", dir.join("server.env"))
        .env("PCDS_ACCESS", dir.join("access.json"))
        .env("PCDS_RULES", dir.join("rules.json"))
        .stdout(out.try_clone().unwrap())
        .stderr(out)
        .spawn()
        .unwrap();
    let mut server = Server { child, log, admin, udp };
    let deadline = Instant::now() + Duration::from_secs(60);
    while server.api("GET", "/api/status", json!({})).0 != 200 {
        assert!(Instant::now() < deadline && server.child.try_wait().unwrap().is_none(), "the server did not start\n{}", server.log());
        std::thread::sleep(Duration::from_millis(100));
    }
    server
}

fn load(server: &Server, data: &str, players: u32, seconds: u32) -> Child {
    Command::new(PCDS).args(["load", &format!("127.0.0.1:{}", server.udp), data, &players.to_string(), &seconds.to_string(), "200"]).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap()
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
// Every flip above is between banks of this build, which write and read the same state. The first
// real update (0.6.0 to 0.6.1) failed on a field the release before did not write
// ("missing field water_only"), which only a state written by that release shows. These tests
// run the release before: PCDS_PREVIOUS_RELEASE is the folder of its tarball (its pcds beside
// libsteam_api.so; `make gate` fetches the last release that is not this version). They are
// ignored, so a plain `cargo test` does not need a download; `make gate` runs them with
// --ignored, where a missing release is a failure and not a skip.

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
/// takes the world over from it, live: the restore works, the players stay, the world goes on.
/// A release that declares it cannot (front protocol, game tables, snapshot formats it does not
/// read) is told so by the front before anything stops: that is a restart by design, and
/// said here, not a failure.
#[test]
#[ignore = "needs the release before: make gate"]
fn a_hot_update_from_the_release_before_works() {
    let Some(data) = tables() else { panic!("PCDS_TEST_DATA=none: the gate needs game tables") };
    let previous = previous_release();
    let dir = rich_dir("previous-release", &data);
    // The build the world is handed to: this one (PCDS_GATE_CANDIDATE: another program, to see
    // that the gate fails for a release that cannot take the release before's world).
    let candidate = std::env::var("PCDS_GATE_CANDIDATE").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from(PCDS));
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
    let message = done["message"].as_str().unwrap_or("");
    if done["ok"] == false && ["front restart", "game tables of format", "needs a restart"].iter().any(|w| message.contains(w)) {
        println!("gate: this build declares it cannot take over the world of pcds {} live (a restart by design): {message}", before["version"]);
        let _ = finish(players);
        std::fs::remove_dir_all(&dir).unwrap();
        return;
    }
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
    // And back, which is a rollback of the update just made: said, not asserted (a release that
    // adds a field to the state cannot be handed a world written without it live; the old bank
    // then goes on, and the way back is --rollback --restart).
    let back = server.update(json!({"rollback": true}));
    println!("gate: and back into pcds {}: {}", before["version"], if back["ok"] == true { "live".to_string() } else { back["message"].to_string() });
    assert_eq!(server.api("POST", "/api/stop", json!({})).0, 200);
    let mut server = server;
    assert!(server.child.wait().unwrap().success(), "{}", server.log());
    println!("gate: pcds {} -> pcds {}, {objects} world objects, the world stood {} ms", before["version"], version(&candidate), done["stoodMs"]);
    drop(server);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Makes the fixture of a release: the state its bank writes for the world with one of every
/// building, kept as server/tests/fixtures/snapshot-<release>.json.gz and restored by
/// handoff::tests::the_snapshot_of_every_release_restores. PCDS_FIXTURE_RELEASE: its folder;
/// PCDS_FIXTURE_OUT: the file. Both banks are that release; a flip makes its snapshot, the
/// bank that took it is killed, and the stop keeps the last snapshot beside the save
/// (docs/hot-update.md, "a crash"). 4 players are in it.
#[test]
#[ignore = "run by hand to make a fixture: docs/hot-update.md, The release before"]
fn make_the_snapshot_fixture_of_a_release() {
    let Some(data) = tables() else { panic!("PCDS_TEST_DATA=none: a fixture needs game tables") };
    let program = std::env::var("PCDS_FIXTURE_RELEASE").map(|d| Path::new(&d).join("pcds")).expect("PCDS_FIXTURE_RELEASE: the folder of the release");
    let out = PathBuf::from(std::env::var("PCDS_FIXTURE_OUT").expect("PCDS_FIXTURE_OUT: the .json.gz to write"));
    let dir = rich_dir("fixture", &data);
    let banks = banks_of(&dir, &program, &program);
    let server = start_with(&program, &dir, &data, &banks, "http://127.0.0.1:9/releases", false);
    let players = load(&server, &data, 4, 25);
    std::thread::sleep(Duration::from_secs(10));
    assert_eq!(server.update(json!({"rollback": true}))["ok"], true, "{}", server.log());
    std::thread::sleep(Duration::from_secs(2));
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
    server.child.wait().unwrap();
    let _ = finish(players);
    let snapshot = std::fs::read(dir.join("world.json.snapshot")).expect("the snapshot kept beside the save");
    let head: Value = serde_json::from_slice(snapshot.split(|b| *b == b'\n').next().unwrap()).unwrap();
    println!("fixture: format {} on game {}, {} bytes", head["format"], head["game_version"], snapshot.len());
    let mut gzip = Command::new("gzip").args(["-9", "-n", "-c"]).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
    gzip.stdin.take().unwrap().write_all(&snapshot).unwrap();
    std::fs::write(&out, gzip.wait_with_output().unwrap().stdout).unwrap();
    drop(server);
    std::fs::remove_dir_all(&dir).unwrap();
}
