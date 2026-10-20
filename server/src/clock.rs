//! The game's clock. The game keeps time in a float (Unity's Time.time, 32 bits) and every timer
//! in it is `Time.time + interval` in that float. Its resolution is 1 ms at the first hour,
//! 0.25 s at 24 days and 1 s from 97 days: a game that runs for months, as this server does,
//! would run every timer late, and a 0.1 s step added to a due time of 2^21 s adds nothing at all.
//!
//! [`GameTime`] is one point on that clock, kept in an f64, with the sums the game does on it:
//!
//! * below [`EXACT_AFTER`] (an hour) the sum is the game's own f32 sum, rounded as the game
//!   rounds it, widened to the f64 without a change (every f32 is an f64): the first hour is
//!   bit for bit what the game does, which parity with the game, the bench fingerprints and the
//!   handoff proof depend on;
//! * from there on the sum is an f64 sum, so a timer never gets coarser than a microsecond in
//!   the next 100 years, whatever the uptime.
//!
//! [`GameTime::f32`] is the one place a time goes back into a float, for what a client reads or
//! the game computes as a float (an RPC argument, a difference of two times).
//!
//! The rule is decided by the value the sum starts from, so `now + interval` and
//! `due + step` need no clock to be passed around. A timer that repeats at a fixed pace (units,
//! gauges) uses [`GameTime::take_due`], which also bounds the catch-up after a jump of the
//! clock. An accumulator that is not a time (the world units' totals) uses [`add_young`] with
//! the clock, with the same two phases.

use std::ops::{Add, AddAssign, Sub};

/// Game seconds up to which sums are the game's f32 sums (an hour: 2^-12 s resolution).
pub const EXACT_AFTER: f64 = 3600.0;

/// A point on the game's clock, in seconds since the game started (the time the world held still
/// not counted: `Game::time`). Also a timer's due time; [`GameTime::NEVER`] is one that does not come.
#[derive(Clone, Copy, Default, PartialEq, PartialOrd)]
pub struct GameTime(f64);

impl GameTime {
    pub const ZERO: GameTime = GameTime(0.0);
    /// A due time that does not come.
    pub const NEVER: GameTime = GameTime(f64::INFINITY);
    /// A due time that has always passed.
    pub const PAST: GameTime = GameTime(f64::NEG_INFINITY);

    /// The clock at `seconds`, as the game reads it: the f32 Time.time while the clock is young,
    /// the exact value after. The only place that decides this; monotonic, as f32 rounding is.
    pub fn at(seconds: f64) -> GameTime {
        GameTime(if seconds < EXACT_AFTER { f64::from(seconds as f32) } else { seconds })
    }

    /// The seconds, exact.
    pub fn secs(self) -> f64 {
        self.0
    }

    /// Time.time as the game's float: what goes to a client, or into a float computation that the
    /// game does in a float. Exact below [`EXACT_AFTER`].
    pub fn f32(self) -> f32 {
        self.0 as f32
    }

    pub fn min(self, other: GameTime) -> GameTime {
        if other.0 < self.0 { other } else { self }
    }

    pub fn max(self, other: GameTime) -> GameTime {
        if other.0 > self.0 { other } else { self }
    }

    /// One more `step` of a timer that repeats at a fixed pace: the f32 sum while young (the
    /// game's `next += step`), the f64 sum after.
    pub fn advance(&mut self, step: f64) {
        self.0 = if self.0 < EXACT_AFTER { f64::from(self.0 as f32 + step as f32) } else { self.0 + step };
    }

    /// The steps of a timer that repeats every `step` seconds that have come by `now` (`now >=
    /// due` each): how many, and the timer is left at its next due time, after `now`. The game
    /// loops `while now >= next { next += step; ... }`; this is the same count and the same due
    /// time in a few operations once the clock is old, however far it jumped (a loop would take
    /// as many rounds as there are steps in the jump: days of game time, minutes of CPU). While
    /// the timer is young it loops (at most an hour of steps).
    pub fn take_due(&mut self, now: GameTime, step: f64) -> u64 {
        let mut steps = 0;
        while self.0 <= now.0 && self.0 < EXACT_AFTER {
            self.advance(step);
            steps += 1;
        }
        if self.0 <= now.0 {
            let from = self.0;
            let mut k = ((now.0 - from) / step).floor() as u64 + 1;
            self.0 = from + k as f64 * step;
            // The division rounds: settle on the first step after `now`.
            while self.0 <= now.0 {
                self.0 += step;
                k += 1;
            }
            while k > 1 && self.0 - step > now.0 {
                self.0 -= step;
                k -= 1;
            }
            steps += k;
        }
        steps
    }
}

