//! SpaceCraft.WaterHandler.IsUnderWater: a position is underwater when it lies inside one of
//! the planet's water boxes, once terraformation has started raising the water. Water boxes
//! move with terraformation (WorldUnitPositioning); the dump holds each box at dump time and
//! the positioning that moves it. The same boxes stand in for the Water layer's colliders in
//! raycasts (surface_below).

use std::collections::HashMap;

use serde_json::Value;

use crate::game::Game;
use crate::geometry::{conj, mul, q, rotate, sub, v3};

#[derive(Clone, Debug)]
struct Volume {
    /// Box in the moving root's local space: center, half size, rotation, scale.
    local_center: [f32; 3],
    half: [f32; 3],
    local_rotation: [f32; 4],
    root_position: [f32; 3],
    root_rotation: [f32; 4],
    positioning: Value,
    active: bool,
    activators: Value,
    /// WaterVolume.ignoreForEnergyDetection: not water for IsUnderWater, still on the Water layer.
    ignored: bool,
}

#[derive(Default)]
pub struct Water {
    /// Planet id -> its water volumes (loaded from scenes/<planet>.json).
    planets: HashMap<String, Vec<Volume>>,
}

fn load(game: &Game, planet: &str) -> Vec<Volume> {
    let path = format!("{}/scenes/{planet}.json", game.data_dir);
    let Ok(text) = std::fs::read_to_string(&path) else { return Vec::new() };
    let Ok(scene) = serde_json::from_str::<Value>(&text) else { return Vec::new() };
    let mut out = Vec::new();
    for v in scene["waterVolumes"].as_array().into_iter().flatten() {
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
        });
    }
    out
}

/// A scene object's state when its scene loads: its HideDependingOnTerraStage components (on it
/// and its parents, "activators" in the dump) decide it once. Without activators (an older
/// dump) the state at dump time stands.
pub fn shown_at_load(game: &Game, object: &Value) -> bool {
    let Some(activators) = object["activators"].as_array() else { return object["active"].as_bool().unwrap_or(true) };
    if activators.is_empty() {
        return object["active"].as_bool().unwrap_or(true);
    }
    !activators.iter().filter(|a| a["type"] == "HideDependingOnTerraStage").any(|a| {
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

/// The current planet's water volumes.
fn volumes(game: &mut Game) -> Vec<Volume> {
    let planet = game.planet_id().to_string();
    if !game.ext.water.planets.contains_key(&planet) {
        let volumes = load(game, &planet);
        game.ext.water.planets.insert(planet.clone(), volumes);
    }
    game.ext.water.planets[&planet].clone()
}

/// A volume's box now: world center and rotation; None while the volume is hidden.
fn box_now(game: &Game, v: &Volume) -> Option<([f32; 3], [f32; 4])> {
    let root = root_now(game, v)?;
    let c = rotate(v.root_rotation, v.local_center);
    Some(([root[0] + c[0], root[1] + c[1], root[2] + c[2]], mul(v.root_rotation, v.local_rotation)))
}

/// WaterHandler.IsUnderWater on the current planet.
pub fn is_under_water(game: &mut Game, position: [f32; 3]) -> bool {
    let volumes: Vec<Volume> = volumes(game).into_iter().filter(|v| !v.ignored).collect();
    // _hasWaterStartedToRise: past the earliest stage any volume starts rising at.
    let earliest = volumes.iter().filter_map(|v| stage_value(game, &v.positioning["startTerraformStage"])).fold(f64::MAX, f64::min);
    if crate::units::value(game, crate::units::Kind::Terraformation, 0) <= earliest {
        return false;
    }
    volumes.iter().any(|v| {
        let Some((center, rotation)) = box_now(game, v) else { return false };
        let local = rotate(conj(rotation), sub(position, center));
        (0..3).all(|i| local[i].abs() <= v.half[i])
    })
}

/// Physics.Raycast straight down from `origin` against the Water layer, within `distance`: the
/// height of the first water surface hit. A ray that starts inside a box does not hit it.
pub fn surface_below(game: &mut Game, origin: [f32; 3], distance: f32) -> Option<f32> {
    let volumes = volumes(game);
    let mut nearest: Option<f32> = None;
    for v in &volumes {
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
    fn rotation_round_trip() {
        let r = crate::geometry::from_euler_y(90.0);
        let v = rotate(r, [1.0, 0.0, 0.0]);
        assert!((v[2] + 1.0).abs() < 1e-5 && v[0].abs() < 1e-5, "{v:?}");
        let back = rotate(conj(r), v);
        assert!((back[0] - 1.0).abs() < 1e-5);
    }
}
