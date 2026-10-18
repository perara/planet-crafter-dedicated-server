//! The configuration schema: every knob of this server in one table, KNOBS, with its type,
//! range, unit, description and whether it applies live. GET /api/config serves the table with
//! each knob's default and value; POST /api/config changes values ({"speed.growth": 2,
//! "access.whitelist": ["Ann"]}; null: back to the default), every one checked here first and
//! then all applied, or none. Rules::validate checks rules.json against the same entries, and
//! the web page's Settings tab is drawn from the table: a new knob is one entry here and no
//! page code. The server.env settings show read-only: the server reads them at start, and
//! `pcds config` edits them.

use serde_json::{json, Value};

use crate::game::Game;
use crate::rules::Rules;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Type {
    Bool,
    Int,
    Float,
    String,
    Enum,
    /// A list of strings.
    List,
    /// Names to whole numbers.
    Map,
}

/// Where a knob's value lives.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Source {
    /// rules.json; the key is the rule's path, "section.rule".
    Rules,
    /// access.json; the key is "access.<list>".
    Access,
    /// The running server's own, back to its start value at a restart.
    Server,
    /// server.env, read at start: shown, never changed here.
    Env,
    /// A server.env secret: shows only whether it is set.
    Secret,
}

pub struct Knob {
    pub key: &'static str,
    pub source: Source,
    pub kind: Type,
    /// Numbers (and a map's values): the inclusive range.
    pub range: Option<(f64, f64)>,
    /// Enum: the allowed values.
    pub values: &'static [&'static str],
    pub unit: &'static str,
    pub description: &'static str,
    /// Applies at once; else at the next restart.
    pub live: bool,
    /// server.env knobs: the value without a line ("": none). Rules take theirs from
    /// Rules::default(), so the two cannot differ.
    pub default: &'static str,
    /// Its value may hold a token (webhook URLs, a signing secret): shown only as set.
    pub secret: bool,
}

const fn knob(source: Source, key: &'static str, kind: Type, unit: &'static str, description: &'static str) -> Knob {
    Knob { key, source, kind, range: None, values: &[], unit, description, live: !matches!(source, Source::Env | Source::Secret), default: "", secret: matches!(source, Source::Secret) }
}

const fn rule(key: &'static str, kind: Type, unit: &'static str, description: &'static str) -> Knob {
    knob(Source::Rules, key, kind, unit, description)
}

const fn list(key: &'static str, description: &'static str) -> Knob {
    knob(Source::Access, key, Type::List, "", description)
}

const fn env(key: &'static str, kind: Type, unit: &'static str, default: &'static str, description: &'static str) -> Knob {
    Knob { default, ..knob(Source::Env, key, kind, unit, description) }
}

impl Knob {
    const fn range(self, min: f64, max: f64) -> Knob {
        Knob { range: Some((min, max)), ..self }
    }

    const fn values(self, values: &'static [&'static str]) -> Knob {
        Knob { values, ..self }
    }

    const fn secret(self) -> Knob {
        Knob { secret: true, ..self }
    }
}

const MULTIPLIER: (f64, f64) = (0.0, 100.0);

