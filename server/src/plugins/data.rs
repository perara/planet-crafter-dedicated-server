//! A plugin that is only data: a rules file the host runs itself, with no code of the plugin's
//! to run, nothing to sandbox and nothing that can loop. It covers the small jobs most servers
//! want: chat commands that answer with a text, a message when a player joins or leaves, an
//! announcement every so often, and rule changes that follow how many players are online.
//!
//! ```toml
//! [vars]
//! discord = "https://discord.gg/example"
//!
//! [[command]]            # !rules, answered to the player who asked
//! name = "rules"
//! reply = "No griefing. More at {discord}"
//! cooldown_seconds = 5
//!
//! [[on_join]]
//! say = "Welcome, {name}! {players} online."
//!
//! [[every]]
//! seconds = 900
//! say = "Join us on {discord}"
//!
//! [[when_players]]       # from 6 players on, plants grow twice as fast
//! at_least = 6
//! set_rules = { speed = { growth = 2.0 } }
//! otherwise_rules = { speed = { growth = 1.0 } }
//! ```
//!
//! Texts may use {name} {client} {players} {planet} {time} {args} {arg1} {arg2} {arg3} and the
//! plugin's own [vars]; {{ and }} are braces. What a file needs (which events, which
//! capabilities) is worked out from its content, and `pcds plugin check` compares it with what
//! the manifest asks for.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use pcds_plugin_api::{Event, Kind, Verdict, caps};
use serde::Deserialize;
use serde_json::Value;

