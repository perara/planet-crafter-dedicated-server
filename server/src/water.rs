//! SpaceCraft.WaterHandler.IsUnderWater: a position is underwater when it lies inside one of
//! the planet's water boxes, once terraformation has started raising the water. Water boxes
//! move with terraformation (WorldUnitPositioning); the dump holds each box at dump time and
//! the positioning that moves it. The same boxes stand in for the Water layer's colliders in
//! raycasts (surface_below).

use std::collections::HashMap;

use serde_json::Value;

use crate::game::Game;
use crate::geometry::{conj, mul, q, rotate, sub, v3};

#[derive(serde::Serialize, serde::Deserialize)]
struct Volume {
    /// Box in the moving root's local space: center, half size, rotation, scale.
    local_center: [f32; 3],
    half: [f32; 3],
    local_rotation: [f32; 4],
    root_position: [f32; 3],
    root_rotation: [f32; 4],
    #[serde(with = "crate::bincodec::json_value")]
    positioning: Value,
    active: bool,
    #[serde(with = "crate::bincodec::json_value")]
    activators: Value,
    /// WaterVolume.ignoreForEnergyDetection: not water for IsUnderWater, still on the Water layer.
    ignored: bool,
    /// Moved by a SceneInterpolationFromStoryEvent: its world object, starting offset, eased.
    story: Option<(i32, [f32; 3], bool)>,
}

#[derive(Default, serde::Serialize, serde::Deserialize)]
pub struct Water {
    /// Planet id -> its water volumes (loaded from scenes/<planet>.json).
    planets: HashMap<String, Vec<Volume>>,
}

/// A planet's water volumes, from its scene dump (none on a DLC planet, which has no dump).
fn load(game: &Game, planet: &str) -> Vec<Volume> {
    let mut out = Vec::new();
    for v in game.scenes.planets.get(planet).into_iter().flat_map(|s| &s.water_volumes) {
        // ponytail: volumes without a BoxCollider (Aqualis' sea) are left out; their water does not move.
        if v["box"].is_null() {
            continue;
        }
        let (root_p, root_r) = (v3(&v["root"]["position"]), q(&v["root"]["rotation"]));
        let b = &v["box"];
        let (box_p, box_r, scale) = (v3(&b["position"]), q(&b["rotation"]), v3(&b["scale"]));
        let center = v3(&b["center"]);
        let size = v3(&b["size"]);
        // Box center in world space at dump time, then in the root's frame.
        let world_center = {
            let c = rotate(box_r, [center[0] * scale[0], center[1] * scale[1], center[2] * scale[2]]);
            [box_p[0] + c[0], box_p[1] + c[1], box_p[2] + c[2]]
        };
        out.push(Volume {
            local_center: rotate(conj(root_r), sub(world_center, root_p)),
            half: [size[0] * scale[0].abs() / 2.0, size[1] * scale[1].abs() / 2.0, size[2] * scale[2].abs() / 2.0],
            local_rotation: mul(conj(root_r), box_r),
            root_position: root_p,
            root_rotation: root_r,
            positioning: v["positioning"].clone(),
            active: shown_at_load(game, v),
            activators: v["activators"].clone(),
            ignored: v["ignoreForEnergyDetection"].as_bool() == Some(true),
            story: v["storyInterpolation"].as_object().map(|s| (s["worldObject"].as_i64().unwrap_or(0) as i32, v3(&s["offset"]), s["ease"].as_bool() == Some(true))),
        });
    }
    out
}

/// A scene object's state when its scene loads: active in the scene, and not hidden by its
/// HideDependingOnTerraStage components (on it and its parents, "activators" in the dump),
/// which decide once and can only hide (an inactive object runs none of its own).
pub fn shown_at_load(game: &Game, object: &Value) -> bool {
    let active = object["active"].as_bool().unwrap_or(true);
    let activators = object["activators"].as_array().map(Vec::as_slice).unwrap_or_default();
    active && !activators.iter().filter(|a| a["type"] == "HideDependingOnTerraStage").any(|a| {
        let f = &a["fields"];
        let (unit, start) = (stage_unit(game, &f["terraformStage"]), stage_value(game, &f["terraformStage"]).unwrap_or(0.0));
        (f["hideIfGreater"].as_bool() == Some(true) && unit > start) || (f["hideIfLesser"].as_bool() == Some(true) && unit < start)
    })
}