impl std::fmt::Debug for GameTime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}s", self.0)
    }
}

impl std::fmt::Display for GameTime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// `time + interval`: the game's float sum while young, an f64 sum after.
impl Add<f32> for GameTime {
    type Output = GameTime;
    fn add(self, interval: f32) -> GameTime {
        GameTime(if self.0 < EXACT_AFTER { f64::from(self.0 as f32 + interval) } else { self.0 + f64::from(interval) })
    }
}

impl AddAssign<f32> for GameTime {
    fn add_assign(&mut self, interval: f32) {
        *self = *self + interval;
    }
}

/// `time - interval`.
impl Sub<f32> for GameTime {
    type Output = GameTime;
    fn sub(self, interval: f32) -> GameTime {
        GameTime(if self.0 < EXACT_AFTER { f64::from(self.0 as f32 - interval) } else { self.0 - f64::from(interval) })
    }
}

/// The seconds between two times, as the float the game computes (`Time.time - start`).
impl Sub for GameTime {
    type Output = f32;
    fn sub(self, earlier: GameTime) -> f32 {
        (self.0 - earlier.0) as f32
    }
}

/// As its bits: a time may be infinite (JSON has no infinity: serde_json writes null), and a
/// zero (most world objects' lock time) is one byte in the binary codec.
impl serde::Serialize for GameTime {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u64(self.0.to_bits())
    }
}

impl<'de> serde::Deserialize<'de> for GameTime {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<GameTime, D::Error> {
        Ok(GameTime(f64::from_bits(serde::Deserialize::deserialize(d)?)))
    }
}

