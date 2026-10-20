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
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::cell::{RefCell, RefMut};

use crate::caps;
use crate::event::{Event, Verdict};
use crate::host::{Error, Host, Level};
use crate::http::HttpRequest;
use crate::query::{self, InventoryInfo, Nearby, ObjectInfo, ObjectRef, PlayerInfo, WorldInfo};
use crate::{Plugin, SavedState, State};

/// The most actions a call may queue on the real host. `MockHost::with_limits` enforces it.
pub const ACTIONS_PER_CALL: u32 = 16;

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
    /// The URL as the plugin wrote it (`${secret.NAME}` is not replaced).
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
    rec: Records,
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
                world: WorldInfo { planet: "Prime".to_string(), ..WorldInfo::default() },
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
                rec: Records::default(),
            }),
        }
    }

    // ---- Setting up the world.

    /// Sets the world's numbers. `game_ms` and `unix_ms` start the clock (a zero `unix_ms` keeps
    /// the current one). A zero `players` or `world_objects` is filled in from the mock's lists.
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

    /// Turns on the real host's limit: a call queues at most 16 actions (say, tell, kick,
    /// teleport, rules_patch, http_request, give, tokens), then gets `Err(Error::Full)`. The
    /// count starts again at `begin_call`, which `Harness` calls before every event.
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

    /// Takes the borrow for a call that needs `cap`: refuses it when the capability is denied,
    /// and, for an action, when this call already queued 16 (with limits on).
    fn gate(&self, cap: &str, action: bool) -> Result<RefMut<'_, Inner>, Error> {
        let mut s = self.inner.borrow_mut();
        if s.deny.iter().any(|c| c == cap) {
            s.rec.denied.push(cap.to_string());
            return Err(Error::Denied);
        }
        if action && s.limits {
            if s.queued >= ACTIONS_PER_CALL {
                return Err(Error::Full);
            }
            s.queued += 1;
        }
        Ok(s)
    }

    fn player_count(&self) -> u32 {
        self.inner.borrow().players.len() as u32
    }

    fn player(&self, client: u64) -> Option<PlayerInfo> {
        self.inner.borrow().players.iter().find(|p| p.client == client).cloned()
    }

    fn object_info(&self, id: i32) -> Option<ObjectInfo> {
        self.inner.borrow().objects.iter().find(|o| o.id == id).cloned()
    }

    fn set_rules(&self, json: &str) {
        self.inner.borrow_mut().rules = json.to_string();
    }

    fn clear_timers(&self) {
        self.inner.borrow_mut().timers.clear();
    }

    /// Takes the timers that are due: (id, how late). A repeating timer is set for its next time
    /// (a time already past is skipped); a one-shot timer is gone.
    fn take_due_timers(&self) -> Vec<(u32, u32)> {
        let mut s = self.inner.borrow_mut();
        let now = s.game_ms;
        let (mut due, mut keep) = (Vec::new(), Vec::new());
        for mut t in core::mem::take(&mut s.timers) {
            if t.due_ms > now {
                keep.push(t);
                continue;
            }
            due.push((t.due_ms, t.id));
            if t.repeat_ms > 0 {
                let r = t.repeat_ms as i64;
                t.due_ms += r;
                if t.due_ms <= now {
                    t.due_ms += (now - t.due_ms) / r * r + r;
                }
                keep.push(t);
            }
        }
        s.timers = keep;
        due.sort();
        due.into_iter().map(|(at, id)| (id, (now - at) as u32)).collect()
    }
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
        let s = self.gate(caps::WORLD_READ, false)?;
        let mut world = s.world.clone();
        world.game_ms = s.game_ms;
        world.unix_ms = s.unix_offset + s.game_ms;
        if world.players == 0 {
            world.players = s.players.len() as u32;
        }
        if world.world_objects == 0 {
            world.world_objects = s.objects.len() as u32;
        }
        Ok(world)
    }

    fn players(&self) -> Result<Vec<PlayerInfo>, Error> {
        Ok(self.gate(caps::WORLD_READ, false)?.players.clone())
    }

    fn rules(&self) -> Result<String, Error> {
        Ok(self.gate(caps::WORLD_READ, false)?.rules.clone())
    }

    fn say(&self, text: &str) -> Result<(), Error> {
        self.gate(caps::CHAT_SAY, true)?.rec.said.push(text.to_string());
        Ok(())
    }

    fn tell(&self, client: u64, text: &str) -> Result<(), Error> {
        self.gate(caps::CHAT_SAY, true)?.rec.told.push((client, text.to_string()));
        Ok(())
    }

    fn kick(&self, client: u64, reason: &str) -> Result<(), Error> {
        self.gate(caps::PLAYER_KICK, true)?.rec.kicked.push((client, reason.to_string()));
        Ok(())
    }

    fn teleport(&self, client: u64, position: [f32; 3]) -> Result<(), Error> {
        self.gate(caps::PLAYER_TELEPORT, true)?.rec.teleported.push((client, position));
        Ok(())
    }

    fn rules_patch(&self, json: &str) -> Result<(), Error> {
        self.gate(caps::RULES_WRITE, true)?.rec.patches.push(json.to_string());
        Ok(())
    }

    fn http_request(&self, request: &HttpRequest<'_>) -> Result<u64, Error> {
        let mut s = self.gate(caps::NET_HTTP, true)?;
        let ticket = s.next_ticket;
        s.next_ticket += 1;
        s.rec.http.push(HttpCall {
            method: request.method.to_string(),
            url: request.url.to_string(),
            headers: request.headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            body: request.body.to_vec(),
            ticket,
        });
        Ok(ticket)
    }

    fn inventory(&self, client: u64) -> Result<InventoryInfo, Error> {
        let s = self.gate(caps::WORLD_READ, false)?;
        s.inventories.iter().find(|(c, _)| *c == client).map(|(_, i)| i.clone()).ok_or(Error::NotFound)
    }

    fn nearby(&self, center: [f32; 3], radius: f32, max: u32) -> Result<Vec<Nearby>, Error> {
        let s = self.gate(caps::WORLD_READ, false)?;
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
        let s = self.gate(caps::WORLD_READ, false)?;
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
        self.gate(caps::PLAYER_GIVE, true)?.rec.given.push((client, group.to_string(), count));
        Ok(())
    }

    fn tokens(&self, amount: i32) -> Result<(), Error> {
        self.gate(caps::WORLD_TOKENS, true)?.rec.tokens += amount as i64;
        Ok(())
    }

    fn set_timer(&self, id: u32, delay_ms: u32, repeat_ms: u32) -> Result<(), Error> {
        let mut s = self.gate(caps::EVENT_TIMER, false)?;
        s.timers.retain(|t| t.id != id);
        let due_ms = s.game_ms + delay_ms as i64;
        s.timers.push(Timer { id, delay_ms, repeat_ms, due_ms });
        Ok(())
    }

    fn cancel_timer(&self, id: u32) -> Result<(), Error> {
        self.gate(caps::EVENT_TIMER, false)?.timers.retain(|t| t.id != id);
        Ok(())
    }
}