/// Whether a WorldUnitPositioning among the activators hides the object now (hideBefore /
/// hideAfter, UpdateEvolutionPositioning).
pub fn hidden_by_positioning(game: &Game, activators: &Value) -> bool {
    activators.as_array().into_iter().flatten().filter(|a| a["type"] == "WorldUnitPositioning").any(|a| {
        let p = &a["fields"];
        let before = stage_value(game, &p["hideBefore"]).is_some_and(|start| (stage_unit(game, &p["hideBefore"]) as f32 as f64) < start);
        let after = stage_value(game, &p["hideAfter"]).is_some_and(|end| (stage_unit(game, &p["hideAfter"]) as f32 as f64) > end);
        before || after
    })
}

fn stage_value(game: &Game, reference: &Value) -> Option<f64> {
    game.data.asset(reference.as_str()?)?["startValue"].as_f64()
}

fn stage_unit(game: &Game, reference: &Value) -> f64 {
    let unit = reference.as_str().and_then(|r| game.data.asset(r)).and_then(|s| s["unitType"].as_str()).and_then(crate::units::Kind::from_name);
    unit.map(|k| crate::units::value(game, k, 0)).unwrap_or(0.0)
}

/// The root's position now (WorldUnitPositioning.UpdateEvolutionPositioning), or None when the
/// volume is hidden by its stage limits.
fn root_now(game: &Game, v: &Volume) -> Option<[f32; 3]> {
    if !v.active || hidden_by_positioning(game, &v.activators) {
        return None;
    }
    let p = &v.positioning;
    if p.is_null() {
        return Some(v.root_position);
    }
    if let Some(start) = stage_value(game, &p["hideBefore"]) {
        if (stage_unit(game, &p["hideBefore"]) as f32 as f64) < start {
            return None;
        }
    }
    if let Some(end) = stage_value(game, &p["hideAfter"]) {
        if (stage_unit(game, &p["hideAfter"]) as f32 as f64) > end {
            return None;
        }
    }
    let (Some(start), Some(end)) = (stage_value(game, &p["startTerraformStage"]), stage_value(game, &p["endTerraformStage"])) else { return Some(v.root_position) };
    if p["changePosition"].as_bool() != Some(true) {
        return Some(v.root_position);
    }
    let value = stage_unit(game, &p["startTerraformStage"]) as f32;
    let t = if (end as f32 - start as f32).abs() > f32::EPSILON { ((value - start as f32) / (end as f32 - start as f32)).clamp(0.0, 1.0) } else { 0.0 };
    let (a, b) = (v3(&p["startPosition"]), v3(&p["endPosition"]));
    Some([a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t, a[2] + (b[2] - a[2]) * t])
}

/// The current planet's water volumes (loaded on first use).
fn volumes(game: &mut Game) -> &[Volume] {
    let planet = game.planet_id();
    if !game.ext.water.planets.contains_key(planet) {
        let volumes = load(game, planet);
        game.ext.water.planets.insert(planet.to_string(), volumes);
    }
    &game.ext.water.planets[game.planet_id()]
}

/// SceneInterpolationFromStoryEvent.InOutQuad.
fn in_out_quad(t: f32) -> f32 {
    if (t as f64) < 0.5 {
        (t * 2.0) * (t * 2.0) / 2.0
    } else {
        1.0 - ((1.0 - t) * 2.0) * ((1.0 - t) * 2.0) / 2.0
    }
}

