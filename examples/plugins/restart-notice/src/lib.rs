//! restart-notice: warns the chat before the daily restart, and tells about saves and updates.
//!
//! The plugin cannot restart anything. The admin's own scheduler does (a systemd timer, cron) and
//! this plugin only says so beforehand: "Server restart in 15 minutes", then "The server restarts now".
//!
//! It shows two things a plugin otherwise lacks: a clock and a way to wait. The clock is the wall
//! clock (`WorldInfo::unix_ms`). The waiting is timers (`Host::set_timer`), which count game time
//! and are gone after a reload. So the plugin keeps no state at all: at `load` it works out from the
//! settings and the wall clock when each warning is due, and sets a timer for each. After a timer
//! has gone off it sets the next one, a day later, from the wall clock again.
//!
//! Game time and wall time can differ (a world that stands still when it is empty does not count
//! game time), so one more timer, every five minutes, sets the others again from the wall clock. A
//! warning that is more than 90 seconds late is dropped: it would tell a time that is no longer true.

use pcds_plugin_api::*;

const MINUTE_MS: i64 = 60_000;
const DAY_MS: i64 = 86_400_000;
/// Warnings at most (and so timers: one for each, one for the restart itself, one to set them again).
const MAX_WARNINGS: usize = 8;
const DEFAULT_WARNINGS: &str = "15,5,1";
/// The timer that sets all the others again from the wall clock, and how often it goes off.
const RESYNC_ID: u32 = 20;
const RESYNC_MS: u32 = 300_000;
/// A warning later than this after its time is not said.
const STALE_MS: i64 = 90_000;
/// The least game time between two "World saved." (or two failed-save lines) in the chat.
const SAVE_GAP_MS: i64 = 30_000;
/// The longest server version the chat line repeats.
const VERSION_CHARS: usize = 40;

/// A setting that does not read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Malformed;

/// The minute of the day (UTC) of "04:00" or "4:00": 0 to 1439. `Ok(None)` for an empty setting.
pub fn parse_restart_at(text: &str) -> Result<Option<u32>, Malformed> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    let (h, m) = text.split_once(':').ok_or(Malformed)?;
    let digits = |s: &str, len: core::ops::RangeInclusive<usize>| len.contains(&s.len()) && s.bytes().all(|b| b.is_ascii_digit());
    if !digits(h, 1..=2) || !digits(m, 2..=2) {
        return Err(Malformed);
    }
    let (h, m): (u32, u32) = (h.parse().map_err(|_| Malformed)?, m.parse().map_err(|_| Malformed)?);
    if h > 23 || m > 59 {
        return Err(Malformed);
    }
    Ok(Some(h * 60 + m))
}

/// The minutes before the restart to warn at, largest first, from "30,10,5,1". `none` and an empty
/// text mean no warnings. `Err` for anything that is not 1 to 8 whole numbers of 1 to 1439.
pub fn parse_warnings(text: &str) -> Result<Vec<u32>, Malformed> {
    let text = text.trim();
    if text.is_empty() || text.eq_ignore_ascii_case("none") {
        return Ok(Vec::new());
    }
    // The setting is at most 40 characters; a longer text is no setting of ours.
    if text.len() > 64 {
        return Err(Malformed);
    }
    let mut minutes = Vec::new();
    for part in text.split(',').take(MAX_WARNINGS + 1) {
        let part = part.trim();
        if part.is_empty() || part.len() > 4 || !part.bytes().all(|b| b.is_ascii_digit()) {
            return Err(Malformed);
        }
        let n: u32 = part.parse().map_err(|_| Malformed)?;
        if !(1..=1439).contains(&n) {
            return Err(Malformed);
        }
        minutes.push(n);
    }
    minutes.sort_unstable_by(|a, b| b.cmp(a));
    minutes.dedup();
    if minutes.len() > MAX_WARNINGS {
        return Err(Malformed);
    }
    Ok(minutes)
}

/// The first time after `since` (Unix milliseconds) at which the clock in UTC reads `minute_of_day`.
fn next_after(since: i64, minute_of_day: u32) -> i64 {
    let base = since.div_euclid(DAY_MS).saturating_mul(DAY_MS).saturating_add(minute_of_day as i64 * MINUTE_MS);
    if base > since { base } else { base.saturating_add(DAY_MS) }
}