use super::ctx::Ctx;
use super::runtime::{Cost, Fault, Runtime};
use super::state::Blob;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rules {
    #[serde(default)]
    pub vars: BTreeMap<String, String>,
    #[serde(default, rename = "command")]
    pub commands: Vec<Cmd>,
    #[serde(default)]
    pub on_join: Vec<Say>,
    #[serde(default)]
    pub on_leave: Vec<Say>,
    #[serde(default)]
    pub every: Vec<Every>,
    #[serde(default)]
    pub when_players: Vec<When>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cmd {
    pub name: String,
    /// Said to the player who asked.
    pub reply: Option<String>,
    /// Said to everyone.
    pub say: Option<String>,
    pub cooldown_seconds: Option<f64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Say {
    /// To everyone.
    pub say: Option<String>,
    /// To the player concerned (the one who joined).
    pub tell: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Every {
    pub seconds: f64,
    /// One text, or a list that is said in turn (the first, the second, ... then the first again).
    pub say: Lines,
    /// Only said while at least this many players are online (default 0: always; 1: not to an empty server).
    pub min_players: Option<u32>,
}

/// A text, or several to rotate through.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Lines {
    One(String),
    Many(Vec<String>),
}

impl Lines {
    pub fn all(&self) -> Vec<&str> {
        match self {
            Lines::One(t) => vec![t.as_str()],
            Lines::Many(l) => l.iter().map(String::as_str).collect(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct When {
    pub at_least: u32,
    /// With `at_least`, a band of player counts: from `at_least` to `at_most` (both included). Without it, `at_least` and above.
    pub at_most: Option<u32>,
    pub set_rules: Option<toml::Value>,
    pub otherwise_rules: Option<toml::Value>,
}

const VARS: [&str; 9] = ["name", "client", "players", "planet", "time", "args", "arg1", "arg2", "arg3"];
/// How many of each a rules file may hold, and how long a text or a variable may be (see `check`).
const MAX_COMMANDS: usize = 64;
const MAX_ENTRIES: usize = 16;
const MAX_EVERY: usize = 64;
const MAX_WHEN: usize = 32;
const MAX_VARS: usize = 64;
const MAX_TEXT_BYTES: usize = 2000;
const MAX_VAR_BYTES: usize = 1000;
/// The ones that need `world.read`.
const WORLD_VARS: [&str; 3] = ["players", "planet", "time"];

impl Rules {
    pub fn parse(text: &str) -> Result<Rules, String> {
        let rules: Rules = toml::from_str(text).map_err(|e| e.to_string())?;
        rules.check()?;
        Ok(rules)
    }

    fn texts(&self) -> Vec<(&str, &str)> {
        let mut t = Vec::new();
        for c in &self.commands {
            t.extend(c.reply.as_deref().map(|s| ("command reply", s)));
            t.extend(c.say.as_deref().map(|s| ("command say", s)));
        }
        for s in self.on_join.iter().chain(&self.on_leave) {
            t.extend(s.say.as_deref().map(|s| ("on_join/on_leave say", s)));
            t.extend(s.tell.as_deref().map(|s| ("on_join/on_leave tell", s)));
        }
        for e in &self.every {
            t.extend(e.say.all().into_iter().map(|l| ("every say", l)));
        }
        t
    }

    fn check(&self) -> Result<(), String> {
        // A rules file has no fuel: what it costs is the size of what it names, at every event and every
        // tick, so the sizes are bounded (a file may be 8 MiB; the same bytes as 250,000 `every` entries
        // would have had each tick walk them all).
        for (what, n, most) in [("[[command]]", self.commands.len(), MAX_COMMANDS), ("[[on_join]]", self.on_join.len(), MAX_ENTRIES), ("[[on_leave]]", self.on_leave.len(), MAX_ENTRIES), ("[[every]]", self.every.len(), MAX_EVERY), ("[[when_players]]", self.when_players.len(), MAX_WHEN), ("[vars]", self.vars.len(), MAX_VARS)] {
            if n > most {
                return Err(format!("{n} {what} entries: at most {most}"));
            }
        }
        if let Some((k, _)) = self.vars.iter().find(|(k, v)| k.len() > MAX_VAR_BYTES || v.len() > MAX_VAR_BYTES) {
            return Err(format!("[vars] {k:?}: a name and a value of {MAX_VAR_BYTES} bytes at most"));
        }
        if let Some((where_, _)) = self.texts().into_iter().find(|(_, t)| t.len() > MAX_TEXT_BYTES) {
            return Err(format!("{where_}: a text of {MAX_TEXT_BYTES} bytes at most (a chat line is {} characters)", super::limits::SAY_CHARS));
        }
        let mut names = BTreeSet::new();
        for c in &self.commands {
            if c.name.is_empty() || c.name.len() > 32 || !c.name.chars().all(|ch| ch.is_alphanumeric() || ch == '_' || ch == '-') {
                return Err(format!("command name {:?}: 1 to 32 letters, digits, _ or -", c.name));
            }
            if !names.insert(c.name.to_lowercase()) {
                return Err(format!("command {:?} twice", c.name));
            }
            if c.reply.is_none() && c.say.is_none() {
                return Err(format!("command {:?} has neither reply nor say", c.name));
            }
            if c.cooldown_seconds.is_some_and(|s| !(0.0..=86_400.0).contains(&s)) {
                return Err(format!("command {:?}: cooldown_seconds from 0 to 86400", c.name));
            }
        }
        for s in self.on_join.iter().chain(&self.on_leave) {
            if s.say.is_none() && s.tell.is_none() {
                return Err("an on_join or on_leave entry has neither say nor tell".into());
            }
        }
        for e in &self.every {
            if !(1.0..=604_800.0).contains(&e.seconds) {
                return Err(format!("every: seconds {} (from 1 to 604800)", e.seconds));
            }
            if e.say.all().is_empty() || e.say.all().len() > MAX_ENTRIES {
                return Err(format!("every: say is one text or a list of 1 to {MAX_ENTRIES} texts"));
            }
        }
        for w in &self.when_players {
            if w.at_most.is_some_and(|m| m < w.at_least) {
                return Err(format!("when_players: at_most {} is below at_least {}", w.at_most.unwrap_or(0), w.at_least));
            }
            if w.set_rules.is_none() && w.otherwise_rules.is_none() {
                return Err("a when_players entry has neither set_rules nor otherwise_rules".into());
            }
            for patch in [&w.set_rules, &w.otherwise_rules].into_iter().flatten() {
                let json = serde_json::to_value(patch).map_err(|e| e.to_string())?;
                if !json.is_object() {
                    return Err("set_rules and otherwise_rules are tables of rules".into());
                }
                super::actions::rules_patch_allowed(&json)?;
            }
        }
        for (where_, text) in self.texts() {
            for var in placeholders(text).map_err(|e| format!("{where_}: {e}"))? {
                if !VARS.contains(&var.as_str()) && !self.vars.contains_key(&var) {
                    return Err(format!("{where_}: {{{var}}} is not a variable (known: {}, and your [vars])", VARS.join(" ")));
                }
            }
            if text.trim().is_empty() {
                return Err(format!("{where_}: an empty text"));
            }
        }
        Ok(())
    }

    /// The command names, lowercase.
    pub fn command_names(&self) -> Vec<String> {
        self.commands.iter().map(|c| c.name.to_lowercase()).collect()
    }

    /// The event kinds the rules need.
    pub fn kinds(&self) -> BTreeSet<Kind> {
        let mut k = BTreeSet::new();
        if !self.commands.is_empty() {
            k.insert(Kind::Command);
        }
        if !self.on_join.is_empty() {
            k.insert(Kind::PlayerJoin);
        }
        if !self.on_leave.is_empty() {
            k.insert(Kind::PlayerLeave);
        }
        if !self.every.is_empty() || !self.when_players.is_empty() {
            k.insert(Kind::Tick);
        }
        k
    }

    /// The capabilities the rules need: the events, saying, reading the world, changing rules.
    pub fn needed_caps(&self) -> BTreeSet<&'static str> {
        let mut c: BTreeSet<&'static str> = self.kinds().into_iter().map(caps::for_event).collect();
        let says = self.commands.iter().any(|c| c.reply.is_some() || c.say.is_some()) || self.on_join.iter().chain(&self.on_leave).any(|s| s.say.is_some() || s.tell.is_some()) || !self.every.is_empty();
        if says {
            c.insert(caps::CHAT_SAY);
        }
        if !self.when_players.is_empty() {
            c.insert(caps::RULES_WRITE);
        }
        let world = self.texts().iter().any(|(_, t)| placeholders(t).unwrap_or_default().iter().any(|v| WORLD_VARS.contains(&v.as_str())));
        if world {
            c.insert(caps::WORLD_READ);
        }
        c
    }
}

/// The {names} in a text; Err for a brace that does not close or hold a name, or a lone }.
fn placeholders(text: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' if chars.peek() == Some(&'{') => {
                chars.next();
            }
            '}' if chars.peek() == Some(&'}') => {
                chars.next();
            }
            '{' => {
                let mut name = String::new();
                loop {
                    match chars.next() {
                        Some('}') => break,
                        Some(c) => name.push(c),
                        None => return Err(format!("a {{ in {text:?} is never closed (write {{{{ for a brace)")),
                    }
                }
                if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                    return Err(format!("a brace in {text:?} does not hold a name (write {{{{ for a brace)"));
                }
                out.push(name);
            }
            '}' => return Err(format!("a lone }} in {text:?} (write }}}} for a brace)")),
            _ => {}
        }
    }
    Ok(out)
}

fn render(text: &str, vars: &HashMap<String, String>) -> String {
    let mut out = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' if chars.peek() == Some(&'{') => {
                chars.next();
                out.push('{');
            }
            '}' if chars.peek() == Some(&'}') => {
                chars.next();
                out.push('}');
            }
            '{' => {
                let name: String = chars.by_ref().take_while(|c| *c != '}').collect();
                out.push_str(vars.get(&name).map(String::as_str).unwrap_or(""));
            }
            c => out.push(c),
        }
    }
    out
}

