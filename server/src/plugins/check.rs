//! `pcds plugin check`: everything that can be told about a plugin before it is installed, and a
//! smoke test: the plugin is started and shown a sample of each event it subscribed to, in a canned
//! world, to see that it does not trap or run out of its budget on the first day.

use std::path::Path;

use pcds_plugin_api::query::{PlayerInfo, WorldInfo};
use pcds_plugin_api::{Event, Kind, caps};

use super::ctx::{Action, Ctx, Grant, World};
use super::manifest::PluginKind;
use super::runtime::Runtime;
use super::{Applied, Loaded, Sink, Status};

#[derive(Debug, Default)]
pub struct Report {
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
    /// What the plugin is, for the admin to read before approving.
    pub facts: Vec<String>,
}

/// A world of two players on Prime, for the smoke test.
#[derive(Default)]
struct Canned {
    /// HTTP requests the samples made the plugin ask for (none is sent).
    http_asked: u32,
}

impl World for Canned {
    fn info(&self) -> WorldInfo {
        WorldInfo { game_ms: 3_725_000, planet: "Prime".into(), players: 16, world_objects: 900, network_objects: 1200, terraformation: 1234.5, standing_still: false, unix_ms: 1_760_000_000_000, command_prefix: "!".into() }
    }
    fn players(&self) -> Vec<PlayerInfo> {
        // A busy server: the samples show what a plugin costs with sixteen players in, not two.
        (1..=16u64).map(|c| PlayerInfo { client: c, name: if c == 1 { "Ann".into() } else if c == 2 { "Bo".into() } else { format!("Player{c}") }, position: [10.0 * c as f32, 2.0, 30.0], gauges: [100.0, 80.0, 90.0, 0.0], player_id: c + 1000, ..Default::default() }).collect()
    }
    fn inventory(&self, client: u64) -> Option<pcds_plugin_api::InventoryInfo> {
        (client <= 16).then(|| pcds_plugin_api::InventoryInfo { slots_used: 3, slots_total: 30, items: vec![pcds_plugin_api::query::ItemCount { group: "Iron".into(), count: 3 }] })
    }
    fn rules(&self) -> String {
        "{}".into()
    }
}

impl Sink for Canned {
    fn apply(&mut self, _: &str, actions: Vec<Action>) -> Applied {
        self.http_asked += actions.iter().filter(|a| matches!(a, Action::Http(_))).count() as u32;
        Applied { done: actions.len() as u32, refused: 0 }
    }
    fn command_prefix(&self) -> String {
        "!".into()
    }
}

/// The capability each host function needs, by its import name.
fn import_cap(import: &str) -> Option<&'static str> {
    Some(match import.strip_prefix("pcds.")? {
                "say" | "tell" => caps::CHAT_SAY,
        "kick" => caps::PLAYER_KICK,
        "teleport" => caps::PLAYER_TELEPORT,
        "rules_patch" => caps::RULES_WRITE,
        "http_request" => caps::NET_HTTP,
        "give" => caps::PLAYER_GIVE,
        "tokens" => caps::WORLD_TOKENS,
        "timer_set" | "timer_cancel" => caps::EVENT_TIMER,
        _ => return None,
    })
}

fn sample(kind: Kind, command: Option<&str>, rpc: Option<&str>, args: usize) -> Event<'static> {
    match kind {
        Kind::Tick => Event::Tick { game_ms: 3_725_000, tick: 744_000, players: 2, since_ms: 1000, unix_ms: 1_760_000_000_000 },
        Kind::PlayerJoin => Event::PlayerJoin { client: 1, name: "Ann", steam_id: "76561198000000000", player_id: 76561198000000000 },
        Kind::PlayerLeave => Event::PlayerLeave { client: 1, name: "Ann", player_id: 76561198000000000 },
        Kind::Chat => Event::Chat { client: 1, name: "Ann", text: "hello everyone", player_id: 76561198000000000 },
        Kind::Command => Event::Command { client: 1, name: "Ann", command: Box::leak(command.unwrap_or("help").to_string().into_boxed_str()), args: ["one", "two", "three"][..args.min(3)].to_vec(), text: "!command one two three", player_id: 76561198000000000 },
        Kind::RpcBefore => Event::RpcBefore { client: 1, rpc: Box::leak(rpc.unwrap_or("Some.Rpc").to_string().into_boxed_str()), object: 42, args_json: "[1,2,3]" },
        Kind::RpcAfter => Event::RpcAfter { client: 1, rpc: Box::leak(rpc.unwrap_or("Some.Rpc").to_string().into_boxed_str()), object: 42, args_json: "[1,2,3]" },
        Kind::ObjectPlaced => Event::ObjectPlaced { id: 100, group: "Drill1", position: [1.0, 2.0, 3.0], client: 1 },
        Kind::ObjectDestroyed => Event::ObjectDestroyed { id: 100, group: "Drill1", position: [1.0, 2.0, 3.0], client: 1 },
        Kind::WorldSaved => Event::WorldSaved { game_ms: 3_725_000, ok: true },
        Kind::RuleChange => Event::RuleChange { changes_json: "{\"speed.growth\":2.0}" },
        Kind::HttpDone => Event::HttpDone { ticket: 1, status: 200, error: "", body: b"{}", headers: vec![("content-type", "application/json")] },
        Kind::Timer => Event::Timer { id: 1, game_ms: 3_725_000, late_ms: 0 },
        Kind::Flip => Event::Flip { game_ms: 3_725_000, server_version: "0.0.0" },
    }
}