/// "2 hours 5 minutes", "1 hour", "7 minutes": a time in whole minutes, rounded up, at least one.
fn how_long(ms: i64) -> String {
    let minutes = (ms.max(0) + MINUTE_MS - 1) / MINUTE_MS;
    let (h, m) = (minutes / 60, minutes % 60);
    let plural = |n: i64, unit: &str| format!("{n} {unit}{}", if n == 1 { "" } else { "s" });
    match (h, m) {
        (0, _) => plural(m.max(1), "minute"),
        (_, 0) => plural(h, "hour"),
        _ => format!("{} {}", plural(h, "hour"), plural(m, "minute")),
    }
}

/// One thing to say at a time of day: `minutes_before` the restart (0: the restart itself), and the
/// time it is waiting for (0: no timer is set).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Slot {
    minutes_before: u32,
    target: i64,
}

struct Notice {
    /// The restart's minute of the day, if there is a good setting.
    restart_min: Option<u32>,
    /// The settings did not parse: `!restart` says so.
    bad_setting: bool,
    /// The warnings, largest first, then the restart itself (`minutes_before` 0).
    slots: Vec<Slot>,
    announce_saves: bool,
    announce_updates: bool,
    /// Game time of the last "World saved." and of the last failed-save line.
    last_saved_say: Option<i64>,
    last_failed_say: Option<i64>,
    /// Whether the log already said that timers or the clock are out of reach.
    complained: bool,
}

impl Notice {
    fn minute_of(&self, slot: &Slot) -> Option<u32> {
        Some((self.restart_min? + 1440 - slot.minutes_before) % 1440)
    }

    /// The wall clock, or `None` (and one log line) if the host will not tell.
    fn clock(&mut self, host: &dyn Host) -> Option<i64> {
        match host.world() {
            Ok(w) if w.unix_ms > 0 => Some(w.unix_ms),
            other => {
                if !self.complained {
                    self.complained = true;
                    host.log(Level::Warn, &format!("no wall clock ({}): cannot schedule the notices", other.map_or_else(|e| format!("{e:?}"), |_| "unix_ms is 0".into())));
                }
                None
            }
        }
    }

    /// Sets the timer of slot `i` for the next time its minute of the day comes after `since`.
    fn arm(&mut self, host: &dyn Host, i: usize, now: i64, since: i64) {
        let Some(slot) = self.slots.get(i).copied() else { return };
        let Some(minute) = self.minute_of(&slot) else { return };
        let target = next_after(since, minute);
        let delay = (target - now).clamp(1, 604_800_000) as u32;
        let set = host.set_timer(i as u32 + 1, delay, 0);
        if let Some(slot) = self.slots.get_mut(i) {
            slot.target = if set.is_ok() { target } else { 0 };
        }
        if let Err(e) = set
            && !self.complained
        {
            self.complained = true;
            host.log(Level::Warn, &format!("cannot set a timer: {e:?}"));
        }
    }

    /// Sets a timer for each slot, and the one that sets them again (in `load`).
    fn arm_all(&mut self, host: &dyn Host) {
        if self.restart_min.is_none() {
            return;
        }
        let Some(now) = self.clock(host) else { return };
        for i in 0..self.slots.len() {
            self.arm(host, i, now, now);
        }
        let _ = host.set_timer(RESYNC_ID, RESYNC_MS, RESYNC_MS);
    }

    /// The timers from the wall clock again. A slot whose time has already come is handled now, as
    /// its own timer would (it says its line if that is still true, and waits for tomorrow): its
    /// timer counts game time and may be a long way off.
    fn resync(&mut self, host: &dyn Host) {
        let Some(now) = self.clock(host) else { return };
        for i in 0..self.slots.len() {
            match self.slots.get(i) {
                Some(s) if s.target != 0 && s.target <= now => self.fire(host, i),
                Some(_) => self.arm(host, i, now, now),
                None => {}
            }
        }
    }

