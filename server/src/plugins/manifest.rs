//! plugin.toml: what a plugin is, what it asks for and what it costs (docs/plugins.md). The
//! manifest is read before any code of the plugin runs: the host knows from it which events to
//! hand the plugin, which commands are its own and which capabilities the admin is asked to
//! grant.

use std::collections::BTreeSet;

use pcds_plugin_api::{Kind, caps};
use serde::Deserialize;

/// The plugin ABI major this server speaks (pcds_plugin_api::ABI_MAJOR) and the newest minor.
pub use pcds_plugin_api::ABI_MAJOR;

/// The file name of a manifest inside a plugin's folder.
pub const FILE: &str = "plugin.toml";

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub plugin: Meta,
    #[serde(default)]
    pub budget: Budget,
    #[serde(default)]
    pub events: Events,
    #[serde(default, rename = "command")]
    pub commands: Vec<Command>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Meta {
    pub id: String,
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub authors: Vec<String>,
    #[serde(default)]
    pub license: String,
    pub kind: PluginKind,
    /// The module (wasm) or the rules file (data), relative to the plugin's folder.
    pub entry: String,
    /// The plugin ABI major the plugin was built for.
    pub abi: u32,
    /// The oldest server version the plugin runs on.
    pub min_server: Option<String>,
    #[serde(default)]
    pub capabilities: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PluginKind {
    Wasm,
    Data,
}

impl PluginKind {
    pub fn name(self) -> &'static str {
        match self {
            PluginKind::Wasm => "wasm",
            PluginKind::Data => "data",
        }
    }
}

