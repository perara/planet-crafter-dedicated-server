//! quest-rewards: small rewards for milestones, each given once and remembered for the player.
//!
//!   Personal milestones (the same reward for each, set by the admin: items and Terra tokens)
//!     the first building, 10, 50 and 200 buildings placed; 1 hour and 10 hours of play
//!   World milestones (the world's terraformation passes a number: the colony gets tokens, once)
//!   !rewards          your progress and the next milestones
//!   !givetokens <n>   (operators) adds n Terra tokens to the world, 1 to 10000
//!
//! What it shows: the action API (`give`, `tokens`) with its limits, and how to keep the books
//! honest: a reward is marked as given only when the host took the call (`Ok`). A call the host
//! refuses for being busy (`Error::Full`: 16 actions in a call, 10 gives and 2 token changes in a
//! second) is simply tried again at the next tick: nothing is queued in state, because every reward
//! is worked out again from the counters (`buildings placed`, `seconds played`) and the two masks of
//! rewards already given. The host may still refuse a `give` when it is applied (the player left,
//! the backpack is full); that cannot be seen from here, and the README says so.

use pcds_plugin_api::*;

const STATE_VERSION: u32 = 1;
/// Players kept in state, and the most dropped at a tick when over (finding one reads them all).
const MAX_PLAYERS: usize = 1000;
const TRIM_PER_TICK: usize = 3;
/// Players online that are looked at.
const MAX_ONLINE: usize = 256;
/// World milestones that can be set, and how many are remembered as paid.
const MAX_WORLD: usize = 8;
const MAX_PAID: usize = 32;
/// One tick credits at most this much game time as play time.
const MAX_STEP_MS: u32 = 60_000;
/// The terraformation is looked at this often (game time).
const WORLD_CHECK_MS: u32 = 30_000;
/// When the host says a capability is not granted, the plugin leaves that action alone this long.
const BLOCK_MS: i64 = 60_000;
const MAX_TOKENS: i32 = 10_000;
const DEFAULT_WORLD_MILESTONES: &str = "10000,100000,1000000,10000000";

/// What counts towards a milestone.
#[derive(Clone, Copy)]
enum Goal {
    /// This many buildings placed.
    Buildings(u32),
    /// This many seconds played.
    Seconds(u32),
}

struct Milestone {
    goal: Goal,
    /// How it is named in chat.
    name: &'static str,
}

/// The order is the bit of the milestone in `Player::items_done` and `tokens_done`.
const MILESTONES: [Milestone; 6] = [
    Milestone { goal: Goal::Buildings(1), name: "your first building" },
    Milestone { goal: Goal::Buildings(10), name: "10 buildings placed" },
    Milestone { goal: Goal::Buildings(50), name: "50 buildings placed" },
    Milestone { goal: Goal::Buildings(200), name: "200 buildings placed" },
    Milestone { goal: Goal::Seconds(3600), name: "1 hour of play" },
    Milestone { goal: Goal::Seconds(36_000), name: "10 hours of play" },
];

impl Milestone {
    fn reached(&self, p: &Player) -> bool {
        match self.goal {
            Goal::Buildings(n) => p.placed >= n,
            Goal::Seconds(s) => p.seconds >= s,
        }
    }
    /// How far from it: "3 more" or "8h 55m more", or `None` when it is reached.
    fn missing(&self, p: &Player) -> Option<String> {
        match self.goal {
            Goal::Buildings(n) => (p.placed < n).then(|| format!("{} more", n - p.placed)),
            Goal::Seconds(s) => (p.seconds < s).then(|| format!("{} more", hours_minutes(s - p.seconds))),
        }
    }
}

/// The admin's settings, read at every tick.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Config {
    item_group: String,
    item_count: u32,
    token_amount: u32,
    world_token_amount: u32,
    /// Ascending, at most `MAX_WORLD`.
    world_milestones: Vec<u64>,
}

impl Default for Config {
    fn default() -> Self {
        Config { item_group: "Iron".into(), item_count: 10, token_amount: 100, world_token_amount: 500, world_milestones: parse_milestones(DEFAULT_WORLD_MILESTONES) }
    }
}

fn parse_milestones(text: &str) -> Vec<u64> {
    let mut v: Vec<u64> = text.split(',').take(MAX_WORLD).filter_map(|t| t.trim().parse::<u64>().ok()).filter(|n| *n > 0).collect();
    v.sort_unstable();
    v.dedup();
    v
}