/// `total + delta` for a total that is not a time (a world unit's), at game time `now`: the
/// game's float sum (the total is an f32 there, and every step rounds) while the clock is young,
/// an f64 sum after, so that a total of millions keeps growing by its steps of a few units.
/// `delta` is the step as the game computes it, in f32.
///
/// One exception while young: a float that has outgrown its steps (at 40 million its own step is
/// 4, and a step of 1.75 rounds back to where it was) never moves in the game, whatever the
/// buildings make. A server restarted in such a world would stand still for the first hour of
/// every run; there the sum is the f64 one at once. Everything the game's float does move by is
/// still its sum, bit for bit.
pub fn add_young(total: f64, delta: f32, now: GameTime) -> f64 {
    if now.0 < EXACT_AFTER {
        let sum = f64::from(total as f32 + delta);
        if delta == 0.0 || sum != f64::from(total as f32) {
            return sum;
        }
    }
    total + f64::from(delta)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The first hour is the game's float arithmetic, step by step: `at`, `+`, `-`, `advance`.
    #[test]
    fn the_first_hour_is_the_games_f32_arithmetic() {
        let mut t32 = 0.0f32;
        let mut t = GameTime::ZERO;
        for i in 0..30_000 {
            t32 += 0.1;
            t.advance(0.1);
            assert_eq!(t.f32(), t32, "advance, step {i}");
            let clock = i as f64 * 0.0123;
            assert_eq!(GameTime::at(clock).f32(), clock as f32);
            assert_eq!((GameTime::at(clock) + 0.75).f32(), clock as f32 + 0.75);
            assert_eq!((GameTime::at(clock) - 0.5).f32(), clock as f32 - 0.5);
            assert_eq!(GameTime::at(clock + 1.0) - GameTime::at(clock), (clock + 1.0) as f32 - clock as f32);
        }
    }

    /// Old clocks do not lose their steps: a day, a month, a year.
    #[test]
    fn an_old_clock_keeps_its_steps() {
        for days in [1.0, 24.3, 97.0, 194.0, 400.0, 3650.0] {
            let start = days * 86_400.0;
            let mut t = GameTime::at(start);
            let mut now = GameTime::at(start);
            let mut steps = 0u64;
            for _ in 0..6_000 {
                now = GameTime::at(now.secs() + 0.1);
                steps += t.take_due(now, 0.1);
            }
            // 600 s: 6000 steps, give or take the one still on its way.
            assert!((steps as i64 - 6000).abs() <= 1, "{days} days: {steps} steps in 600 s");
            assert!(t > now && t.secs() - now.secs() <= 0.1 + 1e-3, "{days} days: due {t:?} at {now:?}");
            // And a sum with an interval is exact to the microsecond.
            let due = now + 0.2;
            assert!((due.secs() - now.secs() - f64::from(0.2f32)).abs() < 1e-6);
        }
    }

    /// `take_due` is the game's loop, step for step, while young; and in a few operations, with
    /// the same count and a due time within rounding, when old and after a jump.
    #[test]
    fn take_due_is_the_loop_it_replaces() {
        for start in [0.0, 5.5, 3599.0, 3600.0, 86_400.0 * 30.0] {
            for jump in [0.0, 0.04, 0.35, 12.34, 600.03, 3.0 * 86_400.0 + 0.07] {
                let now = GameTime::at(start + jump);
                let (mut a, mut b) = (GameTime::at(start), GameTime::at(start));
                let by_loop = {
                    let mut n = 0u64;
                    while now >= b {
                        b.advance(0.1);
                        n += 1;
                        // The loop over days is what take_due exists for: stop it at a million.
                        if n > 1_000_000 {
                            break;
                        }
                    }
                    n
                };
                let n = a.take_due(now, 0.1);
                if by_loop <= 1_000_000 {
                    assert_eq!(n, by_loop, "start {start}, jump {jump}");
                    assert!((a.secs() - b.secs()).abs() < 1e-3, "start {start}, jump {jump}: {a:?} against {b:?}");
                }
                assert!(a > now && a.secs() - now.secs() <= 0.1 + 1e-3, "after: {a:?} at {now:?}");
            }
        }
    }

    /// The accumulator: the float sum while young, the exact one after.
    #[test]
    fn a_total_of_millions_keeps_growing_after_the_first_hour() {
        let (mut young, mut old) = (4.9e6f64, 4.9e6f64);
        for _ in 0..10_000 {
            // An f32 has a step of 0.5 there: 1.75 is added in whole steps of it.
            young = add_young(young, 1.75, GameTime::at(100.0));
            old = add_young(old, 1.75, GameTime::at(86_400.0 * 40.0));
        }
        assert_eq!(young, f64::from(young as f32), "an f32 sum stays an f32");
        // The float's own sum, step by step, wherever it moves.
        let mut by_hand = 4.9e6f32;
        for _ in 0..10_000 {
            by_hand += 1.75;
        }
        assert_eq!(young, f64::from(by_hand));
        assert!((old - (4.9e6 + 17_500.0)).abs() < 1e-6, "{old}");
        // Where the float would stand still for good, the first hour does not.
        assert_eq!(f64::from(4.0e7f32 + 1.75), 4.0e7, "the game's float does not move");
        assert_eq!(add_young(4.0e7, 1.75, GameTime::at(10.0)), 4.0e7 + 1.75);
        // Where the float does move, in steps of its own (2 above 2^24), the first hour is its sum.
        assert_eq!(add_young(16_777_216.0, 3.0, GameTime::at(10.0)), 16_777_220.0);
        assert_eq!(add_young(16_777_216.0, 3.0, GameTime::at(10_000.0)), 16_777_219.0);
    }

    #[test]
    fn times_order_and_go_through_serde() {
        assert!(GameTime::PAST < GameTime::ZERO && GameTime::ZERO < GameTime::NEVER);
        assert_eq!(GameTime::NEVER.min(GameTime::at(5.0)), GameTime::at(5.0));
        assert_eq!(GameTime::PAST.max(GameTime::at(5.0)), GameTime::at(5.0));
        for t in [GameTime::PAST, GameTime::ZERO, GameTime::at(0.1), GameTime::at(1e9), GameTime::NEVER] {
            let bytes = crate::bincodec::to_vec(&t).unwrap();
            assert_eq!(crate::bincodec::from_slice::<GameTime>(&bytes).unwrap(), t);
            let json = serde_json::to_string(&t).unwrap();
            assert_eq!(serde_json::from_str::<GameTime>(&json).unwrap(), t, "{json}");
        }
        assert_eq!(crate::bincodec::to_vec(&GameTime::ZERO).unwrap().len(), 1);
    }
}
