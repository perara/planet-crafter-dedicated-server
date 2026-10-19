//! What a running plugin can reach: the capabilities granted to it, a read-only view of the world
//! for the length of one call, and the queue its actions go to. Every host function of the WASM
//! runtime and every step of a data plugin goes through here, so a capability is checked in one
//! place and a rule about an action holds for both kinds of plugin.
//!
//! Actions are never applied while the plugin runs: they are queued, and the host applies them
//! after the call through the same validated paths the admin API uses (`actions.rs`). A plugin
//! cannot change the world in any other way.

use std::time::Instant;

use pcds_plugin_api::host::{E_BAD, E_DENIED, E_FULL, E_UNSUPPORTED, OK};
use pcds_plugin_api::query::{self, PlayerInfo, WorldInfo};
use pcds_plugin_api::{Level, Writer, caps};
use serde_json::Value;

use super::limits;

/// A set of capabilities, one bit for each of `caps::ALL`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Grant(u32);

impl Grant {
    pub fn from_names<'a>(names: impl IntoIterator<Item = &'a str>) -> Grant {
        Grant(names.into_iter().filter_map(|n| caps::ALL.iter().position(|(c, _)| *c == n)).fold(0, |m, i| m | 1 << i))
    }
    pub fn has(self, cap: &str) -> bool {
        caps::ALL.iter().position(|(c, _)| *c == cap).is_some_and(|i| self.0 & 1 << i != 0)
    }
    pub fn names(self) -> Vec<&'static str> {
        caps::ALL.iter().enumerate().filter(|(i, _)| self.0 & 1 << i != 0).map(|(_, (n, _))| *n).collect()
    }
}

/// What a plugin may read of the world (the game implements it in `view.rs`).
pub trait World {
    fn info(&self) -> WorldInfo;
    fn players(&self) -> Vec<PlayerInfo>;
    /// The rules that differ from the defaults, as JSON text.
    fn rules(&self) -> String;
}

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Say(String),
    Tell(u64, String),
    Kick(u64, String),
    Teleport(u64, [f32; 3]),
    RulesPatch(Value),
}

/// A pointer to the world for one call. The host makes it from the game it holds exclusively and
/// does not touch the game until the plugin has returned, so the plugin's reads see a still world.
#[derive(Clone, Copy)]
struct WorldRef(*const dyn World);

// SAFETY: the pointer is set by `begin` and dropped by `end` inside one call of the host on the
// game's thread; between calls (when a snapshot's threads share or move the game, handoff.rs) it
// is None, and nothing reads it from another thread.
unsafe impl Send for WorldRef {}
unsafe impl Sync for WorldRef {}

pub struct Ctx {
    pub id: String,
    pub grant: Grant,
    world: Option<WorldRef>,
    pub actions: Vec<Action>,
    /// Calls refused for a missing capability since the host last took the count.
    pub denied: u32,
    /// Actions refused (too many, or against a rule) since the host last took the count.
    pub refused: u32,
    second: (Instant, u32),
    /// Rule changes write a file: a few a second at most, whatever the actions' own limit.
    rules_second: (Instant, u32),
    log_second: (Instant, u32),
    pub log_dropped: u64,
}

impl Ctx {
    pub fn new(id: &str, grant: Grant) -> Ctx {
        let now = Instant::now();
        Ctx { id: id.to_string(), grant, world: None, actions: Vec::new(), denied: 0, refused: 0, second: (now, 0), rules_second: (now, 0), log_second: (now, 0), log_dropped: 0 }
    }

