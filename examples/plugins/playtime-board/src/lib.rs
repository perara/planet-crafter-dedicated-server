//! playtime-board: how long each player has been on the server, kept in the plugin's own state.
//!
//!   !top [n]         the n players with the most time (default 5, at most 10), one chat line each:
//!                    `1. Ann 12h 05m`
//!   !playtime [name] your time, or the time of a player online or on the board
//!
//! Time is game time: every tick (5 s) each player online is credited the game time since the
//! last tick. A paused world (`standing_still`) credits nothing.
//!
//! This is the "upgrade story" example. The state has two versions:
//!
//!   version 1  a plain list of (player_id, name, minutes)
//!   version 2  (player_id, name, seconds, last_seen in Unix ms): finer time, and a "last seen"
//!
//! `load` reads both. A world saved by version 1 of this plugin loads in version 2 with each
//! player's minutes turned into seconds and "last seen" unknown. `save` always writes version 2.

use pcds_plugin_api::*;

/// The version `save` writes.
const STATE_VERSION: u32 = 2;
/// The most players the board keeps, whatever the setting says.
const MAX_BOARD: usize = 2000;
const DEFAULT_BOARD: usize = 200;
/// Players credited at one tick, at most.
const MAX_ONLINE: usize = 256;
/// One tick credits at most this much game time (a long gap is not play time).
const MAX_STEP_MS: u32 = 60_000;
const NAME_CHARS: usize = 32;
const TOP_DEFAULT: usize = 5;
const TOP_MAX: usize = 10;
/// Players dropped from a full board at one tick, at most.
const TRIM_PER_TICK: usize = 3;

/// One player on the board.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Row {
    /// The `player_id`, which is the same on every visit.
    id: u64,
    name: String,
    seconds: u32,
    /// Wall clock, Unix ms, when the player was last seen; 0 when not known (migrated from version 1).
    last_seen_ms: i64,
}

#[derive(Default)]
struct Board {
    /// Sorted by `id`: a lookup is a binary search, and walking all of them costs little fuel (a
    /// first version kept a `BTreeMap` and a scan of 2000 players went over the budget).
    players: Vec<Row>,
    /// Milliseconds of game time not yet turned into whole seconds.
    carry_ms: u32,
    board_size: usize,
    /// Wall clock of the last tick; 0 before the first one.
    unix_ms: i64,
}

fn cut(text: &str, n: usize) -> String {
    text.chars().take(n).collect()
}

/// `12h 05m`: whole minutes, hours without a limit.
fn hours_minutes(seconds: u32) -> String {
    let minutes = seconds / 60;
    format!("{}h {:02}m", minutes / 60, minutes % 60)
}

/// `just now`, `5 min ago`, `3h ago`, `2d ago`; `unknown` when the time is not known.
fn ago(now_ms: i64, then_ms: i64) -> String {
    if then_ms <= 0 || now_ms <= 0 {
        return "unknown".to_string();
    }
    let minutes = (now_ms - then_ms).max(0) / 60_000;
    match minutes {
        0 => "just now".to_string(),
        1..=59 => format!("{minutes} min ago"),
        60..=2879 => format!("{}h ago", minutes / 60),
        _ => format!("{}d ago", minutes / 1440),
    }
}

/// `a` has more time than `b`; a tie goes to the lower id so the order is always the same.
fn ahead(a: &Row, b: &Row) -> bool {
    a.seconds > b.seconds || (a.seconds == b.seconds && a.id < b.id)
}

/// The chat command prefix of this server (the world says; `!` if it does not).
fn prefix(host: &dyn Host) -> String {
    host.world().map(|w| w.command_prefix).ok().filter(|p| !p.is_empty()).unwrap_or_else(|| "!".to_string())
}

fn board_size(host: &dyn Host) -> usize {
    host.config("board_size").and_then(|v| v.trim().parse::<usize>().ok()).map_or(DEFAULT_BOARD, |n| n.clamp(10, MAX_BOARD))
}

