//! Server releases from GitHub into a bank (docs/hot-update.md): the release's tarball and its
//! published sha256 (the assets `make dist` and the release workflow make), checked, unpacked
//! into the bank's folder beside the running one, and asked what it is (`pcds bank-info`):
//! the front protocol it speaks and the snapshot formats it reads. The front then flips to it;
//! with the server stopped, `pcds upgrade` only points run/bin/active at it.
//!
//! Trust: the sha256 is published with the release, so it guards against a broken or cut
//! download, not against a release that is not the project's (a GitHub account or a mirror
//! taken over). Checking a signature on the release is out of scope here. What is done: https
//! only (http only for an explicit PCDS_RELEASES of http://127.0.0.1, the tests' release
//! server), tags of version characters only, a tarball of plain files and folders only, and
//! the new program asked what it is with none of the server's secrets in its environment.
//!
//! run/bin/a/ and run/bin/b/ are the banks, each a whole release (pcds, libsteam_api.so, web/) with its bank.json (what bank-info said, the tag, the tarball's
//! sha256); run/bin/active names the one the front starts.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::msg::{json as show, BankInfo, Fallback, RolledBack};
use crate::util::to_hex;

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
    /// The releases URL is http://127.0.0.1 (a test's server): its assets may be http too.
    local: bool,
}

/// Whether a releases URL is the one place plain http is taken: this machine's 127.0.0.1.
fn local(base: &str) -> bool {
    base.strip_prefix("http://127.0.0.1").is_some_and(|rest| rest.starts_with([':', '/']))
}

fn agent(local: bool) -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .https_only(!local)
        .timeout_global(Some(Duration::from_secs(600)))
        .timeout_connect(Some(Duration::from_secs(20)))
        .http_status_as_error(false)
        .user_agent(concat!("pcds/", env!("CARGO_PKG_VERSION")))
        .build();
    ureq::Agent::new_with_config(config)
}

/// A releases URL as it may be shown (/api/status, /api/update, the log, an error): scheme and
/// host only. PCDS_RELEASES can be a private mirror with a token in its user:password@, path or
/// query.
pub fn shown_url(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else { return "(not a URL)".into() };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    format!("{scheme}://{}", authority.rsplit_once('@').map_or(authority, |(_, host)| host))
}

fn get(agent: &ureq::Agent, url: &str) -> Result<Vec<u8>, String> {
    // Whatever the library says of the URL is said of its shown form.
    let shown = shown_url(url);
    let said = |e: String| format!("{shown}: {}", e.replace(url, &shown));
    let mut answer = agent.get(url).header("Accept", "application/vnd.github+json, application/octet-stream, */*").call().map_err(|e| said(e.to_string()))?;
    if !answer.status().is_success() {
        return Err(format!("{shown}: HTTP {}", answer.status().as_u16()));
    }
    answer.body_mut().with_config().limit(MAX_DOWNLOAD).read_to_vec().map_err(|e| said(e.to_string()))
}

/// "0.6.0" and "v0.6.0" are the tag v0.6.0.
pub fn tag(version: &str) -> String {
    if version.starts_with('v') { version.to_string() } else { format!("v{version}") }
}

/// A version or tag as it may go into a URL and a file name: v?[0-9A-Za-z._-]+, without "..".
pub fn check_tag(tag: &str) -> Result<(), String> {
    let rest = tag.strip_prefix('v').unwrap_or(tag);
    if rest.is_empty() || tag.contains("..") || !rest.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)) {
        return Err(format!("{tag:?} is not a release version (v?[0-9A-Za-z._-]+)"));
    }
    Ok(())
}

/// The release `version` (None: the latest) and its two assets.
pub fn lookup(base: &str, version: Option<&str>) -> Result<Release, String> {
    if let Some(v) = version {
        check_tag(v)?;
    }
    let url = match version {
        None => format!("{base}/latest"),
        Some(v) => format!("{base}/tags/{}", tag(v)),
    };
    let local = local(base);
    let shown = shown_url(&url);
    let release: Value = serde_json::from_slice(&get(&agent(local), &url)?).map_err(|e| format!("{shown}: {e}"))?;
    let tag = release["tag_name"].as_str().ok_or_else(|| format!("{shown}: no tag_name"))?.to_string();
    check_tag(&tag).map_err(|e| format!("{shown}: tag_name {e}"))?;
    let asset = |name: &str| {
        let assets = release["assets"].as_array().into_iter().flatten();
        let found = assets.into_iter().find(|a| a["name"] == name).and_then(|a| a["browser_download_url"].as_str());
        found.map(str::to_string).ok_or_else(|| format!("release {tag} has no {name}"))
    };
    let name = format!("pcds-{tag}-linux-x86_64.tar.gz");
    Ok(Release { tarball: asset(&name)?, checksum: asset(&format!("{name}.sha256"))?, tag, local })
}

/// Whether the release in a bank can take over the world the running bank holds (its HELLO
/// or bank.json: format), with this front. Err says what to do instead.
pub fn compatible(info: &BankInfo, live: Option<&BankInfo>) -> Result<(), String> {
    compatible_for(info, live, false)
}

/// The command that installs the release with a restart: for a rollback, going back to the
/// bank that did not run (no download); else the release's own tag (a bank that holds a build
/// has none: its version names it).
fn restart_command(info: &BankInfo, rollback: bool) -> String {
    if rollback {
        return "pcds upgrade --rollback --restart".into();
    }
    let tag = info.tag.clone().or_else(|| info.version.as_deref().map(tag)).unwrap_or_else(|| "VERSION".into());
    format!("pcds upgrade --restart {tag}")
}

/// compatible(), for a rollback (`rollback`: the release is the other bank's, the one that ran
/// before) or an update: the same checks, and the command to run instead says which.
pub fn compatible_for(info: &BankInfo, live: Option<&BankInfo>, rollback: bool) -> Result<(), String> {
    let what = info.tag.clone().unwrap_or_else(|| format!("pcds {}", info.version()));
    let protocol = info.protocol.unwrap_or(0);
    let restart = restart_command(info, rollback);
    if protocol != crate::ipc::PROTOCOL {
        return Err(format!(
            "{what} speaks front protocol {protocol}, this front speaks {}: it cannot take over the running world. Such an update needs a front restart ({restart}), which disconnects every player; Steam players get a new invite code unless PCDS_STEAM_TOKEN is set",
            crate::ipc::PROTOCOL
        ));
    }
    // The tables on disk are the running bank's: a release that reads another format would
    // refuse them (no live: a HELLO checked on its own, nothing runs yet).
    if let Some(live) = live.filter(|l| info.tables_format != l.tables_format) {
        return Err(format!(
            "{what} reads game tables of format {}, the running server's are of format {}: it cannot take over the running world. {restart} {} the game tables again and restarts the server, which disconnects every player",
            show(&info.tables_format),
            show(&live.tables_format),
            if rollback { "goes back, reads" } else { "installs it, reads" }
        ));
    }
    let format = live.and_then(|l| l.format).unwrap_or(0);
    if info.format != Some(format) {
        // A release reads its own snapshot format only: across a change of it an update or a
        // rollback is a restart (the stop saves the world; the other release loads the save).
        // No loss: players are disconnected, nothing is rolled back in the world.
        return Err(format!(
            "{what} reads snapshot format {}, the running world is in format {format}: {} needs a restart ({restart}; the server saves the world as it stops and {what} loads that save), which disconnects every player",
            show(&info.format),
            if rollback { "a rollback across a change of the snapshot format" } else { "it" }
        ));
    }
    Ok(())
}

/// Why a program did not say what it is.
#[derive(Debug)]
enum Asked {
    /// It ran, and did not answer bank-info with a front protocol.
    Unknown,
    Failed(String),
}

