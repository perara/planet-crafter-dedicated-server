//! What the front holds the world in, as a state machine: a flip to another bank (an update, a
//! rollback), a restart after a crash, a stop by the admin. `Phase` is the state; every change of
//! it goes through `Front::enter`, which the tests hold to the table below, and every bank frame
//! or timeout that changes it is handled in this file or in supervise.rs and worlds.rs.
//!
//! ```text
//!                 flip_to                 HELLO checked            SNAPSHOT of this flip
//!     Idle ─────────────────► Starting ────────────────► Snapshotting ───────────────────► Restoring
//!      ▲ ▲ │                      │                          │                                │
//!      │ │ │                      └──── fail_flip ───────────┴─────────── fail_flip ──────────┤
//!      │ │ │ recover (a bank ended, hung)                                                      │
//!      │ │ └──────────────────────► Recovering ◄─┐ (the new bank ended while starting)         │
//!      │ │                              │  └─────┘                                              │
//!      │ └──────── READY (promote) ◄────┴───────────────────────────────────────────────────────┘
//!      │
//!      └── start_bank ◄── Stopped ◄── stop_world (from Idle)
//! ```
//!
//! Starting: the next bank loads its tables while the live one plays. Snapshotting: the live
//! bank stops and snapshots, client frames wait. Restoring: the next bank has the snapshot.
//! READY makes it the live one; a failure before it resumes the live one (RESUME) and the
//! flip's outcome is recorded as failed. The world stands from HANDOFF to READY only (tens of
//! milliseconds). Recovering is the same hand-over without an old bank: the live one died.

use std::os::fd::OwnedFd;
use std::time::{Duration, Instant, SystemTime};

use super::bank_link::{hello_of, Bank};
use super::history::{FlipNote, History, Timeline};
use super::{ms, Front};
use crate::ipc::{self, Frame};
use crate::msg::{self, Hello, Ready, Restore, SnapshotNote};
use crate::updater;

#[derive(PartialEq, Debug, Clone, Copy)]
pub(super) enum Phase {
    Idle,
    /// The next bank loads its tables; the live one runs on.
    Starting,
    /// HANDOFF sent: the live bank stops and snapshots; client frames wait.
    Snapshotting,
    /// The next bank has the snapshot.
    Restoring,
    /// The live bank died: the next one starts from the last snapshot or the save.
    Recovering,
    /// The admin stopped the world (POST /api/world/stop): no bank; joins are refused; the
    /// admin page and API stay, to start it or another world again.
    Stopped,
}

impl Phase {
    /// The name the status gives it.
    pub fn label(self) -> &'static str {
        match self {
            Phase::Idle => "idle",
            Phase::Starting => "starting",
            Phase::Snapshotting => "snapshotting",
            Phase::Restoring => "restoring",
            Phase::Recovering => "recovering",
            Phase::Stopped => "stopped",
        }
    }

    /// Whether the front may go from this phase to `to`: the arrows of the picture above.
    pub fn may_go(self, to: Phase) -> bool {
        use Phase::*;
        matches!(
            (self, to),
            (Idle, Starting | Recovering | Stopped | Idle)
                | (Starting, Snapshotting | Idle)
                | (Snapshotting, Restoring | Idle)
                | (Restoring, Idle)
                | (Recovering, Recovering | Idle)
                | (Stopped, Idle)
        )
    }
}

/// The flip under way.
pub(super) struct Flip {
    pub seq: u64,
    pub reason: String,
    /// HANDOFF sent: the world stands from here.
    pub stopped: Option<Instant>,
    pub snapshot: SnapshotNote,
    /// The flip's events on the monotonic clock (ipc::now_ns, ns): HANDOFF sent, SNAPSHOT
    /// handled, RESTORE flushed to the new bank.
    pub handoff_at: u64,
    pub snapshot_at: u64,
    pub relay_at: u64,
    /// Its snapshot: the last one (last_snapshot) once the new bank restored it and said READY.
    pub kept: Option<(OwnedFd, SystemTime)>,
}