/// The words of what a player typed (at most 8 arguments, each cut), joined by one space.
fn asked_name(args: &[&str]) -> String {
    args.iter().take(8).map(|a| cut(a, NAME_CHARS * 2)).collect::<Vec<_>>().join(" ").split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Looks for a player by what was typed: a name that is the same (ASCII case ignored) wins, else
/// names that begin with it. At most 6 answers; one is a find. A name with a doubled space cannot
/// be found (the chat collapses the spaces of what is typed).
///
/// `see` runs for every name the plugin knows and fuel is the budget, so it turns a name down with
/// as few steps as it can: a length, a first letter, then the rest of the letters.
struct Finder<'a> {
    wanted: &'a [u8],
    first: u8,
    exact: Vec<(u64, String)>,
    begins: Vec<(u64, String)>,
}

impl<'a> Finder<'a> {
    fn new(wanted: &'a str) -> Finder<'a> {
        let wanted = wanted.as_bytes();
        Finder { wanted, first: wanted.first().map_or(0, |b| b.to_ascii_lowercase()), exact: Vec::new(), begins: Vec::new() }
    }

    #[inline]
    fn see(&mut self, id: u64, name: &str) {
        let b = name.as_bytes();
        let same_length = b.len() == self.wanted.len();
        if b.len() < self.wanted.len() || b.first().map_or(0, |c| c.to_ascii_lowercase()) != self.first || (!same_length && self.begins.len() >= 6) {
            return;
        }
        if !b.get(..self.wanted.len()).is_some_and(|head| head.eq_ignore_ascii_case(self.wanted)) || self.exact.iter().chain(self.begins.iter()).any(|(i, _)| *i == id) {
            return;
        }
        // It begins with `wanted`; if it is as long, it is the same.
        let list = if same_length { &mut self.exact } else { &mut self.begins };
        if list.len() < 6 {
            list.push((id, cut(name, NAME_CHARS)));
        }
    }

    fn found(self) -> Vec<(u64, String)> {
        if self.exact.is_empty() { self.begins } else { self.exact }
    }
}

impl Plugin for Board {
    fn load(host: &dyn Host, state: Option<State<'_>>) -> Self {
        let mut me = Board { board_size: board_size(host), ..Board::default() };
        if let Ok(w) = host.world() {
            me.unix_ms = w.unix_ms;
        }
        let Some(state) = state else { return me };
        match state.version {
            1 => me.players = read_v1(state.bytes),
            2 => me.players = read_v2(state.bytes),
            v => host.log(Level::Warn, &format!("state version {v} is newer than this plugin knows: starting with an empty board")),
        }
        me
    }

    fn event(&mut self, host: &dyn Host, event: &Event<'_>) -> Verdict {
        match event {
            Event::Tick { since_ms, unix_ms, .. } => self.on_tick(host, *since_ms, *unix_ms),
            Event::PlayerJoin { name, player_id, .. } => {
                if let Ok(i) = self.find(*player_id) {
                    self.players[i].name = cut(name, NAME_CHARS);
                }
            }
            Event::PlayerLeave { player_id, .. } => {
                let now = self.unix_ms;
                if let Ok(i) = self.find(*player_id) {
                    self.players[i].last_seen_ms = now;
                }
            }
            Event::Command { client, command, args, player_id, .. } => match *command {
                "top" => self.cmd_top(host, *client, args),
                "playtime" => self.cmd_playtime(host, *client, *player_id, args),
                _ => {}
            },
            _ => {}
        }
        Verdict::Continue
    }