impl Config {
    fn read(host: &dyn Host) -> Config {
        let list = host.settings().unwrap_or_default();
        let find = |key: &str| list.iter().find(|(k, _)| k == key).map(|(_, v)| v.trim());
        let int = |key: &str, default: u32, least: u32, most: u32| find(key).and_then(|v| v.parse::<u32>().ok()).map_or(default, |v| v.clamp(least, most));
        let d = Config::default();
        let group = find("item_group").filter(|g| !g.is_empty()).map_or(d.item_group.clone(), |g| g.chars().take(60).collect());
        Config {
            item_group: group,
            item_count: int("item_count", d.item_count, 1, 50),
            token_amount: int("token_amount", d.token_amount, 0, MAX_TOKENS as u32),
            world_token_amount: int("world_token_amount", d.world_token_amount, 0, MAX_TOKENS as u32),
            world_milestones: find("world_milestones").map_or(d.world_milestones, parse_milestones),
        }
    }
}

/// What the plugin knows about one player. `items_done` and `tokens_done` have the bit of each
/// milestone (see `MILESTONES`) whose items, or tokens, the host has taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Player {
    id: u64,
    placed: u32,
    seconds: u32,
    items_done: u32,
    tokens_done: u32,
}

impl Player {
    fn new(id: u64) -> Player {
        Player { id, placed: 0, seconds: 0, items_done: 0, tokens_done: 0 }
    }
    fn done(&self, m: usize) -> bool {
        self.items_done & (1 << m) != 0 && self.tokens_done & (1 << m) != 0
    }
}

/// How one `give` or `tokens` went.
enum Try {
    /// The host took it (or there was nothing to give).
    Done,
    /// Not now: try at the next tick.
    Later,
    /// The host is busy: stop asking in this call.
    Busy,
    /// The player is gone: stop for this player.
    Gone,
}

#[derive(Default)]
struct Quests {
    cfg: Config,
    /// Sorted by `id`.
    players: Vec<Player>,
    /// World milestones already paid (the numbers, so a changed list does not pay twice).
    world_paid: Vec<u64>,
    /// Who is online: (client, player_id), kept from join and leave events and rebuilt at every tick,
    /// so a placement does not need a `players()` query (which costs fuel for every player online).
    online: Vec<(u64, u64)>,
    carry_ms: u32,
    world_ms: u32,
    /// Game time of the last tick.
    now_ms: i64,
    /// While `now_ms` is below these, the capability was refused: leave giving / tokens alone.
    give_blocked_until: i64,
    tokens_blocked_until: i64,
}

fn hours_minutes(seconds: u32) -> String {
    let minutes = seconds / 60;
    format!("{}h {:02}m", minutes / 60, minutes % 60)
}

impl Plugin for Quests {
    fn load(host: &dyn Host, state: Option<State<'_>>) -> Self {
        let mut me = Quests { cfg: Config::read(host), ..Quests::default() };
        let Some(state) = state else { return me };
        match state.version {
            1 => me.read_v1(state.bytes),
            v => host.log(Level::Warn, &format!("state version {v} is newer than this plugin knows: starting with no rewards remembered")),
        }
        me
    }

    fn event(&mut self, host: &dyn Host, event: &Event<'_>) -> Verdict {
        match event {
            Event::Tick { game_ms, since_ms, .. } => self.on_tick(host, *game_ms, *since_ms),
            Event::PlayerJoin { client, player_id, .. } => {
                self.online.retain(|(c, id)| c != client && id != player_id);
                if self.online.len() < MAX_ONLINE {
                    self.online.push((*client, *player_id));
                }
            }
            Event::PlayerLeave { client, .. } => self.online.retain(|(c, _)| c != client),
            Event::ObjectPlaced { client, .. } if *client != 0 => self.on_placed(host, *client),
            Event::Command { client, command, args, player_id, .. } => match command.to_ascii_lowercase().as_str() {
                "rewards" => self.cmd_rewards(host, *client, *player_id),
                "givetokens" => self.cmd_givetokens(host, *client, args),
                _ => {}
            },
            _ => {}
        }
        Verdict::Continue
    }

    fn save(&mut self) -> Option<SavedState> {
        let mut w = Writer::new();
        w.0.reserve(self.players.len() * 24 + self.world_paid.len() * 8 + 8);
        w.u32(self.world_paid.len() as u32);
        self.world_paid.iter().for_each(|t| {
            w.u64(*t);
        });
        w.u32(self.players.len() as u32);
        for p in &self.players {
            w.u64(p.id).u32(p.placed).u32(p.seconds).u32(p.items_done).u32(p.tokens_done);
        }
        Some(SavedState { version: STATE_VERSION, bytes: w.0 })
    }
}

