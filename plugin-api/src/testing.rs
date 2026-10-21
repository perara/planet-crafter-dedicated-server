//! Run a plugin in plain `cargo test`: no server, no WebAssembly. Needs the `testing` feature;
//! add the crate as a dev-dependency with it.
//!
//! - [`MockHost`] is a `Host` that holds a small world, answers queries from it, and writes down
//!   every action the plugin takes.
//! - [`Harness`] owns a mock and your plugin, delivers events and moves the game clock, so the
//!   plugin sees what the real server would send.
//!
//! The mock follows the real host's rules where a test can trip on them: a capability that is
//! not granted refuses the call, and a call may queue only 16 actions.
//!
//! ```
//! use pcds_plugin_api::testing::{Harness, MockHost};
//! use pcds_plugin_api::*;
//!
//! struct Hi;
//!
//! impl Plugin for Hi {
//!     fn load(_: &dyn Host, _: Option<State<'_>>) -> Self {
//!         Hi
//!     }
//!     fn event(&mut self, host: &dyn Host, event: &Event<'_>) -> Verdict {
//!         if let Event::Command { client, name, command: "hi", .. } = event {
//!             let _ = host.tell(*client, &format!("Hi, {name}!"));
//!         }
//!         Verdict::Continue
//!     }
//! }
//!
//! let mut test = Harness::<Hi>::start(MockHost::new());
//! test.join(3, "Ann");
//! test.command(3, "hi", &[]);
//! assert_eq!(test.mock().told(), [(3, "Hi, Ann!".to_string())]);
//! ```

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::cell::{RefCell, RefMut};

use crate::caps;
use crate::event::{Event, Kind, Verdict};
use crate::host::{Error, Host, Level};
use crate::http::HttpRequest;
use crate::query::{self, InventoryInfo, Nearby, ObjectInfo, ObjectRef, PlayerInfo, WorldInfo};
use crate::json;
use crate::{Plugin, SavedState, State};

/// The most actions a call may queue on the real host. `MockHost::with_limits` enforces it.
pub const ACTIONS_PER_CALL: u32 = 16;
// The real host's other numbers (server/src/plugins/limits.rs). The rates are counted in game
// time here, in wall time there.
const ACTIONS_PER_SECOND: u32 = 200;
const GIVES_PER_SECOND: u32 = 10;
const TOKEN_CHANGES_PER_SECOND: u32 = 2;
const RULE_CHANGES_PER_SECOND: u32 = 2;
const HTTP_PER_SECOND: u32 = 10;
const HTTP_IN_FLIGHT: usize = 2;
const MAX_GIVE_COUNT: u32 = 50;
const MAX_TOKENS_PER_ACTION: i32 = 10_000;
const MAX_TIMERS: usize = 16;
const MIN_TIMER_REPEAT_MS: u32 = 100;
const MAX_TIMER_DELAY_MS: u32 = 604_800_000;
/// Timer events one plugin gets in a game tick (here: in a step of `advance_ms`).
const TIMER_EVENTS_PER_TICK: usize = 4;
const SAY_CHARS: usize = 500;
const KICK_REASON_CHARS: usize = 128;
const MAX_TELEPORT: f32 = 100_000.0;

/// The game ticks every 5 ms; `Event::Tick::tick` is the game time divided by this.
const GAME_TICK_MS: i64 = 5;

/// A timer the plugin has set and that has not gone off (or is repeating).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Timer {
    /// The id the plugin gave it.
    pub id: u32,
    /// The delay it was set with.
    pub delay_ms: u32,
    /// The repeat it was set with; 0 for a one-shot timer.
    pub repeat_ms: u32,
    /// The game time of its next call.
    pub due_ms: i64,
}

/// An HTTP request the plugin started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpCall {
    /// `GET`, `POST` and so on.
    pub method: String,
    /// The URL as the plugin wrote it (`${config.KEY}` is not replaced).
    pub url: String,
    /// The request headers.
    pub headers: Vec<(String, String)>,
    /// The request body.
    pub body: Vec<u8>,
    /// The ticket `http_request` handed out. Give it to `Harness::http_done`.
    pub ticket: u64,
}

/// Everything the plugin did, which `clear` forgets.
#[derive(Default, Clone)]
struct Records {
    said: Vec<String>,
    told: Vec<(u64, String)>,
    kicked: Vec<(u64, String)>,
    teleported: Vec<(u64, [f32; 3])>,
    patches: Vec<String>,
    given: Vec<(u64, String, u32)>,
    tokens: i64,
    http: Vec<HttpCall>,
    logs: Vec<(Level, String)>,
    denied: Vec<String>,
}

/// A count of actions in the current second of game time.
#[derive(Default)]
struct Window {
    start: i64,
    n: u32,
}

impl Window {
    /// Starts a new second when the last is over; true if fewer than `max` were counted in this one.
    fn has_room(&mut self, now: i64, max: u32) -> bool {
        if now - self.start >= 1000 {
            *self = Window { start: now, n: 0 };
        }
        self.n < max
    }
}

struct Inner {
    world: WorldInfo,
    game_ms: i64,
    /// Wall clock minus game time: `unix_ms` is `unix_offset + game_ms`.
    unix_offset: i64,
    players: Vec<PlayerInfo>,
    objects: Vec<ObjectInfo>,
    inventories: Vec<(u64, InventoryInfo)>,
    settings: BTreeMap<String, String>,
    rules: String,
    deny: Vec<String>,
    limits: bool,
    queued: u32,
    next_ticket: u64,
    timers: Vec<Timer>,
    /// Timers set in `load`: they start counting at the first step after it, as on the host.
    unstarted: Vec<u32>,
    loading: bool,
    /// Clients the plugin kicked; the harness removes them after the call.
    pending_kicks: Vec<u64>,
    /// Tickets of requests with no answer yet.
    in_flight: Vec<u64>,
    actions: Window,
    gives: Window,
    token_changes: Window,
    rule_changes: Window,
    http_starts: Window,
    rec: Records,
}

impl Inner {
    /// Queues an action: with limits on, a call may queue 16 and a second 200.
    fn queue(&mut self) -> Result<(), Error> {
        if self.limits {
            if self.queued >= ACTIONS_PER_CALL || !self.actions.has_room(self.game_ms, ACTIONS_PER_SECOND) {
                return Err(Error::Full);
            }
            self.queued += 1;
            self.actions.n += 1;
        }
        Ok(())
    }
}

/// A `Host` for tests: a small world you set up, and a record of what the plugin did.
///
/// Every method takes `&self` (the state is behind a `RefCell`), so a test can change the world
/// while the plugin holds the host. Build one with `new` and the `with_*` methods, or change it
/// later with `add_player`, `set_setting` and the like. The readers (`said`, `told`, ...) return
/// copies.
pub struct MockHost {
    inner: RefCell<Inner>,
}

impl Default for MockHost {
    fn default() -> Self {
        Self::new()
    }
}

impl MockHost {
    /// An empty world on the planet "Prime", game time 0, no players, no denied capabilities,
    /// no limits.
    pub fn new() -> MockHost {
        MockHost {
            inner: RefCell::new(Inner {
                world: WorldInfo { planet: "Prime".to_string(), command_prefix: "!".to_string(), ..WorldInfo::default() },
                game_ms: 0,
                unix_offset: 1_760_000_000_000,
                players: Vec::new(),
                objects: Vec::new(),
                inventories: Vec::new(),
                settings: BTreeMap::new(),
                rules: "{}".to_string(),
                deny: Vec::new(),
                limits: false,
                queued: 0,
                next_ticket: 1,
                timers: Vec::new(),
                unstarted: Vec::new(),
                loading: false,
                pending_kicks: Vec::new(),
                in_flight: Vec::new(),
                actions: Window::default(),
                gives: Window::default(),
                token_changes: Window::default(),
                rule_changes: Window::default(),
                http_starts: Window::default(),
                rec: Records::default(),
            }),
        }
    }

    // ---- Setting up the world.

    /// Sets the world's numbers. `game_ms` and `unix_ms` start the clock (a zero `unix_ms` keeps
    /// the current one). A zero `players` or `world_objects` is filled in from the mock's lists, an
    /// empty `command_prefix` reads as `!`.
    pub fn with_world(self, world: WorldInfo) -> MockHost {
        {
            let mut s = self.inner.borrow_mut();
            s.game_ms = world.game_ms;
            if world.unix_ms != 0 {
                s.unix_offset = world.unix_ms - world.game_ms;
            }
            s.world = world;
        }
        self
    }

    /// Adds a player (see `add_player`).
    pub fn with_player(self, player: PlayerInfo) -> MockHost {
        self.add_player(player);
        self
    }

    /// Adds a world object (see `add_object`).
    pub fn with_object(self, object: ObjectInfo) -> MockHost {
        self.add_object(object);
        self
    }

    /// Sets what a player carries (see `set_inventory`).
    pub fn with_inventory(self, client: u64, inventory: InventoryInfo) -> MockHost {
        self.set_inventory(client, inventory);
        self
    }

    /// Sets one of the plugin's settings (see `set_setting`).
    pub fn with_setting(self, key: &str, value: &str) -> MockHost {
        self.set_setting(key, value);
        self
    }

    /// Sets the rules JSON that `Host::rules` returns (`{}` by default).
    pub fn with_rules(self, json: &str) -> MockHost {
        self.inner.borrow_mut().rules = json.to_string();
        self
    }

    /// Refuses the capabilities named (`caps::CHAT_SAY` or `"chat.say"`): a call that needs one
    /// returns `Err(Error::Denied)` and its name goes in `denied`. Panics on a name that is not a
    /// capability, so a typo does not pass as a test.
    pub fn deny(self, capabilities: &[&str]) -> MockHost {
        for name in capabilities {
            assert!(caps::known(name), "{name:?} is not a capability (see pcds_plugin_api::caps)");
            self.inner.borrow_mut().deny.push(name.to_string());
        }
        self
    }

    /// Turns on the real host's rates: a call queues at most 16 actions (say, tell, kick,
    /// teleport, rules_patch, http_request, give, tokens), then gets `Err(Error::Full)`; a
    /// second of game time takes 200 actions, 10 gives, 2 token changes, 2 rule changes and 10
    /// requests, and 2 requests may be alive (until `Harness::http_done`). The count of a call
    /// starts again at `begin_call`, which `Harness` calls before every event. The sizes (a
    /// give of over 50, 16 timers) are always checked, with or without this.
    pub fn with_limits(self) -> MockHost {
        self.inner.borrow_mut().limits = true;
        self
    }