impl Flip {
    pub fn new(seq: u64, reason: &str) -> Flip {
        Flip { seq, reason: reason.to_string(), stopped: None, snapshot: SnapshotNote::default(), handoff_at: 0, snapshot_at: 0, relay_at: 0, kept: None }
    }
}

/// The milliseconds between two moments of the monotonic clock (0: one of them was not noted).
fn gap(from: u64, to: u64) -> f64 {
    if from == 0 || to == 0 { 0.0 } else { (to.saturating_sub(from) as f64 / 1e4).round() / 100.0 }
}

/// What the front knows of a flip that its banks do not tell.
struct Handover {
    from: String,
    to: String,
    /// Client frames held while the world stood.
    held: usize,
    /// How long it stood.
    stood: Duration,
    /// The new bank's version.
    version: Option<String>,
    /// READY handled, and the held frames released, on the monotonic clock.
    ready_at: u64,
    released_at: u64,
}

/// What a finished flip says of itself: the line for the log and the history, and the numbers.
/// The timeline is where the stand went, on the monotonic clock all three processes read: the
/// HANDOFF to the old bank, its snapshot, the SNAPSHOT to the front, the front's relay of the
/// memfd, the RESTORE to the new bank, its restore, the READY to the front, the held frames out.
fn outcome(flip: &Flip, ready: &Ready, h: Handover) -> (String, FlipNote) {
    let verified = match ready.verified {
        Some(true) => format!("; the restored state is the snapshot's, checked in {:.1} ms (in the stand)", ready.verify_ms.unwrap_or(0.0)),
        _ => String::new(),
    };
    let message = format!(
        "flip {} -> {} ({}): the world stood {:.1} ms (snapshot {} bytes written in {:.1} ms, restored in {:.1} ms; {} client frames held){verified}",
        h.from,
        h.to,
        flip.reason,
        ms(h.stood),
        msg::json(&flip.snapshot.bytes),
        flip.snapshot.ms.unwrap_or(0.0),
        ready.restore_ms.unwrap_or(0.0),
        h.held
    );
    let (old, new) = (&flip.snapshot, ready);
    let timeline = Timeline {
        handoff_ms: gap(flip.handoff_at, old.got_at),
        snapshot_ms: gap(old.got_at, old.done_at),
        snapshot_to_front_ms: gap(old.done_at, flip.snapshot_at),
        relay_ms: gap(flip.snapshot_at, flip.relay_at),
        restore_to_bank_ms: gap(flip.relay_at, new.got_at),
        restore_ms: gap(new.got_at, new.ready_at),
        ready_to_front_ms: gap(new.ready_at, h.ready_at),
        release_ms: gap(h.ready_at, h.released_at),
    };
    let note = FlipNote {
        from: h.from,
        to: h.to,
        stood_ms: ms(h.stood),
        snapshot_bytes: flip.snapshot.bytes,
        snapshot_ms: flip.snapshot.ms,
        restore_ms: ready.restore_ms,
        held: h.held,
        verified: ready.verified,
        version: h.version,
        timeline,
        restore_faults: ready.faults,
    };
    (message, note)
}

impl Front {
    /// The one place the phase changes.
    pub(super) fn enter(&mut self, to: Phase) {
        if cfg!(test) {
            assert!(self.phase.may_go(to), "the front went from {:?} to {to:?}", self.phase);
        }
        self.phase = to;
    }

    /// Starts a flip to `bank`.
    pub(super) fn flip_to(&mut self, bank: &str, reason: &str, seq: u64, fail: Option<&str>) -> Result<(), String> {
        if self.phase != Phase::Idle || self.live.is_none() {
            return Err("a flip is under way".into());
        }
        self.flip_fits()?;
        let b = self.spawn(bank, fail)?;
        println!("flip to bank {bank} ({reason}): pid {} loading", b.child.id());
        self.next = Some(b);
        self.enter(Phase::Starting);
        self.flip = Some(Flip::new(seq, reason));
        self.updates.busy = Some((seq, format!("flipping to bank {bank}")));
        Ok(())
    }

    /// Err when a flip would not fit in the container's memory (metrics::flip_memory).
    pub(super) fn flip_fits(&self) -> Result<(), String> {
        let live = self.live.as_ref().map_or(0, |b| super::metrics::process_of(b.child.id()).1);
        super::metrics::flip_memory(live, super::metrics::cgroup_memory())
    }

