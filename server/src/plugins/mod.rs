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
//! - `net.rs`, `config.rs`: `net.http`, a plugin's outbound HTTP on worker threads, and the values
//!   the admin sets for a plugin that it puts into requests.
//! - `admin.rs`: the admin API's part. `crate::events` is where the game hands events over.
//!
//! The game is reached only through the `Sink` trait below, which is also what makes the host
//! testable without game tables.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use pcds_plugin_api::{Event, Kind, Verdict, Writer, caps};
use serde::{Deserialize, Serialize};

pub mod actions;
pub mod admin;
pub mod approvals;
pub mod check;
pub mod config;
pub mod ctx;
mod data;
pub mod history;
pub mod limits;
pub mod manifest;
pub mod net;
pub mod pack;
mod runtime;
pub mod scenario;
pub mod sign;
pub mod state;
pub mod stream;
mod view;
mod wasm;

#[cfg(test)]
mod abi13_tests;
#[cfg(test)]
mod cookbook_tests;
#[cfg(test)]
mod cost;
#[cfg(test)]
mod fuzz_tests;
#[cfg(test)]
mod game_tests;
#[cfg(test)]
mod net_tests;
#[cfg(test)]
mod production_tests;
#[cfg(test)]
mod sign_tests;
#[cfg(test)]
mod tests;

use approvals::{Approval, Approvals};
use ctx::{Action, Ctx, Grant, NetCtx, World};
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
    /// Whether the client is one of the server's operators (`access.operators`).
    fn operator(&self, _client: u64) -> bool {
        false
    }
}

