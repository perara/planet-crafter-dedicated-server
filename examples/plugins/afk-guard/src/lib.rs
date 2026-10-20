//! afk-guard: a small moderator. It removes players who have gone away from the keyboard and it
//! notices a player who takes buildings apart faster than anyone builds.
//!
//!   AFK    a player whose position stays within 1 unit for `afk_minutes` is warned
//!          `afk_warn_seconds` before, then kicked. Operators are never kicked.
//!   Grief  a player who destroys more than `destroy_limit` objects within
//!          `destroy_window_seconds` gets a strike. The player and the operators online are
//!          told; with `grief_action = "kick"` the player is also kicked.
//!   !guard   (operators) who is idle, and who has strikes
//!   !pardon  (operators) clears the strikes of a player, found by name
//!
//! What it shows: reading positions with `world.read`, counting by `player_id` (stable across
//! visits) instead of `client` (the connection number), settings read with `Host::settings`,
//! state with a version, and bounded bookkeeping for every list it keeps.

use pcds_plugin_api::*;

/// Version of the saved bytes. Version 1: a count, then for each player with strikes
/// (player_id, name, strikes, last strike in Unix ms).
const STATE_VERSION: u32 = 1;
/// Players with strikes that are kept; when full, the oldest strike is dropped to make room.
const MAX_STRIKES: usize = 500;
/// Players online that are tracked for idleness and for destroy counts.
const MAX_TRACKED: usize = 64;
/// A strike is forgotten after this long (wall clock).
const STRIKE_TTL_MS: i64 = 24 * 3_600_000;
/// One tick never credits more idle time than this, whatever `since_ms` says.
const MAX_STEP_MS: u32 = 60_000;
/// "Did not move": the squared distance of one unit.
const MOVED_SQ: f32 = 1.0;
/// Only players idle at least this long are listed by `!guard`.
const LIST_IDLE_SECS: u32 = 60;
/// Names kept in state and chat lines are cut to this many characters.
const NAME_CHARS: usize = 32;
/// A chat line made of many names is cut to this many characters (the chat takes 500).
const LINE_CHARS: usize = 400;

/// The settings, read again at every tick so an admin's change takes effect within seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Settings {
    afk_secs: u32,
    warn_secs: u32,
    destroy_limit: u32,
    window_ms: i64,
    kick_on_grief: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { afk_secs: 15 * 60, warn_secs: 60, destroy_limit: 25, window_ms: 60_000, kick_on_grief: false }
    }
}

impl Settings {
    /// The manifest's bounds are checked by the host; the clamps here only keep a host that
    /// forgot to check from doing harm.
    fn read(host: &dyn Host) -> Settings {
        let list = host.settings().unwrap_or_default();
        let find = |key: &str| list.iter().find(|(k, _)| k == key).map(|(_, v)| v.trim());
        let int = |key: &str, default: u32, least: u32, most: u32| find(key).and_then(|v| v.parse::<u32>().ok()).map_or(default, |v| v.clamp(least, most));
        let d = Settings::default();
        Settings {
            afk_secs: int("afk_minutes", d.afk_secs / 60, 1, 240) * 60,
            warn_secs: int("afk_warn_seconds", d.warn_secs, 0, 600),
            destroy_limit: int("destroy_limit", d.destroy_limit, 1, 500),
            window_ms: i64::from(int("destroy_window_seconds", (d.window_ms / 1000) as u32, 5, 3600)) * 1000,
            kick_on_grief: find("grief_action") == Some("kick"),
        }
    }

    /// The warning comes at most halfway to the kick, so a long warning on a short limit still
    /// leaves the player half the time.
    fn warn_secs(&self) -> u32 {
        self.warn_secs.min(self.afk_secs / 2)
    }
}

/// What is known about one player online, for idleness. Not saved: a restart starts everybody's
/// clock again, which is the kind way round.
#[derive(Debug, Clone)]
struct Track {
    player_id: u64,
    /// The connection: a player who left and came back is a new track.
    client: u64,
    /// Where the player last moved to. The player has "moved" when they are over 1 unit from here.
    anchor: [f32; 3],
    idle_ms: u32,
    warned: bool,
    operator: bool,
}