    /// Adds a player; one with the same `client` is replaced.
    pub fn add_player(&self, player: PlayerInfo) {
        let mut s = self.inner.borrow_mut();
        s.players.retain(|p| p.client != player.client);
        s.players.push(player);
    }

    /// Removes the player with this connection number.
    pub fn remove_player(&self, client: u64) {
        self.inner.borrow_mut().players.retain(|p| p.client != client);
    }

    /// Adds a world object; one with the same `id` is replaced.
    pub fn add_object(&self, object: ObjectInfo) {
        let mut s = self.inner.borrow_mut();
        s.objects.retain(|o| o.id != object.id);
        s.objects.push(object);
    }

    /// Removes the world object with this id.
    pub fn remove_object(&self, id: i32) {
        self.inner.borrow_mut().objects.retain(|o| o.id != id);
    }

    /// Sets what a player carries. A player with no inventory set gets `Error::NotFound`.
    pub fn set_inventory(&self, client: u64, inventory: InventoryInfo) {
        let mut s = self.inner.borrow_mut();
        s.inventories.retain(|(c, _)| *c != client);
        s.inventories.push((client, inventory));
    }

    /// Sets one of the plugin's settings (what the admin put in the manifest's `[[config]]`).
    pub fn set_setting(&self, key: &str, value: &str) {
        self.inner.borrow_mut().settings.insert(key.to_string(), value.to_string());
    }

    /// Moves a player. Does nothing if they are not there.
    pub fn set_position(&self, client: u64, position: [f32; 3]) {
        if let Some(p) = self.inner.borrow_mut().players.iter_mut().find(|p| p.client == client) {
            p.position = position;
        }
    }

    /// Makes a player an operator or not. Does nothing if they are not there.
    pub fn set_operator(&self, client: u64, operator: bool) {
        if let Some(p) = self.inner.borrow_mut().players.iter_mut().find(|p| p.client == client) {
            p.operator = operator;
        }
    }

    /// Changes the world's numbers (`terraformation`, `standing_still`, `planet`...). The clock
    /// fields `game_ms` and `unix_ms` are not kept: they come from the clock (`advance_clock`,
    /// `set_unix_ms`).
    pub fn set_world(&self, change: impl FnOnce(&mut WorldInfo)) {
        change(&mut self.inner.borrow_mut().world);
    }

    // ---- The clock.

    /// The game clock, in milliseconds.
    pub fn game_ms(&self) -> i64 {
        self.inner.borrow().game_ms
    }

    /// The wall clock, in Unix milliseconds. It moves with the game clock.
    pub fn unix_ms(&self) -> i64 {
        let s = self.inner.borrow();
        s.unix_offset + s.game_ms
    }

    /// Says what the wall clock reads now. It goes on moving with the game clock.
    pub fn set_unix_ms(&self, unix_ms: i64) {
        let mut s = self.inner.borrow_mut();
        s.unix_offset = unix_ms - s.game_ms;
    }

    /// Moves the game clock on by `ms`. This only moves the clock; `Harness::advance_ms` also
    /// delivers the ticks and timers that fall due.
    pub fn advance_clock(&self, ms: i64) {
        self.inner.borrow_mut().game_ms += ms;
    }

    // ---- What the plugin did.

    /// Starts a new call: the count of queued actions goes back to 0 (it matters only with
    /// `with_limits`). `Harness` does this before every event.
    pub fn begin_call(&self) {
        self.inner.borrow_mut().queued = 0;
    }

    /// Forgets everything recorded (said, told, ..., logs, denied) and the call's action count.
    /// The world, the settings and the timers stay.
    pub fn clear(&self) {
        let mut s = self.inner.borrow_mut();
        s.rec = Records::default();
        s.queued = 0;
    }

    /// The messages to everyone, in order.
    pub fn said(&self) -> Vec<String> {
        self.inner.borrow().rec.said.clone()
    }

    /// The messages to one player: (client, text), in order.
    pub fn told(&self) -> Vec<(u64, String)> {
        self.inner.borrow().rec.told.clone()
    }

    /// What was told to one player, in order.
    pub fn told_to(&self, client: u64) -> Vec<String> {
        self.inner.borrow().rec.told.iter().filter(|(c, _)| *c == client).map(|(_, t)| t.clone()).collect()
    }

    /// The reasons one player was kicked with, in order.
    pub fn kicks_of(&self, client: u64) -> Vec<String> {
        self.inner.borrow().rec.kicked.iter().filter(|(c, _)| *c == client).map(|(_, r)| r.clone()).collect()
    }

    /// The positions one player was teleported to, in order.
    pub fn teleports_of(&self, client: u64) -> Vec<[f32; 3]> {
        self.inner.borrow().rec.teleported.iter().filter(|(c, _)| *c == client).map(|(_, p)| *p).collect()
    }

    /// The items given to one player: (group, count), in order.
    pub fn given_to(&self, client: u64) -> Vec<(String, u32)> {
        self.inner.borrow().rec.given.iter().filter(|(c, _, _)| *c == client).map(|(_, g, n)| (g.clone(), *n)).collect()
    }

    /// The kicks: (client, reason).
    pub fn kicked(&self) -> Vec<(u64, String)> {
        self.inner.borrow().rec.kicked.clone()
    }

    /// The teleports: (client, position).
    pub fn teleported(&self) -> Vec<(u64, [f32; 3])> {
        self.inner.borrow().rec.teleported.clone()
    }

    /// The JSON merge patches sent to the rules.
    pub fn rules_patches(&self) -> Vec<String> {
        self.inner.borrow().rec.patches.clone()
    }

    /// The items given: (client, group, count).
    pub fn given(&self) -> Vec<(u64, String, u32)> {
        self.inner.borrow().rec.given.clone()
    }

    /// The sum of all token amounts added or taken.
    pub fn tokens_total(&self) -> i64 {
        self.inner.borrow().rec.tokens
    }

    /// The timers that are set now, in the order they were set.
    pub fn timers(&self) -> Vec<Timer> {
        self.inner.borrow().timers.clone()
    }

    /// The HTTP requests started, in order.
    pub fn http_requests(&self) -> Vec<HttpCall> {
        self.inner.borrow().rec.http.clone()
    }

    /// The log lines: (level, text).
    pub fn logs(&self) -> Vec<(Level, String)> {
        self.inner.borrow().rec.logs.clone()
    }

    /// The capabilities that refused a call, one entry per refused call.
    pub fn denied(&self) -> Vec<String> {
        self.inner.borrow().rec.denied.clone()
    }

    // ---- Inside: used by the harness and the Host impl.

    /// Takes the borrow for a call that needs `cap`, or refuses it when the capability is denied.
    fn gate(&self, cap: &str) -> Result<RefMut<'_, Inner>, Error> {
        let mut s = self.inner.borrow_mut();
        if s.deny.iter().any(|c| c == cap) {
            s.rec.denied.push(cap.to_string());
            return Err(Error::Denied);
        }
        Ok(s)
    }

    fn command_prefix(&self) -> String {
        let prefix = self.inner.borrow().world.command_prefix.clone();
        if prefix.is_empty() { "!".to_string() } else { prefix }
    }

    /// Writes a line the host says to a player itself (a refused command): it is not the plugin's
    /// action, so it is recorded without any limit.
    fn host_tells(&self, client: u64, text: String) {
        self.inner.borrow_mut().rec.told.push((client, text));
    }

    fn set_loading(&self, loading: bool) {
        self.inner.borrow_mut().loading = loading;
    }

    fn next_kick(&self) -> Option<u64> {
        let mut s = self.inner.borrow_mut();
        if s.pending_kicks.is_empty() { None } else { Some(s.pending_kicks.remove(0)) }
    }

    fn finish_http(&self, ticket: u64) {
        self.inner.borrow_mut().in_flight.retain(|t| *t != ticket);
    }

    fn player_count(&self) -> u32 {
        self.inner.borrow().players.len() as u32
    }

    fn find_player(&self, client: u64) -> Option<PlayerInfo> {
        self.inner.borrow().players.iter().find(|p| p.client == client).cloned()
    }

    fn object_info(&self, id: i32) -> Option<ObjectInfo> {
        self.inner.borrow().objects.iter().find(|o| o.id == id).cloned()
    }

    fn set_rules(&self, json: &str) {
        self.inner.borrow_mut().rules = json.to_string();
    }

    fn clear_timers(&self) {
        let mut s = self.inner.borrow_mut();
        s.timers.clear();
        s.unstarted.clear();
    }

    /// Takes the timers that are due: (id, how late), as the host does at a game tick: the first
    /// four in the order they were set; a repeating timer is set for its next time after now, a
    /// one-shot is gone. With `only`, that timer is made due now and is the only one taken. Timers
    /// set in `load` start counting here.
    fn take_due_timers(&self, only: Option<u32>) -> Vec<(u32, u32)> {
        let mut s = self.inner.borrow_mut();
        let now = s.game_ms;
        for id in core::mem::take(&mut s.unstarted) {
            if let Some(t) = s.timers.iter_mut().find(|t| t.id == id) {
                t.due_ms = now + t.delay_ms as i64;
            }
        }
        let most = if only.is_some() { 1 } else { TIMER_EVENTS_PER_TICK };
        let mut due = Vec::new();
        let mut keep = Vec::new();
        for mut t in core::mem::take(&mut s.timers) {
            if only == Some(t.id) {
                t.due_ms = now;
            }
            if t.due_ms <= now && due.len() < most && only.is_none_or(|id| id == t.id) {
                let at = t.due_ms;
                due.push((t.id, (now - at) as u32));
                if t.repeat_ms > 0 {
                    let r = t.repeat_ms as i64;
                    t.due_ms = at + ((now - at) / r + 1) * r;
                    keep.push(t);
                }
            } else {
                keep.push(t);
            }
        }
        s.timers = keep;
        due
    }
}