    /// A frame of the next bank: its HELLO starts a flip's hand-over (or a restart), its READY
    /// ends it.
    pub(super) fn on_next(&mut self, f: Frame) {
        match f.kind {
            ipc::HELLO => {
                let hello = hello_of(&f);
                if let (Some(b), Ok(hello)) = (self.next.as_mut(), &hello) {
                    b.hello = Some(hello.clone());
                }
                match self.phase {
                    Phase::Recovering => self.restart_hello(hello),
                    Phase::Starting => self.flip_hello(f.n, hello),
                    _ => {}
                }
            }
            ipc::READY if matches!(self.phase, Phase::Restoring | Phase::Recovering) => self.promote(&f.body),
            _ => {}
        }
    }

    /// The next bank's HELLO in a flip: checked against the live bank (protocol, tables, snapshot
    /// formats), and then the world stands: HANDOFF to the live bank.
    fn flip_hello(&mut self, protocol: u64, hello: Result<Hello, String>) {
        let hello = match hello {
            Ok(hello) => hello,
            Err(e) => return self.fail_flip(&e),
        };
        let live = self.live.as_ref().and_then(|b| b.hello.as_ref());
        let mut info = hello.info.clone();
        info.protocol = Some(protocol);
        if let Err(e) = updater::compatible(&info, live.map(|h| &h.info)) {
            return self.fail_flip(&e);
        }
        let live_game = live.and_then(|h| h.game_version.clone());
        if hello.game_version != live_game {
            return self.fail_flip(&format!("it has game {}'s tables, the world runs on {}'s", msg::json(&hello.game_version), msg::json(&live_game)));
        }
        if let (Some(n), Some(l)) = (self.next.as_ref(), self.live.as_ref()) {
            println!(
                "flip: bank {} has loaded its tables after {:.0} ms and holds {:.0} MiB; the live bank holds {:.0} MiB",
                n.name,
                n.started.elapsed().as_secs_f64() * 1000.0,
                super::metrics::process_of(n.child.id()).1 as f64 / 1048576.0,
                super::metrics::process_of(l.child.id()).1 as f64 / 1048576.0
            );
        }
        // Quiesce: from here the world stands.
        if let Some(flip) = self.flip.as_mut() {
            flip.stopped = Some(Instant::now());
        }
        let seq = self.flip.as_ref().map_or(0, |flip| flip.seq);
        if let Some(b) = self.live.as_mut() {
            b.link.queue(ipc::HANDOFF, seq, &[], None, 0);
            let _ = b.link.flush();
            if let Some(flip) = self.flip.as_mut() {
                flip.handoff_at = ipc::now_ns();
            }
        }
        self.enter(Phase::Snapshotting);
        println!("flip: bank {} is checked; the running bank is told to hand the world over", self.next.as_ref().map_or("?", |b| b.name.as_str()));
    }

    /// The live bank's SNAPSHOT: the world, for the next bank to restore.
    pub(super) fn on_snapshot(&mut self, f: Frame) {
        // A SNAPSHOT answers the HANDOFF of one flip: one of an earlier flip (it came after
        // that flip failed and the bank was told to go on) is not this flip's world.
        if !self.flip.as_ref().is_some_and(|flip| flip.seq == f.n) || self.phase != Phase::Snapshotting {
            eprintln!("bank: a snapshot for flip {} dropped (the flip under way: {:?}, {:?})", f.n, self.flip.as_ref().map(|flip| flip.seq), self.phase);
            return;
        }
        match f.fd {
            Some(fd) => {
                if let Some(flip) = self.flip.as_mut() {
                    flip.snapshot_at = ipc::now_ns();
                    flip.snapshot = msg::parse(&f.body).unwrap_or_default();
                    flip.kept = fd.try_clone().ok().map(|d| (d, SystemTime::now()));
                }
                if let Some(n) = self.next.as_mut() {
                    n.link.queue(ipc::RESTORE, 0, &msg::body(&Restore { recover: false }), Some(fd), 0);
                }
                self.enter(Phase::Restoring);
            }
            // It could not write one and went on.
            None => self.fail_flip("the running bank could not write a snapshot"),
        }
    }