fn clock(game_ms: i64) -> String {
    let s = game_ms.max(0) / 1000;
    format!("{}h {:02}m", s / 3600, s % 3600 / 60)
}

pub struct Data {
    rules: Rules,
    ctx: Ctx,
    /// Per `every`: the game time (ms) it is next due, once the first tick has been seen.
    due: Vec<Option<i64>>,
    /// Per `when_players`: what was last applied (true: the "at least" side).
    applied: Vec<Option<bool>>,
    /// The game time (ms) each (command, player) last ran.
    last: HashMap<(usize, u64), i64>,
    /// Per `every`: which of its texts is next.
    turn: Vec<usize>,
}

impl Data {
    pub fn new(rules: Rules, ctx: Ctx) -> Data {
        let (n, m) = (rules.every.len(), rules.when_players.len());
        Data { rules, ctx, due: vec![None; n], applied: vec![None; m], last: HashMap::new(), turn: vec![0; n] }
    }

    /// The variables of one event; the world's (players, planet, time) only when a text uses
    /// them, since each asks the world.
    fn vars(&mut self, texts: &[&str], client: u64, name: &str, args: &[&str]) -> HashMap<String, String> {
        let mut v: HashMap<String, String> = self.rules.vars.iter().map(|(k, x)| (k.clone(), x.clone())).collect();
        v.insert("name".into(), name.to_string());
        v.insert("client".into(), client.to_string());
        v.insert("args".into(), args.join(" "));
        for i in 0..3 {
            v.insert(format!("arg{}", i + 1), args.get(i).map(|a| a.to_string()).unwrap_or_default());
        }
        let wants_world = texts.iter().any(|t| placeholders(t).unwrap_or_default().iter().any(|p| WORLD_VARS.contains(&p.as_str())));
        if wants_world && let Some(info) = self.ctx.info() {
            v.insert("players".into(), info.players.to_string());
            v.insert("planet".into(), info.planet);
            v.insert("time".into(), clock(info.game_ms));
        }
        v
    }
}