impl Sink for crate::game::Game {
    fn apply(&mut self, plugin: &str, queued: Vec<Action>) -> Applied {
        actions::apply(self, plugin, queued)
    }
    fn command_prefix(&self) -> String {
        self.rules.chat.command_prefix.clone().filter(|p| !p.is_empty()).unwrap_or_else(|| "!".into())
    }
    fn operator(&self, client: u64) -> bool {
        view::is_operator(self, client)
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
    /// What the timer scan of the folder has seen.
    #[serde(skip)]
    pub scan: Scan,
    /// The newest stream event the front was told of (`stream_bell`).
    #[serde(skip)]
    pub rung: u64,
    /// The client whose request the game is handling right now (0: none), so that the events of what
    /// it causes (an object built, an object taken apart) can say who did it. Set by `Game::rpc` and by
    /// the end of a deconstruction; never in the snapshot.
    #[serde(skip)]
    pub actor: u64,
    pub saved: state::Saved,
}

/// The timer scan: the folder is looked at every `POLL_EVERY` (a few `stat`s when nothing
/// changes); a change is acted on when two scans in a row see the same new state, so a plugin
/// that is still being copied in is not read half written.
#[derive(Default)]
pub struct Scan {
    next: Option<Instant>,
    seen: Sig,
    settling: Option<Sig>,
}

/// How often the folder is looked at.
pub const POLL_EVERY: Duration = Duration::from_secs(2);

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
    /// Being switched off: its last call and its last save are running (it is never seen so by a
    /// request: the host is single-threaded and does this between two events; it is in the log).
    Unloading,
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
            Status::Unloading => "unloading",
        }
    }
    /// The same status with its text made plain (`clean`).
    fn cleaned(self) -> Status {
        match self {
            Status::Invalid(s) => Status::Invalid(clean(&s)),
            Status::Incompatible(s) => Status::Incompatible(clean(&s)),
            Status::Failed(s) => Status::Failed(clean(&s)),
            other => other,
        }
    }
    pub fn detail(&self) -> Option<&str> {
        match self {
            Status::Invalid(s) | Status::Incompatible(s) | Status::Failed(s) => Some(s),
            Status::Pending => Some("discovered, awaiting approval: pcds plugin enable ID"),
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
    /// What each kind of event cost (indexed by the kind's number).
    pub by_event: [history::KindStats; 16],
    /// The last `FAULTS_KEPT` faults, newest last.
    pub fault_log: VecDeque<history::FaultRecord>,
    /// `net.http`: requests started, answered by a server (any status), without an answer
    /// (refused by the guard, DNS, connection, TLS, timeout, size), and the bytes of bodies sent and taken.
    pub http_requests: u64,
    pub http_answered: u64,
    pub http_failed: u64,
    pub http_bytes_out: u64,
    pub http_bytes_in: u64,
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
    /// Whether this instance has had a tick: its first one says no time has passed (`since_ms` 0), not the whole game time.
    ticked: bool,
    faults: VecDeque<Instant>,
    last_slow_log: Option<Instant>,
    pub stats: Stats,
    /// The entry file's path in its folder, for the scan that notices a change.
    entry_rel: Option<String>,
    /// What the host wants the admin to know about a plugin that keeps running: its files changed
    /// and wait for approval, or a reload of them failed and the old version goes on.
    pub notice: Option<String>,
    /// Switched off for faults but its instance is still held for the final call (`Host::settle`).
    retire_due: bool,
    /// `net.http`, when it is granted: where it may go, its values, its requests alive.
    pub net: Option<NetCtx>,
    /// The values the admin set for the keys the manifest declares (secret ones too: never printed).
    config: config::Values,
    /// Who signed these files: `plugin.sig` checked against the files and the trusted keys.
    pub signed: sign::Signed,
    /// The approval these files were judged by (None: never approved), for the capability diff.
    pub approval: Option<Approval>,
    /// The plugin's own settings: the keys that are not secrets, as set or as their defaults say.
    pub settings: Arc<Vec<(String, String)>>,
    /// The last log lines of the plugin and of the host about it.
    pub logs: Arc<history::LogRing>,
    /// Capabilities the plugin used without having been granted them, in the order it first did.
    pub denied_caps: Vec<&'static str>,
    /// New files on disk that wait for approval while this version goes on: what they ask for.
    pub incoming: Option<Incoming>,
    /// What the plugin asked for while it loaded; the host applies it once the game is its own again.
    boot_queue: Vec<Action>,
}

/// The files that wait for the admin, as far as the admin needs to judge them.
#[derive(Debug, Clone)]
pub struct Incoming {
    pub version: String,
    pub requested: Vec<String>,
    pub allow: Vec<String>,
    pub digest: String,
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
            ticked: false,
            faults: VecDeque::new(),
            last_slow_log: None,
            stats: Stats::default(),
            entry_rel: None,
            notice: None,
            retire_due: false,
            net: None,
            config: config::Values::new(),
            signed: sign::Signed::No,
            approval: None,
            settings: Arc::default(),
            logs: Arc::default(),
            denied_caps: Vec::new(),
            incoming: None,
            boot_queue: Vec::new(),
        }
    }

    pub fn running(&self) -> bool {
        self.status == Status::Running && self.rt.is_some()
    }

    /// What the folder holds, as far as can be told without running any of it: the manifest,
    /// the entry, the approval, and for an approved plugin the module compiled or the rules read.
    /// (An installed plugin's folder is named for its id: `prepare` checks that, a source folder
    /// being checked or installed is named as its author likes.)
    fn inspect(engine: &wasmi::Engine, dir: &Path, approval: Option<&Approval>, values: Option<&config::Values>, keys: &sign::Keys) -> Loaded {
        // Everything the host says of a plugin (its listing, its log lines, the screen of its approval)
        // is made of what its files and its folder say; none of it reaches a terminal as it was written.
        let mut p = Loaded::inspect_files(engine, dir, approval, values, keys);
        p.id = clean(&p.id);
        p.status = p.status.cleaned();
        p.warnings.iter_mut().for_each(|w| *w = clean(w));
        p
    }

    fn inspect_files(engine: &wasmi::Engine, dir: &Path, approval: Option<&Approval>, values: Option<&config::Values>, keys: &sign::Keys) -> Loaded {
        let folder = dir.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let mut p = Loaded::bare(&folder, dir, Status::Pending);
        let manifest_bytes = match read_regular(&dir.join(manifest::FILE), limits::MAX_MANIFEST_BYTES) {
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
        p.entry_rel = Some(m.plugin.entry.clone());
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
        // The entry is whatever the folder holds under that name: read as a file of at most the module
        // limit or not at all (a link to /dev/zero has size 0 and no end, a named pipe blocks the open).
        let entry_path = dir.join(&m.plugin.entry);
        let entry = match read_regular(&entry_path, limits::MAX_MODULE_BYTES) {
            Ok(b) => b,
            Err(e) => {
                p.status = Status::Invalid(format!("{}: {e}", m.plugin.entry));
                p.manifest = Some(m);
                return p;
            }
        };
        p.digest = approvals::digest(&manifest_bytes, &entry);
        p.signed = sign::check(dir, &p.digest, keys);
        match &p.signed {
            sign::Signed::Broken(why) => p.warnings.push(format!("{why}: the plugin is judged as if it were not signed")),
            sign::Signed::Revoked { id } => p.warnings.push(format!("signed by the key {id}, which was revoked: it is not trusted")),
            sign::Signed::Trusted(k) if sign::published(&k.id) => p.warnings.push(format!("signed by the key {}: {}", k.id, sign::PUBLISHED_WARNING)),
            _ => {}
        }
        let (standing, granted) = approvals::standing(approval, &p.digest, &p.requested, &p.signed);
        p.status = match standing {
            approvals::Standing::Pending => Status::Pending,
            approvals::Standing::Disabled => Status::Disabled,
            approvals::Standing::Changed => Status::Changed,
            approvals::Standing::Running => Status::Running,
        };
        let approved = p.status == Status::Running;
        p.grant = Grant::from_names(granted.iter().map(String::as_str));
        // net.http: the allow list is the manifest's (pinned by the digest), and the local network
        // is open only with both capabilities granted. Values are the admin's, for the declared keys.
        p.approval = approval.cloned();
        p.config = values.map(|v| m.config.iter().filter_map(|k| Some((k.key.clone(), v.get(&k.key)?.clone()))).collect()).unwrap_or_default();
        // The settings the plugin reads: each key that is not a secret, set or by default. A stored value
        // that the manifest's declaration refuses (the declaration changed since it was set) is not
        // quietly replaced by the default: the plugin does not start until the admin sets it again.
        let mut settings = Vec::new();
        for k in m.config.iter().filter(|k| !k.is_secret()) {
            match p.config.get(&k.key) {
                Some(v) => match k.check(v) {
                    Ok(text) => settings.push((k.key.clone(), text)),
                    Err(e) => {
                        p.status = Status::Invalid(format!("the stored setting {e}: set it again with pcds plugin config set {} {}", m.id(), k.key));
                        p.manifest = Some(m);
                        return p;
                    }
                },
                None => settings.extend(k.default_text().map(|d| (k.key.clone(), d))),
            }
        }
        p.settings = Arc::new(settings);
        if approved && p.grant.has(caps::NET_HTTP)
            && let Some(net) = &m.net
        {
            let local = p.grant.has(caps::NET_HTTP_LOCAL);
            let patterns = net.allow.iter().filter_map(|a| net::Pattern::parse(a, true).ok()).filter(|pattern| local || !pattern.is_ip()).collect();
            let policy = net::Policy { patterns, local, timeout: Duration::from_millis(net.timeout_ms.unwrap_or(limits::HTTP_TIMEOUT_MS) as u64), max_response: net.max_response_bytes.unwrap_or(limits::HTTP_RESPONSE_BYTES) as usize };
            p.net = Some(NetCtx { policy: Arc::new(policy), config: Arc::new(p.config.clone()), in_flight: Arc::default() });
        }
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
                Err(e) => warn!("plugin {}: its saved state is not used: {e}", self.id),
            }
        }
        self.checkpoint.clone()
    }

    /// Instantiates and starts what `inspect` prepared. On failure the plugin is `Failed`.
    fn start(&mut self, engine: &wasmi::Engine, gauge: &Arc<wasm::Gauge>, saved: &state::Saved, world: &str, boot: Option<Boot>) {
        let Some(pending) = self.pending.take() else { return };
        let state = self.starting_state(saved, world);
        let mut ctx = Ctx::new(&self.id, self.grant);
        ctx.net = self.net.clone();
        ctx.settings = self.settings.clone();
        ctx.logs = self.logs.clone();
        let made: Result<Box<dyn Runtime>, String> = match pending {
            Pending::Wasm(module) => wasm::Wasm::new(engine, &module, ctx, self.limits, gauge.clone()).map(|w| Box::new(w) as Box<dyn Runtime>),
            Pending::Data(rules) => Ok(Box::new(data::Data::new(rules, ctx))),
        };
        let mut queued = Vec::new();
        let started = made.and_then(|mut rt| {
            // The world as it is, for the plugin's `load` to read (the start of a game, a reload, a hot update).
            if let Some(b) = boot {
                // SAFETY: `b` was made from a live reference by the caller, which holds it for this whole call.
                rt.ctx().begin(unsafe { &*b.0 }, b.1);
            }
            let result = rt.start(state.as_ref());
            if boot.is_some() {
                queued = rt.ctx().end();
            }
            result.map(|()| rt).map_err(|f| self.explain_start(&f))
        });
        match started {
            Ok(rt) => {
                self.rt = Some(rt);
                self.boot_queue = queued;
            }
            Err(e) => {
                warn!("plugin {}: {e}", self.id);
                self.status = Status::Failed(clean(&e));
            }
        }
    }

    fn explain_start(&self, fault: &Fault) -> String {
        format!("start failed: {}", self.explain(fault, "starting"))
    }

    /// Whether the same files, approved the same way, were meant: a reload leaves it as it is.
    fn same_files(&self, other: &Loaded) -> bool {
        self.digest == other.digest && self.grant == other.grant && self.limits == other.limits && self.config == other.config
    }

    /// The last call, the last save, then the instance goes (its memory, its capabilities, what
    /// it had queued, its timers): the plugin is being switched off or has been removed. Both calls
    /// are bounded by the fuel of a save; a fault in either is logged, never counted against the
    /// plugin, and the state of its last save is kept instead. `folder_stays`: the plugin's state
    /// is written beside it (not when its folder is gone).
    fn retire(&mut self, saved: &mut state::Saved, world: &str, why: &str, folder_stays: bool) -> Status {
        self.retire_due = false;
        let Some(mut rt) = self.rt.take() else { return self.status.clone() };
        let was = std::mem::replace(&mut self.status, Status::Unloading);
        self.note(pcds_plugin_api::Level::Info, &format!("unloading ({why})"));
        match rt.unload() {
            Ok(()) => match rt.save() {
                Ok(blob) => self.checkpoint = blob,
                Err(f) => error!("plugin {}: its final save failed ({f}); the state of its last save is kept", self.id),
            },
            Err(f) => error!("plugin {}: its unload call failed ({f}); the state of its last save is kept", self.id),
        }
        // Instance and memory go here; whatever it queued in the unload call is never applied.
        drop(rt);
        match &self.checkpoint {
            Some(b) => {
                saved.states.insert(self.id.clone(), b.clone());
            }
            None => {
                saved.states.remove(&self.id);
            }
        }
        if folder_stays {
            let file = state::path(&self.dir, world);
            let result = match &self.checkpoint {
                Some(b) => state::write(&file, b),
                None => std::fs::remove_file(&file).or_else(|e| if e.kind() == std::io::ErrorKind::NotFound { Ok(()) } else { Err(e) }).map_err(|e| format!("{}: {e}", file.display())),
            };
            if let Err(e) = result {
                warn!("plugin {}: its state was not written: {e}", self.id);
            }
        }
        was
    }

    /// A line in the server's log about this plugin, and in the plugin's own history.
    fn note(&self, level: pcds_plugin_api::Level, text: &str) {
        let text = clean(text);
        match level {
            pcds_plugin_api::Level::Error | pcds_plugin_api::Level::Warn => warn!("plugin {}: {text}", self.id),
            _ => info!("plugin {}: {text}", self.id),
        }
        self.logs.push(level, &text);
    }

    /// What a fault says, in words that tell the author what to do. The fuel a call may use depends on
    /// what the call is: an event, a save, the start.
    fn explain(&self, fault: &Fault, during: &str) -> String {
        match fault {
            Fault::OutOfFuel => {
                let (budget, knob) = match during {
                    "saving its state" | "unloading" => (self.limits.fuel_per_save, format!("budget.fuel_per_save, at most {}", limits::MAX_FUEL_PER_SAVE)),
                    "starting" => (limits::FUEL_LOAD, "not adjustable: starting is limited to this".to_string()),
                    _ => (self.limits.fuel_per_event, format!("budget.fuel_per_event, at most {}", limits::MAX_FUEL_PER_EVENT)),
                };
                format!("out of fuel: the call used all {budget} fuel it is allowed (raise {knob}, or do less work per call)")
            }
            Fault::Trap(t) if t.contains("unreachable") => format!("trap: {t} (a panic in Rust compiles to this; build with panic = \"abort\" and look for unwrap, expect, an index out of range or a division by zero)"),
            other => other.to_string(),
        }
    }

    /// A fault: counted, logged, the instance restarted from the last checkpoint; five in a
    /// minute switch the plugin off.
    fn faulted(&mut self, fault: &Fault, during: &str) {
        let now = Instant::now();
        let text = self.explain(fault, during);
        self.stats.faults += 1;
        self.stats.last_error = Some(clean(&format!("{during}: {text}")));
        self.stats.last_fault_unix = Some(approvals::now());
        if self.stats.fault_log.len() >= limits::FAULTS_KEPT {
            self.stats.fault_log.pop_front();
        }
        self.stats.fault_log.push_back(history::FaultRecord { unix: approvals::now(), during: during.to_string(), text: clean(&text) });
        self.faults.push_back(now);
        while self.faults.front().is_some_and(|t| now.duration_since(*t) > limits::FAULT_WINDOW) {
            self.faults.pop_front();
        }
        self.note(pcds_plugin_api::Level::Error, &format!("{during}: {text}"));
        if self.faults.len() >= limits::MAX_FAULTS {
            let why = format!("switched off after {} faults in {} s; the last, in {during}: {text}", self.faults.len(), limits::FAULT_WINDOW.as_secs());
            self.note(pcds_plugin_api::Level::Error, &format!("{why} (pcds plugin reload starts it again)"));
            // No more events from now on; the instance is held until the host has made its last
            // call and saved its state (`Host::settle`, between events).
            self.status = Status::Failed(clean(&why));
            self.retire_due = self.rt.is_some();
            return;
        }
        self.stats.restarts += 1;
        let checkpoint = self.checkpoint.clone();
        if let Some(rt) = self.rt.as_mut()
            && let Err(e) = rt.restart(checkpoint.as_ref())
        {
            let why = format!("could not restart after a fault: {e}");
            self.note(pcds_plugin_api::Level::Error, &why);
            self.rt = None;
            self.status = Status::Failed(clean(&why));
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
    /// trusted-keys.json did not read: no key is trusted.
    pub keys_error: Option<String>,
    /// What the instances hold, counted (docs/plugins.md, "Loading and unloading while the world runs").
    pub gauge: Arc<wasm::Gauge>,
    /// The folder as it was when the plugins were read (`signature`).
    pub sig: Sig,
    /// `net.http`: the workers and the answers waiting to be delivered.
    pub net: net::Net,
    /// A host that sends nothing (`pcds plugin test`, `check`): the requests plugins make are kept here
    /// instead of going to the workers.
    pub offline: Option<Vec<net::Fetch>>,
    boot: Option<Boot>,
}

/// The world as it is while plugins are being started (`pcds_load` may ask about it), for the length of
/// one `start_all_in` or `rescan_in`: set there, cleared there.
#[derive(Clone, Copy)]
struct Boot(*const dyn World, i64);

// SAFETY: made and used inside one call on the game's thread (`Host::start_all_in`, `rescan_in`), then dropped.
unsafe impl Send for Boot {}
unsafe impl Sync for Boot {}

fn erase(world: &dyn World) -> *const dyn World {
    // SAFETY: only the lifetime is erased; `Boot` is used and dropped inside the call that made it.
    unsafe { std::mem::transmute::<&dyn World, &'static dyn World>(world) as *const dyn World }
}

/// Size and modification time of the files that decide what plugins there are: a change in any is
/// a reason to read the folder again.
pub type Sig = Vec<(PathBuf, u128, u64)>;

fn stat(p: &Path) -> (u128, u64) {
    std::fs::metadata(p).map(|m| (m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_nanos()), m.len())).unwrap_or((0, 0))
}

/// The signature of `dir`, given each known plugin's entry file.
fn signature_of(dir: &Path, entry_of: &HashMap<String, String>) -> Sig {
    let mut sig = vec![(dir.to_path_buf(), stat(dir)), (dir.join(approvals::FILE), stat(&dir.join(approvals::FILE))), (dir.join(config::FILE), stat(&dir.join(config::FILE))), (dir.join(sign::KEYS_FILE), stat(&dir.join(sign::KEYS_FILE)))];
    for folder in folders_of(dir) {
        let name = folder.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        sig.push((folder.join(manifest::FILE), stat(&folder.join(manifest::FILE))));
        sig.push((folder.join(sign::FILE), stat(&folder.join(sign::FILE))));
        if let Some(entry) = entry_of.get(&name) {
            sig.push((folder.join(entry), stat(&folder.join(entry))));
        }
        sig.push((folder.clone(), stat(&folder)));
    }
    sig.into_iter().map(|(p, (m, l))| (p, m, l)).collect()
}

/// The folders of `dir` that can hold plugins, by name.
fn folders_of(dir: &Path) -> Vec<PathBuf> {
    let mut folders: Vec<PathBuf> = std::fs::read_dir(dir).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.is_dir() && !p.file_name().is_some_and(|n| n.to_string_lossy().starts_with('.'))).collect();
    folders.sort();
    folders
}