impl Quests {
    /// Version 1: a count and that many paid world milestones (u64), then a count and that many
    /// players (id u64, placed, seconds, items_done, tokens_done: u32 each). A damaged tail is dropped.
    fn read_v1(&mut self, bytes: &[u8]) {
        let mut r = Reader::new(bytes);
        for _ in 0..r.u32().unwrap_or(0).min(MAX_PAID as u32) {
            let Ok(t) = r.u64() else { return };
            self.world_paid.push(t);
        }
        for _ in 0..r.u32().unwrap_or(0).min(MAX_PLAYERS as u32) {
            let (Ok(id), Ok(placed), Ok(seconds), Ok(items_done), Ok(tokens_done)) = (r.u64(), r.u32(), r.u32(), r.u32(), r.u32()) else { break };
            self.players.push(Player { id, placed, seconds, items_done, tokens_done });
        }
        self.players.sort_by_key(|p| p.id);
        self.players.dedup_by_key(|p| p.id);
    }

    fn find(&self, id: u64) -> Result<usize, usize> {
        self.players.binary_search_by_key(&id, |p| p.id)
    }

    /// The index of the player, who is added if new.
    fn ensure(&mut self, id: u64) -> usize {
        self.find(id).unwrap_or_else(|at| {
            self.players.insert(at, Player::new(id));
            at
        })
    }

    // ---- Events.

    /// A building was placed by a connection: it counts for the player who is online with it.
    fn on_placed(&mut self, host: &dyn Host, client: u64) {
        let id = match self.online.iter().find(|(c, _)| *c == client) {
            Some((_, id)) => *id,
            // Not known from a join (the plugin started after the player joined, and no tick yet): ask.
            None => match host.players().ok().and_then(|ps| ps.iter().find(|p| p.client == client).map(|p| p.player_id)) {
                Some(id) => id,
                None => return,
            },
        };
        let i = self.ensure(id);
        self.players[i].placed = self.players[i].placed.saturating_add(1);
        self.pay(host, client, id);
    }

    fn on_tick(&mut self, host: &dyn Host, game_ms: i64, since_ms: u32) {
        self.now_ms = game_ms;
        self.cfg = Config::read(host);
        let world = host.world();
        // A paused world: nobody is playing, and there is nothing to wait for.
        if world.as_ref().is_ok_and(|w| w.standing_still) {
            return;
        }
        let step = since_ms.min(MAX_STEP_MS);
        let total = self.carry_ms.saturating_add(step);
        let (seconds, carry) = (total / 1000, total % 1000);
        self.carry_ms = carry;
        if let Ok(online) = host.players() {
            self.online = online.iter().take(MAX_ONLINE).map(|p| (p.client, p.player_id)).collect();
            let mut ids: Vec<u64> = Vec::with_capacity(online.len());
            for p in online.iter().take(MAX_ONLINE) {
                ids.push(p.player_id);
                let i = self.ensure(p.player_id);
                self.players[i].seconds = self.players[i].seconds.saturating_add(seconds);
                self.pay(host, p.client, p.player_id);
            }
            ids.sort_unstable();
            self.trim(&ids);
        }
        self.world_ms = self.world_ms.saturating_add(step);
        if self.world_ms >= WORLD_CHECK_MS
            && let Ok(w) = world
            && !matches!(self.check_world(host, w.terraformation), Try::Busy)
        {
            // Done, or nothing to do: the next look is in 30 s. (Busy: the next tick looks again.)
            self.world_ms = 0;
        }
    }

    /// Drops players with the least progress who are not in `online` (sorted), a few at a tick.
    fn trim(&mut self, online: &[u64]) {
        for _ in 0..TRIM_PER_TICK {
            if self.players.len() <= MAX_PLAYERS {
                return;
            }
            let mut least: Option<(u64, usize)> = None;
            for (i, p) in self.players.iter().enumerate() {
                let progress = u64::from(p.seconds) + 600 * u64::from(p.placed);
                if least.is_some_and(|(l, _)| progress >= l) || online.binary_search(&p.id).is_ok() {
                    continue;
                }
                least = Some((progress, i));
            }
            match least {
                Some((_, i)) => {
                    self.players.remove(i);
                }
                None => return,
            }
        }
    }

    // ---- Paying.

    /// Gives this player what they have earned and not yet been given, as far as the host lets.
    fn pay(&mut self, host: &dyn Host, client: u64, id: u64) {
        let Ok(i) = self.find(id) else { return };
        let mut p = self.players[i];
        for (m, milestone) in MILESTONES.iter().enumerate() {
            if !milestone.reached(&p) || p.done(m) {
                continue;
            }
            let bit = 1 << m;
            let mut stop = false;
            if p.items_done & bit == 0 {
                match self.give_items(host, client) {
                    Try::Done => p.items_done |= bit,
                    Try::Later => {}
                    Try::Busy | Try::Gone => stop = true,
                }
            }
            if !stop && p.tokens_done & bit == 0 {
                match self.add_tokens(host, self.cfg.token_amount) {
                    Try::Done => p.tokens_done |= bit,
                    Try::Later => {}
                    Try::Busy | Try::Gone => stop = true,
                }
            }
            if p.done(m) {
                let _ = host.tell(client, &format!("Quest reward for {}: {}.", milestone.name, self.reward_text()));
            }
            if stop {
                break;
            }
        }
        self.players[i] = p;
    }

