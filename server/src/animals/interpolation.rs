//! SpaceCraft.SceneInterpolationHandler, server side: moving scene parts (elevators) keep their
//! progress in a world object (growth 0-100, setting = direction while moving); the server
//! starts a move, tells every client its start time, and advances the world object.

use indexmap::IndexMap;
use serde_json::json;

use crate::game::{Args, Game};
use crate::net::Target;

const EPSILON: f32 = 1.1920929E-07;

#[derive(serde::Serialize, serde::Deserialize)]
struct Interpolation {
    speed: f32,
    direction: i32,
    start_value: f32,
    start_time: f32,
}

#[derive(Default, serde::Serialize, serde::Deserialize)]
pub struct State {
    active: IndexMap<i32, Interpolation>,
}

pub fn rpc(game: &mut Game, client: u64, name: &str, a: &Args) -> bool {
    match name {
        "SceneInterpolationHandler.StartInterpolationServerRpc" => start(game, a.i(0), a.f(1), a.i(2)),
        "SceneInterpolationHandler.RegisterInterpolatorServerRpc" => {
            let (wo, speed) = (a.i(0), a.f(1));
            if let Some(i) = game.ext.animals.interpolation.active.get(&wo) {
                let args = [json!(wo), json!(i.start_value), json!(i.speed), json!(i.direction), json!(i.start_time)];
                game.net.rpc_scene(Target::Only(client), "SceneInterpolationHandler.StartInterpolationClientRpc", &args);
                return true;
            }
            match game.wos.get(&wo).map(|w| (w.setting, w.growth)) {
                Some((setting, _)) if setting != 0 => start(game, wo, speed, setting),
                Some((_, growth)) if (growth - 100.0).abs() < EPSILON => {
                    game.net.rpc_scene(Target::Only(client), "SceneInterpolationHandler.SetInitialPositionToEndClientRpc", &[json!(wo)]);
                }
                _ => {}
            }
        }
        "SceneInterpolationHandler.CheckInterpolatorActiveServerRpc" => {
            let wo = a.i(0);
            if let Some(setting) = game.wos.get(&wo).map(|w| w.setting).filter(|s| *s != 0) {
                start(game, wo, a.f(1), setting);
            }
        }
        _ => return false,
    }
    true
}

/// StartInterpolationServerRpc: a running move only speeds up; otherwise a move starts if it
/// leaves its end (up from 0, down from above 0).
fn start(game: &mut Game, wo: i32, speed: f32, direction: i32) {
    // Scene world objects have positive ids; 0 would make a fresh one.
    if wo <= 0 {
        return;
    }
    if let Some(i) = game.ext.animals.interpolation.active.get_mut(&wo) {
        if speed > i.speed {
            i.speed = speed;
        }
        return;
    }
    if !game.wos.contains_key(&wo) {
        let Some(group) = game.data.singletons["SceneInterpolationHandler"]["_interpolationGroup"].as_str().and_then(|r| game.data.group_of_ref(r)) else { return };
        game.create_world_object(group, wo, true);
    }
    let current = game.wos[&wo].growth / 100.0;
    let mut direction = direction;
    if direction == 0 {
        direction = if current > EPSILON { -1 } else { 1 };
    }
    if (direction < 0 && current < EPSILON) || (direction > 0 && current > EPSILON) {
        return;
    }
    let time = game.time;
    game.ext.animals.interpolation.active.insert(wo, Interpolation { speed, direction, start_value: current, start_time: time });
    let w = game.wos.get_mut(&wo).unwrap();
    w.setting = direction;
    w.dont_save = false;
    game.net.rpc_scene(Target::All, "SceneInterpolationHandler.StartInterpolationClientRpc", &[json!(wo), json!(current), json!(speed), json!(direction), json!(time)]);
}

/// LateUpdate on the server: progress into the world object; a finished move clears its setting.
pub fn frame(game: &mut Game) {
    let now = game.time;
    let mut done = Vec::new();
    for (wo, i) in &game.ext.animals.interpolation.active {
        let current = (i.start_value + i.speed * (now - i.start_time) * i.direction as f32).clamp(0.0, 1.0);
        let Some(w) = game.wos.get_mut(wo) else { continue };
        if (current < EPSILON && i.direction == -1) || ((current - 1.0).abs() < EPSILON && i.direction == 1) {
            done.push(*wo);
            w.setting = 0;
        }
        w.growth = current * 100.0;
    }
    for wo in done {
        game.ext.animals.interpolation.active.shift_remove(&wo);
    }
}
