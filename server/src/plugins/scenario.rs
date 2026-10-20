//! `pcds plugin test`: a plugin run in the real host against a made-up world, driven by a scenario
//! file, with no game and no game tables (so it runs anywhere: a laptop, CI).
//!
//! The host is the one the server runs (budgets, capabilities, commands, timers, state, faults);
//! only the world is a stand-in (`Sim`): some players with places and backpacks, some objects, a clock.
//! What the plugin asks for is applied to the stand-in the way the game would (a kick removes the
//! player, a give fills the backpack), written down, and checked against what the scenario expects.
//! HTTP requests are never sent: they are listed, and the scenario hands back an answer
//! (`http_done`) if the plugin should hear one.
//!
//! ```toml
//! name = "welcome"
//! [config]                       # settings, checked against the manifest's [[config]]
//! greeting = "Hi"
//!
//! [[player]]                     # who is online from the start (more can `join`)
//! client = 1
//! name = "Ann"
//!
//! [[step]]
//! join = { client = 2, name = "Bo" }
//! expect = [{ tell = 2, contains = "Welcome" }]   # what this step makes the plugin do
//!
//! [[step]]
//! advance = "10m"                # game time passes: ticks and timers
//!
//! [[step]]
//! chat = { client = 1, text = "!top" }
//! expect = [{ tell = 1, contains = "1." }]
//! ```
//!
//! A step does one thing: `join`, `leave`, `chat`, `advance`, `place`, `destroy`, `rpc`,
//! `world_saved`, `http_done`, `reload` (the state goes through the plugin's `save` and a new instance
//! `load`s it: timers are gone), `flip` (a reload that also tells the plugin it was hot-updated),
//! `set` (change the stand-in world: positions, backpacks, the clock). `expect` entries name a kind
//! (`say`, `tell = CLIENT`, `kick = CLIENT`, `teleport = CLIENT`, `give = CLIENT`, `tokens`,
//! `rules_patch`, `http`, `veto`, `log`) and, optionally, `contains`, `equals`, `count` (how many
//! times; 0 means never; the default is at least once).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use pcds_plugin_api::query::{InventoryInfo, ItemCount, Nearby, ObjectInfo, ObjectRef, PlayerInfo, WorldInfo};
use pcds_plugin_api::{Event, Kind, Level, Verdict};
use serde::Deserialize;
use serde_json::Value;

use super::ctx::{Action, World};
use super::manifest;
use super::state::Saved;
use super::{Applied, Host, Sink};

// ---- the file ------------------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scenario {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub world: WorldSpec,
    #[serde(default)]
    pub config: BTreeMap<String, toml::Value>,
    #[serde(default, rename = "player")]
    pub players: Vec<PlayerSpec>,
    #[serde(default, rename = "object")]
    pub objects: Vec<ObjectSpec>,
    #[serde(default, rename = "step")]
    pub steps: Vec<Step>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorldSpec {
    #[serde(default = "prime")]
    pub planet: String,
    /// Wall clock at the start, Unix milliseconds.
    #[serde(default = "epoch")]
    pub unix_ms: i64,
    /// Game time at the start, milliseconds.
    #[serde(default)]
    pub game_ms: i64,
    /// Slots in a backpack (a `give` beyond it is refused).
    #[serde(default = "slots")]
    pub backpack_slots: u32,
    /// The rules that differ from the defaults, as `Host::rules()` shows them.
    #[serde(default)]
    pub rules: BTreeMap<String, toml::Value>,
}

impl Default for WorldSpec {
    fn default() -> Self {
        WorldSpec { planet: prime(), unix_ms: epoch(), game_ms: 0, backpack_slots: slots(), rules: BTreeMap::new() }
    }
}

