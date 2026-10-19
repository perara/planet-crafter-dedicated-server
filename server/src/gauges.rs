//! SpaceCraft.PlayerGaugesHandler, server side: every player's oxygen, thirst, health and
//! toxicity, drained every 0.2 s from the moment they are set up.

use std::collections::HashMap;

use crate::game::{Args, Game};
use crate::net::{enc, VarValue};

const CLASS: &str = "PlayerGaugesHandler";
const INITIAL: f32 = 100.0;
const INSIDE_OXYGEN: f32 = 60.0;
const UNDERWATER_OXYGEN: f32 = -3.6;
const TICK: f32 = 0.2;

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct State {
    pub client: u64,
    pub next_tick: f32,
    pub rebreather: bool,
    pub outside_oxygen: f32,
}

#[derive(Default, serde::Serialize, serde::Deserialize)]
pub struct Gauges(pub HashMap<u64, State>);

fn drain(game: &Game) -> f32 {
    game.save.settings["modifierGaugeDrain"].as_f64().unwrap_or(1.0) as f32
}

fn get_f(game: &Game, object: u64, var: &str) -> f32 {
    match game.net.get(object, CLASS, var) {
        Some(VarValue::Plain(b)) if b.len() == 4 => f32::from_le_bytes(b[..4].try_into().unwrap()),
        _ => 0.0,
    }
}

fn get_b(game: &Game, object: u64, var: &str) -> bool {
    matches!(game.net.get(object, CLASS, var), Some(VarValue::Plain(b)) if b.first() == Some(&1))
}

fn set_f(game: &mut Game, object: u64, var: &str, v: f32) {
    game.net.set(object, CLASS, var, enc::float(v));
}

fn set_b(game: &mut Game, object: u64, var: &str, v: bool) {
    game.net.set(object, CLASS, var, enc::bool(v));
}

/// Oxygen, thirst, health, toxic of a player object.
pub fn values(game: &Game, object: u64) -> [f32; 4] {
    ["_oxygenValue", "_thirstValue", "_healthValue", "_toxicValue"].map(|v| get_f(game, object, v))
}

/// The maxima of `values` (toxicity's is a fixed 100).
pub fn maxima(game: &Game, object: u64) -> [f32; 4] {
    let [o, w, h] = ["_oxygenMaxValue", "_thirstMaxValue", "_healthMaxValue"].map(|v| get_f(game, object, v));
    [o, w, h, INITIAL]
}

/// UpdateGaugesDependingOnEquipmentServerRpc: the oxygen maximum the first tank gives.
fn oxygen_max(game: &Game, equipment: i32) -> Option<f32> {
    game.inventories.get(&equipment)?.items.iter().find_map(|i| {
        let g = &game.data.groups[game.wos.get(i)?.group];
        (g.str("equipableType") == "OxygenTank").then(|| INITIAL + g.fields["value"].as_i64().unwrap_or(0) as f32)
    })
}

/// SetHasRebreatherServerRpc.
fn has_rebreather(game: &Game, equipment: i32) -> bool {
    let rebreather = game.data.singletons["PlayerGaugesHandler"]["oxygenRebreatherGroupData"].as_str().and_then(|r| game.data.group_of_ref(r));
    match (rebreather, game.inventories.get(&equipment)) {
        (Some(g), Some(inv)) => inv.items.iter().any(|i| game.wos.get(i).is_some_and(|w| w.group == g)),
        _ => false,
    }
}

/// SetupPlayer: PlayerEquipment.InitEquipment (tank and rebreather), the saved values, then
/// StartGaugesConsumption.
pub fn start(game: &mut Game, client: u64, object: u64, gauges: [f32; 4], equipment: i32) {
    if let Some(max) = oxygen_max(game, equipment) {
        set_f(game, object, "_oxygenMaxValue", max);
    }
    let rebreather = has_rebreather(game, equipment);
    set_f(game, object, "_oxygenValue", gauges[0]);
    set_f(game, object, "_thirstValue", gauges[1]);
    set_f(game, object, "_healthValue", gauges[2]);
    set_f(game, object, "_toxicValue", gauges[3]);
    set_b(game, object, "_oxygenDecreaseValue", false);
    set_b(game, object, "_oxygenInfinityStatus", false);
    for max in ["_oxygenMaxValue", "_healthMaxValue", "_thirstMaxValue"] {
        if get_f(game, object, max) < f32::EPSILON {
            set_f(game, object, max, INITIAL);
        }
    }
    let outside = -1.9 * drain(game) * game.rules.gauges.oxygen_drain;
    game.ext.gauges.0.insert(object, State { client, next_tick: game.time, rebreather, outside_oxygen: outside });
}