/// How far a story interpolation still holds the volume from its scene position: the
/// starting offset at progress 0 (also before the story event made its world object), none
/// at 1 (UpdateInterpolation: lerp(start, destination, progress)).
fn story_shift(game: &Game, v: &Volume) -> [f32; 3] {
    let Some((wo, offset, ease)) = v.story else { return [0.0; 3] };
    let t = game.wos.get(&wo).map_or(0.0, |w| (w.growth / 100.0).clamp(0.0, 1.0));
    let t = if ease { in_out_quad(t) } else { t };
    offset.map(|o| o * (1.0 - t))
}

/// A volume's box now: world center and rotation; None while the volume is hidden.
fn box_now(game: &Game, v: &Volume) -> Option<([f32; 3], [f32; 4])> {
    let root = root_now(game, v)?;
    let shift = story_shift(game, v);
    let root = [root[0] + shift[0], root[1] + shift[1], root[2] + shift[2]];
    let c = rotate(v.root_rotation, v.local_center);
    Some(([root[0] + c[0], root[1] + c[1], root[2] + c[2]], mul(v.root_rotation, v.local_rotation)))
}

/// The boxes WaterHandler.IsUnderWater tests on the current planet now: world center,
/// rotation and half size of each shown volume; empty before the water starts to rise. They
/// only move with terraformation, so one set serves every check of an energy update.
pub fn underwater_boxes(game: &mut Game) -> Vec<([f32; 3], [f32; 4], [f32; 3])> {
    volumes(game);
    let game = &*game;
    let volumes = game.ext.water.planets[game.planet_id()].iter().filter(|v| !v.ignored);
    // _hasWaterStartedToRise: past the earliest stage any volume starts rising at.
    let earliest = volumes.clone().filter_map(|v| stage_value(game, &v.positioning["startTerraformStage"])).fold(f64::MAX, f64::min);
    if crate::units::value(game, crate::units::Kind::Terraformation, 0) <= earliest {
        return Vec::new();
    }
    volumes.filter_map(|v| box_now(game, v).map(|(center, rotation)| (center, rotation, v.half))).collect()
}

/// IsUnderWater against underwater_boxes.
pub fn in_boxes(boxes: &[([f32; 3], [f32; 4], [f32; 3])], position: [f32; 3]) -> bool {
    boxes.iter().any(|(center, rotation, half)| {
        let local = rotate(conj(*rotation), sub(position, *center));
        (0..3).all(|i| local[i].abs() <= half[i])
    })
}

/// Whether the current planet's water is known (its scene dump has water volumes): where it is
/// not, a ray at the Water layer cannot be answered.
pub fn known(game: &mut Game) -> bool {
    !volumes(game).is_empty()
}