fn prime() -> String {
    "Prime".into()
}
fn epoch() -> i64 {
    1_760_000_000_000
}
fn slots() -> u32 {
    30
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlayerSpec {
    pub client: u64,
    pub name: String,
    /// Default: `client + 1000`.
    pub player_id: Option<u64>,
    #[serde(default)]
    pub steam_id: String,
    #[serde(default)]
    pub position: [f32; 3],
    #[serde(default)]
    pub operator: bool,
    /// Item group to count.
    #[serde(default)]
    pub inventory: BTreeMap<String, u32>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObjectSpec {
    pub id: i32,
    pub group: String,
    pub position: [f32; 3],
    /// The network object's id; default: the id as a positive number.
    pub net: Option<u64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    /// A word for the transcript.
    pub note: Option<String>,
    pub join: Option<PlayerSpec>,
    pub leave: Option<u64>,
    pub chat: Option<ChatSpec>,
    /// Game time passes: "500ms", "90s", "10m", "2h".
    pub advance: Option<String>,
    pub place: Option<ObjectEvent>,
    pub destroy: Option<ObjectEvent>,
    pub rpc: Option<RpcSpec>,
    pub world_saved: Option<bool>,
    pub http_done: Option<HttpDoneSpec>,
    pub reload: Option<bool>,
    pub flip: Option<bool>,
    pub set: Option<SetSpec>,
    #[serde(default)]
    pub expect: Vec<Expect>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChatSpec {
    pub client: u64,
    pub text: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObjectEvent {
    pub id: i32,
    #[serde(default)]
    pub group: String,
    #[serde(default)]
    pub position: [f32; 3],
    /// Who did it (0: nobody).
    #[serde(default)]
    pub client: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RpcSpec {
    pub client: u64,
    pub name: String,
    #[serde(default)]
    pub object: u64,
    /// The arguments, JSON text.
    #[serde(default = "no_args")]
    pub args: String,
}

fn no_args() -> String {
    "[]".into()
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpDoneSpec {
    /// The ticket `http_request` returned (tickets count from 1 in a test).
    pub ticket: u64,
    #[serde(default = "ok_status")]
    pub status: u32,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub error: String,
}

fn ok_status() -> u32 {
    200
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetSpec {
    pub unix_ms: Option<i64>,
    /// Move a player: client number and position.
    pub position: Option<(u64, [f32; 3])>,
    /// Set what a player carries: client number and group to count.
    pub inventory: Option<(u64, BTreeMap<String, u32>)>,
    pub operator: Option<(u64, bool)>,
    pub object: Option<ObjectSpec>,
    pub remove_object: Option<i32>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Expect {
    pub say: Option<bool>,
    pub tell: Option<u64>,
    pub kick: Option<u64>,
    pub teleport: Option<u64>,
    pub give: Option<u64>,
    pub tokens: Option<bool>,
    pub rules_patch: Option<bool>,
    pub http: Option<bool>,
    pub veto: Option<bool>,
    pub log: Option<bool>,
    pub contains: Option<String>,
    pub equals: Option<String>,
    pub count: Option<usize>,
}

impl Scenario {
    pub fn parse(text: &str) -> Result<Scenario, String> {
        let s: Scenario = toml::from_str(text).map_err(|e| super::clean(&manifest::improve(e.to_string())))?;
        for (i, step) in s.steps.iter().enumerate() {
            let n = [step.join.is_some(), step.leave.is_some(), step.chat.is_some(), step.advance.is_some(), step.place.is_some(), step.destroy.is_some(), step.rpc.is_some(), step.world_saved.is_some(), step.http_done.is_some(), step.reload.is_some(), step.flip.is_some(), step.set.is_some()].iter().filter(|b| **b).count();
            if n != 1 {
                return Err(format!("step {}: a step does exactly one thing (join, leave, chat, advance, place, destroy, rpc, world_saved, http_done, reload, flip or set); this one has {n}", i + 1));
            }
            if let Some(a) = &step.advance {
                duration_ms(a).map_err(|e| format!("step {}: {e}", i + 1))?;
            }
            for e in &step.expect {
                if e.kinds() != 1 {
                    return Err(format!("step {}: an expect names exactly one of say, tell, kick, teleport, give, tokens, rules_patch, http, veto, log", i + 1));
                }
            }
        }
        Ok(s)
    }
}

impl Expect {
    fn kinds(&self) -> usize {
        [self.say.is_some(), self.tell.is_some(), self.kick.is_some(), self.teleport.is_some(), self.give.is_some(), self.tokens.is_some(), self.rules_patch.is_some(), self.http.is_some(), self.veto.is_some(), self.log.is_some()].iter().filter(|b| **b).count()
    }
}

/// "500ms", "90s", "10m", "2h", "1d" as milliseconds.
pub fn duration_ms(text: &str) -> Result<i64, String> {
    let t = text.trim();
    let split = t.find(|c: char| !c.is_ascii_digit()).ok_or_else(|| format!("{text:?}: a time needs its unit (ms, s, m, h, d), as 90s"))?;
    let (n, unit) = t.split_at(split);
    let n: i64 = n.parse().map_err(|_| format!("{text:?}: not a time (as 90s, 10m, 2h)"))?;
    let per = match unit {
        "ms" => 1,
        "s" => 1000,
        "m" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        _ => return Err(format!("{text:?}: the unit is ms, s, m, h or d")),
    };
    n.checked_mul(per).filter(|ms| *ms <= 30 * 86_400_000).ok_or_else(|| format!("{text:?}: at most 30 days"))
}

// ---- the stand-in world --------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct SimPlayer {
    client: u64,
    name: String,
    player_id: u64,
    steam_id: String,
    position: [f32; 3],
    operator: bool,
    inventory: BTreeMap<String, u32>,
}

/// What the plugin has made the world do, in order: the transcript's lines and the expectations' food.
#[derive(Debug, Clone, PartialEq)]
pub enum Done {
    Say(String),
    Tell(u64, String),
    Kick(u64, String),
    Teleport(u64, [f32; 3]),
    Give(u64, String, u32, bool),
    Tokens(i32),
    RulesPatch(String),
}

pub struct Sim {
    planet: String,
    pub game_ms: i64,
    unix_start: i64,
    slots: u32,
    players: Vec<SimPlayer>,
    objects: Vec<ObjectSpec>,
    pub tokens: i64,
    rules: serde_json::Map<String, Value>,
    pub done: Vec<Done>,
    /// Players the plugin kicked: they leave after the call, as in the game.
    left: Vec<(u64, String, u64)>,
}

impl Sim {
    fn new(s: &Scenario) -> Sim {
        let mut rules = serde_json::Map::new();
        for (k, v) in &s.world.rules {
            rules.insert(k.clone(), serde_json::to_value(v).unwrap_or(Value::Null));
        }
        let mut sim = Sim { planet: s.world.planet.clone(), game_ms: s.world.game_ms, unix_start: s.world.unix_ms, slots: s.world.backpack_slots, players: Vec::new(), objects: s.objects.clone(), tokens: 0, rules, done: Vec::new(), left: Vec::new() };
        for p in &s.players {
            sim.add_player(p);
        }
        sim
    }

    fn add_player(&mut self, p: &PlayerSpec) {
        self.players.retain(|q| q.client != p.client);
        self.players.push(SimPlayer { client: p.client, name: p.name.clone(), player_id: p.player_id.unwrap_or(p.client + 1000), steam_id: p.steam_id.clone(), position: p.position, operator: p.operator, inventory: p.inventory.clone() });
    }

    fn player(&self, client: u64) -> Option<&SimPlayer> {
        self.players.iter().find(|p| p.client == client)
    }

    fn unix_ms(&self) -> i64 {
        self.unix_start + self.game_ms
    }
}

impl World for Sim {
    fn info(&self) -> WorldInfo {
        WorldInfo { game_ms: self.game_ms, planet: self.planet.clone(), players: self.players.len() as u32, world_objects: self.objects.len() as u32, network_objects: self.objects.len() as u32 + self.players.len() as u32, terraformation: 1000.0, standing_still: false, unix_ms: self.unix_ms() }
    }
    fn players(&self) -> Vec<PlayerInfo> {
        self.players.iter().map(|p| PlayerInfo { client: p.client, name: p.name.clone(), steam_id: p.steam_id.clone(), position: p.position, gauges: [100.0, 100.0, 100.0, 0.0], player_id: p.player_id, operator: p.operator }).collect()
    }
    fn rules(&self) -> String {
        Value::Object(self.rules.clone()).to_string()
    }
    fn inventory(&self, client: u64) -> Option<InventoryInfo> {
        let p = self.player(client)?;
        Some(InventoryInfo { slots_used: p.inventory.values().sum(), slots_total: self.slots, items: p.inventory.iter().map(|(group, count)| ItemCount { group: group.clone(), count: *count }).collect() })
    }
    fn nearby(&self, center: [f32; 3], radius: f32, max: usize) -> (Vec<Nearby>, usize) {
        let mut found: Vec<Nearby> = self
            .objects
            .iter()
            .map(|o| (o, (0..3).map(|i| (o.position[i] - center[i]).powi(2)).sum::<f32>().sqrt()))
            .filter(|(_, d)| *d <= radius)
            .map(|(o, d)| Nearby { object: object_info(o), distance: d })
            .collect();
        found.sort_by(|a, b| a.distance.total_cmp(&b.distance).then(a.object.id.cmp(&b.object.id)));
        found.truncate(max);
        (found, self.objects.len())
    }
    fn object(&self, which: ObjectRef) -> Option<ObjectInfo> {
        self.objects.iter().find(|o| match which {
            ObjectRef::World(id) => o.id == id,
            ObjectRef::Net(net) => object_info(o).net == net,
        })
        .map(object_info)
    }
}

fn object_info(o: &ObjectSpec) -> ObjectInfo {
    ObjectInfo { id: o.id, net: o.net.unwrap_or(o.id.unsigned_abs() as u64), group: o.group.clone(), position: o.position }
}

impl Sink for Sim {
    fn apply(&mut self, _plugin: &str, actions: Vec<Action>) -> Applied {
        let mut applied = Applied::default();
        for action in actions {
            let ok = match action {
                Action::Say(t) => {
                    self.done.push(Done::Say(t));
                    true
                }
                Action::Tell(c, t) => {
                    self.done.push(Done::Tell(c, t));
                    self.player(c).is_some()
                }
                Action::Kick(c, reason) => {
                    self.done.push(Done::Kick(c, reason));
                    match self.players.iter().position(|p| p.client == c) {
                        Some(i) => {
                            let p = self.players.remove(i);
                            self.left.push((p.client, p.name, p.player_id));
                            true
                        }
                        None => false,
                    }
                }
                Action::Teleport(c, to) => {
                    self.done.push(Done::Teleport(c, to));
                    match self.players.iter_mut().find(|p| p.client == c) {
                        Some(p) => {
                            p.position = to;
                            true
                        }
                        None => false,
                    }
                }
                Action::Give(c, group, count) => {
                    let slots = self.slots;
                    let ok = match self.players.iter_mut().find(|p| p.client == c) {
                        Some(p) => {
                            let free = slots.saturating_sub(p.inventory.values().sum());
                            *p.inventory.entry(group.clone()).or_default() += count.min(free);
                            count <= free
                        }
                        None => false,
                    };
                    self.done.push(Done::Give(c, group, count, ok));
                    ok
                }
                Action::Tokens(n) => {
                    self.tokens = (self.tokens + n as i64).max(0);
                    self.done.push(Done::Tokens(n));
                    true
                }
                Action::RulesPatch(patch) => {
                    // Dotted names in, as `Host::rules()` shows them: speed.growth = 2.
                    if let Some(sections) = patch.as_object() {
                        for (section, values) in sections {
                            for (k, v) in values.as_object().into_iter().flatten() {
                                self.rules.insert(format!("{section}.{k}"), v.clone());
                            }
                        }
                    }
                    self.done.push(Done::RulesPatch(patch.to_string()));
                    true
                }
                Action::Http(_) => false,
            };
            if ok {
                applied.done += 1;
            } else {
                applied.refused += 1;
            }
        }
        applied
    }
    fn command_prefix(&self) -> String {
        "!".into()
    }
    fn operator(&self, client: u64) -> bool {
        self.player(client).is_some_and(|p| p.operator)
    }
}

// ---- running -------------------------------------------------------------------------------------

/// What a run produced.
#[derive(Debug, Default)]
pub struct Outcome {
    /// The transcript, one line each.
    pub lines: Vec<String>,
    /// Expectations that did not hold (and other failures).
    pub failures: Vec<String>,
    /// A table of what each kind of event cost.
    pub costs: Vec<String>,
    pub state: Option<(u32, usize)>,
}

impl Outcome {
    pub fn passed(&self) -> bool {
        self.failures.is_empty()
    }
}

/// A plugin installed in a scratch folder, approved with everything it asks for, and started in a host
/// that sends nothing. Gone when dropped.
pub struct Rig {
    dir: PathBuf,
    pub host: Host,
    saved: Saved,
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Rig {
    /// `folder` is a plugin folder as `pcds plugin install` takes it. `settings`: the values to give its
    /// `[[config]]` keys, checked against their declarations.
    pub fn new(folder: &Path, settings: &BTreeMap<String, toml::Value>) -> Result<Rig, String> {
        let found = super::read_folder(folder);
        let Some(m) = found.manifest.clone() else { return Err(found.status.detail().unwrap_or("no plugin.toml").to_string()) };
        if let super::Status::Invalid(why) | super::Status::Incompatible(why) = &found.status {
            return Err(why.clone());
        }
        let dir = std::env::temp_dir().join(format!("pcds-plugin-test-{}-{}", std::process::id(), super::stream::unix_ms()));
        let to = dir.join(m.id());
        std::fs::create_dir_all(&to).map_err(|e| format!("{}: {e}", to.display()))?;
        for name in [manifest::FILE, m.plugin.entry.as_str()] {
            let bytes = super::read_regular(&folder.join(name), super::limits::MAX_MODULE_BYTES).map_err(|e| format!("{}: {e}", folder.join(name).display()))?;
            let target = to.join(name);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            std::fs::write(&target, bytes).map_err(|e| format!("{}: {e}", target.display()))?;
        }
        let rig = Rig { dir: dir.clone(), host: Host::empty(&dir, "test"), saved: Saved::default() };
        for (key, value) in settings {
            let declared = m.config.iter().find(|k| &k.key == key).ok_or_else(|| format!("[config] {key}: the plugin declares no such setting{} (it declares: {})", manifest::hint(key, m.config.iter().map(|k| k.key.as_str())), m.config.iter().map(|k| k.key.as_str()).collect::<Vec<_>>().join(", ")))?;
            let text = match value {
                toml::Value::String(s) => s.clone(),
                toml::Value::Integer(n) => n.to_string(),
                toml::Value::Float(f) => f.to_string(),
                toml::Value::Boolean(b) => b.to_string(),
                other => return Err(format!("[config] {key}: {other:?} is not a text, number or yes/no")),
            };
            let text = declared.check(&text)?;
            super::config::set(&dir, m.id(), key, Some(&text))?;
        }
        super::approve_digest(&dir, m.id(), None, None)?;
        let mut rig = rig;
        rig.boot()?;
        Ok(rig)
    }

    fn boot(&mut self) -> Result<(), String> {
        let mut host = super::prepare(&self.dir, "test");
        host.offline = Some(Vec::new());
        host.start_all(&self.saved);
        let p = host.plugins.first().ok_or("the plugin was not found after installing it")?;
        if !p.running() {
            return Err(format!("the plugin did not start: {}", p.status.detail().unwrap_or(p.status.name())));
        }
        self.host = host;
        Ok(())
    }

    pub fn plugin(&self) -> &super::Loaded {
        &self.host.plugins[0]
    }

    /// The state through the plugin's `save`, then a new instance that `load`s it.
    pub fn reload(&mut self) -> Result<(), String> {
        self.host.checkpoint(&mut self.saved, None);
        // The counters and the log are the plugin's history, not the instance's: they go on.
        let (stats, logs) = self.host.plugins.first_mut().map(|p| (std::mem::take(&mut p.stats), p.logs.clone())).unwrap_or_default();
        self.boot()?;
        if let Some(p) = self.host.plugins.first_mut() {
            p.stats = stats;
            p.logs = logs;
        }
        Ok(())
    }
}

/// Runs the scenario's steps on the rig.
pub fn run(rig: &mut Rig, scenario: &Scenario) -> Outcome {
    let mut out = Outcome::default();
    let mut sim = Sim::new(scenario);
    let prefix = if scenario.name.is_empty() { String::new() } else { format!("{}: ", scenario.name) };
    out.lines.push(format!("{prefix}{} steps", scenario.steps.len()));
    let mut log_seen = 0u64;
    for (n, step) in scenario.steps.iter().enumerate() {
        sim.done.clear();
        let before = (rig.plugin().stats.calls, rig.plugin().stats.fuel, rig.plugin().stats.faults);
        let heading = format!("step {}{}", n + 1, step.note.as_ref().map(|t| format!(" ({t})")).unwrap_or_default());
        let mut verdict = Verdict::Continue;
        let said = describe(step);
        out.lines.push(format!("{heading}: {said}"));
        let asked_before = rig.host.offline.as_ref().map_or(0, Vec::len);
        if let Some(p) = &step.join {
            sim.add_player(p);
            let (name, id) = (p.name.clone(), sim.player(p.client).map_or(0, |q| q.player_id));
            let event = Event::PlayerJoin { client: p.client, name: &name, steam_id: &p.steam_id, player_id: id };
            deliver(rig, &mut sim, &event);
        } else if let Some(client) = step.leave {
            if let Some(i) = sim.players.iter().position(|p| p.client == client) {
                let p = sim.players.remove(i);
                let event = Event::PlayerLeave { client, name: &p.name, player_id: p.player_id };
                deliver(rig, &mut sim, &event);
            } else {
                out.failures.push(format!("{heading}: client {client} is not online"));
            }
        } else if let Some(c) = &step.chat {
            match sim.player(c.client).map(|p| (p.name.clone(), p.player_id)) {
                Some((name, player_id)) => {
                    let event = Event::Chat { client: c.client, name: &name, text: &c.text, player_id };
                    deliver(rig, &mut sim, &event);
                }
                None => out.failures.push(format!("{heading}: client {} is not online", c.client)),
            }
        } else if let Some(a) = &step.advance {
            // Validated at parse time.
            let total = duration_ms(a).unwrap_or(0);
            let step_ms = 50;
            let mut left = total;
            while left > 0 {
                let now = step_ms.min(left);
                sim.game_ms += now;
                left -= now;
                let event = Event::Tick { game_ms: sim.game_ms, tick: (sim.game_ms / 5) as i32, players: sim.players.len() as u32, since_ms: 0, unix_ms: sim.unix_ms() };
                deliver_tick(rig, &mut sim, &event);
            }
        } else if let Some(o) = &step.place {
            sim.objects.retain(|x| x.id != o.id);
            sim.objects.push(ObjectSpec { id: o.id, group: o.group.clone(), position: o.position, net: None });
            let event = Event::ObjectPlaced { id: o.id, group: &o.group, position: o.position, client: o.client };
            deliver(rig, &mut sim, &event);
        } else if let Some(o) = &step.destroy {
            let known = sim.objects.iter().position(|x| x.id == o.id).map(|i| sim.objects.remove(i));
            let (group, position) = known.map(|k| (k.group, k.position)).unwrap_or((o.group.clone(), o.position));
            let event = Event::ObjectDestroyed { id: o.id, group: &group, position, client: o.client };
            deliver(rig, &mut sim, &event);
        } else if let Some(r) = &step.rpc {
            let event = Event::RpcBefore { client: r.client, rpc: &r.name, object: r.object, args_json: &r.args };
            verdict = deliver(rig, &mut sim, &event);
            if !rig.plugin().rpc.contains(r.name.as_str()) {
                out.lines.push(format!("  note: the plugin did not subscribe to {} (events.rpc), so it never heard of it", r.name));
            }
            out.lines.push(format!("  the request was {}", if verdict == Verdict::Veto { "refused (veto)" } else { "let through" }));
        } else if let Some(ok) = step.world_saved {
            let event = Event::WorldSaved { game_ms: sim.game_ms, ok };
            deliver(rig, &mut sim, &event);
        } else if let Some(h) = &step.http_done {
            let body = h.body.clone().into_bytes();
            let event = Event::HttpDone { ticket: h.ticket, status: h.status, error: &h.error, body: &body, headers: vec![] };
            deliver_http(rig, &mut sim, &event);
        } else if step.reload.is_some() || step.flip.is_some() {
            match rig.reload() {
                Ok(()) => {
                    out.lines.push("  the plugin saved its state and a new instance loaded it".into());
                    if step.flip.is_some() {
                        let event = Event::Flip { game_ms: sim.game_ms, server_version: env!("CARGO_PKG_VERSION") };
                        deliver(rig, &mut sim, &event);
                    }
                }
                Err(e) => out.failures.push(format!("{heading}: {e}")),
            }
        } else if let Some(s) = &step.set {
            if let Some(u) = s.unix_ms {
                sim.unix_start = u - sim.game_ms;
            }
            if let Some((c, pos)) = s.position {
                if let Some(p) = sim.players.iter_mut().find(|p| p.client == c) {
                    p.position = pos;
                }
            }
            if let Some((c, inv)) = &s.inventory {
                if let Some(p) = sim.players.iter_mut().find(|p| p.client == *c) {
                    p.inventory = inv.clone();
                }
            }
            if let Some((c, op)) = s.operator {
                if let Some(p) = sim.players.iter_mut().find(|p| p.client == c) {
                    p.operator = op;
                }
            }
            if let Some(o) = &s.object {
                sim.objects.retain(|x| x.id != o.id);
                sim.objects.push(o.clone());
            }
            if let Some(id) = s.remove_object {
                sim.objects.retain(|x| x.id != id);
            }
        }
        // Players the plugin kicked leave now, and the plugin hears of it.
        for _ in 0..4 {
            let gone: Vec<_> = std::mem::take(&mut sim.left);
            if gone.is_empty() {
                break;
            }
            for (client, name, player_id) in gone {
                let event = Event::PlayerLeave { client, name: &name, player_id };
                deliver(rig, &mut sim, &event);
            }
        }
        // The transcript: what the plugin did.
        for d in &sim.done {
            out.lines.push(format!("  {}", show(d)));
        }
        if let Some(asked) = rig.host.offline.as_ref().map(|v| &v[asked_before..]) {
            for f in asked {
                out.lines.push(format!("  http {} {} (not sent; ticket {}){}", f.method, f.shown, f.ticket, if f.body.is_empty() { String::new() } else { format!(" body {}", short(&String::from_utf8_lossy(&f.body))) }));
            }
        }
        let p = rig.plugin();
        let logs = p.logs.after(log_seen);
        log_seen = logs.last().map_or(log_seen, |l| l.seq);
        for l in &logs {
            out.lines.push(format!("  log {}: {}", level_name(l.level), l.text));
        }
        let (calls, fuel, faults) = (p.stats.calls - before.0, p.stats.fuel - before.1, p.stats.faults - before.2);
        if calls > 0 && !matches!(step.advance, Some(_)) {
            out.lines.push(format!("  ({calls} call{}, {fuel} fuel)", if calls == 1 { "" } else { "s" }));
        }
        if faults > 0 {
            out.failures.push(format!("{heading}: the plugin faulted: {}", p.stats.last_error.as_deref().unwrap_or("?")));
        }
        if !matches!(rig.plugin().status, super::Status::Running) {
            out.failures.push(format!("{heading}: the plugin is {}: {}", rig.plugin().status.name(), rig.plugin().status.detail().unwrap_or("")));
            break;
        }
        // The expectations.
        for e in &step.expect {
            let all: Vec<String> = matching(e, &sim.done, rig.host.offline.as_deref().map_or(&[], |v| &v[asked_before..]), &logs, verdict);
            let hits = all.iter().filter(|text| e.contains.as_ref().is_none_or(|c| text.contains(c.as_str())) && e.equals.as_ref().is_none_or(|q| *text == q)).count();
            let ok = match e.count {
                Some(n) => hits == n,
                None => hits >= 1,
            };
            if !ok {
                out.failures.push(format!("{heading}: expected {} {}, found {hits}{}", e.count.map_or("at least one".to_string(), |n| n.to_string()), describe_expect(e), if all.is_empty() { String::new() } else { format!(" (of this kind: {})", all.iter().map(|t| short(t)).collect::<Vec<_>>().join("; ")) }));
            }
        }
    }
    let p = rig.plugin();
    for k in Kind::ALL {
        let e = p.stats.by_event[k as usize & 15];
        if e.calls > 0 {
            let budget = p.limits.fuel_per_event;
            out.costs.push(format!("{:<18} {:>6} calls  avg {:>8} fuel  max {:>8} fuel ({:.1}% of the {} budget)  avg {:.1} us", k.name(), e.calls, e.fuel / e.calls, e.max_fuel, e.max_fuel as f64 * 100.0 / budget as f64, budget, e.nanos as f64 / e.calls as f64 / 1000.0));
        }
    }
    if let Some(rt) = rig.host.plugins.first_mut().and_then(|p| p.rt.as_mut()) {
        match rt.save() {
            Ok(Some(b)) => out.state = Some((b.version, b.bytes.len())),
            Ok(None) => {}
            Err(f) => out.failures.push(format!("saving its state: {f}")),
        }
    }
    let p = rig.plugin();
    if !p.denied_caps.is_empty() {
        out.failures.push(format!("the plugin used {} without having asked for it (or without being granted it): add it to plugin.toml's capabilities", p.denied_caps.join(", ")));
    }
    out
}

fn deliver(rig: &mut Rig, sim: &mut Sim, event: &Event<'_>) -> Verdict {
    let game_ms = sim.game_ms;
    let v = rig.host.deliver(sim, event, game_ms);
    settle(rig);
    v
}

fn deliver_tick(rig: &mut Rig, sim: &mut Sim, event: &Event<'_>) {
    let game_ms = sim.game_ms;
    rig.host.run(sim, event, game_ms);
    settle(rig);
}

fn deliver_http(rig: &mut Rig, sim: &mut Sim, event: &Event<'_>) -> Verdict {
    // A plugin hears the answer of its own request whatever it subscribed to: straight into the call.
    let game_ms = sim.game_ms;
    rig.host.send(0, sim, event, game_ms);
    settle(rig);
    Verdict::Continue
}

fn settle(rig: &mut Rig) {
    let Rig { host, saved, .. } = rig;
    if host.plugins.iter().any(|p| p.retire_due) {
        host.settle(saved);
    }
}

fn level_name(l: Level) -> &'static str {
    match l {
        Level::Error => "error",
        Level::Warn => "warn",
        Level::Info => "info",
        Level::Debug => "debug",
    }
}

fn short(s: &str) -> String {
    let one: String = s.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    if one.chars().count() > 120 { format!("{:?}...", one.chars().take(117).collect::<String>()) } else { format!("{one:?}") }
}

fn show(d: &Done) -> String {
    match d {
        Done::Say(t) => format!("say {}", short(t)),
        Done::Tell(c, t) => format!("tell {c} {}", short(t)),
        Done::Kick(c, r) => format!("kick {c} {}", short(r)),
        Done::Teleport(c, p) => format!("teleport {c} to {p:?}"),
        Done::Give(c, g, n, ok) => format!("give {c} {n} x {g}{}", if *ok { "" } else { " (refused: no player or no room)" }),
        Done::Tokens(n) => format!("tokens {n:+}"),
        Done::RulesPatch(p) => format!("rules {p}"),
    }
}

fn describe(step: &Step) -> String {
    if let Some(p) = &step.join {
        format!("{} (client {}) joins", p.name, p.client)
    } else if let Some(c) = step.leave {
        format!("client {c} leaves")
    } else if let Some(c) = &step.chat {
        format!("client {} says {}", c.client, short(&c.text))
    } else if let Some(a) = &step.advance {
        format!("{a} of game time pass")
    } else if let Some(o) = &step.place {
        format!("{} {} is placed at {:?} by client {}", o.group, o.id, o.position, o.client)
    } else if let Some(o) = &step.destroy {
        format!("object {} is destroyed by client {}", o.id, o.client)
    } else if let Some(r) = &step.rpc {
        format!("client {} sends {} {}", r.client, r.name, r.args)
    } else if let Some(ok) = step.world_saved {
        format!("the world is saved ({})", if ok { "ok" } else { "failed" })
    } else if let Some(h) = &step.http_done {
        format!("the answer to request {} arrives: status {}", h.ticket, h.status)
    } else if step.flip.is_some() {
        "hot update".into()
    } else if step.reload.is_some() {
        "reload".into()
    } else {
        "the world changes".into()
    }
}

fn describe_expect(e: &Expect) -> String {
    let kind = if e.say.is_some() {
        "say".to_string()
    } else if let Some(c) = e.tell {
        format!("tell {c}")
    } else if let Some(c) = e.kick {
        format!("kick {c}")
    } else if let Some(c) = e.teleport {
        format!("teleport {c}")
    } else if let Some(c) = e.give {
        format!("give {c}")
    } else if e.tokens.is_some() {
        "tokens".into()
    } else if e.rules_patch.is_some() {
        "rules_patch".into()
    } else if e.http.is_some() {
        "http".into()
    } else if e.veto.is_some() {
        "veto".into()
    } else {
        "log".into()
    };
    let with = match (&e.contains, &e.equals) {
        (Some(c), _) => format!(" containing {c:?}"),
        (None, Some(q)) => format!(" equal to {q:?}"),
        _ => String::new(),
    };
    format!("{kind}{with}")
}

/// The texts of the things of the kind `e` names that the step made happen.
fn matching(e: &Expect, done: &[Done], http: &[super::net::Fetch], logs: &[super::history::LogLine], verdict: Verdict) -> Vec<String> {
    let mut texts = Vec::new();
    for d in done {
        match (d, e) {
            (Done::Say(t), Expect { say: Some(_), .. }) => texts.push(t.clone()),
            (Done::Tell(c, t), Expect { tell: Some(w), .. }) if c == w => texts.push(t.clone()),
            (Done::Kick(c, t), Expect { kick: Some(w), .. }) if c == w => texts.push(t.clone()),
            (Done::Teleport(c, p), Expect { teleport: Some(w), .. }) if c == w => texts.push(format!("{p:?}")),
            (Done::Give(c, g, n, _), Expect { give: Some(w), .. }) if c == w => texts.push(format!("{n} {g}")),
            (Done::Tokens(n), Expect { tokens: Some(_), .. }) => texts.push(n.to_string()),
            (Done::RulesPatch(p), Expect { rules_patch: Some(_), .. }) => texts.push(p.clone()),
            _ => {}
        }
    }
    if e.http.is_some() {
        texts.extend(http.iter().map(|f| format!("{} {} {}", f.method, f.shown, String::from_utf8_lossy(&f.body))));
    }
    if e.veto.is_some() && verdict == Verdict::Veto {
        texts.push("veto".into());
    }
    if e.log.is_some() {
        texts.extend(logs.iter().map(|l| l.text.clone()));
    }
    texts
}

/// Finds the scenarios of a plugin folder: the files named, else `scenarios/*.toml`, else
/// `scenario.toml`.
pub fn find(folder: &Path, named: &[String]) -> Result<Vec<PathBuf>, String> {
    if !named.is_empty() {
        return Ok(named.iter().map(PathBuf::from).collect());
    }
    let mut found: Vec<PathBuf> = std::fs::read_dir(folder.join("scenarios")).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "toml")).collect();
    found.sort();
    if found.is_empty() && folder.join("scenario.toml").is_file() {
        found.push(folder.join("scenario.toml"));
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_and_mistakes_in_a_scenario_are_named() {
        assert_eq!(duration_ms("90s"), Ok(90_000));
        assert_eq!(duration_ms("10m"), Ok(600_000));
        assert_eq!(duration_ms("500ms"), Ok(500));
        assert!(duration_ms("10").unwrap_err().contains("unit"));
        assert!(duration_ms("2 weeks").is_err() && duration_ms("99999d").unwrap_err().contains("30 days"));
        assert!(Scenario::parse("[[step]]\nchat = { client = 1, text = \"hi\" }\nadvance = \"1s\"").unwrap_err().contains("exactly one thing"));
        assert!(Scenario::parse("[[step]]\nchatt = { client = 1, text = \"hi\" }").unwrap_err().contains("did you mean `chat`"));
        assert!(Scenario::parse("[[step]]\nadvance = \"1s\"\nexpect = [{ say = true, tell = 1 }]").unwrap_err().contains("exactly one of"));
    }
}
