//! The plugin host (docs/plugins.md). Plugins add commands, rules and reactions to the world
//! without changing the server: a plugin is a folder in `plugins/` beside the rules with a
//! `plugin.toml` and either a WebAssembly module or a rules file.
//!
//! The host lives in the bank, next to the game it serves, and nowhere else: a plugin runs on the
//! game thread, for the length of one event, with a fuel budget, and can only read the world
//! through `Ctx` and queue actions the host applies afterwards (`actions.rs`). A plugin that
//! traps, runs out of fuel or answers nonsense is restarted from its last state; one that does it
//! too often is switched off. None of it can stop the tick or the flip: the bank catches a
//! panic of its own host code, and every wait is bounded.
//!
//! What is where:
//! - `manifest.rs`, `approvals.rs`, `limits.rs`: what a plugin asks for, what the admin allowed,
//!   what the host allows at most.
//! - `ctx.rs`: the capabilities and the queue of actions every plugin call goes through.
//! - `wasm.rs`, `data.rs`: the two kinds of plugin, behind `runtime.rs`.
//! - `state.rs`: plugin state in the snapshot of a flip and on disk.
//! - `stream.rs`: the event stream for out-of-process integrations.
//! - `admin.rs`: the admin API's part. `crate::events` is where the game hands events over.
//!
//! The game is reached only through the `Sink` trait below, which is also what makes the host
//! testable without game tables.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use pcds_plugin_api::{Event, Kind, Verdict, Writer, caps};
use serde::{Deserialize, Serialize};

pub mod actions;
pub mod admin;
pub mod approvals;
pub mod check;
pub mod ctx;
mod data;
pub mod limits;
pub mod manifest;
mod runtime;
pub mod state;
pub mod stream;
mod view;
mod wasm;

#[cfg(test)]
mod cost;
#[cfg(test)]
mod game_tests;
#[cfg(test)]
mod tests;

use approvals::{Approval, Approvals};
use ctx::{Action, Ctx, Grant, World};
use limits::Limits;
use manifest::{Manifest, PluginKind};
use runtime::{Fault, Runtime};
use state::Blob;

pub use actions::Applied;
pub use view::changes_json;

/// The game, as the host sees it: a world to read and a place to apply actions to.
pub trait Sink: World {
    /// Applies queued actions (after the plugin's call).
    fn apply(&mut self, plugin: &str, actions: Vec<Action>) -> Applied;
    /// What chat commands begin with.
    fn command_prefix(&self) -> String;
}

impl Sink for crate::game::Game {
    fn apply(&mut self, plugin: &str, queued: Vec<Action>) -> Applied {
        actions::apply(self, plugin, queued)
    }
    fn command_prefix(&self) -> String {
        self.rules.chat.command_prefix.clone().filter(|p| !p.is_empty()).unwrap_or_else(|| "!".into())
    }
}

/// Part of the game's `ext`: the host (never in the snapshot), what the events want, and what the
/// snapshot carries for plugins.
#[derive(Default, Serialize, Deserialize)]
pub struct Slot {
    #[serde(skip)]
    pub host: Option<Box<Host>>,
    /// One bit for each event kind someone wants (a plugin, or the stream). `events::wants` is
    /// this and nothing else, so an event nobody wants costs one load and one test.
    #[serde(skip)]
    pub interest: u32,
    /// Where plugins are looked for and under which world's state: set once the host has started.
    #[serde(skip)]
    pub place: Option<(PathBuf, String)>,
    #[serde(default)]
    pub saved: state::Saved,
}