/// Reads every folder of `dir` as a plugin: manifests, approvals, modules compiled, nothing started.
fn scan(engine: &wasmi::Engine, dir: &Path) -> (Vec<Loaded>, Option<String>, Option<String>) {
    let approvals = match Approvals::load(dir) {
        Ok(a) => (a, None),
        Err(e) => {
            warn!("plugins: {e}: no plugin runs until that is fixed");
            (Approvals::default(), Some(e))
        }
    };
    let (approvals, error) = approvals;
    // The admin's values for the plugins' [[config]] keys; a file that does not read means none.
    let values = match config::Configs::load(dir) {
        Ok(c) => c,
        Err(e) => {
            warn!("plugins: {e}: plugins run without their config values until that is fixed");
            config::Configs::default()
        }
    };
    // The keys the admin trusts; a file that does not read means none is trusted.
    let (keys, keys_error) = match sign::Keys::load(dir) {
        Ok(k) => (k, None),
        Err(e) => {
            warn!("plugins: {e}: no signing key is trusted until that is fixed");
            (sign::Keys::default(), Some(e))
        }
    };
    let mut folders = folders_of(dir);
    if folders.len() > limits::MAX_PLUGINS {
        warn!("plugins: {} folders in {}; only the first {} (by name) are read", folders.len(), dir.display(), limits::MAX_PLUGINS);
    }
    folders.truncate(limits::MAX_PLUGINS);
    let mut found = Vec::new();
    for folder in folders {
        let id = folder.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let approval = approvals.0.get(&id).filter(|_| error.is_none());
        let mut p = Loaded::inspect(engine, &folder, approval, values.0.get(&id), &keys);
        // Installed plugins live in a folder named for their id: it is how a command or a state file
        // finds its owner, and what `pcds plugin enable ID` names.
        if p.manifest.as_ref().is_some_and(|m| m.id() != id) {
            let id = clean(&id);
            let why = format!("the folder is {id} but the manifest's id is {}: the folder's name is the id", p.id);
            (p.id, p.status, p.pending, p.kinds, p.grant) = (id, Status::Invalid(why), None, 0, Grant::default());
        }
        found.push(p);
    }
    (found, error, keys_error)
}