pub static KNOBS: &[Knob] = &[
    rule("meteo.eventChance", Type::Float, "×", "The chance of a random weather or meteor event at each try, times this; 0 stops them. Events players cause (rockets sent to space) still come.").range(MULTIPLIER.0, MULTIPLIER.1),
    rule("meteo.materialDespawnSeconds", Type::Float, "s", "Seconds before the materials a meteor drops disappear; 0: they stay. Unset: the game's own time. The rocks of an impact are drawn by each game itself and keep their time.").range(0.0, 86_400.0),
    rule("gauges.oxygenDrain", Type::Float, "×", "Oxygen use, times this, on top of the world's own gauge drain setting.").range(MULTIPLIER.0, MULTIPLIER.1),
    rule("gauges.thirstDrain", Type::Float, "×", "Water use, times this.").range(MULTIPLIER.0, MULTIPLIER.1),
    rule("gauges.foodDrain", Type::Float, "×", "Food use, times this.").range(MULTIPLIER.0, MULTIPLIER.1),
    rule("gauges.noDeath", Type::Bool, "", "Oxygen, water and health never drop below 1, so no one dies."),
    rule("speed.terraformation", Type::Float, "×", "What buildings add to every unit but energy, times this, on top of the world's pace.").range(MULTIPLIER.0, MULTIPLIER.1),
    // A machine or plant at speed 0 would never be due again: from 0.01.
    rule("speed.machines", Type::Float, "×", "Extractors, collectors, auto-crafters and recyclers work this many times faster.").range(0.01, MULTIPLIER.1),
    rule("speed.growth", Type::Float, "×", "Plants in vegetubes, spreaders, farms and vegetable growers grow this many times faster (the clients see the same growth). Incubators keep their time.").range(0.01, MULTIPLIER.1),
    rule("death.consequences", Type::Enum, "", "What dying costs. Unset: the world's own setting.").values(&crate::rules::CONSEQUENCES),
    rule("death.dropChance", Type::Float, "%", "With DropSomeItems: the percent chance each item is dropped (with 4 items or fewer, all go). Unset: the game's 50.").range(0.0, 100.0),
    rule("loot.amount", Type::Float, "×", "Items in generated containers (wreck safes, crates), times this, up to the size of the container.").range(MULTIPLIER.0, MULTIPLIER.1),
    rule("limits.maxPlayers", Type::Int, "players", "Players at once; more are refused with the game's session-full message. Unset: PCDS_MAX_PLAYERS, else 16.").range(1.0, 100.0),
    rule("world.pauseWhenEmpty", Type::Bool, "", "With no player in the game (past PlayerReady, as the game counts players), the world stands still: its clock, and with it every timer, day and night, growth, production, terraformation, meteo, drone and vehicle. It goes on from there when a player is in."),
    rule("chat.webhooks", Type::List, "", "http:// or https:// URLs every chat message is POSTed to as JSON (see the README's chat hooks).").secret(),
    rule("chat.secret", Type::String, "", "Signs every webhook POST: X-PCDS-Signature: sha256=<HMAC-SHA256 of the body, hex>. Unset: unsigned.").secret(),
    rule("chat.exec", Type::List, "", "A program and its arguments (no shell), run for each chat message with its JSON on stdin; what it prints is said back in the chat.").secret(),
    rule("chat.commandPrefix", Type::String, "", "Chat messages starting with this (1 to 8 characters, no spaces) are commands too: \"!tp home\" gives command tp, args [home]. Unset: none."),
    rule("chat.timeoutSeconds", Type::Float, "s", "The longest a webhook call or the exec program may take.").range(0.1, 60.0),
    rule("chat.retries", Type::Int, "", "More tries after a webhook fails (no answer, 429, 5xx), 1 s, 2 s... apart.").range(0.0, 10.0),
    rule("limits.buildings", Type::Map, "", "Building group id to how many the world may have (Drill1 = 3). One more is taken down at once and its materials go back to the builder.").range(0.0, u32::MAX as f64),
    list("access.allowedIPs", "Addresses or CIDR ranges (192.168.1.0/24) that may connect over direct IP; empty: everyone. Players who no longer qualify are kicked."),
    list("access.whitelist", "Names or SteamID64s that may play; empty: everyone. SteamIDs count only for invite-code players, where they are verified."),
    list("access.bannedNames", "Banned names and SteamID64s."),
    list("access.bannedIPs", "Banned addresses or CIDR ranges (direct-IP players)."),
    knob(Source::Server, "server.autosaveMinutes", Type::Int, "min", "Minutes between saves; 0: only on stop and through the API. Until the next restart, which starts from PCDS_AUTOSAVE.").range(0.0, crate::MAX_AUTOSAVE as f64),
    env("PCDS_LISTEN", Type::String, "", "0.0.0.0:7777", "Direct-IP address (UDP) players join; off disables it."),
    env("PCDS_STEAM", Type::Enum, "", "on", "Invite codes through Steam; off disables them.").values(&["on", "off"]),
    Knob { source: Source::Secret, ..env("PCDS_STEAM_TOKEN", Type::String, "", "", "A Steam game server login token (app 1284190): keeps the invite code the same across restarts.") },
    env("PCDS_SESSION", Type::Int, "", "4242", "The last four digits of the invite code."),
    env("PCDS_AUTOSAVE", Type::Int, "min", "5", "Minutes between saves at start (server.autosaveMinutes changes it until the next restart)."),
    env("PCDS_BACKUPS", Type::Int, "", "24", "Save backups kept in backups/ next to the world; 0 keeps none."),
    env("PCDS_BACKUP_MINUTES", Type::Int, "min", "60", "Minutes between those backups."),
    env("PCDS_ADMIN", Type::String, "", "127.0.0.1:7782", "The admin API and web page's address; off disables them."),
    Knob { source: Source::Secret, ..env("PCDS_ADMIN_TOKEN", Type::String, "", "", "The admin API's bearer token (pcds token).") },
    env("PCDS_MAX_PLAYERS", Type::Int, "players", "16", "Players at once while the limits.maxPlayers rule is unset."),
    env("PCDS_WORLD", Type::String, "", "world", "The world pcds starts: worlds/<name>.json (pcds world use NAME)."),
    env("PCDS_ACCESS", Type::String, "", "", "The access lists file; unset: access.json next to the world."),
    env("PCDS_RULES", Type::String, "", "", "The rules file; unset: rules.json next to the world."),
    env("PCDS_WEB", Type::String, "", "", "This web page's file; unset: web/index.html next to the server binary."),
    env("PCDS_TRACE", Type::String, "", "", "Set: log every incoming message and RPC."),
];