    /// "+10 Iron, +100 Terra tokens".
    fn reward_text(&self) -> String {
        let mut parts = Vec::new();
        if self.cfg.item_count > 0 {
            parts.push(format!("+{} {}", self.cfg.item_count, self.cfg.item_group));
        }
        if self.cfg.token_amount > 0 {
            parts.push(format!("+{} Terra tokens", self.cfg.token_amount));
        }
        parts.join(", ")
    }

    fn give_items(&mut self, host: &dyn Host, client: u64) -> Try {
        if self.cfg.item_count == 0 {
            return Try::Done;
        }
        if self.now_ms < self.give_blocked_until {
            return Try::Later;
        }
        // A full backpack would refuse the items when they are applied, after we have counted them as given.
        match host.inventory(client) {
            Ok(inv) if inv.slots_total > 0 && inv.slots_used >= inv.slots_total => return Try::Later,
            Err(Error::NotFound) => return Try::Gone,
            _ => {}
        }
        match host.give(client, &self.cfg.item_group, self.cfg.item_count) {
            Ok(()) => Try::Done,
            Err(Error::Full) => Try::Busy,
            Err(e @ (Error::Denied | Error::Unsupported)) => {
                host.log(Level::Warn, &format!("cannot give items ({e:?}): the capability player.give must be granted; trying again in a minute"));
                self.give_blocked_until = self.now_ms + BLOCK_MS;
                Try::Later
            }
            Err(e) => {
                // The host will never take this (a bad item name): giving it up is better than asking at every tick.
                host.log(Level::Warn, &format!("items for item_group {:?} were refused ({e:?}): check the setting; the reward is counted as given", self.cfg.item_group));
                Try::Done
            }
        }
    }

    fn add_tokens(&mut self, host: &dyn Host, amount: u32) -> Try {
        if amount == 0 {
            return Try::Done;
        }
        if self.now_ms < self.tokens_blocked_until {
            return Try::Later;
        }
        match host.tokens(amount.min(MAX_TOKENS as u32) as i32) {
            Ok(()) => Try::Done,
            Err(Error::Full) => Try::Busy,
            Err(e @ (Error::Denied | Error::Unsupported)) => {
                host.log(Level::Warn, &format!("cannot add tokens ({e:?}): the capability world.tokens must be granted; trying again in a minute"));
                self.tokens_blocked_until = self.now_ms + BLOCK_MS;
                Try::Later
            }
            Err(e) => {
                host.log(Level::Warn, &format!("tokens were refused ({e:?}): the reward is counted as given"));
                Try::Done
            }
        }
    }

    /// Pays the world milestones the terraformation has passed and nobody has been paid for.
    fn check_world(&mut self, host: &dyn Host, terraformation: f64) -> Try {
        let due: Vec<u64> = self.cfg.world_milestones.iter().copied().filter(|t| terraformation >= *t as f64 && !self.world_paid.contains(t)).collect();
        for t in due {
            match self.add_tokens(host, self.cfg.world_token_amount) {
                Try::Done => {}
                other => return other,
            }
            if self.world_paid.len() >= MAX_PAID {
                self.world_paid.remove(0);
            }
            self.world_paid.push(t);
            let tokens = if self.cfg.world_token_amount > 0 { format!(" The colony receives {} Terra tokens.", self.cfg.world_token_amount) } else { String::new() };
            let _ = host.say(&format!("World milestone: terraformation passed {t}!{tokens}"));
        }
        Try::Done
    }

    // ---- Commands.