impl Slot {
    /// The union of what the running plugins and the stream want.
    pub fn refresh(&mut self) {
        self.interest = self.host.as_ref().map_or(0, |h| h.interest) | self.saved.stream.wanted();
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Running,
    /// Installed, never approved.
    Pending,
    /// Approved, then switched off by the admin.
    Disabled,
    /// Approved, but its files are not the ones that were.
    Changed,
    /// Its manifest, module or rules do not hold up.
    Invalid(String),
    /// Built for another API major or a newer server.
    Incompatible(String),
    /// Started and failed, or switched off for faults.
    Failed(String),
}

impl Status {
    pub fn name(&self) -> &'static str {
        match self {
            Status::Running => "running",
            Status::Pending => "pending",
            Status::Disabled => "disabled",
            Status::Changed => "changed",
            Status::Invalid(_) => "invalid",
            Status::Incompatible(_) => "incompatible",
            Status::Failed(_) => "failed",
        }
    }
    pub fn detail(&self) -> Option<&str> {
        match self {
            Status::Invalid(s) | Status::Incompatible(s) | Status::Failed(s) => Some(s),
            Status::Pending => Some("installed, not approved: pcds plugin enable ID"),
            Status::Changed => Some("its files changed since they were approved: pcds plugin enable ID again"),
            _ => None,
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct Stats {
    pub calls: u64,
    pub fuel: u64,
    pub nanos: u64,
    pub max_nanos: u64,
    pub faults: u64,
    pub slow: u64,
    pub denied: u64,
    pub refused: u64,
    pub actions: u64,
    pub vetoes: u64,
    pub restarts: u64,
    /// Tick events skipped because the tick's budget was spent (they come at the next tick).
    pub shed: u64,
    pub last_error: Option<String>,
    pub last_fault_unix: Option<u64>,
}

/// What a plugin still has to be started with, when it is approved.
enum Pending {
    Wasm(wasmi::Module),
    Data(data::Rules),
}

pub struct Loaded {
    pub id: String,
    pub dir: PathBuf,
    pub manifest: Option<Manifest>,
    pub status: Status,
    pub digest: String,
    pub requested: Vec<String>,
    pub grant: Grant,
    pub limits: Limits,
    pub warnings: Vec<String>,
    pub commands: Vec<String>,
    kinds: u32,
    rpc: HashSet<String>,
    pending: Option<Pending>,
    rt: Option<Box<dyn Runtime>>,
    checkpoint: Option<Blob>,
    next_tick: i64,
    last_tick: i64,
    faults: VecDeque<Instant>,
    last_slow_log: Option<Instant>,
    pub stats: Stats,
}

impl Loaded {
    fn bare(id: &str, dir: &Path, status: Status) -> Loaded {
        Loaded {
            id: id.to_string(),
            dir: dir.to_path_buf(),
            manifest: None,
            status,
            digest: String::new(),
            requested: Vec::new(),
            grant: Grant::default(),
            limits: Limits::resolve(&Default::default(), &Default::default()),
            warnings: Vec::new(),
            commands: Vec::new(),
            kinds: 0,
            rpc: HashSet::new(),
            pending: None,
            rt: None,
            checkpoint: None,
            next_tick: 0,
            last_tick: 0,
            faults: VecDeque::new(),
            last_slow_log: None,
            stats: Stats::default(),
        }
    }

    pub fn running(&self) -> bool {
        self.status == Status::Running && self.rt.is_some()
    }

    /// What the folder holds, as far as can be told without running any of it: the manifest,
    /// the entry, the approval, and for an approved plugin the module compiled or the rules read.
    /// (An installed plugin's folder is named for its id: `prepare` checks that, a source folder
    /// being checked or installed is named as its author likes.)
    fn inspect(engine: &wasmi::Engine, dir: &Path, approval: Option<&Approval>) -> Loaded {
        let folder = dir.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let mut p = Loaded::bare(&folder, dir, Status::Pending);
        let manifest_bytes = match std::fs::read(dir.join(manifest::FILE)) {
            Ok(b) => b,
            Err(e) => {
                p.status = Status::Invalid(format!("{}: {e}", manifest::FILE));
                return p;
            }
        };
        let m = match std::str::from_utf8(&manifest_bytes).map_err(|e| e.to_string()).and_then(Manifest::parse) {
            Ok(m) => m,
            Err(e) => {
                p.status = Status::Invalid(format!("{}: {e}", manifest::FILE));
                return p;
            }
        };
        p.id = m.id().to_string();
        p.requested = m.plugin.capabilities.clone();
        p.commands = m.commands.iter().map(|c| c.name.to_lowercase()).collect();
        p.limits = Limits::resolve(&m.budget, &approval.map(|a| a.limits.clone()).unwrap_or_default());
        if m.plugin.abi != manifest::ABI_MAJOR {
            p.status = Status::Incompatible(format!("built for plugin ABI {}; this server speaks {}", m.plugin.abi, manifest::ABI_MAJOR));
            p.manifest = Some(m);
            return p;
        }
        if let Some(min) = m.plugin.min_server.as_deref().and_then(manifest::version_triple)
            && Some(min) > manifest::version_triple(env!("CARGO_PKG_VERSION"))
        {
            p.status = Status::Incompatible(format!("needs server {}.{}.{} or newer; this is {}", min.0, min.1, min.2, env!("CARGO_PKG_VERSION")));
            p.manifest = Some(m);
            return p;
        }
        // The entry's size is bounded before it is read: a plugin folder is the admin's, but a
        // file that is a symlink to /dev/zero must not stop the start.
        let entry_path = dir.join(&m.plugin.entry);
        let entry = match std::fs::metadata(&entry_path).map_err(|e| e.to_string()).and_then(|meta| if meta.len() as usize > limits::MAX_MODULE_BYTES { Err(format!("{} bytes: at most {} are loaded", meta.len(), limits::MAX_MODULE_BYTES)) } else { std::fs::read(&entry_path).map_err(|e| e.to_string()) }) {
            Ok(b) => b,
            Err(e) => {
                p.status = Status::Invalid(format!("{}: {e}", m.plugin.entry));
                p.manifest = Some(m);
                return p;
            }
        };
        p.digest = approvals::digest(&manifest_bytes, &entry);
        p.status = match approval {
            None => Status::Pending,
            Some(a) if !a.enabled => Status::Disabled,
            Some(a) if a.digest != p.digest => Status::Changed,
            Some(_) => Status::Running,
        };
        let approved = p.status == Status::Running;
        p.grant = Grant::from_names(approval.filter(|_| approved).into_iter().flat_map(|a| a.grant.iter().map(String::as_str)).filter(|g| p.requested.iter().any(|r| r == g)));
        // What an approved plugin is: its module compiled, or its rules read. Unapproved ones are
        // listed but not looked into any further than the manifest.
        let kinds: BTreeSet<Kind> = match m.plugin.kind {
            PluginKind::Wasm => {
                if approved {
                    match wasm::compile(engine, &entry) {
                        Ok((module, _)) => p.pending = Some(Pending::Wasm(module)),
                        Err(e) => p.status = Status::Invalid(e),
                    }
                }
                m.kinds()
            }
            PluginKind::Data => {
                let text = String::from_utf8_lossy(&entry);
                match data::Rules::parse(&text) {
                    Ok(rules) => {
                        let needed: Vec<&str> = rules.needed_caps().into_iter().filter(|c| !p.requested.iter().any(|r| r == c)).collect();
                        if !needed.is_empty() {
                            p.status = Status::Invalid(format!("the rules need capabilities the manifest does not ask for: {}", needed.join(", ")));
                        }
                        p.commands = rules.command_names();
                        let kinds = rules.kinds();
                        if approved {
                            p.pending = Some(Pending::Data(rules));
                        }
                        kinds
                    }
                    Err(e) => {
                        p.status = Status::Invalid(format!("{}: {e}", m.plugin.entry));
                        BTreeSet::new()
                    }
                }
            }
        };
        // What the plugin is called for: the events it asked for and was granted.
        p.kinds = kinds.iter().filter(|k| p.grant.has(caps::for_event(**k))).fold(0, |m, k| m | k.bit());
        for k in kinds.iter().filter(|k| approved && !p.grant.has(caps::for_event(**k))) {
            p.warnings.push(format!("{} events are not delivered: {} is not granted", k.name(), caps::for_event(*k)));
        }
        p.rpc = m.events.rpc.iter().cloned().collect();
        p.manifest = Some(m);
        if p.status != Status::Running {
            p.pending = None;
        }
        p
    }

    /// The state this plugin starts with: the checkpoint it carries, the snapshot's, or the file
    /// beside it.
    fn starting_state(&mut self, saved: &state::Saved, world: &str) -> Option<Blob> {
        if self.checkpoint.is_none() {
            self.checkpoint = saved.states.get(&self.id).cloned();
        }
        if self.checkpoint.is_none() {
            match state::read(&state::path(&self.dir, world)) {
                Ok(b) => self.checkpoint = b,
                Err(e) => eprintln!("plugin {}: its saved state is not used: {e}", self.id),
            }
        }
        self.checkpoint.clone()
    }

    /// Instantiates and starts what `inspect` prepared. On failure the plugin is `Failed`.
    fn start(&mut self, engine: &wasmi::Engine, saved: &state::Saved, world: &str) {
        let Some(pending) = self.pending.take() else { return };
        let state = self.starting_state(saved, world);
        let ctx = Ctx::new(&self.id, self.grant);
        let made: Result<Box<dyn Runtime>, String> = match pending {
            Pending::Wasm(module) => wasm::Wasm::new(engine, &module, ctx, self.limits).map(|w| Box::new(w) as Box<dyn Runtime>),
            Pending::Data(rules) => Ok(Box::new(data::Data::new(rules, ctx))),
        };
        let started = made.and_then(|mut rt| rt.start(state.as_ref()).map(|()| rt).map_err(|f| format!("start failed: {f}")));
        match started {
            Ok(rt) => self.rt = Some(rt),
            Err(e) => {
                eprintln!("plugin {}: {e}", self.id);
                self.status = Status::Failed(e);
            }
        }
    }

    /// Whether the same plugin, approved the same way, would be the same: a reload keeps it running.
    fn same_as(&self, other: &Loaded) -> bool {
        self.running() && other.status == Status::Running && self.digest == other.digest && self.grant == other.grant && self.limits == other.limits
    }

    /// A fault: counted, logged, the instance restarted from the last checkpoint; five in a
    /// minute switch the plugin off.
    fn faulted(&mut self, fault: &Fault, during: &str) {
        let now = Instant::now();
        self.stats.faults += 1;
        self.stats.last_error = Some(format!("{during}: {fault}"));
        self.stats.last_fault_unix = Some(approvals::now());
        self.faults.push_back(now);
        while self.faults.front().is_some_and(|t| now.duration_since(*t) > limits::FAULT_WINDOW) {
            self.faults.pop_front();
        }
        eprintln!("plugin {}: {during}: {fault}", self.id);
        if self.faults.len() >= limits::MAX_FAULTS {
            let why = format!("switched off after {} faults in {} s; the last, in {during}: {fault}", self.faults.len(), limits::FAULT_WINDOW.as_secs());
            eprintln!("plugin {}: {why} (pcds plugin reload {} starts it again)", self.id, self.id);
            self.rt = None;
            self.status = Status::Failed(why);
            return;
        }
        self.stats.restarts += 1;
        let checkpoint = self.checkpoint.clone();
        if let Some(rt) = self.rt.as_mut()
            && let Err(e) = rt.restart(checkpoint.as_ref())
        {
            let why = format!("could not restart after a fault: {e}");
            eprintln!("plugin {}: {why}", self.id);
            self.rt = None;
            self.status = Status::Failed(why);
        }
    }

    fn wants(&self, event: &Event<'_>, game_ms: i64) -> bool {
        if !self.running() || self.kinds & event.kind().bit() == 0 {
            return false;
        }
        match event {
            Event::Command { command, .. } => self.commands.iter().any(|c| c.eq_ignore_ascii_case(command)),
            Event::RpcBefore { rpc, .. } | Event::RpcAfter { rpc, .. } => self.rpc.contains(*rpc),
            Event::Tick { .. } => game_ms >= self.next_tick,
            _ => true,
        }
    }
}

/// All the plugins of one bank.
pub struct Host {
    pub dir: PathBuf,
    pub world: String,
    engine: wasmi::Engine,
    pub plugins: Vec<Loaded>,
    /// Lowercase command name to the plugin that owns it.
    commands: HashMap<String, usize>,
    /// The kinds the running plugins want.
    pub interest: u32,
    /// Where the next tick's round of plugins begins (they take turns when the budget is short).
    rotate: usize,
    /// approved.json did not read: nothing runs.
    pub approvals_error: Option<String>,
}

/// Reads the plugin folder and prepares every plugin: manifests, approvals, modules compiled.
/// Nothing of a plugin runs yet, and the game is not needed, so a new bank does this while the
/// old one still plays: a hot update stops the world for the `attach` that follows, not for this.
pub fn prepare(dir: &Path, world: &str) -> Host {
    let engine = wasm::engine();
    let mut host = Host { dir: dir.to_path_buf(), world: world.to_string(), engine: engine.clone(), plugins: Vec::new(), commands: HashMap::new(), interest: 0, rotate: 0, approvals_error: None };
    let approvals = match Approvals::load(dir) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("plugins: {e}: no plugin runs until that is fixed");
            host.approvals_error = Some(e);
            Approvals::default()
        }
    };
    let mut folders: Vec<PathBuf> = std::fs::read_dir(dir).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.is_dir() && !p.file_name().is_some_and(|n| n.to_string_lossy().starts_with('.'))).collect();
    folders.sort();
    for folder in folders.into_iter().take(limits::MAX_PLUGINS) {
        let id = folder.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let approval = approvals.0.get(&id).filter(|_| host.approvals_error.is_none());
        let mut found = Loaded::inspect(&engine, &folder, approval);
        // Installed plugins live in a folder named for their id: it is how a command or a state file
        // finds its owner, and what `pcds plugin enable ID` names.
        if found.manifest.as_ref().is_some_and(|m| m.id() != id) {
            let why = format!("the folder is {id} but the manifest's id is {}: the folder's name is the id", found.id);
            (found.id, found.status, found.pending, found.kinds, found.grant) = (id, Status::Invalid(why), None, 0, Grant::default());
        }
        host.plugins.push(found);
    }
    host.index_commands();
    host
}

