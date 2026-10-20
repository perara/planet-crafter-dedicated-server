//! The banks the front runs: child processes of the same program on a unix socket each (ipc.rs).
//! Here are starting one (`Front::spawn`), waiting for its first frames (at the boot and for a
//! restart: nobody plays yet, so the front may wait), and the loop's reading of the live bank and
//! the next one, which hands every frame to whoever handles its kind:
//!
//! | frame, from the live bank | handled by |
//! |---|---|
//! | DATA, KICK | clients.rs, transport.rs: a batch to a client, a kick |
//! | ANSWER | admin_routes.rs: the reply to an admin request |
//! | CHAT, CHATCFG | chat_hooks.rs: the feed and the hooks |
//! | OPS | ops.rs: the rules the front acts on |
//! | SNAPSHOT | flip.rs: the world the old bank hands over |
//!
//! and from the next bank, HELLO and READY, flip.rs (a flip) or supervise.rs (a restart).

use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::{Duration, Instant, SystemTime};

use super::{Front, Phase, BOOT_FAILED, KICK_DELAY, LOAD_DEADLINE, STOP_GRACE};
use crate::ipc::{self, Frame, Link};
use crate::msg::{self, BankInfo, Hello, Restore};
use crate::updater;

/// A bank: its process, the socket to it, and what it said.
pub(super) struct Bank {
    /// "a" or "b".
    pub name: String,
    pub child: Child,
    pub link: Link,
    /// Its HELLO (None until it came).
    pub hello: Option<Hello>,
    pub started: Instant,
    /// The last frame it sent: the watchdog's clock (supervise::watch_verdict).
    pub heard: Instant,
}

impl Bank {
    /// The version its HELLO says.
    pub fn version(&self) -> Option<&str> {
        self.hello.as_ref().and_then(|h| h.info.version.as_deref())
    }

    /// The game version of the tables it loaded, "" before its HELLO.
    pub fn game_version(&self) -> &str {
        self.hello.as_ref().and_then(|h| h.game_version.as_deref()).unwrap_or("")
    }

    /// What its HELLO says it is.
    pub fn info(&self) -> Option<&BankInfo> {
        self.hello.as_ref().map(|h| &h.info)
    }
}

/// A bank whose socket closed (it is ending) or broke (it is killed): waited for; how it ended.
pub(super) fn end(b: &mut Bank) -> String {
    if let Some(why) = &b.link.broken {
        eprintln!("bank {} (pid {}): {why}; killed", b.name, b.child.id());
        let _ = b.child.kill();
    }
    let status = b.child.wait().map(|s| s.to_string()).unwrap_or_default();
    match &b.link.broken {
        Some(why) => format!("{status}, after {why}"),
        None => status,
    }
}

/// A bank's next frame, read until `deadline` or its end.
fn wait_frame(b: &mut Bank, deadline: Instant) -> Option<Frame> {
    loop {
        let _ = b.link.flush();
        if let Some(f) = b.link.next() {
            return Some(f);
        }
        if Instant::now() > deadline {
            let _ = b.child.kill();
            let _ = b.child.wait();
            return None;
        }
        ipc::wait(&[b.link.fd()], &[b.link.pending()], Duration::from_millis(100));
        if !b.link.fill(usize::MAX) {
            let status = end(b);
            eprintln!("bank {} ended ({status})", b.name);
            return None;
        }
    }
}

/// A bank's HELLO, refused unless it speaks this front's protocol (at the boot, a flip and a
/// restart after a crash alike: a world never goes to a bank that reads the frames otherwise).
pub(super) fn hello_of(f: &Frame) -> Result<Hello, String> {
    if f.kind != ipc::HELLO {
        return Err(format!("frame {} instead of HELLO", f.kind));
    }
    let mut hello: Hello = msg::parse(&f.body).map_err(|e| format!("its HELLO is not JSON ({e})"))?;
    hello.info.protocol = Some(f.n);
    if f.n != ipc::PROTOCOL {
        // The same words as an update refused for its protocol.
        updater::compatible(&hello.info, None)?;
    }
    hello.info.protocol = None;
    Ok(hello)
}

