//! Server releases from GitHub into a bank (docs/hot-update.md): the release's tarball and its
//! published sha256 (the assets `make dist` and the release workflow make), checked, unpacked
//! into the bank's folder beside the running one, and asked what it is (`pcds bank-info`):
//! the front protocol it speaks and the snapshot formats it reads. The front then flips to it;
//! with the server stopped, `pcds upgrade` only points run/bin/active at it.
//!
//! run/bin/a/ and run/bin/b/ are the banks, each a whole release (pcds, libsteam_api.so, web/) with its bank.json (what bank-info said, the tag, the tarball's
//! sha256); run/bin/active names the one the front starts.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

/// The releases of this project (PCDS_RELEASES points elsewhere: a mirror, or a test).
pub const RELEASES: &str = "https://api.github.com/repos/perara/planet-crafter-dedicated-server/releases";
/// The largest tarball read (a release is about 15 MB).
const MAX_DOWNLOAD: u64 = 256 << 20;
pub const BANKS: [&str; 2] = ["a", "b"];

pub fn releases_url() -> String {
    crate::env("PCDS_RELEASES").unwrap_or_else(|| RELEASES.into())
}

#[derive(Debug)]
pub struct Release {
    pub tag: String,
    tarball: String,
    checksum: String,
}

fn agent() -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(600)))
        .timeout_connect(Some(Duration::from_secs(20)))
        .http_status_as_error(false)
        .user_agent(concat!("pcds/", env!("CARGO_PKG_VERSION")))
        .build();
    ureq::Agent::new_with_config(config)
}

fn get(agent: &ureq::Agent, url: &str) -> Result<Vec<u8>, String> {
    let mut answer = agent.get(url).header("Accept", "application/vnd.github+json, application/octet-stream, */*").call().map_err(|e| format!("{url}: {e}"))?;
    if !answer.status().is_success() {
        return Err(format!("{url}: HTTP {}", answer.status().as_u16()));
    }
    answer.body_mut().with_config().limit(MAX_DOWNLOAD).read_to_vec().map_err(|e| format!("{url}: {e}"))
}

/// "0.6.0" and "v0.6.0" are the tag v0.6.0.
pub fn tag(version: &str) -> String {
    if version.starts_with('v') { version.to_string() } else { format!("v{version}") }
}

