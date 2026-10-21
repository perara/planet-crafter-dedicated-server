//! Terraformation: SpaceCraft.WorldUnitsHandler, WorldUnitsPlanet and the WorldUnit kinds.
//! Each planet has ten units; buildings' generation sets their per-second rates once a second,
//! and the totals grow every tenth of a second, scaled down for each extra player.

use serde_json::{json, Value};

use crate::clock::{add_young, GameTime};
use crate::data::stable_hash;
use crate::game::Game;
use crate::net::enc;

/// _allUnits order: the order the game updates them in.
#[derive(Clone, Copy, PartialEq, Debug, serde::Serialize, serde::Deserialize)]
pub enum Kind {
    Oxygen,
    Heat,
    Pressure,
    Energy,
    Terraformation,
    Biomass,
    Plants,
    Insects,
    Animals,
    Purification,
}

pub const KINDS: [Kind; 10] = [Kind::Oxygen, Kind::Heat, Kind::Pressure, Kind::Energy, Kind::Terraformation, Kind::Biomass, Kind::Plants, Kind::Insects, Kind::Animals, Kind::Purification];

impl Kind {
    fn var(self) -> &'static str {
        match self {
            Kind::Oxygen => "_unitOxygen",
            Kind::Heat => "_unitHeat",
            Kind::Pressure => "_unitPressure",
            Kind::Energy => "_unitEnergy",
            Kind::Terraformation => "_unitTerraformation",
            Kind::Biomass => "_unitBiomass",
            Kind::Plants => "_unitPlants",
            Kind::Insects => "_unitInsects",
            Kind::Animals => "_unitAnimals",
            Kind::Purification => "_unitPurification",
        }
    }
    /// DataConfig.WorldUnitType name, as world objects' generation is indexed.
    pub fn unit_type(self) -> &'static str {
        match self {
            Kind::Oxygen => "Oxygen",
            Kind::Heat => "Heat",
            Kind::Pressure => "Pressure",
            Kind::Energy => "Energy",
            Kind::Terraformation => "Terraformation",
            Kind::Biomass => "Biomass",
            Kind::Plants => "Plants",
            Kind::Insects => "Insects",
            Kind::Animals => "Animals",
            Kind::Purification => "Purification",
        }
    }
    fn labels(self) -> usize {
        match self {
            Kind::Energy => 1,
            Kind::Terraformation => 11,
            Kind::Biomass => 7,
            Kind::Plants | Kind::Insects | Kind::Animals => 6,
            Kind::Purification => 8,
            _ => 4,
        }
    }
    pub fn from_name(name: &str) -> Option<Kind> {
        KINDS.iter().copied().find(|k| k.unit_type() == name)
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Unit {
    pub kind: Kind,
    /// The game's `_currentTotalValue` is a float; here an f64 that holds the float's values while
    /// the clock is young (clock::add_young), so that a total of millions goes on growing after.
    pub total: f64,
    pub per_sec: f32,
    pub increase: f32,
    pub decrease: f32,
    pub label: i32,
    previous_index: u64,
    pub force_reset: bool,
    dirty: bool,
}

impl Unit {
    fn new(kind: Kind, total: f32) -> Unit {
        Unit { kind, total: f64::from(total), per_sec: 0.0, increase: 0.0, decrease: 0.0, label: 0, previous_index: 0, force_reset: false, dirty: true }
    }

    fn bytes(&self) -> Vec<u8> {
        [(self.total as f32).to_le_bytes(), self.per_sec.to_le_bytes(), self.increase.to_le_bytes(), self.decrease.to_le_bytes(), self.label.to_le_bytes()].concat()
    }

    fn set_label(&mut self, value: f64) {
        let previous = self.label;
        let index = if value > 0.0 { value.log(1000.0).floor() as i32 } else { 0 };
        self.label = index.max(0).min(self.kind.labels() as i32 - 1);
        if previous != self.label {
            self.dirty = true;
        }
    }

    fn base_define(&mut self) {
        let previous = self.per_sec;
        self.per_sec = self.increase + self.decrease;
        if (previous - self.per_sec).abs() > f32::EPSILON {
            self.dirty = true;
        }
    }

    fn reset_per_sec(&mut self) {
        if self.increase.abs() > 0.0 || self.decrease.abs() > 0.0 {
            self.increase = 0.0;
            self.decrease = 0.0;
            self.dirty = true;
        }
    }

    /// `steps` calls of the game's Compute(dt, factor) at game time `now`. Each is a float sum
    /// while the clock is young; from then on the total is an f64 and the steps, which all add the
    /// same, are one sum.
    fn compute(&mut self, steps: u64, dt: f32, factor: f32, now: GameTime) {
        let previous = self.total as f32;
        let delta = self.per_sec * dt * factor;
        if now.secs() < crate::clock::EXACT_AFTER {
            for _ in 0..steps {
                self.total = add_young(self.total, delta, now);
            }
        } else {
            self.total += steps as f64 * f64::from(delta);
        }
        // Dirty when the float the clients get moved: a total of millions moves in its steps.
        if (previous - self.total as f32).abs() > f32::EPSILON {
            self.dirty = true;
        }
        self.set_label(self.total);
    }

    fn is_increasing(&self) -> bool {
        self.increase > self.decrease.abs()
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct PlanetUnits {
    pub hash: i32,
    pub id: String,
    pub object: u64,
    pub units: Vec<Unit>,
}

impl PlanetUnits {
    pub fn get(&self, kind: Kind) -> &Unit {
        &self.units[KINDS.iter().position(|k| *k == kind).unwrap()]
    }
}

#[derive(Default, serde::Serialize, serde::Deserialize)]
pub struct Units {
    pub planets: Vec<PlanetUnits>,
    /// WorldObjectsHandler._stateConstructedObjectsIndex: changes when buildings come or go.
    /// The game's is a float, which stops counting at 2^24 changes (the units would then never
    /// be recounted); a u64 does not.
    pub constructed_index: u64,
    next_compute: GameTime,
    next_define: GameTime,
    /// The terraformation totals _unitSysTerraformation was last made from (bits).
    sys_from: Option<Vec<u64>>,
}

impl Units {
    pub fn planet(&self, hash: i32) -> Option<&PlanetUnits> {
        self.planets.iter().find(|p| p.hash == hash)
    }
    pub fn planet_mut(&mut self, hash: i32) -> Option<&mut PlanetUnits> {
        self.planets.iter_mut().find(|p| p.hash == hash)
    }
    /// ForceResetAllValues for a planet (current when 0).
    pub fn force_reset(&mut self, hash: i32) {
        if let Some(p) = self.planet_mut(hash) {
            p.units.iter_mut().for_each(|u| u.force_reset = true);
        }
    }
}

fn level(state: &Value, key: &str) -> f32 {
    state[key].as_f64().unwrap_or(0.0) as f32
}

/// CreateUnits for one planet, from its saved state (or zero for a newly unlocked planet).
pub fn create(game: &mut Game, planet_id: &str, state: Option<&Value>) {
    let hash = stable_hash(planet_id);
    if game.units.planet(hash).is_some() {
        return;
    }
    let needs_purification = game.data.asset(&format!("PlanetData:PlanetData-{planet_id}")).and_then(|p| p["needPurification"].as_bool()).unwrap_or(false);
    let s = state.cloned().unwrap_or(json!({}));
    let (o, h, p, pl, i, a) = (level(&s, "unitOxygenLevel"), level(&s, "unitHeatLevel"), level(&s, "unitPressureLevel"), level(&s, "unitPlantsLevel"), level(&s, "unitInsectsLevel"), level(&s, "unitAnimalsLevel"));
    let purification = if needs_purification { if state.is_some() { level(&s, "unitPurificationLevel") } else { 0.0 } } else { -1.0 };
    // GetTerraformationStartingValue sums the levels as doubles.
    let terra = [o, h, p, pl, i, a].iter().map(|v| *v as f64).sum::<f64>() + (level(&s, "unitPurificationLevel") as f64).max(0.0);
    let totals = [o, h, p, 0.0, if state.is_some() { terra as f32 } else { 0.0 }, pl + i + a, pl, i, a, purification];
    let units: Vec<Unit> = KINDS.iter().zip(totals).map(|(k, t)| Unit::new(*k, t)).collect();
    let mut o = crate::net::NetObj::from_layout(&game.net_data.prefabs["WorldUnitsPlanet"].layout(game.tick_number()));
    // WorldUnitsHandler.CreateUnits spawns it below the handler.
    o.parent = game.net.find_class("WorldUnitsHandler").map(|(object, _)| object);
    o.flags |= crate::protocol::HAS_PARENT;
    o.init("WorldUnitsPlanet", "_planetIdHash", enc::int(hash));
    for u in &units {
        o.init("WorldUnitsPlanet", u.kind.var(), u.bytes());
    }
    let object = game.net.spawn(o);
    game.units.planets.push(PlanetUnits { hash, id: planet_id.to_string(), object, units });
}

pub fn init(game: &mut Game) {
    // The steps begin where the clock stands (a game that starts late, as the time-travel tests').
    game.units.next_define = game.time;
    game.units.next_compute = game.time;
    for planet in game.save.planets.clone() {
        if let Some(id) = planet["planetId"].as_str() {
            create(game, id, Some(&planet));
        }
    }
}

/// SetIncreaseAndDecreaseForWorldObjects: the rates from every building on the planet.
fn from_world_objects(game: &Game, planet: i32, unit: &mut Unit) -> bool {
    if game.units.constructed_index == unit.previous_index && !unit.force_reset {
        return false;
    }
    unit.previous_index = game.units.constructed_index;
    let (before_inc, before_dec) = (unit.increase, unit.decrease);
    unit.increase = 0.0;
    unit.decrease = 0.0;
    let ty = unit.kind.unit_type();
    let index = crate::model::unit_index(ty);
    let mut multiplier = 0.0;
    let pace = if unit.kind == Kind::Energy { 1.0 } else { game.rules.speed.terraformation };
    for id in &game.constructed {
        let Some(w) = game.wos.get(id) else { continue };
        if w.planet_hash() != planet {
            continue;
        }
        let Some(index) = index else { continue };
        let generation = w.unit_generation(index) * pace;
        if generation > 0.0 {
            unit.increase += generation;
        } else if generation < 0.0 {
            unit.decrease += generation;
        }
        let g = &game.data.groups[w.group];
        if g.constructible && g.str("worlUnitMultiplied") == ty {
            multiplier += w.unit_multiplier[index];
        }
    }
    unit.increase *= if multiplier == 0.0 { 1.0 } else { multiplier };
    unit.force_reset = false;
    if (before_inc - unit.increase).abs() > f32::EPSILON || (before_dec - unit.decrease).abs() > f32::EPSILON {
        unit.dirty = true;
    }
    true
}

/// DefineFinalValuePerSec for every unit of a planet, in the game's order.
fn define(game: &mut Game, index: usize) {
    let hash = game.units.planets[index].hash;
    let free_craft = game.save.settings["freeCraft"].as_bool().unwrap_or(false);
    for k in 0..KINDS.len() {
        let mut unit = game.units.planets[index].units[k].clone();
        let p = &game.units.planets[index];
        let energy_up = p.get(Kind::Energy).is_increasing();
        match unit.kind {
            Kind::Oxygen | Kind::Heat | Kind::Pressure => {
                if !energy_up {
                    unit.reset_per_sec();
                    unit.base_define();
                } else if from_world_objects(game, hash, &mut unit) {
                    unit.base_define();
                }
            }
            Kind::Purification => {
                if unit.total > -1.0 {
                    if !energy_up {
                        unit.reset_per_sec();
                        unit.base_define();
                    } else if from_world_objects(game, hash, &mut unit) {
                        unit.base_define();
                    }
                }
            }
            Kind::Energy => {
                if free_craft != game.ext.creative_mode {
                    unit.force_reset = true;
                }
                game.ext.creative_mode = free_craft;
                if from_world_objects(game, hash, &mut unit) {
                    unit.base_define();
                }
            }
            Kind::Plants | Kind::Insects | Kind::Animals => {
                if from_world_objects(game, hash, &mut unit) {
                    unit.base_define();
                }
            }
            Kind::Terraformation => {
                let (o, h, pr, b, pu) = (p.get(Kind::Oxygen), p.get(Kind::Heat), p.get(Kind::Pressure), p.get(Kind::Biomass), p.get(Kind::Purification));
                unit.increase = o.increase + h.increase + pr.increase + b.increase;
                unit.decrease = o.decrease + h.decrease + pr.decrease + b.decrease;
                if pu.total > 0.0 {
                    unit.increase += pu.increase;
                    unit.decrease += pu.decrease;
                }
                unit.base_define();
            }
            Kind::Biomass => {
                let (pl, i, a) = (p.get(Kind::Plants), p.get(Kind::Insects), p.get(Kind::Animals));
                unit.increase = pl.increase + i.increase + a.increase;
                unit.decrease = pl.decrease + i.decrease + a.decrease;
                unit.base_define();
            }
        }
        game.units.planets[index].units[k] = unit;
    }
}

/// DefineFinalValuePerSec of the current planet's energy alone (RequireEnergyHandler does this
/// before deciding which machines run).
pub fn define_energy(game: &mut Game) {
    let hash = game.planet_hash();
    let Some(index) = game.units.planets.iter().position(|p| p.hash == hash) else { return };
    let k = KINDS.iter().position(|k| *k == Kind::Energy).unwrap();
    let mut unit = game.units.planets[index].units[k].clone();
    let free_craft = game.save.settings["freeCraft"].as_bool().unwrap_or(false);
    if free_craft != game.ext.creative_mode {
        unit.force_reset = true;
    }
    game.ext.creative_mode = free_craft;
    if from_world_objects(game, hash, &mut unit) {
        unit.base_define();
    }
    game.units.planets[index].units[k] = unit;
}

/// GameSettingsHandler.GetComputedTerraformationMultiplayerFactor.
fn multiplayer_factor(game: &Game, kind: Kind) -> f32 {
    let players = game.clients.values().filter(|c| c.ready.is_some()).count();
    if players == 0 || kind == Kind::Energy {
        return 1.0;
    }
    let modifier = game.save.settings["modifierMultiplayerTerraformationFactor"].as_f64().unwrap_or(0.0) as f32;
    1.0 / (1.0 + (players as f32 - 1.0) * modifier)
}

/// The passes of define that change anything: Oxygen, Heat and Pressure read the energy of the
/// previous pass, Terraformation reads Biomass and Purification of it, and everything else settles
/// in the first, so a third pass is the second. A jump of the clock (days with the bank stopped)
/// runs these and not a pass per second of the jump.
const DEFINE_PASSES: u64 = 3;

pub fn tick(game: &mut Game) {
    let now = game.time;
    let defines = game.units.next_define.take_due(now, 1.0);
    for _ in 0..defines.min(DEFINE_PASSES) {
        for i in 0..game.units.planets.len() {
            define(game, i);
        }
    }
    let computes = game.units.next_compute.take_due(now, 0.1);
    if computes > 0 {
        // The same for all steps of the tick: nothing changes between them.
        let factors = KINDS.map(|k| multiplayer_factor(game, k));
        for p in game.units.planets.iter_mut() {
            for (u, factor) in p.units.iter_mut().zip(&factors) {
                u.compute(computes, 0.1, *factor, now);
            }
        }
    }
    for i in 0..game.units.planets.len() {
        let object = game.units.planets[i].object;
        for k in 0..KINDS.len() {
            let u = &mut game.units.planets[i].units[k];
            if u.dirty {
                u.dirty = false;
                let (var, bytes) = (u.kind.var(), u.bytes());
                game.net.set(object, "WorldUnitsPlanet", var, bytes);
            }
        }
    }
    // A function of the planets' terraformation totals: made again only when one changed.
    let totals = || game.units.planets.iter().map(|p| p.get(Kind::Terraformation).total.to_bits());
    if !game.units.sys_from.as_ref().is_some_and(|from| totals().eq(from.iter().copied())) {
        game.units.sys_from = Some(totals().collect());
        let sys = system_terraformation(game);
        game.set_on("WorldUnitsHandler", "_unitSysTerraformation", sys);
    }
}

/// WorldUnitSystemTerraformation as the game writes it: GetValue multiplies the planets'
/// terraformation together but never stores it, so only the label index (SetCurrentLabelIndex,
/// which marks it dirty when it changes) travels; the value and rates stay 0.
pub fn system_terraformation(game: &Game) -> Vec<u8> {
    let mut total = 0.0f64;
    for p in &game.units.planets {
        let v = p.get(Kind::Terraformation).total;
        total = if total < f64::EPSILON { v } else { total * v.max(1.0) };
    }
    let label = if total > 0.0 { (total.log(1000.0).floor() as i32).clamp(0, 36) } else { 0 };
    [0f32.to_le_bytes(), 0f32.to_le_bytes(), 0f32.to_le_bytes(), 0f32.to_le_bytes(), label.to_le_bytes()].concat()
}

/// A unit's value on a planet (0 = current planet).
pub fn value(game: &Game, kind: Kind, planet: i32) -> f64 {
    let planet = if planet == 0 { game.planet_hash() } else { planet };
    game.units.planet(planet).map(|p| p.get(kind).total).unwrap_or(0.0)
}

/// SetAndGetPlanetStateValues: one JsonablePlanetState per unlocked planet.
pub fn planet_states(game: &Game) -> Vec<Value> {
    game.units
        .planets
        .iter()
        .map(|p| {
            let v = |k: Kind| crate::model::JsonFloat(p.get(k).total as f32);
            json!({
                "planetId": p.id,
                "unitOxygenLevel": v(Kind::Oxygen),
                "unitHeatLevel": v(Kind::Heat),
                "unitPressureLevel": v(Kind::Pressure),
                "unitPlantsLevel": v(Kind::Plants),
                "unitInsectsLevel": v(Kind::Insects),
                "unitAnimalsLevel": v(Kind::Animals),
                "unitPurificationLevel": v(Kind::Purification),
            })
        })
        .collect()
}

/// WorldObjectsHandler.ChangeStateIndex: buildings changed, rates need recomputing.
pub fn constructed_changed(game: &mut Game) {
    game.units.constructed_index += 1;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::*;

    fn unit(g: &Game, kind: Kind) -> Unit {
        g.units.planet(g.planet_hash()).unwrap().get(kind).clone()
    }

    /// Every second the rates come from the planet's powered buildings (scaled by the world's
    /// pace); every tenth the totals grow, slower for each extra player as the world's
    /// multiplayer factor says. The units' variables follow, the save keeps the levels.
    #[test]
    fn units_grow_from_powered_buildings_and_slow_down_for_more_players() {
        let mut save = save();
        save.settings["modifierMultiplayerTerraformationFactor"] = json!(0.5);
        let Some(mut g) = crate::game::test_game(save) else { return };
        join(&mut g, 1, "Ann");
        place(&mut g, "Heater1", [570.0, 3.0, 500.0]);
        place(&mut g, "EnergyGenerator1", [575.0, 3.0, 500.0]);
        let heater = f64::from(g.data.groups[g.data.group("Heater1").unwrap()].unit("Heat"));
        let run = |g: &mut Game, seconds: f32| {
            let end = g.time + seconds;
            while g.time < end {
                g.time += 0.05;
                tick(g);
            }
        };
        run(&mut g, 1.2);
        assert!((f64::from(unit(&g, Kind::Heat).per_sec) - heater).abs() < 1e-6, "{} per s", unit(&g, Kind::Heat).per_sec);
        assert!(unit(&g, Kind::Energy).is_increasing());
        let before = unit(&g, Kind::Heat).total;
        run(&mut g, 10.0);
        let alone = unit(&g, Kind::Heat).total - before;
        assert!((alone - 10.0 * heater).abs() < 0.2 * heater, "{alone}");
        let t = unit(&g, Kind::Terraformation);
        assert!(t.total >= unit(&g, Kind::Heat).total && f64::from(t.per_sec) >= heater);
        // Two players: 1 / (1 + 0.5) as fast. Energy is not scaled.
        join(&mut g, 2, "Bob");
        let before = unit(&g, Kind::Heat).total;
        run(&mut g, 10.0);
        let together = unit(&g, Kind::Heat).total - before;
        assert!((together / alone - 1.0 / 1.5).abs() < 0.05, "{together} vs {alone}");
        assert_eq!(multiplayer_factor(&g, Kind::Energy), 1.0);
        // The planet's variable and the save.
        let object = g.units.planet(g.planet_hash()).unwrap().object;
        assert_eq!(var(&g, object, "WorldUnitsPlanet", "_unitHeat"), unit(&g, Kind::Heat).bytes());
        let level = g.snapshot().planets.into_iter().find(|p| p["planetId"] == "Prime").unwrap()["unitHeatLevel"].as_f64().unwrap();
        assert!((f64::from(level as f32) - unit(&g, Kind::Heat).total).abs() < 1e-3);
        let (handler, _) = g.net.find_class("WorldUnitsHandler").unwrap();
        assert_eq!(var(&g, handler, "WorldUnitsHandler", "_unitSysTerraformation"), system_terraformation(&g));
        // As the real host sends it: no value, only the label of the product.
        assert_eq!(system_terraformation(&g)[..16], [0; 16]);
    }

    /// Unpowered, oxygen, heat and pressure stop; a planet's levels load from the save, with
    /// terraformation their sum.
    #[test]
    fn unpowered_planets_stand_still_and_levels_load_from_the_save() {
        let mut save = save();
        save.planets = vec![json!({"planetId": "Prime", "unitOxygenLevel": 100.0, "unitHeatLevel": 20.0, "unitPressureLevel": 3.0, "unitPlantsLevel": 7.0})];
        let Some(mut g) = crate::game::test_game(save) else { return };
        assert_eq!((unit(&g, Kind::Oxygen).total, unit(&g, Kind::Biomass).total, unit(&g, Kind::Terraformation).total), (100.0, 7.0, 130.0));
        place(&mut g, "Heater1", [570.0, 3.0, 500.0]);
        for _ in 0..40 {
            g.time += 0.05;
            tick(&mut g);
        }
        assert_eq!((unit(&g, Kind::Heat).per_sec, unit(&g, Kind::Heat).total), (0.0, 20.0));
        assert_eq!(unit(&g, Kind::Energy).per_sec, unit(&g, Kind::Energy).decrease, "the heater only uses energy");
    }

    /// A world with Ann, a heater and a generator, at game time `start`, its first define done.
    fn heated_at(start: f64) -> Option<(Game, f32)> {
        let mut g = crate::game::test_game(save())?;
        join(&mut g, 1, "Ann");
        place(&mut g, "Heater1", [570.0, 3.0, 500.0]);
        place(&mut g, "EnergyGenerator1", [575.0, 3.0, 500.0]);
        let heater = g.data.groups[g.data.group("Heater1").unwrap()].unit("Heat");
        g.time = GameTime::at(start);
        g.units.next_define = g.time;
        g.units.next_compute = g.time;
        tick(&mut g);
        Some((g, heater))
    }

    fn run(g: &mut Game, seconds: f64, step: f64) {
        let end = g.time.secs() + seconds;
        while g.time.secs() < end {
            g.time = GameTime::at(g.time.secs() + step);
            tick(g);
        }
    }

    /// The totals grow at the buildings' rate however long the server has run: in the game's
    /// float the 0.1 s step was 0.125 s from 6 days of uptime (the world at 80% speed) and nothing
    /// from 2^21 s (24 days), where the catch-up loop never ended and the bank hung; and a total
    /// of millions stops growing at all (at 17.5 a second: 36 days).
    #[test]
    fn units_grow_at_their_rate_whatever_the_uptime() {
        for days in [0.0, 0.04, 1.0, 3.0, 7.0, 25.0, 60.0, 200.0, 400.0] {
            let Some((mut g, heater)) = heated_at(days * 86_400.0) else { return };
            run(&mut g, 2.0, 0.07);
            let before = unit(&g, Kind::Heat).total;
            run(&mut g, 60.0, 0.07);
            let grown = unit(&g, Kind::Heat).total - before;
            // The tenth of a second in progress at either end is all the slack there is.
            assert!((grown - 60.0 * f64::from(heater)).abs() <= 0.1 * f64::from(heater) + 1e-3, "{days} days: {grown} in 60 s at {heater} per s");
        }
    }

    /// A planet whose total is in the millions keeps growing by its steps (the float's step there
    /// is bigger than twice one), however long the server has run or since it was started.
    #[test]
    fn a_total_of_millions_keeps_growing() {
        for start in [10.0, 7200.0, 30.0 * 86_400.0] {
            let Some((mut g, heater)) = heated_at(start) else { return };
            // The float's step at 40 million is 4: a tenth of a second of the heater's rate is far under half of it.
            let planet = g.planet_hash();
            g.units.planet_mut(planet).unwrap().units.iter_mut().filter(|u| u.kind == Kind::Heat).for_each(|u| u.total = 40_000_000.0);
            run(&mut g, 60.0, 0.07);
            let grown = unit(&g, Kind::Heat).total - 40_000_000.0;
            assert!((grown - 60.0 * f64::from(heater)).abs() <= 0.1 * f64::from(heater) + 1e-6, "starting at {start} s: {grown} in 60 s at {heater} a second");
        }
    }

    /// A jump of the clock (a stopped process, a stall) costs a few passes, not a pass per tenth
    /// of a second of the jump (it was 14 CPU minutes for 3 days), and the totals have grown by the
    /// whole jump.
    #[test]
    fn a_jump_of_the_clock_is_one_catch_up() {
        for start in [100.0, 4000.0, 30.0 * 86_400.0] {
            let Some((mut g, heater)) = heated_at(start) else { return };
            run(&mut g, 2.0, 0.07);
            let before = unit(&g, Kind::Heat).total;
            let jump = if start < 1000.0 { 600.0 } else { 3.0 * 86_400.0 };
            let began = std::time::Instant::now();
            g.time = GameTime::at(g.time.secs() + jump);
            tick(&mut g);
            let grown = unit(&g, Kind::Heat).total - before;
            println!("start {start} s: a jump of {jump} s took {:?}", began.elapsed());
            if start >= 1000.0 {
                assert!(began.elapsed() < std::time::Duration::from_secs(1), "{:?} for a jump of {jump} s", began.elapsed());
                assert!((grown - jump * f64::from(heater)).abs() <= 0.1 * f64::from(heater) + 1e-6 * jump * f64::from(heater), "{grown} in {jump} s at {heater}");
            }
        }
    }

    /// The passes `tick` makes for a jump are all that change anything: more of them leave the
    /// units as they are (the rates of every unit, whatever order they read each other in).
    #[test]
    fn three_define_passes_are_all_there_are() {
        let mut states = Vec::new();
        for passes in 1..=6 {
            let Some((mut g, _)) = heated_at(10.0) else { return };
            // A building came and the free-craft setting changed: every kind has something to redo.
            place(&mut g, "Heater1", [580.0, 3.0, 500.0]);
            place(&mut g, "Beehive1", [585.0, 3.0, 500.0]);
            g.save.settings["freeCraft"] = json!(true);
            for _ in 0..passes {
                for i in 0..g.units.planets.len() {
                    define(&mut g, i);
                }
            }
            states.push(crate::bincodec::to_vec(&g.units).unwrap());
        }
        assert!(states[2..].iter().all(|s| *s == states[2]), "more than three passes changed the units");
    }
}