impl Runtime for Data {
    fn ctx(&mut self) -> &mut Ctx {
        &mut self.ctx
    }

    fn start(&mut self, _state: Option<&Blob>) -> Result<(), Fault> {
        Ok(())
    }

    fn event(&mut self, event: &Event<'_>, _payload: &[u8]) -> Result<(Verdict, Cost), Fault> {
        let done = (Verdict::Continue, Cost::default());
        match event {
            Event::Command { client, name, command, args, .. } => {
                let Some(i) = self.rules.commands.iter().position(|c| c.name.eq_ignore_ascii_case(command)) else { return Ok(done) };
                let c = self.rules.commands[i].clone();
                let now = self.ctx.info().map(|w| w.game_ms).unwrap_or(0);
                if let Some(wait) = c.cooldown_seconds.filter(|s| *s > 0.0)
                    && self.last.get(&(i, *client)).is_some_and(|t| (now - t) as f64 / 1000.0 < wait)
                {
                    return Ok(done);
                }
                self.last.insert((i, *client), now);
                let texts: Vec<&str> = c.reply.iter().chain(&c.say).map(String::as_str).collect();
                let vars = self.vars(&texts, *client, name, args);
                if let Some(t) = &c.reply {
                    self.ctx.tell(*client, &render(t, &vars));
                }
                if let Some(t) = &c.say {
                    self.ctx.say(&render(t, &vars));
                }
            }
            Event::PlayerJoin { client, name, .. } | Event::PlayerLeave { client, name, .. } => {
                let joined = matches!(event, Event::PlayerJoin { .. });
                let entries = if joined { self.rules.on_join.clone() } else { self.rules.on_leave.clone() };
                for s in entries {
                    let texts: Vec<&str> = s.say.iter().chain(&s.tell).map(String::as_str).collect();
                    let vars = self.vars(&texts, *client, name, &[]);
                    if let Some(t) = &s.say {
                        self.ctx.say(&render(t, &vars));
                    }
                    if let Some(t) = s.tell.as_ref().filter(|_| joined) {
                        self.ctx.tell(*client, &render(t, &vars));
                    }
                }
            }
            Event::Tick { game_ms, players, .. } => {
                for i in 0..self.rules.every.len() {
                    let (seconds, lines, min) = (self.rules.every[i].seconds, self.rules.every[i].say.all().into_iter().map(str::to_string).collect::<Vec<_>>(), self.rules.every[i].min_players.unwrap_or(0));
                    // The first tick only starts the clock: nothing is announced at once.
                    let due = *self.due[i].get_or_insert(game_ms + (seconds * 1000.0) as i64);
                    if *game_ms >= due {
                        self.due[i] = Some(game_ms + (seconds * 1000.0) as i64);
                        // Said only with enough players; the turn does not move while it is not said.
                        if *players >= min {
                            let text = &lines[self.turn[i] % lines.len()];
                            self.turn[i] += 1;
                            let vars = self.vars(&[text], 0, "", &[]);
                            self.ctx.say(&render(text, &vars));
                        }
                    }
                }
                for i in 0..self.rules.when_players.len() {
                    let w = self.rules.when_players[i].clone();
                    let high = *players >= w.at_least && w.at_most.is_none_or(|m| *players <= m);
                    if self.applied[i] == Some(high) {
                        continue;
                    }
                    self.applied[i] = Some(high);
                    let patch = if high { &w.set_rules } else { &w.otherwise_rules };
                    if let Some(p) = patch.as_ref().and_then(|p| serde_json::to_value(p).ok()) {
                        let code = self.ctx.rules_patch(&Value::to_string(&p));
                        if code != 0 {
                            // The plugin's wish was refused (the capability, or a rule's range):
                            // asked again at the next tick, not forgotten.
                            self.applied[i] = None;
                        }
                    }
                }
            }
            _ => {}
        }
        Ok(done)
    }