/// The release `version` (None: the latest) and its two assets.
pub fn lookup(base: &str, version: Option<&str>) -> Result<Release, String> {
    let url = match version {
        None => format!("{base}/latest"),
        Some(v) => format!("{base}/tags/{}", tag(v)),
    };
    let release: Value = serde_json::from_slice(&get(&agent(), &url)?).map_err(|e| format!("{url}: {e}"))?;
    let tag = release["tag_name"].as_str().ok_or_else(|| format!("{url}: no tag_name"))?.to_string();
    let asset = |name: &str| {
        let assets = release["assets"].as_array().into_iter().flatten();
        let found = assets.into_iter().find(|a| a["name"] == name).and_then(|a| a["browser_download_url"].as_str());
        found.map(str::to_string).ok_or_else(|| format!("release {tag} has no {name}"))
    };
    let name = format!("pcds-{tag}-linux-x86_64.tar.gz");
    Ok(Release { tarball: asset(&name)?, checksum: asset(&format!("{name}.sha256"))?, tag })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Whether the release in a bank can take over the world the running bank holds (its HELLO
/// or bank.json: format), with this front. Err says what to do instead.
pub fn compatible(info: &Value, live: &Value) -> Result<(), String> {
    let what = info["tag"].as_str().map(str::to_string).unwrap_or_else(|| format!("pcds {}", info["version"].as_str().unwrap_or("?")));
    let protocol = info["protocol"].as_u64().unwrap_or(0);
    if protocol != crate::ipc::PROTOCOL {
        return Err(format!(
            "{what} speaks front protocol {protocol}, this front speaks {}: it cannot take over the running world. Such an update needs a front restart (pcds upgrade --restart {}), which disconnects every player; Steam players get a new invite code unless PCDS_STEAM_TOKEN is set",
            crate::ipc::PROTOCOL,
            info["tag"].as_str().unwrap_or("VERSION")
        ));
    }
    let format = live["format"].as_u64().unwrap_or(0);
    if !info["reads"].as_array().is_some_and(|r| r.iter().any(|f| f.as_u64() == Some(format))) {
        return Err(format!(
            "{what} reads snapshot formats {}, the running world is in format {format}: it needs a restart (pcds upgrade --restart {}; the world then loads from the save), which disconnects every player",
            info["reads"],
            info["tag"].as_str().unwrap_or("VERSION")
        ));
    }
    Ok(())
}

/// What a bank's program says it is (`pcds bank-info`), within 20 s.
fn ask(program: &Path) -> Result<Value, String> {
    let mut child = Command::new(program).arg("bank-info").stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().map_err(|e| format!("{}: {e}", program.display()))?;
    let deadline = Instant::now() + Duration::from_secs(20);
    while child.try_wait().map_err(|e| e.to_string())?.is_none() {
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err("bank-info: no answer within 20 s".into());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    let info: Value = serde_json::from_slice(&out.stdout).ok().filter(|v: &Value| out.status.success() && v["protocol"].is_u64()).ok_or("its pcds does not know bank-info")?;
    Ok(info)
}

/// The release into banks/<bank>: downloaded, checked against its sha256, unpacked beside the
/// bank, asked what it is, and only then moved over the bank. `live`: the running bank's HELLO,
/// for a release that is to take over its world (a hot update); None for an install while the
/// server is stopped. Err leaves the bank as it was.
pub fn install(release: &Release, banks: &Path, bank: &str, live: Option<&Value>) -> Result<Value, String> {
    if !BANKS.contains(&bank) {
        return Err(format!("no bank {bank}"));
    }
    let new = banks.join(format!(".{bank}.new"));
    let result = unpack(release, banks, &new).and_then(|info| {
        if let Some(live) = live {
            compatible(&info, live)?;
        }
        put(banks, bank, &new)?;
        Ok(info)
    });
    let _ = std::fs::remove_dir_all(&new);
    result
}

/// A bank's new folder, complete beside it, moved over it. A running program of the old one
/// keeps its files (they are renamed away, not overwritten).
pub fn put(banks: &Path, bank: &str, new: &Path) -> Result<(), String> {
    let path = banks.join(bank);
    let old = banks.join(format!(".{bank}.old"));
    let _ = std::fs::remove_dir_all(&old);
    if std::fs::symlink_metadata(&path).is_ok() {
        std::fs::rename(&path, &old).map_err(|e| format!("{}: {e}", path.display()))?;
    }
    std::fs::rename(new, &path).map_err(|e| format!("{}: {e}", path.display()))?;
    let _ = std::fs::remove_dir_all(&old);
    Ok(())
}

fn unpack(release: &Release, banks: &Path, new: &Path) -> Result<Value, String> {
    let tag = &release.tag;
    let http = agent();
    let tarball = get(&http, &release.tarball)?;
    let published = String::from_utf8_lossy(&get(&http, &release.checksum)?).split_whitespace().next().unwrap_or("").to_ascii_lowercase();
    let downloaded = hex(ring::digest::digest(&ring::digest::SHA256, &tarball).as_ref());
    if published != downloaded {
        return Err(format!("{tag}: the download does not match its published sha256 ({published} published, {downloaded} downloaded); nothing installed"));
    }
    let _ = std::fs::remove_dir_all(new);
    std::fs::create_dir_all(new).map_err(|e| format!("{}: {e}", new.display()))?;
    let file = banks.join(format!(".{tag}.tar.gz"));
    std::fs::write(&file, &tarball).map_err(|e| format!("{}: {e}", file.display()))?;
    let tar = Command::new("tar").arg("-xzf").arg(&file).arg("-C").arg(new).arg("--strip-components=1").status();
    let _ = std::fs::remove_file(&file);
    match tar {
        Ok(s) if s.success() => {}
        Ok(s) => return Err(format!("{tag}: tar could not unpack it ({s})")),
        Err(e) => return Err(format!("tar: {e} (install tar with your package manager)")),
    }
    let program = new.join("pcds");
    if !program.is_file() {
        return Err(format!("{tag}: no pcds in the tarball"));
    }
    let mut info = ask(&program).map_err(|e| format!("{tag}: {e}: a release from before hot updates. It needs a front restart (pcds upgrade --restart {tag}), which disconnects every player"))?;
    info["tag"] = tag.clone().into();
    info["sha256"] = downloaded.into();
    info["installed"] = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0).into();
    write_info(new, &info)?;
    Ok(info)
}

pub fn write_info(bank: &Path, info: &Value) -> Result<(), String> {
    let path = bank.join("bank.json");
    std::fs::write(&path, serde_json::to_string_pretty(info).unwrap_or_default() + "\n").map_err(|e| format!("{}: {e}", path.display()))
}

/// What is in a bank (its bank.json); None: nothing installed there.
pub fn info(banks: &Path, bank: &str) -> Option<Value> {
    let text = std::fs::read_to_string(banks.join(bank).join("bank.json")).ok()?;
    serde_json::from_str(&text).ok().filter(|_| banks.join(bank).join("pcds").is_file())
}

/// The bank the front starts: run/bin/active ("a" when there is none yet).
pub fn active(banks: &Path) -> String {
    let text = std::fs::read_to_string(banks.join("active")).unwrap_or_default();
    BANKS.iter().find(|b| **b == text.trim()).unwrap_or(&"a").to_string()
}

/// Points run/bin/active at a bank: written beside and moved over, so it is one or the other.
pub fn set_active(banks: &Path, bank: &str) -> Result<(), String> {
    let new = banks.join(".active.new");
    std::fs::write(&new, format!("{bank}\n")).and_then(|_| std::fs::rename(&new, banks.join("active"))).map_err(|e| format!("{}: {e}", banks.join("active").display()))
}

pub fn other(bank: &str) -> &'static str {
    if bank == "a" { "b" } else { "a" }
}

/// Whether the tag is a later release than the version ("v0.6.0" after "0.5.0").
pub fn newer(tag: &str, version: &str) -> bool {
    let numbers = |v: &str| v.trim_start_matches('v').split(['.', '-']).map(|p| p.parse::<u64>().unwrap_or(0)).collect::<Vec<_>>();
    numbers(tag) > numbers(version)
}

/// The admin API's view of the banks.
pub fn describe(banks: &Path) -> Value {
    let installed: Vec<Value> = BANKS.iter().map(|b| json!({"bank": b, "info": info(banks, b)})).collect();
    json!({"dir": banks, "active": active(banks), "installed": installed})
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::path::PathBuf;

    /// A release tarball as `make dist` lays it out, with `program` as its pcds.
    pub fn tarball(dir: &Path, tag: &str, program: &[u8]) -> Vec<u8> {
        let name = format!("pcds-{tag}-linux-x86_64");
        let root = dir.join(&name);
        std::fs::create_dir_all(root.join("web")).unwrap();
        std::fs::write(root.join("pcds"), program).unwrap();
        std::fs::set_permissions(root.join("pcds"), std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        std::fs::write(root.join("web/index.html"), "<html></html>").unwrap();
        let out = dir.join(format!("{name}.tar.gz"));
        assert!(Command::new("tar").arg("-czf").arg(&out).arg("-C").arg(dir).arg(&name).status().unwrap().success());
        let bytes = std::fs::read(&out).unwrap();
        std::fs::remove_dir_all(&root).unwrap();
        std::fs::remove_file(&out).unwrap();
        bytes
    }

    /// The routes of a release on the fake server: its JSON (as latest too), tarball and sha256.
    pub fn release(base_hint: &str, tag: &str, tarball: Vec<u8>, sha256: String) -> Vec<(String, Vec<u8>)> {
        let name = format!("pcds-{tag}-linux-x86_64.tar.gz");
        let json = json!({"tag_name": tag, "assets": [
            {"name": name, "browser_download_url": format!("{base_hint}/download/{name}")},
            {"name": format!("{name}.sha256"), "browser_download_url": format!("{base_hint}/download/{name}.sha256")},
        ]});
        vec![
            (format!("/releases/tags/{tag}"), json.to_string().into_bytes()),
            ("/releases/latest".into(), json.to_string().into_bytes()),
            (format!("/download/{name}"), tarball),
            (format!("/download/{name}.sha256"), format!("{sha256}  {name}\n").into_bytes()),
        ]
    }

    /// A pcds that answers bank-info as a release speaking `protocol` would.
    fn script(protocol: u64) -> Vec<u8> {
        format!("#!/bin/sh\n[ \"$1\" = bank-info ] && echo '{{\"version\":\"9.9.9\",\"protocol\":{protocol},\"format\":1,\"reads\":[0,1]}}'\n").into_bytes()
    }

    /// A release server for one release on 127.0.0.1; its releases URL.
    fn fake(dir: &Path, tag: &str, program: &[u8], tamper: bool) -> String {
        let tar = tarball(dir, tag, program);
        let sum = hex(ring::digest::digest(&ring::digest::SHA256, &tar).as_ref());
        let sum = if tamper { format!("{}{}", if sum.starts_with('0') { '1' } else { '0' }, &sum[1..]) } else { sum };
        format!("{}/releases", serve(release("BASE", tag, tar, sum)))
    }

    /// A server on 127.0.0.1 answering GET path -> body (404 for the rest), "BASE" in the
    /// bodies replaced by its own address; that address.
    pub fn serve(routes: Vec<(String, Vec<u8>)>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let routes: Vec<(String, Vec<u8>)> = routes.into_iter().map(|(p, b)| (p, String::from_utf8(b.clone()).map(|t| t.replace("BASE", &base).into_bytes()).unwrap_or(b))).collect();
        std::thread::spawn(move || {
            for mut s in listener.incoming().flatten() {
                let mut line = String::new();
                let mut reader = BufReader::new(s.try_clone().unwrap());
                reader.read_line(&mut line).unwrap();
                while reader.read_line(&mut String::new()).is_ok_and(|n| n > 2) {}
                let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
                let (status, body) = routes.iter().find(|(p, _)| *p == path).map(|(_, b)| ("200 OK", b.clone())).unwrap_or(("404 Not Found", b"{}".to_vec()));
                let _ = write!(s, "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                let _ = s.write_all(&body);
            }
        });
        base
    }

    fn test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pcds-updater-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    const LIVE: &str = r#"{"protocol": 2, "format": 1}"#;

    #[test]
    fn a_good_release_is_installed_into_the_bank() {
        let dir = test_dir("good");
        let base = fake(&dir, "v9.9.9", &script(crate::ipc::PROTOCOL), false);
        let banks = dir.join("bin");
        std::fs::create_dir_all(banks.join("b")).unwrap();
        std::fs::write(banks.join("b/old"), "").unwrap();
        let release = lookup(&base, None).unwrap();
        assert_eq!(release.tag, "v9.9.9");
        let live: Value = serde_json::from_str(LIVE).unwrap();
        let info = install(&release, &banks, "b", Some(&live)).unwrap();
        assert_eq!((info["tag"].as_str(), info["protocol"].as_u64()), (Some("v9.9.9"), Some(crate::ipc::PROTOCOL)));
        assert!(banks.join("b/pcds").is_file() && banks.join("b/web/index.html").is_file() && !banks.join("b/old").exists());
        assert_eq!(super::info(&banks, "b").unwrap()["tag"], "v9.9.9");
        assert_eq!(std::fs::read_dir(&banks).unwrap().count(), 1, "nothing left beside the bank");
        // A given version, as v-less as people write it.
        assert_eq!(lookup(&base, Some("9.9.9")).unwrap().tag, "v9.9.9");
        assert!(lookup(&base, Some("v1.0.0")).unwrap_err().contains("404"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_download_that_does_not_match_its_sha256_is_refused() {
        let dir = test_dir("bad-sha");
        let base = fake(&dir, "v9.9.9", &script(crate::ipc::PROTOCOL), true);
        let banks = dir.join("bin");
        std::fs::create_dir_all(banks.join("b")).unwrap();
        std::fs::write(banks.join("b/pcds"), "the old one").unwrap();
        let e = install(&lookup(&base, None).unwrap(), &banks, "b", None).unwrap_err();
        assert!(e.contains("does not match its published sha256") && e.contains("nothing installed"), "{e}");
        assert_eq!(std::fs::read_to_string(banks.join("b/pcds")).unwrap(), "the old one", "the bank is as it was");
        assert_eq!(std::fs::read_dir(&banks).unwrap().count(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_release_of_another_front_protocol_is_refused_with_what_to_do() {
        let dir = test_dir("protocol");
        let banks = dir.join("bin");
        let live: Value = serde_json::from_str(LIVE).unwrap();
        let base = fake(&dir, "v9.9.9", &script(999), false);
        let e = install(&lookup(&base, None).unwrap(), &banks, "b", Some(&live)).unwrap_err();
        assert!(e.contains("front protocol 999") && e.contains("pcds upgrade --restart v9.9.9") && e.contains("disconnects every player") && e.contains("Steam"), "{e}");
        assert!(!banks.join("b").exists());
        // Stopped, a release of any protocol installs (the front starts from it).
        install(&lookup(&base, None).unwrap(), &banks, "b", None).unwrap();
        // A release from before hot updates has no bank-info.
        let base = fake(&dir, "v0.4.0", b"#!/bin/sh\nexit 1\n", false);
        let e = install(&lookup(&base, None).unwrap(), &banks, "a", Some(&live)).unwrap_err();
        assert!(e.contains("before hot updates"), "{e}");
        // A release that cannot read the running world's snapshot format.
        let old: Value = serde_json::from_str(r#"{"protocol": 2, "format": 7}"#).unwrap();
        assert!(compatible(&super::info(&banks, "b").map(|mut i| { i["protocol"] = crate::ipc::PROTOCOL.into(); i }).unwrap(), &old).unwrap_err().contains("format 7"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_active_bank_and_versions() {
        let dir = test_dir("active");
        assert_eq!(active(&dir), "a");
        set_active(&dir, "b").unwrap();
        assert_eq!((active(&dir), other("b"), other("a")), ("b".to_string(), "a", "b"));
        assert!(newer("v0.6.0", "0.5.0") && newer("v0.10.0", "0.9.9") && !newer("v0.5.0", "0.5.0") && !newer("v0.4.9", "0.5.0"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