impl Host {
    pub fn is_empty(&self) -> bool {
        self.plugins.is_empty()
    }

    /// Who owns each command: the first running plugin by id; a later one that declares it too
    /// is told in its warnings.
    fn index_commands(&mut self) {
        self.commands.clear();
        for i in 0..self.plugins.len() {
            if self.plugins[i].status != Status::Running {
                continue;
            }
            for c in self.plugins[i].commands.clone() {
                match self.commands.get(&c) {
                    Some(owner) if *owner != i => {
                        let by = self.plugins[*owner].id.clone();
                        self.plugins[i].warnings.push(format!("the command {c} belongs to {by}: it is not delivered to this plugin"));
                    }
                    _ => {
                        self.commands.insert(c, i);
                    }
                }
            }
        }
    }

    fn recompute(&mut self) {
        self.interest = self.plugins.iter().filter(|p| p.running()).fold(0, |m, p| m | p.kinds);
        // A command is delivered as its own kind, found in chat messages: both bits are wanted.
        if self.plugins.iter().any(|p| p.running() && p.kinds & Kind::Command.bit() != 0) {
            self.interest |= Kind::Chat.bit();
        }
    }

    /// Starts every approved plugin with its state (from `saved`, else from disk).
    fn start_all(&mut self, saved: &state::Saved) {
        let (engine, world) = (self.engine.clone(), self.world.clone());
        for p in &mut self.plugins {
            p.start(&engine, saved, &world);
        }
        self.index_commands();
        self.recompute();
    }