/// What a bank's program says it is (`pcds bank-info`), within 20 s. A downloaded program, so
/// it runs with PATH, HOME and LANG only, as the chat exec hook does: never with the admin or
/// Steam token.
fn ask(program: &Path) -> Result<BankInfo, Asked> {
    let mut command = Command::new(program);
    command.arg("bank-info").env_clear().stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
    for key in ["PATH", "HOME", "LANG"] {
        if let Some(v) = std::env::var_os(key) {
            command.env(key, v);
        }
    }
    // A program that cannot be run (not executable, wrong architecture) is told apart from one
    // that runs and answers wrongly.
    // ETXTBSY: another thread's fork holds the file's write descriptor until its exec (the tests
    // run installs side by side); it passes at once.
    let mut tries = 0;
    let mut child = loop {
        match command.spawn() {
            Err(e) if e.raw_os_error() == Some(libc::ETXTBSY) && tries < 20 => {
                tries += 1;
                std::thread::sleep(Duration::from_millis(50));
            }
            other => break other.map_err(|e| Asked::Failed(format!("{} cannot be run: {e}", program.display())))?,
        }
    };
    let deadline = Instant::now() + Duration::from_secs(20);
    while child.try_wait().map_err(|e| Asked::Failed(e.to_string()))?.is_none() {
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(Asked::Failed("bank-info: no answer within 20 s".into()));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let out = child.wait_with_output().map_err(|e| Asked::Failed(e.to_string()))?;
    serde_json::from_slice(&out.stdout).ok().filter(|v: &BankInfo| out.status.success() && v.protocol.is_some()).ok_or(Asked::Unknown)
}

/// The release into banks/<bank>: downloaded, checked against its sha256, unpacked beside the
/// bank, asked what it is, and only then moved over the bank. `live`: the running bank's HELLO,
/// for a release that is to take over its world (a hot update); None for an install while the
/// server is stopped. Err leaves the bank as it was.
pub fn install(release: &Release, banks: &Path, bank: &str, live: Option<&BankInfo>) -> Result<BankInfo, String> {
    if !BANKS.contains(&bank) {
        return Err(format!("no bank {bank}"));
    }
    std::fs::create_dir_all(banks).map_err(|e| format!("{}: {e}", banks.display()))?;
    sweep(banks);
    let new = staging(banks, bank);
    let result = unpack(release, &new).and_then(|info| {
        if let Some(live) = live {
            compatible(&info, Some(live))?;
        }
        put(banks, bank, &new)?;
        Ok(info)
    });
    let _ = std::fs::remove_dir_all(&new);
    result
}

/// The banks folder's lock (banks/.lock, flock), held while a bank or `active` is replaced:
/// the front's updater and the command line (`pcds upgrade --restart`, `pcds setup`) may
/// install at once. Released when the file is dropped. Only renames and small writes happen
/// under it (the front takes it on its loop at a flip): downloads and unpacking go beside the
/// banks under names of their own process (staging).
pub fn lock(banks: &Path) -> Result<std::fs::File, String> {
    use std::os::fd::AsRawFd;
    let path = banks.join(".lock");
    let err = |e: std::io::Error| format!("{}: {e}", path.display());
    std::fs::create_dir_all(banks).map_err(err)?;
    let file = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(&path).map_err(err)?;
    // SAFETY: flock on a descriptor this function owns.
    while unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
        let e = std::io::Error::last_os_error();
        if e.kind() != std::io::ErrorKind::Interrupted {
            return Err(err(e));
        }
    }
    Ok(file)
}

/// Where this process unpacks a release for `bank`: .<bank>.new.<pid>, nobody else's.
pub fn staging(banks: &Path, bank: &str) -> std::path::PathBuf {
    banks.join(format!(".{bank}.new.{}", std::process::id()))
}

/// What an install of a process that is gone left beside the banks (it crashed mid-way).
fn sweep(banks: &Path) {
    for entry in std::fs::read_dir(banks).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        // .<bank>.new.<pid> and its .tar.gz
        let Some((_, rest)) = name.strip_prefix('.').and_then(|n| n.split_once(".new.")) else { continue };
        let Ok(pid) = rest.split('.').next().unwrap_or("").parse::<i32>() else { continue };
        // SAFETY: signal 0 only asks whether the process exists.
        if pid > 0 && unsafe { libc::kill(pid, 0) } != 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
            let _ = std::fs::remove_dir_all(entry.path()).or_else(|_| std::fs::remove_file(entry.path()));
        }
    }
}

/// A bank's new folder, complete beside it, moved over it (under the banks' lock). A running
/// program of the old one keeps its files (they are renamed away, not overwritten). Err leaves
/// the bank as it was.
pub fn put(banks: &Path, bank: &str, new: &Path) -> Result<(), String> {
    put_with(banks, bank, new, |p| std::fs::File::open(p)?.sync_all(), |_| false)
}

/// Every file and folder under `dir`, `dir` last, through `sync` (fsync).
fn sync_tree(dir: &Path, sync: &impl Fn(&Path) -> std::io::Result<()>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            sync_tree(&entry.path(), sync)?;
        } else {
            sync(&entry.path())?;
        }
    }
    sync(dir)
}

/// What an install that died between its two renames left (`put`): `.<bank>.old` holds the bank
/// as it was. With no bank in its place the old one is put back; with a whole new bank in
/// place (it died after the second rename) the old one is only to be removed. Each thing done
/// is returned, to be told. Under the banks' lock (`recover` takes it; `put` holds it).
fn recover_locked(banks: &Path) -> Result<Vec<String>, String> {
    let mut done = Vec::new();
    for bank in BANKS {
        let (path, old) = (banks.join(bank), banks.join(format!(".{bank}.old")));
        if std::fs::symlink_metadata(&old).is_err() {
            continue;
        }
        if std::fs::symlink_metadata(&path).is_err() {
            std::fs::rename(&old, &path).map_err(|e| format!("{}: could not be put back as bank {bank}: {e}", old.display()))?;
            done.push(format!("bank {bank} was missing after an install was interrupted: the bank it held is back"));
        } else {
            std::fs::remove_dir_all(&old).map_err(|e| format!("{}: {e}", old.display()))?;
        }
    }
    Ok(done)
}

/// Makes the banks folder whole after a crash of an install (`recover_locked`), under the lock.
/// The front does it before it picks the bank to start.
pub fn recover(banks: &Path) -> Result<Vec<String>, String> {
    let _lock = lock(banks)?;
    recover_locked(banks)
}

/// put() with the flush as a parameter, and `crash`, which a test uses to end the process
/// where it stands (true at "renamed away" and "renamed in": the function returns with the
/// disk as a crash there leaves it). The unpacked files reach the disk before the folder is
/// moved to the bank's name, and the move itself after: a power cut leaves the old bank or a
/// whole new one, not a bank whose program is a name with no bytes (the data of a file the
/// system had not written yet).
fn put_with(banks: &Path, bank: &str, new: &Path, sync: impl Fn(&Path) -> std::io::Result<()>, crash: impl Fn(&str) -> bool) -> Result<(), String> {
    let _lock = lock(banks)?;
    sync_tree(new, &sync).map_err(|e| format!("{}: could not be flushed to disk before it is installed: {e}", new.display()))?;
    // A crash of an earlier install: what it left is made whole first (never removed blindly: the
    // bank it moved away may be the only copy).
    for done in recover_locked(banks)? {
        println!("banks: {done}");
    }
    let path = banks.join(bank);
    let old = banks.join(format!(".{bank}.old"));
    let had = std::fs::symlink_metadata(&path).is_ok();
    if had {
        std::fs::rename(&path, &old).map_err(|e| format!("{}: {e}", path.display()))?;
    }
    if crash("renamed away") {
        return Err("crashed".into());
    }
    if let Err(e) = std::fs::rename(new, &path) {
        if had && let Err(back) = std::fs::rename(&old, &path) {
            return Err(format!("{}: {e}; and the bank could not be put back from {}: {back}", path.display(), old.display()));
        }
        return Err(format!("{}: {e}", path.display()));
    }
    if crash("renamed in") {
        return Err("crashed".into());
    }
    let _ = std::fs::remove_dir_all(&old);
    // The renames themselves.
    sync(banks).map_err(|e| format!("{}: could not be flushed to disk: {e} (the bank is installed)", banks.display()))
}