/// What the plugin asks for; the host's own limits (limits.rs) cap each, and the admin may lower
/// them in approved.json.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Budget {
    pub fuel_per_event: Option<u64>,
    pub fuel_per_save: Option<u64>,
    pub memory_pages: Option<u32>,
    pub tick_every_ms: Option<u32>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Events {
    /// Event kind names (Kind::name), besides chat commands, which `[[command]]` declares.
    #[serde(default)]
    pub subscribe: Vec<String>,
    /// The RPC names `rpc_before` and `rpc_after` are delivered for.
    #[serde(default)]
    pub rpc: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Command {
    pub name: String,
    #[serde(default)]
    pub help: String,
}

pub fn valid_id(id: &str) -> bool {
    let mut chars = id.chars();
    id.len() <= 32 && chars.next().is_some_and(|c| c.is_ascii_lowercase()) && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// "1.2.3" as numbers (a pre-release or build suffix is refused: plugin versions are plain).
pub fn version_triple(v: &str) -> Option<(u64, u64, u64)> {
    let mut parts = v.split('.');
    let n = |p: Option<&str>| p.filter(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit())).and_then(|p| p.parse().ok());
    let t = (n(parts.next())?, n(parts.next())?, n(parts.next())?);
    parts.next().is_none().then_some(t)
}

impl Manifest {
    pub fn parse(text: &str) -> Result<Manifest, String> {
        let m: Manifest = toml::from_str(text).map_err(|e| e.to_string())?;
        m.validate()?;
        Ok(m)
    }

    fn validate(&self) -> Result<(), String> {
        let p = &self.plugin;
        if !valid_id(&p.id) {
            return Err(format!("plugin.id {:?}: a lowercase letter, then lowercase letters, digits and dashes, 32 characters at most", p.id));
        }
        if p.name.trim().is_empty() || p.name.chars().count() > 64 {
            return Err("plugin.name: 1 to 64 characters".into());
        }
        if version_triple(&p.version).is_none() {
            return Err(format!("plugin.version {:?}: three numbers, as 1.2.3", p.version));
        }
        if let Some(v) = &p.min_server
            && version_triple(v).is_none()
        {
            return Err(format!("plugin.min_server {v:?}: three numbers, as 0.7.0"));
        }
        if p.entry.is_empty() || p.entry.starts_with('/') || p.entry.split('/').any(|c| c == ".." || c.is_empty()) {
            return Err(format!("plugin.entry {:?}: a path inside the plugin's folder", p.entry));
        }
        for c in &p.capabilities {
            if !caps::known(c) {
                return Err(format!("plugin.capabilities: {c:?} is not a capability (known: {})", caps::ALL.iter().map(|(n, _)| *n).collect::<Vec<_>>().join(", ")));
            }
        }
        if p.capabilities.iter().collect::<BTreeSet<_>>().len() != p.capabilities.len() {
            return Err("plugin.capabilities lists one twice".into());
        }
        let wants = |cap: &str| p.capabilities.iter().any(|c| c == cap);
        for name in &self.events.subscribe {
            let kind = Kind::from_name(name).filter(|k| *k != Kind::Command).ok_or_else(|| format!("events.subscribe: {name:?} is not an event (known: {}; commands come from [[command]])", names_without_command()))?;
            if !wants(caps::for_event(kind)) {
                return Err(format!("events.subscribe has {name:?}, which needs the capability {}: add it to plugin.capabilities", caps::for_event(kind)));
            }
        }
        if !self.commands.is_empty() && !wants(caps::EVENT_CHAT) {
            return Err(format!("[[command]] needs the capability {}: add it to plugin.capabilities", caps::EVENT_CHAT));
        }
        let rpc_events = self.events.subscribe.iter().any(|n| n == "rpc_before" || n == "rpc_after");
        if rpc_events != !self.events.rpc.is_empty() {
            return Err("events.rpc (the RPC names) and events.subscribe with rpc_before or rpc_after go together".into());
        }
        if self.events.rpc.len() > 32 || self.events.rpc.iter().any(|r| r.is_empty() || r.len() > 128) {
            return Err("events.rpc: at most 32 names of 1 to 128 characters".into());
        }
        let mut seen = BTreeSet::new();
        for c in &self.commands {
            let ok = !c.name.is_empty() && c.name.len() <= 32 && c.name.chars().all(|ch| ch.is_alphanumeric() || ch == '_' || ch == '-');
            if !ok {
                return Err(format!("command name {:?}: 1 to 32 letters, digits, _ or -", c.name));
            }
            if !seen.insert(c.name.to_lowercase()) {
                return Err(format!("command {:?} is declared twice", c.name));
            }
        }
        if p.kind == PluginKind::Data && !(self.events.subscribe.is_empty() && self.events.rpc.is_empty() && self.commands.is_empty()) {
            return Err("a data plugin's events and commands come from its rules file: no events.subscribe, events.rpc or [[command]]".into());
        }
        if self.budget.tick_every_ms.is_some_and(|t| t < crate::plugins::limits::MIN_TICK_MS) {
            return Err(format!("budget.tick_every_ms: at least {} (a game tick is 5 ms; a plugin that needs more often should do less per call)", crate::plugins::limits::MIN_TICK_MS));
        }
        Ok(())
    }

    pub fn id(&self) -> &str {
        &self.plugin.id
    }

    /// The kinds the manifest subscribes to (chat commands make `Command` one).
    pub fn kinds(&self) -> BTreeSet<Kind> {
        let mut kinds: BTreeSet<Kind> = self.events.subscribe.iter().filter_map(|n| Kind::from_name(n)).collect();
        if !self.commands.is_empty() {
            kinds.insert(Kind::Command);
        }
        kinds
    }
}

fn names_without_command() -> String {
    Kind::ALL.iter().filter(|k| **k != Kind::Command).map(|k| k.name()).collect::<Vec<_>>().join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = r#"
        [plugin]
        id = "hello-stats"
        name = "Hello"
        version = "0.1.0"
        kind = "wasm"
        entry = "plugin.wasm"
        abi = 1
        capabilities = ["event.chat", "event.tick", "world.read", "chat.say"]

        [events]
        subscribe = ["tick"]

        [[command]]
        name = "stats"
    "#;

    #[test]
    fn a_good_manifest_parses_and_names_its_kinds() {
        let m = Manifest::parse(GOOD).unwrap();
        assert_eq!(m.id(), "hello-stats");
        assert_eq!(m.kinds().into_iter().collect::<Vec<_>>(), vec![Kind::Tick, Kind::Command]);
    }

    #[test]
    fn mistakes_are_refused_with_the_reason() {
        let bad = |from: &str, to: &str| Manifest::parse(&GOOD.replace(from, to)).unwrap_err();
        assert!(bad("hello-stats", "Hello Stats").contains("plugin.id"));
        assert!(bad("0.1.0", "0.1").contains("plugin.version"));
        assert!(bad("\"event.tick\", ", "").contains("needs the capability event.tick"));
        assert!(bad("\"event.chat\", ", "").contains("needs the capability event.chat"));
        assert!(bad("\"world.read\"", "\"world.write\"").contains("not a capability"));
        assert!(bad("[\"tick\"]", "[\"tock\"]").contains("not an event"));
        assert!(bad("plugin.wasm", "../plugin.wasm").contains("plugin.entry"));
        assert!(bad("name = \"stats\"", "name = \"two words\"").contains("command name"));
        assert!(bad("abi = 1", "abi = 1\nsurprise = 2").contains("unknown field"));
        assert!(bad("subscribe = [\"tick\"]", "subscribe = [\"tick\"]\nrpc = [\"A.B\"]").contains("go together"));
        assert!(Manifest::parse(&format!("{GOOD}\n[budget]\ntick_every_ms = 1")).unwrap_err().contains("tick_every_ms"));
    }

    #[test]
    fn ids_and_versions() {
        assert!(valid_id("a") && valid_id("discord-bridge2") && !valid_id("2fast") && !valid_id("Bad") && !valid_id("a_b") && !valid_id(&"x".repeat(33)));
        assert_eq!(version_triple("1.22.3"), Some((1, 22, 3)));
        assert_eq!(version_triple("1.2"), None);
        assert_eq!(version_triple("1.2.3-beta"), None);
        assert_eq!(version_triple("1.2.3.4"), None);
    }
}