    fn summary_line(&self) -> String {
        let running: Vec<&str> = self.plugins.iter().filter(|p| p.running()).map(|p| p.id.as_str()).collect();
        let waiting = self.plugins.iter().filter(|p| matches!(p.status, Status::Pending | Status::Changed)).count();
        let broken = self.plugins.iter().filter(|p| matches!(p.status, Status::Invalid(_) | Status::Incompatible(_) | Status::Failed(_))).count();
        format!("plugins: {} running{}, {waiting} waiting for approval, {broken} broken", running.len(), if running.is_empty() { String::new() } else { format!(" ({})", running.join(", ")) })
    }

    /// Delivers one event to the plugins that want it, applying what they ask for after each; the
    /// verdict (a veto of an RPC, from a plugin that may).
    pub fn run(&mut self, sink: &mut dyn Sink, event: &Event<'_>, game_ms: i64) -> Verdict {
        let n = self.plugins.len();
        let started = Instant::now();
        let mut payload: Option<Vec<u8>> = None;
        let mut verdict = Verdict::Continue;
        let is_tick = event.kind() == Kind::Tick;
        let first = if is_tick && n > 0 { self.rotate % n } else { 0 };
        for step in 0..n {
            let i = (first + step) % n;
            if !self.plugins[i].wants(event, game_ms) {
                continue;
            }
            // Command events go only to the plugin that owns the command.
            if let Event::Command { command, .. } = event
                && self.commands.get(&command.to_lowercase()) != Some(&i)
            {
                continue;
            }
            if is_tick && started.elapsed() > limits::TICK_BUDGET {
                self.plugins[i].stats.shed += 1;
                self.rotate = i;
                break;
            }
            let p = &mut self.plugins[i];
            if payload.is_none() && p.manifest.as_ref().is_some_and(|m| m.plugin.kind == PluginKind::Wasm) {
                let mut w = Writer::new();
                event.encode(&mut w);
                payload = Some(w.0);
            }
            // A tick comes with the game time since the plugin's last one.
            let owned;
            let event = if let Event::Tick { game_ms, tick, players, .. } = event {
                owned = Event::Tick { game_ms: *game_ms, tick: *tick, players: *players, since_ms: (*game_ms - p.last_tick).clamp(0, u32::MAX as i64) as u32 };
                if payload.is_some() {
                    let mut w = Writer::new();
                    owned.encode(&mut w);
                    payload = Some(w.0);
                }
                &owned
            } else {
                event
            };
            let bytes = payload.as_deref().unwrap_or(&[]);
            let Some(rt) = p.rt.as_mut() else { continue };
            rt.ctx().begin(&*sink);
            let t = Instant::now();
            let result = rt.event(event, bytes);
            let spent = t.elapsed();
            let (queued, denied, refused) = {
                let c = rt.ctx();
                (c.end(), std::mem::take(&mut c.denied), std::mem::take(&mut c.refused))
            };
            p.stats.calls += 1;
            p.stats.nanos += spent.as_nanos() as u64;
            p.stats.max_nanos = p.stats.max_nanos.max(spent.as_nanos() as u64);
            p.stats.denied += denied as u64;
            p.stats.refused += refused as u64;
            if spent > limits::SLOW_CALL {
                p.stats.slow += 1;
                if p.last_slow_log.is_none_or(|t| t.elapsed() > Duration::from_secs(10)) {
                    p.last_slow_log = Some(Instant::now());
                    eprintln!("plugin {}: {} took {} ms (slow calls are counted; the fuel limit stops runaways)", p.id, event.kind().name(), spent.as_millis());
                }
            }
            if is_tick {
                p.last_tick = game_ms;
                p.next_tick = game_ms + p.limits.tick_every_ms as i64;
            }
            match result {
                Ok((v, cost)) => {
                    p.stats.fuel += cost.fuel;
                    if v == Verdict::Veto {
                        if event.kind() == Kind::RpcBefore && p.grant.has(caps::RPC_VETO) {
                            p.stats.vetoes += 1;
                            verdict = Verdict::Veto;
                        } else {
                            p.stats.denied += 1;
                        }
                    }
                }
                Err(f) => {
                    // What the plugin asked for before it failed is not done.
                    p.faulted(&f, event.kind().name());
                    self.recompute();
                    continue;
                }
            }
            let applied = sink.apply(&p.id, queued);
            let p = &mut self.plugins[i];
            p.stats.actions += applied.done as u64;
            p.stats.refused += applied.refused as u64;
            if verdict == Verdict::Veto {
                break;
            }
        }
        verdict
    }