fn unpack(release: &Release, new: &Path) -> Result<BankInfo, String> {
    let tag = &release.tag;
    let http = agent(release.local);
    let tarball = get(&http, &release.tarball)?;
    let published = String::from_utf8_lossy(&get(&http, &release.checksum)?).split_whitespace().next().unwrap_or("").to_ascii_lowercase();
    let downloaded = to_hex(ring::digest::digest(&ring::digest::SHA256, &tarball).as_ref());
    if published != downloaded {
        return Err(format!("{tag}: the download does not match its published sha256 ({published} published, {downloaded} downloaded); nothing installed"));
    }
    let _ = std::fs::remove_dir_all(new);
    std::fs::create_dir_all(new).map_err(|e| format!("{}: {e}", new.display()))?;
    let file = std::path::PathBuf::from(format!("{}.tar.gz", new.display()));
    // A tarball that could not be written whole (a full disk) is not left beside the banks: this
    // process's own leftovers are never swept (sweep only takes those of processes that are gone).
    std::fs::write(&file, &tarball).map_err(|e| {
        let _ = std::fs::remove_file(&file);
        format!("{}: {e}", file.display())
    })?;
    let tar = Command::new("tar").arg("-xzf").arg(&file).arg("-C").arg(new).arg("--strip-components=1").status();
    let _ = std::fs::remove_file(&file);
    match tar {
        Ok(s) if s.success() => {}
        Ok(s) => return Err(format!("{tag}: tar could not unpack it ({s})")),
        Err(e) => return Err(format!("tar: {e} (install tar with your package manager)")),
    }
    plain_files(new, new).map_err(|e| format!("{tag}: the tarball holds {e}, which is not a file or a folder of its own (a link, a hard link or a device in it could point the server anywhere); nothing installed"))?;
    let program = new.join("pcds");
    if !program.is_file() {
        return Err(format!("{tag}: no pcds in the tarball"));
    }
    let mut info = ask(&program).map_err(|e| match e {
        Asked::Unknown => format!("{tag}: its pcds does not answer bank-info; nothing installed"),
        Asked::Failed(e) => format!("{tag}: {e}"),
    })?;
    info.tag = Some(tag.clone());
    info.sha256 = Some(downloaded);
    info.installed = Some(SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0));
    write_info(new, &info)?;
    Ok(info)
}

/// Err names the first thing under `dir` that is neither a plain file nor a folder (a link, a
/// device, a fifo) or is a file with more than one name (a hard link in the tarball, which tar
/// extracts as a second name of the same file): a link in a bank could point the server anywhere.
fn plain_files(root: &Path, dir: &Path) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;
    for entry in std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))? {
        let entry = entry.map_err(|e| e.to_string())?;
        let kind = entry.file_type().map_err(|e| e.to_string())?;
        let path = entry.path();
        if kind.is_dir() {
            plain_files(root, &path)?;
        } else if !kind.is_file() || entry.metadata().map_err(|e| e.to_string())?.nlink() > 1 {
            return Err(path.strip_prefix(root).unwrap_or(&path).display().to_string());
        }
    }
    Ok(())
}

pub fn write_info(bank: &Path, info: &BankInfo) -> Result<(), String> {
    let path = bank.join("bank.json");
    std::fs::write(&path, serde_json::to_string_pretty(info).unwrap_or_default() + "\n").map_err(|e| format!("{}: {e}", path.display()))
}

/// What is in a bank (its bank.json); None: nothing installed there.
pub fn info(banks: &Path, bank: &str) -> Option<BankInfo> {
    let text = std::fs::read_to_string(banks.join(bank).join("bank.json")).ok()?;
    serde_json::from_str(&text).ok().filter(|_| banks.join(bank).join("pcds").is_file())
}

