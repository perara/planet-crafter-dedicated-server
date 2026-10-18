//! Server rules: what this server changes about the game. They live in rules.json beside the
//! world (PCDS_RULES for another path) and change live through the admin API (GET and POST
//! /api/rules, `pcds rules`). Every default is the game's own behaviour, so a missing or empty
//! file changes nothing; a file with a typo or an out-of-range value stops the server at start
//! (and is refused by the API) rather than being half applied.
//!
//! Adding a rule: a field below with its default, its entry in config::KNOBS (type, range, unit,
//! description: `validate` checks it, the admin page shows it), its use at the one place the
//! game decides that value (read `game.rules` there), and a line in `changed` when a change
//! must reach what already exists (not just the next use).

use std::collections::BTreeMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::game::Game;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct Rules {
    pub meteo: Meteo,
    pub gauges: Gauges,
    pub speed: Speed,
    pub death: Death,
    pub loot: Loot,
    pub limits: Limits,
    pub world: World,
    pub chat: Chat,
    pub backup: Backup,
}

/// Off-site copies of the save backups: each new backup is uploaded to an S3-compatible bucket
/// (AWS S3, Cloudflare R2, Backblaze B2, MinIO...) as <prefix><backup file name>. Off while
/// s3Bucket is unset; the keys are never shown or logged.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct Backup {
    /// https://s3.<region>.amazonaws.com, https://<account>.r2.cloudflarestorage.com, ...
    pub s3_endpoint: Option<String>,
    pub s3_bucket: Option<String>,
    pub s3_region: String,
    pub s3_prefix: String,
    pub s3_access_key: Option<String>,
    pub s3_secret_key: Option<String>,
}