    /// Asks every running plugin for its state, within `budget` altogether; one the budget cannot
    /// wait for keeps the state of its last save. The states go to `saved` (the snapshot's).
    pub fn checkpoint(&mut self, saved: &mut state::Saved, budget: Option<Duration>) {
        let t = Instant::now();
        for p in self.plugins.iter_mut().filter(|p| p.running()) {
            if budget.is_some_and(|b| t.elapsed() > b) {
                eprintln!("plugin {}: no time to save its state for the hand-over; the state of its last save goes on", p.id);
                p.stats.shed += 1;
                continue;
            }
            let Some(rt) = p.rt.as_mut() else { continue };
            match rt.save() {
                Ok(Some(blob)) => {
                    p.checkpoint = Some(blob.clone());
                    saved.states.insert(p.id.clone(), blob);
                }
                Ok(None) => {
                    p.checkpoint = None;
                    saved.states.remove(&p.id);
                }
                Err(f) => p.faulted(&f, "saving its state"),
            }
        }
        self.recompute();
    }

    /// Writes the state of each running plugin beside it (so a restart or a crash keeps it).
    pub fn write_states(&self) {
        for p in self.plugins.iter().filter(|p| p.running()) {
            let file = state::path(&p.dir, &self.world);
            let result = match &p.checkpoint {
                Some(b) => state::write(&file, b),
                None => std::fs::remove_file(&file).or_else(|e| if e.kind() == std::io::ErrorKind::NotFound { Ok(()) } else { Err(e) }).map_err(|e| format!("{}: {e}", file.display())),
            };
            if let Err(e) = result {
                eprintln!("plugin {}: its state was not written: {e}", p.id);
            }
        }
    }
}

