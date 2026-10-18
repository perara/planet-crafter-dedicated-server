//! Ore veins (MachineGenerationGroupVein): scene objects that extractors mine. A vein with
//! content has a world object (GenerationGroupVein) that counts what was mined, and an
//! extractor's mining ray (MachineGenerator.SetMiningRayGeneration) finds the veins below it.
//! The veins come from the planet's scene dump (scenes/<planet>.json "oreVeins").

use serde_json::Value;

use crate::game::Game;
use crate::geometry::{conj, rotate, sub};

#[derive(Clone)]
pub struct Vein {
    id: i32,
    groups: Vec<usize>,
    identifier: Value,
    count: i32,
    /// The colliders on the vein's object, as boxes in world space: center, half size, rotation.
    boxes: Vec<([f32; 3], [f32; 3], [f32; 4])>,
    /// Shown when the scene loaded (HideDependingOnTerraStage on it or its parents).
    shown_at_load: bool,
    activators: Value,
    /// Its Start ran (on its first activation).
    started: bool,
}

/// The current planet's veins.
pub struct State {
    planet: usize,
    /// None: the planet has no scene dump (a DLC planet), or a test without veins.
    veins: Option<Vec<Vein>>,
    next_check: f32,
}

impl Vein {
    /// activeInHierarchy now: raycasts and Start only see active objects.
    fn active(&self, game: &Game) -> bool {
        self.shown_at_load && !crate::water::hidden_by_positioning(game, &self.activators)
    }
}

fn v3(v: &Value) -> [f32; 3] {
    [0, 1, 2].map(|i| v[i].as_f64().unwrap_or(0.0) as f32)
}

fn q(v: &Value) -> [f32; 4] {
    [0, 1, 2, 3].map(|i| v[i].as_f64().unwrap_or(if i == 3 { 1.0 } else { 0.0 }) as f32)
}

/// The current planet's veins; None on a planet without a scene dump (a DLC planet).
fn load(game: &Game) -> Option<Vec<Vein>> {
    let veins = game.scenes.planets.get(game.planet_id())?.ore_veins.iter().map(|v| {
        let f = &v["fields"];
        // WorldRandomizer.ReplaceOreVein, in Start.
        let groups = f["groups"].as_array().into_iter().flatten().filter_map(|r| game.data.group_of_ref(r.as_str()?)).map(|g| crate::planets::randomized_group(game, g).unwrap_or(g)).collect();
        let boxes = v["colliders"].as_array().into_iter().flatten().filter(|c| c["enabled"].as_bool() != Some(false)).map(|c| {
            let b = &c["box"];
            if b.is_null() {
                // Other collider shapes: their world bounds.
                let s = v3(&c["boundsSize"]);
                (v3(&c["boundsCenter"]), [s[0] / 2.0, s[1] / 2.0, s[2] / 2.0], [0.0, 0.0, 0.0, 1.0])
            } else {
                let (p, r, scale, center, size) = (v3(&b["position"]), q(&b["rotation"]), v3(&b["scale"]), v3(&b["center"]), v3(&b["size"]));
                let c = rotate(r, [center[0] * scale[0], center[1] * scale[1], center[2] * scale[2]]);
                ([p[0] + c[0], p[1] + c[1], p[2] + c[2]], [0, 1, 2].map(|i| size[i] * scale[i].abs() / 2.0), r)
            }
        }).collect();
        let (id, count) = (v["id"].as_i64().unwrap_or(0) as i32, f["oreVeinContentCount"].as_i64().unwrap_or(0) as i32);
        Vein { id, groups, identifier: f["oreVeinIdentifer"].clone(), count, boxes, shown_at_load: crate::water::shown_at_load(game, v), activators: v["activators"].clone(), started: false }
    });
    Some(veins.collect())
}

/// Loads the current planet's veins after a start or a travel; runs each vein's Start when it
/// first shows (checked every second, as terraformation shows some of them later).
pub fn ensure(game: &mut Game) {
    if game.ext.machines.veins.as_ref().map(|v| v.planet) != Some(game.planet) {
        let veins = load(game);
        game.ext.machines.veins = Some(State { planet: game.planet, veins, next_check: 0.0 });
    }
    let s = game.ext.machines.veins.as_ref().unwrap();
    if game.time >= s.next_check {
        game.ext.machines.veins.as_mut().unwrap().next_check = game.time + 1.0;
        start(game);
    }
}

/// MachineGenerationGroupVein.Start on the server: each vein with content gets its world
/// object, its capacity the vein's content count.
fn start(game: &mut Game) {
    let Some(group) = game.data.group("GenerationGroupVein") else { return };
    let Some(veins) = game.ext.machines.veins.as_ref().and_then(|s| s.veins.clone()) else { return };
    let mut due = Vec::new();
    for (i, v) in veins.iter().enumerate() {
        if !v.started && v.active(game) {
            due.push(i);
        }
    }
    let list = game.ext.machines.veins.as_mut().unwrap().veins.as_mut().unwrap();
    for &i in &due {
        list[i].started = true;
    }
    for (id, count) in due.iter().map(|&i| (veins[i].id, veins[i].count)).filter(|(id, count)| *count > 0 && *id != 0) {
        if !game.wos.contains_key(&id) {
            game.create_world_object(group, id, true);
        }
        let w = game.wos.get_mut(&id).unwrap();
        w.count = [w.count[0].min(count), count];
        w.growth = w.count[0] as f32 / w.count[1] as f32 * 100.0;
    }
}