    /// The timer of slot `i` went off.
    fn fire(&mut self, host: &dyn Host, i: usize) {
        let Some(slot) = self.slots.get(i).copied() else { return };
        if slot.target == 0 {
            return;
        }
        let Some(now) = self.clock(host) else { return };
        if slot.target - now > STALE_MS {
            // Early by more than a moment: the wall clock was set back. Wait for the real time.
            self.arm(host, i, now, now);
            return;
        }
        if now - slot.target <= STALE_MS {
            let text = match slot.minutes_before {
                0 => "The server restarts now".to_string(),
                1 => "Server restart in 1 minute".to_string(),
                n => format!("Server restart in {n} minutes"),
            };
            let _ = host.say(&text);
        } else {
            host.log(Level::Debug, &format!("dropped a notice {} s late", (now - slot.target) / 1000));
        }
        self.arm(host, i, now, now.max(slot.target));
    }

    /// Whether a line may be said at game time `now`, given when the last of its kind was.
    fn gap_ok(last: &mut Option<i64>, now: i64) -> bool {
        let ok = last.is_none_or(|l| now - l >= SAVE_GAP_MS || now < l);
        if ok {
            *last = Some(now);
        }
        ok
    }
}

impl Plugin for Notice {
    fn load(host: &dyn Host, _state: Option<State<'_>>) -> Self {
        let mut me = Notice { restart_min: None, bad_setting: false, slots: Vec::new(), announce_saves: false, announce_updates: true, last_saved_say: None, last_failed_say: None, complained: false };
        me.announce_saves = host.config_bool("announce_saves").unwrap_or(false);
        me.announce_updates = host.config_bool("announce_updates").unwrap_or(true);
        let restart_at = host.config("restart_at").unwrap_or_default();
        match parse_restart_at(&restart_at) {
            Ok(minute) => me.restart_min = minute,
            Err(Malformed) => {
                me.bad_setting = true;
                host.log(Level::Warn, "restart_at is not a time as HH:MM (UTC), as 04:00: no restart is announced");
            }
        }
        let warn_text = host.config("warn_minutes").unwrap_or_else(|| DEFAULT_WARNINGS.to_string());
        let warnings = parse_warnings(&warn_text).unwrap_or_else(|Malformed| {
            me.bad_setting = true;
            host.log(Level::Warn, "warn_minutes is not up to 8 numbers of 1 to 1439 separated by commas, as 30,10,5,1: using 15,5,1");
            parse_warnings(DEFAULT_WARNINGS).unwrap_or_default()
        });
        me.slots = warnings.into_iter().chain(Some(0)).map(|minutes_before| Slot { minutes_before, target: 0 }).collect();
        me.arm_all(host);
        me
    }

    fn event(&mut self, host: &dyn Host, event: &Event<'_>) -> Verdict {
        match event {
            Event::Timer { id: RESYNC_ID, .. } => self.resync(host),
            Event::Timer { id, .. } if *id >= 1 => self.fire(host, *id as usize - 1),
            Event::Command { client, command: "restart", .. } => {
                let text = self.next_restart_text(host);
                let _ = host.tell(*client, &text);
            }
            Event::WorldSaved { game_ms, ok: true } if self.announce_saves && Notice::gap_ok(&mut self.last_saved_say, *game_ms) => {
                let _ = host.say("World saved.");
            }
            Event::WorldSaved { game_ms, ok: false } if Notice::gap_ok(&mut self.last_failed_say, *game_ms) => {
                let _ = host.say("The world could not be saved, tell an admin.");
            }
            Event::Flip { server_version, .. } if self.announce_updates => {
                let version: String = server_version.chars().filter(|c| !c.is_control()).take(VERSION_CHARS).collect();
                let _ = host.say(&format!("The server was updated to version {version} without a restart."));
            }
            _ => {}
        }
        Verdict::Continue
    }
}

impl Notice {
    /// The answer to `!restart`.
    fn next_restart_text(&mut self, host: &dyn Host) -> String {
        let Some(minute) = self.restart_min else {
            return if self.bad_setting { "The restart time is not set up right, tell an admin.".into() } else { "No restart is scheduled.".into() };
        };
        let Some(now) = self.clock(host) else { return "I cannot read the clock, ask an admin.".into() };
        let at = next_after(now, minute);
        format!("Next restart in {} (at {:02}:{:02} UTC).", how_long(at - now), minute / 60, minute % 60)
    }
}

export!(Notice);

