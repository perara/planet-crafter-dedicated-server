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
    /// Where the plugin may send HTTP requests (capability `net.http`).
    pub net: Option<Net>,
    /// Values the admin sets for this plugin (`pcds plugin config set`), put into requests as
    /// `${config.KEY}`.
    #[serde(default, rename = "config")]
    pub config: Vec<ConfigKey>,
}

/// `[net]`: the allow list the approval pins, and the plugin's asks for time and size.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Net {
    /// `scheme://host[:port]` patterns; `*.` in front of a name covers the names below it.
    #[serde(default)]
    pub allow: Vec<String>,
    /// The wall clock of one request, in milliseconds.
    pub timeout_ms: Option<u32>,
    /// The most of an answer's body the plugin takes (an answer over it is an error).
    pub max_response_bytes: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigKey {
    pub key: String,
    #[serde(default)]
    pub description: String,
    /// A secret is never shown: not by the listing, not by the log, not to the plugin. The default.
    #[serde(default = "yes")]
    pub secret: bool,
}

fn yes() -> bool {
    true
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
        // A TOML error quotes the line it stumbled on: whatever the author put there.
        let m: Manifest = toml::from_str(text).map_err(|e| super::clean(&e.to_string()))?;
        m.validate()?;
        Ok(m)
    }

    fn validate(&self) -> Result<(), String> {
        let p = &self.plugin;
        if !valid_id(&p.id) {
            return Err(format!("plugin.id {:?}: a lowercase letter, then lowercase letters, digits and dashes, 32 characters at most", p.id));
        }
        if p.name.trim().is_empty() {
            return Err("plugin.name: 1 to 64 characters".into());
        }
        // The author's words are shown on the admin's terminal above the capabilities and the question:
        // one line of plain text each (docs/plugins.md, "What a manifest may say").
        let line = |what: &str, text: &str, most: usize| {
            if text.chars().count() > most || super::clean(text) != text {
                return Err(format!("{what}: one line of plain text of {most} characters at most (no control character, line break or direction mark)"));
            }
            Ok(())
        };
        line("plugin.name", &p.name, 64)?;
        line("plugin.description", &p.description, 500)?;
        line("plugin.license", &p.license, 64)?;
        if p.authors.len() > 16 {
            return Err("plugin.authors: 16 at most".into());
        }
        for a in &p.authors {
            line("plugin.authors", a, 64)?;
        }
        for c in &self.commands {
            line("[[command]] help", &c.help, 200)?;
        }
        for k in &self.config {
            line("[[config]] description", &k.description, 200)?;
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
            let kind = Kind::from_name(name).filter(|k| *k != Kind::Command && *k != Kind::HttpDone).ok_or_else(|| format!("events.subscribe: {name:?} is not an event to subscribe to (known: {}; commands come from [[command]], and http_done is delivered for the requests the plugin made)", names_without_command()))?;
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
        self.validate_net()?;
        if self.budget.tick_every_ms.is_some_and(|t| t < crate::plugins::limits::MIN_TICK_MS) {
            return Err(format!("budget.tick_every_ms: at least {} (a game tick is 5 ms; a plugin that needs more often should do less per call)", crate::plugins::limits::MIN_TICK_MS));
        }
        Ok(())
    }

    fn validate_net(&self) -> Result<(), String> {
        let p = &self.plugin;
        let asks = |cap: &str| p.capabilities.iter().any(|c| c == cap);
        let (http, local) = (asks(caps::NET_HTTP), asks(caps::NET_HTTP_LOCAL));
        if local && !http {
            return Err(format!("{} goes with {}: ask for both", caps::NET_HTTP_LOCAL, caps::NET_HTTP));
        }
        match (&self.net, http) {
            (None, true) => return Err(format!("{} needs a [net] section with allow = [...]: the hosts it may reach", caps::NET_HTTP)),
            (Some(_), false) => return Err(format!("[net] needs the capability {}: add it to plugin.capabilities", caps::NET_HTTP)),
            _ => {}
        }
        if let Some(net) = &self.net {
            if net.allow.is_empty() || net.allow.len() > 16 {
                return Err("net.allow: 1 to 16 patterns".into());
            }
            for pattern in &net.allow {
                crate::plugins::net::Pattern::parse(pattern, local)?;
            }
            if net.allow.iter().collect::<BTreeSet<_>>().len() != net.allow.len() {
                return Err("net.allow lists one twice".into());
            }
            if net.timeout_ms.is_some_and(|t| t == 0 || t > crate::plugins::limits::MAX_HTTP_TIMEOUT_MS) {
                return Err(format!("net.timeout_ms: 1 to {}", crate::plugins::limits::MAX_HTTP_TIMEOUT_MS));
            }
            if net.max_response_bytes.is_some_and(|t| t == 0 || t > crate::plugins::limits::MAX_HTTP_RESPONSE) {
                return Err(format!("net.max_response_bytes: 1 to {}", crate::plugins::limits::MAX_HTTP_RESPONSE));
            }
        }
        if self.config.len() > 8 {
            return Err("at most 8 [[config]] keys".into());
        }
        let mut seen = BTreeSet::new();
        for c in &self.config {
            let ok = !c.key.is_empty() && c.key.len() <= 32 && c.key.starts_with(|ch: char| ch.is_ascii_lowercase()) && c.key.chars().all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_');
            if !ok {
                return Err(format!("config key {:?}: a lowercase letter, then lowercase letters, digits and _, 32 at most", c.key));
            }
            if !seen.insert(&c.key) {
                return Err(format!("config key {:?} is declared twice", c.key));
            }
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
    Kind::ALL.iter().filter(|k| **k != Kind::Command && **k != Kind::HttpDone).map(|k| k.name()).collect::<Vec<_>>().join(", ")
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
        assert!(bad("[\"tick\"]", "[\"http_done\"]").contains("not an event to subscribe to"));
        assert!(bad("plugin.wasm", "../plugin.wasm").contains("plugin.entry"));
        assert!(bad("name = \"stats\"", "name = \"two words\"").contains("command name"));
        assert!(bad("abi = 1", "abi = 1\nsurprise = 2").contains("unknown field"));
        assert!(bad("subscribe = [\"tick\"]", "subscribe = [\"tick\"]\nrpc = [\"A.B\"]").contains("go together"));
        assert!(Manifest::parse(&format!("{GOOD}\n[budget]\ntick_every_ms = 1")).unwrap_err().contains("tick_every_ms"));
    }

    const NET: &str = r#"
        [plugin]
        id = "notify"
        name = "Notify"
        version = "0.1.0"
        kind = "wasm"
        entry = "plugin.wasm"
        abi = 1
        capabilities = ["event.chat", "net.http"]

        [events]
        subscribe = ["chat"]

        [net]
        allow = ["https://discord.com", "https://*.example.com:8443"]
        timeout_ms = 5000
        max_response_bytes = 4096

        [[config]]
        key = "hook"
        description = "the webhook's path"
    "#;

    #[test]
    fn net_access_is_declared_with_its_hosts_and_its_keys() {
        let m = Manifest::parse(NET).unwrap();
        assert_eq!(m.net.as_ref().unwrap().allow.len(), 2);
        assert!(m.config[0].secret, "a key is a secret unless the manifest says otherwise");
        let bad = |from: &str, to: &str| Manifest::parse(&NET.replace(from, to)).unwrap_err();
        assert!(bad("\"event.chat\", \"net.http\"", "\"event.chat\"").contains("[net] needs the capability net.http"));
        assert!(bad("\n        [net]\n", "\n        [nnet]\n").contains("unknown field"));
        assert!(Manifest::parse(&NET.replace("\"net.http\"]", "\"net.http\", \"net.http.local\"]").replace("\"https://discord.com\", ", "")).is_ok());
        assert!(bad("\"net.http\"]", "\"net.http.local\"]").contains("goes with net.http"));
        assert!(Manifest::parse(NET.split("[net]").next().unwrap()).unwrap_err().contains("needs a [net] section"));
        assert!(bad("allow = [\"https://discord.com\", ", "allow = [\"https://127.0.0.1\", ").contains("net.http.local"));
        assert!(bad("allow = [\"https://discord.com\", ", "allow = [\"https://*.com\", ").contains("public suffix"));
        assert!(bad("allow = [\"https://discord.com\", ", "allow = [\"https://discord.com\", \"https://discord.com\", ").contains("twice"));
        assert!(bad("allow = [\"https://discord.com\", \"https://*.example.com:8443\"]", "allow = []").contains("1 to 16"));
        assert!(bad("timeout_ms = 5000", "timeout_ms = 999999").contains("timeout_ms"));
        assert!(bad("max_response_bytes = 4096", "max_response_bytes = 0").contains("max_response_bytes"));
        assert!(bad("key = \"hook\"", "key = \"Hook\"").contains("config key"));
        assert!(bad("key = \"hook\"", "key = \"hook\"\n[[config]]\nkey = \"hook\"").contains("twice"));
    }

    /// The digest a signature covers is sha256(manifest, 0, entry): the split is only unambiguous if no
    /// manifest holds a zero byte (an attacker with a signed pair could else move the zero and some bytes
    /// of the module into the manifest). TOML takes no control character, in a comment or a string.
    #[test]
    fn a_manifest_with_a_zero_byte_is_not_a_manifest() {
        for at in ["\n# a comment \u{0} here", "\n# \u{0}", "\nx = \"a\u{0}b\"", "\u{0}"] {
            assert!(Manifest::parse(&format!("{GOOD}{at}")).is_err(), "{at:?}");
        }
        assert!(Manifest::parse(&format!("{GOOD}\n# fine\tcomment")).is_ok());
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