fn add(game: &mut Game, object: u64, var: &str, max: &str, delta: f32) {
    let max = get_f(game, object, max).max(0.0);
    let mut v = (get_f(game, object, var) + delta).clamp(0.0, max);
    // Rules: noDeath keeps the vital gauges at 1 or more.
    if game.rules.gauges.no_death {
        v = v.max(1.0f32.min(max));
    }
    set_f(game, object, var, v);
}

fn add_health(game: &mut Game, object: u64, delta: f32) {
    add(game, object, "_healthValue", "_healthMaxValue", delta);
    if get_f(game, object, "_healthValue") < f32::EPSILON {
        set_f(game, object, "_toxicValue", 0.0);
    }
}

/// Terraformation threshold of one of the current planet's stages ("noNeedForOxygenStage").
fn stage_value(game: &Game, field: &str) -> f64 {
    let planet = game.planet_id();
    let stage = game.data.asset(&format!("PlanetData:PlanetData-{planet}")).and_then(|p| p[field].as_str()).unwrap_or("");
    game.data.asset(stage).and_then(|s| s["startValue"].as_f64()).unwrap_or(f64::MAX)
}

/// SetOutsideOxygenChangeValue.
fn outside_oxygen(game: &mut Game, object: u64, rebreather: bool) -> f32 {
    let rate = -1.9 * drain(game) * game.rules.gauges.oxygen_drain;
    let terra = crate::units::value(game, crate::units::Kind::Terraformation, 0);
    let no_need = stage_value(game, "noNeedForOxygenStage");
    let mut outside = rate;
    if get_b(game, object, "_playerIsUnderWater") {
        if terra > no_need {
            outside = UNDERWATER_OXYGEN * drain(game) * game.rules.gauges.oxygen_drain;
        }
    } else if terra > no_need {
        outside = INSIDE_OXYGEN;
    } else if rebreather {
        let start = stage_value(game, "startBreathMoreStage");
        let t = (((terra - start) / (no_need - start)) as f32).clamp(0.0, 1.0);
        outside = rate + (0.0 - rate) * t;
    }
    set_f(game, object, "_currentOutsideOxygenChangeValuePerSec", outside);
    set_b(game, object, "_oxygenDecreaseValue", rebreather && outside < 0.0);
    set_b(game, object, "_oxygenInfinityStatus", outside >= 0.0);
    outside
}

/// GaugesTick for every player due.
pub fn tick(game: &mut Game) {
    let due: Vec<(u64, State)> = game.ext.gauges.0.iter().filter(|(_, s)| s.next_tick <= game.time).map(|(o, s)| (*o, s.clone())).collect();
    for (object, mut s) in due {
        if !game.net.objects.contains_key(&object) {
            game.ext.gauges.0.remove(&object);
            continue;
        }
        s.next_tick += TICK;
        if !get_b(game, object, "_pauseConsumption") {
            s.outside_oxygen = outside_oxygen(game, object, s.rebreather);
            // PlayerEffects only live on the client that ate the food: no effect here.
            let oxygen = if get_b(game, object, "_playerCanBreath") { INSIDE_OXYGEN } else { s.outside_oxygen };
            add(game, object, "_oxygenValue", "_oxygenMaxValue", oxygen * TICK);
            add(game, object, "_thirstValue", "_thirstMaxValue", -0.13 * drain(game) * game.rules.gauges.thirst_drain * TICK);
            add_health(game, object, -0.06 * drain(game) * game.rules.gauges.food_drain * TICK);
            let toxic_rate = -drain(game);
            if toxic_rate.abs() > 0.0 && get_f(game, object, "_healthValue") > 0.0 {
                let filter = 1.0 - crate::equipment::equipment_value(game, s.client, "ToxinsFilter") as f32 / 100.0;
                match crate::toxic::tracked_area_progress(game, s.client) {
                    Some((progress, toxic_time)) if progress < 1.0 => {
                        let rate = toxic_rate * (1.0 - progress) * filter;
                        set_f(game, object, "_toxicDecreaseValue", rate);
                        set_f(game, object, "_toxicMaxValue", toxic_time);
                        let v = toxic_time.min(get_f(game, object, "_toxicValue") - TICK * rate);
                        set_f(game, object, "_toxicValue", v);
                    }
                    _ => {
                        set_f(game, object, "_toxicDecreaseValue", 0.0);
                        let v = (get_f(game, object, "_toxicValue") - 10.0 * drain(game) * TICK * (1.0 + (1.0 - filter))).max(0.0);
                        set_f(game, object, "_toxicValue", v);
                    }
                }
            } else {
                set_f(game, object, "_toxicValue", 0.0);
            }
            crate::status::check_dying(game, s.client, object);
        }
        game.ext.gauges.0.insert(object, s);
    }
}

