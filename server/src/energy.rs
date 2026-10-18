//! SpaceCraft.RequireEnergyHandler, server side: whether each planet produces more energy than
//! it uses decides whether its machines run; checked every 2 s and on every energy change.

use crate::game::Game;
use crate::units::Kind;

const CHECK_EVERY: f32 = 2.0;

#[derive(Default, serde::Serialize, serde::Deserialize)]
pub struct Energy {
    next_check: f32,
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
    let current = game.planet_hash();
    let mut water = None;
    for wo in machines {
        let Some((planet, position, group)) = game.wos.get(&wo).map(|w| (w.planet_hash(), w.position, w.group)) else { continue };
        let enough = has_enough(game, planet);
        // RequireEnergy.CheckEnergyStatus: a machine underwater (unless it works there) gets
        // no energy, on the planet the session is on.
        let mut underwater = false;
        if planet == current {
            let works = game.data.prefab_components(&game.data.groups[group].id, "RequireEnergy").first().and_then(|c| c["worksUnderWater"].as_bool()).unwrap_or(false);
            if !works {
                let boxes = water.get_or_insert_with(|| crate::water::underwater_boxes(game));
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
        crate::machines::set_has_energy(game, wo, enough && !underwater);
    }
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