    fn cmd_rewards(&mut self, host: &dyn Host, client: u64, id: u64) {
        let p = self.find(id).map_or(Player::new(id), |i| self.players[i]);
        let done = (0..MILESTONES.len()).filter(|m| p.done(*m)).count();
        let waiting = (0..MILESTONES.len()).filter(|m| MILESTONES[*m].reached(&p) && !p.done(*m)).count();
        let wait = match waiting {
            0 => String::new(),
            1 => " 1 reward is waiting; it is given when the backpack has room.".to_string(),
            n => format!(" {n} rewards are waiting; they are given when the backpack has room."),
        };
        let _ = host.tell(client, &format!("You have placed {} buildings and played {}. Milestones done: {done} of {}.{wait}", p.placed, hours_minutes(p.seconds), MILESTONES.len()));
        // The next building milestone and the next time milestone that are not reached yet.
        let mut next = Vec::new();
        let (mut building, mut time) = (false, false);
        for m in &MILESTONES {
            let Some(left) = m.missing(&p) else { continue };
            let seen = match m.goal {
                Goal::Buildings(_) => &mut building,
                Goal::Seconds(_) => &mut time,
            };
            if !*seen {
                *seen = true;
                next.push(format!("{} ({left})", m.name));
            }
        }
        let line = if next.is_empty() { "All milestones are done.".to_string() } else { format!("Next: {}. Each gives {}.", next.join(", "), self.reward_text()) };
        let _ = host.tell(client, &line);
        let terra = host.world().map_or(0, |w| w.terraformation as u64);
        let upcoming = self.cfg.world_milestones.iter().find(|t| !self.world_paid.contains(t));
        let line = match upcoming {
            Some(t) => format!("World: terraformation {terra}; the next milestone is {t}."),
            None => format!("World: terraformation {terra}; every milestone is reached."),
        };
        let _ = host.tell(client, &line);
    }

    fn cmd_givetokens(&mut self, host: &dyn Host, client: u64, args: &[&str]) {
        let amount = args.first().and_then(|a| a.parse::<i32>().ok()).filter(|n| (1..=MAX_TOKENS).contains(n));
        let Some(amount) = amount else {
            let _ = host.tell(client, "Use: !givetokens <number from 1 to 10000>");
            return;
        };
        let text = match host.tokens(amount) {
            Ok(()) => {
                host.log(Level::Info, &format!("an operator added {amount} Terra tokens"));
                format!("Added {amount} Terra tokens.")
            }
            Err(Error::Full) => "Token changes are limited to 2 a second: try again in a moment.".to_string(),
            Err(Error::Denied) => "This plugin may not change the tokens (world.tokens is not granted).".to_string(),
            Err(e) => format!("The tokens could not be added: {e:?}."),
        };
        let _ = host.tell(client, &text);
    }
}

export!(Quests);

#[cfg(test)]
mod tests {
    use super::*;
    use pcds_plugin_api::testing::{Harness, MockHost};

    const OP: u64 = 1;
    const ANN: u64 = 2;

    fn roomy() -> InventoryInfo {
        InventoryInfo { slots_used: 3, slots_total: 30, items: vec![] }
    }

    /// A harness with a backpack for everyone who joins: the mock answers `NotFound` for a player with none.
    struct Room {
        t: Harness<Quests>,
    }

    fn room(mock: MockHost) -> Room {
        let mut t = Harness::<Quests>::start(mock).tick_every_ms(5000).tick_step_ms(1000);
        t.join(OP, "Root");
        t.join(ANN, "Ann");
        let mut root = t.mock().players().unwrap().into_iter().find(|p| p.client == OP).unwrap();
        root.operator = true;
        t.mock().add_player(root);
        t.mock().set_inventory(OP, roomy());
        t.mock().set_inventory(ANN, roomy());
        Room { t }
    }

    fn told(t: &Harness<Quests>, client: u64) -> Vec<String> {
        t.mock().told().into_iter().filter(|(c, _)| *c == client).map(|(_, s)| s).collect()
    }

    fn place(t: &mut Harness<Quests>, from: i32, n: i32, client: u64) {
        for id in from..from + n {
            t.place(id, "Wall", [0.0; 3], client);
        }
    }

    #[test]
    fn the_first_building_pays_items_and_tokens_once() {
        let Room { mut t } = room(MockHost::new());
        place(&mut t, 1, 1, ANN);
        assert_eq!(t.mock().given(), [(ANN, "Iron".to_string(), 10)]);
        assert_eq!(t.mock().tokens_total(), 100);
        assert_eq!(told(&t, ANN), ["Quest reward for your first building: +10 Iron, +100 Terra tokens."]);
        // The second building is not a milestone; neither is placing it again after a reload.
        place(&mut t, 2, 1, ANN);
        t.reload();
        place(&mut t, 3, 1, ANN);
        assert_eq!(t.mock().given().len(), 1);
        assert_eq!(t.mock().tokens_total(), 100);
    }

    #[test]
    fn a_connection_number_that_is_used_again_belongs_to_the_new_player() {
        let Room { mut t } = room(MockHost::new());
        t.leave(ANN);
        t.join_as(ANN, "Bo", 5555, "");
        t.mock().set_inventory(ANN, roomy());
        place(&mut t, 1, 1, ANN);
        assert_eq!(t.plugin().players.iter().map(|p| p.id).collect::<Vec<_>>(), [5555]);
        // After a reload nobody is known from a join: the host is asked once.
        t.reload();
        assert!(t.plugin().online.is_empty());
        place(&mut t, 2, 1, ANN);
        assert_eq!(t.plugin().players[0].placed, 2);
    }

