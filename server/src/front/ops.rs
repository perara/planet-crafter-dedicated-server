//! The rules the front acts on, as the live bank says them (OPS, at its start and whenever the
//! rules change): the autosave timer, the update schedule, the watchdog's limit and the log's format and level. The front
//! holds these because it outlives a bank; a rule it has not heard of yet has the default.

use std::time::{Duration, Instant};

use super::updates::check_every;
use super::Front;
use crate::msg::{self, Ops};

/// The autosave timer: the bank is told to save every `minutes` (0: never).
pub(super) struct Autosave {
    pub minutes: u64,
    next: Instant,
}

impl Autosave {
    pub fn new(minutes: u64) -> Autosave {
        Autosave { minutes, next: Instant::now() + Self::every(minutes) }
    }

    fn every(minutes: u64) -> Duration {
        Duration::from_secs(minutes.max(1) * 60)
    }

    /// The rule changed: another interval, from now.
    pub fn set(&mut self, minutes: Option<u64>) {
        if let Some(m) = minutes.filter(|m| *m != self.minutes) {
            self.minutes = m;
            self.next = Instant::now() + Self::every(m);
        }
    }

    /// The timer starts again (a world started).
    pub fn restart(&mut self) {
        self.next = Instant::now() + Self::every(self.minutes);
    }

    /// Whether the bank is to save now (and the next time is set).
    pub fn due(&mut self, now: Instant, playing: bool) -> bool {
        if self.minutes > 0 && now >= self.next && playing {
            self.next += Duration::from_secs(self.minutes * 60);
            return true;
        }
        false
    }
}

impl Front {
    /// OPS from the live bank.
    pub(super) fn ops(&mut self, body: &[u8]) {
        let o: Ops = msg::parse(body).unwrap_or_default();
        self.autosave.set(o.autosave_minutes);
        if let Some(auto) = o.auto_update {
            self.updates.auto = auto;
        }
        if let Some(hours) = o.update_check_hours {
            self.updates.every = check_every(hours);
        }
        if let Some(seconds) = o.bank_watchdog_seconds {
            self.watch.limit = Duration::from_secs(seconds);
        }
        // The front's own lines follow the rules log.format and log.level too.
        if let (Some(format), Some(level)) = (o.log_format.as_deref().and_then(crate::logging::Format::parse), o.log_level.as_deref().and_then(crate::logging::Level::parse)) {
            crate::logging::set(format, level);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::test_front;
    use super::*;
    use crate::ipc::{self, Frame};

    /// What the live bank says of the rules the front acts on (OPS) sets the autosave timer and
    /// the updater, and a value out of range is clamped, not trusted.
    #[test]
    fn the_rules_the_bank_sends_set_the_autosave_and_the_updater() {
        let mut front = test_front();
        assert_eq!((front.autosave.minutes, front.updates.auto, front.updates.every), (5, false, Duration::from_secs(24 * 3600)));
        let ops = |front: &mut Front, body: &str| front.on_live(Frame { kind: ipc::OPS, n: 0, body: body.as_bytes().to_vec(), fd: None });
        ops(&mut front, r#"{"autosaveMinutes": 0, "autoUpdate": true, "updateCheckHours": 6}"#);
        assert_eq!((front.autosave.minutes, front.updates.auto, front.updates.every), (0, true, Duration::from_secs(6 * 3600)));
        ops(&mut front, r#"{"autosaveMinutes": 10, "updateCheckHours": 100000}"#);
        assert_eq!((front.autosave.minutes, front.updates.auto, front.updates.every), (10, true, Duration::from_secs(8760 * 3600)));
        // Not JSON, or other fields: nothing changes.
        ops(&mut front, "no");
        ops(&mut front, r#"{"somethingElse": 1}"#);
        assert_eq!((front.autosave.minutes, front.updates.auto), (10, true));
        // The watchdog's rule comes the same way.
        ops(&mut front, r#"{"bankWatchdogSeconds": 15}"#);
        assert_eq!(front.watch.limit, Duration::from_secs(15));
        ops(&mut front, r#"{"bankWatchdogSeconds": 0}"#);
        assert!(front.watch.limit.is_zero());
    }

    /// The timer fires once an interval while a bank plays, never at 0, and the interval
    /// changes from the moment the rule does.
    #[test]
    fn the_autosave_is_due_once_an_interval_while_a_bank_plays() {
        let mut timer = Autosave::new(5);
        let now = Instant::now();
        assert!(!timer.due(now, true), "not at once");
        let later = now + Duration::from_secs(5 * 60 + 1);
        assert!(!timer.due(later, false), "nothing plays");
        assert!(timer.due(later, true) && !timer.due(later, true), "once");
        timer.set(Some(0));
        assert!(!timer.due(later + Duration::from_secs(3600), true), "0: never");
        timer.set(Some(1));
        assert!(!timer.due(Instant::now(), true) && timer.due(Instant::now() + Duration::from_secs(61), true));
    }
}