    /// Starts a call: the plugin may read `world` until `end`.
    pub fn begin(&mut self, world: &dyn World) {
        // SAFETY (of the later reads): the pointer is used only between begin and end, while the
        // host holds the game and runs nothing else. The lifetime is erased here and nowhere else.
        let ptr: *const (dyn World + '_) = world;
        self.world = Some(WorldRef(unsafe { std::mem::transmute::<*const (dyn World + '_), *const (dyn World + 'static)>(ptr) }));
        self.actions.clear();
    }

    /// Ends a call: the plugin reads no more, and the queued actions go to the host.
    pub fn end(&mut self) -> Vec<Action> {
        self.world = None;
        std::mem::take(&mut self.actions)
    }

    fn world(&self) -> Option<&dyn World> {
        // SAFETY: see `begin`: set only while the host's call is in progress.
        self.world.map(|w| unsafe { &*w.0 })
    }

    fn allowed(&mut self, cap: &str) -> bool {
        let ok = self.grant.has(cap);
        if !ok {
            self.denied += 1;
        }
        ok
    }

    fn queue(&mut self, action: Action) -> i32 {
        if self.second.0.elapsed().as_secs() >= 1 {
            self.second = (Instant::now(), 0);
        }
        if self.actions.len() >= limits::ACTIONS_PER_CALL || self.second.1 >= limits::ACTIONS_PER_SECOND {
            self.refused += 1;
            return E_FULL;
        }
        self.second.1 += 1;
        self.actions.push(action);
        OK
    }

    /// `log`: one line in the server's log, prefixed with the plugin's id. Control characters
    /// become spaces (a plugin cannot forge log lines), the rate and the length are bounded.
    pub fn log(&mut self, level: Level, message: &str) {
        if self.log_second.0.elapsed().as_secs() >= 1 {
            if self.log_dropped > 0 {
                eprintln!("plugin {}: {} log lines dropped (over {} a second)", self.id, self.log_dropped, limits::LOG_LINES_PER_SECOND);
                self.log_dropped = 0;
            }
            self.log_second = (Instant::now(), 0);
        }
        if self.log_second.1 >= limits::LOG_LINES_PER_SECOND {
            self.log_dropped += 1;
            return;
        }
        self.log_second.1 += 1;
        let line: String = message.chars().take(limits::LOG_LINE_CHARS).map(|c| if c.is_control() { ' ' } else { c }).collect();
        match level {
            Level::Error | Level::Warn => eprintln!("plugin {}: {line}", self.id),
            Level::Info => println!("plugin {}: {line}", self.id),
            // Debug lines are for the plugin's author, who reads them with the log at hand.
            Level::Debug => println!("plugin {} (debug): {line}", self.id),
        }
    }

    /// `query`: the answer's bytes, or the error code.
    pub fn query(&mut self, kind: u32) -> Result<Vec<u8>, i32> {
        if !self.allowed(caps::WORLD_READ) {
            return Err(E_DENIED);
        }
        let Some(world) = self.world() else { return Err(E_UNSUPPORTED) };
        let mut w = Writer::new();
        match kind {
            query::Q_WORLD => world.info().encode(&mut w),
            query::Q_PLAYERS => query::encode_players(&world.players(), &mut w),
            query::Q_RULES => {
                w.0.extend_from_slice(world.rules().as_bytes());
            }
            _ => return Err(E_UNSUPPORTED),
        }
        Ok(w.0)
    }

    /// The world's facts for a data plugin.
    pub fn info(&mut self) -> Option<WorldInfo> {
        if !self.allowed(caps::WORLD_READ) {
            return None;
        }
        self.world().map(|w| w.info())
    }

    pub fn say(&mut self, text: &str) -> i32 {
        if !self.allowed(caps::CHAT_SAY) {
            return E_DENIED;
        }
        let text = line(text);
        if text.is_empty() { E_BAD } else { self.queue(Action::Say(text)) }
    }

    pub fn tell(&mut self, client: u64, text: &str) -> i32 {
        if !self.allowed(caps::CHAT_SAY) {
            return E_DENIED;
        }
        let text = line(text);
        if text.is_empty() { E_BAD } else { self.queue(Action::Tell(client, text)) }
    }

    pub fn kick(&mut self, client: u64, reason: &str) -> i32 {
        if !self.allowed(caps::PLAYER_KICK) {
            return E_DENIED;
        }
        let reason: String = line(reason).chars().take(128).collect();
        self.queue(Action::Kick(client, if reason.is_empty() { "Kicked".into() } else { reason }))
    }

    pub fn teleport(&mut self, client: u64, position: [f32; 3]) -> i32 {
        if !self.allowed(caps::PLAYER_TELEPORT) {
            return E_DENIED;
        }
        // Finite and within what the game's planets hold (they are a few kilometres across).
        if !position.iter().all(|v| v.is_finite() && v.abs() <= 100_000.0) {
            return E_BAD;
        }
        self.queue(Action::Teleport(client, position))
    }

    pub fn rules_patch(&mut self, json: &str) -> i32 {
        if !self.allowed(caps::RULES_WRITE) {
            return E_DENIED;
        }
        let Ok(patch) = serde_json::from_str::<Value>(json) else { return E_BAD };
        if !patch.is_object() {
            return E_BAD;
        }
        if let Err(why) = super::actions::rules_patch_allowed(&patch) {
            eprintln!("plugin {}: rules patch refused: {why}", self.id);
            self.refused += 1;
            return E_DENIED;
        }
        if self.rules_second.0.elapsed().as_secs() >= 1 {
            self.rules_second = (Instant::now(), 0);
        }
        if self.rules_second.1 >= limits::RULE_CHANGES_PER_SECOND {
            self.refused += 1;
            return E_FULL;
        }
        let code = self.queue(Action::RulesPatch(patch));
        if code == OK {
            self.rules_second.1 += 1;
        }
        code
    }
}

/// One chat line: control characters out, cut to the chat's own limit.
fn line(text: &str) -> String {
    text.chars().filter(|c| !c.is_control() || *c == '\n').take(limits::SAY_CHARS).collect::<String>().trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Still;
    impl World for Still {
        fn info(&self) -> WorldInfo {
            WorldInfo { planet: "Prime".into(), players: 2, ..Default::default() }
        }
        fn players(&self) -> Vec<PlayerInfo> {
            vec![PlayerInfo { client: 3, name: "Ann".into(), ..Default::default() }]
        }
        fn rules(&self) -> String {
            "{}".into()
        }
    }

    #[test]
    fn capabilities_gate_every_call_and_actions_wait_in_the_queue() {
        let mut c = Ctx::new("t", Grant::from_names(["chat.say", "world.read"]));
        c.begin(&Still);
        assert_eq!(c.say("hello"), OK);
        assert_eq!(c.tell(3, "psst"), OK);
        assert_eq!(c.kick(3, "bye"), E_DENIED);
        assert_eq!(c.teleport(3, [0.0; 3]), E_DENIED);
        assert_eq!(c.rules_patch("{}"), E_DENIED);
        assert_eq!(c.denied, 3);
        assert_eq!(WorldInfo::decode(&c.query(query::Q_WORLD).unwrap()).unwrap().planet, "Prime");
        assert_eq!(query::decode_players(&c.query(query::Q_PLAYERS).unwrap()).unwrap()[0].name, "Ann");
        assert_eq!(c.query(99), Err(E_UNSUPPORTED));
        assert_eq!(c.end(), vec![Action::Say("hello".into()), Action::Tell(3, "psst".into())]);
        // After the call there is no world to read.
        assert_eq!(c.query(query::Q_WORLD), Err(E_UNSUPPORTED));
        let mut none = Ctx::new("t", Grant::default());
        none.begin(&Still);
        assert_eq!(none.query(query::Q_WORLD), Err(E_DENIED));
    }

    #[test]
    fn the_queue_and_the_log_are_bounded_and_text_is_cleaned() {
        let mut c = Ctx::new("t", Grant::from_names(["chat.say", "player.teleport", "player.kick"]));
        c.begin(&Still);
        for _ in 0..limits::ACTIONS_PER_CALL {
            assert_eq!(c.say("x"), OK);
        }
        assert_eq!(c.say("one too many"), E_FULL);
        assert_eq!(c.refused, 1);
        c.end();
        c.begin(&Still);
        assert_eq!(c.say("\u{7}\u{8}  "), E_BAD, "text that is only control characters is refused");
        assert_eq!(c.teleport(1, [f32::NAN, 0.0, 0.0]), E_BAD);
        assert_eq!(c.teleport(1, [1e9, 0.0, 0.0]), E_BAD);
        assert_eq!(c.kick(1, &"r".repeat(500)), OK);
        let Action::Kick(_, reason) = c.end().pop().unwrap() else { panic!() };
        assert_eq!(reason.chars().count(), 128);
        assert_eq!(line(&"é".repeat(900)).chars().count(), limits::SAY_CHARS);
        // A rule change writes the rules file: two a second.
        let mut c = Ctx::new("t", Grant::from_names(["rules.write"]));
        c.begin(&Still);
        let results: Vec<i32> = (0..4).map(|_| c.rules_patch("{\"speed\": {\"growth\": 2}}")).collect();
        assert_eq!(results, vec![OK, OK, E_FULL, E_FULL]);
        assert_eq!(c.end().len(), 2);
    }
}