/// Reads the plugin folder and prepares every plugin: manifests, approvals, modules compiled.
/// Nothing of a plugin runs yet, and the game is not needed, so a new bank does this while the
/// old one still plays: a hot update stops the world for the `attach` that follows, not for this.
pub fn prepare(dir: &Path, world: &str) -> Host {
    let mut host = Host::empty(dir, world);
    let (found, error, keys_error) = scan(&host.engine, dir);
    host.plugins = found;
    host.approvals_error = error;
    host.keys_error = keys_error;
    host.index_commands();
    host.sig = host.signature();
    host
}

impl Host {
    /// A host with no plugin.
    pub fn empty(dir: &Path, world: &str) -> Host {
        Host { dir: dir.to_path_buf(), world: world.to_string(), engine: wasm::engine(), plugins: Vec::new(), commands: HashMap::new(), interest: 0, rotate: 0, approvals_error: None, keys_error: None, gauge: Arc::default(), sig: Vec::new(), net: net::Net::default(), offline: None, boot: None }
    }

    /// The folder's files that matter, as they are now.
    pub fn signature(&self) -> Sig {
        signature_of(&self.dir, &self.plugins.iter().filter_map(|p| Some((p.id.clone(), p.entry_rel.clone()?))).collect())
    }

    /// The final call and save of a plugin switched off for faults, now that no event is running
    /// (the instance was held for it); and nothing else.
    pub fn settle(&mut self, saved: &mut state::Saved) {
        let world = self.world.clone();
        for p in self.plugins.iter_mut().filter(|p| p.retire_due) {
            let was = p.retire(saved, &world, "switched off for faults", true);
            p.status = was;
        }
        self.index_commands();
        self.recompute();
    }

    /// What each plugin's state is called, for `announce`.
    fn states(&self) -> HashMap<String, String> {
        self.plugins.iter().map(|p| (p.id.clone(), format!("{}{}", p.status.name(), p.notice.as_deref().map_or(String::new(), |n| format!(" ({n})"))))).collect()
    }

    /// One line in the log for each plugin whose state differs from `before`, and for each that is
    /// gone: a transition is said once.
    fn announce(&self, before: &HashMap<String, String>) {
        for p in &self.plugins {
            let now = format!("{}{}", p.status.name(), p.notice.as_deref().map_or(String::new(), |n| format!(" ({n})")));
            if before.get(&p.id) != Some(&now) {
                match p.status.detail() {
                    Some(d) if !matches!(p.status, Status::Running) => info!("plugin {}: {} ({d})", p.id, p.status.name()),
                    _ => info!("plugin {}: {now}", p.id),
                }
            }
        }
        for id in before.keys().filter(|id| !self.plugins.iter().any(|p| &p.id == *id)) {
            info!("plugin {id}: removed");
        }
    }