/// Your plugin and a `MockHost`, with sugar to deliver events the way the server does.
///
/// The `join`, `leave`, `place` and `destroy` calls also keep the mock's player and object lists
/// in step. Every event is one call: the mock's action count starts again before it. Records stay
/// in the mock until `clear`.
pub struct Harness<P: Plugin> {
    mock: MockHost,
    plugin: P,
    tick_every_ms: u32,
    step_ms: u32,
    last_tick_ms: i64,
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
        let plugin = P::load(&mock, state);
        let last_tick_ms = mock.game_ms();
        Harness { mock, plugin, tick_every_ms: 1000, step_ms: 50, last_tick_ms }
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

    /// Delivers any event as one call, and returns the plugin's verdict.
    pub fn event(&mut self, event: &Event<'_>) -> Verdict {
        self.mock.begin_call();
        self.plugin.event(&self.mock, event)
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
        let who = self.mock.player(client).unwrap_or_else(|| PlayerInfo { client, player_id: client + 1000, ..PlayerInfo::default() });
        self.mock.remove_player(client);
        self.event(&Event::PlayerLeave { client, name: &who.name, player_id: who.player_id })
    }

    /// A player says something: delivers `Event::Chat`. The player should have joined.
    pub fn chat(&mut self, client: u64, text: &str) -> Verdict {
        let who = self.mock.player(client).unwrap_or_default();
        self.event(&Event::Chat { client, name: &who.name, text, player_id: who.player_id })
    }

    /// A player uses a chat command: delivers `Event::Command` with `text` as the player typed
    /// it, `!name arg ...`. The player should have joined.
    pub fn command(&mut self, client: u64, name: &str, args: &[&str]) -> Verdict {
        let who = self.mock.player(client).unwrap_or_default();
        let mut text = alloc::format!("!{name}");
        for arg in args {
            text.push(' ');
            text.push_str(arg);
        }
        self.event(&Event::Command { client, name: &who.name, command: name, args: args.to_vec(), text: &text, player_id: who.player_id })
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
    /// `Event::ObjectPlaced`.
    pub fn place(&mut self, id: i32, group: &str, position: [f32; 3], client: u64) -> Verdict {
        self.mock.add_object(ObjectInfo { id, net: 0, group: group.to_string(), position });
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
    /// the ticket from `MockHost::http_requests`. A `status` of 0 means no answer.
    pub fn http_done(&mut self, ticket: u64, status: u32, body: &[u8]) -> Verdict {
        self.event(&Event::HttpDone { ticket, status, error: "", body, headers: Vec::new() })
    }

    // ---- Time.

    /// Lets `ms` of game time pass, one step at a time (`tick_step_ms`). At the end of each step it
    /// delivers a `Tick` if `tick_every_ms` has passed since the last one, then a `Timer` for each
    /// timer that is due (oldest first). A repeating timer is called at most once per step; a
    /// repeat that fell inside a step is skipped. A timer set while these are delivered waits for
    /// the next step.
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
            for (id, late_ms) in self.mock.take_due_timers() {
                self.event(&Event::Timer { id, game_ms: now, late_ms });
            }
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
        let state = saved.as_ref().map(|s| State { version: s.version, bytes: &s.bytes });
        self.plugin = P::load(&self.mock, state);
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
        let mut t = start().tick_every_ms(0);
        t.command(1, "arm", &["1", "10", "10"]);
        t.advance_ms(200);
        assert_eq!(t.plugin().timers.len(), 4);
        // Due at 10, 20, ..., delivered at 50 (40 late), then the next one after that: 60, at 100.
        assert_eq!(t.plugin().timers[1], (1, 100, 40));
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
            mock.tokens(1).unwrap();
        }
        assert_eq!(mock.give(1, "Iron", 1), Err(Error::Full));
        mock.begin_call();
        assert_eq!(mock.give(1, "Iron", 1), Ok(()));
        assert_eq!(mock.tokens_total(), 16);
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
        let first = mock.http_request(&HttpRequest { method: "POST", url: "https://hook.example/${secret.h}", headers: vec![("Content-Type", "application/json")], body: b"{}" }).unwrap();
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
        assert_eq!(http[0], HttpCall { method: "POST".into(), url: "https://hook.example/${secret.h}".into(), headers: vec![("Content-Type".into(), "application/json".into())], body: b"{}".to_vec(), ticket: 1 });
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
}