#[cfg(test)]
mod tests {
    use super::*;
    use pcds_plugin_api::testing::{Harness, MockHost};

    /// A day of the calendar (day 20,000 since 1970) at hours and minutes UTC.
    fn at(h: i64, m: i64) -> i64 {
        20_000 * DAY_MS + (h * 60 + m) * MINUTE_MS
    }

    /// A mock that has the wall clock at `now` and the settings given.
    fn mock(now: i64, settings: &[(&str, &str)]) -> MockHost {
        let mut mock = MockHost::new().with_world(WorldInfo { unix_ms: now, ..Default::default() });
        for (k, v) in settings {
            mock = mock.with_setting(k, v);
        }
        mock
    }

    fn start_on(mock: MockHost) -> Harness<Notice> {
        // The manifest's events, no ticks (the plugin does not ask for them) and a step of a second:
        // a timer is late by less than that.
        let mut t = Harness::<Notice>::start(mock).subscribe(&[Kind::WorldSaved, Kind::Flip]).tick_every_ms(0).tick_step_ms(1000);
        t.declare_command("restart", 0, 0, false);
        t
    }

    fn start(now: i64, settings: &[(&str, &str)]) -> Harness<Notice> {
        start_on(mock(now, settings))
    }

    const MIN: u64 = 60_000;

    #[test]
    fn the_warnings_come_at_their_minutes_and_the_last_one_at_the_restart() {
        let mut t = start(at(3, 30), &[("restart_at", "04:00")]);
        t.advance_ms(14 * MIN);
        assert!(t.mock().said().is_empty());
        t.advance_ms(MIN);
        assert_eq!(t.mock().said(), ["Server restart in 15 minutes"]);
        t.advance_ms(10 * MIN);
        t.advance_ms(4 * MIN);
        t.advance_ms(MIN);
        assert_eq!(t.mock().said(), ["Server restart in 15 minutes", "Server restart in 5 minutes", "Server restart in 1 minute", "The server restarts now"]);
        // Tomorrow's are set, and nothing more is said in between.
        t.advance_ms(60 * MIN);
        assert_eq!(t.mock().said().len(), 4);
        t.advance_ms(22 * 60 * MIN + 45 * MIN);
        assert_eq!(t.mock().said().len(), 5);
        assert_eq!(t.mock().said()[4], "Server restart in 15 minutes");
    }

    #[test]
    fn the_warnings_are_the_admins_list_and_the_default_is_15_5_1() {
        let mut t = start(at(3, 0), &[("restart_at", "4:00"), ("warn_minutes", "30, 10,5")]);
        t.advance_ms(61 * MIN);
        assert_eq!(t.mock().said(), ["Server restart in 30 minutes", "Server restart in 10 minutes", "Server restart in 5 minutes", "The server restarts now"]);
        // The mock has no defaults: the plugin knows them too.
        let t = start(at(3, 0), &[("restart_at", "04:00")]);
        let ids: Vec<u32> = t.mock().timers().iter().map(|x| x.id).collect();
        assert_eq!(ids, [1, 2, 3, 4, RESYNC_ID]);
    }

    #[test]
    fn a_reload_in_the_middle_does_not_move_or_repeat_a_warning() {
        let mut t = start(at(3, 40), &[("restart_at", "04:00")]);
        t.advance_ms(5 * MIN + 30_000); // 03:45:30, after the 15-minute warning
        assert_eq!(t.mock().said(), ["Server restart in 15 minutes"]);
        t.reload();
        // The timers are set again from the clock: the 5-minute warning is due in 9 and a half minutes.
        let due: Vec<(u32, i64)> = t.mock().timers().iter().filter(|x| x.id != RESYNC_ID).map(|x| (x.id, x.due_ms - t.mock().game_ms())).collect();
        assert_eq!(due[0], (1, DAY_MS - 30_000));
        assert_eq!(due[1], (2, 9 * MINUTE_MS + 30_000));
        t.advance_ms(9 * MIN + 30_000);
        assert_eq!(t.mock().said(), ["Server restart in 15 minutes", "Server restart in 5 minutes"]);
    }