/// Distance along a ray straight down from `origin` to a box, within the ray's 1000 m.
fn hit_down(origin: [f32; 3], (center, half, rotation): &([f32; 3], [f32; 3], [f32; 4])) -> Option<f32> {
    let o = rotate(conj(*rotation), sub(origin, *center));
    let d = rotate(conj(*rotation), [0.0, -1.0, 0.0]);
    // A ray that starts inside a collider does not hit it.
    if (0..3).all(|i| o[i].abs() <= half[i]) {
        return None;
    }
    let (mut near, mut far) = (0.0f32, 1000.0f32);
    for i in 0..3 {
        if d[i].abs() < 1e-6 {
            if o[i].abs() > half[i] {
                return None;
            }
            continue;
        }
        let (a, b) = ((-half[i] - o[i]) / d[i], (half[i] - o[i]) / d[i]);
        near = near.max(a.min(b));
        far = far.min(a.max(b));
        if near > far {
            return None;
        }
    }
    Some(near)
}

/// SetMiningRayGeneration's ray from 25 m above the machine, into the current planet's veins
/// (TryToCastRay casts only active machines): the linked groups become each vein's groups
/// (miningRays times each), and a vein with content becomes the linked world object. Without
/// vein data the save's linked groups stay.
/// ponytail: veins sorted by distance, where PhysX returns them in its own order (it matters
/// only under two veins at once).
pub fn cast(game: &mut Game, wo: i32, f: &Value) {
    let rays = f["miningRays"].as_i64().unwrap_or(0);
    if rays <= 0 {
        return;
    }
    ensure(game);
    let Some(veins) = game.ext.machines.veins.as_ref().and_then(|s| s.veins.clone()) else { return };
    let Some(p) = game.wos.get(&wo).map(|w| w.position) else { return };
    let origin = [p[0], p[1] + 25.0, p[2]];
    let allowed = f["oreAllowedToMine"].as_array().cloned().unwrap_or_default();
    let mut hits: Vec<(f32, &Vein)> = veins.iter().filter(|v| allowed.contains(&v.identifier) && v.active(game)).filter_map(|v| v.boxes.iter().filter_map(|b| hit_down(origin, b)).reduce(f32::min).map(|d| (d, v))).collect();
    hits.sort_by(|a, b| a.0.total_cmp(&b.0));
    if f["onlyUseFirstVeinDetected"].as_bool() == Some(true) {
        hits.truncate(1);
    }
    let when_empty = f["generateWhenVeinIsEmpty"].as_bool() == Some(true);
    let mut linked = Vec::new();
    let mut linked_wo = None;
    for (_, v) in &hits {
        if !when_empty {
            for g in &v.groups {
                linked.extend(std::iter::repeat_n(*g, rays as usize));
            }
        }
        if v.count > 0 {
            linked_wo = Some(v.id);
        }
    }
    let w = game.wos.get_mut(&wo).unwrap();
    if f["setGroupsDataViaLinkedGroup"].as_bool() != Some(true) {
        w.linked_groups = Some(linked);
    }
    if let Some(id) = linked_wo {
        w.linked_wo = id;
    }
}

/// For tests of the rays' bookkeeping: the planet without veins.
#[cfg(test)]
pub fn without_veins(game: &mut Game) {
    game.ext.machines.veins = Some(State { planet: game.planet, veins: None, next_check: f32::MAX });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ray_hits_a_box_below_from_above_only() {
        let b = ([0.0, -2.0, 0.0], [3.0, 1.0, 3.0], [0.0, 0.0, 0.0, 1.0]);
        assert_eq!(hit_down([1.0, 25.0, 1.0], &b), Some(26.0));
        assert_eq!(hit_down([4.0, 25.0, 1.0], &b), None, "beside the box");
        assert_eq!(hit_down([0.0, -2.0, 0.0], &b), None, "starting inside");
        // Turned 45 degrees about Y: the corner reaches x = 3 * sqrt(2).
        let turned = ([0.0, -2.0, 0.0], [3.0, 1.0, 3.0], [0.0, 0.38268343, 0.0, 0.9238795]);
        assert!(hit_down([4.0, 25.0, 0.0], &turned).is_some());
    }

    #[test]
    fn extractors_mine_the_vein_below_them() {
        let Some(mut game) = crate::machines::tests::game() else { return };
        let iridium = game.data.group("Iridium").unwrap();
        // Prime's tier-1 Iridium vein at (2083, -281, -76), 200 m across.
        let over = crate::machines::tests::build(&mut game, "OreExtractor1", [2083.0, 30.0, -76.0]);
        assert_eq!(game.wos[&over].linked_groups, Some(vec![iridium, iridium]));
        let beside = crate::machines::tests::build(&mut game, "OreExtractor1", [2083.0, 30.0, 400.0]);
        assert_eq!(game.wos[&beside].linked_groups, Some(vec![]));
    }
}