/// Physics.Raycast straight down from `origin` against the Water layer, within `distance`: the
/// height of the first water surface hit. A ray that starts inside a box does not hit it.
pub fn surface_below(game: &mut Game, origin: [f32; 3], distance: f32) -> Option<f32> {
    volumes(game);
    let game = &*game;
    let mut nearest: Option<f32> = None;
    for v in &game.ext.water.planets[game.planet_id()] {
        let Some((center, rotation)) = box_now(game, v) else { continue };
        let (o, d) = (rotate(conj(rotation), sub(origin, center)), rotate(conj(rotation), [0.0, -1.0, 0.0]));
        // Slabs: the ray's entry and exit along each box axis.
        let (mut enter, mut exit) = (f32::MIN, f32::MAX);
        for i in 0..3 {
            if d[i].abs() < 1e-6 {
                if o[i].abs() > v.half[i] {
                    enter = f32::MAX;
                }
                continue;
            }
            let (a, b) = ((-v.half[i] - o[i]) / d[i], (v.half[i] - o[i]) / d[i]);
            enter = enter.max(a.min(b));
            exit = exit.min(a.max(b));
        }
        if enter > 0.0 && enter <= exit && enter <= distance && nearest.is_none_or(|n| enter < n) {
            nearest = Some(enter);
        }
    }
    nearest.map(|t| origin[1] - t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn story_interpolated_water_starts_offset_and_ends_at_its_scene_position() {
        let Some(mut g) = crate::game::test_game(crate::game::tests::save()) else { return };
        let v = Volume { local_center: [0.0; 3], half: [1.0; 3], local_rotation: [0.0, 0.0, 0.0, 1.0], root_position: [10.0, 20.0, 30.0], root_rotation: [0.0, 0.0, 0.0, 1.0], positioning: Value::Null, active: true, activators: Value::Null, ignored: false, story: Some((123, [0.0, -400.0, 0.0], true)) };
        // Before the story event made its world object: the starting offset.
        assert_eq!(box_now(&g, &v).unwrap().0, [10.0, -380.0, 30.0]);
        let group = g.data.singletons["SceneInterpolationHandler"]["_interpolationGroup"].as_str().and_then(|r| g.data.group_of_ref(r)).unwrap();
        g.create_world_object(group, 123, true);
        for (growth, y) in [(0.0, -380.0), (25.0, -380.0 + 400.0 * 0.125), (50.0, -180.0), (100.0, 20.0)] {
            g.wos.get_mut(&123).unwrap().growth = growth;
            assert_eq!(box_now(&g, &v).unwrap().0, [10.0, y, 30.0], "progress {growth}");
        }
    }

    /// A machine built underwater has no energy from the start (RequireEnergyHandler.
    /// AddToRequester checks it at once), not only from the next 2 s check: the real host left
    /// a new lamp and generator at the shore out of GetEnergyGroupData right away.
    #[test]
    fn machines_built_underwater_have_no_energy_at_once() {
        let Some((mut g, _)) = crate::testkit::world() else { return };
        let stage = g.data.assets.iter().find(|(k, v)| k.starts_with("TerraformStage:") && v["startValue"].as_f64() == Some(0.0)).map(|(k, _)| k.clone()).unwrap();
        let hash = g.planet_hash();
        let p = g.units.planets.iter_mut().find(|p| p.hash == hash).unwrap();
        let k = crate::units::KINDS.iter().position(|k| *k == crate::units::Kind::Terraformation).unwrap();
        p.units[k].total = 10.0;
        volumes(&mut g);
        let planet = g.planet_id().to_string();
        let positioning = serde_json::json!({"startTerraformStage": stage});
        g.ext.water.planets.get_mut(&planet).unwrap().push(Volume { local_center: [0.0; 3], half: [5.0; 3], local_rotation: [0.0, 0.0, 0.0, 1.0], root_position: [570.0, 3.0, 500.0], root_rotation: [0.0, 0.0, 0.0, 1.0], positioning, active: true, activators: Value::Null, ignored: false, story: None });
        let (generator, _) = crate::testkit::place(&mut g, "EnergyGenerator1", [570.0, 1.0, 500.0]);
        assert_eq!(g.wos[&generator].energy, 0.0);
        assert!(crate::energy::energy_group_data(&g).iter().all(|e| e[0] != serde_json::json!(g.data.groups[g.wos[&generator].group].hash)));
    }

    #[test]
    fn an_inactive_object_stays_hidden_whatever_its_activators() {
        let Some(g) = crate::game::test_game(crate::game::tests::save()) else { return };
        let positioning = serde_json::json!([{"type": "WorldUnitPositioning", "fields": {}}]);
        assert!(!shown_at_load(&g, &serde_json::json!({"active": false, "activators": positioning})));
        assert!(shown_at_load(&g, &serde_json::json!({"active": true, "activators": positioning})));
    }

    #[test]
    fn rotation_round_trip() {
        let r = crate::geometry::from_euler_y(90.0);
        let v = rotate(r, [1.0, 0.0, 0.0]);
        assert!((v[2] + 1.0).abs() < 1e-5 && v[0].abs() < 1e-5, "{v:?}");
        let back = rotate(conj(r), v);
        assert!((back[0] - 1.0).abs() < 1e-5);
    }
}
