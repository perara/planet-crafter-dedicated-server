//! Keeping a world in a bank: what happens when the live bank ends or hangs (the watchdog), when
//! the next one is late, and how often a world may be restarted before the front gives up (the
//! crash window, and the release that came by a flip given up for the one before it).

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use super::bank_link::end;
use super::{Front, Phase, CRASH_WINDOW, LOAD_DEADLINE, MAX_CRASHES, RESTARTED};
use crate::ipc;
use crate::msg::{self, AdminCall, Hello, Restore};
use crate::updater;

/// The next bank, once it has the snapshot, must say READY within this (the world stands).
pub(super) const READY_DEADLINE: Duration = Duration::from_secs(5);

/// The game's endings the front counts: more than MAX_CRASHES within CRASH_WINDOW and it gives up.
#[derive(Default)]
pub(super) struct CrashWindow(VecDeque<Instant>);

impl CrashWindow {
    /// An ending now; whether there are too many in the window.
    pub fn record(&mut self, now: Instant) -> bool {
        self.0.push_back(now);
        while self.0.front().is_some_and(|t| now - *t > CRASH_WINDOW) {
            self.0.pop_front();
        }
        self.0.len() > MAX_CRASHES
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// A new release starts with none.
    pub fn clear(&mut self) {
        self.0.clear();
    }
}

/// The rule limits.bankWatchdogSeconds, and when the live bank was last pinged.
pub(super) struct Watchdog {
    pub limit: Duration,
    pinged: Option<Instant>,
}

impl Watchdog {
    pub fn new(seconds: u64) -> Watchdog {
        Watchdog { limit: Duration::from_secs(seconds), pinged: None }
    }
}

/// What the watchdog does about the live bank.
#[derive(Debug, PartialEq)]
pub(super) enum Watch {
    /// Its clock does not run now (not playing): start counting again from here.
    Rest,
    Fine,
    /// Quiet for a while: ask it something.
    Ping,
    /// Silent this long: kill it.
    Kill(Duration),
}

/// How much longer than the watchdog a bank may be silent while an admin request that writes the
/// world's files (a save, a backup, a world's file) waits for it: a save on a slow disk is slow,
/// not hung. Past it a save that never ends is a hang too.
const SAVE_WAIT: Duration = Duration::from_secs(600);

/// An admin request that has the bank write the world's files.
pub(super) fn saves(path: &str) -> bool {
    ["/api/save", "/api/backup", "/api/worlds"].iter().any(|p| path.starts_with(p))
}

/// The watchdog's decision. The bank's clock runs only while the world plays (Idle): never during
/// a flip or a recovery (the front waits for those with deadlines of its own) and never when
/// stopped (there is no bank). A paused world is playing as far as this goes: its loop still
/// answers. `silent`: since the bank's last frame; `watchdog`: the rule (zero: off); `saving`: a
/// request that writes files waits; `pinged`: since the last ping.
pub(super) fn watch_verdict(phase: Phase, silent: Duration, watchdog: Duration, saving: bool, pinged: Option<Duration>) -> Watch {
    if phase != Phase::Idle || watchdog.is_zero() {
        return Watch::Rest;
    }
    let allowed = if saving { watchdog + SAVE_WAIT } else { watchdog };
    if silent > allowed {
        return Watch::Kill(silent);
    }
    // Asked a quarter of the way in, then every quarter, until it speaks.
    let every = (watchdog / 4).max(Duration::from_secs(1));
    if silent >= every && pinged.is_none_or(|p| p >= every) {
        return Watch::Ping;
    }
    Watch::Fine
}

impl Front {
    /// The live bank ended.
    pub(super) fn live_died(&mut self) {
        let Some(mut b) = self.live.take() else { return };
        let status = end(&mut b);
        let name = b.name.clone();
        eprintln!("bank {name} (pid {}) ended ({status})", b.child.id());
        match self.phase {
            // The snapshot is with the new bank already: it takes over as if nothing happened.
            Phase::Restoring => eprintln!("the new bank has its snapshot: it takes over"),
            Phase::Starting | Phase::Snapshotting => {
                self.fail_flip("the running bank ended");
            }
            _ => self.recover(&name),
        }
    }

    /// A bank died outside a flip: the world starts again in that bank, from the last snapshot
    /// when it is newer than the save, else the save. Its players were ahead of both: they are
    /// told to join again. The Steam login and the invite code stay.
    pub(super) fn recover(&mut self, bank: &str) {
        let now = Instant::now();
        if self.crashes.record(now) {
            self.give_up(bank);
        }
        let told = self.dismiss_all(&crate::protocol::reason_batch(RESTARTED), now);
        self.queue.clear();
        self.admin.forget_answers();
        // The snapshot stays held: should this bank die too before it saves, the next one
        // starts from it again.
        let save_time = crate::util::modified(std::path::Path::new(&self.save));
        self.recover_from = None;
        let from = match &self.last_snapshot {
            Some((fd, at)) if *at > save_time => match fd.try_clone() {
                Ok(fd) => {
                    self.recover_from = Some(fd);
                    format!("the snapshot of the last flip ({} s old, newer than the save)", at.elapsed().map(|d| d.as_secs()).unwrap_or(0))
                }
                Err(e) => format!("the save: the snapshot of the last flip, newer, could not be passed on ({e})"),
            },
            _ => "the save".to_string(),
        };
        match self.spawn(bank, None) {
            Ok(b) => {
                eprintln!("restarting bank {bank} (pid {}) from {from}; {} players were told to join again", b.child.id(), told);
                self.next = Some(b);
                self.enter(Phase::Recovering);
            }
            Err(e) => {
                eprintln!("cannot restart bank {bank}: {e}");
                self.shutdown(1);
            }
        }
    }

