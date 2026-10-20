//! What a running plugin can reach: the capabilities granted to it, a read-only view of the world
//! for the length of one call, and the queue its actions go to. Every host function of the WASM
//! runtime and every step of a data plugin goes through here, so a capability is checked in one
//! place and a rule about an action holds for both kinds of plugin.
//!
//! Actions are never applied while the plugin runs: they are queued, and the host applies them
//! after the call through the same validated paths the admin API uses (`actions.rs`). A plugin
//! cannot change the world in any other way.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Instant;

use pcds_plugin_api::host::{E_BAD, E_DENIED, E_FULL, E_NOT_ALLOWED, E_NOT_FOUND, E_UNSUPPORTED, OK};
use pcds_plugin_api::query::{self, InventoryInfo, Nearby, NearbyArgs, ObjectInfo, ObjectRef, PlayerInfo, WorldInfo};
use pcds_plugin_api::{Level, Writer, caps};
use serde_json::Value;

use super::history::LogRing;
use super::limits;
use super::net::{self, Fetch, Policy, Reject};

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
    /// What a player carries; None if they are not online.
    fn inventory(&self, _client: u64) -> Option<InventoryInfo> {
        None
    }
    /// The world objects within `radius` of `center`, closest first, at most `max`; and how many
    /// objects the search looked at (it is paid for in fuel).
    fn nearby(&self, _center: [f32; 3], _radius: f32, _max: usize) -> (Vec<Nearby>, usize) {
        (Vec::new(), 0)
    }
    fn object(&self, _which: ObjectRef) -> Option<ObjectInfo> {
        None
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Say(String),
    Tell(u64, String),
    Kick(u64, String),
    Teleport(u64, [f32; 3]),
    RulesPatch(Value),
    /// `player.give`: `count` of the item `group` into the backpack of a player.
    Give(u64, String, u32),
    /// `world.tokens`: Terra tokens added (or, negative, taken away).
    Tokens(i32),
    /// An HTTP request (`net.http`): not applied to the game; the host hands it to its workers.
    Http(Fetch),
}

/// What a plugin with `net.http` carries: where it may go, the values the admin set for it, and how
/// many of its requests are alive (queued or running).
#[derive(Clone)]
pub struct NetCtx {
    pub policy: Arc<Policy>,
    pub config: Arc<BTreeMap<String, String>>,
    pub in_flight: Arc<AtomicU32>,
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

/// A timer a plugin set (`event.timer`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimerSlot {
    pub id: u32,
    pub delay_ms: u32,
    pub repeat_ms: u32,
    /// Game time (ms) it is due; None until the first game tick after it was set (a timer set in
    /// `load` starts counting then).
    pub due: Option<i64>,
}

pub struct Ctx {
    pub id: String,
    pub grant: Grant,
    /// The plugin's own settings: the `[[config]]` keys that are not secrets, as the admin set them
    /// or as the defaults say. `Q_CONFIG` answers with them.
    pub settings: Arc<Vec<(String, String)>>,
    pub timers: Vec<TimerSlot>,
    /// Game time (ms) of the call in progress, when the host knows it.
    now_ms: Option<i64>,
    /// Fuel the host's work in a query used beyond the call's price (the scan of `Q_NEARBY`): the
    /// WebAssembly runtime takes it out of the call's budget after each query.
    pub extra_fuel: u64,
    /// The capabilities this plugin used without having them, in the order it first did; the host
    /// says each once in the log and the listing keeps them.
    pub denied_caps: Vec<&'static str>,
    pub logs: Arc<LogRing>,
    gives_second: (Instant, u32),
    tokens_second: (Instant, u32),
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
    /// `net.http`: set when the capability is granted.
    pub net: Option<NetCtx>,
    http_second: (Instant, u32),
    /// The answers to `query` in this call, by kind: the world stands still during a call, so an
    /// answer is built once however often it is asked for.
    answers: Vec<(u32, Vec<u8>)>,
}

impl Ctx {
    pub fn new(id: &str, grant: Grant) -> Ctx {
        let now = Instant::now();
        Ctx { id: id.to_string(), grant, settings: Arc::default(), timers: Vec::new(), now_ms: None, extra_fuel: 0, denied_caps: Vec::new(), logs: Arc::default(), gives_second: (now, 0), tokens_second: (now, 0), world: None, actions: Vec::new(), denied: 0, refused: 0, second: (now, 0), rules_second: (now, 0), log_second: (now, 0), log_dropped: 0, net: None, http_second: (now, 0), answers: Vec::new() }
    }

    /// A fresh context for the same plugin (a restart after a fault): same id, grant and network access.
    pub fn renewed(&self) -> Ctx {
        let mut c = Ctx::new(&self.id, self.grant);
        c.net = self.net.clone();
        c.settings = self.settings.clone();
        c.logs = self.logs.clone();
        c.denied_caps = self.denied_caps.clone();
        c
    }

