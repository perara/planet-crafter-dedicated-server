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
    rule("backup.s3Endpoint", Type::String, "", "An S3-compatible service to copy every save backup to: https://s3.<region>.amazonaws.com, https://<account>.r2.cloudflarestorage.com, a MinIO address... Unset: no off-site copies."),
    rule("backup.s3Bucket", Type::String, "", "The bucket the backups go to. Unset: off."),
    rule("backup.s3Region", Type::String, "", "The bucket's region (auto for Cloudflare R2)."),
    rule("backup.s3Prefix", Type::String, "", "Put before each backup's file name in the bucket."),
    rule("backup.s3AccessKey", Type::String, "", "The access key id of a key that may write to the bucket.").secret(),
    rule("backup.s3SecretKey", Type::String, "", "Its secret access key.").secret(),
    rule("limits.buildings", Type::Map, "", "Building group id to how many the world may have (Drill1 = 3). One more is taken down at once and its materials go back to the builder.").range(0.0, u32::MAX as f64),
    list("access.allowedIPs", "Addresses or CIDR ranges (192.168.1.0/24) that may connect over direct IP; empty: everyone. Players who no longer qualify are kicked."),
    list("access.whitelist", "Names or SteamID64s that may play; empty: everyone. SteamIDs count only for invite-code players, where they are verified."),
    list("access.bannedNames", "Banned names and SteamID64s."),
    list("access.bannedIPs", "Banned addresses or CIDR ranges (direct-IP players)."),
    knob(Source::Server, "server.autosaveMinutes", Type::Int, "min", "Minutes between saves; 0: only on stop and through the API. Until the next restart, which starts from PCDS_AUTOSAVE.").range(0.0, crate::MAX_AUTOSAVE as f64),
    env("PCDS_LISTEN", Type::String, "", "0.0.0.0:7777", "Direct-IP address (UDP) players join; off disables it."),
    env("PCDS_STEAM", Type::Enum, "", "on", "Invite codes through Steam; off disables them.").values(&["on", "off"]),
    Knob { source: Source::Secret, ..env("PCDS_STEAM_TOKEN", Type::String, "", "", "A Steam game server login token (app 1284190): keeps the invite code the same across restarts.") },
    env("PCDS_SESSION", Type::Int, "", "4242", "The last four digits of the invite code.").range(0.0, 9999.0),
    env("PCDS_AUTOSAVE", Type::Int, "min", "5", "Minutes between saves at start (server.autosaveMinutes changes it until the next restart).").range(0.0, crate::MAX_AUTOSAVE as f64),
    env("PCDS_BACKUPS", Type::Int, "", "24", "Save backups kept in backups/ next to the world; 0 keeps none.").range(0.0, 10_000.0),
    env("PCDS_BACKUP_MINUTES", Type::Int, "min", "60", "Minutes between those backups.").range(0.0, 525_600.0),
    env("PCDS_ADMIN", Type::String, "", "127.0.0.1:7782", "The admin API and web page's address; off disables them."),
    Knob { source: Source::Secret, ..env("PCDS_ADMIN_TOKEN", Type::String, "", "", "The admin API's bearer token (pcds token).") },
    env("PCDS_MAX_PLAYERS", Type::Int, "players", "16", "Players at once while the limits.maxPlayers rule is unset.").range(1.0, 100.0),
    env("PCDS_WORLD", Type::String, "", "world", "The world pcds starts: worlds/<name>.json (pcds world use NAME)."),
    env("PCDS_ACCESS", Type::String, "", "", "The access lists file; unset: run/access.json, the installation's (pcds run; a pcds serve started by hand: access.json next to the world)."),
    env("PCDS_RULES", Type::String, "", "", "The rules file; unset: run/rules.json, the installation's (pcds run; a pcds serve started by hand: rules.json next to the world)."),
    env("PCDS_WEB", Type::String, "", "", "This web page's file; unset: web/index.html next to the server binary."),
    env("PCDS_TRACE", Type::String, "", "", "Set: log every incoming message and RPC."),
    env("PCDS_TRUST_LOOPBACK", Type::Enum, "", "0", "1: believe the SteamID a direct-IP client from 127.0.0.1 claims (tests, the load and probe tools). Never behind a UDP relay, where every player comes from 127.0.0.1.").values(&["0", "1"]),
    env("PCDS_AUTO_UPDATE", Type::Enum, "", "off", "on: install each new server release from GitHub as it comes out, into the standby bank, and hand the running world over to it (a hot update: players stay connected). A release that needs a front restart is not installed.").values(&["off", "on"]),
    env("PCDS_UPDATE_CHECK_HOURS", Type::Int, "h", "24", "With PCDS_AUTO_UPDATE=on: hours between checks for a new release.").range(1.0, 8760.0),
    env("PCDS_RELEASES", Type::String, "", "", "Where pcds upgrade and PCDS_AUTO_UPDATE look up releases; unset: GitHub's API for this project. A mirror, or a test's server (plain http only for 127.0.0.1)."),
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
            Type::Float => v.as_f64().is_some_and(|x| (x as f32).is_finite() && self.range.is_none_or(|(lo, hi)| (lo as f32..=hi as f32).contains(&(x as f32)))),
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