    /// The next bank runs: it becomes the live one, the old one ends.
    pub(super) fn promote(&mut self, ready: &[u8]) {
        let Some(new) = self.next.take() else { return };
        let ready: Ready = msg::parse(ready).unwrap_or_default();
        let old = self.live.take();
        let ready_at = ipc::now_ns();
        let recovered = self.phase == Phase::Recovering;
        self.enter(Phase::Idle);
        let (from, to) = (old.as_ref().map(|b| b.name.clone()).unwrap_or_else(|| "-".into()), new.name.clone());
        if let Some(mut b) = old {
            b.link.queue(ipc::EXIT, 0, &[], None, 0);
            let _ = b.link.flush();
            self.exiting.push((b.child, Instant::now()));
        }
        // Its loading was the flip's wait, not silence: the watchdog counts from here.
        self.live = Some(Bank { heard: Instant::now(), ..new });
        let held = self.release_held();
        // The held frames go now: what follows waits for no disk.
        self.flush_banks();
        let (released, released_at) = (Instant::now(), ipc::now_ns());
        let rolled_back = self.flip.as_ref().is_some_and(|f| f.reason == "rollback").then(|| from.clone());
        self.admin.set_page(self.web_page(&to));
        if recovered {
            self.persist_flip(&to, None, None);
            println!("bank {to} (pid {}) runs the world again", self.live.as_ref().map_or(0, |b| b.child.id()));
            return;
        }
        let Some(mut flip) = self.flip.take() else { return self.persist_flip(&to, None, None) };
        // Restored, so a crash restarts from it (one that failed to restore must not be).
        if let Some(kept) = flip.kept.take() {
            self.last_snapshot = Some(kept);
        }
        let stood = flip.stopped.map(|t| released.duration_since(t)).unwrap_or_default();
        self.flips += 1;
        self.totals.last_stand_ms = Some(ms(stood));
        // The crashes counted so far were another release's: the new one starts with none.
        self.crashes.clear();
        self.flipped = Some(to.clone());
        let version = self.live.as_ref().and_then(|b| b.version().map(str::to_string));
        let (message, note) = outcome(&flip, &ready, Handover { from, to: to.clone(), held, stood, version, ready_at, released_at });
        println!("{message}");
        println!("flip timeline (ms): {}", msg::json(&note.timeline));
        self.persist_flip(&to, rolled_back, Some((flip.seq, message, note)));
    }

    /// `active` points at `bank` from now on, and a rollback notes the bank it left (so that the
    /// automatic update does not bring back what the owner left): both flush to disk, so a
    /// thread writes them.
    /// `record`: the flip's outcome (seq, message, fields), recorded once the files are written.
    fn persist_flip(&mut self, bank: &str, rolled_back_from: Option<String>, record: Option<(u64, String, FlipNote)>) {
        let Some(dir) = self.banks.clone() else {
            if let Some((seq, message, note)) = record {
                self.record(History::flipped(seq, message, note));
            }
            return;
        };
        let (bank, tx) = (bank.to_string(), self.updates.tx.clone());
        self.persisting.retain(|t| !t.is_finished());
        let write = move || {
            if let Err(e) = updater::set_active(&dir, &bank) {
                eprintln!("banks: {e}");
            }
            if let Some(from) = rolled_back_from
                && let Err(e) = updater::set_rolled_back(&dir, &from)
            {
                eprintln!("banks: the rollback from bank {from} could not be noted ({e}): the automatic update may install its release again");
            }
            if let Some((seq, message, note)) = record {
                let _ = tx.send(super::updates::Done::Flipped { seq, message, note });
            }
        };
        match std::thread::Builder::new().name("banks-folder".into()).spawn(write) {
            Ok(t) => self.persisting.push(t),
            Err(e) => eprintln!("banks: no thread to write the banks folder ({e})"),
        }
    }