    #[test]
    fn ten_fifty_and_two_hundred_buildings_each_pay_once() {
        let Room { mut t } = room(MockHost::new());
        place(&mut t, 1, 9, ANN);
        assert_eq!(t.mock().given().len(), 1, "only the first so far");
        place(&mut t, 10, 1, ANN);
        assert_eq!(t.mock().given().len(), 2);
        place(&mut t, 11, 190, ANN);
        assert_eq!(t.mock().given().len(), 4, "first, 10, 50, 200");
        place(&mut t, 201, 50, ANN);
        assert_eq!(t.mock().given().len(), 4, "nothing more for buildings");
        assert_eq!(t.mock().tokens_total(), 400);
        let rewards: Vec<String> = told(&t, ANN);
        assert!(rewards[3].starts_with("Quest reward for 200 buildings placed"), "{rewards:?}");
    }

    #[test]
    fn only_buildings_of_players_who_are_online_count() {
        let Room { mut t } = room(MockHost::new());
        place(&mut t, 1, 5, 0);
        place(&mut t, 10, 5, 77);
        assert!(t.mock().given().is_empty() && t.plugin().players.is_empty());
        // A player who left: the placement arrives with a connection that is gone.
        t.leave(ANN);
        place(&mut t, 20, 5, ANN);
        assert!(t.mock().given().is_empty());
    }

    #[test]
    fn play_time_pays_after_one_hour_and_after_ten() {
        let Room { mut t } = room(MockHost::new());
        t.advance_ms(3_590_000);
        assert!(t.mock().given().is_empty());
        t.advance_ms(20_000);
        assert_eq!(t.mock().given().len(), 2, "Ann and the operator each");
        assert!(told(&t, ANN)[0].starts_with("Quest reward for 1 hour of play"));
        t.advance_ms(9 * 3_600_000);
        assert_eq!(t.mock().given().len(), 4);
        assert_eq!(t.mock().tokens_total(), 400);
        t.advance_ms(3_600_000);
        assert_eq!(t.mock().given().len(), 4, "once each");
    }

    #[test]
    fn progress_and_the_rewards_given_survive_a_reload() {
        let Room { mut t } = room(MockHost::new());
        place(&mut t, 1, 9, ANN);
        t.advance_ms(1_800_000);
        t.reload();
        t.clear();
        place(&mut t, 100, 1, ANN);
        assert_eq!(told(&t, ANN).len(), 1, "the tenth building, not the first again");
        t.advance_ms(1_800_000);
        assert_eq!(told(&t, ANN).len(), 2, "the hour");
        let saved = t.save().unwrap();
        assert_eq!(saved.version, 1);
    }

    #[test]
    fn a_full_backpack_waits_and_the_reward_comes_when_there_is_room() {
        let Room { mut t } = room(MockHost::new());
        t.mock().set_inventory(ANN, InventoryInfo { slots_used: 30, slots_total: 30, items: vec![] });
        place(&mut t, 1, 1, ANN);
        assert!(t.mock().given().is_empty());
        // Tokens are not tied to the backpack: they were given, the items were not, and the player is not told yet.
        assert_eq!(t.mock().tokens_total(), 100);
        t.command(ANN, "rewards", &[]);
        assert!(told(&t, ANN)[0].contains("1 reward is waiting"), "{:?}", told(&t, ANN));
        t.advance_ms(10_000);
        assert!(t.mock().given().is_empty());
        t.mock().set_inventory(ANN, roomy());
        t.advance_ms(5000);
        assert_eq!(t.mock().given(), [(ANN, "Iron".to_string(), 10)]);
        assert_eq!(t.mock().tokens_total(), 100, "the tokens are not given twice");
        assert_eq!(told(&t, ANN).iter().filter(|s| s.starts_with("Quest reward")).count(), 1);
    }

    #[test]
    fn a_busy_host_is_asked_again_at_the_next_tick_and_nothing_is_paid_twice() {
        // 8 players earn the hour at the same tick: 3 actions each is 24, the call takes 16.
        let mut t = Harness::<Quests>::start(MockHost::new().with_limits()).tick_every_ms(5000).tick_step_ms(1000);
        for c in 10..18u64 {
            t.join(c, &format!("P{c}"));
            t.mock().set_inventory(c, roomy());
        }
        t.advance_ms(3_595_000);
        assert!(t.mock().given().is_empty());
        t.advance_ms(5000);
        let first = t.mock().given().len();
        assert!(first < 8, "the limit stopped some: {first}");
        for _ in 0..5 {
            t.advance_ms(5000);
        }
        let given = t.mock().given();
        assert_eq!(given.len(), 8, "every player got the items");
        assert_eq!(t.mock().tokens_total(), 800, "and the tokens, once");
        let mut clients: Vec<u64> = given.iter().map(|g| g.0).collect();
        clients.sort_unstable();
        clients.dedup();
        assert_eq!(clients.len(), 8, "once each");
        let tells = t.mock().told().iter().filter(|(_, s)| s.starts_with("Quest reward")).count();
        assert_eq!(tells, 8, "each told once, when both parts were given");
    }