/// The bank the front starts: run/bin/active. No file yet is a fresh installation, which starts
/// from bank a; a file that says anything but a or b is an error saying what to do, never bank a.
pub fn active(banks: &Path) -> Result<String, String> {
    let path = banks.join("active");
    match std::fs::read_to_string(&path) {
        Ok(text) => BANKS.iter().find(|b| **b == text.trim()).map(|b| b.to_string()).ok_or_else(|| format!("{}: it holds {:?}, and must say a or b, the bank the server runs from. Write a or b in it (the other bank is the standby), or delete the file to start from bank a", path.display(), text.trim())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok("a".into()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// Points run/bin/active at a bank: written beside, flushed, and moved over (under the banks'
/// lock), so it is one or the other, also after a crash.
pub fn set_active(banks: &Path, bank: &str) -> Result<(), String> {
    let _lock = lock(banks)?;
    crate::util::replace_file(&banks.join("active"), format!("{bank}\n").as_bytes(), 0o644)
}

/// The bank the owner rolled back from, and its release (banks/rolled-back: {"bank", "tag"}).
/// The automatic update does not install that tag again, and `pcds run` installs nothing over
/// that bank; a later release, an explicit update (`pcds upgrade`, POST /api/update with a
/// version or the latest) or the next rollback replaces or clears it.
pub fn rolled_back(banks: &Path) -> Option<RolledBack> {
    let text = std::fs::read_to_string(banks.join("rolled-back")).ok()?;
    serde_json::from_str(&text).ok()
}

/// A rollback away from `bank`: it is noted with the release it holds.
pub fn set_rolled_back(banks: &Path, bank: &str) -> Result<(), String> {
    note_rolled_back(banks, bank, None)
}

/// The front's own rollback: the release in `bank` ended the game over and over after it was
/// flipped to (front/supervise.rs give_up). The note also says why and when, for the dashboard and
/// `pcds status` (a rollback of the owner's is not explained: they asked for it).
pub fn set_rolled_back_by_crashes(banks: &Path, bank: &str, why: &str) -> Result<(), String> {
    note_rolled_back(banks, bank, Some(why))
}

fn note_rolled_back(banks: &Path, bank: &str, why: Option<&str>) -> Result<(), String> {
    let held = info(banks, bank).unwrap_or_default();
    let tag = held.tag.or_else(|| held.version.as_deref().map(tag));
    let note = RolledBack { bank: bank.to_string(), tag, reason: why.map(str::to_string), at: why.map(|_| SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)) };
    let _lock = lock(banks)?;
    crate::util::replace_file(&banks.join("rolled-back"), (crate::msg::text(&note) + "\n").as_bytes(), 0o644)
}

/// The bank a release that keeps ending the game is given up for: the other one. Err (why not):
/// it holds no release, or it is the one the server was rolled back from (the release in `bad`
/// was installed after it): going back would only bring the old trouble back.
pub fn crash_rollback_target(banks: &Path, bad: &str) -> Result<&'static str, String> {
    let to = other(bad);
    if info(banks, to).is_none() {
        return Err(format!("bank {to} holds no release to go back to"));
    }
    if rolled_back(banks).is_some_and(|r| r.bank == to) {
        return Err(format!("bank {to} is the one the server was rolled back from"));
    }
    Ok(to)
}

pub fn clear_rolled_back(banks: &Path) -> Result<(), String> {
    let path = banks.join("rolled-back");
    let _lock = lock(banks)?;
    match std::fs::remove_file(&path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(format!("{}: {e}", path.display())),
        _ => Ok(()),
    }
}

/// A start that ran bank `to` because bank `from` could not start the world (the front's boot,
/// or `pcds run` when the active bank's front could not): banks/fallback, {"from", "to",
/// "reason", "at"}. Shown (describe, so /api/status and `pcds status`) while `to` is the active
/// bank; `pcds upgrade` clears it.
pub fn note_fallback(banks: &Path, from: &str, to: &str, reason: &str) -> Result<(), String> {
    let at = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let note = Fallback { from: from.to_string(), to: to.to_string(), reason: reason.to_string(), at: Some(at) };
    let _lock = lock(banks)?;
    crate::util::replace_file(&banks.join("fallback"), (crate::msg::text(&note) + "\n").as_bytes(), 0o644)
}

/// The fallback note, while the bank it started is the active one.
pub fn fallback(banks: &Path) -> Option<Fallback> {
    let text = std::fs::read_to_string(banks.join("fallback")).ok()?;
    serde_json::from_str(&text).ok().filter(|f: &Fallback| active(banks).is_ok_and(|a| f.to == a))
}

pub fn clear_fallback(banks: &Path) -> Result<(), String> {
    let path = banks.join("fallback");
    let _lock = lock(banks)?;
    match std::fs::remove_file(&path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(format!("{}: {e}", path.display())),
        _ => Ok(()),
    }
}

/// What the automatic update does with the latest release `tag` after a rollback: skip it
/// (Some: why) when it is the release rolled back from; a later one clears the note.
pub fn auto_skips(banks: &Path, tag: &str) -> Result<Option<String>, String> {
    let Some(from) = rolled_back(banks).and_then(|r| r.tag) else { return Ok(None) };
    if from == tag {
        return Ok(Some(format!("{tag} is the release the server was rolled back from: the automatic update leaves it out (pcds upgrade {tag} installs it)")));
    }
    if newer(tag, &from) {
        clear_rolled_back(banks)?;
    }
    Ok(None)
}

pub fn other(bank: &str) -> &'static str {
    if bank == "a" { "b" } else { "a" }
}

/// Whether the tag is a later release than the version ("v0.6.0" after "0.5.0"), as semantic
/// versions order them: a pre-release comes before its release ("1.0.0-rc.1" < "1.0.0").
pub fn newer(tag: &str, version: &str) -> bool {
    version_key(tag) > version_key(version)
}

/// The release a bank holds, as a later release is compared with it: its tag (bank.json, written
/// by an install of a release) when it has one, else the program's version (a build, or a
/// release folder's program). A tag the program's version does not match (a forgotten bump)
/// then still counts as installed, and is not fetched again at every check.
pub fn held_release(banks: &Path, bank: &str, version: &str) -> String {
    info(banks, bank).and_then(|i| i.tag).unwrap_or_else(|| version.to_string())
}

/// A pre-release's part: numbers by value, and below words (semver 11.4).
#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum Part<'a> {
    Number(u64),
    Word(&'a str),
}

/// What versions are ordered by: MAJOR.MINOR.PATCH, then whether it is a release (above its
/// pre-releases), then the pre-release's parts in turn (fewer parts first when the rest are
/// equal). Build metadata (+...) does not count.
fn version_key(v: &str) -> (Vec<u64>, bool, Vec<Part<'_>>) {
    let v = v.trim_start_matches('v');
    let v = v.split_once('+').map_or(v, |(v, _)| v);
    let (core, pre) = v.split_once('-').map_or((v, None), |(c, p)| (c, Some(p)));
    let mut numbers: Vec<u64> = core.split('.').map(|p| p.parse().unwrap_or(0)).collect();
    numbers.resize(numbers.len().max(3), 0);
    let parts = pre.map(|p| p.split('.').map(|x| x.parse().map_or(Part::Word(x), Part::Number)).collect()).unwrap_or_default();
    (numbers, pre.is_none(), parts)
}

/// The admin API's view of the banks.
pub fn describe(banks: &Path) -> Value {
    let installed: Vec<Value> = BANKS.iter().map(|b| json!({"bank": b, "info": info(banks, b)})).collect();
    let active = active(banks);
    let mut v = json!({"dir": banks, "active": active.as_ref().ok(), "installed": installed, "fallback": fallback(banks), "rolledBack": rolled_back(banks)});
    if let Err(e) = active {
        v["error"] = json!(e);
    }
    v
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
        format!("#!/bin/sh\n[ \"$1\" = bank-info ] && echo '{{\"version\":\"9.9.9\",\"protocol\":{protocol},\"format\":1}}'\n").into_bytes()
    }

    /// A release server for one release on 127.0.0.1; its releases URL.
    fn fake(dir: &Path, tag: &str, program: &[u8], tamper: bool) -> String {
        let tar = tarball(dir, tag, program);
        let sum = to_hex(ring::digest::digest(&ring::digest::SHA256, &tar).as_ref());
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

    /// What is in the banks folder, but its lock.
    fn names(banks: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(banks).unwrap().flatten().map(|e| e.file_name().to_string_lossy().to_string()).filter(|n| n != ".lock").collect();
        names.sort();
        names
    }

    #[test]
    fn a_bank_is_replaced_whole_or_not_at_all() {
        let dir = test_dir("put");
        let banks = dir.join("bin");
        std::fs::create_dir_all(banks.join("b")).unwrap();
        std::fs::write(banks.join("b/pcds"), "old").unwrap();
        // The second rename fails (nothing to move): the bank is put back.
        assert!(put(&banks, "b", &banks.join(".b.new.0")).is_err());
        assert_eq!((std::fs::read_to_string(banks.join("b/pcds")).unwrap(), names(&banks)), ("old".into(), vec!["b".to_string()]));
        // Two installs at once, each from its own staging folder: one whole bank after both.
        let installs: Vec<_> = (0..2)
            .map(|i| {
                let banks = banks.clone();
                std::thread::spawn(move || {
                    for n in 0..20 {
                        let new = banks.join(format!(".b.new.{i}-{n}"));
                        std::fs::create_dir_all(&new).unwrap();
                        std::fs::write(new.join("pcds"), format!("install {i}")).unwrap();
                        put(&banks, "b", &new).unwrap();
                        set_active(&banks, if i == 0 { "a" } else { "b" }).unwrap();
                    }
                })
            })
            .collect();
        installs.into_iter().for_each(|t| t.join().unwrap());
        assert!(std::fs::read_to_string(banks.join("b/pcds")).unwrap().starts_with("install "));
        assert_eq!(names(&banks), ["active", "b"]);
        // What an install that crashed left: removed by the next one; a live process's stays.
        std::fs::create_dir_all(banks.join(".a.new.999999999")).unwrap();
        std::fs::write(banks.join(".a.new.999999999.tar.gz"), "").unwrap();
        std::fs::create_dir_all(banks.join(format!(".a.new.{}", std::process::id()))).unwrap();
        sweep(&banks);
        assert_eq!(names(&banks), [format!(".a.new.{}", std::process::id()), "active".into(), "b".into()]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A crash between the two renames of `put` leaves the bank missing and `.<bank>.old` holding
    /// it; a crash after them leaves a whole new bank and the old one beside it. `recover` (the
    /// front's start) and the next `put` make either whole: the old bank is put back, or the
    /// leftover removed; with the crash injected at each point.
    #[test]
    fn a_crash_inside_an_install_is_made_whole_at_the_next_start() {
        let dir = test_dir("put-crash");
        let banks = dir.join("bin");
        let bank = || std::fs::read_to_string(banks.join("b/pcds")).unwrap_or_else(|e| e.to_string());
        let stage = |n: u32| {
            let new = banks.join(format!(".b.new.{n}"));
            std::fs::create_dir_all(&new).unwrap();
            std::fs::write(new.join("pcds"), "new").unwrap();
            new
        };
        for point in ["renamed away", "renamed in"] {
            let _ = std::fs::remove_dir_all(&banks);
            std::fs::create_dir_all(banks.join("b")).unwrap();
            std::fs::write(banks.join("b/pcds"), "old").unwrap();
            assert_eq!(put_with(&banks, "b", &stage(1), |_| Ok(()), |at| at == point).unwrap_err(), "crashed");
            if point == "renamed away" {
                assert_eq!(names(&banks), [".b.new.1", ".b.old"], "the bank is missing: its folder is the .old one");
                assert!(!banks.join("b").exists());
            } else {
                assert_eq!((bank(), banks.join(".b.old/pcds").is_file()), ("new".into(), true), "a whole new bank, the old one beside it");
            }
            let told = recover(&banks).unwrap();
            if point == "renamed away" {
                assert_eq!((bank(), told.len()), ("old".into(), 1), "the bank it held is back: {told:?}");
            } else {
                assert_eq!((bank(), told.len()), ("new".into(), 0), "the leftover goes, the new bank stays");
            }
            assert!(!banks.join(".b.old").exists());
            // Nothing to recover: nothing done.
            assert_eq!(recover(&banks).unwrap(), Vec::<String>::new());
        }
        // The install tried again after a crash (no recover in between) must not remove the only
        // copy of the old bank: put finds `.old`, puts it back, and then replaces it.
        let _ = std::fs::remove_dir_all(&banks);
        std::fs::create_dir_all(banks.join("b")).unwrap();
        std::fs::write(banks.join("b/pcds"), "old").unwrap();
        put_with(&banks, "b", &stage(2), |_| Ok(()), |at| at == "renamed away").unwrap_err();
        put_with(&banks, "b", &stage(3), |_| Ok(()), |_| false).unwrap();
        assert_eq!(bank(), "new");
        assert!(!banks.join(".b.old").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    const LIVE: &str = r#"{"format": 1}"#;

    /// A bank's info from the JSON a test spells out.
    fn bi(v: Value) -> BankInfo {
        serde_json::from_value(v).unwrap()
    }

    /// The unpacked files are flushed before the folder is moved to the bank's name, and the
    /// move after; a flush that fails stops the install with the bank as it was.
    #[test]
    fn a_bank_is_flushed_to_disk_before_it_is_moved_into_place() {
        use std::cell::RefCell;
        let dir = test_dir("put-sync");
        let (banks, new) = (dir.join("bin"), dir.join("bin/.b.new.1"));
        std::fs::create_dir_all(banks.join("b")).unwrap();
        std::fs::write(banks.join("b/pcds"), "old").unwrap();
        std::fs::create_dir_all(new.join("web")).unwrap();
        std::fs::write(new.join("pcds"), "new").unwrap();
        std::fs::write(new.join("web/index.html"), "new").unwrap();
        // What was in the bank's place at each flush.
        let log = RefCell::new(Vec::new());
        let record = |p: &Path| {
            let at = std::fs::read_to_string(banks.join("b/pcds")).unwrap_or_default();
            log.borrow_mut().push((p.strip_prefix(&banks).unwrap_or(p).to_path_buf(), at));
            Ok(())
        };
        put_with(&banks, "b", &new, record, |_| false).unwrap();
        let log = log.into_inner();
        let names: Vec<String> = log.iter().map(|(p, _)| p.display().to_string()).collect();
        // Both files, the web folder, the new folder (last of them), then the banks folder.
        assert_eq!(names, [".b.new.1/web/index.html", ".b.new.1/web", ".b.new.1/pcds", ".b.new.1", ""].map(String::from).to_vec(), "{names:?}");
        assert!(log[..4].iter().all(|(_, bank)| bank == "old"), "flushed while the old bank stood: {log:?}");
        assert_eq!(log[4].1, "new", "the move flushed after it was made");
        assert_eq!(std::fs::read_to_string(banks.join("b/pcds")).unwrap(), "new");
        // A flush that fails: nothing moved.
        std::fs::create_dir_all(dir.join("bin/.b.new.2")).unwrap();
        std::fs::write(dir.join("bin/.b.new.2/pcds"), "newer").unwrap();
        let e = put_with(&banks, "b", &dir.join("bin/.b.new.2"), |_| Err(std::io::Error::other("disk gone")), |_| false).unwrap_err();
        assert!(e.contains("could not be flushed to disk before it is installed: disk gone"), "{e}");
        assert_eq!(std::fs::read_to_string(banks.join("b/pcds")).unwrap(), "new");
        // The real flush works on files and folders.
        put(&banks, "b", &dir.join("bin/.b.new.2")).unwrap();
        assert_eq!(std::fs::read_to_string(banks.join("b/pcds")).unwrap(), "newer");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_good_release_is_installed_into_the_bank() {
        let dir = test_dir("good");
        let base = fake(&dir, "v9.9.9", &script(crate::ipc::PROTOCOL), false);
        let banks = dir.join("bin");
        std::fs::create_dir_all(banks.join("b")).unwrap();
        std::fs::write(banks.join("b/old"), "").unwrap();
        let release = lookup(&base, None).unwrap();
        assert_eq!(release.tag, "v9.9.9");
        let live: BankInfo = serde_json::from_str(LIVE).unwrap();
        let info = install(&release, &banks, "b", Some(&live)).unwrap();
        assert_eq!((info.tag.as_deref(), info.protocol), (Some("v9.9.9"), Some(crate::ipc::PROTOCOL)));
        assert!(banks.join("b/pcds").is_file() && banks.join("b/web/index.html").is_file() && !banks.join("b/old").exists());
        assert_eq!(super::info(&banks, "b").unwrap().tag.as_deref(), Some("v9.9.9"));
        assert_eq!(names(&banks), ["b"], "nothing left beside the bank");
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
        assert_eq!(names(&banks), ["b"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_release_of_another_front_protocol_is_refused_with_what_to_do() {
        let dir = test_dir("protocol");
        let banks = dir.join("bin");
        let live: BankInfo = serde_json::from_str(LIVE).unwrap();
        let base = fake(&dir, "v9.9.9", &script(999), false);
        let e = install(&lookup(&base, None).unwrap(), &banks, "b", Some(&live)).unwrap_err();
        assert!(e.contains("front protocol 999") && e.contains("pcds upgrade --restart v9.9.9") && e.contains("disconnects every player") && e.contains("Steam"), "{e}");
        assert!(!banks.join("b").exists());
        // Stopped, a release of any protocol installs (the front starts from it).
        install(&lookup(&base, None).unwrap(), &banks, "b", None).unwrap();
        // A release whose pcds does not answer bank-info is not installed.
        let base = fake(&dir, "v0.4.0", b"#!/bin/sh\nexit 1\n", false);
        let e = install(&lookup(&base, None).unwrap(), &banks, "a", Some(&live)).unwrap_err();
        assert!(e.contains("does not answer bank-info"), "{e}");
        // A release that cannot read the running world's snapshot format.
        let old: BankInfo = serde_json::from_str(r#"{"protocol": 2, "format": 7}"#).unwrap();
        assert!(compatible(&super::info(&banks, "b").map(|mut i| { i.protocol = Some(crate::ipc::PROTOCOL); i }).unwrap(), Some(&old)).unwrap_err().contains("format 7"));
        // A release that reads game tables of another format than the running bank's.
        let tables = |f: u64| bi(json!({"version": "9.9.9", "tag": "v9.9.9", "protocol": crate::ipc::PROTOCOL, "format": 1, "tablesFormat": f}));
        let live = bi(json!({"format": 1, "tablesFormat": 1}));
        assert!(compatible(&tables(1), Some(&live)).is_ok());
        let e = compatible(&tables(2), Some(&live)).unwrap_err();
        assert!(e.contains("tables of format 2") && e.contains("are of format 1") && e.contains("pcds upgrade --restart v9.9.9"), "{e}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn releases_come_over_https_with_plain_tags_and_files() {
        // http only from 127.0.0.1, which only a test's PCDS_RELEASES names.
        assert!(local("http://127.0.0.1:8080/releases") && !local("http://127.0.0.1.evil.example/releases") && !local("http://example.com/releases") && !local(RELEASES));
        let e = get(&agent(false), "http://127.0.0.1:9/releases/latest").unwrap_err();
        assert!(e.to_lowercase().contains("https"), "{e}");
        for good in ["v0.6.0", "0.6.0", "v1.0.0-rc.1", "v2_b"] {
            check_tag(good).unwrap();
        }
        for bad in ["", "v", "v1/../../x", "v1..2", "v1 2", "v1;rm", "../x", "v1/2"] {
            assert!(check_tag(bad).is_err(), "{bad:?}");
        }
        assert!(lookup("http://127.0.0.1:9/releases", Some("../../x")).unwrap_err().contains("not a release version"));
        // A tarball with a link in it: nothing installed.
        let dir = test_dir("links");
        let tar = {
            let name = "pcds-v9.9.9-linux-x86_64";
            std::fs::create_dir_all(dir.join(name)).unwrap();
            std::fs::write(dir.join(name).join("pcds"), script(crate::ipc::PROTOCOL)).unwrap();
            std::os::unix::fs::symlink("/etc/passwd", dir.join(name).join("web")).unwrap();
            assert!(Command::new("tar").arg("-czf").arg(dir.join("t.tar.gz")).arg("-C").arg(&dir).arg(name).status().unwrap().success());
            std::fs::remove_dir_all(dir.join(name)).unwrap();
            std::fs::read(dir.join("t.tar.gz")).unwrap()
        };
        let sum = to_hex(ring::digest::digest(&ring::digest::SHA256, &tar).as_ref());
        let base = format!("{}/releases", serve(release("BASE", "v9.9.9", tar, sum)));
        let banks = dir.join("bin");
        let e = install(&lookup(&base, None).unwrap(), &banks, "b", None).unwrap_err();
        assert!(e.contains("web, which is not a file or a folder"), "{e}");
        assert!(!banks.join("b").exists());
        std::fs::remove_dir_all(&dir).unwrap();
        // A hard link: a file with two names looks like a file. The tar lists it as a link and the
        // unpacked bank holds one inode twice.
        let dir = test_dir("hardlinks");
        let tar = {
            let name = "pcds-v9.9.8-linux-x86_64";
            std::fs::create_dir_all(dir.join(name)).unwrap();
            std::fs::write(dir.join(name).join("pcds"), script(crate::ipc::PROTOCOL)).unwrap();
            std::fs::hard_link(dir.join(name).join("pcds"), dir.join(name).join("web")).unwrap();
            assert!(Command::new("tar").arg("-czf").arg(dir.join("t.tar.gz")).arg("-C").arg(&dir).arg(name).status().unwrap().success());
            std::fs::remove_dir_all(dir.join(name)).unwrap();
            std::fs::read(dir.join("t.tar.gz")).unwrap()
        };
        let sum = to_hex(ring::digest::digest(&ring::digest::SHA256, &tar).as_ref());
        let base = format!("{}/releases", serve(release("BASE", "v9.9.8", tar, sum)));
        let banks = dir.join("bin");
        let e = install(&lookup(&base, None).unwrap(), &banks, "b", None).unwrap_err();
        assert!(e.contains("which is not a file or a folder") && !banks.join("b").exists(), "{e}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A hard link in a release tarball looks like a file once extracted: it is refused by its link
    /// count (a crafted tar, made by GNU tar from two names of one file), also in a folder.
    #[test]
    fn a_hard_link_in_a_tarball_is_refused() {
        let dir = test_dir("hardlink");
        let name = "pcds-v9.9.9-linux-x86_64";
        std::fs::create_dir_all(dir.join(name).join("web")).unwrap();
        std::fs::write(dir.join(name).join("pcds"), script(crate::ipc::PROTOCOL)).unwrap();
        std::fs::write(dir.join(name).join("web/a.html"), "<html></html>").unwrap();
        std::fs::hard_link(dir.join(name).join("web/a.html"), dir.join(name).join("web/b.html")).unwrap();
        let out = dir.join("t.tar.gz");
        assert!(Command::new("tar").arg("-czf").arg(&out).arg("-C").arg(&dir).arg(name).status().unwrap().success());
        let listing = Command::new("tar").arg("-tvzf").arg(&out).output().unwrap().stdout;
        assert!(String::from_utf8_lossy(&listing).lines().any(|l| l.starts_with('h')), "the crafted tar holds a hard link entry");
        let tar = std::fs::read(&out).unwrap();
        let sum = to_hex(ring::digest::digest(&ring::digest::SHA256, &tar).as_ref());
        let base = format!("{}/releases", serve(release("BASE", "v9.9.9", tar, sum)));
        let banks = dir.join("bin");
        let e = install(&lookup(&base, None).unwrap(), &banks, "b", None).unwrap_err();
        assert!(e.contains("web/") && e.contains(".html, which is not a file or a folder") && e.contains("hard link"), "{e}");
        assert!(!banks.join("b").exists());
        // A plain tree with no second names passes.
        let plain = dir.join("plain");
        std::fs::create_dir_all(plain.join("web")).unwrap();
        std::fs::write(plain.join("web/a.html"), "x").unwrap();
        std::fs::write(plain.join("pcds"), "x").unwrap();
        assert_eq!(plain_files(&plain, &plain), Ok(()));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// PCDS_RELEASES may be a private mirror with a token in it: only scheme and host are shown,
    /// also in the errors of a lookup that fails.
    #[test]
    fn a_releases_url_is_shown_as_scheme_and_host_only() {
        for (url, shown) in [
            ("https://api.github.com/repos/x/y/releases", "https://api.github.com"),
            ("https://user:tok@mirror.example:8443/r?token=abc#f", "https://mirror.example:8443"),
            ("http://127.0.0.1:9/releases?k=v", "http://127.0.0.1:9"),
            ("https://tok@host?x=1", "https://host"),
            ("no scheme/at all?token=abc", "(not a URL)"),
        ] {
            assert_eq!(shown_url(url), shown, "{url}");
        }
        let url = "http://user:s3cr3t@127.0.0.1:9/releases/latest?token=t0k3n";
        for e in [get(&agent(true), url).unwrap_err(), lookup("http://user:s3cr3t@127.0.0.1:9/releases?token=t0k3n", None).unwrap_err()] {
            assert!(e.starts_with("http://127.0.0.1:9: ") && !e.contains("s3cr3t") && !e.contains("t0k3n"), "{e}");
        }
    }

    /// bank-info runs with PATH, HOME and LANG only: no token of the server reaches it.
    #[test]
    fn bank_info_sees_no_more_than_path_home_and_lang() {
        let dir = test_dir("env");
        let program = dir.join("pcds");
        std::fs::write(&program, "#!/bin/sh\nprintf '{\"protocol\":1,\"env\":\"%s\"}\\n' \"$(env | cut -d= -f1 | sort | tr '\\n' ' ')\"\n").unwrap();
        std::fs::set_permissions(&program, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        let info = ask(&program).unwrap();
        let seen: Vec<&str> = info.extra["env"].as_str().unwrap().split_whitespace().collect();
        // What the shell sets itself (PWD, SHLVL, _) aside.
        assert!(seen.contains(&"PATH") && seen.iter().all(|k| ["PATH", "HOME", "LANG", "PWD", "OLDPWD", "SHLVL", "_"].contains(k)), "{seen:?}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_automatic_update_leaves_out_the_release_rolled_back_from() {
        let dir = test_dir("rolled-back");
        std::fs::create_dir_all(dir.join("b")).unwrap();
        std::fs::write(dir.join("b/pcds"), "").unwrap();
        write_info(&dir.join("b"), &bi(json!({"version": "0.7.0", "tag": "v0.7.0"}))).unwrap();
        assert_eq!(auto_skips(&dir, "v0.7.0").unwrap(), None, "no rollback: nothing left out");
        set_rolled_back(&dir, "b").unwrap();
        assert_eq!(rolled_back(&dir).unwrap(), RolledBack { bank: "b".into(), tag: Some("v0.7.0".into()), reason: None, at: None });
        assert!(auto_skips(&dir, "v0.7.0").unwrap().unwrap().contains("rolled back from"));
        assert!(rolled_back(&dir).is_some(), "an older or the same release keeps the note");
        assert_eq!(auto_skips(&dir, "v0.6.5").unwrap(), None);
        assert!(rolled_back(&dir).is_some());
        // A later release: installed, and the note is gone.
        assert_eq!(auto_skips(&dir, "v0.8.0").unwrap(), None);
        assert!(rolled_back(&dir).is_none());
        // A bank of a local build is noted by its version.
        write_info(&dir.join("b"), &bi(json!({"version": "0.7.1"}))).unwrap();
        set_rolled_back(&dir, "b").unwrap();
        assert_eq!(rolled_back(&dir).unwrap().tag.as_deref(), Some("v0.7.1"));
        clear_rolled_back(&dir).unwrap();
        clear_rolled_back(&dir).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A release that keeps ending the game is given up for the other bank, with the reason in
    /// the note; never for an empty bank, nor for the bank the server was rolled back from.
    #[test]
    fn a_release_that_keeps_crashing_is_given_up_for_the_other_bank_when_there_is_one() {
        let dir = test_dir("crash-rollback");
        let bank = |name: &str, tag: &str| {
            std::fs::create_dir_all(dir.join(name)).unwrap();
            std::fs::write(dir.join(name).join("pcds"), "").unwrap();
            write_info(&dir.join(name), &bi(json!({"version": "0.7.0", "tag": tag}))).unwrap();
        };
        bank("b", "v0.7.0");
        assert!(crash_rollback_target(&dir, "b").unwrap_err().contains("bank a holds no release"), "an empty bank is not gone back to");
        bank("a", "v0.6.0");
        assert_eq!(crash_rollback_target(&dir, "b").unwrap(), "a");
        set_rolled_back_by_crashes(&dir, "b", "ended the game 6 times").unwrap();
        let note = rolled_back(&dir).unwrap();
        assert_eq!((note.bank.as_str(), note.tag.as_deref(), note.reason.as_deref()), ("b", Some("v0.7.0"), Some("ended the game 6 times")));
        assert!(note.at.unwrap() > 0);
        assert_eq!(describe(&dir)["rolledBack"], serde_json::to_value(&note).unwrap(), "shown in the banks of /api/status");
        // The automatic update leaves that release out, like any rollback.
        assert!(auto_skips(&dir, "v0.7.0").unwrap().unwrap().contains("rolled back from"));
        // And a bank that was rolled back from is not the target of the next give-up.
        assert!(crash_rollback_target(&dir, "a").unwrap_err().contains("bank b is the one the server was rolled back from"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_fallback_is_noted_while_its_bank_is_active() {
        let dir = test_dir("fallback");
        set_active(&dir, "a").unwrap();
        note_fallback(&dir, "b", "a", "could not start the world").unwrap();
        let shown = describe(&dir)["fallback"].clone();
        assert_eq!((shown["from"].as_str(), shown["to"].as_str(), shown["reason"].as_str()), (Some("b"), Some("a"), Some("could not start the world")));
        // A flip or an upgrade to the other bank: no longer what runs.
        set_active(&dir, "b").unwrap();
        assert!(describe(&dir)["fallback"].is_null());
        set_active(&dir, "a").unwrap();
        clear_fallback(&dir).unwrap();
        assert!(fallback(&dir).is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// banks/active says a or b. A fresh installation has no file and starts from bank a; a file
    /// that says anything else is an error that says what to do, not bank a, and the status shows it.
    #[test]
    fn an_active_file_that_says_neither_bank_is_an_error_and_not_bank_a() {
        let dir = test_dir("active-garbage");
        for garbage in ["", "c\n", "bank b", "\u{0}\u{0}"] {
            std::fs::write(dir.join("active"), garbage).unwrap();
            let e = active(&dir).unwrap_err();
            assert!(e.contains("active") && e.contains("must say a or b") && e.contains("delete the file"), "{garbage:?}: {e}");
            assert_eq!(describe(&dir)["error"], json!(e));
            assert_eq!(describe(&dir)["active"], Value::Null);
        }
        // Spaces around the name do not matter; a directory in its place is an error too.
        std::fs::write(dir.join("active"), " b \n").unwrap();
        assert_eq!(active(&dir).unwrap(), "b");
        assert!(describe(&dir).get("error").is_none());
        std::fs::remove_file(dir.join("active")).unwrap();
        std::fs::create_dir(dir.join("active")).unwrap();
        assert!(active(&dir).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_active_bank_and_versions() {
        let dir = test_dir("active");
        assert_eq!(active(&dir).unwrap(), "a");
        set_active(&dir, "b").unwrap();
        assert_eq!((active(&dir).unwrap(), other("b"), other("a")), ("b".to_string(), "a", "b"));
        assert!(newer("v0.6.0", "0.5.0") && newer("v0.10.0", "0.9.9") && !newer("v0.5.0", "0.5.0") && !newer("v0.4.9", "0.5.0"));
        // Semantic version order: a pre-release before its release, its parts one by one.
        let order = ["0.9.9", "1.0.0-alpha", "1.0.0-alpha.1", "1.0.0-alpha.beta", "1.0.0-beta.2", "1.0.0-beta.11", "1.0.0-rc.1", "1.0.0", "v1.0.1", "1.1"];
        for (i, a) in order.iter().enumerate() {
            for (j, b) in order.iter().enumerate() {
                assert_eq!(newer(b, a), j > i, "{b} after {a}");
            }
        }
        assert!(!newer("1.0.0+build.2", "1.0.0") && !newer("v1.0", "1.0.0"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A release whose program says an older version than its tag (a forgotten bump) is the
    /// release installed: the tag is compared, not the version, so the check does not find it
    /// "available" for ever (the automatic update would flip to it at every check). A bank
    /// without a tag (a build, a release folder's program) is compared by its version.
    #[test]
    fn a_release_is_compared_by_its_tag_when_the_bank_has_one() {
        let dir = test_dir("held");
        for (bank, info) in [("a", json!({"version": "0.6.1", "tag": "v0.6.2"})), ("b", json!({"version": "0.6.1"}))] {
            std::fs::create_dir_all(dir.join(bank)).unwrap();
            std::fs::write(dir.join(bank).join("pcds"), "").unwrap();
            write_info(&dir.join(bank), &bi(info)).unwrap();
        }
        let held = held_release(&dir, "a", "0.6.1");
        assert_eq!(held, "v0.6.2");
        assert!(!newer("v0.6.2", &held), "the release that is installed is not available again");
        assert!(newer("v0.6.3", &held));
        let held = held_release(&dir, "b", "0.6.1");
        assert_eq!(held, "0.6.1", "no tag: the program's version");
        assert!(newer("v0.6.2", &held));
        assert_eq!(held_release(&dir, "none", "0.5.0"), "0.5.0", "no bank.json at all");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A server on 127.0.0.1 that answers GET path with the raw bytes given (a whole HTTP
    /// response, "BASE" replaced by its address): a status other than 200, a redirect, a body
    /// shorter than its Content-Length.
    fn raw(routes: Vec<(&str, String)>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let routes: Vec<(String, String)> = routes.into_iter().map(|(p, b)| (p.to_string(), b.replace("BASE", &base))).collect();
        std::thread::spawn(move || {
            for mut s in listener.incoming().flatten() {
                let mut line = String::new();
                let mut reader = BufReader::new(s.try_clone().unwrap());
                reader.read_line(&mut line).unwrap();
                while reader.read_line(&mut String::new()).is_ok_and(|n| n > 2) {}
                let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
                let answer = routes.iter().find(|(p, _)| *p == path).map(|(_, b)| b.clone()).unwrap_or_else(|| "HTTP/1.1 404 Not Found\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}".into());
                let _ = s.write_all(answer.as_bytes());
            }
        });
        format!("{base}/releases")
    }

    /// What GitHub (or a mirror) does when it is not well: rate limits, a redirect, a release
    /// without the assets, a body cut short, nonsense. Each is an error that says what, and
    /// nothing is installed; a redirect is followed.
    #[test]
    fn a_release_feed_that_is_not_well_is_an_error_and_nothing_is_installed() {
        let ok = |body: &str| format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
        let no_assets = r#"{"tag_name": "v1.1.0", "assets": [{"name": "pcds-v1.1.0-linux-x86_64.tar.gz", "browser_download_url": "http://x/x"}]}"#;
        let base = raw(vec![
            ("/releases/latest", "HTTP/1.1 403 Forbidden\r\nContent-Length: 24\r\nConnection: close\r\n\r\n{\"message\":\"rate limit\"}".into()),
            ("/releases/tags/v1.0.0", "HTTP/1.1 302 Found\r\nLocation: BASE/elsewhere\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into()),
            ("/elsewhere", ok(r#"{"tag_name": "v1.0.0", "assets": []}"#)),
            ("/releases/tags/v1.1.0", ok(no_assets)),
            ("/releases/tags/v1.2.0", ok("not json")),
            ("/releases/tags/v1.3.0", ok(r#"{"assets": []}"#)),
            ("/releases/tags/v1.4.0", "HTTP/1.1 200 OK\r\nContent-Length: 500\r\nConnection: close\r\n\r\n{\"tag_name\"".into()),
            ("/releases/tags/v1.5.0", ok(r#"{"tag_name": "v1.5.0/../../x", "assets": []}"#)),
            ("/releases/tags/v1.6.0", "HTTP/1.1 429 Too Many Requests\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}".into()),
        ]);
        let err = |version: Option<&str>| lookup(&base, version).unwrap_err();
        assert!(err(None).contains("HTTP 403"), "{}", err(None));
        assert!(err(Some("v1.6.0")).contains("HTTP 429"));
        // The redirect is followed (to a release that has no assets).
        assert!(err(Some("v1.0.0")).contains("has no pcds-v1.0.0-linux-x86_64.tar.gz"), "{}", err(Some("v1.0.0")));
        assert!(err(Some("v1.1.0")).contains("has no pcds-v1.1.0-linux-x86_64.tar.gz.sha256"), "{}", err(Some("v1.1.0")));
        assert!(err(Some("v1.2.0")).starts_with("http://127.0.0.1:"), "nonsense is an error naming the server (not the path: it may hold a token)");
        assert!(err(Some("v1.3.0")).contains("no tag_name"));
        assert!(!err(Some("v1.4.0")).is_empty(), "a body cut short is an error");
        assert!(err(Some("v1.5.0")).contains("not a release version"), "a tag from the feed is checked as a typed one is");
        assert!(err(Some("v9.9.9")).contains("404"));
    }

    /// A tarball that is cut short or is not gzip at all (its sha256 matches: the feed served
    /// it so), one without a program, one whose program cannot run, and a tarball that cannot
    /// be written to the disk: each leaves the bank as it was and nothing beside it.
    #[test]
    fn a_broken_tarball_is_refused_and_leaves_the_banks_as_they_were() {
        let dir = test_dir("broken-tarball");
        let banks = dir.join("bin");
        std::fs::create_dir_all(banks.join("b")).unwrap();
        std::fs::write(banks.join("b/pcds"), "the old one").unwrap();
        let serve_tar = |tag: &str, tar: Vec<u8>| {
            let sum = to_hex(ring::digest::digest(&ring::digest::SHA256, &tar).as_ref());
            lookup(&format!("{}/releases", serve(release("BASE", tag, tar, sum))), None).unwrap()
        };
        let good = tarball(&dir, "v9.9.9", &script(crate::ipc::PROTOCOL));
        // Cut short: tar fails.
        let release_cut = serve_tar("v9.9.9", good[..good.len() / 2].to_vec());
        let e = install(&release_cut, &banks, "b", None).unwrap_err();
        assert!(e.contains("tar could not unpack it"), "{e}");
        // Not a tarball.
        let e = install(&serve_tar("v9.9.9", b"<html>an error page</html>".to_vec()), &banks, "b", None).unwrap_err();
        assert!(e.contains("tar could not unpack it"), "{e}");
        // A tarball without a pcds in it.
        let empty = {
            let root = dir.join("pcds-v9.9.9-linux-x86_64");
            std::fs::create_dir_all(&root).unwrap();
            std::fs::write(root.join("README.md"), "no program").unwrap();
            assert!(Command::new("tar").arg("-czf").arg(dir.join("e.tar.gz")).arg("-C").arg(&dir).arg("pcds-v9.9.9-linux-x86_64").status().unwrap().success());
            std::fs::read(dir.join("e.tar.gz")).unwrap()
        };
        let e = install(&serve_tar("v9.9.9", empty), &banks, "b", None).unwrap_err();
        assert!(e.contains("no pcds in the tarball"), "{e}");
        // A pcds that is not executable cannot be run: that is not an answer to bank-info.
        let plain = {
            let root = dir.join("pcds-v9.9.8-linux-x86_64");
            std::fs::create_dir_all(&root).unwrap();
            std::fs::write(root.join("pcds"), script(crate::ipc::PROTOCOL)).unwrap();
            assert!(Command::new("tar").arg("-czf").arg(dir.join("p.tar.gz")).arg("-C").arg(&dir).arg("pcds-v9.9.8-linux-x86_64").status().unwrap().success());
            std::fs::read(dir.join("p.tar.gz")).unwrap()
        };
        let e = install(&serve_tar("v9.9.8", plain), &banks, "b", None).unwrap_err();
        assert!(e.contains("cannot be run") && !e.contains("does not answer"), "{e}");
        // A disk that takes no more (/dev/full behind the tarball's name): the partial file is
        // removed, and the bank is as it was.
        let release_good = serve_tar("v9.9.9", good);
        let file = std::path::PathBuf::from(format!("{}.tar.gz", staging(&banks, "b").display()));
        std::os::unix::fs::symlink("/dev/full", &file).unwrap();
        let e = install(&release_good, &banks, "b", None).unwrap_err();
        assert!(e.contains(".tar.gz"), "{e}");
        assert!(file.symlink_metadata().is_err(), "the partial tarball is removed");
        assert_eq!(std::fs::read_to_string(banks.join("b/pcds")).unwrap(), "the old one");
        assert_eq!(names(&banks), ["b"], "nothing left beside the bank");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Going back to the release before, when it cannot read what the running one writes (the
    /// running world is in snapshot format 3, that release is in 2): refused with `--rollback --restart`, not
    /// `--restart VERSION`, also for a bank that holds a build and has no tag; an update names
    /// the release by its tag, or by its version when there is no tag.
    #[test]
    fn a_rollback_across_a_snapshot_format_is_a_restart_and_says_which() {
        let before = bi(json!({"version": "0.6.0", "protocol": crate::ipc::PROTOCOL, "format": 2, "tablesFormat": 2}));
        let live = bi(json!({"format": 3, "tablesFormat": 2}));
        let live = Some(&live);
        let e = compatible_for(&before, live, true).unwrap_err();
        assert!(e.contains("a rollback across a change of the snapshot format") && e.contains("pcds upgrade --rollback --restart") && !e.contains("VERSION") && e.contains("disconnects every player"), "{e}");
        let e = compatible_for(&before, live, false).unwrap_err();
        assert!(e.contains("pcds upgrade --restart v0.6.0") && !e.contains("VERSION"), "{e}");
        let mut tagged = before.clone();
        tagged.tag = Some("v0.6.0".into());
        assert!(compatible_for(&tagged, live, false).unwrap_err().contains("pcds upgrade --restart v0.6.0"));
        // The other refusals of a rollback name the same command.
        let mut other = tagged.clone();
        other.protocol = Some(2);
        assert!(compatible_for(&other, live, true).unwrap_err().contains("(pcds upgrade --rollback --restart)"));
        let mut other = tagged.clone();
        other.tables_format = Some(1);
        assert!(compatible_for(&other, live, true).unwrap_err().contains("pcds upgrade --rollback --restart goes back, reads"));
        // The release in the format of the running world goes back hot.
        let mut same = before.clone();
        same.format = Some(3);
        assert!(compatible_for(&same, live, true).is_ok());
    }

    /// A banks folder that cannot be written (permissions): the install fails with the path, the
    /// bank is as it was, nothing is left beside it. (Not as root: root writes anywhere.)
    #[test]
    fn a_banks_folder_that_cannot_be_written_fails_the_install_and_changes_nothing() {
        // SAFETY: geteuid only reads.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let dir = test_dir("read-only");
        let base = fake(&dir, "v9.9.9", &script(crate::ipc::PROTOCOL), false);
        let banks = dir.join("bin");
        std::fs::create_dir_all(banks.join("b")).unwrap();
        std::fs::write(banks.join("b/pcds"), "the old one").unwrap();
        // The folder itself read-only: no staging folder can be made beside the banks.
        std::fs::set_permissions(&banks, std::os::unix::fs::PermissionsExt::from_mode(0o555)).unwrap();
        let e = install(&lookup(&base, None).unwrap(), &banks, "b", None).unwrap_err();
        std::fs::set_permissions(&banks, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        assert!(e.contains("bin") && e.to_lowercase().contains("denied"), "{e}");
        assert_eq!(std::fs::read_to_string(banks.join("b/pcds")).unwrap(), "the old one");
        assert_eq!(names(&banks), ["b"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

#[cfg(any(fuzzing, pcds_corpus))]
#[path = "../fuzz/targets/updater.rs"]
pub mod fuzz;