// ---- the game's entry points -------------------------------------------------------------------

/// Starts the prepared plugins in a game that is ready (a started world or a restored one). With
/// no plugin there is no host at all, and the game pays nothing.
pub fn attach(game: &mut crate::game::Game, mut host: Host) {
    let slot = &mut game.ext.plugins;
    slot.place = Some((host.dir.clone(), host.world.clone()));
    if host.is_empty() {
        slot.host = None;
        slot.refresh();
        return;
    }
    host.start_all(&slot.saved);
    // State of plugins that are gone is not carried on.
    let ids: HashSet<&str> = host.plugins.iter().map(|p| p.id.as_str()).collect();
    slot.saved.states.retain(|id, _| ids.contains(id.as_str()));
    println!("{}", host.summary_line());
    slot.host = Some(Box::new(host));
    slot.refresh();
}

/// Delivers an event to the plugins; Veto when one refused an RPC that may be refused.
pub fn dispatch(game: &mut crate::game::Game, event: &Event<'_>) -> Verdict {
    // The stream first: it only records (and notices when nobody asks any more).
    if game.ext.plugins.saved.stream.wanted() & event.kind().bit() != 0 {
        game.ext.plugins.saved.stream.push(event);
    }
    // A chat message that starts with the prefix and names a plugin's command is that command's event.
    let mut verdict = Verdict::Continue;
    let Some(mut host) = game.ext.plugins.host.take() else {
        game.ext.plugins.refresh();
        return verdict;
    };
    let game_ms = (game.time as f64 * 1000.0) as i64;
    verdict = host.run(game, event, game_ms);
    if let Event::Chat { client, name, text } = event
        && host.interest & Kind::Command.bit() != 0
    {
        let prefix = Sink::command_prefix(game);
        if let Some((command, args)) = crate::chathooks::command(Some(&prefix), text) {
            let command = Event::Command { client: *client, name, command: &command, args: args.iter().map(String::as_str).collect(), text };
            host.run(game, &command, game_ms);
        }
    }
    game.ext.plugins.host = Some(host);
    game.ext.plugins.refresh();
    verdict
}