/// The PlayerGaugesHandler RPCs (on the sender's own player object).
pub fn rpc(game: &mut Game, client: u64, object: u64, name: &str, a: &Args) -> bool {
    if game.player_object(client) != Some(object) {
        return name.starts_with("PlayerGaugesHandler.");
    }
    match name {
        "PlayerGaugesHandler.AddOxygenServerRpc" => add(game, object, "_oxygenValue", "_oxygenMaxValue", a.f(0)),
        "PlayerGaugesHandler.AddWaterServerRpc" => add(game, object, "_thirstValue", "_thirstMaxValue", a.f(0)),
        "PlayerGaugesHandler.AddHealthServerRpc" => add_health(game, object, a.f(0)),
        "PlayerGaugesHandler.RemoveToxicServerRpc" => {
            let v = (get_f(game, object, "_toxicValue") - a.f(0)).max(0.0);
            set_f(game, object, "_toxicValue", v);
        }
        "PlayerGaugesHandler.DrinkServerRpc" => add(game, object, "_thirstValue", "_thirstMaxValue", a.i(0) as f32),
        "PlayerGaugesHandler.BreathServerRpc" => add(game, object, "_oxygenValue", "_oxygenMaxValue", a.i(0) as f32),
        "PlayerGaugesHandler.EatServerRpc" => add_health(game, object, a.i(0) as f32),
        "PlayerGaugesHandler.PurifyServerRpc" => {
            let v = (get_f(game, object, "_toxicValue") - a.i(0) as f32).max(0.0);
            set_f(game, object, "_toxicValue", v);
        }
        "PlayerGaugesHandler.UpdateGaugesDependingOnEquipmentServerRpc" => {
            let max = oxygen_max(game, a.i(0)).unwrap_or(INITIAL);
            if get_f(game, object, "_oxygenValue") > max {
                set_f(game, object, "_oxygenValue", max);
            }
            set_f(game, object, "_oxygenMaxValue", max);
        }
        "PlayerGaugesHandler.SetHasRebreatherServerRpc" => {
            let has = has_rebreather(game, a.i(0));
            if let Some(s) = game.ext.gauges.0.get_mut(&object) {
                s.rebreather = has;
            }
        }
        _ => return false,
    }
    true
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::testkit::*;

    fn set(g: &mut Game, object: u64, values: [f32; 4]) {
        for (var, v) in ["_oxygenValue", "_thirstValue", "_healthValue", "_toxicValue"].into_iter().zip(values) {
            set_f(g, object, var, v);
        }
    }

    /// Each gauge RPC moves the sender's own gauge, clamped to its maximum and zero; one sent
    /// on someone else's player changes nothing. The save holds the result.
    #[test]
    fn gauge_rpcs_change_the_senders_own_gauges_within_their_bounds() {
        let Some((mut g, ann)) = world() else { return };
        let bob = join(&mut g, 2, "Bob");
        let cases: [(&str, serde_json::Value, [f32; 4]); 8] = [
            ("PlayerGaugesHandler.AddOxygenServerRpc", json!(30.5), [80.5, 50.0, 50.0, 40.0]),
            ("PlayerGaugesHandler.AddWaterServerRpc", json!(80.0), [50.0, 100.0, 50.0, 40.0]),
            ("PlayerGaugesHandler.AddHealthServerRpc", json!(-10.0), [50.0, 50.0, 40.0, 40.0]),
            ("PlayerGaugesHandler.RemoveToxicServerRpc", json!(55.0), [50.0, 50.0, 50.0, 0.0]),
            ("PlayerGaugesHandler.DrinkServerRpc", json!(-60), [50.0, 0.0, 50.0, 40.0]),
            ("PlayerGaugesHandler.BreathServerRpc", json!(20), [70.0, 50.0, 50.0, 40.0]),
            ("PlayerGaugesHandler.EatServerRpc", json!(-50), [50.0, 50.0, 0.0, 0.0]),
            ("PlayerGaugesHandler.PurifyServerRpc", json!(15), [50.0, 50.0, 50.0, 25.0]),
        ];
        for (name, arg, want) in cases {
            set(&mut g, ann, [50.0, 50.0, 50.0, 40.0]);
            set(&mut g, bob, [50.0, 50.0, 50.0, 40.0]);
            send_on(&mut g, 1, ann, name, std::slice::from_ref(&arg));
            assert_eq!(values(&g, ann), want, "{name}");
            send_on(&mut g, 1, bob, name, &[arg]);
            assert_eq!(values(&g, bob), [50.0, 50.0, 50.0, 40.0], "{name} on another player");
        }
        let saved = g.snapshot().players.into_iter().find(|p| p["name"] == "Ann").unwrap();
        assert_eq!((saved["playerGaugeHealth"].as_f64(), saved["playerGaugeToxic"].as_f64()), (Some(50.0), Some(25.0)));
    }

    /// The first oxygen tank in the equipment sets the maximum; without one it is 100 again,
    /// and oxygen above it is cut. A rebreather in the equipment slows oxygen loss outside.
    #[test]
    fn equipment_sets_the_oxygen_maximum_and_the_rebreather() {
        let Some((mut g, ann)) = world() else { return };
        let (_, equipment) = backpack(&g, 1);
        let tank = give(&mut g, "OxygenTank1", equipment);
        send_on(&mut g, 1, ann, "PlayerGaugesHandler.UpdateGaugesDependingOnEquipmentServerRpc", &[json!(equipment)]);
        let bonus = g.data.groups[g.wos[&tank].group].fields["value"].as_i64().unwrap() as f32;
        assert!(bonus > 0.0);
        assert_eq!(get_f(&g, ann, "_oxygenMaxValue"), 100.0 + bonus);
        set_f(&mut g, ann, "_oxygenValue", 100.0 + bonus);
        g.inventory_remove(equipment, tank);
        send_on(&mut g, 1, ann, "PlayerGaugesHandler.UpdateGaugesDependingOnEquipmentServerRpc", &[json!(equipment)]);
        assert_eq!((get_f(&g, ann, "_oxygenMaxValue"), get_f(&g, ann, "_oxygenValue")), (100.0, 100.0));

        let rebreather = g.data.singletons["PlayerGaugesHandler"]["oxygenRebreatherGroupData"].as_str().and_then(|r| g.data.group_of_ref(r)).expect("the rebreather group");
        let id = g.create_world_object(rebreather, 0, true);
        g.inventory_add(equipment, id, true);
        send_on(&mut g, 1, ann, "PlayerGaugesHandler.SetHasRebreatherServerRpc", &[json!(equipment)]);
        assert!(g.ext.gauges.0[&ann].rebreather);
        g.inventory_remove(equipment, id);
        send_on(&mut g, 1, ann, "PlayerGaugesHandler.SetHasRebreatherServerRpc", &[json!(equipment)]);
        assert!(!g.ext.gauges.0[&ann].rebreather);
    }

    /// Every 0.2 s: oxygen falls outside, rises where the player can breathe (a habitat) and
    /// falls more slowly under water once the air is breathable; nothing moves while paused.
    #[test]
    fn gauges_drain_every_fifth_of_a_second_as_in_the_game() {
        let Some((mut g, ann)) = world() else { return };
        let step = |g: &mut Game| {
            g.time += TICK;
            tick(g);
            values(g, ann)
        };
        set(&mut g, ann, [50.0, 50.0, 50.0, 0.0]);
        let v = step(&mut g);
        assert!((v[0] - (50.0 - 1.9 * TICK)).abs() < 1e-4 && v[1] < 50.0 && v[2] < 50.0, "{v:?}");
        assert!(!get_b(&g, ann, "_oxygenInfinityStatus") && get_f(&g, ann, "_currentOutsideOxygenChangeValuePerSec") == -1.9);
        g.net.set(ann, CLASS, "_playerCanBreath", enc::bool(true));
        let before = values(&g, ann)[0];
        assert!((step(&mut g)[0] - (before + INSIDE_OXYGEN * TICK)).abs() < 1e-4);
        g.net.set(ann, CLASS, "_playerCanBreath", enc::bool(false));
        g.net.set(ann, CLASS, "_pauseConsumption", enc::bool(true));
        let paused = values(&g, ann);
        assert_eq!(step(&mut g), paused);
        g.net.set(ann, CLASS, "_pauseConsumption", enc::bool(false));
        // Air breathable (terraformation above the stage) but under water: -3.6/s.
        let stage = stage_value(&g, "noNeedForOxygenStage");
        let planet = g.planet_hash();
        let units = &mut g.units.planet_mut(planet).unwrap().units;
        units.iter_mut().find(|u| u.kind == crate::units::Kind::Terraformation).unwrap().total = stage as f32 * 2.0;
        g.net.set(ann, CLASS, "_playerIsUnderWater", enc::bool(true));
        let before = values(&g, ann)[0];
        let after = step(&mut g)[0];
        assert!((after - (before + UNDERWATER_OXYGEN * TICK)).abs() < 1e-4, "{before} -> {after}, stage {stage}, outside {}", get_f(&g, ann, "_currentOutsideOxygenChangeValuePerSec"));
        g.net.set(ann, CLASS, "_playerIsUnderWater", enc::bool(false));
        step(&mut g);
        assert!(get_b(&g, ann, "_oxygenInfinityStatus"), "breathable air: oxygen does not run out");
        // A player who left is dropped from the ticks.
        g.disconnect(1);
        step(&mut g);
        assert!(!g.ext.gauges.0.contains_key(&ann));
    }
}