    fn save(&mut self) -> Result<Option<Blob>, Fault> {
        Ok(None)
    }

    fn restart(&mut self, _state: Option<&Blob>) -> Result<(), Fault> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::ctx::{Action, Grant, World};
    use pcds_plugin_api::query::{PlayerInfo, WorldInfo};

    struct Fixed(u32);
    impl World for Fixed {
        fn info(&self) -> WorldInfo {
            WorldInfo { game_ms: 3_725_000, planet: "Prime".into(), players: self.0, ..Default::default() }
        }
        fn players(&self) -> Vec<PlayerInfo> {
            vec![]
        }
        fn rules(&self) -> String {
            "{}".into()
        }
    }

    const FILE: &str = r#"
        [vars]
        discord = "discord.gg/x"
        [[command]]
        name = "Rules"
        reply = "Be kind, {name}. {discord} ({players} on {planet}, {time}) args={args} first={arg1}"
        cooldown_seconds = 10
        [[on_join]]
        say = "Welcome {name}"
        tell = "Try !rules {{braces}}"
        [[every]]
        seconds = 60
        say = "Join {discord}"
        [[when_players]]
        at_least = 3
        set_rules = { speed = { growth = 2.0 } }
        otherwise_rules = { speed = { growth = 1.0 } }
    "#;