    #[test]
    fn world_milestones_pay_the_colony_once_and_are_remembered() {
        let low = WorldInfo { terraformation: 500.0, ..WorldInfo::default() };
        let mut t = Harness::<Quests>::start(MockHost::new().with_world(low).with_setting("world_milestones", "1000, 2000,junk,0")).tick_every_ms(5000).tick_step_ms(1000);
        t.join(ANN, "Ann");
        t.advance_ms(120_000);
        assert_eq!(t.mock().tokens_total(), 0);
        let saved = t.save().unwrap();
        // The world has grown (a new mock stands in for it, with the saved state): 1000 is passed, 2000 is not.
        let high = WorldInfo { terraformation: 1500.0, ..WorldInfo::default() };
        let mock = MockHost::new().with_world(high).with_setting("world_milestones", "1000,2000");
        let mut t = Harness::<Quests>::start_with_state(mock, saved.version, &saved.bytes).tick_every_ms(5000).tick_step_ms(1000);
        t.join(ANN, "Ann");
        t.advance_ms(29_000);
        assert_eq!(t.mock().tokens_total(), 0, "looked at every 30 s of game time");
        t.advance_ms(2000);
        assert_eq!(t.mock().tokens_total(), 500);
        assert_eq!(t.mock().said(), ["World milestone: terraformation passed 1000! The colony receives 500 Terra tokens."]);
        t.advance_ms(300_000);
        t.reload();
        t.advance_ms(300_000);
        assert_eq!(t.mock().tokens_total(), 500, "once");
        assert_eq!(t.plugin().world_paid, [1000]);
    }

    #[test]
    fn a_new_milestone_in_the_setting_pays_but_one_already_paid_does_not_come_back() {
        let world = WorldInfo { terraformation: 5000.0, ..WorldInfo::default() };
        let mut t = Harness::<Quests>::start(MockHost::new().with_world(world).with_setting("world_milestones", "1000")).tick_every_ms(5000).tick_step_ms(1000);
        t.join(ANN, "Ann");
        t.advance_ms(35_000);
        assert_eq!(t.mock().tokens_total(), 500);
        t.mock().set_setting("world_milestones", "1000,3000,9000");
        t.advance_ms(35_000);
        assert_eq!(t.mock().tokens_total(), 1000, "3000 is new and passed; 1000 was paid; 9000 is not reached");
    }

    #[test]
    fn givetokens_adds_tokens_for_operators_and_refuses_nonsense() {
        let Room { mut t } = room(MockHost::new());
        t.command(OP, "givetokens", &["250"]);
        assert_eq!(t.mock().tokens_total(), 250);
        assert_eq!(told(&t, OP), ["Added 250 Terra tokens."]);
        t.clear();
        for bad in ["0", "-5", "10001", "abc", "", "99999999999999999999", "1.5"] {
            t.command(OP, "givetokens", &[bad]);
        }
        t.command(OP, "givetokens", &[]);
        assert_eq!(t.mock().tokens_total(), 0);
        assert!(told(&t, OP).iter().all(|s| s == "Use: !givetokens <number from 1 to 10000>"));
        assert_eq!(told(&t, OP).len(), 8);
        t.command(OP, "givetokens", &["10000"]);
        assert_eq!(t.mock().tokens_total(), 10_000);
        let denied = room(MockHost::new().deny(&["world.tokens"]));
        let mut t = denied.t;
        t.command(OP, "givetokens", &["5"]);
        assert_eq!(told(&t, OP), ["This plugin may not change the tokens (world.tokens is not granted)."]);
    }

    #[test]
    fn rewards_shows_progress_and_the_next_milestones() {
        let world = WorldInfo { terraformation: 4200.0, ..WorldInfo::default() };
        let Room { mut t } = room(MockHost::new().with_world(world));
        place(&mut t, 1, 7, ANN);
        t.advance_ms(65 * 60_000);
        t.clear();
        t.command(ANN, "rewards", &[]);
        assert_eq!(
            told(&t, ANN),
            [
                "You have placed 7 buildings and played 1h 05m. Milestones done: 2 of 6.",
                "Next: 10 buildings placed (3 more), 10 hours of play (8h 55m more). Each gives +10 Iron, +100 Terra tokens.",
                "World: terraformation 4200; the next milestone is 10000.",
            ]
        );
        place(&mut t, 100, 200, ANN);
        t.advance_ms(9 * 3_600_000);
        t.clear();
        t.command(ANN, "rewards", &[]);
        assert_eq!(told(&t, ANN)[1], "All milestones are done.");
    }