    /// Reads the folder again and makes the plugins what it says, between two events: a new folder
    /// is discovered (and starts when it is approved), a changed one is replaced with its state
    /// handed over (`save` of the old, `load` of the new, which reads the version), a disabled or
    /// removed one gets its last call and save and is dropped. A new version that does not start,
    /// or whose files wait for approval, leaves the old one running, and says so (`notice`). A plugin
    /// switched off for faults starts again only on an explicit reload (`explicit`), not when the
    /// folder merely changed elsewhere. Returns the summary line.
    pub fn rescan(&mut self, saved: &mut state::Saved, explicit: bool) -> String {
        let before = self.states();
        let boot = self.boot;
        let (fresh, error, keys_error) = scan(&self.engine, &self.dir);
        self.approvals_error = error;
        self.keys_error = keys_error;
        let mut old = std::mem::take(&mut self.plugins);
        let world = self.world.clone();
        let mut next: Vec<Loaded> = Vec::new();
        for mut p in fresh {
            let Some(i) = old.iter().position(|q| q.id == p.id) else {
                p.start(&self.engine, &self.gauge, saved, &world, boot);
                next.push(p);
                continue;
            };
            let mut prev = old.swap_remove(i);
            if prev.same_files(&p) && (prev.running() || (!explicit && matches!(prev.status, Status::Failed(_)))) {
                // Nothing changed for it: the running one stays, with its counters.
                prev.notice = None;
                prev.incoming = None;
                next.push(prev);
                continue;
            }
            if prev.running() && p.pending.is_some() && p.status == Status::Running {
                // A new version, approved: its state from the old one's save.
                let handed = match prev.rt.as_mut().map(|rt| rt.save()) {
                    Some(Ok(blob)) => blob,
                    Some(Err(f)) => {
                        error!("plugin {}: its save for the reload failed ({f}); the state of its last save is used", p.id);
                        prev.checkpoint.clone()
                    }
                    None => prev.checkpoint.clone(),
                };
                p.checkpoint = handed;
                p.logs = prev.logs.clone();
                p.start(&self.engine, &self.gauge, saved, &world, boot);
                if p.rt.is_some() {
                    p.stats.restarts = prev.stats.restarts;
                    if let Some(b) = &p.checkpoint {
                        saved.states.insert(p.id.clone(), b.clone());
                    }
                    next.push(p);
                } else {
                    // The new version did not start: the old goes on.
                    let why = match &p.status {
                        Status::Failed(w) => w.clone(),
                        other => other.detail().unwrap_or("it did not start").to_string(),
                    };
                    error!("plugin {}: the reload failed ({why}); the old version keeps running", p.id);
                    prev.notice = Some(format!("a reload failed: {why}; the old version keeps running"));
                    next.push(prev);
                }
                continue;
            }
            let torn = p.status == Status::Pending && matches!(p.signed, sign::Signed::Broken(_));
            if prev.running() && (torn || matches!(p.status, Status::Changed | Status::Invalid(_) | Status::Incompatible(_))) {
                // Files that cannot be loaded (or are not approved yet, or whose signature no longer
                // holds): the approved version, in memory, goes on until the admin disables it or
                // approves the new one.
                let why = match &p.status {
                    Status::Changed => "its files changed and wait for approval".to_string(),
                    Status::Pending => "its files changed and their signature does not hold".to_string(),
                    other => format!("its new files do not hold up: {}", other.detail().unwrap_or("")),
                };
                prev.notice = Some(format!("{why}; the old version keeps running"));
                prev.incoming = p.manifest.as_ref().map(|m| Incoming { version: m.plugin.version.clone(), requested: p.requested.clone(), allow: m.net.as_ref().map(|n| n.allow.clone()).unwrap_or_default(), digest: p.digest.clone() });
                next.push(prev);
                continue;
            }
            if prev.rt.is_some() {
                // Disabled, approval gone: the last call and save, then it goes.
                let why = match &p.status {
                    Status::Disabled => "disabled",
                    Status::Pending => "no longer approved",
                    _ => "switched off",
                };
                prev.retire(saved, &world, why, true);
            }
            if p.checkpoint.is_none() {
                p.checkpoint = prev.checkpoint.take();
            }
            // A plugin that is to run starts (an explicit reload of a failed one, or an enable).
            if p.pending.is_some() && p.status == Status::Running {
                p.start(&self.engine, &self.gauge, saved, &world, boot);
            }
            next.push(p);
        }
        // Folders that are gone: the plugin gets its last call, its state is not written (nowhere to).
        for mut gone in old {
            gone.retire(saved, &world, "its folder was removed", false);
            saved.states.remove(&gone.id);
        }
        self.plugins = next;
        self.index_commands();
        self.recompute();
        let ids: HashSet<&str> = self.plugins.iter().map(|p| p.id.as_str()).collect();
        saved.states.retain(|id, _| ids.contains(id.as_str()));
        self.sig = self.signature();
        self.announce(&before);
        self.summary_line()
    }

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
        // Timers are counted out in game ticks: a plugin that may set one wants the tick, though it did not subscribe to it.
        if self.plugins.iter().any(|p| p.running() && p.grant.has(caps::EVENT_TIMER)) {
            self.interest |= Kind::Tick.bit();
        }
        // A command is delivered as its own kind, found in chat messages: both bits are wanted.
        if self.plugins.iter().any(|p| p.running() && p.kinds & Kind::Command.bit() != 0) {
            self.interest |= Kind::Chat.bit();
        }
    }

    /// `start_all`, with the world there for the plugins' `load` to read. What they ask for in `load`
    /// is applied by `flush_boot` once the game is free.
    pub fn start_all_in(&mut self, saved: &state::Saved, world: &dyn World, game_ms: i64) {
        self.boot = Some(Boot(erase(world), game_ms));
        self.start_all(saved);
        self.boot = None;
    }

    /// `rescan`, with the world there for the new plugins' `load`.
    pub fn rescan_in(&mut self, saved: &mut state::Saved, explicit: bool, world: &dyn World, game_ms: i64) -> String {
        self.boot = Some(Boot(erase(world), game_ms));
        let line = self.rescan(saved, explicit);
        self.boot = None;
        line
    }

    /// Applies what plugins asked for while they loaded (a `say`, a `give`, a request).
    pub fn flush_boot(&mut self, sink: &mut dyn Sink) {
        for i in 0..self.plugins.len() {
            let queued = std::mem::take(&mut self.plugins[i].boot_queue);
            if !queued.is_empty() {
                self.dispatch_actions(i, sink, queued);
            }
        }
    }

    /// Starts every approved plugin with its state (from `saved`, else from disk).
    fn start_all(&mut self, saved: &state::Saved) {
        let boot = self.boot;
        let (engine, world) = (self.engine.clone(), self.world.clone());
        let gauge = self.gauge.clone();
        for p in &mut self.plugins {
            p.start(&engine, &gauge, saved, &world, boot);
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
            // Command events go only to the plugin that owns the command, and only when the player may
            // use it and typed it as the manifest says; otherwise the player is told why, by the host.
            if let Event::Command { command, client, args, .. } = event {
                if self.commands.get(&command.to_lowercase()) != Some(&i) {
                    continue;
                }
                if let Some(reply) = self.command_refusal(i, sink, command, *client, args.len()) {
                    let id = self.plugins[i].id.clone();
                    sink.apply(&id, vec![Action::Tell(*client, reply)]);
                    continue;
                }
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
            let event = if let Event::Tick { game_ms, tick, players, unix_ms, .. } = event {
                owned = Event::Tick { game_ms: *game_ms, tick: *tick, players: *players, unix_ms: *unix_ms, since_ms: if p.ticked { (*game_ms - p.last_tick).clamp(0, u32::MAX as i64) as u32 } else { 0 } };
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
            if self.call(i, sink, event, bytes, game_ms) == Verdict::Veto {
                verdict = Verdict::Veto;
                break;
            }
        }
        if is_tick {
            self.fire_timers(sink, game_ms, started);
        }
        verdict
    }

    /// One event of the game to the plugins: `run`, then, for a chat message that begins with the
    /// command prefix and names a plugin's command, that command. The verdict is the RPC's. (The caller
    /// then gives a plugin switched off for faults its last call: `settle`.)
    pub fn deliver(&mut self, sink: &mut dyn Sink, event: &Event<'_>, game_ms: i64) -> Verdict {
        let verdict = self.run(sink, event, game_ms);
        if let Event::Chat { client, name, text, player_id } = event
            && self.interest & Kind::Command.bit() != 0
        {
            let prefix = sink.command_prefix();
            if let Some((command, args)) = crate::chathooks::command(Some(&prefix), text) {
                // The name is matched without regard to case; the plugin is given it in lower case.
                let command = command.to_lowercase();
                let command = Event::Command { client: *client, name, command: &command, args: args.iter().map(String::as_str).collect(), text, player_id: *player_id };
                self.run(sink, &command, game_ms);
            }
        }
        verdict
    }

    /// Why a player may not use a command as typed, in words for the player; None if they may.
    fn command_refusal(&self, i: usize, sink: &dyn Sink, command: &str, client: u64, args: usize) -> Option<String> {
        let c = self.plugins[i].manifest.as_ref()?.commands.iter().find(|c| c.name.eq_ignore_ascii_case(command))?;
        let prefix = sink.command_prefix();
        if c.permission == manifest::Permission::Operator && !sink.operator(client) {
            return Some(format!("{prefix}{} is for the server's operators.", c.name));
        }
        let (least, most) = (c.min_args.unwrap_or(0) as usize, c.max_args.map_or(usize::MAX, |m| m as usize));
        if args < least || args > most {
            let usage = if c.usage.is_empty() { String::new() } else { format!(" {}", c.usage) };
            return Some(format!("Use: {prefix}{}{usage}", c.name));
        }
        None
    }

    /// Delivers the timers that are due to the plugins that set them: after the tick's own events, within
    /// the tick's budget (a plugin left out keeps its due timers for the next tick), at most
    /// `TIMER_EVENTS_PER_TICK` for each plugin.
    fn fire_timers(&mut self, sink: &mut dyn Sink, game_ms: i64, started: Instant) {
        for i in 0..self.plugins.len() {
            if !self.plugins[i].running() || !self.plugins[i].grant.has(caps::EVENT_TIMER) {
                continue;
            }
            let Some(rt) = self.plugins[i].rt.as_mut() else { continue };
            if rt.ctx().timers.is_empty() {
                continue;
            }
            if started.elapsed() > limits::TICK_BUDGET {
                self.plugins[i].stats.shed += 1;
                continue;
            }
            let due = rt.ctx().timers_due(game_ms, limits::TIMER_EVENTS_PER_TICK);
            for (id, late_ms) in due {
                let event = Event::Timer { id, game_ms, late_ms };
                let mut w = Writer::new();
                event.encode(&mut w);
                self.call(i, sink, &event, &w.0, game_ms);
                // A fault restarts the instance, which sets its timers again in `load`: the rest of
                // this round belonged to the one that was.
                if !self.plugins[i].running() || self.plugins[i].rt.as_mut().is_none_or(|rt| rt.ctx().timers.is_empty()) {
                    break;
                }
            }
        }
    }

    /// One call of plugin `i` with an event (`bytes`: the event as a module reads it), and what
    /// follows: its counters, its faults, its queued actions applied. Veto when it refused an RPC it may.
    fn call(&mut self, i: usize, sink: &mut dyn Sink, event: &Event<'_>, bytes: &[u8], game_ms: i64) -> Verdict {
        let is_tick = event.kind() == Kind::Tick;
        let mut verdict = Verdict::Continue;
        let p = &mut self.plugins[i];
        let Some(rt) = p.rt.as_mut() else { return verdict };
        rt.ctx().begin(&*sink, game_ms);
        let t = Instant::now();
        // A panic in the host's own code while it serves this plugin (a bug in a host function or in the world's view) is this plugin's
        // fault, counted as one: the plugin is restarted, the game goes on. (The actions are applied below, outside this: a panic in the
        // game's own code is the bank's to contain.)
        let result = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| rt.event(event, bytes))) {
            Ok(r) => r,
            Err(p) => {
                let what = p.downcast_ref::<String>().cloned().or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_default();
                Err(Fault::Trap(format!("the server panicked while it served this plugin ({}): the plugin is restarted", clean(&what))))
            }
        };
        let spent = t.elapsed();
        let (queued, denied, refused) = {
            let c = rt.ctx();
            (c.end(), std::mem::take(&mut c.denied), std::mem::take(&mut c.refused))
        };
        if rt.ctx().denied_caps.len() != p.denied_caps.len() {
            p.denied_caps = rt.ctx().denied_caps.clone();
        }
        p.stats.calls += 1;
        p.stats.nanos += spent.as_nanos() as u64;
        p.stats.max_nanos = p.stats.max_nanos.max(spent.as_nanos() as u64);
        p.stats.denied += denied as u64;
        p.stats.refused += refused as u64;
        if spent > limits::SLOW_CALL {
            p.stats.slow += 1;
            if p.last_slow_log.is_none_or(|t| t.elapsed() > Duration::from_secs(10)) {
                p.last_slow_log = Some(Instant::now());
                p.note(pcds_plugin_api::Level::Warn, &format!("{} took {} ms (slow calls are counted; the fuel limit stops runaways)", event.kind().name(), spent.as_millis()));
            }
        }
        if is_tick {
            p.last_tick = game_ms;
            p.ticked = true;
            p.next_tick = game_ms + p.limits.tick_every_ms as i64;
        }
        match result {
            Ok((v, cost)) => {
                p.stats.fuel += cost.fuel;
                p.stats.by_event[event.kind() as usize & 15].add(cost.fuel, spent.as_nanos() as u64);
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
                return Verdict::Continue;
            }
        }
        self.dispatch_actions(i, sink, queued);
        verdict
    }

    /// What plugin `i` asked for in a call: its HTTP requests go to the workers, the rest is applied to the game.
    fn dispatch_actions(&mut self, i: usize, sink: &mut dyn Sink, queued: Vec<Action>) {
        let (http, queued): (Vec<Action>, Vec<Action>) = queued.into_iter().partition(|a| matches!(a, Action::Http(_)));
        if !http.is_empty() {
            let p = &mut self.plugins[i];
            if let Some(net) = p.net.clone() {
                for action in http {
                    let Action::Http(fetch) = action else { continue };
                    p.stats.http_requests += 1;
                    p.stats.http_bytes_out += fetch.body.len() as u64;
                    match self.offline.as_mut() {
                        Some(kept) => kept.push(fetch),
                        None => self.net.submit(&p.id, fetch, net.policy.clone(), net.in_flight.clone()),
                    }
                }
            }
        }
        let applied = sink.apply(&self.plugins[i].id, queued);
        let p = &mut self.plugins[i];
        p.stats.actions += applied.done as u64;
        p.stats.refused += applied.refused as u64;
    }

    /// Whether requests of plugins are out and their answers not taken yet.
    pub fn net_waiting(&self) -> bool {
        self.net.outstanding > 0
    }

    /// One event straight to plugin `i`, whatever it subscribed to (the answer to its own HTTP request, in a test).
    pub fn send(&mut self, i: usize, sink: &mut dyn Sink, event: &Event<'_>, game_ms: i64) {
        let mut w = Writer::new();
        event.encode(&mut w);
        self.call(i, sink, event, &w.0, game_ms);
    }

    /// Hands finished HTTP requests to the plugins that made them, as `HttpDone` events, at most
    /// `HTTP_DELIVER` of them (the game thread does this between two ticks). An answer whose plugin is
    /// gone, switched off or no longer holds `net.http` is dropped. One line of the log for each request:
    /// the method, `scheme://host[:port]`, the result: never the path, the query or a header.
    pub fn deliver_http(&mut self, sink: &mut dyn Sink, game_ms: i64) {
        for done in self.net.take(limits::HTTP_DELIVER) {
            info!("{}", net::log_line(&done));
            let Some(i) = self.plugins.iter().position(|p| p.id == done.plugin && p.running() && p.grant.has(caps::NET_HTTP)) else { continue };
            let p = &mut self.plugins[i];
            let (mut status, mut error, mut body) = (done.status, done.error.as_str(), done.body.as_slice());
            // An answer the module's memory cannot hold would fault it for nothing it did.
            if body.len() > p.limits.memory_pages as usize * 65536 / 2 {
                (status, error, body) = (0, "the answer does not fit in the plugin's memory (raise budget.memory_pages or lower net.max_response_bytes)", &[]);
            }
            if status > 0 {
                p.stats.http_answered += 1;
            } else {
                p.stats.http_failed += 1;
            }
            p.stats.http_bytes_in += body.len() as u64;
            let headers: Vec<(&str, &str)> = done.headers.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
            let event = Event::HttpDone { ticket: done.ticket, status, error, body, headers };
            let mut w = Writer::new();
            event.encode(&mut w);
            self.call(i, sink, &event, &w.0, game_ms);
        }
    }

    /// Asks every running plugin for its state, within `budget` altogether; one the budget cannot
    /// wait for keeps the state of its last save. The states go to `saved` (the snapshot's).
    pub fn checkpoint(&mut self, saved: &mut state::Saved, budget: Option<Duration>) {
        let t = Instant::now();
        for p in self.plugins.iter_mut().filter(|p| p.running()) {
            if budget.is_some_and(|b| t.elapsed() > b) {
                warn!("plugin {}: no time to save its state for the hand-over; the state of its last save goes on", p.id);
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
        self.settle(saved);
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
                warn!("plugin {}: its state was not written: {e}", p.id);
            }
        }
    }
}