    fn save(&mut self) -> Option<SavedState> {
        let mut w = Writer::new();
        w.0.reserve(self.players.len() * 48 + 4);
        w.u32(self.players.len() as u32);
        for r in &self.players {
            w.u64(r.id).str(&r.name).u32(r.seconds).i64(r.last_seen_ms);
        }
        Some(SavedState { version: STATE_VERSION, bytes: w.0 })
    }
}

/// Version 1: a count, then (player_id u64, name, minutes u32). A damaged tail is dropped.
fn read_v1(bytes: &[u8]) -> Vec<Row> {
    let mut r = Reader::new(bytes);
    let mut out = Vec::new();
    for _ in 0..r.u32().unwrap_or(0).min(MAX_BOARD as u32) {
        let (Ok(id), Ok(name), Ok(minutes)) = (r.u64(), r.str(), r.u32()) else { break };
        out.push(Row { id, name: cut(name, NAME_CHARS), seconds: minutes.saturating_mul(60), last_seen_ms: 0 });
    }
    sorted(out)
}

/// Version 2: a count, then (player_id u64, name, seconds u32, last_seen_ms i64).
fn read_v2(bytes: &[u8]) -> Vec<Row> {
    let mut r = Reader::new(bytes);
    let mut out = Vec::new();
    for _ in 0..r.u32().unwrap_or(0).min(MAX_BOARD as u32) {
        let (Ok(id), Ok(name), Ok(seconds), Ok(last_seen_ms)) = (r.u64(), r.str(), r.u32(), r.i64()) else { break };
        out.push(Row { id, name: cut(name, NAME_CHARS), seconds, last_seen_ms });
    }
    sorted(out)
}

/// By id, one row for each id (the first wins): the order `Board::players` relies on.
fn sorted(mut rows: Vec<Row>) -> Vec<Row> {
    rows.sort_by_key(|r| r.id);
    rows.dedup_by_key(|r| r.id);
    rows
}

impl Board {
    /// Where the player is, or where they would go.
    fn find(&self, id: u64) -> Result<usize, usize> {
        self.players.binary_search_by_key(&id, |r| r.id)
    }

    fn on_tick(&mut self, host: &dyn Host, since_ms: u32, unix_ms: i64) {
        self.unix_ms = unix_ms;
        self.board_size = board_size(host);
        // A paused world: nobody is playing.
        if host.world().is_ok_and(|w| w.standing_still) {
            return;
        }
        let Ok(online) = host.players() else { return };
        let total = self.carry_ms.saturating_add(since_ms.min(MAX_STEP_MS));
        let (seconds, carry) = (total / 1000, total % 1000);
        self.carry_ms = carry;
        let mut ids: Vec<u64> = Vec::with_capacity(online.len().min(MAX_ONLINE));
        for p in online.iter().take(MAX_ONLINE) {
            ids.push(p.player_id);
            let i = self.find(p.player_id).unwrap_or_else(|at| {
                self.players.insert(at, Row { id: p.player_id, name: cut(&p.name, NAME_CHARS), seconds: 0, last_seen_ms: 0 });
                at
            });
            let row = &mut self.players[i];
            row.seconds = row.seconds.saturating_add(seconds);
            row.last_seen_ms = unix_ms;
        }
        ids.sort_unstable();
        self.trim(&ids);
    }

    /// Makes the board fit `board_size` by dropping players with the least time who are not in
    /// `online` (sorted). At most `TRIM_PER_TICK` go at a time, because finding one means looking at
    /// every player and fuel is the budget: a board that is over stays a little over for a few ticks.
    fn trim(&mut self, online: &[u64]) {
        for _ in 0..TRIM_PER_TICK {
            if self.players.len() <= self.board_size || !self.drop_least(online) {
                break;
            }
        }
    }

    /// Drops the player with the least time who is not online (a tie: the one seen longest ago).
    /// Whether a player is online is asked only of those that beat the least so far.
    fn drop_least(&mut self, online: &[u64]) -> bool {
        let mut least: Option<(u32, i64, usize)> = None;
        for (i, r) in self.players.iter().enumerate() {
            if least.is_some_and(|(s, seen, _)| (r.seconds, r.last_seen_ms) >= (s, seen)) || online.binary_search(&r.id).is_ok() {
                continue;
            }
            least = Some((r.seconds, r.last_seen_ms, i));
        }
        least.map(|(_, _, i)| self.players.remove(i)).is_some()
    }

    // ---- Commands.