    /// A flip that did not work: the new bank is killed, the old one goes on with the frames
    /// held for the new one.
    pub(super) fn fail_flip(&mut self, why: &str) {
        if let Some(mut b) = self.next.take() {
            let _ = b.child.kill();
            self.exiting.push((b.child, Instant::now()));
        }
        let phase = self.phase;
        self.enter(Phase::Idle);
        let flip = self.flip.take();
        self.totals.flips_failed += u64::from(flip.is_some());
        let stood = flip.as_ref().and_then(|f| f.stopped).map(|t| t.elapsed());
        let message = match (&mut self.live, phase) {
            (Some(b), Phase::Snapshotting | Phase::Restoring) => {
                b.link.queue(ipc::RESUME, 0, &[], None, 0);
                self.release_held();
                format!("flip failed ({why}); the running bank went on, the world stood {:.1} ms", ms(stood.unwrap_or_default()))
            }
            (Some(_), _) => format!("flip refused ({why}); the running bank never stopped"),
            (None, _) => format!("flip failed ({why})"),
        };
        eprintln!("{message}");
        if let Some(f) = flip {
            self.record(History::new(f.seq, false, message.clone()));
        }
        if self.live.is_none() {
            match self.banks.as_deref().map(updater::active).transpose() {
                Ok(name) => self.recover(&name.unwrap_or_else(|| "a".into())),
                Err(e) => eprintln!("banks: {e}: the world stays stopped"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{fake_bank, test_front};
    use super::*;

    /// The arrows of the picture, and no others.
    #[test]
    fn the_phases_go_only_where_the_picture_says() {
        use Phase::*;
        let all = [Idle, Starting, Snapshotting, Restoring, Recovering, Stopped];
        let arrows = [(Idle, Starting), (Idle, Recovering), (Idle, Stopped), (Starting, Snapshotting), (Starting, Idle), (Snapshotting, Restoring), (Snapshotting, Idle), (Restoring, Idle), (Recovering, Recovering), (Recovering, Idle), (Stopped, Idle), (Idle, Idle)];
        for from in all {
            for to in all {
                assert_eq!(from.may_go(to), arrows.contains(&(from, to)), "{from:?} -> {to:?}");
            }
        }
        // Only a flip that has not stopped the world, or one that has, can fail; a stopped world only starts.
        assert!(!Stopped.may_go(Starting) && !Restoring.may_go(Snapshotting));
    }

    /// Flip 1 timed out and the old bank was told to go on; its late snapshot comes while
    /// flip 2 waits for its own: dropped, and flip 2 takes only the snapshot of its HANDOFF.
    #[test]
    fn a_flip_takes_only_the_snapshot_of_its_own_handoff() {
        let mut front = test_front();
        let (mut live, mut old) = fake_bank("a");
        live.hello = Some(serde_json::from_value(serde_json::json!({"format": 1})).unwrap());
        let (next, mut new) = fake_bank("b");
        front.live = Some(live);
        front.next = Some(next);
        front.phase = Phase::Starting;
        front.flip = Some(Flip::new(2, "test"));
        front.on_next(Frame { kind: ipc::HELLO, n: ipc::PROTOCOL, body: br#"{"format":1}"#.to_vec(), fd: None });
        assert_eq!(front.phase, Phase::Snapshotting);
        let handoff = old.recv().unwrap();
        assert_eq!((handoff.kind, handoff.n), (ipc::HANDOFF, 2), "HANDOFF carries the flip");
        old.send_fd(ipc::SNAPSHOT, 1, b"{}", Some(super::super::tests::snapshot_fd("flip 1")));
        old.send(ipc::SNAPSHOT, 1, &[]);
        front.read_banks();
        assert_eq!(front.phase, Phase::Snapshotting, "flip 1's snapshots are not flip 2's");
        assert!(front.last_snapshot.is_none() && front.flip.is_some());
        old.send_fd(ipc::SNAPSHOT, 2, b"{}", Some(super::super::tests::snapshot_fd("flip 2")));
        front.read_banks();
        assert_eq!(front.phase, Phase::Restoring);
        front.flush_banks();
        let restore = new.recv().unwrap();
        assert_eq!((restore.kind, ipc::read_memfd(restore.fd.as_ref().unwrap()).unwrap()), (ipc::RESTORE, b"flip 2".to_vec()));
        // A crash restarts from it only once the new bank restored it.
        assert!(front.last_snapshot.is_none());
        front.on_next(Frame { kind: ipc::READY, n: 0, body: b"{}".to_vec(), fd: None });
        assert_eq!(front.last_snapshot.as_ref().map(|(fd, _)| ipc::read_memfd(fd).unwrap()), Some(b"flip 2".to_vec()));
        let exiting = front.exiting.drain(..).map(|(c, _)| c);
        for mut child in [front.live.take(), front.next.take()].into_iter().flatten().map(|b| b.child).chain(exiting) {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    /// The log line and the history's numbers of a flip, from what the banks and the front measured.
    #[test]
    fn a_flip_says_what_it_cost() {
        let mut flip = Flip::new(4, "update to v1");
        flip.snapshot = SnapshotNote { bytes: Some(2048), ms: Some(1.5), got_at: 2_000_000, done_at: 3_000_000 };
        (flip.handoff_at, flip.snapshot_at, flip.relay_at) = (1_000_000, 3_500_000, 4_000_000);
        let ready = Ready { restore_ms: Some(2.25), verify_ms: Some(0.5), verified: Some(true), got_at: 5_000_000, ready_at: 6_000_000, faults: Some(9) };
        let (message, note) = outcome(&flip, &ready, Handover { from: "a".into(), to: "b".into(), held: 3, stood: Duration::from_millis(12), version: Some("0.7.0".into()), ready_at: 6_500_000, released_at: 7_000_000 });
        assert_eq!(message, "flip a -> b (update to v1): the world stood 12.0 ms (snapshot 2048 bytes written in 1.5 ms, restored in 2.2 ms; 3 client frames held); the restored state is the snapshot's, checked in 0.5 ms (in the stand)");
        assert_eq!((note.stood_ms, note.held, note.snapshot_bytes, note.restore_faults, note.version.as_deref()), (12.0, 3, Some(2048), Some(9), Some("0.7.0")));
        // 1 ms is 1_000_000 ns: the gaps between the marks, in ms.
        assert_eq!(note.timeline, Timeline { handoff_ms: 1.0, snapshot_ms: 1.0, snapshot_to_front_ms: 0.5, relay_ms: 0.5, restore_to_bank_ms: 1.0, restore_ms: 1.0, ready_to_front_ms: 0.5, release_ms: 0.5 });
        // A bank that did not say: nulls, and no gaps.
        let (message, note) = outcome(&Flip::new(5, "rollback"), &Ready::default(), Handover { from: "b".into(), to: "a".into(), held: 0, stood: Duration::ZERO, version: None, ready_at: 1, released_at: 2 });
        assert!(message.contains("snapshot null bytes written in 0.0 ms, restored in 0.0 ms") && !message.contains("checked in"), "{message}");
        assert_eq!((note.snapshot_bytes, note.verified, note.timeline), (None, None, Timeline::default()));
    }

    /// A bank that loaded for a minute and a half is not "silent for a minute and a half" the
    /// moment it takes the world: the watchdog counts from when it became the live one.
    #[test]
    fn a_bank_that_loaded_for_long_is_not_silent_when_it_takes_over() {
        use super::super::supervise::{watch_verdict, Watch};
        use std::time::Duration;
        let mut front = test_front();
        let (mut next, _peer) = fake_bank("b");
        next.heard = Instant::now().checked_sub(Duration::from_secs(300)).unwrap();
        front.next = Some(next);
        front.phase = Phase::Recovering;
        front.promote(b"{}");
        assert_eq!(front.phase, Phase::Idle);
        let live = front.live.as_ref().expect("it is the live bank");
        assert!(live.heard.elapsed() < Duration::from_secs(2), "{:?}", live.heard.elapsed());
        let silent = live.heard.elapsed();
        assert!(matches!(watch_verdict(front.phase, silent, front.watch.limit, false, None), Watch::Fine), "not killed at once");
        for mut child in [front.live.take()].into_iter().flatten().map(|b| b.child).chain(front.exiting.drain(..).map(|(c, _)| c)) {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}