// ---- the game's entry points -------------------------------------------------------------------

/// Starts the prepared plugins in a game that is ready (a started world or a restored one). With
/// no plugin there is no host at all, and the game pays nothing. The folder may have changed since
/// the plugins were prepared (an approval given during a hot update, say): it is looked at once
/// more here, so the set that runs is the folder's at this moment, not at the compile.
pub fn attach(game: &mut crate::game::Game, mut host: Host, flipped: bool) {
    game.ext.plugins.place = Some((host.dir.clone(), host.world.clone()));
    let changed = host.signature() != host.sig;
    let game_ms = (game.time.secs() * 1000.0) as i64;
    // The state of the plugins is a part of the game: it is taken out while they start, which they do reading the world.
    let mut saved = std::mem::take(&mut game.ext.plugins.saved);
    if !host.is_empty() {
        host.start_all_in(&saved, &*game, game_ms);
        // State of plugins that are gone is not carried on.
        let ids: HashSet<&str> = host.plugins.iter().map(|p| p.id.as_str()).collect();
        saved.states.retain(|id, _| ids.contains(id.as_str()));
        info!("{}", host.summary_line());
        host.announce(&HashMap::new());
    }
    if changed {
        host.rescan_in(&mut saved, false, &*game, game_ms);
    }
    game.ext.plugins.saved = saved;
    host.flush_boot(game);
    game.ext.plugins.scan = Scan { next: Some(Instant::now() + POLL_EVERY), seen: host.sig.clone(), settling: None };
    game.ext.plugins.host = (!host.is_empty()).then(|| Box::new(host));
    game.ext.plugins.refresh();
    // A hot update: the plugins that asked hear that they were carried over, once, before the world runs on.
    if flipped && let Some(mut host) = game.ext.plugins.host.take() {
        host.run(game, &Event::Flip { game_ms, server_version: env!("CARGO_PKG_VERSION") }, game_ms);
        game.ext.plugins.host = Some(host);
        game.ext.plugins.refresh();
    }
}