/// One chat line as the host cleans it: control characters out (not newlines), cut to 500
/// characters, trimmed.
fn line(text: &str, most: usize) -> String {
    text.chars().filter(|c| !c.is_control() || *c == '\n').take(most).collect::<String>().trim().to_string()
}

fn distance(a: [f32; 3], b: [f32; 3]) -> f32 {
    let (x, y, z) = (a[0] - b[0], a[1] - b[1], a[2] - b[2]);
    (x * x + y * y + z * z).sqrt()
}

impl Host for MockHost {
    fn log(&self, level: Level, message: &str) {
        self.inner.borrow_mut().rec.logs.push((level, message.to_string()));
    }

    fn world(&self) -> Result<WorldInfo, Error> {
        let s = self.gate(caps::WORLD_READ)?;
        let mut world = s.world.clone();
        world.game_ms = s.game_ms;
        world.unix_ms = s.unix_offset + s.game_ms;
        if world.players == 0 {
            world.players = s.players.len() as u32;
        }
        if world.world_objects == 0 {
            world.world_objects = s.objects.len() as u32;
        }
        if world.command_prefix.is_empty() {
            world.command_prefix = "!".to_string();
        }
        Ok(world)
    }

    fn players(&self) -> Result<Vec<PlayerInfo>, Error> {
        Ok(self.gate(caps::WORLD_READ)?.players.clone())
    }

    fn rules(&self) -> Result<String, Error> {
        Ok(self.gate(caps::WORLD_READ)?.rules.clone())
    }

    fn say(&self, text: &str) -> Result<(), Error> {
        let mut s = self.gate(caps::CHAT_SAY)?;
        let text = line(text, SAY_CHARS);
        if text.is_empty() {
            return Err(Error::Bad);
        }
        s.queue()?;
        s.rec.said.push(text);
        Ok(())
    }

    fn tell(&self, client: u64, text: &str) -> Result<(), Error> {
        let mut s = self.gate(caps::CHAT_SAY)?;
        let text = line(text, SAY_CHARS);
        if text.is_empty() {
            return Err(Error::Bad);
        }
        s.queue()?;
        s.rec.told.push((client, text));
        Ok(())
    }

    fn kick(&self, client: u64, reason: &str) -> Result<(), Error> {
        let mut s = self.gate(caps::PLAYER_KICK)?;
        let reason = line(reason, KICK_REASON_CHARS);
        s.queue()?;
        s.rec.kicked.push((client, if reason.is_empty() { "Kicked".to_string() } else { reason }));
        s.pending_kicks.push(client);
        Ok(())
    }

    fn teleport(&self, client: u64, position: [f32; 3]) -> Result<(), Error> {
        let mut s = self.gate(caps::PLAYER_TELEPORT)?;
        if !position.iter().all(|v| v.is_finite() && v.abs() <= MAX_TELEPORT) {
            return Err(Error::Bad);
        }
        s.queue()?;
        s.rec.teleported.push((client, position));
        Ok(())
    }

    fn rules_patch(&self, patch: &str) -> Result<(), Error> {
        let mut s = self.gate(caps::RULES_WRITE)?;
        if !matches!(json::parse(patch), Ok(json::Value::Object(_))) {
            return Err(Error::Bad);
        }
        let now = s.game_ms;
        if s.limits && !s.rule_changes.has_room(now, RULE_CHANGES_PER_SECOND) {
            return Err(Error::Full);
        }
        s.queue()?;
        s.rule_changes.n += 1;
        s.rec.patches.push(patch.to_string());
        Ok(())
    }

    fn http_request(&self, request: &HttpRequest<'_>) -> Result<u64, Error> {
        let mut s = self.gate(caps::NET_HTTP)?;
        if !["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE"].contains(&request.method) || request.url.is_empty() {
            return Err(Error::Bad);
        }
        let now = s.game_ms;
        if s.limits && (s.in_flight.len() >= HTTP_IN_FLIGHT || !s.http_starts.has_room(now, HTTP_PER_SECOND)) {
            return Err(Error::Full);
        }
        s.queue()?;
        s.http_starts.n += 1;
        let ticket = s.next_ticket;
        s.next_ticket += 1;
        s.in_flight.push(ticket);
        s.rec.http.push(HttpCall {
            method: request.method.to_string(),
            url: request.url.to_string(),
            headers: request.headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            body: request.body.to_vec(),
            ticket,
        });
        Ok(ticket)
    }

    fn player(&self, client: u64) -> Result<PlayerInfo, Error> {
        let s = self.gate(caps::WORLD_READ)?;
        s.players.iter().find(|p| p.client == client).cloned().ok_or(Error::NotFound)
    }

    fn inventory(&self, client: u64) -> Result<InventoryInfo, Error> {
        let s = self.gate(caps::WORLD_READ)?;
        if !s.players.iter().any(|p| p.client == client) {
            return Err(Error::NotFound);
        }
        Ok(s.inventories.iter().find(|(c, _)| *c == client).map(|(_, i)| i.clone()).unwrap_or(InventoryInfo { slots_used: 0, slots_total: 30, items: Vec::new() }))
    }

    fn nearby(&self, center: [f32; 3], radius: f32, max: u32) -> Result<Vec<Nearby>, Error> {
        let s = self.gate(caps::WORLD_READ)?;
        if !center.iter().chain([&radius]).all(|v| v.is_finite()) || radius < 0.0 {
            return Err(Error::Bad);
        }
        let radius = radius.min(query::MAX_NEARBY_RADIUS);
        let mut found: Vec<Nearby> = s
            .objects
            .iter()
            .map(|o| Nearby { object: o.clone(), distance: distance(o.position, center) })
            .filter(|n| n.distance <= radius)
            .collect();
        found.sort_by(|a, b| a.distance.total_cmp(&b.distance).then(a.object.id.cmp(&b.object.id)));
        found.truncate(max.min(query::MAX_NEARBY) as usize);
        Ok(found)
    }

    fn object(&self, which: ObjectRef) -> Result<Option<ObjectInfo>, Error> {
        let s = self.gate(caps::WORLD_READ)?;
        Ok(s.objects
            .iter()
            .find(|o| match which {
                ObjectRef::World(id) => o.id == id,
                // A world object with net 0 has no network object.
                ObjectRef::Net(net) => net != 0 && o.net == net,
            })
            .cloned())
    }

    fn settings(&self) -> Result<Vec<(String, String)>, Error> {
        Ok(self.inner.borrow().settings.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
    }

    fn give(&self, client: u64, group: &str, count: u32) -> Result<(), Error> {
        let mut s = self.gate(caps::PLAYER_GIVE)?;
        let name_ok = !group.is_empty() && group.len() <= 64 && group.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
        if !name_ok || count == 0 || count > MAX_GIVE_COUNT {
            return Err(Error::Bad);
        }
        let now = s.game_ms;
        if s.limits && !s.gives.has_room(now, GIVES_PER_SECOND) {
            return Err(Error::Full);
        }
        s.queue()?;
        s.gives.n += 1;
        s.rec.given.push((client, group.to_string(), count));
        Ok(())
    }

    fn tokens(&self, amount: i32) -> Result<(), Error> {
        let mut s = self.gate(caps::WORLD_TOKENS)?;
        if amount == 0 || amount.abs() > MAX_TOKENS_PER_ACTION {
            return Err(Error::Bad);
        }
        let now = s.game_ms;
        if s.limits && !s.token_changes.has_room(now, TOKEN_CHANGES_PER_SECOND) {
            return Err(Error::Full);
        }
        s.queue()?;
        s.token_changes.n += 1;
        s.rec.tokens += amount as i64;
        Ok(())
    }

    fn set_timer(&self, id: u32, delay_ms: u32, repeat_ms: u32) -> Result<(), Error> {
        let mut s = self.gate(caps::EVENT_TIMER)?;
        if delay_ms > MAX_TIMER_DELAY_MS || repeat_ms > MAX_TIMER_DELAY_MS || (repeat_ms != 0 && repeat_ms < MIN_TIMER_REPEAT_MS) {
            return Err(Error::Bad);
        }
        if s.timers.len() >= MAX_TIMERS && !s.timers.iter().any(|t| t.id == id) {
            return Err(Error::Full);
        }
        let due_ms = s.game_ms + delay_ms as i64;
        let timer = Timer { id, delay_ms, repeat_ms, due_ms };
        match s.timers.iter().position(|t| t.id == id) {
            Some(i) => s.timers[i] = timer,
            None => s.timers.push(timer),
        }
        s.unstarted.retain(|u| *u != id);
        if s.loading {
            s.unstarted.push(id);
        }
        Ok(())
    }

    fn cancel_timer(&self, id: u32) -> Result<(), Error> {
        let mut s = self.gate(caps::EVENT_TIMER)?;
        s.timers.retain(|t| t.id != id);
        s.unstarted.retain(|u| *u != id);
        Ok(())
    }
}

/// Your plugin and a `MockHost`, with sugar to deliver events the way the server does.
///
/// The `join`, `leave`, `place` and `destroy` calls also keep the mock's player and object lists
/// in step. Every event is one call: the mock's action count starts again before it. Records stay
/// in the mock until `clear`.
///
/// Like the host, the harness removes a player the plugin kicked and delivers their
/// `Event::PlayerLeave` right after the call that kicked them.
pub struct Harness<P: Plugin> {
    mock: MockHost,
    plugin: P,
    tick_every_ms: u32,
    step_ms: u32,
    last_tick_ms: i64,
    /// The interest mask of `subscribe`; `None`: every event.
    subscribed: Option<u32>,
    commands: Vec<CommandSpec>,
}

/// A command as the manifest declares it (`Harness::declare_command`).
struct CommandSpec {
    name: String,
    min_args: u32,
    max_args: u32,
    operator_only: bool,
}

impl<P: Plugin> Harness<P> {
    /// Loads the plugin with no saved state (`Plugin::load` gets `None`).
    pub fn start(mock: MockHost) -> Harness<P> {
        Self::boot(mock, None)
    }

    /// Loads the plugin with state it saved earlier, as after a restart.
    pub fn start_with_state(mock: MockHost, version: u32, bytes: &[u8]) -> Harness<P> {
        Self::boot(mock, Some(State { version, bytes }))
    }