    #[test]
    fn warnings_that_are_late_because_the_world_stood_still_are_dropped() {
        let mut t = start(at(3, 40), &[("restart_at", "04:00")]);
        // The wall clock goes on while game time stands still: by the time game time has gone five
        // minutes it is 04:03 on the wall, and every warning of today is over.
        t.set_unix_ms(at(3, 58));
        t.advance_ms(5 * MIN + 1000); // timers set in `load` start counting at the first call, one step in
        assert!(t.mock().said().is_empty(), "{:?}", t.mock().said());
        // All four are set for tomorrow, from the wall clock.
        let next_day = t.mock().timers().iter().filter(|x| x.id <= 4).all(|x| x.due_ms - t.mock().game_ms() > 23 * 60 * MINUTE_MS);
        assert!(next_day, "{:?}", t.mock().timers());
        t.advance_ms(23 * 60 * MIN + 43 * MIN);
        assert_eq!(t.mock().said(), ["Server restart in 15 minutes"]);
    }

    #[test]
    fn a_warning_only_a_moment_late_is_still_said_by_the_resync() {
        let mut t = start(at(3, 40), &[("restart_at", "04:00"), ("warn_minutes", "30,5")]);
        // The wall is 04:00:30 when the resync looks (game time stood still for 15 minutes): the
        // restart line is 30 seconds late and still true, the warnings are not.
        t.set_unix_ms(at(3, 55) + 30_000);
        t.advance_ms(5 * MIN + 1000); // timers set in `load` start counting at the first call, one step in
        assert_eq!(t.mock().said(), ["The server restarts now"]);
    }

    #[test]
    fn a_wall_clock_set_back_waits_for_the_real_time() {
        let mut t = start(at(3, 30), &[("restart_at", "04:00"), ("warn_minutes", "15")]);
        // An hour back: the 15-minute warning goes off in game time, but it is only 02:45 by the wall.
        t.set_unix_ms(at(2, 30));
        t.advance_ms(15 * MIN);
        assert!(t.mock().said().is_empty());
        t.advance_ms(60 * MIN);
        assert_eq!(t.mock().said().len(), 1, "{:?}", t.mock().said());
    }

    #[test]
    fn restart_tells_the_time_left_in_hours_and_minutes() {
        let mut t = start(at(1, 55), &[("restart_at", "04:00")]);
        t.join(1, "Ann");
        t.command(1, "restart", &[]);
        t.command(1, "restart", &["garbage", "more"]);
        assert_eq!(t.mock().told()[0].1, "Next restart in 2 hours 5 minutes (at 04:00 UTC).");
        // The host answers more arguments than the manifest allows with the usage, and does not call the plugin.
        assert_eq!(t.mock().told()[1].1, "Use: !restart");
        t.set_unix_ms(at(3, 0));
        t.command(1, "restart", &[]);
        assert_eq!(t.mock().told()[2].1, "Next restart in 1 hour (at 04:00 UTC).");
        t.set_unix_ms(at(3, 59) + 30_000);
        t.command(1, "restart", &[]);
        assert_eq!(t.mock().told()[3].1, "Next restart in 1 minute (at 04:00 UTC).");
        t.set_unix_ms(at(4, 0));
        t.command(1, "restart", &[]);
        assert_eq!(t.mock().told()[4].1, "Next restart in 24 hours (at 04:00 UTC).");
        t.set_unix_ms(at(4, 1));
        t.command(1, "restart", &[]);
        assert_eq!(t.mock().told()[5].1, "Next restart in 23 hours 59 minutes (at 04:00 UTC).");
    }

    #[test]
    fn without_a_restart_time_nothing_is_scheduled_and_restart_says_so() {
        let mut t = start(at(3, 0), &[]);
        t.join(1, "Ann");
        assert!(t.mock().timers().is_empty());
        t.command(1, "restart", &[]);
        assert_eq!(t.mock().told()[0].1, "No restart is scheduled.");
        t.advance_ms(30 * MIN);
        assert!(t.mock().said().is_empty());
    }