pub fn knob_for(key: &str) -> Option<&'static Knob> {
    KNOBS.iter().find(|k| k.key == key)
}

/// The value at "a.b" in a JSON object (Null when there is none).
pub fn at(v: &Value, key: &str) -> Value {
    key.split('.').fold(v, |v, k| &v[k]).clone()
}

/// The "section.rule" paths of the rules, with their values.
fn leaves(rules: &Value) -> Vec<(String, Value)> {
    let sections = rules.as_object().into_iter().flatten();
    sections.flat_map(|(s, fields)| fields.as_object().into_iter().flatten().map(move |(f, v)| (format!("{s}.{f}"), v.clone()))).collect()
}

impl Type {
    fn name(self) -> &'static str {
        match self {
            Type::Bool => "bool",
            Type::Int => "int",
            Type::Float => "float",
            Type::String => "string",
            Type::Enum => "enum",
            Type::List => "list",
            Type::Map => "map",
        }
    }
}

/// The page's section for a knob: its key's first part ("speed" → "Speed"); server.env.
fn group(source: Source, key: &str) -> String {
    if matches!(source, Source::Env | Source::Secret) {
        return "server.env".into();
    }
    let mut g = key.split('.').next().unwrap_or_default().to_string();
    if let Some(first) = g.get_mut(..1) {
        first.make_ascii_uppercase();
    }
    g
}

impl Knob {
    /// server.env's: shown, changed only there.
    fn read_only(&self) -> bool {
        matches!(self.source, Source::Env | Source::Secret)
    }

    /// Whether a value is what this knob takes; null always is (unset, or back to the default).
    pub fn check(&self, v: &Value) -> Result<(), String> {
        let whole = |v: &Value| v.as_i64().is_some_and(|x| self.range.is_none_or(|(lo, hi)| (lo..=hi).contains(&(x as f64))));
        let ok = match self.kind {
            _ if v.is_null() => true,
            Type::Bool => v.is_boolean(),
            Type::Int => whole(v),
            // The rules are f32: compared as f32, so 0.01 is not below 0.01.
            Type::Float => v.as_f64().is_some_and(|x| x.is_finite() && self.range.is_none_or(|(lo, hi)| (lo as f32..=hi as f32).contains(&(x as f32)))),
            Type::String => v.is_string(),
            Type::Enum => v.as_str().is_some_and(|s| self.values.contains(&s)),
            Type::List => v.as_array().is_some_and(|a| a.iter().all(Value::is_string)),
            Type::Map => v.as_object().is_some_and(|m| m.values().all(whole)),
        };
        if ok {
            return Ok(());
        }
        let range = self.range.map(|(lo, hi)| format!(" from {lo} to {hi}")).unwrap_or_default();
        let expected = match self.kind {
            Type::Bool => "true or false".to_string(),
            Type::Int => format!("a whole number{range}"),
            Type::Float => format!("a number{range}"),
            Type::String => "text".into(),
            Type::Enum => format!("one of {:?}", self.values),
            Type::List => "a list of text".into(),
            Type::Map => format!("names with whole numbers{range}"),
        };
        // A secret's value never goes into a message (it may hold a token).
        let shown = if self.secret { "the value".to_string() } else { v.to_string() };
        Err(format!("{}: {shown} is not {expected}", self.key))
    }

    fn describe(&self, default: Value, value: Value) -> Value {
        let mut o = json!({"key": self.key, "group": group(self.source, self.key), "type": self.kind.name(), "unit": self.unit, "description": self.description, "live": self.live, "default": default, "value": value});
        if let Some((min, max)) = self.range {
            (o["min"], o["max"]) = (min.into(), max.into());
        }
        if !self.values.is_empty() {
            o["values"] = json!(self.values);
        }
        if self.secret {
            o["secret"] = true.into();
        }
        if self.read_only() {
            o["readOnly"] = "Set in server.env (pcds config); applies at the next restart (pcds restart). Shown: what this server started with.".into();
        }
        o
    }
}