/// The plugins' state into the snapshot's part of `ext` (before a hot update's snapshot, and at
/// every world save). `budget` bounds the time of all the plugins together.
pub fn checkpoint(game: &mut crate::game::Game, budget: Option<Duration>) {
    let slot = &mut game.ext.plugins;
    if let Some(host) = slot.host.as_mut() {
        host.checkpoint(&mut slot.saved, budget);
        slot.refresh();
    }
}

/// A world save: the plugins' states, in the snapshot's part and on disk.
pub fn saved(game: &mut crate::game::Game) {
    checkpoint(game, None);
    if let Some(host) = game.ext.plugins.host.as_ref() {
        host.write_states();
    }
}

/// Reads the folder again and starts what changed (the admin changed an approval, installed or
/// removed a plugin). A plugin whose files and approval are as they were keeps running; one that
/// changed is saved, replaced and started with its state.
pub fn reload(game: &mut crate::game::Game) -> Result<String, String> {
    let (dir, world) = game.ext.plugins.place.clone().ok_or("the plugin host has not started")?;
    let mut fresh = prepare(&dir, &world);
    let mut old = game.ext.plugins.host.take().map(|h| *h);
    if let Some(old) = old.as_mut() {
        old.checkpoint(&mut game.ext.plugins.saved, Some(limits::FLIP_BUDGET));
    }
    let engine = fresh.engine.clone();
    for p in &mut fresh.plugins {
        if let Some(prev) = old.as_mut().and_then(|o| o.plugins.iter().position(|q| q.id == p.id).map(|i| o.plugins.swap_remove(i))) {
            if prev.same_as(p) {
                // Unchanged: the running one stays (its counters too).
                *p = prev;
                continue;
            }
            p.checkpoint = prev.checkpoint;
            p.stats.restarts = prev.stats.restarts;
        }
        p.start(&engine, &game.ext.plugins.saved, &world);
    }
    fresh.index_commands();
    fresh.recompute();
    let line = fresh.summary_line();
    let ids: HashSet<&str> = fresh.plugins.iter().map(|p| p.id.as_str()).collect();
    game.ext.plugins.saved.states.retain(|id, _| ids.contains(id.as_str()));
    game.ext.plugins.host = (!fresh.is_empty()).then(|| Box::new(fresh));
    game.ext.plugins.refresh();
    println!("{line}");
    Ok(line)
}