    #[test]
    fn a_malformed_setting_is_logged_and_nothing_is_announced() {
        for bad in ["4pm", "24:00", "04:60", "0400", "4:5", "::", "04:00:00", "٠٤:٠٠", "-1:00", "04:00 UTC"] {
            let mut t = start(at(3, 0), &[("restart_at", bad)]);
            t.join(1, "Ann");
            assert!(t.mock().timers().is_empty(), "{bad}");
            assert!(t.mock().logs().iter().any(|(l, m)| *l == Level::Warn && m.contains("restart_at")), "{bad}");
            t.command(1, "restart", &[]);
            assert_eq!(t.mock().told()[0].1, "The restart time is not set up right, tell an admin.");
        }
        // A bad list of warnings falls back to the default and is told.
        for bad in ["15,,1", "0", "1440", "-5", "five", "1,2,3,4,5,6,7,8,9", "15 5"] {
            let t = start(at(3, 0), &[("restart_at", "04:00"), ("warn_minutes", bad)]);
            assert!(t.mock().logs().iter().any(|(_, m)| m.contains("warn_minutes")), "{bad}");
            assert_eq!(t.mock().timers().len(), 5, "{bad}: the default 15,5,1, the restart and the resync");
        }
        let t = start(at(3, 0), &[("restart_at", "04:00"), ("warn_minutes", "none")]);
        assert_eq!(t.mock().timers().len(), 2);
        assert_eq!(parse_warnings("5,30,5,10"), Ok(vec![30, 10, 5]));
        assert_eq!(parse_restart_at(" 04:00 "), Ok(Some(240)));
        assert_eq!(parse_restart_at("23:59"), Ok(Some(1439)));
        assert_eq!(parse_restart_at(""), Ok(None));
        assert!(parse_restart_at(&"1".repeat(10_000)).is_err());
        assert!(parse_warnings(&"1,".repeat(10_000)).is_err());
    }

    #[test]
    fn a_restart_just_after_midnight_warns_the_evening_before() {
        let mut t = start(at(23, 40), &[("restart_at", "00:05"), ("warn_minutes", "15,5")]);
        t.advance_ms(26 * MIN);
        assert_eq!(t.mock().said(), ["Server restart in 15 minutes", "Server restart in 5 minutes", "The server restarts now"]);
    }

    #[test]
    fn saves_are_told_only_when_asked_and_a_failed_one_always() {
        let mut t = start(at(3, 0), &[]);
        t.world_saved(true);
        assert!(t.mock().said().is_empty());
        t.world_saved(false);
        assert_eq!(t.mock().said(), ["The world could not be saved, tell an admin."]);
        let mut t = start(at(3, 0), &[("announce_saves", "true")]);
        t.world_saved(true);
        assert_eq!(t.mock().said(), ["World saved."]);
    }

    #[test]
    fn a_flood_of_saves_is_one_line_in_half_a_minute_under_the_action_limit() {
        let mock = mock(at(3, 0), &[("announce_saves", "true")]).with_limits();
        let mut t = start_on(mock);
        for _ in 0..200 {
            t.world_saved(true);
            t.world_saved(false);
        }
        assert_eq!(t.mock().said().len(), 2);
        t.advance_ms(31_000);
        t.world_saved(true);
        t.world_saved(false);
        assert_eq!(t.mock().said().len(), 4);
        assert!(t.mock().denied().is_empty());
    }

    #[test]
    fn a_hot_update_is_told_unless_the_admin_turned_it_off() {
        let mut t = start(at(3, 0), &[]);
        t.flip("0.9.1");
        assert_eq!(t.mock().said(), ["The server was updated to version 0.9.1 without a restart."]);
        let mut t = start(at(3, 0), &[("announce_updates", "false")]);
        t.flip("0.9.1");
        assert!(t.mock().said().is_empty());
        // A hostile version string: cut, and with no control characters.
        let mut t = start(at(3, 0), &[]);
        t.flip(&format!("1.0\n\u{7}{}", "x".repeat(500)));
        let line = &t.mock().said()[0];
        assert!(line.chars().count() < 100 && !line.contains('\n') && !line.contains('\u{7}'), "{line}");
    }

    #[test]
    fn a_hot_update_sets_the_timers_again_in_the_new_instance() {
        let mut t = start(at(3, 40), &[("restart_at", "04:00")]);
        t.advance_ms(MIN);
        t.reload();
        t.flip("0.9.1");
        t.advance_ms(19 * MIN);
        assert_eq!(t.mock().said().last().map(String::as_str), Some("The server restarts now"));
        assert_eq!(t.mock().said().len(), 5, "{:?}", t.mock().said());
    }