/// A server.env value as its type (a number for an int that parses).
fn env_value(kind: Type, text: &str) -> Value {
    match (kind, text.parse::<i64>()) {
        _ if text.is_empty() => Value::Null,
        (Type::Int, Ok(n)) => n.into(),
        _ => text.into(),
    }
}

/// Every knob with its default and value: {"knobs": [...]}.
pub fn get(game: &Game, autosave: u64) -> Value {
    let rules = serde_json::to_value(&game.rules).unwrap_or_default();
    let defaults = serde_json::to_value(Rules::default()).unwrap_or_default();
    let access = serde_json::to_value(&game.access.lists).unwrap_or_default();
    let mut knobs: Vec<Value> = KNOBS
        .iter()
        .map(|k| {
            let (default, value) = match k.source {
                Source::Rules if k.secret => {
                    let set = at(&rules, k.key) != at(&defaults, k.key);
                    (Value::Null, if set { "(set)".into() } else { Value::Null })
                }
                Source::Rules => (at(&defaults, k.key), at(&rules, k.key)),
                Source::Access => (json!([]), at(&access, &k.key["access.".len()..])),
                // The one server knob: server.autosaveMinutes.
                Source::Server => (crate::autosave_start().into(), autosave.into()),
                Source::Env => (env_value(k.kind, k.default), env_value(k.kind, &crate::env(k.key).unwrap_or_default())),
                Source::Secret => (Value::Null, crate::env(k.key).map(|_| "(set)").into()),
            };
            k.describe(default, value)
        })
        .collect();
    // A rule without an entry still shows, typed by its default (the schema test fails until it
    // has one). ponytail: an unset (null) default reads as text; its entry gives the real type.
    for (key, default) in leaves(&defaults).into_iter().filter(|(key, _)| knob_for(key).is_none()) {
        let kind = match &default {
            Value::Bool(_) => Type::Bool,
            Value::Number(n) if n.is_f64() => Type::Float,
            Value::Number(_) => Type::Int,
            Value::Array(_) => Type::List,
            Value::Object(_) => Type::Map,
            _ => Type::String,
        };
        let mut o = rule("", kind, "", "Not in the configuration schema yet (config.rs).").describe(default, at(&rules, &key));
        (o["key"], o["group"]) = (key.as_str().into(), group(Source::Rules, &key).into());
        knobs.push(o);
    }
    json!({ "knobs": knobs })
}