    fn cmd_top(&mut self, host: &dyn Host, client: u64, args: &[&str]) {
        let n = match args.first() {
            None => TOP_DEFAULT,
            Some(a) => match a.parse::<usize>() {
                Ok(n) => n.clamp(1, TOP_MAX),
                Err(_) => {
                    let _ = host.tell(client, &format!("Use: {}top [number from 1 to 10]", prefix(host)));
                    return;
                }
            },
        };
        // The best n, found in one pass: a short sorted list, and most players fail the first comparison.
        let mut best: Vec<&Row> = Vec::with_capacity(n + 1);
        for r in &self.players {
            if best.len() == n && !ahead(r, best[n - 1]) {
                continue;
            }
            let at = best.iter().position(|b| ahead(r, b)).unwrap_or(best.len());
            best.insert(at, r);
            best.truncate(n);
        }
        if best.is_empty() {
            let _ = host.tell(client, "Nobody has played yet.");
        }
        for (i, r) in best.iter().enumerate() {
            let _ = host.tell(client, &format!("{}. {} {}", i + 1, r.name, hours_minutes(r.seconds)));
        }
    }

    fn cmd_playtime(&mut self, host: &dyn Host, client: u64, own_id: u64, args: &[&str]) {
        if args.is_empty() {
            let text = match self.find(own_id) {
                Ok(i) => {
                    let me = &self.players[i];
                    format!("You have played {} (rank {} of {}).", hours_minutes(me.seconds), 1 + self.players.iter().filter(|r| ahead(r, me)).count(), self.players.len())
                }
                Err(_) => "You have played 0h 00m.".to_string(),
            };
            let _ = host.tell(client, &text);
            return;
        }
        let wanted = asked_name(args);
        // Players online now (they may not be on the board for another tick), then the board.
        let online = host.players().unwrap_or_default();
        let mut finder = Finder::new(&wanted);
        for p in &online {
            finder.see(p.player_id, &p.name);
        }
        for r in &self.players {
            finder.see(r.id, &r.name);
        }
        let text = match finder.found().as_slice() {
            [] => format!("No player named {} is online or on the board.", cut(&wanted, NAME_CHARS)),
            [(id, name)] => {
                let row = self.find(*id).ok().map(|i| &self.players[i]);
                let seconds = row.map_or(0, |r| r.seconds);
                if online.iter().any(|p| p.player_id == *id) {
                    format!("{name}: {} played (online now).", hours_minutes(seconds))
                } else {
                    format!("{name}: {} played, last seen {}.", hours_minutes(seconds), ago(self.unix_ms, row.map_or(0, |r| r.last_seen_ms)))
                }
            }
            many => cut(&format!("Several players match: {}. Type more of the name.", many.iter().take(5).map(|(_, n)| n.as_str()).collect::<Vec<_>>().join(", ")), 400),
        };
        let _ = host.tell(client, &text);
    }
}

export!(Board);

#[cfg(test)]
mod tests {
    use super::*;
    use pcds_plugin_api::testing::{Harness, MockHost};

    /// The bytes version 1 of this plugin saved: the same layout `read_v1` reads.
    fn v1_bytes(list: &[(u64, &str, u32)]) -> Vec<u8> {
        let mut w = Writer::new();
        w.u32(list.len() as u32);
        for (id, name, minutes) in list {
            w.u64(*id).str(name).u32(*minutes);
        }
        w.0
    }

    fn row(t: &Harness<Board>, id: u64) -> Row {
        t.plugin().players[t.plugin().find(id).unwrap()].clone()
    }

    fn start() -> Harness<Board> {
        Harness::<Board>::start(MockHost::new()).tick_every_ms(5000).tick_step_ms(1000)
    }

    fn told(t: &Harness<Board>) -> Vec<String> {
        t.mock().told().into_iter().map(|(_, s)| s).collect()
    }

    fn minutes(t: &mut Harness<Board>, n: u64) {
        t.advance_ms(n * 60_000);
    }