impl Front {
    /// The program of a bank: its folder's pcds, or this program without banks.
    pub(super) fn exe(&self, bank: &str) -> PathBuf {
        match &self.banks {
            Some(dir) => dir.join(bank).join("pcds"),
            None => std::env::current_exe().expect("this program"),
        }
    }

    /// The web page of a bank's release.
    pub(super) fn web_page(&self, bank: &str) -> Option<String> {
        let dir = self.exe(bank);
        Some(dir.with_file_name("web").join("index.html").to_string_lossy().to_string())
    }

    /// A bank process on a socket of its own (its fd 3).
    pub(super) fn spawn(&self, bank: &str, fail: Option<&str>) -> Result<Bank, String> {
        let (ours, theirs) = UnixStream::pair().map_err(|e| format!("socketpair: {e}"))?;
        ours.set_nonblocking(true).map_err(|e| e.to_string())?;
        let exe = self.exe(bank);
        let mut command = Command::new(&exe);
        command.arg("bank").arg(&self.dir).arg(&self.save).env("PCDS_IPC_FD", "3");
        if let Some(f) = fail {
            command.env("PCDS_DEV", format!("{},bank-fail={f}", crate::env("PCDS_DEV").unwrap_or_default()));
        }
        let fd = theirs.as_raw_fd();
        // SAFETY: between fork and exec only async-signal-safe calls: dup2 and fcntl. fd 3 then
        // is the socket, without close-on-exec.
        unsafe {
            command.pre_exec(move || {
                if fd == 3 {
                    libc::fcntl(3, libc::F_SETFD, 0);
                } else if libc::dup2(fd, 3) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn().map_err(|e| format!("{}: {e}", exe.display()))?;
        Ok(Bank { name: bank.to_string(), child, link: Link::new(ours), hello: None, started: Instant::now(), heard: Instant::now() })
    }

    /// The first bank, waited for (nobody is connected yet): the active one, else the other one
    /// when the active one cannot start (a broken update), as a bootloader falls back.
    pub(super) fn boot(&mut self) {
        // A rollback the previous front made for crashes is in the history of this one (the
        // note outlives it: banks/rolled-back, until a later release or an update clears it).
        if let Some(r) = self.banks.as_deref().and_then(updater::rolled_back).filter(|r| r["reason"].is_string()) {
            let message = format!("the release in bank {} ({}) was given up: {}; the server went back to bank {}", r["bank"].as_str().unwrap_or("?"), r["tag"].as_str().unwrap_or("?"), r["reason"].as_str().unwrap_or(""), updater::other(r["bank"].as_str().unwrap_or("")));
            let text = |k: &str| r[k].as_str().map(str::to_string);
            self.updates.history.push_back(super::history::History::rolled_back(message, r["at"].as_u64(), text("bank"), text("tag")));
        }
        let first = self.banks.as_deref().map(updater::active).unwrap_or_else(|| "a".into());
        let mut kept = self.kept_snapshot().unwrap_or_else(|e| {
            eprintln!("{e}");
            std::process::exit(1)
        });
        let mut tried = Vec::new();
        for bank in [first.clone(), updater::other(&first).to_string()] {
            if self.banks.as_ref().is_some_and(|b| !b.join(&bank).join("pcds").is_file()) {
                continue;
            }
            let snapshot = match kept.as_ref().map(|(fd, _)| fd.try_clone()).transpose() {
                Ok(fd) => fd,
                Err(e) => {
                    eprintln!("{}: {e}", super::snapshot::snapshot_path(&self.save));
                    std::process::exit(1);
                }
            };
            match self.boot_bank(&bank, snapshot, &mut kept) {
                Ok(b) => {
                    println!("bank {bank} (pid {}, pcds {}) running the world after {:?}", b.child.id(), b.version().unwrap_or("?"), b.started.elapsed());
                    if let Some((fd, at)) = kept {
                        println!("the world is the one kept in {} at the last stop (the last flip's snapshot, newer than the save)", super::snapshot::snapshot_path(&self.save));
                        self.last_snapshot = Some((fd, at));
                    }
                    if bank != first {
                        let reason = tried.join("; ");
                        eprintln!("bank {first} could not start the world ({reason}); running bank {bank}, which ran before");
                        if let Some(dir) = &self.banks
                            && let Err(e) = updater::note_fallback(dir, &first, &bank, &format!("could not load the world: {reason}"))
                        {
                            eprintln!("banks: {e}");
                        }
                    }
                    self.game_version = b.game_version().to_string();
                    self.admin.set_page(self.web_page(&bank));
                    if let Some(dir) = &self.banks
                        && let Err(e) = updater::set_active(dir, &bank)
                    {
                        eprintln!("banks: {e}");
                    }
                    self.live = Some(Bank { heard: Instant::now(), ..b });
                    return;
                }
                Err(e) => tried.push(format!("bank {bank}: {e}")),
            }
            if self.banks.is_none() {
                break;
            }
        }
        eprintln!("no bank could start the world: {}", tried.join("; "));
        if kept.is_some() {
            eprintln!("they were given the world kept in {} at the last stop; move it away to start from the save instead", super::snapshot::snapshot_path(&self.save));
        }
        std::process::exit(BOOT_FAILED);
    }

    /// A bank started on the world: the kept snapshot's, else the save's. A kept snapshot this
    /// bank can never run (another game's tables, a format it does not read) is moved aside,
    /// and `kept` emptied: the world starts from the save.
    pub(super) fn boot_bank(&self, bank: &str, mut snapshot: Option<OwnedFd>, kept: &mut Option<(OwnedFd, SystemTime)>) -> Result<Bank, String> {
        let mut b = self.spawn(bank, None)?;
        let deadline = Instant::now() + LOAD_DEADLINE;
        let hello = wait_frame(&mut b, deadline).ok_or("it ended before its hello (see above)")?;
        let hello = hello_of(&hello).inspect_err(|_| {
            let _ = b.child.kill();
            let _ = b.child.wait();
        })?;
        if let Some(why) = snapshot.as_ref().and_then(|fd| ipc::read_memfd(fd).ok()).and_then(|s| super::snapshot::never_runs(&s, &hello)) {
            let path = super::snapshot::snapshot_path(&self.save);
            let aside = format!("{path}.unusable-{}", crate::save::stamp(SystemTime::now()));
            match std::fs::rename(&path, &aside) {
                Ok(()) => eprintln!("{path} can never run ({why}): moved to {aside}; the world starts from the save, which is older"),
                Err(e) => eprintln!("{path} can never run ({why}), and could not be moved aside ({e}); the world starts from the save, which is older"),
            }
            (snapshot, *kept) = (None, None);
        }
        b.hello = Some(hello);
        match snapshot {
            Some(fd) => b.link.queue(ipc::RESTORE, 0, &msg::body(&Restore { recover: true }), Some(fd), 0),
            None => b.link.queue(ipc::START, 0, &[], None, 0),
        }
        let ready = wait_frame(&mut b, deadline).ok_or("it ended while loading the world (see above)")?;
        if ready.kind != ipc::READY {
            let _ = b.child.kill();
            let _ = b.child.wait();
            return Err(format!("frame {} instead of READY", ready.kind));
        }
        Ok(b)
    }

    /// What the banks' sockets take now (a bank that is gone shows at the next read).
    pub(super) fn flush_banks(&mut self) {
        for b in [self.live.as_mut(), self.next.as_mut()].into_iter().flatten() {
            let _ = b.link.flush();
        }
    }

    /// Reads both banks. Whether anything went out to a client.
    pub(super) fn read_banks(&mut self) -> bool {
        let mut sent = false;
        // Up to 4 MiB a loop from each: the clients' sessions must not wait on a flood.
        // A bank whose bytes are no frames (ipc::MAX_FRAME) counts as one that ended.
        let live_alive = self.live.as_mut().is_none_or(|b| b.link.fill(4 << 20));
        while let Some(f) = self.live.as_mut().and_then(|b| b.link.next()) {
            sent |= self.on_live(f);
        }
        if !live_alive || self.live.as_ref().is_some_and(|b| b.link.broken.is_some()) {
            self.live_died();
        }
        let next_alive = self.next.as_mut().is_none_or(|b| b.link.fill(4 << 20));
        while let Some(f) = self.next.as_mut().and_then(|b| b.link.next()) {
            self.on_next(f);
        }
        // READY may have made it the live one, with frames after READY in its inbox.
        while let Some(f) = self.live.as_mut().and_then(|b| b.link.next()) {
            sent |= self.on_live(f);
        }
        if self.live.as_ref().is_some_and(|b| b.link.broken.is_some()) {
            self.live_died();
        }
        if (!next_alive || self.next.as_ref().is_some_and(|b| b.link.broken.is_some())) && self.next.is_some() {
            let status = self.next.as_mut().map(end).unwrap_or_default();
            match self.phase {
                Phase::Recovering => {
                    let b = self.next.take().unwrap();
                    eprintln!("bank {} ended while starting ({status})", b.name);
                    self.recover(&b.name);
                }
                _ => self.fail_flip(&format!("the new bank ended ({status})")),
            }
        }
        sent
    }

    /// A frame of the live bank. Whether it sent anything to a client.
    pub(super) fn on_live(&mut self, f: Frame) -> bool {
        if let Some(b) = self.live.as_mut() {
            b.heard = Instant::now();
        }
        match f.kind {
            ipc::DATA => return self.to_client(f.n, &f.body),
            ipc::KICK => {
                self.net.kick(f.n, Instant::now() + KICK_DELAY);
            }
            ipc::ANSWER => self.answer(f.n, &f.body),
            ipc::CHAT => match msg::parse(&f.body) {
                Ok(event) => self.hooks.feed.push(event),
                Err(e) => eprintln!("chat: an event the front cannot read ({e})"),
            },
            ipc::OPS => self.ops(&f.body),
            ipc::CHATCFG => match super::chat_hooks::chat_config(&f.body) {
                Ok(chat) => self.hooks.configure(chat),
                Err(e) => eprintln!("chat: {e}; the hooks keep their settings"),
            },
            ipc::SNAPSHOT => self.on_snapshot(f),
            _ => {}
        }
        false
    }

    /// The live bank told to save and stop, and waited for (within STOP_GRACE twice). Err: it
    /// ended without saving (it says where the world is kept), or was killed before it saved.
    pub(super) fn stop_live(&mut self) -> Result<(), String> {
        let Some(mut b) = self.live.take() else { return Ok(()) };
        b.link.queue(ipc::STOP, 0, &[], None, 0);
        let deadline = Instant::now() + STOP_GRACE;
        loop {
            if b.link.flush().is_err() || Instant::now() > deadline {
                break;
            }
            if let Ok(Some(_)) = b.child.try_wait() {
                break;
            }
            // Its last batches still drain, so its writes never block its save.
            if ipc::wait(&[b.link.fd()], &[b.link.pending()], Duration::from_millis(50))[0] && !b.link.fill(usize::MAX) {
                break;
            }
            while b.link.next().is_some() {}
        }
        match b.child.try_wait() {
            Ok(Some(_)) => {}
            _ if b.link.broken.is_some() => {
                end(&mut b);
            }
            _ => {
                let deadline = Instant::now() + STOP_GRACE;
                while Instant::now() < deadline && matches!(b.child.try_wait(), Ok(None)) {
                    std::thread::sleep(Duration::from_millis(50));
                }
                let _ = b.child.kill();
            }
        }
        match b.child.wait() {
            Ok(s) if !s.success() => Err(format!("bank {} ended ({s}) at the stop: the world may not be saved (see above)", b.name)),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bank_of_another_protocol_is_refused_on_every_path() {
        let hello = |kind, n, body: &str| hello_of(&Frame { kind, n, body: body.as_bytes().to_vec(), fd: None });
        assert_eq!(hello(ipc::HELLO, ipc::PROTOCOL, r#"{"version":"1"}"#).unwrap().info.version(), "1");
        assert!(hello(ipc::HELLO, 999, r#"{"version":"1"}"#).unwrap_err().contains("front protocol 999"));
        assert!(hello(ipc::READY, ipc::PROTOCOL, "{}").is_err());
        assert!(hello(ipc::HELLO, ipc::PROTOCOL, "not json").is_err());
    }
}