    fn boot(mock: MockHost, state: Option<State<'_>>) -> Harness<P> {
        mock.begin_call();
        mock.set_loading(true);
        let plugin = P::load(&mock, state);
        mock.set_loading(false);
        let last_tick_ms = mock.game_ms();
        let mut harness = Harness { mock, plugin, tick_every_ms: 1000, step_ms: 50, last_tick_ms, subscribed: None, commands: Vec::new() };
        harness.settle();
        harness
    }

    /// How often a tick is delivered, in game milliseconds; 0 for never. The default is 1000.
    pub fn tick_every_ms(mut self, ms: u32) -> Harness<P> {
        self.tick_every_ms = ms;
        self
    }

    /// The step `advance_ms` moves the clock in (50 by default; the game itself steps 5). Ticks and
    /// timers are delivered at the end of a step, so a smaller step is a finer clock. At least 1.
    pub fn tick_step_ms(mut self, ms: u32) -> Harness<P> {
        self.step_ms = ms.max(1);
        self
    }

    /// Delivers only the kinds of event given, as a manifest's `events` list does (the default is
    /// every kind). `Timer`, `HttpDone` and `Command` are always delivered, as on the host.
    pub fn subscribe(mut self, kinds: &[Kind]) -> Harness<P> {
        self.subscribed = Some(kinds.iter().fold(0, |mask, k| mask | k.bit()));
        self
    }

    /// Declares a chat command as the manifest does. From then on `command` answers like the host: a
    /// player who is not an operator (for `operator_only`), or who gives fewer than `min_args` or
    /// more than `max_args` arguments (`u32::MAX`: no limit), is told `!name is for the server's
    /// operators.` or `Use: !name`, and the plugin is not called. A command not declared is delivered.
    pub fn declare_command(&mut self, name: &str, min_args: u32, max_args: u32, operator_only: bool) {
        let name = name.to_ascii_lowercase();
        self.commands.retain(|c| c.name != name);
        self.commands.push(CommandSpec { name, min_args, max_args, operator_only });
    }

    /// The mock host.
    pub fn mock(&self) -> &MockHost {
        &self.mock
    }

    /// The plugin, to look at its fields.
    pub fn plugin(&self) -> &P {
        &self.plugin
    }

    /// The plugin, to change its fields.
    pub fn plugin_mut(&mut self) -> &mut P {
        &mut self.plugin
    }

    /// Forgets the mock's records (see `MockHost::clear`).
    pub fn clear(&self) {
        self.mock.clear();
    }

    /// Says what the wall clock reads now (see `MockHost::set_unix_ms`).
    pub fn set_unix_ms(&self, unix_ms: i64) {
        self.mock.set_unix_ms(unix_ms);
    }

    /// Delivers any event as one call, and returns the plugin's verdict. With `subscribe`, an
    /// event of a kind not listed is not delivered (the answer is `Continue`). Players the plugin
    /// kicked in the call leave afterwards.
    pub fn event(&mut self, event: &Event<'_>) -> Verdict {
        let verdict = self.deliver(event);
        self.settle();
        verdict
    }

    fn deliver(&mut self, event: &Event<'_>) -> Verdict {
        let kind = event.kind();
        let always = matches!(kind, Kind::Timer | Kind::HttpDone | Kind::Command);
        if !always && self.subscribed.is_some_and(|mask| mask & kind.bit() == 0) {
            return Verdict::Continue;
        }
        self.mock.begin_call();
        self.plugin.event(&self.mock, event)
    }

    /// Applies what the host does after a call: a player the plugin kicked is removed and their
    /// `PlayerLeave` is delivered (which may kick again).
    fn settle(&mut self) {
        while let Some(client) = self.mock.next_kick() {
            if let Some(who) = self.mock.find_player(client) {
                self.mock.remove_player(client);
                self.deliver(&Event::PlayerLeave { client, name: &who.name, player_id: who.player_id });
            }
        }
    }

    // ---- Players and chat.

    /// A player joins: adds them to the mock at (0, 0, 0) with full gauges, `player_id` of
    /// `client + 1000` and no Steam id, and delivers `Event::PlayerJoin`.
    pub fn join(&mut self, client: u64, name: &str) -> Verdict {
        self.join_as(client, name, client + 1000, "")
    }

    /// Like `join`, with the player's id and Steam id given.
    pub fn join_as(&mut self, client: u64, name: &str, player_id: u64, steam_id: &str) -> Verdict {
        self.mock.add_player(PlayerInfo {
            client,
            name: name.to_string(),
            steam_id: steam_id.to_string(),
            position: [0.0; 3],
            gauges: [100.0, 100.0, 100.0, 0.0],
            player_id,
            operator: false,
        });
        self.event(&Event::PlayerJoin { client, name, steam_id, player_id })
    }

    /// A player leaves: removes them from the mock and delivers `Event::PlayerLeave`.
    pub fn leave(&mut self, client: u64) -> Verdict {
        let who = self.mock.find_player(client).unwrap_or_else(|| PlayerInfo { client, player_id: client + 1000, ..PlayerInfo::default() });
        self.mock.remove_player(client);
        self.event(&Event::PlayerLeave { client, name: &who.name, player_id: who.player_id })
    }

    /// A player says something: delivers `Event::Chat`. The player should have joined.
    pub fn chat(&mut self, client: u64, text: &str) -> Verdict {
        let who = self.mock.find_player(client).unwrap_or_default();
        self.event(&Event::Chat { client, name: &who.name, text, player_id: who.player_id })
    }

    /// A player uses a chat command: delivers `Event::Command` with `text` as the player typed it
    /// (the world's prefix, `!` by default, the name, the arguments). The `command` delivered is
    /// the name in lower case, as the host will deliver it. If the command was declared
    /// (`declare_command`) and the player may not use it or gave the wrong number of arguments, the
    /// host's answer is recorded as told to the player and nothing is delivered. The player should
    /// have joined.
    pub fn command(&mut self, client: u64, name: &str, args: &[&str]) -> Verdict {
        let who = self.mock.find_player(client).unwrap_or_default();
        let prefix = self.mock.command_prefix();
        let command = name.to_ascii_lowercase();
        if let Some(spec) = self.commands.iter().find(|c| c.name == command) {
            let refusal = if spec.operator_only && !who.operator {
                Some(format!("{prefix}{command} is for the server's operators."))
            } else if (args.len() as u64) < spec.min_args as u64 || (args.len() as u64) > spec.max_args as u64 {
                Some(format!("Use: {prefix}{command}"))
            } else {
                None
            };
            if let Some(reply) = refusal {
                self.mock.host_tells(client, reply);
                return Verdict::Continue;
            }
        }
        let mut text = format!("{prefix}{name}");
        for arg in args {
            text.push(' ');
            text.push_str(arg);
        }
        self.event(&Event::Command { client, name: &who.name, command: &command, args: args.to_vec(), text: &text, player_id: who.player_id })
    }

    // ---- The world.

    /// A client RPC is about to run: delivers `Event::RpcBefore`. `Verdict::Veto` drops it.
    pub fn rpc_before(&mut self, client: u64, rpc: &str, object: u64, args_json: &str) -> Verdict {
        self.event(&Event::RpcBefore { client, rpc, object, args_json })
    }

    /// A client RPC has run: delivers `Event::RpcAfter`.
    pub fn rpc_after(&mut self, client: u64, rpc: &str, object: u64, args_json: &str) -> Verdict {
        self.event(&Event::RpcAfter { client, rpc, object, args_json })
    }

    /// An object is placed (`client` 0: nobody built it): adds it to the mock and delivers
    /// `Event::ObjectPlaced`. The object has no network id; see `place_net`.
    pub fn place(&mut self, id: i32, group: &str, position: [f32; 3], client: u64) -> Verdict {
        self.place_net(id, group, position, 0, client)
    }

    /// Like `place`, for an object with the network id `net` (what `Host::object` finds with
    /// `ObjectRef::Net` and an RPC's `object` is).
    pub fn place_net(&mut self, id: i32, group: &str, position: [f32; 3], net: u64, client: u64) -> Verdict {
        self.mock.add_object(ObjectInfo { id, net, group: group.to_string(), position });
        self.event(&Event::ObjectPlaced { id, group, position, client })
    }

    /// An object is destroyed (`client` 0: nobody did it): removes it from the mock and delivers
    /// `Event::ObjectDestroyed` with the group and position the mock knew.
    pub fn destroy(&mut self, id: i32, client: u64) -> Verdict {
        let object = self.mock.object_info(id).unwrap_or_default();
        self.mock.remove_object(id);
        self.event(&Event::ObjectDestroyed { id, group: &object.group, position: object.position, client })
    }

    /// The world was saved: delivers `Event::WorldSaved` at the current game time.
    pub fn world_saved(&mut self, ok: bool) -> Verdict {
        let game_ms = self.mock.game_ms();
        self.event(&Event::WorldSaved { game_ms, ok })
    }

    /// The rules changed: makes `Host::rules` return `changes_json` and delivers `Event::RuleChange`.
    pub fn rule_change(&mut self, changes_json: &str) -> Verdict {
        self.mock.set_rules(changes_json);
        self.event(&Event::RuleChange { changes_json })
    }

    /// The server was hot-updated to `server_version`: delivers `Event::Flip`.
    pub fn flip(&mut self, server_version: &str) -> Verdict {
        let game_ms = self.mock.game_ms();
        self.event(&Event::Flip { game_ms, server_version })
    }

    /// An HTTP request is over: delivers `Event::HttpDone` with no headers and no error text. Take
    /// the ticket from `MockHost::http_requests`.
    pub fn http_done(&mut self, ticket: u64, status: u32, body: &[u8]) -> Verdict {
        self.http_done_with(ticket, status, "", &[], body)
    }

    /// Like `http_done`, with the answer's headers and, for a request that got no answer (`status`
    /// 0), the error text. The request is no longer in flight for the limit of `with_limits`.
    pub fn http_done_with(&mut self, ticket: u64, status: u32, error: &str, headers: &[(&str, &str)], body: &[u8]) -> Verdict {
        self.mock.finish_http(ticket);
        self.event(&Event::HttpDone { ticket, status, error, body, headers: headers.to_vec() })
    }

    // ---- Time.