// ---- approving, for the admin API and `pcds plugin` ---------------------------------------------

/// What a folder holds, read as far as the manifest and the entry go (no approval, nothing compiled).
pub fn read_folder(folder: &Path) -> Loaded {
    Loaded::inspect(&wasm::engine(), folder, None)
}

/// Approves a plugin: records `grant` (default: everything it asks for; never more) and the digest
/// of the files as they are now, and enables it. The server reads it at the next reload.
pub fn approve(dir: &Path, id: &str, grant: Option<Vec<String>>) -> Result<Approval, String> {
    if !manifest::valid_id(id) {
        return Err(format!("{id:?} is not a plugin id"));
    }
    let found = read_folder(&dir.join(id));
    if let Status::Invalid(why) | Status::Incompatible(why) = &found.status {
        return Err(format!("{id}: {why}"));
    }
    if found.id != id {
        return Err(format!("{id}: the manifest's id is {}: the folder's name is the id", found.id));
    }
    let grant = grant.unwrap_or_else(|| found.requested.clone());
    if let Some(extra) = grant.iter().find(|g| !found.requested.contains(g)) {
        return Err(format!("{id} does not ask for {extra}: it can only be granted what the manifest asks for ({})", found.requested.join(", ")));
    }
    let mut all = Approvals::load(dir)?;
    let limits = all.0.get(id).map(|a| a.limits.clone()).unwrap_or_default();
    let approval = Approval { enabled: true, grant, digest: found.digest, limits, approved_at: approvals::now() };
    all.0.insert(id.to_string(), approval.clone());
    all.save(dir)?;
    Ok(approval)
}

/// Switches an approved plugin off (it stays approved with the same grant for `approve` to switch on).
pub fn disable(dir: &Path, id: &str) -> Result<(), String> {
    let mut all = Approvals::load(dir)?;
    let a = all.0.get_mut(id).ok_or_else(|| format!("{id} was never enabled"))?;
    a.enabled = false;
    all.save(dir)
}

/// Whether a plugin wants the RPC named `name` (so the event's arguments are only built then).
pub fn wants_rpc(game: &crate::game::Game, name: &str) -> bool {
    game.ext.plugins.host.as_ref().is_some_and(|h| h.plugins.iter().any(|p| p.running() && p.rpc.contains(name)))
}