    /// Starts a call: the plugin may read `world` until `end`. `now_ms` is the game time.
    pub fn begin(&mut self, world: &dyn World, now_ms: i64) {
        self.now_ms = Some(now_ms);
        // SAFETY (of the later reads): the pointer is used only between begin and end, while the
        // host holds the game and runs nothing else. The lifetime is erased here and nowhere else.
        let ptr: *const (dyn World + '_) = world;
        self.world = Some(WorldRef(unsafe { std::mem::transmute::<*const (dyn World + '_), *const (dyn World + 'static)>(ptr) }));
        self.actions.clear();
        self.answers.clear();
    }

    /// Ends a call: the plugin reads no more, and the queued actions go to the host.
    pub fn end(&mut self) -> Vec<Action> {
        self.world = None;
        self.now_ms = None;
        self.answers.clear();
        std::mem::take(&mut self.actions)
    }

    fn world(&self) -> Option<&dyn World> {
        // SAFETY: see `begin`: set only while the host's call is in progress.
        self.world.map(|w| unsafe { &*w.0 })
    }

    fn allowed(&mut self, cap: &'static str) -> bool {
        let ok = self.grant.has(cap);
        if !ok {
            self.denied += 1;
            if !self.denied_caps.contains(&cap) {
                self.denied_caps.push(cap);
                // Once for each capability: the plugin's author finds the cause in the log, not a bare -1.
                self.log(Level::Warn, &format!("it used something that needs the capability {cap}, which is not granted: the call returned an error (denied) and did nothing; ask for {cap} in plugin.toml and have it approved"));
            }
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
        self.logs.push(level, &line);
        match level {
            Level::Error | Level::Warn => eprintln!("plugin {}: {line}", self.id),
            Level::Info => println!("plugin {}: {line}", self.id),
            // Debug lines are for the plugin's author, who reads them with the log at hand.
            Level::Debug => println!("plugin {} (debug): {line}", self.id),
        }
    }

    /// `query`: the answer's bytes, or the error code. A query without an argument is built the first
    /// time it is asked for in a call (the world stands still during one).
    pub fn query(&mut self, kind: u32, arg: &[u8]) -> Result<&[u8], i32> {
        if kind == query::Q_CONFIG {
            let mut w = Writer::new();
            query::encode_settings(&self.settings, &mut w);
            return self.answer(kind, w.0);
        }
        if !(query::Q_WORLD..=query::Q_OBJECT).contains(&kind) {
            return Err(E_UNSUPPORTED);
        }
        if !self.allowed(caps::WORLD_READ) {
            return Err(E_DENIED);
        }
        if arg.is_empty()
            && let Some(i) = self.answers.iter().position(|(k, _)| *k == kind)
        {
            return Ok(&self.answers[i].1);
        }
        let Some(world) = self.world() else { return Err(E_UNSUPPORTED) };
        let mut w = Writer::new();
        let mut scanned = 0usize;
        match kind {
            query::Q_WORLD => world.info().encode(&mut w),
            query::Q_PLAYERS => query::encode_players(&world.players(), &mut w),
            query::Q_RULES => {
                w.0.extend_from_slice(world.rules().as_bytes());
            }
            query::Q_INVENTORY => {
                let Ok(client) = <[u8; 8]>::try_from(arg).map(u64::from_le_bytes) else { return Err(E_BAD) };
                let Some(inventory) = world.inventory(client) else { return Err(E_NOT_FOUND) };
                inventory.encode(&mut w);
            }
            query::Q_NEARBY => {
                let Ok(a) = NearbyArgs::decode(arg) else { return Err(E_BAD) };
                if !a.center.iter().chain([&a.radius]).all(|v| v.is_finite()) || a.radius < 0.0 {
                    return Err(E_BAD);
                }
                let (found, looked) = world.nearby(a.center, a.radius.min(query::MAX_NEARBY_RADIUS), a.max.min(query::MAX_NEARBY) as usize);
                scanned = looked;
                query::encode_nearby(&found, &mut w);
            }
            query::Q_OBJECT => {
                let Ok(which) = ObjectRef::decode(arg) else { return Err(E_BAD) };
                match world.object(which) {
                    Some(o) => {
                        w.bool(true);
                        o.encode(&mut w);
                    }
                    None => {
                        w.bool(false);
                    }
                }
            }
            _ => return Err(E_UNSUPPORTED),
        }
        self.extra_fuel += scanned as u64 * limits::FUEL_PER_SCANNED_OBJECT;
        if !arg.is_empty() {
            // Kept in the call's answers for the length of the borrow only; asked again, built again.
            self.answers.retain(|(k, _)| *k != kind | 0x8000_0000);
            self.answers.push((kind | 0x8000_0000, w.0));
            return Ok(&self.answers[self.answers.len() - 1].1);
        }
        self.answer(kind, w.0)
    }

    fn answer(&mut self, kind: u32, bytes: Vec<u8>) -> Result<&[u8], i32> {
        self.answers.retain(|(k, _)| *k != kind);
        self.answers.push((kind, bytes));
        Ok(&self.answers[self.answers.len() - 1].1)
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

    /// `give`: `count` of the item `group` for a player. Queued; whether the player is still there and
    /// the backpack has room is decided when it is applied.
    pub fn give(&mut self, client: u64, group: &str, count: u32) -> i32 {
        if !self.allowed(caps::PLAYER_GIVE) {
            return E_DENIED;
        }
        let name_ok = !group.is_empty() && group.len() <= 64 && group.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
        if !name_ok || count == 0 || count > limits::MAX_GIVE_COUNT {
            return E_BAD;
        }
        if self.gives_second.0.elapsed().as_secs() >= 1 {
            self.gives_second = (Instant::now(), 0);
        }
        if self.gives_second.1 >= limits::GIVES_PER_SECOND {
            self.refused += 1;
            return E_FULL;
        }
        let code = self.queue(Action::Give(client, group.to_string(), count));
        if code == OK {
            self.gives_second.1 += 1;
        }
        code
    }

    /// `tokens`: Terra tokens added to the world's count or taken away.
    pub fn tokens(&mut self, amount: i32) -> i32 {
        if !self.allowed(caps::WORLD_TOKENS) {
            return E_DENIED;
        }
        if amount == 0 || amount.abs() > limits::MAX_TOKENS_PER_ACTION {
            return E_BAD;
        }
        if self.tokens_second.0.elapsed().as_secs() >= 1 {
            self.tokens_second = (Instant::now(), 0);
        }
        if self.tokens_second.1 >= limits::TOKEN_CHANGES_PER_SECOND {
            self.refused += 1;
            return E_FULL;
        }
        let code = self.queue(Action::Tokens(amount));
        if code == OK {
            self.tokens_second.1 += 1;
        }
        code
    }

    /// `timer_set`: takes effect at once (it is not an action on the world). The timer counts from
    /// the game time of this call, or from the next game tick when there is no call (in `load`).
    pub fn timer_set(&mut self, id: u32, delay_ms: u32, repeat_ms: u32) -> i32 {
        if !self.allowed(caps::EVENT_TIMER) {
            return E_DENIED;
        }
        if delay_ms > limits::MAX_TIMER_DELAY_MS || repeat_ms > limits::MAX_TIMER_DELAY_MS || (repeat_ms != 0 && repeat_ms < limits::MIN_TIMER_REPEAT_MS) {
            return E_BAD;
        }
        let slot = TimerSlot { id, delay_ms, repeat_ms, due: self.now_ms.map(|n| n + delay_ms as i64) };
        if let Some(t) = self.timers.iter_mut().find(|t| t.id == id) {
            *t = slot;
        } else if self.timers.len() >= limits::MAX_TIMERS {
            self.refused += 1;
            return E_FULL;
        } else {
            self.timers.push(slot);
        }
        OK
    }

    pub fn timer_cancel(&mut self, id: u32) -> i32 {
        if !self.allowed(caps::EVENT_TIMER) {
            return E_DENIED;
        }
        self.timers.retain(|t| t.id != id);
        OK
    }

    /// The timers due at game time `now`, as (id, how late in ms), at most `most`; a one-shot is
    /// removed, a repeating one is set for its next time after `now`. Timers that had no time yet start counting.
    pub fn timers_due(&mut self, now: i64, most: usize) -> Vec<(u32, u32)> {
        let mut due = Vec::new();
        for t in &mut self.timers {
            let at = *t.due.get_or_insert(now + t.delay_ms as i64);
            if at <= now && due.len() < most {
                due.push((t.id, (now - at).clamp(0, u32::MAX as i64) as u32));
                if t.repeat_ms == 0 {
                    t.due = Some(i64::MAX);
                } else {
                    let r = t.repeat_ms as i64;
                    t.due = Some(at + ((now - at) / r + 1) * r);
                }
            }
        }
        self.timers.retain(|t| t.due != Some(i64::MAX));
        due
    }

    /// The answer of `http_request` for a plugin that could not make a request at all (no capability,
    /// no `[net]`), so that the host copies nothing for it; None: it may.
    pub fn http_refused(&mut self) -> Option<i64> {
        if !self.allowed(caps::NET_HTTP) {
            Some(E_DENIED as i64)
        } else if self.net.is_none() {
            Some(E_UNSUPPORTED as i64)
        } else {
            None
        }
    }

    /// `http_request`: a ticket (positive), or the error code. The request is checked here, as far as
    /// it can be without the network (the list, the form, the rate); what is left (DNS, the address,
    /// the connection, the answer) comes in `HttpDone`. Nothing is sent from here: the request
    /// is queued like an action, and a call that faults has it dropped.
    pub fn http_request(&mut self, bytes: &[u8]) -> i64 {
        if !self.allowed(caps::NET_HTTP) {
            return E_DENIED as i64;
        }
        let Some(net) = self.net.clone() else { return E_UNSUPPORTED as i64 };
        let Ok(request) = pcds_plugin_api::HttpRequest::decode(bytes) else { return E_BAD as i64 };
        let mut fetch = match net.policy.build(&request, &net.config) {
            Ok(f) => f,
            Err(Reject::Bad(why)) => {
                self.log(Level::Warn, &format!("http request refused: {why}"));
                self.refused += 1;
                return E_BAD as i64;
            }
            Err(Reject::NotAllowed(why)) => {
                self.log(Level::Warn, &format!("http request refused: {why}"));
                self.refused += 1;
                return E_NOT_ALLOWED as i64;
            }
        };
        if self.http_second.0.elapsed().as_secs() >= 1 {
            self.http_second = (Instant::now(), 0);
        }
        let waiting = self.actions.iter().filter(|a| matches!(a, Action::Http(_))).count() as u32;
        if self.http_second.1 >= limits::HTTP_PER_SECOND || net.in_flight.load(Ordering::SeqCst) + waiting >= limits::HTTP_IN_FLIGHT {
            self.refused += 1;
            return E_FULL as i64;
        }
        fetch.ticket = net::next_ticket();
        let ticket = fetch.ticket;
        let code = self.queue(Action::Http(fetch));
        if code != OK {
            return code as i64;
        }
        self.http_second.1 += 1;
        ticket as i64
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
        c.begin(&Still, 0);
        assert_eq!(c.say("hello"), OK);
        assert_eq!(c.tell(3, "psst"), OK);
        assert_eq!(c.kick(3, "bye"), E_DENIED);
        assert_eq!(c.teleport(3, [0.0; 3]), E_DENIED);
        assert_eq!(c.rules_patch("{}"), E_DENIED);
        assert_eq!(c.denied, 3);
        assert_eq!(WorldInfo::decode(&c.query(query::Q_WORLD, &[]).unwrap()).unwrap().planet, "Prime");
        assert_eq!(query::decode_players(&c.query(query::Q_PLAYERS, &[]).unwrap()).unwrap()[0].name, "Ann");
        assert_eq!(c.query(99, &[]), Err(E_UNSUPPORTED));
        assert_eq!(c.end(), vec![Action::Say("hello".into()), Action::Tell(3, "psst".into())]);
        // After the call there is no world to read.
        assert_eq!(c.query(query::Q_WORLD, &[]), Err(E_UNSUPPORTED));
        let mut none = Ctx::new("t", Grant::default());
        none.begin(&Still, 0);
        assert_eq!(none.query(query::Q_WORLD, &[]), Err(E_DENIED));
    }

    #[test]
    fn the_queue_and_the_log_are_bounded_and_text_is_cleaned() {
        let mut c = Ctx::new("t", Grant::from_names(["chat.say", "player.teleport", "player.kick"]));
        c.begin(&Still, 0);
        for _ in 0..limits::ACTIONS_PER_CALL {
            assert_eq!(c.say("x"), OK);
        }
        assert_eq!(c.say("one too many"), E_FULL);
        assert_eq!(c.refused, 1);
        c.end();
        c.begin(&Still, 0);
        assert_eq!(c.say("\u{7}\u{8}  "), E_BAD, "text that is only control characters is refused");
        assert_eq!(c.teleport(1, [f32::NAN, 0.0, 0.0]), E_BAD);
        assert_eq!(c.teleport(1, [1e9, 0.0, 0.0]), E_BAD);
        assert_eq!(c.kick(1, &"r".repeat(500)), OK);
        let Action::Kick(_, reason) = c.end().pop().unwrap() else { panic!() };
        assert_eq!(reason.chars().count(), 128);
        assert_eq!(line(&"é".repeat(900)).chars().count(), limits::SAY_CHARS);
        // A rule change writes the rules file: two a second.
        let mut c = Ctx::new("t", Grant::from_names(["rules.write"]));
        c.begin(&Still, 0);
        let results: Vec<i32> = (0..4).map(|_| c.rules_patch("{\"speed\": {\"growth\": 2}}")).collect();
        assert_eq!(results, vec![OK, OK, E_FULL, E_FULL]);
        assert_eq!(c.end().len(), 2);
    }
}