    /// Lets `ms` of game time pass, one step at a time (`tick_step_ms`). At the end of each step it
    /// delivers a `Tick` if `tick_every_ms` has passed since the last one, then a `Timer` for each
    /// timer that is due, as the host does at a game tick: the first four, in the order they were
    /// set (the rest wait for the next step). A repeating timer is called at most once per step; a
    /// repeat that fell inside a step is skipped. A timer set while these are delivered waits for
    /// the next step, and one set in `load` starts counting at the first step.
    pub fn advance_ms(&mut self, ms: u64) {
        let mut left = ms as i64;
        while left > 0 {
            let step = left.min(self.step_ms as i64);
            left -= step;
            self.mock.advance_clock(step);
            let now = self.mock.game_ms();
            if self.tick_every_ms > 0 && now - self.last_tick_ms >= self.tick_every_ms as i64 {
                let since_ms = (now - self.last_tick_ms) as u32;
                self.last_tick_ms = now;
                self.event(&Event::Tick { game_ms: now, tick: (now / GAME_TICK_MS) as i32, players: self.mock.player_count(), since_ms, unix_ms: self.mock.unix_ms() });
            }
            for (id, late_ms) in self.mock.take_due_timers(None) {
                self.event(&Event::Timer { id, game_ms: now, late_ms });
            }
        }
    }

    /// Calls a timer now, whenever it is due: delivers `Event::Timer` with `late_ms` 0, and removes
    /// the timer if it is a one-shot or sets it for its next time if it repeats, as the host does.
    /// Panics if the plugin has no timer with this id.
    pub fn fire_timer(&mut self, id: u32) -> Verdict {
        assert!(self.mock.timers().iter().any(|t| t.id == id), "no timer {id} is set");
        let now = self.mock.game_ms();
        match self.mock.take_due_timers(Some(id)).first() {
            Some(&(id, late_ms)) => self.event(&Event::Timer { id, game_ms: now, late_ms }),
            None => Verdict::Continue,
        }
    }

    // ---- Saving and loading.

    /// Asks the plugin for its state, as the server does at a world save.
    pub fn save(&mut self) -> Option<SavedState> {
        self.mock.begin_call();
        self.plugin.save()
    }

    /// Replaces the plugin as a reload or a hot update does: asks the old one for its state, drops
    /// it and its timers (the real host drops timers with the instance), and calls `load` of a new
    /// one with that state. Use it to test a state migration or a plugin that sets its timers in
    /// `load`.
    pub fn reload(&mut self) {
        let saved = self.save();
        self.mock.clear_timers();
        self.mock.begin_call();
        self.mock.set_loading(true);
        let state = saved.as_ref().map(|s| State { version: s.version, bytes: &s.bytes });
        self.plugin = P::load(&self.mock, state);
        self.mock.set_loading(false);
        self.settle();
    }