/// The doorbell for the front's long polls of the event stream (`ipc::STREAM`): the body to send when
/// the stream has a newer event than the last bell told of, else none. The bank asks once a loop; it
/// costs two loads and a comparison, and a stream nobody listens to has no new events.
pub fn stream_bell(game: &mut crate::game::Game) -> Option<String> {
    let slot = &mut game.ext.plugins;
    let last = slot.saved.stream.last();
    if last <= slot.rung {
        return None;
    }
    slot.rung = last;
    Some(serde_json::json!({"epoch": slot.saved.stream.epoch, "last": last}).to_string())
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
    let game_ms = (game.time.secs() * 1000.0) as i64;
    verdict = host.deliver(game, event, game_ms);
    if host.plugins.iter().any(|p| p.retire_due) {
        host.settle(&mut game.ext.plugins.saved);
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

/// Reads the folder again now (the admin asked: the command line, the admin API, the dashboard):
/// see `Host::rescan`. A plugin switched off for faults starts again.
pub fn reload(game: &mut crate::game::Game) -> Result<String, String> {
    rescan_game(game, true)
}

fn rescan_game(game: &mut crate::game::Game, explicit: bool) -> Result<String, String> {
    let (dir, world) = game.ext.plugins.place.clone().ok_or("the plugin host has not started")?;
    let mut host = game.ext.plugins.host.take().map(|h| *h).unwrap_or_else(|| Host::empty(&dir, &world));
    let game_ms = (game.time.secs() * 1000.0) as i64;
    let mut saved = std::mem::take(&mut game.ext.plugins.saved);
    let line = host.rescan_in(&mut saved, explicit, &*game, game_ms);
    game.ext.plugins.saved = saved;
    host.flush_boot(game);
    game.ext.plugins.scan.seen = host.sig.clone();
    game.ext.plugins.scan.settling = None;
    game.ext.plugins.host = (!host.is_empty()).then(|| Box::new(host));
    game.ext.plugins.refresh();
    Ok(line)
}

/// Called by the bank's loop, between two events, as often as it likes: every `POLL_EVERY` it
/// looks at the folder, and reads it again when two looks in a row saw the same change (a new
/// folder, an approval, a file replaced, a folder removed). It also lets stream consumers that
/// stopped asking go. With nothing changed it is a few `stat`s.
pub fn poll(game: &mut crate::game::Game) {
    // Answers of plugins' HTTP requests are handed over every time (a test of a counter when none is out).
    if game.ext.plugins.host.as_ref().is_some_and(|h| h.net_waiting()) {
        deliver_http(game);
    }
    let slot = &mut game.ext.plugins;
    let now = Instant::now();
    if slot.place.is_none() || slot.scan.next.is_some_and(|t| now < t) {
        return;
    }
    slot.scan.next = Some(now + POLL_EVERY);
    poll_now(game);
}

/// The finished HTTP requests of plugins, to the plugins (`Host::deliver_http`), between two events.
pub(crate) fn deliver_http(game: &mut crate::game::Game) {
    let Some(mut host) = game.ext.plugins.host.take() else { return };
    let game_ms = (game.time.secs() * 1000.0) as i64;
    host.deliver_http(game, game_ms);
    if host.plugins.iter().any(|p| p.retire_due) {
        host.settle(&mut game.ext.plugins.saved);
    }
    game.ext.plugins.host = Some(host);
    game.ext.plugins.refresh();
}

/// `poll` without the timer.
pub(crate) fn poll_now(game: &mut crate::game::Game) {
    let slot = &mut game.ext.plugins;
    let Some((dir, _)) = slot.place.clone() else { return };
    slot.refresh();
    let current = match slot.host.as_ref() {
        Some(h) => h.signature(),
        None => signature_of(&dir, &HashMap::new()),
    };
    if current == slot.scan.seen {
        slot.scan.settling = None;
        return;
    }
    if slot.scan.settling.as_ref() != Some(&current) {
        slot.scan.settling = Some(current);
        return;
    }
    let _ = rescan_game(game, false);
}

// ---- approving, for the admin API and `pcds plugin` ---------------------------------------------

/// What a folder holds, read as far as the manifest and the entry go (no approval, nothing compiled).
pub fn read_folder(folder: &Path) -> Loaded {
    Loaded::inspect(&wasm::engine(), folder, None, None, &sign::Keys::default())
}

/// A file of a plugin's folder, at most `max` bytes of it. The folder is what an archive unpacked,
/// and every folder of plugins/ is read before anyone approves it: a named pipe in the place of a file
/// would block the open for ever (the game thread reads these), and a device such as /dev/zero has
/// a size of 0 and no end. Only a regular file is read (a link to one is followed), and it is
/// opened without waiting so that even a pipe is looked at and refused.
pub(crate) fn read_regular(path: &Path, max: usize) -> std::io::Result<Vec<u8>> {
    use std::io::{Error, ErrorKind, Read};
    use std::os::unix::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY).open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(Error::new(ErrorKind::InvalidInput, "not a regular file"));
    }
    if meta.len() > max as u64 {
        return Err(Error::new(ErrorKind::InvalidData, format!("{} bytes: at most {max} are loaded", meta.len())));
    }
    let mut bytes = Vec::with_capacity(meta.len() as usize);
    file.take(max as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > max {
        return Err(Error::new(ErrorKind::InvalidData, format!("over {max} bytes: at most {max} are loaded")));
    }
    Ok(bytes)
}

/// Text an author chose, made safe to put on a terminal and in a log: a control character (an escape
/// sequence that hides, redraws or sets the clipboard; a line break that forges a line), a mark that
/// turns the text around (U+202E) and the zero-width ones become `?`. Plain text is returned as it is.
pub fn clean(s: &str) -> String {
    s.chars().map(|c| if c.is_control() || matches!(c, '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{2064}' | '\u{2066}'..='\u{2069}' | '\u{feff}') { '?' } else { c }).collect()
}

/// The same, with the keys the admin trusts (for the signature to be judged).
pub fn read_folder_with(folder: &Path, keys: &sign::Keys) -> Loaded {
    Loaded::inspect(&wasm::engine(), folder, None, None, keys)
}

/// Approves a plugin: records `grant` (default: everything it asks for; never more) and the digest
/// of the files as they are now, and enables it. The server reads it at the next reload.
#[cfg(test)]
pub fn approve(dir: &Path, id: &str, grant: Option<Vec<String>>) -> Result<Approval, String> {
    approve_digest(dir, id, grant, None)
}

/// `approve`, for the files whose digest the admin was shown (`expect`): the approval records the
/// digest of the files it reads now, so files swapped between the screen and the answer (a folder a
/// sync or a `git pull` keeps current, a plugin's author with the folder open) would be approved
/// unseen. They are refused instead.
pub fn approve_digest(dir: &Path, id: &str, grant: Option<Vec<String>>, expect: Option<&str>) -> Result<Approval, String> {
    if !manifest::valid_id(id) {
        return Err(format!("{id:?} is not a plugin id"));
    }
    let keys = sign::Keys::load(dir).unwrap_or_default();
    let found = read_folder_with(&dir.join(id), &keys);
    if let Status::Invalid(why) | Status::Incompatible(why) = &found.status {
        return Err(format!("{id}: {why}"));
    }
    if found.id != id {
        return Err(format!("{id}: the manifest's id is {}: the folder's name is the id", found.id));
    }
    if let Some(shown) = expect.filter(|d| *d != found.digest) {
        let short = |d: &str| d.chars().take(12).collect::<String>();
        return Err(format!("{id}: its files changed after you looked at them (digest {} then, {} now): nothing approved; look at it again", short(shown), short(&found.digest)));
    }
    let grant = grant.unwrap_or_else(|| found.requested.clone());
    if let Some(extra) = grant.iter().find(|g| !found.requested.contains(g)) {
        return Err(format!("{id} does not ask for {extra}: it can only be granted what the manifest asks for ({})", found.requested.join(", ")));
    }
    let mut all = Approvals::load(dir)?;
    let limits = all.0.get(id).map(|a| a.limits.clone()).unwrap_or_default();
    let signer = found.signed.trusted().map(|k| k.id.clone());
    let allow = found.manifest.as_ref().and_then(|m| m.net.as_ref()).map(|n| n.allow.clone()).unwrap_or_default();
    let approval = Approval { enabled: true, grant, digest: found.digest, limits, approved_at: approvals::now(), signer, asked: found.requested.clone(), allow };
    all.0.insert(id.to_string(), approval.clone());
    all.save(dir)?;
    Ok(approval)
}

/// Switches an approved plugin off (it stays approved with the same grant for `approve` to switch on).
pub fn disable(dir: &Path, id: &str) -> Result<(), String> {
    let mut all = Approvals::load(dir)?;
    // A plugin that runs because a trusted key vouches for it has no approval: its switching off is one.
    if !all.0.contains_key(id) && (!manifest::valid_id(id) || !dir.join(id).is_dir()) {
        return Err(format!("{id} is not an installed plugin"));
    }
    let a = all.0.entry(id.to_string()).or_insert_with(|| Approval { enabled: false, grant: Vec::new(), digest: String::new(), limits: Default::default(), approved_at: approvals::now(), signer: None, asked: Vec::new(), allow: Vec::new() });
    a.enabled = false;
    all.save(dir)
}

/// Whether a plugin wants the RPC named `name` (so the event's arguments are only built then).
pub fn wants_rpc(game: &crate::game::Game, name: &str) -> bool {
    game.ext.plugins.host.as_ref().is_some_and(|h| h.plugins.iter().any(|p| p.running() && p.rpc.contains(name)))
}
