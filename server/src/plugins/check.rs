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
struct Canned;

impl World for Canned {
    fn info(&self) -> WorldInfo {
        WorldInfo { game_ms: 3_725_000, planet: "Prime".into(), players: 2, world_objects: 900, network_objects: 1200, terraformation: 1234.5, standing_still: false }
    }
    fn players(&self) -> Vec<PlayerInfo> {
        vec![PlayerInfo { client: 1, name: "Ann".into(), position: [10.0, 2.0, 30.0], gauges: [100.0, 80.0, 90.0, 0.0], ..Default::default() }, PlayerInfo { client: 2, name: "Bo".into(), ..Default::default() }]
    }
    fn rules(&self) -> String {
        "{}".into()
    }
}

impl Sink for Canned {
    fn apply(&mut self, _: &str, actions: Vec<Action>) -> Applied {
        Applied { done: actions.len() as u32, refused: 0 }
    }
    fn command_prefix(&self) -> String {
        "!".into()
    }
}

/// The capability each host function needs, by its import name.
fn import_cap(import: &str) -> Option<&'static str> {
    Some(match import.strip_prefix("pcds.")? {
        "query" => caps::WORLD_READ,
        "say" | "tell" => caps::CHAT_SAY,
        "kick" => caps::PLAYER_KICK,
        "teleport" => caps::PLAYER_TELEPORT,
        "rules_patch" => caps::RULES_WRITE,
        "http_request" => caps::NET_HTTP,
        _ => return None,
    })
}

fn sample(kind: Kind, command: Option<&str>, rpc: Option<&str>) -> Event<'static> {
    match kind {
        Kind::Tick => Event::Tick { game_ms: 3_725_000, tick: 744_000, players: 2, since_ms: 1000 },
        Kind::PlayerJoin => Event::PlayerJoin { client: 1, name: "Ann", steam_id: "76561198000000000" },
        Kind::PlayerLeave => Event::PlayerLeave { client: 1, name: "Ann" },
        Kind::Chat => Event::Chat { client: 1, name: "Ann", text: "hello everyone" },
        Kind::Command => Event::Command { client: 1, name: "Ann", command: Box::leak(command.unwrap_or("help").to_string().into_boxed_str()), args: vec!["one", "two"], text: "!command one two" },
        Kind::RpcBefore => Event::RpcBefore { client: 1, rpc: Box::leak(rpc.unwrap_or("Some.Rpc").to_string().into_boxed_str()), object: 42, args_json: "[1,2,3]" },
        Kind::RpcAfter => Event::RpcAfter { client: 1, rpc: Box::leak(rpc.unwrap_or("Some.Rpc").to_string().into_boxed_str()), object: 42, args_json: "[1,2,3]" },
        Kind::ObjectPlaced => Event::ObjectPlaced { id: 100, group: "Drill1", position: [1.0, 2.0, 3.0] },
        Kind::ObjectDestroyed => Event::ObjectDestroyed { id: 100, group: "Drill1", position: [1.0, 2.0, 3.0] },
        Kind::WorldSaved => Event::WorldSaved { game_ms: 3_725_000, ok: true },
        Kind::RuleChange => Event::RuleChange { changes_json: "{\"speed.growth\":2.0}" },
        Kind::HttpDone => Event::HttpDone { ticket: 1, status: 200, error: "", body: b"{}", headers: vec![("content-type", "application/json")] },
    }
}

/// Checks the plugin in `folder`. `installed`: the plugins folder, to find commands another plugin
/// already owns.
pub fn check_folder(folder: &Path, installed: Option<&Path>) -> Report {
    let mut report = Report::default();
    let engine = super::wasm::engine();
    // As an approved plugin with everything granted, so what the host would do with it is done.
    let mut found = Loaded::inspect(&engine, folder, None, None);
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
    if p.capabilities.is_empty() {
        report.facts.push("asks for no capability: it can read nothing and change nothing".into());
    }
    // Prepare it as the host would after approval: read the entry again with the grant.
    let entry = match std::fs::read(folder.join(&p.entry)) {
        Ok(e) => e,
        Err(e) => {
            report.errors.push(format!("{}: {e}", p.entry));
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
                let limits = super::limits::Limits::resolve(&m.budget, &Default::default());
                match super::wasm::Wasm::new(&engine, &module, Ctx::new(&p.id, grant), limits, Default::default()) {
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
                runtime = Some(Box::new(super::data::Data::new(rules, Ctx::new(&p.id, grant))));
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
            let o = Loaded::inspect(&engine, &other, None, None);
            for c in o.commands.iter().filter(|c| found.commands.contains(c)) {
                report.warnings.push(format!("the command !{c} is also declared by the installed plugin {}: the one first by id gets it", o.id));
            }
        }
    }
    // The smoke test: start it, then show it an event of each kind it wants.
    if let Some(mut rt) = runtime {
        match rt.start(None) {
            Err(f) => report.errors.push(format!("start: {f}")),
            Ok(()) => {
                let mut world = Canned;
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
                        let event = sample(*kind, command, rpc);
                        let mut payload = pcds_plugin_api::Writer::new();
                        event.encode(&mut payload);
                        rt.ctx().begin(&world);
                        let result = rt.event(&event, &payload.0);
                        let (queued, denied) = {
                            let c = rt.ctx();
                            (c.end(), std::mem::take(&mut c.denied))
                        };
                        match result {
                            Ok((_, cost)) => report.facts.push(format!("{} sample: ok, {} fuel, {} actions asked", kind.name(), cost.fuel, queued.len())),
                            Err(f) => report.errors.push(format!("{} sample: {f}", kind.name())),
                        }
                        if denied > 0 {
                            report.warnings.push(format!("{} sample: {denied} calls were refused for a missing capability", kind.name()));
                        }
                        let _ = world.apply(&p.id, queued);
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

fn import_cap_needed(cap: &str) -> bool {
    [caps::WORLD_READ, caps::CHAT_SAY, caps::PLAYER_KICK, caps::PLAYER_TELEPORT, caps::RULES_WRITE, caps::NET_HTTP].contains(&cap)
}
