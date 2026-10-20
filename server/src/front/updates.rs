//! Updates: looking up the latest release, downloading it into the standby bank on a thread of
//! its own, and flipping to it when it is in (flip.rs), by request (POST /api/update) or by
//! schedule (the rule update.auto). `Updates` is the one place that holds the history, and
//! `Front::record` the one that adds to it.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::history::{FlipNote, History};
use super::admin_routes::error;
use super::{Front, Phase};
use crate::msg::BankInfo;
use crate::updater;

/// What an update or a rollback asked for while the world is stopped is told.
pub(super) const STOPPED_UPDATE: &str = "the world is stopped, so no bank runs a world to hand over: start the world first (the dashboard's Start button, POST /api/world/start) and update then, or run pcds upgrade VERSION --restart, which installs the release and restarts the server";

/// An automatic check that failed (GitHub unreachable, rate limited) is made again after this
/// (or the check interval, when that is shorter), not after the whole interval; one that
/// found the world busy (a flip, a crash restart, a stopped world) after BUSY_RETRY.
const CHECK_RETRY: Duration = Duration::from_secs(3600);
const BUSY_RETRY: Duration = Duration::from_secs(600);

/// The time between the automatic checks for hours (the rule update.checkHours); the hook
/// PCDS_DEV=check-seconds=N makes them seconds (the end-to-end tests).
pub(super) fn check_every(hours: impl Into<u64>) -> Duration {
    let seconds = crate::config::dev("check-seconds").and_then(|s| s.parse::<u64>().ok());
    seconds.map_or_else(|| Duration::from_secs(hours.into().clamp(1, 8760) * 3600), Duration::from_secs)
}

/// How an update's work on a thread ended.
pub(super) enum Done {
    /// A flip's banks-folder writes are on disk: its outcome is recorded now, so that whoever
    /// sees it done also finds `active` and the rollback note written.
    Flipped { seq: u64, message: String, note: FlipNote },
    Installed { seq: u64, bank: String, info: BankInfo },
    Failed { seq: u64, message: String },
    UpToDate { seq: u64, tag: String },
    /// The automatic check left the latest release out (the one rolled back from).
    Skipped { seq: u64, message: String },
    /// A check only (POST /api/update {"check": true}): the latest release, and whether it is
    /// newer than the live bank's.
    Checked { seq: u64, tag: String, newer: bool },
}

pub(super) struct Updates {
    pub tx: Sender<Done>,
    rx: Receiver<Done>,
    /// The request being worked on (download or flip).
    pub busy: Option<(u64, String)>,
    pub seq: u64,
    pub history: VecDeque<History>,
    pub auto: bool,
    pub every: Duration,
    pub next_check: Instant,
    /// The request the automatic check made (its failure is tried again sooner).
    pub auto_seq: Option<u64>,
    latest: Option<String>,
    /// The latest release is later than the live bank's (as the last check found).
    newer: bool,
    checked: Option<u64>,
}

impl Updates {
    /// The rules' values (update) until the bank says what the rules file holds (OPS).
    pub fn new() -> Updates {
        let (tx, rx) = std::sync::mpsc::channel();
        let rules = crate::rules::Rules::default();
        let (auto, every) = (rules.update.auto, check_every(rules.update.check_hours));
        // The first check a while after the start: a crash loop must not ask GitHub each time.
        Updates { tx, rx, busy: None, seq: 0, history: VecDeque::new(), auto, every, next_check: Instant::now() + every.min(Duration::from_secs(600)), auto_seq: None, latest: None, newer: false, checked: None }
    }
}

impl Front {
    pub(super) fn update_status(&self) -> Value {
        let u = &self.updates;
        json!({
            "state": if u.busy.is_some() { "busy" } else { "idle" },
            "doing": u.busy.as_ref().map(|(_, what)| what.clone()),
            "seq": u.seq,
            "last": u.history.back(),
            "history": u.history,
            "auto": u.auto,
            "checkHours": u.every.as_secs() / 3600,
            "latest": u.latest,
            "available": u.newer,
            "checkedAt": u.checked,
            "releases": self.banks.as_ref().map(|_| updater::shown_url(&updater::releases_url())),
        })
    }

    pub(super) fn record(&mut self, entry: History) {
        let seq = entry.seq;
        self.updates.history.push_back(entry);
        while self.updates.history.len() > 20 {
            self.updates.history.pop_front();
        }
        if self.updates.busy.as_ref().is_some_and(|(s, _)| *s == seq) {
            self.updates.busy = None;
        }
    }