/// Every server.env setting the server reads (config::KNOBS' Env and Secret entries), checked
/// once at start, as `get` (the environment, or the settings file) has them: Err naming each
/// setting that is not what its entry takes (a secret's value never shown). The server refuses
/// to start on it: a mistyped value is never read as the default.
pub fn check_env(get: impl Fn(&str) -> Option<String>) -> Result<(), String> {
    let mut bad = Vec::new();
    for k in KNOBS.iter().filter(|k| k.read_only()) {
        let Some(text) = get(k.key).filter(|t| !t.is_empty()) else { continue };
        let checked = match k.key {
            // Addresses: host:port, or off.
            "PCDS_LISTEN" | "PCDS_ADMIN" if text != "off" && text.parse::<std::net::SocketAddr>().is_err() => Err(format!("{}: {text:?} is not an address (HOST:PORT) or off", k.key)),
            _ => k.check(&env_value(k.kind, &text)),
        };
        bad.extend(checked.err());
    }
    if bad.is_empty() { Ok(()) } else { Err(format!("server.env: {} (pcds config edits it)", bad.join("; "))) }
}

/// A whole-number server.env setting, or its default: what check_env() let through at start.
pub fn env_number(key: &str) -> u64 {
    let k = knob_for(key).filter(|k| k.kind == Type::Int).unwrap_or_else(|| panic!("{key}: not a whole-number setting in config::KNOBS"));
    let text = crate::env(key).unwrap_or_else(|| k.default.to_string());
    text.parse().unwrap_or_else(|_| panic!("{key}={text:?}: check_env() refuses that at start"))
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
    let rule_error = |e: String| {
        let key = e.split_once(':').map(|(k, _)| k).filter(|k| leaves(&defaults).iter().any(|(l, _)| l == k)).unwrap_or_default();
        fail(key, e.clone())
    };
    let rules_changed = rules != game.rules;
    if rules_changed {
        rules.validate(&game.data).map_err(rule_error)?;
    }
    // All or none: everything is checked above; then access.json, then rules.json is written,
    // and a rules file that cannot be written puts the access lists back.
    let old_access = game.access.lists.clone();
    if access_changed {
        game.access.set(access).map_err(|e| fail("", e))?;
    }
    if rules_changed && let Err(e) = crate::rules::set(game, rules) {
        if access_changed {
            let _ = game.access.set(old_access);
        }
        return Err(rule_error(e));
    }
    if access_changed {
        crate::bank::enforce(game);
    }
    if let Some(m) = minutes {
        *autosave = m;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The PCDS_* names the source uses that are not KNOBS, so not server.env settings read at
    /// start: install (the installer and the commands read or set them), internal (the
    /// processes of this program talk through them, or a diagnostic of the table reader) and
    /// test (hooks and fixtures; some are honoured by the production binary because the end-to-end
    /// tests run it).
    const OTHER_ENV: &[(&str, &str)] = &[
        ("PCDS_ADMIN_BIND", "install"),
        ("PCDS_CONTAINER", "install"),
        ("PCDS_ENV_FILE", "install"),
        ("PCDS_GAME_DIR", "install"),
        ("PCDS_HOME", "install"),
        ("PCDS_NEW_WORLD", "install"),
        ("PCDS_PORT", "install"),
        ("PCDS_UNIT", "install"),
        ("PCDS_BANKS", "internal"),
        ("PCDS_DIFF_SHOW", "internal"),
        ("PCDS_IPC_FD", "internal"),
        ("PCDS_REACH_IL", "internal"),
        ("PCDS_WRECKS_TRACE", "internal"),
        ("PCDS_BANK_FAIL", "test"),
        ("PCDS_FLIP_VERIFY", "test"),
        ("PCDS_RNG_SEED", "test"),
        ("PCDS_TEST_DATA", "test"),
        ("PCDS_TEST_FAULTS", "test"),
    ];

    /// (file, text) of every source file's non-test part: up to its `#[cfg(test)] mod ...tests`,
    /// without the files that are test code only and without this one (which names them all).
    fn sources() -> Vec<(String, String)> {
        fn walk(dir: &std::path::Path, out: &mut Vec<(String, String)>) {
            for entry in std::fs::read_dir(dir).unwrap().flatten() {
                let path = entry.path();
                let name = entry.file_name().to_string_lossy().to_string();
                if path.is_dir() {
                    walk(&path, out);
                } else if name.ends_with(".rs") && !["bench.rs", "coverage.rs", "scenario.rs", "testkit.rs", "tests.rs", "play_tests.rs", "config.rs"].contains(&name.as_str()) {
                    let text = std::fs::read_to_string(&path).unwrap();
                    let lines: Vec<&str> = text.lines().collect();
                    let end = (0..lines.len()).find(|&i| lines[i] == "#[cfg(test)]" && lines.get(i + 1).is_some_and(|l| l.contains("mod ") && l.contains("tests"))).unwrap_or(lines.len());
                    out.push((path.display().to_string(), lines[..end].join("\n")));
                }
            }
        }
        let mut out = Vec::new();
        walk(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"), &mut out);
        out
    }

    /// Every PCDS_NAME in a text.
    fn names(text: &str) -> Vec<&str> {
        let mut out = Vec::new();
        for (at, _) in text.match_indices("PCDS_") {
            let len = text[at..].find(|c: char| !(c.is_ascii_uppercase() || c == '_')).unwrap_or(text.len() - at);
            if len > 5 {
                out.push(&text[at..at + len]);
            }
        }
        out
    }

    /// A PCDS_ name in the program is a KNOBS entry or on the list above with its scope, and
    /// the list holds no name that is gone.
    #[test]
    fn every_environment_name_in_the_source_is_in_the_schema_or_the_list() {
        let sources = sources();
        assert!(sources.len() > 40);
        for (file, text) in &sources {
            for name in names(text) {
                assert!(knob_for(name).is_some() || OTHER_ENV.iter().any(|(n, _)| *n == name), "{file} uses {name}: a KNOBS entry (config.rs) if it is a setting, else OTHER_ENV with its scope");
            }
        }
        for (name, scope) in OTHER_ENV {
            assert!(["install", "internal", "test"].contains(scope), "{name}: scope {scope}");
            assert!(knob_for(name).is_none(), "{name} is a KNOBS entry");
            assert!(sources.iter().any(|(_, text)| names(text).contains(name)), "{name} is in no source file any more: remove it from OTHER_ENV");
        }
    }

    /// The table rows of the README whose first cell is `x`: (x, the second cell).
    fn rows(readme: &str) -> Vec<(String, String)> {
        let cells = |line: &str| -> Vec<String> { line.trim().trim_matches('|').split(" | ").map(|c| c.trim().to_string()).collect() };
        readme.lines().filter(|l| l.starts_with("| `")).map(cells).filter(|c| c.len() >= 2).filter_map(|c| Some((c[0].strip_prefix('`')?.strip_suffix('`')?.to_string(), c[1].clone()))).collect()
    }

    /// The README's Settings table lists the server.env knobs and nothing else; where it gives
    /// a default and the knob has one, they are the same.
    #[test]
    fn the_readme_settings_are_the_server_env_knobs() {
        let readme = include_str!("../../README.md");
        let section = readme.split("\n## Settings\n").nth(1).unwrap().split("\n## ").next().unwrap();
        let documented = rows(section);
        let mut keys: Vec<&str> = documented.iter().map(|(k, _)| k.as_str()).collect();
        let mut wanted: Vec<&str> = KNOBS.iter().filter(|k| k.read_only()).map(|k| k.key).collect();
        keys.sort();
        wanted.sort();
        assert_eq!(keys, wanted, "README Settings table and config::KNOBS");
        for (key, default) in &documented {
            let knob = knob_for(key).unwrap();
            assert!(knob.default.is_empty() || *default == format!("`{}`", knob.default), "{key}: README says {default}, KNOBS {:?}", knob.default);
        }
    }

    /// The README's rule tables list the rules of KNOBS and nothing else; where they give a
    /// plain default, it is the rule's.
    #[test]
    fn the_readme_rules_are_the_rule_knobs() {
        let documented: Vec<(String, String)> = rows(include_str!("../../README.md")).into_iter().filter(|(k, _)| !k.starts_with("PCDS_")).collect();
        let mut keys: Vec<&str> = documented.iter().map(|(k, _)| k.as_str()).collect();
        let mut wanted: Vec<&str> = KNOBS.iter().filter(|k| k.source == Source::Rules).map(|k| k.key).collect();
        keys.sort();
        wanted.sort();
        assert_eq!(keys, wanted, "README rule tables and config::KNOBS");
        let defaults = serde_json::to_value(Rules::default()).unwrap();
        for (key, shown) in &documented {
            let text = match at(&defaults, key) {
                Value::Bool(b) => b.to_string(),
                Value::Number(n) => n.as_f64().map(|f| f.to_string()).unwrap_or_default(),
                Value::String(s) => s,
                _ => continue,
            };
            assert_eq!(*shown, format!("`{text}`"), "{key}: README and Rules::default()");
        }
    }

    #[test]
    fn server_env_settings_are_checked_at_start() {
        let with = |pairs: &[(&str, &str)]| {
            let pairs: Vec<(String, String)> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
            check_env(move |key| pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone()))
        };
        // Every default, and nothing set, starts.
        let defaults: Vec<(&str, &str)> = KNOBS.iter().filter(|k| k.read_only() && !k.default.is_empty()).map(|k| (k.key, k.default)).collect();
        with(&defaults).unwrap();
        with(&[]).unwrap();
        with(&[("PCDS_LISTEN", "off"), ("PCDS_ADMIN", "[::1]:7782"), ("PCDS_AUTOSAVE", "0"), ("PCDS_SESSION", "0042")]).unwrap();
        for (key, value, says) in [
            ("PCDS_AUTOSAVE", "five", "PCDS_AUTOSAVE: \"five\" is not a whole number from 0 to 1440"),
            ("PCDS_AUTOSAVE", "100000", "from 0 to 1440"),
            ("PCDS_BACKUPS", "-1", "PCDS_BACKUPS: -1 is not"),
            ("PCDS_BACKUP_MINUTES", "1h", "PCDS_BACKUP_MINUTES"),
            ("PCDS_SESSION", "12345", "PCDS_SESSION: 12345 is not a whole number from 0 to 9999"),
            ("PCDS_MAX_PLAYERS", "0", "PCDS_MAX_PLAYERS"),
            ("PCDS_AUTO_UPDATE", "yes", "PCDS_AUTO_UPDATE: \"yes\" is not one of"),
            ("PCDS_UPDATE_CHECK_HOURS", "0", "PCDS_UPDATE_CHECK_HOURS"),
            ("PCDS_STEAM", "true", "PCDS_STEAM"),
            ("PCDS_LISTEN", "7777", "PCDS_LISTEN: \"7777\" is not an address"),
            ("PCDS_ADMIN", "localhost:7782", "PCDS_ADMIN"),
        ] {
            let e = with(&[(key, value)]).unwrap_err();
            assert!(e.contains(says) && e.starts_with("server.env: "), "{key}={value}: {e}");
        }
        // Two at once are both named.
        let e = with(&[("PCDS_AUTOSAVE", "x"), ("PCDS_BACKUPS", "y")]).unwrap_err();
        assert!(e.contains("PCDS_AUTOSAVE") && e.contains("PCDS_BACKUPS"), "{e}");
        // The number readers take the default when unset.
        assert_eq!((env_number("PCDS_SESSION"), env_number("PCDS_UPDATE_CHECK_HOURS")), (4242, 24));
    }

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
                (Type::String, None) if k.key == "backup.s3Endpoint" => (vec![json!("https://s3.eu-north-1.amazonaws.com"), json!("http://127.0.0.1:9000")], vec![json!("ftp://x"), json!("https://s3.example.com/bucket"), json!(1)]),
                (Type::String, None) if k.key == "backup.s3Bucket" => (vec![json!("my-saves.pcds")], vec![json!("No_Upper"), json!("ab"), json!(1)]),
                (Type::String, None) if k.key == "backup.s3Region" => (vec![json!("eu-north-1"), json!("auto")], vec![json!(""), json!("eu north"), json!(1)]),
                (Type::String, None) if k.key == "backup.s3Prefix" => (vec![json!(""), json!("servers/prime/")], vec![json!("/abs"), json!("a/../b"), json!(1)]),
                (Type::String, None) if k.key == "backup.s3AccessKey" || k.key == "backup.s3SecretKey" => (vec![json!("AKIAEXAMPLE")], vec![json!(""), json!(1)]),
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