    /// The restarted bank's HELLO: the world goes to it, from the last snapshot or the save.
    /// A bank of another protocol cannot be given the world: the front must be started again,
    /// from that bank's program.
    pub(super) fn restart_hello(&mut self, hello: Result<Hello, String>) {
        if let Err(e) = &hello {
            let name = self.next.as_ref().map(|b| b.name.clone()).unwrap_or_default();
            eprintln!("bank {name} cannot run the world again: {e}; stopping");
            self.shutdown(1);
        }
        let start = match self.recover_from.take() {
            Some(fd) => (ipc::RESTORE, Some(fd)),
            None => (ipc::START, None),
        };
        let body = if start.0 == ipc::RESTORE { msg::body(&Restore { recover: true }) } else { Vec::new() };
        if let Some(b) = self.next.as_mut() {
            b.link.queue(start.0, 0, &body, start.1, 0);
        }
    }

    /// More crashes than MAX_CRASHES within CRASH_WINDOW: the front ends (the supervisor starts
    /// it again). When the bank that keeps ending came by a flip of this front, the release in
    /// it is given up first: it is noted as rolled back (the automatic update leaves it out) and
    /// `active` points at the other bank, so the supervisor's restart runs the release that ran
    /// before. Without that the restart would start the same bank, for ever.
    fn give_up(&mut self, bank: &str) -> ! {
        let seen = format!("the game ended {} times within {} minutes", self.crashes.len(), CRASH_WINDOW.as_secs() / 60);
        let rolled = match (&self.banks, self.flipped.as_deref() == Some(bank)) {
            (Some(dir), true) => updater::crash_rollback_target(dir, bank).and_then(|to| {
                let tag = updater::info(dir, bank).map_or("?".into(), |i| i.tag.as_deref().or(i.version.as_deref()).unwrap_or("?").to_string());
                let why = format!("{seen} after it was flipped to");
                updater::set_rolled_back_by_crashes(dir, bank, &why)?;
                updater::set_active(dir, to)?;
                Ok(format!("ROLLED BACK: the release in bank {bank} ({tag}) is given up, {why}: the server stops and starts again in bank {to}; the automatic update leaves {tag} out (pcds upgrade {tag} installs it)"))
            }),
            (Some(_), false) => Err(format!("bank {bank} was the active one when this server started")),
            (None, _) => Err("this server runs from a build, without banks".into()),
        };
        match rolled {
            Ok(line) => eprintln!("{line}"),
            Err(why) => eprintln!("{seen}: stopping (systemd starts the server again); no rollback: {why}"),
        }
        self.shutdown(1)
    }

    /// The watchdog: a live bank whose game loop has been silent too long is killed and the
    /// world starts again as after a crash (recover: it counts toward the crash limit). Its
    /// clock is the last frame of any kind from the bank; a quiet bank is pinged (GET /api/ping,
    /// an admin request every bank answers from its loop).
    fn watch_bank(&mut self) {
        let now = Instant::now();
        let saving = self.admin.waits_for(saves);
        let phase = self.phase;
        let Some(live) = self.live.as_mut() else { return };
        match watch_verdict(phase, now.saturating_duration_since(live.heard), self.watch.limit, saving, self.watch.pinged.map(|t| now.saturating_duration_since(t))) {
            Watch::Rest => {
                live.heard = now;
                self.watch.pinged = None;
            }
            Watch::Fine => {}
            Watch::Ping => {
                self.watch.pinged = Some(now);
                let id = self.admin.next_id();
                let ping = AdminCall { method: "GET".into(), path: "/api/ping".into(), body: serde_json::json!({}) };
                live.link.queue(ipc::ADMIN, id, &msg::body(&ping), None, 0);
            }
            Watch::Kill(silent) => {
                eprintln!("bank {} (pid {}) said nothing for {} s (the rule limits.bankWatchdogSeconds is {} s): it is hung; killed, and the world starts again from the last snapshot or save, as after a crash", live.name, live.child.id(), silent.as_secs(), self.watch.limit.as_secs());
                let mut b = self.live.take().expect("live bank");
                let _ = b.child.kill();
                // Killed, not waited for for ever: a process stuck in the disk dies when it returns.
                let dead = Instant::now() + Duration::from_secs(5);
                while Instant::now() < dead && matches!(b.child.try_wait(), Ok(None)) {
                    std::thread::sleep(Duration::from_millis(20));
                }
                let name = b.name.clone();
                self.exiting.push((b.child, Instant::now()));
                self.watch.pinged = None;
                self.recover(&name);
            }
        }
    }