impl Default for Backup {
    fn default() -> Self {
        Backup { s3_endpoint: None, s3_bucket: None, s3_region: "us-east-1".into(), s3_prefix: "pcds/".into(), s3_access_key: None, s3_secret_key: None }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct Meteo {
    /// The chance of a random meteo event (weather, meteor showers) at each try, times this;
    /// 0 stops them. Events players cause (rockets sent to space) still come.
    pub event_chance: f32,
    /// Seconds before the materials a meteor drops disappear; 0: they stay. None: the game's
    /// time. (The rocks of an impact are the clients' own effect, out of the server's reach.)
    pub material_despawn_seconds: Option<f32>,
}

impl Default for Meteo {
    fn default() -> Self {
        Meteo { event_chance: 1.0, material_despawn_seconds: None }
    }
}

/// Drain multipliers on top of the world's own gauge setting (modifierGaugeDrain).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct Gauges {
    pub oxygen_drain: f32,
    pub thirst_drain: f32,
    pub food_drain: f32,
    /// Oxygen, water and health never drop below 1, so no one dies.
    pub no_death: bool,
}

impl Default for Gauges {
    fn default() -> Self {
        Gauges { oxygen_drain: 1.0, thirst_drain: 1.0, food_drain: 1.0, no_death: false }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct Speed {
    /// Terraformation from buildings (every unit but energy), on top of the world's pace.
    pub terraformation: f32,
    /// Extractors, collectors, auto-crafters and recyclers work this many times faster.
    pub machines: f32,
    /// Plants in vegetubes, spreaders, farms and vegetable growers grow this many times
    /// faster (told to the clients the way growth fuses are). Incubators keep their time.
    pub growth: f32,
}

impl Default for Speed {
    fn default() -> Self {
        Speed { terraformation: 1.0, machines: 1.0, growth: 1.0 }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct Death {
    /// NoConsequences, DropSomeItems, DropAllItems or DeleteSaveFile; None: the world's own.
    pub consequences: Option<String>,
    /// DropSomeItems: the percent chance each item is dropped (more than 4 items; with 4 or
    /// fewer all go). None: the game's 50.
    pub drop_chance: Option<f32>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct Loot {
    /// Items in generated containers (wreck safes, crates), times this, up to the size of the
    /// container.
    pub amount: f32,
}

impl Default for Loot {
    fn default() -> Self {
        Loot { amount: 1.0 }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct Limits {
    /// None: PCDS_MAX_PLAYERS, else 16.
    pub max_players: Option<usize>,
    /// Building group id -> how many the world may have. One more is taken down at once and
    /// its materials go back to the builder.
    pub buildings: BTreeMap<String, u32>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct World {
    /// With no player in the game (past PlayerReady, as the game counts its players), the world
    /// stands still: its clock, and with it every timer, day and night, growth, production,
    /// terraformation, meteo, drone and vehicle. It goes on from there when a player is in.
    pub pause_when_empty: bool,
}

/// Chat hooks (chathooks.rs): every chat message as a JSON event, sent off the game thread.
/// Only admins set these (rules.json, the admin API); webhooks, secret and exec are never logged.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct Chat {
    /// http:// or https:// URLs each event is POSTed to.
    pub webhooks: Vec<String>,
    /// Signs every POST: X-PCDS-Signature: sha256=<HMAC-SHA256 of the body, hex>. None: unsigned.
    pub secret: Option<String>,
    /// A program and its arguments (no shell), run for each event with its JSON on stdin.
    pub exec: Vec<String>,
    /// Messages starting with this are commands too: "!tp home" -> "tp", ["home"]. None: none.
    pub command_prefix: Option<String>,
    /// The longest a webhook or the exec program may take, in seconds.
    pub timeout_seconds: f32,
    /// More tries after a webhook fails (no answer, 429, 5xx), 1 s, 2 s... apart.
    pub retries: u32,
}

impl Default for Chat {
    fn default() -> Self {
        Chat { webhooks: Vec::new(), secret: None, exec: Vec::new(), command_prefix: None, timeout_seconds: 5.0, retries: 2 }
    }
}

/// Rules shown only as set, never with their value (URLs and arguments may hold tokens).
const HIDDEN: [&str; 5] = ["chat.webhooks", "chat.secret", "chat.exec", "backup.s3AccessKey", "backup.s3SecretKey"];

/// The rules as the admin API shows them: the HIDDEN ones as "(set)" when set.
pub fn shown(rules: &Rules) -> Value {
    let (mut v, d) = (serde_json::to_value(rules).unwrap_or_default(), serde_json::to_value(Rules::default()).unwrap_or_default());
    for key in HIDDEN {
        let (section, field) = key.split_once('.').unwrap_or_default();
        if v[section][field] != d[section][field] {
            v[section][field] = "(set)".into();
        }
    }
    v
}

/// What is wrong with a rules file serde could not read, naming the rule but never its value
/// (the message is logged and served by /api/logs, and a value may hold a token).
fn unreadable(text: &str, e: &serde_json::Error) -> String {
    let Ok(file) = serde_json::from_str::<Value>(text) else { return format!("not JSON (line {}, column {})", e.line(), e.column()) };
    let defaults = serde_json::to_value(Rules::default()).unwrap_or_default();
    let Some(sections) = file.as_object() else { return "not a JSON object of sections".into() };
    for (section, fields) in sections {
        let Some(fields) = fields.as_object().filter(|_| defaults[section].is_object()) else { return format!("{section}: no such section") };
        for (field, value) in fields {
            if defaults[section].get(field).is_none() {
                return format!("{section}.{field}: no such rule");
            }
            if Rules::default().patched(&serde_json::json!({ section: { field: value } })).is_err() {
                return format!("{section}.{field}: not a value this rule takes");
            }
        }
    }
    format!("not the rules' format (line {}, column {})", e.line(), e.column())
}

pub const CONSEQUENCES: [&str; 4] = ["NoConsequences", "DropSomeItems", "DropAllItems", "DeleteSaveFile"];

impl Rules {
    /// Every rule as its entry in config::KNOBS says, and the building limits name buildings.
    pub fn validate(&self, data: &crate::gamedata::GameData) -> Result<(), String> {
        let rules = serde_json::to_value(self).map_err(|e| e.to_string())?;
        for knob in crate::config::KNOBS.iter().filter(|k| k.source == crate::config::Source::Rules) {
            knob.check(&crate::config::at(&rules, knob.key))?;
        }
        let chat = &self.chat;
        // Never the value in the message: it may hold a token.
        for (i, url) in chat.webhooks.iter().enumerate() {
            let uri = url.parse::<ureq::http::Uri>().ok();
            if !uri.is_some_and(|u| matches!(u.scheme_str(), Some("http" | "https")) && u.host().is_some()) {
                return Err(format!("chat.webhooks: entry {} is not an http:// or https:// URL", i + 1));
            }
        }
        let b = &self.backup;
        if let Some(url) = &b.s3_endpoint {
            let uri = url.parse::<ureq::http::Uri>().ok();
            if !uri.is_some_and(|u| matches!(u.scheme_str(), Some("http" | "https")) && u.host().is_some() && u.path().trim_matches('/').is_empty()) {
                return Err("backup.s3Endpoint: an http:// or https:// address without a path (the bucket is s3Bucket)".into());
            }
        }
        if let Some(bucket) = &b.s3_bucket
            && (!(3..=63).contains(&bucket.len()) || !bucket.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'.' || c == b'-'))
        {
            return Err("backup.s3Bucket: 3 to 63 lower-case letters, digits, '.' and '-'".into());
        }
        if b.s3_region.is_empty() || !b.s3_region.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-') {
            return Err("backup.s3Region: letters, digits and '-' (us-east-1, auto, ...)".into());
        }
        if b.s3_prefix.contains("..") || b.s3_prefix.starts_with('/') || !b.s3_prefix.bytes().all(|c| c.is_ascii_alphanumeric() || b"/._-".contains(&c)) {
            return Err("backup.s3Prefix: letters, digits, '/', '.', '_' and '-', not starting with '/'".into());
        }
        if b.s3_access_key.as_deref() == Some("") || b.s3_secret_key.as_deref() == Some("") {
            return Err("backup.s3AccessKey / s3SecretKey: empty (null: unset)".into());
        }
        if chat.secret.as_deref() == Some("") {
            return Err("chat.secret: empty (null: unsigned)".into());
        }
        if chat.exec.first().is_some_and(|p| p.is_empty()) {
            return Err("chat.exec: the first entry is the program to run".into());
        }
        if chat.command_prefix.as_deref().is_some_and(|p| p.is_empty() || p.chars().count() > 8 || p.contains(char::is_whitespace)) {
            return Err("chat.commandPrefix: 1 to 8 characters, no spaces (null: no commands)".into());
        }
        for group in self.limits.buildings.keys() {
            if !data.group(group).is_some_and(|g| data.groups[g].constructible) {
                return Err(format!("limits.buildings: {group:?} is not a building's group id"));
            }
        }
        Ok(())
    }

    /// The rules after a JSON merge patch (RFC 7386: objects merge, null resets a value).
    /// Each value is checked as JSON first: a number past f32 would become infinity, which
    /// serializes as null and leaves a rules.json the server cannot load.
    pub fn patched(&self, patch: &Value) -> Result<Rules, String> {
        let mut v = serde_json::to_value(self).map_err(|e| e.to_string())?;
        merge(&mut v, patch);
        for knob in crate::config::KNOBS.iter().filter(|k| k.source == crate::config::Source::Rules) {
            knob.check(&crate::config::at(&v, knob.key))?;
        }
        serde_json::from_value(v).map_err(|e| e.to_string())
    }

    /// rules.json, or the defaults when there is none.
    pub fn load(path: &str) -> Result<Rules, String> {
        match std::fs::read_to_string(path) {
            Ok(text) if text.trim().is_empty() => Ok(Rules::default()),
            Ok(text) => serde_json::from_str(&text).map_err(|e| format!("{path}: {}", unreadable(&text, &e))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Rules::default()),
            Err(e) => Err(format!("{path}: {e}")),
        }
    }

    pub fn save(&self, path: &str) -> std::io::Result<()> {
        crate::save::write_atomic(path, (serde_json::to_string_pretty(self).unwrap_or_default() + "\n").as_bytes())
    }

    /// limits.maxPlayers, else PCDS_MAX_PLAYERS, else 16.
    pub fn max_players(&self) -> usize {
        self.limits.max_players.unwrap_or_else(|| crate::config::env_number("PCDS_MAX_PLAYERS") as usize)
    }
}

fn merge(target: &mut Value, patch: &Value) {
    let Value::Object(fields) = patch else {
        *target = patch.clone();
        return;
    };
    if !target.is_object() {
        *target = Value::Object(Default::default());
    }
    let map = target.as_object_mut().unwrap();
    for (k, v) in fields {
        match v {
            Value::Null => {
                map.remove(k);
            }
            _ => merge(map.entry(k.clone()).or_insert(Value::Null), v),
        }
    }
}

/// The rules read at start (checked already), where set() keeps them.
pub fn start(game: &mut Game, rules: Rules, path: String) {
    game.rules = rules;
    game.rules_path = Some(path);
    changed(game, &Rules::default());
    let changes = changes(&game.rules);
    if !changes.is_empty() {
        println!("rules: {}", changes.join(", "));
    }
}

/// The rules that differ from the game's behaviour, as "section.rule=value".
pub fn changes(rules: &Rules) -> Vec<String> {
    fn walk(path: &str, v: &Value, d: &Value, out: &mut Vec<String>) {
        match (v, d) {
            (Value::Object(o), Value::Object(defaults)) if !path.contains('.') => {
                for (k, x) in o {
                    let p = if path.is_empty() { k.clone() } else { format!("{path}.{k}") };
                    walk(&p, x, defaults.get(k).unwrap_or(&Value::Null), out);
                }
            }
            _ if v != d && HIDDEN.contains(&path) => out.push(format!("{path}=(set)")),
            _ if v != d => out.push(format!("{path}={v}")),
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk("", &serde_json::to_value(rules).unwrap_or_default(), &serde_json::to_value(Rules::default()).unwrap_or_default(), &mut out);
    out
}

/// New rules from the API: checked, kept in rules.json, and applied.
pub fn set(game: &mut Game, rules: Rules) -> Result<(), String> {
    rules.validate(&game.data)?;
    if let Some(path) = &game.rules_path {
        rules.save(path).map_err(|e| format!("{path}: {e}"))?;
    }
    let old = std::mem::replace(&mut game.rules, rules);
    changed(game, &old);
    Ok(())
}

/// What a change must reach at once; every other rule is read where it is used.
fn changed(game: &mut Game, old: &Rules) {
    if game.rules.speed.terraformation != old.speed.terraformation {
        let planets: Vec<i32> = game.units.planets.iter().map(|p| p.hash).collect();
        planets.into_iter().for_each(|p| game.units.force_reset(p));
    }
    if game.rules.speed.growth != old.speed.growth {
        crate::machines::add_growth_speed_to_all(game, game.rules.speed.growth - old.speed.growth);
    }
    if game.rules.chat != old.chat {
        crate::chathooks::configure(game);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_round_trip_and_patches_merge() {
        assert_eq!(serde_json::from_str::<Rules>("{}").unwrap(), Rules::default());
        let r = Rules::default().patched(&serde_json::json!({"gauges": {"thirstDrain": 0.5}, "limits": {"buildings": {"Drill1": 3}}})).unwrap();
        assert_eq!((r.gauges.thirst_drain, r.gauges.oxygen_drain, r.limits.buildings["Drill1"]), (0.5, 1.0, 3));
        let back = r.patched(&serde_json::json!({"gauges": {"thirstDrain": null}})).unwrap();
        assert_eq!(back.gauges.thirst_drain, 1.0, "null resets to the default");
        assert!(Rules::default().patched(&serde_json::json!({"gauges": {"thirstDrian": 2}})).is_err(), "typos are refused");
        // Past f32 it would be infinity, saved as null: refused before it gets there.
        for big in [serde_json::json!({"speed": {"growth": 1e39}}), serde_json::json!({"chat": {"timeoutSeconds": 1e39}}), serde_json::json!({"death": {"dropChance": 1e39}})] {
            assert!(Rules::default().patched(&big).is_err(), "{big}");
        }
    }

    #[test]
    fn out_of_range_rules_are_refused() {
        let Some(dir) = crate::test_data() else { return };
        let (_, data, _) = crate::tables(dir);
        assert!(Rules::default().validate(&data).is_ok());
        for patch in [
            serde_json::json!({"speed": {"machines": 0}}),
            serde_json::json!({"gauges": {"foodDrain": -1}}),
            serde_json::json!({"death": {"consequences": "Explode"}}),
            serde_json::json!({"limits": {"buildings": {"Iron": 1}}}),
            serde_json::json!({"meteo": {"materialDespawnSeconds": 1e9}}),
            serde_json::json!({"chat": {"webhooks": ["ftp://example.com/"]}}),
            serde_json::json!({"chat": {"webhooks": ["not a url"]}}),
            serde_json::json!({"chat": {"exec": [""]}}),
            serde_json::json!({"chat": {"commandPrefix": "a b"}}),
            serde_json::json!({"chat": {"timeoutSeconds": 0}}),
            serde_json::json!({"chat": {"retries": 11}}),
        ] {
            assert!(Rules::default().patched(&patch).and_then(|r| r.validate(&data)).is_err(), "{patch}");
        }
        assert!(Rules::default().patched(&serde_json::json!({"limits": {"buildings": {"Drill1": 2}}})).unwrap().validate(&data).is_ok());
        let hooks = serde_json::json!({"chat": {"webhooks": ["https://example.com/hook", "http://127.0.0.1:8080/"], "secret": "s", "exec": ["/usr/local/bin/on-chat"], "commandPrefix": "!"}});
        assert!(Rules::default().patched(&hooks).unwrap().validate(&data).is_ok());
    }

    #[test]
    fn a_bad_rules_file_is_named_without_its_values() {
        let dir = std::env::temp_dir().join(format!("pcds-rules-load-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rules.json").to_string_lossy().to_string();
        for (text, says) in [
            (r#"{"chat": {"webhooks": "https://example.com/hook/t0ken"}}"#, "chat.webhooks: not a value this rule takes"),
            (r#"{"chat": {"secret": 5}}"#, "chat.secret: not a value"),
            (r#"{"sped": {}}"#, "sped: no such section"),
            (r#"{"speed": {"grwoth": "t0ken"}}"#, "speed.grwoth: no such rule"),
            (r#"{"chat": {"secret": "t0ken""#, "not JSON"),
        ] {
            std::fs::write(&path, text).unwrap();
            let e = Rules::load(&path).unwrap_err();
            assert!(e.contains(says) && !e.contains("t0ken"), "{text}: {e}");
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn chat_hook_targets_and_secrets_are_never_shown() {
        let r = Rules::default().patched(&serde_json::json!({"chat": {"webhooks": ["https://example.com/hook/t0ken"], "secret": "s3cret", "exec": ["on-chat", "--key=k3y"], "commandPrefix": "!"}})).unwrap();
        let api = shown(&r);
        assert_eq!((api["chat"]["webhooks"].as_str(), api["chat"]["secret"].as_str(), api["chat"]["exec"].as_str(), api["chat"]["commandPrefix"].as_str()), (Some("(set)"), Some("(set)"), Some("(set)"), Some("!")));
        assert_eq!(shown(&Rules::default())["chat"]["webhooks"], serde_json::json!([]));
        let shown = changes(&r).join(" ");
        assert_eq!(shown, "chat.webhooks=(set) chat.secret=(set) chat.exec=(set) chat.commandPrefix=\"!\"");
    }

    use crate::game::tests::{join, save};
    use crate::game::{test_game, Args};
    use serde_json::json;

    fn inventory(g: &Game, client: u64) -> i32 {
        g.clients[&client].ready.as_ref().unwrap().inventory
    }

    #[test]
    fn machines_speed_runs_generators_faster() {
        // Side by side: when the normal collector has made its second bottle, the one at
        // twice the speed has made a third (its first comes at the same time).
        let world = |speed: f32| {
            let mut g = crate::machines::tests::game()?;
            g.rules.speed.machines = speed;
            let collector = crate::machines::tests::build(&mut g, "WaterCollector1", [0.0, 5.0, 0.0]);
            g.ext.machines.energy.insert(collector, true);
            Some((g, collector))
        };
        let (Some((mut normal, a)), Some((mut fast, b))) = (world(1.0), world(2.0)) else { return };
        let made = |g: &Game, wo: i32| g.inventories[&g.wos[&wo].linked_inventory].items.len();
        while made(&normal, a) < 2 && normal.time < 5000.0 {
            for g in [&mut normal, &mut fast] {
                g.time += 1.0;
                crate::machines::tick(g);
            }
        }
        assert_eq!(made(&normal, a), 2);
        assert!(made(&fast, b) >= 3, "{} at twice the speed", made(&fast, b));
    }

    #[test]
    fn growth_speed_grows_plants_faster_and_tells_the_clients() {
        let Some(mut g) = crate::machines::tests::game() else { return };
        g.rules.speed.growth = 2.0;
        let tube = crate::machines::tests::build(&mut g, "Vegetube1", [10.0, 5.0, 0.0]);
        assert!(crate::planets::tests::rpcs_sent(&g).iter().any(|r| r == "ConstructibleProxy.SetGrowerSpeedMultiplierClientRpc"));
        let inv = g.wos[&tube].linked_inventory;
        g.add_new_item(g.data.group("Seed0").unwrap(), inv, 0);
        g.ext.machines.energy.insert(tube, true);
        let (state, start) = crate::machines::growers::outside_grower_state(&g, tube).unwrap();
        g.time += 1.0;
        crate::machines::tick(&mut g);
        let mut replay = crate::geometry::MathRandom { state };
        assert_eq!(g.wos[&tube].growth, (start + 0.05 * 2.0 * 1.0 * replay.next_float()).min(100.0), "growth step at twice the multiplier");
    }

    #[test]
    fn gauge_drains_follow_the_rules_and_no_death_keeps_players_alive() {
        let after = |rules: Rules, seconds: usize| {
            let mut g = test_game(save())?;
            g.rules = rules;
            let player = join(&mut g, 1, "Ann");
            for _ in 0..seconds * 5 {
                g.time += 0.2;
                crate::gauges::tick(&mut g);
            }
            Some(crate::gauges::values(&g, player))
        };
        let Some(normal) = after(Rules::default(), 10) else { return };
        let thirsty = after(Rules::default().patched(&json!({"gauges": {"thirstDrain": 2}})).unwrap(), 10).unwrap();
        assert!(((100.0 - thirsty[1]) - 2.0 * (100.0 - normal[1])).abs() < 1e-3, "{normal:?} {thirsty:?}");
        let harsh = Rules::default().patched(&json!({"gauges": {"oxygenDrain": 100, "foodDrain": 100, "noDeath": true}})).unwrap();
        let alive = after(harsh, 60).unwrap();
        assert!(alive[0] >= 1.0 && alive[2] >= 1.0, "{alive:?}");
    }

    #[test]
    fn death_consequences_come_from_the_rules() {
        for (consequence, kept) in [("NoConsequences", true), ("DropAllItems", false)] {
            let Some(mut g) = test_game(save()) else { return };
            g.rules.death.consequences = Some(consequence.into());
            join(&mut g, 1, "Ann");
            let inv = inventory(&g, 1);
            let (_, iron) = g.add_new_item(g.data.group("Iron").unwrap(), inv, 0);
            crate::status::handle_dying_consequences(&mut g, 1);
            assert_eq!(g.inventories[&inv].items.contains(&iron), kept, "{consequence}");
        }
    }

    #[test]
    fn meteor_materials_despawn_when_the_rules_say() {
        for (rule, due) in [(None, Some(30.0)), (Some(0.0), None), (Some(5.0), Some(5.0))] {
            let Some(mut g) = test_game(save()) else { return };
            g.rules.meteo.material_despawn_seconds = rule;
            join(&mut g, 1, "Ann");
            let iron = g.data.groups[g.data.group("Iron").unwrap()].hash;
            g.world_objects_rpc(1, "WorldObjectsHandler.CreateAndDropOnFloorServerRpc", &Args(vec![json!(iron), json!([560.0, 3.0, 500.0]), json!(0.6), json!(true), json!(false), json!(30.0)]));
            let timer = g.timers.0.iter().find_map(|(t, timer)| matches!(timer, crate::scene::Timer::Despawn(..)).then_some(*t - g.time));
            assert_eq!(timer, due, "{rule:?}");
        }
    }

    #[test]
    fn an_empty_world_stands_still_and_goes_on_when_a_player_is_in() {
        // Steps of whole binary fractions keep the clock exact.
        fn run(g: &mut Game, seconds: f64, step: f64) {
            let end = g.sim_time.unwrap() + seconds;
            while g.sim_time.unwrap() < end {
                g.sim_time = Some(g.sim_time.unwrap() + step);
                g.tick();
            }
        }
        let Some(mut g) = test_game(save()) else { return };
        g.rules.world.pause_when_empty = true;
        g.sim_time = Some(0.0);
        run(&mut g, 10.0, 0.5);
        assert_eq!(g.time, 0.0, "empty from the start");
        join(&mut g, 1, "Ann");
        run(&mut g, 1.0, 0.125);
        assert!((g.time - 1.0).abs() < 1e-3, "on from where it stood, not 11 s on: {}", g.time);
        // Ann drops iron (despawns in 30 s) and leaves: nothing moves for 100 s.
        let iron = g.data.groups[g.data.group("Iron").unwrap()].hash;
        g.world_objects_rpc(1, "WorldObjectsHandler.CreateAndDropOnFloorServerRpc", &Args(vec![json!(iron), json!([560.0, 3.0, 500.0]), json!(0.6), json!(true), json!(false), json!(30.0)]));
        let (due, wo) = g.timers.0.iter().find_map(|(t, timer)| match timer { crate::scene::Timer::Despawn(wo, _) => Some((*t, *wo)), _ => None }).unwrap();
        g.disconnect(1);
        g.advanced = false;
        let (held, tick) = (g.time, g.tick_number());
        run(&mut g, 100.0, 0.5);
        assert_eq!((g.time, g.tick_number(), g.timers.0.len()), (held, tick, 1), "time, network ticks and timers stand still");
        assert!(g.wos.contains_key(&wo) && !g.advanced, "nothing despawned, nothing to autosave");
        // Bob comes: the clock goes on from there, and the iron goes 30 s after it was dropped.
        join(&mut g, 2, "Bob");
        run(&mut g, 29.0, 0.125);
        assert!((g.time - (held + 29.0)).abs() < 1e-3 && g.wos.contains_key(&wo), "no timer fires because the clock moved: {}", g.time);
        run(&mut g, 2.0, 0.125);
        assert!(g.time >= due && !g.wos.contains_key(&wo), "the despawn came on time");
        // Off again while empty: the world goes on.
        g.disconnect(2);
        g.rules.world.pause_when_empty = false;
        run(&mut g, 1.0, 0.5);
        assert!((g.time - (held + 32.0)).abs() < 1e-3, "{}", g.time);
    }

    #[test]
    fn a_building_past_its_limit_comes_down_and_pays_back() {
        let Some(mut g) = test_game(save()) else { return };
        g.rules.limits.buildings.insert("Vegetube1".into(), 1);
        join(&mut g, 1, "Ann");
        let group = g.data.group("Vegetube1").unwrap();
        let (hash, recipe) = (g.data.groups[group].hash, g.data.groups[group].recipe.len());
        let inv = inventory(&g, 1);
        let build = |g: &mut Game, x: f32| {
            g.world_objects_rpc(1, "WorldObjectsHandler.CreateAndInstantiateWorldObjectServerRpc", &Args(vec![json!(hash), json!([x, 3.0, 500.0]), json!([0.0, 0.0, 0.0, 1.0]), json!(false), json!(false), json!(true), json!(false)]));
        };
        build(&mut g, 560.0);
        let before = g.inventories[&inv].items.len();
        build(&mut g, 570.0);
        assert_eq!(g.group_count(true, group), 1);
        assert_eq!(g.inventories[&inv].items.len(), before + recipe, "the second one's materials are back");
        assert!(crate::planets::tests::rpcs_sent(&g).iter().any(|r| r == "ChatHandler.ReceiveChatMessageClientRpc"));
    }

    #[test]
    fn max_players_and_loot_amount_come_from_the_rules() {
        let Some(mut g) = test_game(save()) else { return };
        g.rules.limits.max_players = Some(1);
        join(&mut g, 1, "Ann");
        g.connect(2, None, false, None);
        let data = format!("{}|0", g.net_data.game_version);
        let mut w = crate::wire::Writer::new();
        w.packed_i32(0).u64(0).i32(data.len() as i32).bytes(data.as_bytes());
        g.receive(2, &crate::wire::batch(&[(crate::protocol::CONNECTION_REQUEST, w.0)]));
        assert!(g.kicks.iter().any(|(c, reason)| *c == 2 && reason == "MULTIPLAYER_Game_Session_Full"));
        // A wreck safe holds nothing at loot amount 0.
        let Some(mut g) = crate::planets::tests::game() else { return };
        g.rules.loot.amount = 0.0;
        let safe = g.data.group("ProceduralWreckSafe").unwrap();
        let id = g.new_world_object_id();
        g.create_world_object(safe, id, true);
        let planet = g.planet_hash();
        let w = g.wos.get_mut(&id).unwrap();
        w.position = [600.0, 5.0, 500.0];
        w.planet = planet;
        g.instantiate(id, false, 0);
        assert!(g.inventories[&g.wos[&id].linked_inventory].items.is_empty());
    }

    #[test]
    fn terraformation_speed_scales_what_buildings_make_but_not_energy() {
        let rates = |pace: f32| {
            let mut g = crate::machines::tests::game()?;
            g.rules.speed.terraformation = pace;
            crate::machines::tests::build(&mut g, "Fireplace", [10.0, 5.0, 0.0]);
            crate::machines::tests::build(&mut g, "EnergyGenerator1", [20.0, 5.0, 0.0]);
            for i in 1..=50 {
                g.sim_time = Some(i as f64 * 0.1);
                g.tick();
            }
            let p = g.units.planet(g.planet_hash())?;
            Some((p.get(crate::units::Kind::Heat).increase, p.get(crate::units::Kind::Energy).increase))
        };
        let (Some((heat, energy)), Some((heat2, energy2))) = (rates(1.0), rates(2.0)) else { return };
        assert!(heat > 0.0 && (heat2 - 2.0 * heat).abs() < 1e-6, "{heat} {heat2}");
        assert!(energy > 0.0 && energy == energy2, "{energy} {energy2}");
    }
}