    #[test]
    fn time_is_counted_for_those_online_and_top_shows_the_best_first() {
        let mut t = start();
        t.join(1, "Ann");
        t.join(2, "Bo");
        t.join(3, "Cy");
        minutes(&mut t, 30);
        t.leave(3);
        minutes(&mut t, 90);
        t.leave(2);
        minutes(&mut t, 60);
        // Ann 3h 00m, Bo 2h 00m, Cy 0h 30m.
        t.clear();
        t.command(1, "top", &[]);
        assert_eq!(told(&t), ["1. Ann 3h 00m", "2. Bo 2h 00m", "3. Cy 0h 30m"]);
        t.clear();
        t.command(1, "top", &["2"]);
        assert_eq!(told(&t), ["1. Ann 3h 00m", "2. Bo 2h 00m"]);
        t.clear();
        t.command(1, "playtime", &[]);
        assert_eq!(told(&t), ["You have played 3h 00m (rank 1 of 3)."]);
        t.clear();
        t.command(1, "playtime", &["bo"]);
        assert_eq!(told(&t), ["Bo: 2h 00m played, last seen 1h ago."]);
        t.clear();
        t.command(2, "playtime", &["ANN"]);
        // Bo is not online any more; the command still works (the mock does not care).
        assert_eq!(told(&t), ["Ann: 3h 00m played (online now)."]);
    }

    #[test]
    fn top_takes_a_number_and_refuses_garbage() {
        let mut t = start();
        for i in 1..=12u64 {
            t.join(i, &format!("P{i}"));
            minutes(&mut t, 1);
        }
        for (arg, lines) in [("1", 1), ("10", 10), ("99", 10), ("0", 1), ("3", 3)] {
            t.clear();
            t.command(1, "top", &[arg]);
            assert_eq!(told(&t).len(), lines, "!top {arg}");
        }
        t.clear();
        t.command(1, "top", &[]);
        assert_eq!(told(&t).len(), 5);
        for bad in ["abc", "-3", "2.5", "99999999999999999999999", ""] {
            t.clear();
            t.command(1, "top", &[bad]);
            assert_eq!(told(&t), ["Use: !top [number from 1 to 10]"], "!top {bad:?}");
        }
        t.clear();
        t.command(1, "TOP", &["2"]);
        assert_eq!(told(&t).len(), 2, "the command name may be typed in capitals");
        let mut empty = start();
        empty.join(1, "Ann");
        empty.command(1, "top", &[]);
        assert_eq!(told(&empty), ["Nobody has played yet."]);
    }

    #[test]
    fn a_paused_world_counts_nothing_until_it_runs_again() {
        let mut t = start();
        t.join(1, "Ann");
        t.mock().set_world(|w| w.standing_still = true);
        minutes(&mut t, 10);
        assert!(t.plugin().players.is_empty());
        t.mock().set_world(|w| w.standing_still = false);
        minutes(&mut t, 2);
        assert_eq!(row(&t, 1001).seconds, 120);
    }

    #[test]
    fn the_usage_text_uses_the_prefix_the_world_has() {
        let mut t = start();
        t.join(1, "Ann");
        t.mock().set_world(|w| w.command_prefix = "?".into());
        t.command(1, "top", &["many"]);
        assert_eq!(told(&t), ["Use: ?top [number from 1 to 10]"]);
    }

    #[test]
    fn partial_seconds_are_carried_not_lost() {
        let mut t = Harness::<Board>::start(MockHost::new()).tick_every_ms(700).tick_step_ms(100);
        t.join(1, "Ann");
        t.advance_ms(100_000);
        // 142 ticks of 700 ms = 99.4 s: 99 whole seconds, 400 ms carried.
        assert_eq!(row(&t, 1001).seconds, 99);
        assert_eq!(t.plugin().carry_ms, 400);
    }

    #[test]
    fn the_board_survives_a_reload_with_names_and_last_seen() {
        let mut t = start();
        t.join(1, "Ann");
        minutes(&mut t, 10);
        t.leave(1);
        t.reload();
        t.join(2, "Bo");
        t.clear();
        t.command(2, "playtime", &["ann"]);
        assert_eq!(told(&t), ["Ann: 0h 10m played, last seen just now."]);
        let saved = t.save().unwrap();
        assert_eq!(saved.version, 2);
    }