/// A change from the API, {"key": value or null}: every value checked, then all applied (or
/// none). Err: {"error", "key"} for the API's answer.
pub fn set(game: &mut Game, autosave: &mut u64, body: &Value) -> Result<(), Value> {
    let fail = |key: &str, error: String| json!({ "error": error, "key": key });
    let Some(changes) = body.as_object() else { return Err(fail("", "a JSON object of setting: value".into())) };
    let defaults = serde_json::to_value(Rules::default()).unwrap_or_default();
    let mut rules = serde_json::to_value(&game.rules).unwrap_or_default();
    let mut access = serde_json::to_value(&game.access.lists).unwrap_or_default();
    let (mut access_changed, mut minutes) = (false, None);
    for (key, value) in changes {
        let source = match knob_for(key) {
            Some(k) if k.read_only() => return Err(fail(key, format!("{key}: set in server.env (pcds config), not here"))),
            Some(k) => {
                k.check(value).map_err(|e| fail(key, e))?;
                k.source
            }
            // A rule without an entry yet: Rules::validate and its type check it.
            None if leaves(&defaults).iter().any(|(k, _)| k == key) => Source::Rules,
            None => return Err(fail(key, format!("{key}: no such setting"))),
        };
        match source {
            Source::Rules => {
                let (section, field) = key.split_once('.').unwrap_or_default();
                match value {
                    Value::Null => drop(rules[section].as_object_mut().and_then(|s| s.remove(field))),
                    v => rules[section][field] = v.clone(),
                }
            }
            Source::Access => {
                access[&key["access.".len()..]] = if value.is_null() { json!([]) } else { value.clone() };
                access_changed = true;
            }
            Source::Server => minutes = Some(value.as_u64().unwrap_or_else(crate::autosave_start)),
            Source::Env | Source::Secret => unreachable!("read-only knobs are refused above"),
        }
    }
    let rules: Rules = serde_json::from_value(rules).map_err(|e| fail("", e.to_string()))?;
    let access: crate::access::Lists = serde_json::from_value(access).map_err(|e| fail("", e.to_string()))?;
    if rules != game.rules {
        crate::rules::set(game, rules).map_err(|e| {
            let key = e.split_once(':').map(|(k, _)| k).filter(|k| leaves(&defaults).iter().any(|(l, _)| l == k)).unwrap_or_default();
            fail(key, e.clone())
        })?;
    }
    if access_changed {
        game.access.set(access);
        crate::enforce(game);
    }
    if let Some(m) = minutes {
        *autosave = m;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule_knobs() -> impl Iterator<Item = &'static Knob> {
        KNOBS.iter().filter(|k| k.source == Source::Rules)
    }

    #[test]
    fn the_schema_covers_every_rule_and_nothing_else() {
        let rules: Vec<String> = leaves(&serde_json::to_value(Rules::default()).unwrap()).into_iter().map(|(k, _)| k).collect();
        let schema: Vec<&str> = rule_knobs().map(|k| k.key).collect();
        let missing: Vec<&String> = rules.iter().filter(|r| !schema.contains(&r.as_str())).collect();
        let extra: Vec<&&str> = schema.iter().filter(|k| !rules.iter().any(|r| r == **k)).collect();
        assert!(missing.is_empty(), "rules without an entry in config::KNOBS: {missing:?}");
        assert!(extra.is_empty(), "entries in config::KNOBS that are no rule: {extra:?}");
        let mut keys: Vec<&str> = KNOBS.iter().map(|k| k.key).collect();
        keys.sort();
        keys.dedup();
        assert_eq!(keys.len(), KNOBS.len(), "every key once");
    }

    /// Each rule takes what its entry says and refuses the rest, through Rules::validate (what
    /// rules.json and the API go through).
    #[test]
    fn each_rule_validates_as_its_schema_entry_says() {
        let Some(dir) = crate::test_data() else { return };
        let (_, data, _) = crate::tables(dir);
        for k in rule_knobs() {
            let takes = |v: Value| {
                let (section, field) = k.key.split_once('.').unwrap();
                Rules::default().patched(&json!({ section: { field: v } })).and_then(|r| r.validate(&data)).is_ok()
            };
            let default = at(&serde_json::to_value(Rules::default()).unwrap(), k.key);
            assert!(k.check(&default).is_ok() && takes(default.clone()), "{}: its default {default}", k.key);
            assert!(takes(Value::Null), "{}: null resets", k.key);
            let (good, bad): (Vec<Value>, Vec<Value>) = match (k.kind, k.range) {
                (Type::Bool, _) => (vec![json!(true), json!(false)], vec![json!(1), json!("yes")]),
                (Type::Float, Some((lo, hi))) => {
                    let step = (hi - lo) / 100.0;
                    (vec![json!(lo), json!(hi), json!((lo + hi) / 2.0)], vec![json!(lo - step), json!(hi + step), json!("1")])
                }
                (Type::Int, Some((lo, hi))) => (vec![json!(lo as i64), json!(hi as i64)], vec![json!(lo as i64 - 1), json!(hi as i64 + 1), json!(lo + 0.5)]),
                (Type::Enum, _) => (k.values.iter().map(|v| json!(v)).collect(), vec![json!("Nonsense"), json!(1)]),
                (Type::Map, Some((lo, _))) => (vec![json!({}), json!({"Drill1": lo as i64})], vec![json!({"Drill1": lo as i64 - 1}), json!({"Drill1": "x"}), json!([])]),
                (Type::List, None) if k.key == "chat.webhooks" => (vec![json!([]), json!(["https://example.com/hook"])], vec![json!(["example.com"]), json!([1]), json!("https://example.com")]),
                (Type::List, None) if k.key == "chat.exec" => (vec![json!([]), json!(["/bin/true", "-x"])], vec![json!([""]), json!([1]), json!("/bin/true")]),
                (Type::String, None) if k.key == "chat.secret" => (vec![json!("s3cret")], vec![json!(""), json!(1)]),
                (Type::String, None) if k.key == "chat.commandPrefix" => (vec![json!("!"), json!("pcds/")], vec![json!(""), json!("toolong!!"), json!("a b"), json!(1)]),
                (kind, range) => panic!("{}: {kind:?} with {range:?}: add its cases here", k.key),
            };
            for v in good {
                assert!(k.check(&v).is_ok() && takes(v.clone()), "{} takes {v}", k.key);
            }
            // Refused by Rules::validate: the entry's check, or the rule's own (URLs, the
            // program to run, the command prefix).
            for v in bad {
                assert!(!takes(v.clone()), "{} refuses {v}", k.key);
            }
        }
    }
}