    #[test]
    fn denied_capabilities_are_logged_once_and_never_trap() {
        // No timers: the warnings cannot be scheduled.
        let mock = mock(at(3, 0), &[("restart_at", "04:00")]).deny(&["event.timer"]);
        let mut t = start_on(mock);
        assert!(t.mock().timers().is_empty());
        assert_eq!(t.mock().logs().iter().filter(|(_, m)| m.contains("cannot set a timer")).count(), 1);
        t.advance_ms(2 * 60 * MIN);
        assert!(t.mock().said().is_empty());
        // No clock: nothing scheduled, and !restart says so.
        let mock = mock_denied_world();
        let mut t = start_on(mock);
        t.join(1, "Ann");
        t.command(1, "restart", &[]);
        assert_eq!(t.mock().told()[0].1, "I cannot read the clock, ask an admin.");
        assert!(t.mock().timers().is_empty());
        // No chat: events run, say nothing, and do not fault.
        let mock = mock_denied_chat();
        let mut t = start_on(mock);
        t.advance_ms(60 * MIN);
        t.world_saved(false);
        t.flip("1");
        t.join(1, "Ann");
        t.command(1, "restart", &[]);
        assert!(t.mock().said().is_empty() && t.mock().told().is_empty());
        assert!(!t.mock().denied().is_empty());
    }

    fn mock_denied_world() -> MockHost {
        mock(at(3, 0), &[("restart_at", "04:00")]).deny(&["world.read"])
    }

    fn mock_denied_chat() -> MockHost {
        mock(at(3, 0), &[("restart_at", "04:00")]).deny(&["chat.say"])
    }

    #[test]
    fn foreign_and_stale_timers_are_ignored() {
        let mut t = start(at(3, 0), &[("restart_at", "04:00")]);
        for id in [0, 7, 8, 9, 19, 21, u32::MAX] {
            t.event(&Event::Timer { id, game_ms: 0, late_ms: 0 });
        }
        assert!(t.mock().said().is_empty());
        // The restart timer cancelled by someone else (a timer that went off but has no target) does nothing twice.
        t.advance_ms(61 * MIN);
        assert_eq!(t.mock().said().len(), 4);
        t.event(&Event::Timer { id: 4, game_ms: 0, late_ms: 0 });
        assert_eq!(t.mock().said().len(), 4);
    }

    #[test]
    fn the_notice_saves_no_state_and_a_reload_keeps_working() {
        let mut t = start(at(3, 0), &[("restart_at", "04:00")]);
        assert!(t.save().is_none());
        t.reload();
        t.reload();
        t.advance_ms(61 * MIN);
        assert_eq!(t.mock().said().len(), 4);
        // State from some other version of the plugin is not read.
        let t = Harness::<Notice>::start_with_state(mock(at(3, 0), &[("restart_at", "04:00")]), 9, &[1, 2, 3]);
        assert_eq!(t.mock().timers().len(), 5);
    }

    #[test]
    fn load_sets_the_timers_itself_and_only_when_there_is_a_restart_time() {
        let t = start(at(3, 0), &[("restart_at", "04:00")]);
        assert_eq!(t.mock().timers().iter().map(|x| x.id).collect::<Vec<_>>(), [1, 2, 3, 4, RESYNC_ID]);
        // Each timer is set from the wall clock: the 15-minute warning in 45 minutes.
        assert_eq!(t.mock().timers()[0].delay_ms, 45 * 60_000);
        let t = start(at(3, 0), &[]);
        assert!(t.mock().timers().is_empty());
    }

    #[test]
    fn a_timer_fired_by_hand_checks_the_wall_clock_before_it_speaks() {
        let mut t = start(at(3, 0), &[("restart_at", "04:00")]);
        // Too early by 45 minutes: nothing is said and the timer is set again for the real time.
        t.fire_timer(1);
        assert!(t.mock().said().is_empty());
        assert_eq!(t.mock().timers().iter().find(|x| x.id == 1).map(|x| x.delay_ms), Some(45 * 60_000));
        t.set_unix_ms(at(3, 45));
        t.fire_timer(1);
        assert_eq!(t.mock().said(), ["Server restart in 15 minutes"]);
    }
}