    /// POST /api/update: {"version": "v0.6.0"} (null: the latest; "onlyNewer": true installs
    /// it only if it is later than the live bank's), {"check": true}: only look up the latest,
    /// or {"rollback": true}: back to the other bank. The answer comes at once with the
    /// request's "seq"; GET /api/update has its outcome in "history" when done.
    pub(super) fn update_request(&mut self, body: &Value) -> (u16, String) {
        // No bank runs: there is no world to hand over. Said as it is (the dashboard says the same).
        if self.phase == Phase::Stopped {
            return (409, json!({"error": STOPPED_UPDATE, "stopped": true}).to_string());
        }
        if self.updates.busy.is_some() || self.phase != Phase::Idle {
            return (409, json!({"error": "an update or a flip is under way", "doing": self.updates.busy.as_ref().map(|b| b.1.clone())}).to_string());
        }
        let Some(live) = &self.live else { return error(503, "the game is starting") };
        let target = updater::other(&live.name).to_string();
        // The e2e tests' fault injection; off unless the front was started with PCDS_DEV=test-faults.
        let fail = body["failForTest"].as_str().filter(|_| crate::config::dev("test-faults").is_some()).map(str::to_string);
        self.updates.seq += 1;
        let seq = self.updates.seq;
        if body["rollback"] == true {
            if let Some(dir) = &self.banks {
                let Some(info) = updater::info(dir, &target) else { return error(400, &format!("bank {target} holds no release to go back to")) };
                if let Err(e) = updater::compatible_for(&info, live.info(), true) {
                    return error(400, &e);
                }
            }
            return match self.flip_to(&target, "rollback", seq, fail.as_deref()) {
                Ok(()) => (200, json!({"ok": true, "seq": seq, "bank": target}).to_string()),
                Err(e) => error(500, &e),
            };
        }
        let Some(dir) = self.banks.clone() else {
            return error(400, "no banks: this server runs from a build, not from pcds run (PCDS_HOME/bin); rollback flips between two copies of it");
        };
        if body["check"] == true {
            self.check(seq);
            return (200, json!({"ok": true, "seq": seq}).to_string());
        }
        // Before the download: a release that cannot be flipped to is not fetched for nothing.
        if let Err(e) = self.flip_fits() {
            return error(409, &e);
        }
        let version = body["version"].as_str().map(str::to_string);
        self.download(seq, dir, target, version, body["onlyNewer"] == true, false);
        (200, json!({"ok": true, "seq": seq}).to_string())
    }

    /// A download into the standby bank, on a thread of its own (it takes seconds); the loop
    /// flips when it is in. `only_newer`: install only a later release than the live bank's.
    /// `auto`: the automatic check, which also leaves out the release rolled back from; an
    /// update asked for clears that note.
    pub(super) fn download(&mut self, seq: u64, dir: PathBuf, bank: String, version: Option<String>, only_newer: bool, auto: bool) {
        let live = self.live.as_ref().and_then(|b| b.info().cloned()).unwrap_or_default();
        let held = self.held_release(&dir);
        let tx = self.updates.tx.clone();
        self.updates.busy = Some((seq, format!("downloading {} into bank {bank}", version.as_deref().unwrap_or("the latest release"))));
        answer_on_thread(tx, seq, move || {
            let release = updater::lookup(&updater::releases_url(), version.as_deref())?;
            if only_newer && !updater::newer(&release.tag, &held) {
                return Ok(Done::UpToDate { seq, tag: release.tag });
            }
            if auto && let Some(why) = updater::auto_skips(&dir, &release.tag)? {
                return Ok(Done::Skipped { seq, message: why });
            }
            let info = updater::install(&release, &dir, &bank, Some(&live))?;
            if !auto {
                updater::clear_rolled_back(&dir)?;
            }
            Ok(Done::Installed { seq, bank, info })
        });
    }

    /// The release the live bank holds (its tag, else its version: updater::held_release).
    pub(super) fn held_release(&self, dir: &std::path::Path) -> String {
        self.live.as_ref().map(|b| updater::held_release(dir, &b.name, b.version().unwrap_or(""))).unwrap_or_default()
    }

    /// The latest release looked up on a thread of its own; nothing is installed.
    pub(super) fn check(&mut self, seq: u64) {
        let live = self.banks.as_deref().map(|dir| self.held_release(dir)).unwrap_or_default();
        let tx = self.updates.tx.clone();
        self.updates.busy = Some((seq, "checking for a new release".into()));
        answer_on_thread(tx, seq, move || updater::lookup(&updater::releases_url(), None).map(|release| Done::Checked { seq, newer: updater::newer(&release.tag, &live), tag: release.tag }));
    }