    #[test]
    fn version_1_state_loads_and_is_saved_as_version_2() {
        // Real version-1 bytes: a count of 2, then (id, name, minutes) twice.
        let mut hand: Vec<u8> = vec![2, 0, 0, 0];
        hand.extend_from_slice(&1001u64.to_le_bytes());
        hand.extend_from_slice(&[3, 0, 0, 0, b'A', b'n', b'n']);
        hand.extend_from_slice(&125u32.to_le_bytes());
        hand.extend_from_slice(&1002u64.to_le_bytes());
        hand.extend_from_slice(&[2, 0, 0, 0, b'B', b'o']);
        hand.extend_from_slice(&7u32.to_le_bytes());
        assert_eq!(hand, v1_bytes(&[(1001, "Ann", 125), (1002, "Bo", 7)]));
        let mut t = Harness::<Board>::start_with_state(MockHost::new(), 1, &hand).tick_every_ms(5000).tick_step_ms(1000);
        t.join(9, "Zed");
        t.command(9, "top", &[]);
        assert_eq!(told(&t), ["1. Ann 2h 05m", "2. Bo 0h 07m"]);
        t.clear();
        // Offline and migrated: the time of the last visit is not known.
        t.command(9, "playtime", &["Ann"]);
        assert_eq!(told(&t), ["Ann: 2h 05m played, last seen unknown."]);
        // It is saved as version 2 and a new instance reads it back, seconds and all.
        t.reload();
        assert_eq!(row(&t, 1001), Row { id: 1001, name: "Ann".into(), seconds: 125 * 60, last_seen_ms: 0 });
        let saved = t.save().unwrap();
        assert_eq!(saved.version, 2);
        // A damaged version 1 (cut in the middle of an entry) keeps what is whole.
        let cut_short = &hand[..hand.len() - 3];
        let t = Harness::<Board>::start_with_state(MockHost::new(), 1, cut_short);
        assert_eq!(t.plugin().players.len(), 1);
        // A huge number of minutes does not overflow.
        let t = Harness::<Board>::start_with_state(MockHost::new(), 1, &v1_bytes(&[(5, "Max", u32::MAX)]));
        assert_eq!(row(&t, 5).seconds, u32::MAX);
    }

    #[test]
    fn a_newer_state_version_starts_empty_with_a_log_line() {
        let t = Harness::<Board>::start_with_state(MockHost::new(), 3, &[1, 2, 3]);
        assert!(t.plugin().players.is_empty());
        assert!(t.mock().logs().iter().any(|(l, s)| *l == Level::Warn && s.contains("state version 3")));
    }

    fn v2_bytes(list: &[(u64, &str, u32)]) -> Vec<u8> {
        let mut w = Writer::new();
        w.u32(list.len() as u32);
        for (id, name, seconds) in list {
            w.u64(*id).str(name).u32(*seconds).i64(1);
        }
        w.0
    }

    #[test]
    fn a_full_board_drops_the_least_time_among_those_not_online() {
        // Ten offline players with 1000 s, 2000 s, ... 10000 s; the board holds ten.
        let list: Vec<(u64, &str, u32)> = (1..=10).map(|i| (i as u64, "Old", i * 1000)).collect();
        let mock = MockHost::new().with_setting("board_size", "10");
        let mut t = Harness::<Board>::start_with_state(mock, 2, &v2_bytes(&list)).tick_every_ms(5000).tick_step_ms(1000);
        t.join_as(50, "New", 5000, "");
        t.advance_ms(5000);
        assert_eq!(t.plugin().players.len(), 10);
        assert!(t.plugin().find(1).is_err(), "the least time went");
        assert!(t.plugin().find(5000).is_ok(), "a player online is never dropped");
        // The admin lowers nothing below 10: a smaller number is read as 10.
        t.mock().set_setting("board_size", "3");
        t.advance_ms(5000);
        assert_eq!(t.plugin().players.len(), 10);
    }

    #[test]
    fn lowering_board_size_trims_a_few_at_a_time() {
        let list: Vec<(u64, &str, u32)> = (1..=40).map(|i| (i as u64, "Old", i * 100)).collect();
        let mut t = Harness::<Board>::start_with_state(MockHost::new().with_setting("board_size", "200"), 2, &v2_bytes(&list)).tick_every_ms(5000).tick_step_ms(1000);
        t.mock().set_setting("board_size", "25");
        t.advance_ms(5000);
        assert_eq!(t.plugin().players.len(), 37, "three at a tick");
        t.advance_ms(25_000);
        let kept: Vec<u64> = t.plugin().players.iter().map(|r| r.id).collect();
        assert_eq!(kept, (16..=40).collect::<Vec<u64>>());
    }