    /// Switches the plugin off as the server does: calls `Plugin::unload`, drops the timers, then
    /// asks for the state one more time and returns it.
    pub fn unload(&mut self) -> Option<SavedState> {
        self.plugin.unload();
        self.mock.clear_timers();
        self.save()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::{Reader, Writer};
    use alloc::format;
    use alloc::vec;

    /// A plugin that keeps a note of what it was given and does what its commands say.
    #[derive(Default)]
    struct Probe {
        ticks: Vec<(i64, i32, u32, u32, i64)>,
        timers: Vec<(u32, i64, u32)>,
        results: Vec<Result<(), Error>>,
        count: u32,
        loaded: Option<(u32, Vec<u8>)>,
        unloaded: bool,
        http_done: Vec<(u64, u32)>,
    }

    impl Plugin for Probe {
        fn load(host: &dyn Host, state: Option<State<'_>>) -> Self {
            let mut me = Probe::default();
            if let Some(state) = state {
                me.loaded = Some((state.version, state.bytes.to_vec()));
                // Version 1 held a u32; version 2 holds a u32 and an unused byte.
                me.count = Reader::new(state.bytes).u32().unwrap_or(0);
            }
            host.log(Level::Info, "loaded");
            me
        }

        fn event(&mut self, host: &dyn Host, event: &Event<'_>) -> Verdict {
            match event {
                Event::Tick { game_ms, tick, players, since_ms, unix_ms } => self.ticks.push((*game_ms, *tick, *players, *since_ms, *unix_ms)),
                Event::Timer { id, game_ms, late_ms } => self.timers.push((*id, *game_ms, *late_ms)),
                Event::HttpDone { ticket, status, .. } => self.http_done.push((*ticket, *status)),
                Event::Command { command: "arm", args, .. } => {
                    // arm <id> <delay> <repeat>
                    let n: Vec<u32> = args.iter().map(|a| a.parse().unwrap()).collect();
                    self.results.push(host.set_timer(n[0], n[1], n[2]));
                }
                Event::Command { command: "spam", .. } => {
                    for _ in 0..20 {
                        self.results.push(host.say("spam"));
                    }
                }
                Event::Command { client, command: "count", .. } => {
                    self.count += 1;
                    let _ = host.tell(*client, &format!("{}", self.count));
                }
                Event::RpcBefore { args_json, .. } if args_json.contains("evil") => return Verdict::Veto,
                _ => {}
            }
            Verdict::Continue
        }

        fn save(&mut self) -> Option<SavedState> {
            let mut w = Writer::new();
            w.u32(self.count);
            Some(SavedState { version: 2, bytes: w.0 })
        }

        fn unload(&mut self) {
            self.unloaded = true;
        }
    }

    fn start() -> Harness<Probe> {
        Harness::start(MockHost::new())
    }

    fn at(position: [f32; 3], id: i32) -> ObjectInfo {
        ObjectInfo { id, net: (id + 500) as u64, group: "Drill1".to_string(), position }
    }

    #[test]
    fn the_tick_comes_at_the_cadence_with_the_right_numbers() {
        let mut t = start();
        t.join(1, "Ann");
        t.advance_ms(3500);
        let ticks = &t.plugin().ticks;
        assert_eq!(ticks.len(), 3);
        let unix = t.mock().unix_ms() - t.mock().game_ms();
        for (i, tick) in ticks.iter().enumerate() {
            let at = 1000 * (i as i64 + 1);
            assert_eq!(*tick, (at, (at / 5) as i32, 1, 1000, unix + at));
        }
        // A finer cadence and a coarser step: a tick waits for the end of a step.
        let mut t = start().tick_every_ms(250).tick_step_ms(100);
        t.advance_ms(1000);
        let sinces: Vec<u32> = t.plugin().ticks.iter().map(|k| k.3).collect();
        assert_eq!(sinces, [300, 300, 300]);
        let mut none = start().tick_every_ms(0);
        none.advance_ms(10_000);
        assert!(none.plugin().ticks.is_empty());
    }

    #[test]
    fn the_unix_clock_moves_with_the_game_clock() {
        let mut t = start();
        t.set_unix_ms(1_000_000);
        t.advance_ms(2000);
        assert_eq!(t.mock().unix_ms(), 1_002_000);
        assert_eq!(t.mock().world().unwrap().unix_ms, 1_002_000);
        assert_eq!(t.mock().world().unwrap().game_ms, 2000);
        let world = WorldInfo { game_ms: 7000, unix_ms: 9_000_000, planet: "Moon".into(), ..WorldInfo::default() };
        let t = Harness::<Probe>::start(MockHost::new().with_world(world));
        assert_eq!((t.mock().game_ms(), t.mock().unix_ms()), (7000, 9_000_000));
    }

    #[test]
    fn a_repeating_timer_fires_the_right_number_of_times_and_a_one_shot_once() {
        let mut t = start().tick_every_ms(0);
        t.join(1, "Ann");
        t.command(1, "arm", &["7", "500", "500"]);
        t.command(1, "arm", &["8", "120", "0"]);
        assert_eq!(t.mock().timers().len(), 2);
        t.advance_ms(2600);
        let fired = |id: u32| t.plugin().timers.iter().filter(|x| x.0 == id).cloned().collect::<Vec<_>>();
        // Due at 500, 1000, ..., 2500: five times, on the step that ends at the time.
        assert_eq!(fired(7).iter().map(|x| (x.1, x.2)).collect::<Vec<_>>(), [(500, 0), (1000, 0), (1500, 0), (2000, 0), (2500, 0)]);
        // Due at 120: delivered at the end of the step at 150, 30 ms late; then gone.
        assert_eq!(fired(8), [(8, 150, 30)]);
        assert_eq!(t.mock().timers().iter().map(|x| x.id).collect::<Vec<_>>(), [7]);
        assert_eq!(t.mock().timers()[0].due_ms, 3000);
        // Setting an id again replaces it; cancelling takes it away.
        t.command(1, "arm", &["7", "10000", "0"]);
        assert_eq!(t.mock().timers().len(), 1);
        t.mock().cancel_timer(7).unwrap();
        t.mock().cancel_timer(7).unwrap();
        assert!(t.mock().timers().is_empty());
    }

    #[test]
    fn a_repeat_shorter_than_a_step_is_called_once_per_step() {
        let mut t = start().tick_every_ms(0).tick_step_ms(250);
        t.command(1, "arm", &["1", "100", "100"]);
        t.advance_ms(1000);
        // Due at 100: delivered at 250 (150 late); the repeats at 200 and 300 are skipped to 300,
        // which is delivered at 500, and so on.
        assert_eq!(t.plugin().timers, [(1, 250, 150), (1, 500, 200), (1, 750, 150), (1, 1000, 200)]);
    }

    #[test]
    fn a_reload_hands_the_state_over_and_drops_the_timers() {
        let mut t = start().tick_every_ms(0);
        t.join(1, "Ann");
        t.command(1, "count", &[]);
        t.command(1, "count", &[]);
        t.command(1, "arm", &["7", "500", "500"]);
        t.reload();
        assert_eq!(t.plugin().loaded, Some((2, vec![2, 0, 0, 0])));
        assert_eq!(t.plugin().count, 2);
        assert!(t.mock().timers().is_empty());
        t.advance_ms(2000);
        assert!(t.plugin().timers.is_empty());
        // A restart with state written by an older version.
        let old = Harness::<Probe>::start_with_state(MockHost::new(), 1, &[9, 0, 0, 0]);
        assert_eq!((old.plugin().loaded.clone(), old.plugin().count), (Some((1, vec![9, 0, 0, 0])), 9));
        assert!(start().plugin().loaded.is_none());
    }

    #[test]
    fn unload_asks_for_the_state_and_keeps_the_timers_off() {
        let mut t = start();
        t.command(1, "arm", &["7", "500", "500"]);
        let saved = t.unload().unwrap();
        assert_eq!((saved.version, saved.bytes), (2, vec![0, 0, 0, 0]));
        assert!(t.plugin().unloaded);
        assert!(t.mock().timers().is_empty());
    }

    #[test]
    fn a_call_queues_at_most_16_actions_with_limits_on() {
        let mut t = Harness::<Probe>::start(MockHost::new().with_limits());
        t.join(1, "Ann");
        t.command(1, "spam", &[]);
        let r = &t.plugin().results;
        assert_eq!(r.len(), 20);
        assert!(r[..16].iter().all(|x| x.is_ok()));
        assert!(r[16..].iter().all(|x| *x == Err(Error::Full)));
        assert_eq!(t.mock().said().len(), 16);
        // The next event is a new call. Reads and timers are not actions.
        t.command(1, "count", &[]);
        assert_eq!(t.mock().told().len(), 1);
        for _ in 0..40 {
            t.mock().world().unwrap();
            t.mock().set_timer(1, 10, 0).unwrap();
        }
        // Without limits, all of them go through.
        let mut free = start();
        free.command(1, "spam", &[]);
        assert_eq!(free.mock().said().len(), 20);
        // begin_call resets the count by hand.
        let mock = MockHost::new().with_limits();
        for _ in 0..16 {
            mock.say("x").unwrap();
        }
        assert_eq!(mock.kick(1, "r"), Err(Error::Full));
        mock.begin_call();
        assert_eq!(mock.kick(1, "r"), Ok(()));
    }

    #[test]
    fn a_denied_capability_refuses_the_call_and_is_recorded() {
        let mock = MockHost::new().with_player(PlayerInfo { client: 1, ..Default::default() }).deny(&[caps::CHAT_SAY, "world.read", "event.timer"]);
        assert_eq!(mock.say("x"), Err(Error::Denied));
        assert_eq!(mock.tell(1, "x"), Err(Error::Denied));
        assert_eq!(mock.world().err(), Some(Error::Denied));
        assert_eq!(mock.players().err(), Some(Error::Denied));
        assert_eq!(mock.nearby([0.0; 3], 10.0, 5).err(), Some(Error::Denied));
        assert_eq!(mock.set_timer(1, 1, 0), Err(Error::Denied));
        assert!(mock.said().is_empty() && mock.told().is_empty() && mock.timers().is_empty());
        assert_eq!(mock.denied(), ["chat.say", "chat.say", "world.read", "world.read", "world.read", "event.timer"]);
        // What is not denied works, and settings need no capability.
        assert!(mock.kick(1, "bye").is_ok());
        assert!(mock.settings().is_ok());
        mock.clear();
        assert!(mock.denied().is_empty() && mock.kicked().is_empty());
        // A denied call is not a queued action.
        let limited = MockHost::new().deny(&["player.give"]).with_limits();
        for _ in 0..40 {
            assert_eq!(limited.give(1, "Iron", 1), Err(Error::Denied));
        }
        assert!(limited.say("still room").is_ok());
    }

    #[test]
    fn each_host_call_needs_the_capability_the_reference_names() {
        let host = |cap: &str| MockHost::new().deny(&[cap]);
        let url = HttpRequest { method: "GET", url: "https://a.example/", headers: vec![], body: b"" };
        assert_eq!(host(caps::PLAYER_KICK).kick(1, ""), Err(Error::Denied));
        assert_eq!(host(caps::PLAYER_TELEPORT).teleport(1, [0.0; 3]), Err(Error::Denied));
        assert_eq!(host(caps::RULES_WRITE).rules_patch("{}"), Err(Error::Denied));
        assert_eq!(host(caps::NET_HTTP).http_request(&url), Err(Error::Denied));
        assert_eq!(host(caps::PLAYER_GIVE).give(1, "Iron", 1), Err(Error::Denied));
        assert_eq!(host(caps::WORLD_TOKENS).tokens(5), Err(Error::Denied));
        assert_eq!(host(caps::EVENT_TIMER).cancel_timer(1), Err(Error::Denied));
        assert_eq!(host(caps::WORLD_READ).rules().err(), Some(Error::Denied));
        assert_eq!(host(caps::WORLD_READ).inventory(1).err(), Some(Error::Denied));
        assert_eq!(host(caps::WORLD_READ).object(ObjectRef::World(1)).err(), Some(Error::Denied));
        // Denying one leaves the others alone.
        assert!(host(caps::PLAYER_KICK).teleport(1, [0.0; 3]).is_ok());
    }

    #[test]
    #[should_panic(expected = "not a capability")]
    fn denying_a_name_that_is_no_capability_panics() {
        let _ = MockHost::new().deny(&["chat.sey"]);
    }

    #[test]
    fn nearby_filters_sorts_and_cuts() {
        let mock = MockHost::new()
            .with_object(at([30.0, 0.0, 0.0], 3))
            .with_object(at([10.0, 0.0, 0.0], 1))
            .with_object(at([0.0, 20.0, 0.0], 2))
            .with_object(at([0.0, 0.0, 20.0], 4))
            .with_object(at([300.0, 0.0, 0.0], 5));
        let ids = |found: Vec<Nearby>| found.iter().map(|n| n.object.id).collect::<Vec<_>>();
        let found = mock.nearby([0.0; 3], 100.0, 10).unwrap();
        // Closest first; equal distances by id.
        assert_eq!(ids(found.clone()), [1, 2, 4, 3]);
        assert_eq!(found[0].distance, 10.0);
        assert_eq!(ids(mock.nearby([0.0; 3], 100.0, 2).unwrap()), [1, 2]);
        assert_eq!(ids(mock.nearby([0.0; 3], 25.0, 10).unwrap()), [1, 2, 4]);
        assert_eq!(ids(mock.nearby([290.0, 0.0, 0.0], 15.0, 10).unwrap()), [5]);
        assert!(mock.nearby([0.0; 3], 100.0, 0).unwrap().is_empty());
        // The ceilings: 64 objects, and a radius of at most 1000.
        let many = (0..100).fold(MockHost::new(), |m, i| m.with_object(at([i as f32, 0.0, 0.0], i)));
        assert_eq!(many.nearby([0.0; 3], 500.0, 1000).unwrap().len(), query::MAX_NEARBY as usize);
        let far = MockHost::new().with_object(at([1500.0, 0.0, 0.0], 1)).with_object(at([900.0, 0.0, 0.0], 2));
        assert_eq!(ids(far.nearby([0.0; 3], 50_000.0, 10).unwrap()), [2]);
    }

    #[test]
    fn queries_answer_from_the_mock_world() {
        let mock = MockHost::new()
            .with_object(at([1.0, 2.0, 3.0], -4))
            .with_player(PlayerInfo { client: 7, name: "Ann".into(), ..Default::default() })
            .with_inventory(7, InventoryInfo { slots_used: 1, slots_total: 30, items: vec![query::ItemCount { group: "Iron".into(), count: 5 }] })
            .with_setting("zeta", "1")
            .with_setting("alpha", "2")
            .with_rules(r#"{"speed.growth":2}"#);
        assert_eq!(mock.object(ObjectRef::World(-4)).unwrap().unwrap().position, [1.0, 2.0, 3.0]);
        assert_eq!(mock.object(ObjectRef::Net(496)).unwrap().unwrap().id, -4);
        assert_eq!(mock.object(ObjectRef::World(9)), Ok(None));
        assert_eq!(mock.object(ObjectRef::Net(0)), Ok(None));
        assert_eq!(mock.inventory(7).unwrap().count("Iron"), 5);
        assert_eq!(mock.inventory(8), Err(Error::NotFound));
        assert_eq!(mock.player(7).unwrap().name, "Ann");
        assert_eq!(mock.player(8), Err(Error::NotFound));
        assert_eq!(mock.settings().unwrap(), [("alpha".to_string(), "2".to_string()), ("zeta".to_string(), "1".to_string())]);
        assert_eq!(mock.config("alpha"), Some("2".to_string()));
        assert_eq!(mock.config("nope"), None);
        assert_eq!(mock.rules().unwrap(), r#"{"speed.growth":2}"#);
        assert_eq!(mock.world().unwrap().world_objects, 1);
        assert_eq!(MockHost::new().rules().unwrap(), "{}");
    }

    #[test]
    fn the_world_follows_the_player_list_unless_it_is_set() {
        let mock = MockHost::new();
        assert_eq!(mock.world().unwrap().players, 0);
        mock.add_player(PlayerInfo { client: 1, ..Default::default() });
        mock.add_player(PlayerInfo { client: 2, ..Default::default() });
        mock.add_player(PlayerInfo { client: 2, name: "again".into(), ..Default::default() });
        assert_eq!(mock.world().unwrap().players, 2);
        assert_eq!(mock.players().unwrap().len(), 2);
        mock.remove_player(1);
        assert_eq!(mock.world().unwrap().players, 1);
        let set = MockHost::new().with_world(WorldInfo { players: 12, planet: "Moon".into(), ..WorldInfo::default() });
        assert_eq!((set.world().unwrap().players, set.world().unwrap().planet), (12, "Moon".to_string()));
    }

    #[test]
    fn the_sugar_delivers_events_and_keeps_the_mock_in_step() {
        let mut t = start();
        t.join(3, "Ann");
        t.join_as(4, "Bo", 76561198000000000, "76561198000000000");
        let players = t.mock().players().unwrap();
        assert_eq!((players[0].player_id, players[0].position, players[0].steam_id.as_str()), (1003, [0.0; 3], ""));
        assert_eq!((players[1].player_id, players[1].steam_id.as_str()), (76561198000000000, "76561198000000000"));
        assert_eq!(t.mock().world().unwrap().players, 2);
        t.leave(3);
        assert_eq!(t.mock().world().unwrap().players, 1);
        t.place(-5, "Drill1", [1.0, 0.0, 0.0], 4);
        assert_eq!(t.mock().object(ObjectRef::World(-5)).unwrap().unwrap().group, "Drill1");
        t.destroy(-5, 0);
        assert_eq!(t.mock().object(ObjectRef::World(-5)), Ok(None));
        assert_eq!(t.rpc_before(4, "Move", 9, r#"["evil"]"#), Verdict::Veto);
        assert_eq!(t.rpc_before(4, "Move", 9, "[1]"), Verdict::Continue);
        assert_eq!(t.rpc_after(4, "Move", 9, "[1]"), Verdict::Continue);
        t.command(4, "count", &[]);
        assert_eq!(t.mock().told(), [(4, "1".to_string())]);
    }

    #[test]
    fn events_carry_what_the_server_would_send() {
        struct Spy(Vec<std::string::String>);
        impl Plugin for Spy {
            fn load(_: &dyn Host, _: Option<State<'_>>) -> Self {
                Spy(Vec::new())
            }
            fn event(&mut self, _: &dyn Host, event: &Event<'_>) -> Verdict {
                self.0.push(format!("{event:?}"));
                Verdict::Continue
            }
        }
        let mut t = Harness::<Spy>::start(MockHost::new());
        t.join(3, "Ann");
        t.chat(3, "hej");
        t.command(3, "tp", &["home", "now"]);
        t.command(3, "stats", &[]);
        t.leave(3);
        t.world_saved(true);
        t.flip("0.9.0");
        t.rule_change(r#"{"a":1}"#);
        t.http_done(4, 200, b"ok");
        let seen = &t.plugin().0;
        assert_eq!(seen[0], r#"PlayerJoin { client: 3, name: "Ann", steam_id: "", player_id: 1003 }"#);
        assert_eq!(seen[1], r#"Chat { client: 3, name: "Ann", text: "hej", player_id: 1003 }"#);
        assert_eq!(seen[2], r#"Command { client: 3, name: "Ann", command: "tp", args: ["home", "now"], text: "!tp home now", player_id: 1003 }"#);
        assert!(seen[3].contains(r#"args: [], text: "!stats""#));
        assert_eq!(seen[4], r#"PlayerLeave { client: 3, name: "Ann", player_id: 1003 }"#);
        assert_eq!(seen[5], "WorldSaved { game_ms: 0, ok: true }");
        assert_eq!(seen[6], r#"Flip { game_ms: 0, server_version: "0.9.0" }"#);
        assert_eq!(seen[7], r#"RuleChange { changes_json: "{\"a\":1}" }"#);
        assert!(seen[8].starts_with("HttpDone { ticket: 4, status: 200"));
        assert_eq!(t.mock().rules().unwrap(), r#"{"a":1}"#);
    }

    #[test]
    fn actions_are_recorded_in_full() {
        let mock = MockHost::new();
        mock.say("all").unwrap();
        mock.tell(2, "you").unwrap();
        mock.kick(3, "afk").unwrap();
        mock.teleport(4, [1.0, 2.0, 3.0]).unwrap();
        mock.rules_patch(r#"{"a":1}"#).unwrap();
        mock.give(5, "Iron", 3).unwrap();
        mock.tokens(10).unwrap();
        mock.tokens(-4).unwrap();
        mock.log(Level::Warn, "careful");
        let first = mock.http_request(&HttpRequest { method: "POST", url: "https://hook.example/${config.h}", headers: vec![("Content-Type", "application/json")], body: b"{}" }).unwrap();
        let second = mock.http_request(&HttpRequest { method: "GET", url: "https://a.example/", headers: vec![], body: b"" }).unwrap();
        assert_eq!((first, second), (1, 2));
        assert_eq!(mock.said(), ["all"]);
        assert_eq!(mock.told(), [(2, "you".to_string())]);
        assert_eq!(mock.kicked(), [(3, "afk".to_string())]);
        assert_eq!(mock.teleported(), [(4, [1.0, 2.0, 3.0])]);
        assert_eq!(mock.rules_patches(), [r#"{"a":1}"#]);
        assert_eq!(mock.given(), [(5, "Iron".to_string(), 3)]);
        assert_eq!(mock.tokens_total(), 6);
        assert_eq!(mock.logs(), [(Level::Warn, "careful".to_string())]);
        let http = mock.http_requests();
        assert_eq!(http[0], HttpCall { method: "POST".into(), url: "https://hook.example/${config.h}".into(), headers: vec![("Content-Type".into(), "application/json".into())], body: b"{}".to_vec(), ticket: 1 });
        mock.clear();
        assert!(mock.said().is_empty() && mock.http_requests().is_empty() && mock.logs().is_empty());
        assert_eq!(mock.tokens_total(), 0);
        // Tickets go on counting after a clear.
        assert_eq!(mock.http_request(&HttpRequest { method: "GET", url: "https://a.example/", headers: vec![], body: b"" }), Ok(3));
    }

    #[test]
    fn an_http_answer_comes_back_by_ticket() {
        let mut t = start();
        let ticket = t.mock().http_request(&HttpRequest { method: "GET", url: "https://a.example/", headers: vec![], body: b"" }).unwrap();
        t.http_done(ticket, 204, b"");
        assert_eq!(t.plugin().http_done, [(ticket, 204)]);
    }

    #[test]
    fn plugin_and_clear_are_reachable_from_the_harness() {
        let mut t = start();
        t.plugin_mut().count = 41;
        t.join(1, "Ann");
        t.command(1, "count", &[]);
        assert_eq!(t.mock().told(), [(1, "42".to_string())]);
        t.clear();
        assert!(t.mock().told().is_empty());
        assert_eq!(t.mock().logs().len(), 0);
    }

    #[test]
    fn the_host_s_bounds_on_give_tokens_timers_and_text_are_checked() {
        let mock = MockHost::new();
        assert_eq!(mock.give(1, "Iron", 0), Err(Error::Bad));
        assert_eq!(mock.give(1, "Iron", 51), Err(Error::Bad));
        assert_eq!(mock.give(1, "", 1), Err(Error::Bad));
        assert_eq!(mock.give(1, "Iron Ore", 1), Err(Error::Bad));
        assert_eq!(mock.give(1, "Iron", 50), Ok(()));
        assert_eq!(mock.tokens(0), Err(Error::Bad));
        assert_eq!(mock.tokens(10_001), Err(Error::Bad));
        assert_eq!(mock.tokens(-10_001), Err(Error::Bad));
        assert_eq!(mock.tokens(-10_000), Ok(()));
        assert_eq!(mock.set_timer(1, 0, 99), Err(Error::Bad));
        assert_eq!(mock.set_timer(1, 604_800_001, 0), Err(Error::Bad));
        assert_eq!(mock.set_timer(1, 0, 604_800_001), Err(Error::Bad));
        assert_eq!(mock.set_timer(1, 604_800_000, 100), Ok(()));
        for id in 2..=16 {
            mock.set_timer(id, 1000, 0).unwrap();
        }
        assert_eq!(mock.set_timer(17, 1000, 0), Err(Error::Full));
        // Replacing one of the 16 is fine.
        assert_eq!(mock.set_timer(5, 1, 0), Ok(()));
        assert_eq!(mock.timers().len(), 16);
        assert_eq!(mock.say(" \u{7} "), Err(Error::Bad));
        assert_eq!(mock.teleport(1, [f32::NAN, 0.0, 0.0]), Err(Error::Bad));
        assert_eq!(mock.teleport(1, [100_001.0, 0.0, 0.0]), Err(Error::Bad));
        assert_eq!(mock.rules_patch("[1]"), Err(Error::Bad));
        assert_eq!(mock.rules_patch("nope"), Err(Error::Bad));
        assert_eq!(mock.rules_patch(r#"{"a":1}"#), Ok(()));
        // Text is cleaned and cut as the host does; an empty kick reason becomes "Kicked".
        mock.say(&format!("  hi\u{7}\n{}", "x".repeat(600))).unwrap();
        // 500 characters are kept, then the leading spaces are trimmed.
        assert_eq!(mock.said()[0].chars().count(), 498);
        assert!(mock.said()[0].starts_with("hi\nx"));
        mock.kick(1, "").unwrap();
        assert_eq!(mock.kicks_of(1), ["Kicked"]);
        assert_eq!(mock.given(), [(1, "Iron".to_string(), 50)]);
        assert_eq!(mock.tokens_total(), -10_000);
    }

    #[test]
    fn the_hosts_rates_apply_with_limits_in_game_seconds() {
        let mut t = Harness::<Probe>::start(MockHost::new().with_limits()).tick_every_ms(0);
        let m = t.mock();
        assert_eq!((m.tokens(1), m.tokens(1), m.tokens(1)), (Ok(()), Ok(()), Err(Error::Full)));
        assert_eq!((m.rules_patch("{}"), m.rules_patch("{}"), m.rules_patch("{}")), (Ok(()), Ok(()), Err(Error::Full)));
        m.begin_call();
        for _ in 0..10 {
            m.give(1, "Iron", 1).unwrap();
        }
        assert_eq!(m.give(1, "Iron", 1), Err(Error::Full));
        t.advance_ms(1000);
        let m = t.mock();
        assert_eq!((m.tokens(1), m.give(1, "Iron", 1)), (Ok(()), Ok(())));
        // Two requests alive at most, until one is answered.
        let get = HttpRequest { method: "GET", url: "https://a.example/", headers: vec![], body: b"" };
        assert_eq!((m.http_request(&get), m.http_request(&get), m.http_request(&get)), (Ok(1), Ok(2), Err(Error::Full)));
        t.http_done(1, 200, b"");
        assert_eq!(t.mock().http_request(&get), Ok(3));
        let bad = HttpRequest { method: "FETCH", ..get.clone() };
        assert_eq!(t.mock().http_request(&bad), Err(Error::Bad));
    }

    #[test]
    fn the_mock_world_can_be_changed_while_the_plugin_runs() {
        let t = start();
        t.mock().add_player(PlayerInfo { client: 1, name: "Ann".into(), ..Default::default() });
        t.mock().set_position(1, [1.0, 2.0, 3.0]);
        t.mock().set_operator(1, true);
        t.mock().set_position(9, [0.0; 3]);
        let p = t.mock().player(1).unwrap();
        assert_eq!((p.position, p.operator), ([1.0, 2.0, 3.0], true));
        t.mock().set_world(|w| {
            w.terraformation = 99.5;
            w.standing_still = true;
            w.planet = "Moon".into();
            w.game_ms = 123_456;
        });
        let w = t.mock().world().unwrap();
        assert_eq!((w.terraformation, w.standing_still, w.planet.as_str(), w.game_ms, w.command_prefix.as_str()), (99.5, true, "Moon", 0, "!"));
    }

    #[test]
    fn place_net_gives_the_object_a_network_id() {
        let mut t = start();
        t.place_net(-5, "Drill1", [1.0, 0.0, 0.0], 777, 3);
        assert_eq!(t.mock().object(ObjectRef::Net(777)).unwrap().unwrap().id, -5);
        t.place(-6, "Drill1", [1.0, 0.0, 0.0], 3);
        assert_eq!(t.mock().object(ObjectRef::World(-6)).unwrap().unwrap().net, 0);
    }

    #[test]
    fn a_timer_can_be_fired_by_hand_and_one_set_in_load_starts_at_the_first_step() {
        let mut t = start().tick_every_ms(0);
        t.command(1, "arm", &["1", "5000", "0"]);
        t.command(1, "arm", &["2", "5000", "1000"]);
        t.advance_ms(100);
        t.fire_timer(1);
        t.fire_timer(2);
        assert_eq!(t.plugin().timers, [(1, 100, 0), (2, 100, 0)]);
        // The one-shot is gone; the repeating one is due again a repeat later.
        assert_eq!(t.mock().timers().iter().map(|x| (x.id, x.due_ms)).collect::<Vec<_>>(), [(2, 1100)]);
        // A timer set in load counts from the first step, not from the load.
        struct Early;
        impl Plugin for Early {
            fn load(host: &dyn Host, _: Option<State<'_>>) -> Self {
                host.set_timer(1, 200, 0).unwrap();
                Early
            }
            fn event(&mut self, host: &dyn Host, event: &Event<'_>) -> Verdict {
                if let Event::Timer { id, game_ms, .. } = event {
                    host.log(Level::Info, &format!("timer {id} at {game_ms}"));
                }
                Verdict::Continue
            }
        }
        let mock = MockHost::new();
        mock.advance_clock(1000);
        let mut e = Harness::<Early>::start(mock).tick_every_ms(0);
        e.advance_ms(150);
        assert!(e.mock().logs().is_empty());
        e.advance_ms(100);
        // Counted from the first step (1050): due at 1250.
        assert_eq!(e.mock().logs(), [(Level::Info, "timer 1 at 1250".to_string())]);
    }

    #[test]
    #[should_panic(expected = "no timer 5 is set")]
    fn firing_a_timer_that_is_not_set_panics() {
        start().fire_timer(5);
    }

    #[test]
    fn at_most_four_timers_come_in_a_step_in_the_order_set() {
        let mut t = start().tick_every_ms(0);
        for id in [9, 3, 7, 1, 5, 2] {
            t.mock().set_timer(id, 10, 0).unwrap();
        }
        t.advance_ms(50);
        assert_eq!(t.plugin().timers.iter().map(|x| x.0).collect::<Vec<_>>(), [9, 3, 7, 1]);
        t.advance_ms(50);
        assert_eq!(t.plugin().timers.iter().map(|x| x.0).collect::<Vec<_>>(), [9, 3, 7, 1, 5, 2]);
        assert_eq!(t.plugin().timers[4].2, 90);
    }

    #[test]
    fn subscribe_holds_back_the_kinds_not_listed() {
        let mut t = start().subscribe(&[Kind::PlayerJoin]).tick_every_ms(100);
        t.join(1, "Ann");
        t.chat(1, "hi");
        t.advance_ms(300);
        t.rpc_before(1, "x", 1, "[]");
        t.place(1, "Drill1", [0.0; 3], 0);
        assert!(t.plugin().ticks.is_empty());
        // Timers, commands and HTTP answers always pass.
        t.command(1, "count", &[]);
        t.mock().set_timer(1, 10, 0).unwrap();
        t.advance_ms(50);
        t.http_done(4, 200, b"");
        assert_eq!(t.mock().told(), [(1, "1".to_string())]);
        assert_eq!(t.plugin().timers.len(), 1);
        assert_eq!(t.plugin().http_done, [(4, 200)]);
    }

    #[test]
    fn a_kicked_player_leaves_after_the_call() {
        struct Bouncer(Vec<std::string::String>);
        impl Plugin for Bouncer {
            fn load(_: &dyn Host, _: Option<State<'_>>) -> Self {
                Bouncer(Vec::new())
            }
            fn event(&mut self, host: &dyn Host, event: &Event<'_>) -> Verdict {
                match event {
                    Event::Chat { client, text: "bye", .. } => {
                        host.kick(*client, "asked for it").unwrap();
                        // Still there during the call.
                        self.0.push(format!("online {}", host.players().unwrap().len()));
                    }
                    Event::PlayerLeave { name, player_id, .. } => self.0.push(format!("left {name} {player_id}")),
                    _ => {}
                }
                Verdict::Continue
            }
        }
        let mut t = Harness::<Bouncer>::start(MockHost::new());
        t.join(1, "Ann");
        t.join(2, "Bo");
        t.chat(1, "bye");
        assert_eq!(t.plugin().0, ["online 2", "left Ann 1001"]);
        assert_eq!(t.mock().kicks_of(1), ["asked for it"]);
        assert_eq!(t.mock().players().unwrap().len(), 1);
        assert_eq!(t.mock().inventory(1), Err(Error::NotFound));
        // A kick of someone who is not there changes nothing.
        t.mock().kick(77, "").unwrap();
        t.chat(2, "hello");
        assert_eq!(t.plugin().0.len(), 2);
    }

    #[test]
    fn a_declared_command_is_refused_like_the_host_does() {
        let mut t = start();
        t.join(1, "Ann");
        t.join(2, "Op");
        t.mock().set_operator(2, true);
        t.declare_command("Count", 0, 1, false);
        t.declare_command("ban", 1, u32::MAX, true);
        // Too many arguments: the player is told, the plugin is not called.
        t.command(1, "count", &["a", "b"]);
        assert_eq!(t.mock().told_to(1), ["Use: !count"]);
        assert_eq!(t.plugin().count, 0);
        // The right number, whatever the case: delivered, in lower case.
        t.command(1, "COUNT", &["a"]);
        assert_eq!(t.plugin().count, 1);
        assert_eq!(t.mock().told_to(1), ["Use: !count", "1"]);
        t.command(1, "ban", &["x"]);
        assert_eq!(t.mock().told_to(1)[2], "!ban is for the server's operators.");
        t.command(2, "ban", &[]);
        assert_eq!(t.mock().told_to(2), ["Use: !ban"]);
        // A command nobody declared is delivered as it is.
        t.command(1, "count", &[]);
        assert_eq!(t.plugin().count, 2);
    }

    #[test]
    fn the_command_event_carries_a_lower_case_name_and_the_typed_text() {
        struct Spy(Vec<(std::string::String, std::string::String)>);
        impl Plugin for Spy {
            fn load(_: &dyn Host, _: Option<State<'_>>) -> Self {
                Spy(Vec::new())
            }
            fn event(&mut self, _: &dyn Host, event: &Event<'_>) -> Verdict {
                if let Event::Command { command, text, .. } = event {
                    self.0.push((command.to_string(), text.to_string()));
                }
                Verdict::Continue
            }
        }
        let mock = MockHost::new();
        mock.set_world(|w| w.command_prefix = "?".into());
        let mut t = Harness::<Spy>::start(mock);
        t.command(1, "Stats", &["X"]);
        assert_eq!(t.plugin().0, [("stats".to_string(), "?Stats X".to_string())]);
    }

    #[test]
    fn a_joined_player_has_an_empty_backpack_that_exists() {
        let mut t = start();
        assert_eq!(t.mock().inventory(1), Err(Error::NotFound));
        t.join(1, "Ann");
        let inventory = t.mock().inventory(1).unwrap();
        assert_eq!((inventory.slots_used, inventory.slots_total, inventory.items.len()), (0, 30, 0));
        t.leave(1);
        assert_eq!(t.mock().inventory(1), Err(Error::NotFound));
    }

    #[test]
    fn the_readers_filter_by_player() {
        let mock = MockHost::new();
        mock.tell(1, "a").unwrap();
        mock.tell(2, "b").unwrap();
        mock.tell(1, "c").unwrap();
        mock.give(1, "Iron", 2).unwrap();
        mock.give(2, "Ice", 3).unwrap();
        mock.teleport(2, [1.0, 2.0, 3.0]).unwrap();
        mock.kick(2, "x").unwrap();
        assert_eq!(mock.told_to(1), ["a", "c"]);
        assert_eq!(mock.given_to(2), [("Ice".to_string(), 3)]);
        assert_eq!(mock.teleports_of(2), [[1.0, 2.0, 3.0]]);
        assert_eq!(mock.teleports_of(1), Vec::<[f32; 3]>::new());
        assert_eq!(mock.kicks_of(2), ["x"]);
    }

    #[test]
    fn an_http_answer_can_carry_headers_and_an_error() {
        struct Echo(Vec<std::string::String>);
        impl Plugin for Echo {
            fn load(_: &dyn Host, _: Option<State<'_>>) -> Self {
                Echo(Vec::new())
            }
            fn event(&mut self, _: &dyn Host, event: &Event<'_>) -> Verdict {
                if let Event::HttpDone { status, error, headers, body, .. } = event {
                    self.0.push(format!("{status} {error:?} {headers:?} {}", body.len()));
                }
                Verdict::Continue
            }
        }
        let mut t = Harness::<Echo>::start(MockHost::new());
        t.http_done_with(1, 200, "", &[("etag", "x")], b"abc");
        t.http_done_with(2, 0, "timed out", &[], b"");
        assert_eq!(t.plugin().0, [r#"200 "" [("etag", "x")] 3"#, r#"0 "timed out" [] 0"#]);
    }

    #[test]
    fn the_error_reads_in_plain_words() {
        assert_eq!(Error::Denied.to_string(), "denied: the capability is not granted");
        assert!(Error::NotFound.to_string().starts_with("not found"));
        let boxed: std::boxed::Box<dyn std::error::Error> = std::boxed::Box::new(Error::Full);
        assert!(boxed.to_string().starts_with("full"));
    }
}