    /// Timeouts and the old banks' ends.
    pub(super) fn supervise(&mut self) {
        self.watch_bank();
        if let Some(b) = &self.next {
            let late = match self.phase {
                Phase::Starting | Phase::Recovering => b.started.elapsed() > LOAD_DEADLINE,
                Phase::Snapshotting | Phase::Restoring => self.flip.as_ref().and_then(|f| f.stopped).is_some_and(|t| t.elapsed() > READY_DEADLINE),
                Phase::Idle | Phase::Stopped => false,
            };
            if late {
                match self.phase {
                    Phase::Recovering => {
                        let mut b = self.next.take().unwrap();
                        let _ = b.child.kill();
                        let _ = b.child.wait();
                        self.recover(&b.name);
                    }
                    _ => self.fail_flip(&format!("not ready within {:?}", if self.phase == Phase::Starting { LOAD_DEADLINE } else { READY_DEADLINE })),
                }
            }
        }
        // The old banks: EXIT given; killed if they linger.
        self.exiting.retain_mut(|(child, since)| match child.try_wait() {
            Ok(Some(_)) => false,
            _ if since.elapsed() > Duration::from_secs(10) => {
                let _ = child.kill();
                let _ = child.wait();
                false
            }
            _ => true,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The watchdog's clock runs only while the world plays; a flip, a recovery, a stop, the
    /// rule at 0 and a save the admin waits for each stop it (or lengthen it), a pause does not.
    #[test]
    fn the_watchdog_kills_only_a_bank_that_is_silent_while_the_world_plays() {
        let s = Duration::from_secs;
        let verdict = |phase, silent, saving| watch_verdict(phase, s(silent), s(60), saving, None);
        // Quiet banks are asked, silent ones are killed after the rule's time, not before.
        assert_eq!(verdict(Phase::Idle, 5, false), Watch::Fine);
        assert_eq!(verdict(Phase::Idle, 15, false), Watch::Ping);
        assert_eq!(verdict(Phase::Idle, 60, false), Watch::Ping);
        assert_eq!(verdict(Phase::Idle, 61, false), Watch::Kill(s(61)));
        // A SIGSTOP of 10 s is nothing; one of 120 s is a hang.
        assert!(matches!(verdict(Phase::Idle, 10, false), Watch::Fine));
        assert!(matches!(verdict(Phase::Idle, 120, false), Watch::Kill(_)));
        // Asked a quarter of the way in and every quarter after it, not at every loop.
        assert_eq!(watch_verdict(Phase::Idle, s(20), s(60), false, Some(s(2))), Watch::Fine);
        assert_eq!(watch_verdict(Phase::Idle, s(30), s(60), false, Some(s(15))), Watch::Ping);
        // Never during a flip, a recovery or a stop, however long.
        for phase in [Phase::Starting, Phase::Snapshotting, Phase::Restoring, Phase::Recovering, Phase::Stopped] {
            assert_eq!(verdict(phase, 10_000, false), Watch::Rest, "{phase:?}");
        }
        // The rule 0 is off.
        assert_eq!(watch_verdict(Phase::Idle, s(10_000), s(0), false, None), Watch::Rest);
        // A save the admin waits for gets 10 minutes more; past them it is a hang too.
        assert_eq!(verdict(Phase::Idle, 300, true), Watch::Ping);
        assert_eq!(verdict(Phase::Idle, 660, true), Watch::Ping);
        assert_eq!(verdict(Phase::Idle, 661, true), Watch::Kill(s(661)));
        assert!(saves("/api/save") && saves("/api/backup/s3") && saves("/api/worlds/restore") && !saves("/api/status") && !saves("/api/pause"));
        // The default is a minute.
        assert_eq!(crate::rules::Rules::default().limits.bank_watchdog_seconds, 60);
        assert_eq!(super::super::tests::test_front().watch.limit, s(60));
    }

    /// More than five endings within ten minutes are too many; older ones fall out of the window.
    #[test]
    fn the_crash_window_counts_the_endings_of_the_last_ten_minutes() {
        let mut window = CrashWindow::default();
        let start = Instant::now();
        for i in 0..MAX_CRASHES {
            assert!(!window.record(start + Duration::from_secs(i as u64)), "{i}");
        }
        assert!(window.record(start + Duration::from_secs(10)), "the sixth within the window");
        assert_eq!(window.len(), MAX_CRASHES + 1);
        // Ten minutes later only the new one counts.
        assert!(!window.record(start + CRASH_WINDOW + Duration::from_secs(60)));
        assert_eq!(window.len(), 1);
        window.clear();
        assert_eq!(window.len(), 0);
    }
}