    fn run(rules: &Rules, players: u32, events: &[Event<'_>]) -> Vec<Action> {
        let mut d = Data::new(rules.clone(), Ctx::new("t", Grant::from_names(["chat.say", "world.read", "rules.write"])));
        let world = Fixed(players);
        let mut out = Vec::new();
        for e in events {
            d.ctx.begin(&world, 0);
            d.event(e, &[]).unwrap();
            out.extend(d.ctx.end());
        }
        out
    }

    #[test]
    fn a_rules_file_says_what_it_needs() {
        let r = Rules::parse(FILE).unwrap();
        assert_eq!(r.command_names(), vec!["rules"]);
        assert_eq!(r.kinds().into_iter().collect::<Vec<_>>(), vec![Kind::Tick, Kind::PlayerJoin, Kind::Command]);
        assert_eq!(r.needed_caps().into_iter().collect::<Vec<_>>(), vec!["chat.say", "event.chat", "event.player", "event.tick", "rules.write", "world.read"]);
    }

    #[test]
    fn commands_joins_announcements_and_rule_changes_act() {
        let r = Rules::parse(FILE).unwrap();
        let cmd = |t: &str| Event::Command { client: 7, name: "Ann", command: "RULES", args: vec!["a", "b"], text: t.to_string().leak(), player_id: 1 };
        let tick = |ms, players| Event::Tick { game_ms: ms, tick: 0, players, since_ms: 1000, unix_ms: 0 };
        let actions = run(&r, 5, &[cmd("!rules a b"), cmd("!rules a b"), Event::PlayerJoin { client: 7, name: "Ann", steam_id: "", player_id: 1 }, tick(1000, 5), tick(30_000, 5), tick(61_000, 2), tick(62_000, 2)]);
        let [Action::Tell(7, reply), Action::Say(welcome), Action::Tell(7, tell), Action::RulesPatch(up), Action::Say(announce), Action::RulesPatch(down)] = &actions[..] else { panic!("{actions:?}") };
        assert_eq!(reply, "Be kind, Ann. discord.gg/x (5 on Prime, 1h 02m) args=a b first=a", "the second !rules within 10 s is not answered");
        assert_eq!((welcome.as_str(), tell.as_str()), ("Welcome Ann", "Try !rules {braces}"));
        assert_eq!(up["speed"]["growth"], 2.0);
        assert_eq!(down["speed"]["growth"], 1.0);
        assert_eq!(announce, "Join discord.gg/x", "after 60 s of ticks, and not at the first tick");
    }

    #[test]
    fn mistakes_in_a_rules_file_are_named() {
        let bad = |t: &str| Rules::parse(t).unwrap_err();
        assert!(bad("[[command]]\nname = \"a\"").contains("neither reply nor say"));
        assert!(bad("[[command]]\nname = \"a b\"\nreply = \"x\"").contains("command name"));
        assert!(bad("[[command]]\nname = \"a\"\nreply = \"{nope}\"").contains("{nope} is not a variable"));
        assert!(bad("[[command]]\nname = \"a\"\nreply = \"{name\"").contains("never closed"));
        assert!(bad("[[command]]\nname = \"a\"\nreply = \"a } b\"").contains("lone"));
        assert!(bad("[[every]]\nseconds = 0.1\nsay = \"x\"").contains("seconds"));
        assert!(bad("[[when_players]]\nat_least = 1\nset_rules = { chat = { exec = [\"/bin/sh\"] } }").contains("plugins may change only"));
        assert!(bad("surprise = 1").contains("unknown field"));
    }

    /// A rules file runs without fuel: the entries a tick walks and the text an event renders must be
    /// bounded, or one 8 MiB file makes every tick (and every !command) cost milliseconds.
    #[test]
    fn a_rules_file_is_bounded_in_what_each_tick_and_event_walks() {
        let many = |entry: &str, n: usize| entry.repeat(n);
        let bad = |t: String| Rules::parse(&t).unwrap_err();
        assert!(bad(many("[[every]]\nseconds = 1\nsay = \"x\"\n", 250_000)).contains("at most 64"));
        assert!(bad(many("[[when_players]]\nat_least = 1\nset_rules = { speed = { growth = 2.0 } }\n", 33)).contains("at most 32"));
        assert!(bad((0..65).map(|i| format!("[[command]]\nname = \"c{i}\"\nreply = \"x\"\n")).collect()).contains("at most 64"));
        assert!(bad(format!("[[on_join]]\nsay = \"{}\"\n", "a".repeat(2001))).contains("2000 bytes"));
        assert!(bad(format!("[vars]\nk = \"{}\"\n", "a".repeat(1001))).contains("1000 bytes"));
        // And what is allowed is allowed.
        assert!(Rules::parse(&many("[[every]]\nseconds = 1\nsay = \"x\"\n", 64)).is_ok());
        assert!(Rules::parse(&format!("[[on_join]]\nsay = \"{}\"\n", "a".repeat(2000))).is_ok());
    }
}