    pub(super) fn updates(&mut self) {
        while let Ok(done) = self.updates.rx.try_recv() {
            if let Done::Flipped { seq, message, note } = done {
                self.record(History::flipped(seq, message, note));
                continue;
            }
            self.updates.checked = Some(crate::util::now_secs());
            match done {
                Done::Flipped { .. } => {}
                Done::Installed { seq, bank, info } => {
                    let tag = info.tag.as_deref().unwrap_or("?").to_string();
                    self.updates.latest = Some(tag.clone());
                    self.updates.newer = false;
                    println!("update: {tag} installed in bank {bank} (sha256 {} checked)", info.sha256.as_deref().unwrap_or(""));
                    if let Err(e) = self.flip_to(&bank, &format!("update to {tag}"), seq, None) {
                        self.record(History::new(seq, false, format!("{tag} is in bank {bank}, but the flip did not start: {e}")));
                    }
                }
                Done::Failed { seq, message } => {
                    eprintln!("update: {message}");
                    if self.updates.auto_seq == Some(seq) {
                        self.updates.next_check = Instant::now() + CHECK_RETRY.min(self.updates.every);
                    }
                    self.record(History::new(seq, false, message));
                }
                Done::UpToDate { seq, tag } => {
                    self.updates.latest = Some(tag.clone());
                    self.updates.newer = false;
                    self.record(History::new(seq, true, format!("up to date ({tag} is the latest release)")));
                }
                Done::Skipped { seq, message } => {
                    println!("update: {message}");
                    self.record(History::new(seq, true, message));
                }
                Done::Checked { seq, tag, newer } => {
                    self.updates.latest = Some(tag.clone());
                    self.updates.newer = newer;
                    let message = if newer { format!("{tag} is available") } else { format!("up to date ({tag} is the latest release)") };
                    self.record(History::checked(seq, message));
                }
            }
        }
        if self.updates.auto && Instant::now() >= self.updates.next_check {
            self.updates.next_check = Instant::now() + self.updates.every;
            if let (Some(dir), Some(live), None, Phase::Idle) = (self.banks.clone(), &self.live, &self.updates.busy, self.phase) {
                let bank = updater::other(&live.name).to_string();
                self.updates.seq += 1;
                let seq = self.updates.seq;
                println!("update: checking for a new release (the rule update.auto is on)");
                self.updates.auto_seq = Some(seq);
                self.download(seq, dir, bank, None, true, true);
            } else {
                self.updates.next_check = Instant::now() + BUSY_RETRY.min(self.updates.every);
                println!("update: the automatic check waits ({}): next in {} min", if self.live.is_none() || self.phase != Phase::Idle { format!("the world is not running ({:?})", self.phase) } else { "an update is under way".into() }, BUSY_RETRY.min(self.updates.every).as_secs() / 60);
            }
        }
    }
}

/// An update's work on a thread of its own, whose outcome always comes: a panic in it is a
/// Failed one, so the update is never busy for ever.
fn answer_on_thread(tx: Sender<Done>, seq: u64, work: impl FnOnce() -> Result<Done, String> + Send + 'static) {
    std::thread::spawn(move || {
        let done = std::panic::catch_unwind(std::panic::AssertUnwindSafe(work)).unwrap_or_else(|_| Err("the update's thread panicked (see above)".into()));
        let _ = tx.send(done.unwrap_or_else(|message| Done::Failed { seq, message }));
    });
}

#[cfg(test)]
mod tests {
    use super::super::tests::test_front;
    use super::*;

    /// The automatic check that failed (GitHub offline, a rate limit) is made again within the
    /// hour, not after the day of the check interval; one that finds the world busy within 10
    /// minutes; a check that works waits its interval.
    #[test]
    fn a_failed_or_skipped_automatic_check_is_made_again_sooner() {
        let mut front = test_front();
        front.updates.auto = true;
        front.updates.every = Duration::from_secs(24 * 3600);
        // A check that cannot run (no live bank: the world is stopped or recovering).
        front.updates.next_check = Instant::now();
        front.updates();
        let wait = front.updates.next_check.saturating_duration_since(Instant::now());
        assert!(wait > Duration::from_secs(590) && wait <= BUSY_RETRY, "{wait:?}");
        // A check that failed.
        front.updates.auto_seq = Some(7);
        front.updates.tx.send(Done::Failed { seq: 7, message: "GitHub: HTTP 403".into() }).unwrap();
        front.updates.next_check = Instant::now() + front.updates.every;
        front.updates();
        let wait = front.updates.next_check.saturating_duration_since(Instant::now());
        assert!(wait > Duration::from_secs(3590) && wait <= CHECK_RETRY, "{wait:?}");
        // A manual update's failure leaves the schedule alone.
        front.updates.next_check = Instant::now() + front.updates.every;
        front.updates.tx.send(Done::Failed { seq: 8, message: "sha256".into() }).unwrap();
        front.updates();
        assert!(front.updates.next_check > Instant::now() + Duration::from_secs(23 * 3600));
        // Never longer than the interval itself (update.checkHours=1 stays at an hour).
        front.updates.every = Duration::from_secs(1800);
        front.updates.auto_seq = Some(9);
        front.updates.tx.send(Done::Failed { seq: 9, message: "x".into() }).unwrap();
        front.updates();
        assert!(front.updates.next_check <= Instant::now() + Duration::from_secs(1800));
    }

    /// A stopped world says so to an update request, not that "an update or a flip is under way".
    #[test]
    fn an_update_of_a_stopped_world_says_it_is_stopped() {
        let mut front = test_front();
        front.phase = Phase::Stopped;
        let (status, body) = front.update_request(&json!({"check": true}));
        assert!(status == 409 && body.contains("the world is stopped"), "{status} {body}");
    }

    /// A panic in an update's thread is that update's failure, not a busy update for ever.
    #[test]
    fn a_panicking_update_thread_fails_its_update() {
        let (tx, rx) = std::sync::mpsc::channel();
        answer_on_thread(tx, 3, || panic!("test panic"));
        assert!(matches!(rx.recv_timeout(Duration::from_secs(10)), Ok(Done::Failed { seq: 3, message }) if message.contains("panicked")));
    }
}