    #[test]
    fn settings_change_the_reward() {
        let mock = MockHost::new().with_setting("item_group", "Cobalt").with_setting("item_count", "50").with_setting("token_amount", "0");
        let Room { mut t } = room(mock);
        place(&mut t, 1, 1, ANN);
        assert_eq!(t.mock().given(), [(ANN, "Cobalt".to_string(), 50)]);
        assert_eq!(t.mock().tokens_total(), 0);
        assert_eq!(told(&t, ANN), ["Quest reward for your first building: +50 Cobalt."]);
        // Bad values fall back to the defaults or the nearest good value.
        let mock = MockHost::new().with_setting("item_count", "9999").with_setting("item_group", "  ").with_setting("token_amount", "-3");
        let Room { mut t } = room(mock);
        place(&mut t, 1, 1, ANN);
        assert_eq!(t.mock().given(), [(ANN, "Iron".to_string(), 50)]);
        assert_eq!(t.mock().tokens_total(), 100);
    }

    #[test]
    fn without_the_capabilities_it_logs_once_and_tries_again_later() {
        let Room { mut t } = room(MockHost::new().deny(&["player.give", "world.tokens"]));
        place(&mut t, 1, 3, ANN);
        t.advance_ms(30_000);
        assert!(t.mock().given().is_empty());
        let warns = t.mock().logs().iter().filter(|(l, s)| *l == Level::Warn && s.contains("must be granted")).count();
        assert_eq!(warns, 2, "one line for items and one for tokens, not one for every try");
        assert!(told(&t, ANN).is_empty(), "no reward text for a reward not given");
        // Denied chat and world reading do not panic either.
        let mut t = Harness::<Quests>::start(MockHost::new().deny(&["chat.say", "world.read"])).tick_every_ms(5000).tick_step_ms(1000);
        t.join(ANN, "Ann");
        place(&mut t, 1, 3, ANN);
        t.advance_ms(60_000);
        t.command(ANN, "rewards", &[]);
        t.command(OP, "givetokens", &["5"]);
        // Placements are attributed from the join event, so they still count; the backpack cannot be
        // looked at, so the items are given anyway; nothing can be said.
        assert_eq!(t.mock().given().len(), 1);
        assert!(t.mock().told().is_empty());
    }

    #[test]
    fn hostile_input_and_floods_stay_bounded() {
        let Room { mut t } = room(MockHost::new().with_setting("item_group", &"x".repeat(5000)));
        // A flood of placements by one player, and by clients that are not there.
        place(&mut t, 1, 3000, ANN);
        place(&mut t, 5000, 3000, 999);
        assert_eq!(t.mock().given().len(), 4);
        assert!(t.mock().given().iter().all(|g| g.1.chars().count() <= 60));
        // More players than the table holds, each for a moment.
        for i in 0..1200u64 {
            t.join_as(100 + i, "Visitor", 50_000 + i, "");
            t.mock().set_inventory(100 + i, roomy());
            t.place(10_000 + i as i32, "Wall", [0.0; 3], 100 + i);
            t.leave(100 + i);
            t.advance_ms(1000);
        }
        for _ in 0..60 {
            t.advance_ms(5000);
        }
        assert!(t.plugin().players.len() <= MAX_PLAYERS, "{}", t.plugin().players.len());
        t.command(ANN, "rewards", &["extra", "words"]);
        t.command(ANN, "REWARDS", &[&"z".repeat(100_000)]);
        for bytes in [&[][..], &[255, 255, 255, 255], &[1, 0, 0, 0, 9, 9], &[0, 0, 0, 0, 255, 255, 255, 255], &[2, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0]] {
            let t = Harness::<Quests>::start_with_state(MockHost::new(), STATE_VERSION, bytes);
            assert!(t.plugin().players.len() <= 1 && t.plugin().world_paid.len() <= 2);
        }
    }

    #[test]
    fn a_newer_state_version_is_ignored_with_a_log_line() {
        let t = Harness::<Quests>::start_with_state(MockHost::new(), 7, &[1, 2, 3]);
        assert!(t.plugin().players.is_empty());
        assert!(t.mock().logs().iter().any(|(l, s)| *l == Level::Warn && s.contains("state version 7")));
    }

    #[test]
    fn a_paused_world_gives_no_play_time() {
        let still = WorldInfo { standing_still: true, ..WorldInfo::default() };
        let Room { mut t } = room(MockHost::new().with_world(still));
        t.advance_ms(2 * 3_600_000);
        assert!(t.mock().given().is_empty());
    }
}
