//! Server rules: what this server changes about the game. They live in rules.json beside the
//! world (PCDS_RULES for another path) and change live through the admin API (GET and POST
//! /api/rules, `pcds rules`). Every default is the game's own behaviour, so a missing or empty
//! file changes nothing; a file with a typo or an out-of-range value stops the server at start
//! (and is refused by the API) rather than being half applied.
//!
//! Adding a rule: a field below with its default, its range in `validate`, its use at the one
//! place the game decides that value (read `game.rules` there), and a line in `changed` when a
//! change must reach what already exists (not just the next use).

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

pub const CONSEQUENCES: [&str; 4] = ["NoConsequences", "DropSomeItems", "DropAllItems", "DeleteSaveFile"];

impl Rules {
    pub fn validate(&self, data: &crate::gamedata::GameData) -> Result<(), String> {
        let multiplier = |name: &str, v: f32| if v.is_finite() && (0.0..=100.0).contains(&v) { Ok(()) } else { Err(format!("{name}: {v} is not a multiplier from 0 to 100")) };
        multiplier("meteo.eventChance", self.meteo.event_chance)?;
        multiplier("gauges.oxygenDrain", self.gauges.oxygen_drain)?;
        multiplier("gauges.thirstDrain", self.gauges.thirst_drain)?;
        multiplier("gauges.foodDrain", self.gauges.food_drain)?;
        multiplier("speed.terraformation", self.speed.terraformation)?;
        multiplier("loot.amount", self.loot.amount)?;
        // A machine or plant at speed 0 would never be due again: from 0.01.
        for (name, v) in [("speed.machines", self.speed.machines), ("speed.growth", self.speed.growth)] {
            multiplier(name, v)?;
            if v < 0.01 {
                return Err(format!("{name}: {v} is below 0.01"));
            }
        }
        if let Some(s) = self.meteo.material_despawn_seconds.filter(|s| !(s.is_finite() && (0.0..=86_400.0).contains(s))) {
            return Err(format!("meteo.materialDespawnSeconds: {s} is not 0 to 86400 seconds"));
        }
        if let Some(c) = self.death.consequences.as_deref().filter(|c| !CONSEQUENCES.contains(c)) {
            return Err(format!("death.consequences: {c:?} is not one of {CONSEQUENCES:?}"));
        }
        if let Some(p) = self.death.drop_chance.filter(|p| !(p.is_finite() && (0.0..=100.0).contains(p))) {
            return Err(format!("death.dropChance: {p} is not a percent"));
        }
        if self.limits.max_players.is_some_and(|n| !(1..=100).contains(&n)) {
            return Err("limits.maxPlayers: from 1 to 100".into());
        }
        for group in self.limits.buildings.keys() {
            if !data.group(group).is_some_and(|g| data.groups[g].constructible) {
                return Err(format!("limits.buildings: {group:?} is not a building's group id"));
            }
        }
        Ok(())
    }

    /// The rules after a JSON merge patch (RFC 7386: objects merge, null resets a value).
    pub fn patched(&self, patch: &Value) -> Result<Rules, String> {
        let mut v = serde_json::to_value(self).map_err(|e| e.to_string())?;
        merge(&mut v, patch);
        serde_json::from_value(v).map_err(|e| e.to_string())
    }

    /// rules.json, or the defaults when there is none.
    pub fn load(path: &str) -> Result<Rules, String> {
        match std::fs::read_to_string(path) {
            Ok(text) if text.trim().is_empty() => Ok(Rules::default()),
            Ok(text) => serde_json::from_str(&text).map_err(|e| format!("{path}: {e}")),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Rules::default()),
            Err(e) => Err(format!("{path}: {e}")),
        }
    }

    pub fn save(&self, path: &str) -> std::io::Result<()> {
        let tmp = format!("{path}.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(self).unwrap_or_default() + "\n")?;
        std::fs::rename(&tmp, path)
    }

    /// limits.maxPlayers, else PCDS_MAX_PLAYERS, else 16.
    pub fn max_players(&self) -> usize {
        self.limits.max_players.or_else(|| std::env::var("PCDS_MAX_PLAYERS").ok().and_then(|v| v.parse().ok())).unwrap_or(16)
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
        ] {
            assert!(Rules::default().patched(&patch).unwrap().validate(&data).is_err(), "{patch}");
        }
        assert!(Rules::default().patched(&serde_json::json!({"limits": {"buildings": {"Drill1": 2}}})).unwrap().validate(&data).is_ok());
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
