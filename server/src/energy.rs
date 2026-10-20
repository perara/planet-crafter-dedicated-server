//! SpaceCraft.RequireEnergyHandler, server side: whether each planet produces more energy than
//! it uses decides whether its machines run; checked every 2 s and on every energy change.

use crate::clock::GameTime;
use crate::game::Game;
use crate::units::Kind;

const CHECK_EVERY: f32 = 2.0;

#[derive(Default, serde::Serialize, serde::Deserialize)]
pub struct Energy {
    next_check: GameTime,
}

/// Is a planet's energy balance positive (Energy unit increasing)?
pub fn has_enough(game: &Game, planet: i32) -> bool {
    let planet = if planet == 0 { game.planet_hash() } else { planet };
    game.units.planet(planet).is_some_and(|p| {
        let e = p.get(Kind::Energy);
        e.increase > e.decrease.abs()
    })
}

/// UpdateAllEnergyRequester: tell every machine whether it has energy.
pub fn update_all_energy_requesters(game: &mut Game) {
    crate::units::define_energy(game);
    let machines: Vec<i32> = game.ext.machines.with_energy();
    let mut water = Underwater::default();
    for wo in machines {
        let Some(planet) = game.wos.get(&wo).map(|w| w.planet_hash()) else { continue };
        let enough = has_enough(game, planet);
        let runs = check_energy_status(game, wo, enough, &mut water);
        crate::machines::set_has_energy(game, wo, runs);
    }
}

/// What the machines of one update share: the water boxes, and per group whether its machines
/// work underwater (a lookup in the prefab's JSON, so once per group, not per machine).
#[derive(Default)]
pub struct Underwater {
    boxes: Option<crate::water::Boxes>,
    works: crate::idmap::FxHashMap<usize, bool>,
}

/// RequireEnergy.CheckEnergyStatus for one machine: whether it runs. A machine underwater
/// (unless it works there) gets no energy, on the planet the session is on. `water` caches
/// the water boxes across the machines of one update.
pub fn check_energy_status(game: &mut Game, wo: i32, enough: bool, water: &mut Underwater) -> bool {
    let Some((planet, position, group)) = game.wos.get(&wo).map(|w| (w.planet_hash(), w.position, w.group)) else { return enough };
    let current = game.planet_hash();
    let mut underwater = false;
    if planet == current {
        let works = *water.works.entry(group).or_insert_with(|| game.data.prefab_components(&game.data.groups[group].id, "RequireEnergy").first().and_then(|c| c["worksUnderWater"].as_bool()).unwrap_or(false));
        if !works {
            let boxes = water.boxes.get_or_insert_with(|| crate::water::underwater_boxes(game));
            underwater = crate::water::in_boxes(boxes, [position[0], position[1] + 2.0, position[2]]);
        }
        if underwater {
            if game.wos.get(&wo).is_some_and(|w| w.energy != 0.0) {
                game.units.force_reset(current);
            }
            if let Some(w) = game.wos.get_mut(&wo) {
                w.energy = 0.0;
            }
        }
    }
    enough && !underwater
}

pub fn tick(game: &mut Game) {
    if game.time >= game.ext.energy.next_check {
        game.ext.energy.next_check = game.time + CHECK_EVERY;
        update_all_energy_requesters(game);
    }
}

/// GetEnergyGroupDataServerRpc: per group, the current planet's total generation and count.
pub fn energy_group_data(game: &Game) -> Vec<serde_json::Value> {
    let planet = game.planet_hash();
    let energy = crate::model::unit_index("Energy").unwrap();
    let mut out: Vec<(i32, f32, i32)> = Vec::new();
    for id in &game.constructed {
        let Some(w) = game.wos.get(id).filter(|w| w.planet_hash() == planet) else { continue };
        let generation = w.unit_generation(energy);
        if generation.abs() > 0.0 {
            let hash = game.data.groups[w.group].hash;
            match out.iter_mut().find(|e| e.0 == hash) {
                Some(e) => {
                    e.1 += generation;
                    e.2 += 1;
                }
                None => out.push((hash, generation, 1)),
            }
        }
    }
    out.into_iter().map(|(h, g, n)| serde_json::json!([h, g, n])).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::*;

    /// A machine runs while the planet makes more energy than it uses: checked when buildings
    /// with energy come and go, and every 2 s.
    #[test]
    fn machines_run_while_the_planet_has_energy() {
        let Some((mut g, _)) = world() else { return };
        let (heater, _) = place(&mut g, "Heater1", [570.0, 3.0, 500.0]);
        let powered = |g: &Game| g.ext.machines.energy.get(&heater).copied();
        assert!(!has_enough(&g, 0));
        assert_eq!(powered(&g), Some(false));
        let (generator, _) = place(&mut g, "EnergyGenerator1", [575.0, 3.0, 500.0]);
        assert!(has_enough(&g, 0));
        assert_eq!(powered(&g), Some(true));
        g.destroy_world_object(generator, true);
        assert_eq!(powered(&g), Some(false), "the generator gone");
        // The periodic check: a generator that appeared without an energy change is seen in 2 s.
        g.ext.machines.energy.insert(heater, true);
        g.time += 0.1;
        tick(&mut g);
        assert_eq!(powered(&g), Some(false));
        g.ext.machines.energy.insert(heater, true);
        g.time += 1.0;
        tick(&mut g);
        assert_eq!(powered(&g), Some(true), "not due yet");
        g.time += 1.5;
        tick(&mut g);
        assert_eq!(powered(&g), Some(false));
        assert!(!has_enough(&g, 12345), "a planet without units has none");
    }
}