/// A player with strikes, kept in state.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Strike {
    player_id: u64,
    name: String,
    strikes: u32,
    last_unix_ms: i64,
}

#[derive(Default)]
struct Guard {
    settings: Settings,
    strikes: Vec<Strike>,
    tracks: Vec<Track>,
    /// Game times of recent destroys, for each player who did any. Never longer than `destroy_limit + 1`.
    destroys: Vec<(u64, Vec<i64>)>,
    /// Game time and wall clock of the last tick (or of `load`): the destroy event has no clock.
    now_ms: i64,
    unix_ms: i64,
}

/// `text` cut to `n` characters.
fn cut(text: &str, n: usize) -> String {
    text.chars().take(n).collect()
}

/// "7m 05s", or "45s" under a minute.
fn minutes_seconds(secs: u32) -> String {
    if secs < 60 { format!("{secs}s") } else { format!("{}m {:02}s", secs / 60, secs % 60) }
}

/// The words of what a player typed (at most 8 arguments of 64 characters), joined by one space.
fn asked_name(args: &[&str]) -> String {
    args.iter().take(8).map(|a| cut(a, NAME_CHARS * 2)).collect::<Vec<_>>().join(" ").split_whitespace().collect::<Vec<_>>().join(" ")
}

fn dist_sq(a: [f32; 3], b: [f32; 3]) -> f32 {
    let (x, y, z) = (a[0] - b[0], a[1] - b[1], a[2] - b[2]);
    x * x + y * y + z * z
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

impl Plugin for Guard {
    fn load(host: &dyn Host, state: Option<State<'_>>) -> Self {
        // The world cannot be read inside `load` (only settings can): the clocks start at 0, and
        // the first tick, or the first destroy that needs them, sets them.
        let mut me = Guard { settings: Settings::read(host), ..Guard::default() };
        let Some(state) = state else { return me };
        match state.version {
            1 => me.strikes = read_strikes(state.bytes),
            v => host.log(Level::Warn, &format!("state version {v} is newer than this plugin knows: starting without strikes")),
        }
        me
    }

    fn event(&mut self, host: &dyn Host, event: &Event<'_>) -> Verdict {
        match event {
            Event::Tick { game_ms, since_ms, unix_ms, .. } => self.on_tick(host, *game_ms, *since_ms, *unix_ms),
            Event::PlayerJoin { name, player_id, .. } => {
                // A new visit: the idle clock and the destroy count start again; a known name is kept up to date.
                self.forget_session(*player_id);
                if let Some(s) = self.strikes.iter_mut().find(|s| s.player_id == *player_id) {
                    s.name = cut(name, NAME_CHARS);
                }
            }
            Event::PlayerLeave { player_id, .. } => self.forget_session(*player_id),
            Event::ObjectDestroyed { client, .. } => self.on_destroyed(host, *client),
            Event::Command { client, command, args, .. } => match command.to_ascii_lowercase().as_str() {
                "guard" => self.cmd_guard(host, *client),
                "pardon" => self.cmd_pardon(host, *client, args),
                _ => {}
            },
            _ => {}
        }
        Verdict::Continue
    }

    fn save(&mut self) -> Option<SavedState> {
        let mut w = Writer::new();
        w.u32(self.strikes.len() as u32);
        for s in &self.strikes {
            w.u64(s.player_id).str(&s.name).u32(s.strikes).i64(s.last_unix_ms);
        }
        Some(SavedState { version: STATE_VERSION, bytes: w.0 })
    }
}

/// Reads version 1 state. A damaged tail is dropped, what was read before it is kept.
fn read_strikes(bytes: &[u8]) -> Vec<Strike> {
    let mut r = Reader::new(bytes);
    let mut out = Vec::new();
    let n = r.u32().unwrap_or(0) as usize;
    for _ in 0..n.min(MAX_STRIKES) {
        let (Ok(player_id), Ok(name), Ok(strikes), Ok(last_unix_ms)) = (r.u64(), r.str(), r.u32(), r.i64()) else { break };
        out.push(Strike { player_id, name: cut(name, NAME_CHARS), strikes, last_unix_ms });
    }
    out
}

impl Guard {
    fn forget_session(&mut self, player_id: u64) {
        self.tracks.retain(|t| t.player_id != player_id);
        self.destroys.retain(|(id, _)| *id != player_id);
    }

    // ---- AFK.

    fn on_tick(&mut self, host: &dyn Host, game_ms: i64, since_ms: u32, unix_ms: i64) {
        self.now_ms = game_ms;
        self.unix_ms = unix_ms;
        self.settings = Settings::read(host);
        self.expire_strikes();
        let Ok(players) = host.players() else { return };
        // While the world stands still nobody is idle: the clock does not run.
        let step = if host.world().is_ok_and(|w| w.standing_still) { 0 } else { since_ms.min(MAX_STEP_MS) };
        let mut old = core::mem::take(&mut self.tracks);
        for p in players.iter().take(MAX_TRACKED) {
            let mut t = match old.iter().position(|t| t.player_id == p.player_id && t.client == p.client) {
                Some(i) => old.swap_remove(i),
                None => Track { player_id: p.player_id, client: p.client, anchor: p.position, idle_ms: 0, warned: false, operator: p.operator },
            };
            t.operator = p.operator;
            // A position that is not a number is "not within a unit" (NaN compares false): it counts as
            // having moved, so bad data never gets a player kicked.
            if dist_sq(p.position, t.anchor) <= MOVED_SQ {
                t.idle_ms = t.idle_ms.saturating_add(step);
            } else {
                t.anchor = p.position;
                t.idle_ms = 0;
                t.warned = false;
            }
            self.tracks.push(t);
        }
        self.act_on_idle(host);
    }

    /// Warns and kicks. A `Full` answer (16 actions in a call) leaves the rest for the next tick.
    fn act_on_idle(&mut self, host: &dyn Host) {
        let (afk, warn) = (self.settings.afk_secs, self.settings.warn_secs());
        let mut gone = Vec::new();
        for t in self.tracks.iter_mut().filter(|t| !t.operator) {
            let idle = t.idle_ms / 1000;
            if idle >= afk {
                match host.kick(t.client, &format!("Removed for being away from the keyboard ({}). You are welcome to rejoin.", minutes_seconds(idle))) {
                    Ok(()) => gone.push(t.client),
                    Err(Error::Full) => break,
                    Err(e) => {
                        host.log(Level::Warn, &format!("cannot kick an idle player: {e:?}"));
                        t.idle_ms = 0;
                    }
                }
            } else if !t.warned && idle + warn >= afk {
                match host.tell(t.client, &format!("You seem to be away. Move within {} seconds or you will be removed from the server.", afk - idle)) {
                    Err(Error::Full) => break,
                    _ => t.warned = true,
                }
            }
        }
        self.tracks.retain(|t| !gone.contains(&t.client));
    }

    // ---- Grief.

    fn on_destroyed(&mut self, host: &dyn Host, client: u64) {
        // Nobody did it (a meteor, a machine): not a player's doing.
        if client == 0 {
            return;
        }
        let Ok(players) = host.players() else { return };
        let Some(p) = players.iter().find(|p| p.client == client) else { return };
        if p.operator {
            return;
        }
        if self.unix_ms == 0
            && let Ok(w) = host.world()
        {
            self.now_ms = w.game_ms;
            self.unix_ms = w.unix_ms;
        }
        let (limit, window) = (self.settings.destroy_limit as usize, self.settings.window_ms);
        let now = self.now_ms;
        let i = match self.destroys.iter().position(|(id, _)| *id == p.player_id) {
            Some(i) => i,
            None => {
                if self.destroys.len() >= MAX_TRACKED {
                    self.destroys.remove(0);
                }
                self.destroys.push((p.player_id, Vec::new()));
                self.destroys.len() - 1
            }
        };
        let list = &mut self.destroys[i].1;
        list.retain(|t| now - *t < window);
        list.push(now);
        // A setting lowered since the list grew: keep the newest ones that matter.
        if list.len() > limit + 1 {
            list.drain(..list.len() - limit - 1);
        }
        if list.len() <= limit {
            return;
        }
        let count = list.len();
        list.clear();
        let strikes = self.add_strike(p.player_id, &p.name);
        let seconds = window / 1000;
        let what = if self.settings.kick_on_grief { "kicked" } else { "warned" };
        let line = format!("Guard: {} destroyed {count} objects in {seconds} s (strike {strikes}). Action: {what}.", cut(&p.name, NAME_CHARS));
        for op in players.iter().filter(|o| o.operator) {
            let _ = host.tell(op.client, &line);
        }
        if self.settings.kick_on_grief {
            let _ = host.kick(client, &format!("Removed by the grief guard: {count} objects destroyed within {seconds} seconds."));
        } else {
            let _ = host.tell(client, &format!("Guard: you took apart {count} objects in {seconds} s. Slow down; the operators have been told."));
        }
    }

    /// One more strike for the player; returns how many they have now.
    fn add_strike(&mut self, player_id: u64, name: &str) -> u32 {
        let (name, now) = (cut(name, NAME_CHARS), self.unix_ms);
        if let Some(s) = self.strikes.iter_mut().find(|s| s.player_id == player_id) {
            s.name = name;
            s.strikes = s.strikes.saturating_add(1);
            s.last_unix_ms = now;
            return s.strikes;
        }
        if self.strikes.len() >= MAX_STRIKES {
            // The oldest strike goes. (Strikes expire after a day anyway: this is a flood guard.)
            if let Some(oldest) = (0..self.strikes.len()).min_by_key(|i| self.strikes[*i].last_unix_ms) {
                self.strikes.swap_remove(oldest);
            }
        }
        self.strikes.push(Strike { player_id, name, strikes: 1, last_unix_ms: now });
        1
    }

    fn expire_strikes(&mut self) {
        let now = self.unix_ms;
        // Without a wall clock nothing expires. A strike given before the clock was known
        // (last time 0) starts its day now.
        if now > 0 {
            self.strikes.iter_mut().filter(|s| s.last_unix_ms <= 0).for_each(|s| s.last_unix_ms = now);
            self.strikes.retain(|s| now - s.last_unix_ms < STRIKE_TTL_MS);
        }
    }

    // ---- Commands.

    fn cmd_guard(&mut self, host: &dyn Host, client: u64) {
        let s = self.settings;
        let _ = host.tell(
            client,
            &format!("Guard: away after {} min (warning {} s before); grief: more than {} destroyed in {} s ({}).", s.afk_secs / 60, s.warn_secs(), s.destroy_limit, s.window_ms / 1000, if s.kick_on_grief { "kick" } else { "warn" }),
        );
        // Names are looked up here, not kept for every player at every tick.
        let players = host.players().unwrap_or_default();
        let mut idle: Vec<(String, &Track)> = self.tracks.iter().filter(|t| t.idle_ms / 1000 >= LIST_IDLE_SECS).map(|t| (players.iter().find(|p| p.player_id == t.player_id).map_or_else(|| "?".to_string(), |p| cut(&p.name, NAME_CHARS)), t)).collect();
        idle.sort_by(|a, b| b.1.idle_ms.cmp(&a.1.idle_ms).then_with(|| a.0.cmp(&b.0)));
        let line = if idle.is_empty() {
            "Idle: nobody.".to_string()
        } else {
            let shown: Vec<String> = idle.iter().take(10).map(|(name, t)| format!("{name} {}{}", minutes_seconds(t.idle_ms / 1000), if t.operator { " (operator)" } else { "" })).collect();
            format!("Idle: {}", shown.join(", "))
        };
        let _ = host.tell(client, &cut(&line, LINE_CHARS));
        let mut struck: Vec<&Strike> = self.strikes.iter().collect();
        struck.sort_by(|a, b| b.strikes.cmp(&a.strikes).then(b.last_unix_ms.cmp(&a.last_unix_ms)));
        let line = if struck.is_empty() { "Strikes: none.".to_string() } else { format!("Strikes: {}", struck.iter().take(10).map(|s| format!("{} {}", s.name, s.strikes)).collect::<Vec<_>>().join(", ")) };
        let _ = host.tell(client, &cut(&line, LINE_CHARS));
    }

    fn cmd_pardon(&mut self, host: &dyn Host, client: u64, args: &[&str]) {
        let wanted = asked_name(args);
        if wanted.is_empty() {
            let _ = host.tell(client, "Use: !pardon <player name>");
            return;
        }
        // Players online (their name now) first, then players with strikes (the name they had).
        let online = host.players().unwrap_or_default();
        let mut finder = Finder::new(&wanted);
        for p in &online {
            finder.see(p.player_id, &p.name);
        }
        for s in &self.strikes {
            finder.see(s.player_id, &s.name);
        }
        match finder.found().as_slice() {
            [] => {
                let _ = host.tell(client, &format!("No player named {} is online or has strikes.", cut(&wanted, NAME_CHARS)));
            }
            [(id, name)] => {
                let had = self.strikes.iter().find(|s| s.player_id == *id).map_or(0, |s| s.strikes);
                self.strikes.retain(|s| s.player_id != *id);
                self.destroys.retain(|(d, _)| d != id);
                let _ = host.tell(client, &format!("Pardoned {name}: {had} strike{} cleared.", if had == 1 { "" } else { "s" }));
            }
            many => {
                let names: Vec<&str> = many.iter().take(5).map(|(_, n)| n.as_str()).collect();
                let _ = host.tell(client, &cut(&format!("Several players match: {}. Type more of the name.", names.join(", ")), LINE_CHARS));
            }
        }
    }
}

export!(Guard);

#[cfg(test)]
mod tests {
    use super::*;
    use pcds_plugin_api::testing::{Harness, MockHost};

    const OP: u64 = 1;
    const ANN: u64 = 2;
    const BO: u64 = 3;

    /// The server's operator (client 1) and two players, a mock that ticks every 5 s like the manifest says.
    fn room(mock: MockHost) -> Harness<Guard> {
        let mut t = Harness::<Guard>::start(mock).tick_every_ms(5000).tick_step_ms(1000);
        t.join(OP, "Root");
        t.join(ANN, "Ann");
        t.join(BO, "Bo");
        // join() adds players at the origin and not operator: make client 1 an operator.
        let mut root = t.mock().players().unwrap().into_iter().find(|p| p.client == OP).unwrap();
        root.operator = true;
        t.mock().add_player(root);
        t
    }

    fn mock() -> MockHost {
        MockHost::new().with_setting("afk_minutes", "1").with_setting("afk_warn_seconds", "20").with_setting("destroy_limit", "3").with_setting("destroy_window_seconds", "60")
    }

    fn move_to(t: &Harness<Guard>, client: u64, position: [f32; 3]) {
        let mut p = t.mock().players().unwrap().into_iter().find(|p| p.client == client).unwrap();
        p.position = position;
        t.mock().add_player(p);
    }

    fn told_to(t: &Harness<Guard>, client: u64) -> Vec<String> {
        t.mock().told().into_iter().filter(|(c, _)| *c == client).map(|(_, s)| s).collect()
    }

    #[test]
    fn an_idle_player_is_warned_then_kicked_and_the_operator_is_not() {
        let mut t = room(mock());
        t.advance_ms(35_000);
        assert!(told_to(&t, ANN).is_empty(), "no warning before the last 20 seconds");
        t.advance_ms(10_000);
        let warning = told_to(&t, ANN);
        assert_eq!(warning.len(), 1, "{warning:?}");
        assert!(warning[0].starts_with("You seem to be away. Move within "), "{}", warning[0]);
        assert!(t.mock().kicked().is_empty());
        t.advance_ms(30_000);
        let kicked = t.mock().kicked();
        let clients: Vec<u64> = kicked.iter().map(|k| k.0).collect();
        assert_eq!(clients, [ANN, BO], "{kicked:?}");
        assert_eq!(kicked[0].1, "Removed for being away from the keyboard (1m 00s). You are welcome to rejoin.");
        assert_eq!(told_to(&t, ANN).len(), 1, "one warning only");
    }

    #[test]
    fn moving_resets_the_clock_and_a_small_shuffle_does_not() {
        let mut t = room(mock());
        t.advance_ms(45_000);
        assert_eq!(told_to(&t, ANN).len(), 1);
        move_to(&t, ANN, [5.0, 0.0, 0.0]);
        // Bo only shuffles by half a unit.
        move_to(&t, BO, [0.5, 0.0, 0.0]);
        t.advance_ms(50_000);
        let kicked: Vec<u64> = t.mock().kicked().iter().map(|k| k.0).collect();
        assert_eq!(kicked, [BO], "Ann moved, Bo did not");
        // Ann is warned a second time after another quiet spell: the move reset the warning.
        assert_eq!(told_to(&t, ANN).len(), 2);
    }

    #[test]
    fn nobody_ages_while_the_world_stands_still() {
        let still = WorldInfo { standing_still: true, ..WorldInfo::default() };
        let mut t = room(mock().with_world(still));
        t.advance_ms(10 * 60_000);
        assert!(t.mock().kicked().is_empty() && told_to(&t, ANN).is_empty());
    }

    #[test]
    fn a_player_who_comes_back_starts_a_new_clock() {
        let mut t = room(mock());
        t.advance_ms(40_000);
        t.leave(ANN);
        t.join_as(ANN + 10, "Ann", ANN + 1000, "");
        t.advance_ms(30_000);
        assert!(t.mock().kicked().iter().all(|k| k.0 != ANN + 10), "30 s into the new visit is too early");
        assert!(told_to(&t, ANN + 10).is_empty());
    }

    #[test]
    fn a_flood_of_destroys_gives_a_strike_a_warning_and_a_note_to_the_operators() {
        let mut t = room(mock());
        for id in 0..3 {
            t.destroy(id, ANN);
        }
        assert!(t.mock().told().is_empty(), "three is the limit, not over it");
        t.destroy(3, ANN);
        let ops = told_to(&t, OP);
        assert_eq!(ops, ["Guard: Ann destroyed 4 objects in 60 s (strike 1). Action: warned."]);
        assert!(told_to(&t, ANN)[0].starts_with("Guard: you took apart 4 objects"));
        assert!(t.mock().kicked().is_empty());
        // The count starts again after a strike: four more are the second strike.
        for id in 10..14 {
            t.destroy(id, ANN);
        }
        assert!(told_to(&t, OP)[1].contains("strike 2"));
    }

    #[test]
    fn slow_destroying_and_things_nobody_did_are_not_counted() {
        let mut t = room(mock().with_setting("afk_minutes", "240"));
        for id in 0..40 {
            t.destroy(id, 0);
        }
        // One every 20 s: never more than 3 inside 60 s.
        for id in 100..110 {
            t.destroy(id, BO);
            t.advance_ms(20_000);
        }
        // A client that is gone cannot be named.
        t.destroy(300, 99);
        assert!(t.mock().told().is_empty() && t.mock().kicked().is_empty());
        assert!(t.plugin().strikes.is_empty());
    }

    #[test]
    fn operators_may_take_things_apart() {
        let mut t = room(mock());
        for id in 0..30 {
            t.destroy(id, OP);
        }
        assert!(t.mock().told().is_empty() && t.plugin().strikes.is_empty());
    }

    #[test]
    fn with_grief_action_kick_the_player_goes() {
        let mut t = room(mock().with_setting("grief_action", "kick"));
        for id in 0..4 {
            t.destroy(id, BO);
        }
        assert_eq!(t.mock().kicked(), [(BO, "Removed by the grief guard: 4 objects destroyed within 60 seconds.".to_string())]);
        assert!(told_to(&t, OP)[0].ends_with("Action: kicked."));
    }

    #[test]
    fn strikes_survive_a_reload_and_expire_after_a_day() {
        let mut t = room(mock());
        for id in 0..4 {
            t.destroy(id, ANN);
        }
        t.advance_ms(5000);
        t.reload();
        t.clear();
        t.command(OP, "guard", &[]);
        assert!(told_to(&t, OP).iter().any(|l| l == "Strikes: Ann 1"), "{:?}", t.mock().told());
        // A day and a bit later (the wall clock jumps; the next tick sees it).
        let now = t.mock().unix_ms();
        t.set_unix_ms(now + 25 * 3_600_000);
        t.advance_ms(5000);
        t.clear();
        t.command(OP, "guard", &[]);
        assert!(told_to(&t, OP).iter().any(|l| l == "Strikes: none."), "{:?}", t.mock().told());
    }

    #[test]
    fn a_strike_given_before_the_clock_was_known_starts_its_day_at_the_first_tick() {
        let strike = Strike { player_id: 1, name: "A".into(), strikes: 1, last_unix_ms: 0 };
        let mut g = Guard { strikes: vec![strike], unix_ms: 5_000_000, ..Guard::default() };
        g.expire_strikes();
        assert_eq!(g.strikes.len(), 1);
        assert_eq!(g.strikes[0].last_unix_ms, 5_000_000);
    }

    #[test]
    fn destroys_right_after_a_start_ask_the_world_for_the_clock() {
        // `load` cannot read the world on the real host, so the clocks are 0 until a tick or a destroy needs them.
        let mut t = room(mock());
        t.reload();
        assert_eq!(t.plugin().unix_ms, 0);
        for id in 0..4 {
            t.destroy(id, BO);
        }
        assert_eq!(t.plugin().strikes.len(), 1);
        assert!(t.plugin().strikes[0].last_unix_ms > 0);
    }

    #[test]
    fn guard_lists_the_idle_and_the_settings() {
        let mut t = room(mock().with_setting("afk_minutes", "5"));
        t.advance_ms(30_000);
        t.clear();
        t.command(OP, "guard", &[]);
        let told = told_to(&t, OP);
        assert_eq!(told[0], "Guard: away after 5 min (warning 20 s before); grief: more than 3 destroyed in 60 s (warn).");
        assert_eq!(told[1], "Idle: nobody.", "under a minute is not listed");
        assert_eq!(told[2], "Strikes: none.");
        t.advance_ms(60_000);
        t.clear();
        t.command(OP, "guard", &[]);
        assert_eq!(told_to(&t, OP)[1], "Idle: Ann 1m 30s, Bo 1m 30s, Root 1m 30s (operator)");
    }

    #[test]
    fn pardon_clears_strikes_by_name_online_or_saved() {
        let mut t = room(mock());
        for id in 0..4 {
            t.destroy(id, ANN);
        }
        t.clear();
        t.command(OP, "pardon", &["ann"]);
        assert_eq!(told_to(&t, OP), ["Pardoned Ann: 1 strike cleared."]);
        assert!(t.plugin().strikes.is_empty());
        // A player who left is found by the name saved with the strike.
        for id in 20..24 {
            t.destroy(id, BO);
        }
        t.leave(BO);
        t.clear();
        t.command(OP, "pardon", &["BO"]);
        assert_eq!(told_to(&t, OP), ["Pardoned Bo: 1 strike cleared."]);
        t.clear();
        t.command(OP, "pardon", &["Nobody", "Here"]);
        assert_eq!(told_to(&t, OP), ["No player named Nobody Here is online or has strikes."]);
        // "Ro" begins two names; no pardon is given.
        t.join(30, "Ron");
        t.clear();
        t.command(OP, "pardon", &["ro"]);
        assert!(told_to(&t, OP)[0].starts_with("Several players match: "), "{:?}", t.mock().told());
    }

    #[test]
    fn hostile_input_is_cut_or_ignored() {
        let mut t = room(mock());
        let huge = "x".repeat(100_000);
        t.command(OP, "pardon", &[&huge]);
        t.command(OP, "pardon", &[]);
        t.command(OP, "PARDON", &["\u{0}", "\u{1F680}"]);
        t.command(OP, "guard", &[&huge]);
        assert!(t.mock().told().iter().all(|(_, s)| s.chars().count() <= LINE_CHARS + 20), "every line is short");
        // A flood of destroys from a thousand clients that are not online, and one that is.
        for i in 0..10_000u64 {
            t.destroy(i as i32, 1000 + i % 1000);
        }
        for id in 0..10_000 {
            t.destroy(id, BO);
        }
        assert!(t.plugin().destroys.iter().all(|(_, v)| v.len() <= 4), "the list of a player never grows past the limit + 1");
        assert!(t.plugin().strikes.len() <= MAX_STRIKES);
        // Garbage state does not stop the plugin from starting.
        for bytes in [&[][..], &[255, 255, 255, 255], &[1, 0, 0, 0, 9, 9], &[2, 0, 0, 0]] {
            let t = Harness::<Guard>::start_with_state(MockHost::new(), STATE_VERSION, bytes);
            assert!(t.plugin().strikes.len() <= 2);
        }
    }

    #[test]
    fn a_newer_state_version_is_ignored_with_a_log_line() {
        let t = Harness::<Guard>::start_with_state(MockHost::new(), 9, &[1, 2, 3]);
        assert!(t.plugin().strikes.is_empty());
        assert!(t.mock().logs().iter().any(|(l, s)| *l == Level::Warn && s.contains("state version 9")));
    }

    #[test]
    fn strikes_are_capped() {
        let mut g = Guard { unix_ms: 1000, ..Guard::default() };
        for id in 0..(MAX_STRIKES as u64 + 50) {
            g.unix_ms += 1;
            g.add_strike(id, "P");
        }
        assert_eq!(g.strikes.len(), MAX_STRIKES);
        assert!(g.strikes.iter().all(|s| s.player_id >= 50), "the oldest were dropped");
    }

    #[test]
    fn many_idle_players_hit_the_sixteen_actions_limit_and_are_kicked_over_two_ticks() {
        let mut t = Harness::<Guard>::start(mock().with_limits()).tick_every_ms(5000).tick_step_ms(1000);
        for c in 10..40u64 {
            t.join(c, &format!("P{c}"));
        }
        let mut seen = 0;
        for _ in 0..30 {
            t.advance_ms(5000);
            let kicked = t.mock().kicked();
            assert!(kicked.len() - seen <= 16, "one call queues at most 16 actions");
            // The server removes a kicked player; the mock does not.
            kicked[seen..].iter().for_each(|k| {
                t.leave(k.0);
            });
            seen = kicked.len();
        }
        assert_eq!(seen, 30, "all 30 are gone in the end, over more than one tick");
    }

    #[test]
    fn without_the_capabilities_it_logs_and_does_not_panic() {
        let mut t = room(mock().deny(&["player.kick", "chat.say"]));
        t.advance_ms(120_000);
        for id in 0..10 {
            t.destroy(id, ANN);
        }
        t.command(OP, "guard", &[]);
        assert!(t.mock().kicked().is_empty() && t.mock().told().is_empty());
        let mut t = Harness::<Guard>::start(mock().deny(&["world.read"])).tick_every_ms(5000);
        t.join(ANN, "Ann");
        t.advance_ms(120_000);
        t.destroy(1, ANN);
        t.command(ANN, "guard", &[]);
        assert!(t.mock().kicked().is_empty());
    }
}