/// Checks the plugin in `folder`. `installed`: the plugins folder, to find commands another plugin
/// already owns.
pub fn check_folder(folder: &Path, installed: Option<&Path>) -> Report {
    let mut report = Report::default();
    let engine = super::wasm::engine();
    // As an approved plugin with everything granted, so what the host would do with it is done.
    let keys = installed.and_then(|d| super::sign::Keys::load(d).ok()).unwrap_or_default();
    let mut found = Loaded::inspect(&engine, folder, None, None, &keys);
    let Some(m) = found.manifest.clone() else {
        report.errors.push(found.status.detail().unwrap_or("not a plugin").to_string());
        return report;
    };
    if let Status::Invalid(why) | Status::Incompatible(why) = &found.status {
        report.errors.push(why.clone());
        return report;
    }
    let p = &m.plugin;
    report.facts.push(format!("{} {} ({}, plugin ABI {})", p.name, p.version, p.kind.name(), p.abi));
    for (name, allows) in caps::ALL.iter().filter(|(n, _)| p.capabilities.iter().any(|c| c == n)) {
        report.facts.push(format!("asks for {name}: {allows}"));
    }
    if let Some(net) = &m.net {
        report.facts.push(format!("may send HTTP requests to: {}", net.allow.join(", ")));
        if net.allow.iter().any(|a| a.contains("://*.")) {
            report.warnings.push("net.allow has a wildcard: the plugin may reach every name below it, whoever runs them".into());
        }
        if net.allow.iter().any(|a| a.starts_with("http://")) {
            report.warnings.push("net.allow has a plain http:// pattern: those requests (and what they carry) can be read on the way".into());
        }
    }
    for k in &m.config {
        report.facts.push(format!("config {}: {}{}", k.key, if k.description.is_empty() { "(no description)" } else { &k.description }, if k.is_secret() { " (a secret)" } else { "" }));
    }
    match &found.signed {
        super::sign::Signed::No => report.facts.push("unsigned".into()),
        super::sign::Signed::Broken(why) => report.warnings.push(format!("{why}: it will be judged as if unsigned")),
        other => report.facts.push(other.describe(&p.capabilities)),
    }
    if p.capabilities.is_empty() {
        report.facts.push("asks for no capability: it can read nothing and change nothing".into());
    }
    // Prepare it as the host would after approval: read the entry again with the grant.
    let entry = match super::read_regular(&folder.join(&p.entry), super::limits::MAX_MODULE_BYTES) {
        Ok(e) => e,
        Err(e) => {
            let build = if matches!(p.kind, PluginKind::Wasm) && e.kind() == std::io::ErrorKind::NotFound { " (build it first: 'cargo build --release' in the plugin's folder, README.md there)" } else { "" };
            report.errors.push(format!("{}: {e}{build}", p.entry));
            return report;
        }
    };
    let grant = Grant::from_names(p.capabilities.iter().map(String::as_str));
    let mut runtime: Option<Box<dyn Runtime>> = None;
    let mut kinds = m.kinds();
    match p.kind {
        PluginKind::Wasm => match super::wasm::compile(&engine, &entry) {
            Err(e) => report.errors.push(e),
            Ok((module, inspected)) => {
                report.facts.push(format!("module of {} bytes, {} imports", inspected.bytes, inspected.imports.len()));
                let used: std::collections::BTreeSet<&str> = inspected.imports.iter().filter_map(|i| import_cap(i)).collect();
                for cap in &used {
                    if !grant.has(cap) {
                        report.errors.push(format!("the module calls a host function that needs {cap}, which the manifest does not ask for: every such call would be refused"));
                    }
                }
                for cap in p.capabilities.iter().filter(|c| import_cap_needed(c) && !used.contains(c.as_str())) {
                    report.warnings.push(format!("the manifest asks for {cap}, but the module imports nothing that needs it"));
                }
                // A setting that is a secret cannot be read by the plugin: it is only put into a request. A key the module never names
                // as `${config.KEY}` and that is secret is a setting the plugin cannot see.
                for k in m.config.iter().filter(|k| k.is_secret()) {
                    let needle = format!("config.{}", k.key);
                    if !entry.windows(needle.len()).any(|w| w == needle.as_bytes()) {
                        report.warnings.push(format!("the setting {} is a secret (the default for a string), and the module never uses ${{config.{}}}: the plugin cannot read it. Say secret = false if the plugin reads it with host.config()", k.key, k.key));
                    }
                }
                let limits = super::limits::Limits::resolve(&m.budget, &Default::default());
                match super::wasm::Wasm::new(&engine, &module, { let mut c = Ctx::new(&p.id, grant); c.settings = found.settings.clone(); c }, limits, Default::default()) {
                    Ok(w) => runtime = Some(Box::new(w)),
                    Err(e) => report.errors.push(format!("the module does not start: {e}")),
                }
            }
        },
        PluginKind::Data => match super::data::Rules::parse(&String::from_utf8_lossy(&entry)) {
            Err(e) => report.errors.push(format!("{}: {e}", p.entry)),
            Ok(rules) => {
                let needed = rules.needed_caps();
                for cap in needed.iter().filter(|c| !grant.has(c)) {
                    report.errors.push(format!("the rules need {cap}, which the manifest does not ask for"));
                }
                for cap in p.capabilities.iter().filter(|c| !needed.contains(c.as_str())) {
                    report.warnings.push(format!("the manifest asks for {cap}, but the rules never use it"));
                }
                report.facts.push(format!("rules: {} commands, {} joins, {} leaves, {} announcements, {} rule switches", rules.commands.len(), rules.on_join.len(), rules.on_leave.len(), rules.every.len(), rules.when_players.len()));
                kinds = rules.kinds();
                found.commands = rules.command_names();
                runtime = Some(Box::new(super::data::Data::new(rules, { let mut c = Ctx::new(&p.id, grant); c.settings = found.settings.clone(); c })));
            }
        },
    }
    if kinds.is_empty() {
        report.warnings.push("the plugin subscribes to no event and has no command: it will never be called".into());
    }
    if !found.commands.is_empty() {
        report.facts.push(format!("commands: {}", found.commands.iter().map(|c| format!("!{c}")).collect::<Vec<_>>().join(" ")));
    }
    // Commands another installed plugin owns.
    if let Some(dir) = installed {
        let this = std::fs::canonicalize(folder).ok();
        for other in std::fs::read_dir(dir).into_iter().flatten().flatten().map(|e| e.path()).filter(|f| f.is_dir() && std::fs::canonicalize(f).ok() != this) {
            let o = Loaded::inspect(&engine, &other, None, None, &keys);
            for c in o.commands.iter().filter(|c| found.commands.contains(c)) {
                report.warnings.push(format!("the command !{c} is also declared by the installed plugin {}: the one first by id gets it", o.id));
            }
        }
    }
    // The smoke test: start it, then show it an event of each kind it wants.
    if let Some(mut rt) = runtime {
        // Started as the server starts it: the world there to read.
        let canned = Canned::default();
        rt.ctx().begin(&canned, 3_725_000);
        let started = rt.start(None);
        let _ = rt.ctx().end();
        match started {
            Err(f) => report.errors.push(format!("start: {f}")),
            Ok(()) => {
                let mut world = Canned::default();
                for kind in &kinds {
                    if !grant.has(caps::for_event(*kind)) {
                        continue;
                    }
                    let names: Vec<String> = match kind {
                        Kind::Command => found.commands.clone(),
                        Kind::RpcBefore | Kind::RpcAfter => m.events.rpc.clone(),
                        _ => vec![String::new()],
                    };
                    for name in names {
                        let (command, rpc) = ((*kind == Kind::Command).then_some(name.as_str()), matches!(kind, Kind::RpcBefore | Kind::RpcAfter).then_some(name.as_str()));
                        // A command is typed the way its manifest says: the fewest arguments it takes.
                        let args = m.commands.iter().find(|c| Some(c.name.to_lowercase().as_str()) == command).and_then(|c| c.min_args).unwrap_or(0) as usize;
                        let event = sample(*kind, command, rpc, args);
                        // The first call of an instance is dearer: the engine translates the functions it runs for the first time and
                        // charges for it. The sample is the second call; the first is shown beside it, because it is the one a budget must hold.
                        let (first, _, _) = call_once(&mut rt, &mut world, &p.id, &event);
                        let (second, asked, denied) = call_once(&mut rt, &mut world, &p.id, &event);
                        let label = match (command, rpc) {
                            (Some(c), _) => format!("{} !{c}", kind.name()),
                            (_, Some(r)) => format!("{} {r}", kind.name()),
                            _ => kind.name().to_string(),
                        };
                        match (first, second) {
                            (Ok(first), Ok(second)) => {
                                let budget = found.limits.fuel_per_event;
                                report.facts.push(format!("{label} sample: ok, {} fuel ({:.1}% of the {budget} budget), first call {} fuel, {asked} actions asked", second.fuel, second.fuel as f64 * 100.0 / budget as f64, first.fuel));
                                if first.fuel * 2 > budget {
                                    report.warnings.push(format!("{label}: the first call used {} of {budget} fuel: close to the budget with 16 players online. Ask for more (budget.fuel_per_event, at most {}) or do less in one call", first.fuel, super::limits::MAX_FUEL_PER_EVENT));
                                }
                            }
                            (Err(f), _) | (_, Err(f)) => report.errors.push(format!("{label} sample: {f}")),
                        }
                        if denied > 0 {
                            report.warnings.push(format!("{label} sample: {denied} calls were refused for a missing capability"));
                        }
                    }
                }
                // A timer: delivered to a plugin that may set one.
                if grant.has(caps::EVENT_TIMER) {
                    let event = Event::Timer { id: 1, game_ms: 3_725_000, late_ms: 0 };
                    let (first, _, _) = call_once(&mut rt, &mut world, &p.id, &event);
                    match first {
                        Ok(c) => report.facts.push(format!("timer sample (id 1): ok, {} fuel ({:.1}% of the {} budget)", c.fuel, c.fuel as f64 * 100.0 / found.limits.fuel_per_event as f64, found.limits.fuel_per_event)),
                        Err(f) => report.errors.push(format!("timer sample (id 1): {f}")),
                    }
                }
                // A plugin that may send requests is shown answers of each sort (it hears them in the host
                // whatever it subscribed to); what it asks for in return is not sent from here.
                if grant.has(caps::NET_HTTP) {
                    let answers: [(&str, Event<'static>); 3] = [
                        ("an answer of 200", Event::HttpDone { ticket: 1, status: 200, error: "", body: b"{}", headers: vec![("content-type", "application/json")] }),
                        ("an answer of 500", Event::HttpDone { ticket: 1, status: 500, error: "", body: b"oops", headers: vec![] }),
                        ("a request that failed", Event::HttpDone { ticket: 1, status: 0, error: "timed out", body: &[], headers: vec![] }),
                    ];
                    for (what, event) in answers {
                        let mut payload = pcds_plugin_api::Writer::new();
                        event.encode(&mut payload);
                        rt.ctx().begin(&world, 3_725_000);
                        let result = rt.event(&event, &payload.0);
                        let (queued, denied) = {
                            let c = rt.ctx();
                            (c.end(), std::mem::take(&mut c.denied))
                        };
                        let asked = queued.iter().filter(|a| matches!(a, Action::Http(_))).count();
                        world.http_asked += asked as u32;
                        match result {
                            Ok((_, cost)) => report.facts.push(format!("http_done sample ({what}): ok, {} fuel, {asked} requests asked (check sends none)", cost.fuel)),
                            Err(f) => report.errors.push(format!("http_done sample ({what}): {f}")),
                        }
                        if denied > 0 {
                            report.warnings.push(format!("http_done sample: {denied} calls were refused for a missing capability"));
                        }
                    }
                    let asked = world.http_asked;
                    if asked > 0 {
                        report.facts.push(format!("the samples made it ask for {asked} HTTP requests; check sends none (a request is sent only by a running server, to the hosts it lists)"));
                    }
                }
                match rt.save() {
                    Ok(Some(b)) => report.facts.push(format!("state: {} bytes, version {}", b.bytes.len(), b.version)),
                    Ok(None) => report.facts.push("keeps no state".into()),
                    Err(f) => report.errors.push(format!("save: {f}")),
                }
            }
        }
    }
    report
}

/// Capabilities that a host function stands for (`query` is not one: it needs `world.read` for the world and nothing for the
/// plugin's own settings, and which it asks for is not in the module).
/// One event to the started plugin in the canned world: the outcome, the actions it asked for, the calls refused.
fn call_once(rt: &mut Box<dyn Runtime>, world: &mut Canned, id: &str, event: &Event<'_>) -> (Result<super::runtime::Cost, super::runtime::Fault>, usize, u32) {
    let mut payload = pcds_plugin_api::Writer::new();
    event.encode(&mut payload);
    rt.ctx().begin(&*world, 3_725_000);
    let result = rt.event(event, &payload.0).map(|(_, cost)| cost);
    let (queued, denied) = {
        let c = rt.ctx();
        (c.end(), std::mem::take(&mut c.denied))
    };
    let n = queued.len();
    let _ = world.apply(id, queued);
    (result, n, denied)
}

fn import_cap_needed(cap: &str) -> bool {
    [caps::CHAT_SAY, caps::PLAYER_KICK, caps::PLAYER_TELEPORT, caps::RULES_WRITE, caps::NET_HTTP, caps::PLAYER_GIVE, caps::WORLD_TOKENS, caps::EVENT_TIMER].contains(&cap)
}