    #[test]
    fn a_flood_of_new_players_keeps_the_board_at_its_size() {
        let mut t = Harness::<Board>::start(MockHost::new().with_setting("board_size", "50")).tick_every_ms(1000).tick_step_ms(1000);
        for i in 0..400u64 {
            t.join_as(i + 1, &format!("Player {i}"), 10_000 + i, "");
            t.advance_ms(1000);
            t.leave(i + 1);
            assert!(t.plugin().players.len() <= 50);
        }
        // More players online than the board holds: nobody online is dropped, the board is over for a while.
        for i in 0..60u64 {
            t.join_as(1000 + i, "Crowd", 90_000 + i, "");
        }
        t.advance_ms(1000);
        assert!(t.plugin().players.len() >= 60 && t.plugin().players.len() <= 60 + 50);
        t.clear();
        t.command(1000, "top", &["10"]);
        assert_eq!(told(&t).len(), 10);
    }

    #[test]
    fn hostile_input_is_cut_or_refused() {
        let mut t = start();
        t.join(1, "Ann");
        minutes(&mut t, 1);
        let huge = "y".repeat(100_000);
        t.command(1, "playtime", &[&huge]);
        t.command(1, "playtime", &["\u{0}", "\u{1F680}", &huge]);
        t.command(1, "top", &[&huge]);
        t.command(1, "top", &["1", "2", "3"]);
        t.command(1, "nothing", &[]);
        assert!(told(&t).iter().all(|s| s.chars().count() < 120), "{:?}", told(&t));
        t.clear();
        t.command(1, "playtime", &["nobody", "here"]);
        assert_eq!(told(&t), ["No player named nobody here is online or on the board."]);
        // Capitals are ignored, and the words may come as separate arguments or be the start of a name.
        let mut t = start();
        t.join(1, "Ann Lee");
        minutes(&mut t, 1);
        t.clear();
        t.command(1, "playtime", &["ann", "lee"]);
        t.command(1, "playtime", &["AN"]);
        assert_eq!(told(&t), ["Ann Lee: 0h 01m played (online now).", "Ann Lee: 0h 01m played (online now)."]);
        // A name with a doubled space cannot be typed (the chat collapses the spaces), so it is not found by its full name.
        t.join(2, "Bo  Peep");
        t.clear();
        t.command(1, "playtime", &["bo", "peep"]);
        assert_eq!(told(&t), ["No player named bo peep is online or on the board."]);
        // Names are cut when kept.
        let mut t = start();
        t.join(1, &"N".repeat(5000));
        minutes(&mut t, 1);
        assert_eq!(row(&t, 1001).name.chars().count(), NAME_CHARS);
        // Garbage bytes as a state, in both versions.
        for v in [1, 2] {
            for bytes in [&[][..], &[255, 255, 255, 255], &[1, 0, 0, 0, 1, 2, 3], &[9, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 255, 255, 255, 255]] {
                let t = Harness::<Board>::start_with_state(MockHost::new(), v, bytes);
                assert!(t.plugin().players.len() <= 1);
            }
        }
    }

    #[test]
    fn top_ten_fits_in_one_call_with_the_real_hosts_limit() {
        let mut t = Harness::<Board>::start(MockHost::new().with_limits()).tick_every_ms(5000).tick_step_ms(1000);
        for i in 1..=20u64 {
            t.join(i, &format!("P{i}"));
        }
        t.advance_ms(60_000);
        t.clear();
        t.command(1, "top", &["10"]);
        assert_eq!(told(&t).len(), 10);
    }

    #[test]
    fn denied_capabilities_do_not_panic() {
        let mut t = Harness::<Board>::start(MockHost::new().deny(&["chat.say", "world.read"])).tick_every_ms(5000);
        t.join(1, "Ann");
        t.advance_ms(60_000);
        t.command(1, "top", &[]);
        t.command(1, "playtime", &["Ann"]);
        t.reload();
        assert!(t.mock().told().is_empty());
        assert!(t.plugin().players.is_empty(), "without world.read nothing can be counted");
    }
}
