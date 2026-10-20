//! SpaceCraft.SceneInterpolationHandler, server side: moving scene parts (elevators) keep their
//! progress in a world object (growth 0-100, setting = direction while moving); the server
//! starts a move, tells every client its start time, and advances the world object.

use indexmap::IndexMap;
use serde_json::json;

use crate::clock::GameTime;
use crate::game::{Args, Game};
use crate::net::Target;

const EPSILON: f32 = 1.1920929E-07;
/// What a client may start: a scene part (an id of the scene's), at a speed that gets somewhere
/// (the slowest of the game's takes minutes; 0 or a speed of 1e-30 never ends), and no more moves
/// at once than a scene has parts that move. Each running move costs every frame, a message to all
/// the clients and a saved world object.
const MIN_SPEED: f32 = 1e-4;
const MAX_ACTIVE: usize = 1024;

#[derive(serde::Serialize, serde::Deserialize)]
struct Interpolation {
    speed: f32,
    direction: i32,
    start_value: f32,
    start_time: GameTime,
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
                let args = [json!(wo), json!(i.start_value), json!(i.speed), json!(i.direction), json!(i.start_time.f32())];
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
    // Scene world objects have positive ids below the players'; 0 would make a fresh one.
    if wo <= 0 || !crate::model::is_from_scene(wo) || speed.is_nan() || speed < MIN_SPEED {
        return;
    }
    if let Some(i) = game.ext.animals.interpolation.active.get_mut(&wo) {
        if speed > i.speed {
            i.speed = speed;
        }
        return;
    }
    if game.ext.animals.interpolation.active.len() >= MAX_ACTIVE {
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
    game.set_save_state(wo, true);
    game.net.rpc_scene(Target::All, "SceneInterpolationHandler.StartInterpolationClientRpc", &[json!(wo), json!(current), json!(speed), json!(direction), json!(time.f32())]);
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::testkit::*;

    const ELEVATOR: i32 = 101_700_000;

    fn step(g: &mut Game, seconds: f32) {
        g.time += seconds;
        frame(g);
    }

    /// An elevator (a scene interpolator) started by a client moves for everyone from the same
    /// start time; a client arriving mid-way hears the running move, one arriving later the end
    /// position; the save keeps the progress.
    #[test]
    fn scene_interpolators_move_for_everyone_and_late_joiners_catch_up() {
        let Some((mut g, _)) = world() else { return };
        g.time = crate::clock::GameTime::at(10.0);
        send_scene(&mut g, 1, "SceneInterpolationHandler.StartInterpolationServerRpc", &[json!(ELEVATOR), json!(0.25), json!(0)]);
        let start = sent(&g, "SceneInterpolationHandler.StartInterpolationClientRpc");
        assert_eq!(start.into_iter().map(|s| (s.to, s.args)).collect::<Vec<_>>(), vec![(Target::All, vec![json!(ELEVATOR), json!(0.0), json!(0.25), json!(1), json!(10.0)])], "at 0: up");
        // Faster asked while running: the speed goes up, nothing else.
        send_scene(&mut g, 1, "SceneInterpolationHandler.StartInterpolationServerRpc", &[json!(ELEVATOR), json!(0.5), json!(1)]);
        send_scene(&mut g, 1, "SceneInterpolationHandler.StartInterpolationServerRpc", &[json!(ELEVATOR), json!(0.1), json!(1)]);
        assert_eq!(g.ext.animals.interpolation.active[&ELEVATOR].speed, 0.5);
        step(&mut g, 1.0);
        assert_eq!(g.wos[&ELEVATOR].growth, 50.0);
        join(&mut g, 2, "Bob");
        g.net.out.clear();
        send_scene(&mut g, 2, "SceneInterpolationHandler.RegisterInterpolatorServerRpc", &[json!(ELEVATOR), json!(0.5)]);
        assert_eq!(sent(&g, "SceneInterpolationHandler.StartInterpolationClientRpc").into_iter().map(|s| (s.to, s.args)).collect::<Vec<_>>(), vec![(Target::Only(2), vec![json!(ELEVATOR), json!(0.0), json!(0.5), json!(1), json!(10.0)])]);
        step(&mut g, 1.5);
        assert_eq!((g.wos[&ELEVATOR].growth, g.wos[&ELEVATOR].setting), (100.0, 0), "arrived: no direction");
        assert!(g.ext.animals.interpolation.active.is_empty());
        let saved = saved_object(&mut g, ELEVATOR).unwrap();
        assert_eq!(saved["grwth"], json!(100));
        let mut g = reload(&mut g);
        join(&mut g, 3, "Carl");
        g.net.out.clear();
        send_scene(&mut g, 3, "SceneInterpolationHandler.RegisterInterpolatorServerRpc", &[json!(ELEVATOR), json!(0.5)]);
        assert_eq!(one(&g, "SceneInterpolationHandler.SetInitialPositionToEndClientRpc"), vec![json!(ELEVATOR)]);
        // Up again from the top: no move; down (direction 0 above 0) it goes.
        send_scene(&mut g, 3, "SceneInterpolationHandler.StartInterpolationServerRpc", &[json!(ELEVATOR), json!(0.5), json!(1)]);
        assert!(g.ext.animals.interpolation.active.is_empty());
        send_scene(&mut g, 3, "SceneInterpolationHandler.StartInterpolationServerRpc", &[json!(ELEVATOR), json!(0.5), json!(0)]);
        assert_eq!(g.ext.animals.interpolation.active[&ELEVATOR].direction, -1);
    }

    /// A client cannot keep moves running without end: each costs every frame, a message to everyone
    /// and a saved world object. Only scene ids, only a speed that gets somewhere, only so many.
    #[test]
    fn a_client_cannot_start_moves_without_end() {
        let Some((mut g, _)) = world() else { return };
        for (id, speed) in [(200_000_000, 0.5), (i32::MAX, 0.5), (ELEVATOR, 0.0), (ELEVATOR, 1e-30), (ELEVATOR, -1.0)] {
            send_scene(&mut g, 1, "SceneInterpolationHandler.StartInterpolationServerRpc", &[json!(id), json!(speed), json!(1)]);
        }
        assert!(g.ext.animals.interpolation.active.is_empty(), "no scene id, no speed, no move");
        let before = g.wos.len();
        for i in 0..MAX_ACTIVE as i32 + 100 {
            send_scene(&mut g, 1, "SceneInterpolationHandler.StartInterpolationServerRpc", &[json!(101_000_000 + i), json!(0.0002), json!(1)]);
            if i % 16 == 15 {
                g.time += 0.2;
                g.tick();
            }
        }
        for _ in 0..200 {
            g.time += 0.2;
            g.tick();
        }
        assert_eq!(g.ext.animals.interpolation.active.len(), MAX_ACTIVE);
        assert_eq!(g.wos.len(), before + MAX_ACTIVE);
        // The ones running can still be sped up.
        send_scene(&mut g, 1, "SceneInterpolationHandler.StartInterpolationServerRpc", &[json!(101_000_000), json!(0.5), json!(1)]);
        g.time += 0.2;
        g.tick();
        assert_eq!(g.ext.animals.interpolation.active[&101_000_000].speed, 0.5);
    }

    /// A move the save left running (its setting) starts again when a client checks it, as in
    /// the game only where the rule lets a move start: down from above 0 (an interrupted move
    /// up stays, as the game's does).
    #[test]
    fn interrupted_moves_go_on_when_a_client_checks_them() {
        let Some((mut g, _)) = world() else { return };
        g.create_world_object(g.data.singletons["SceneInterpolationHandler"]["_interpolationGroup"].as_str().and_then(|r| g.data.group_of_ref(r)).unwrap(), ELEVATOR, true);
        g.wos.get_mut(&ELEVATOR).unwrap().growth = 100.0;
        send_scene(&mut g, 1, "SceneInterpolationHandler.StartInterpolationServerRpc", &[json!(ELEVATOR), json!(0.25), json!(-1)]);
        step(&mut g, 2.0);
        let mut g = reload(&mut g);
        join(&mut g, 1, "Ann");
        assert_eq!((g.wos[&ELEVATOR].setting, g.wos[&ELEVATOR].growth), (-1, 50.0));
        assert!(g.ext.animals.interpolation.active.is_empty());
        g.net.out.clear();
        send_scene(&mut g, 1, "SceneInterpolationHandler.CheckInterpolatorActiveServerRpc", &[json!(ELEVATOR), json!(0.25)]);
        assert_eq!(one(&g, "SceneInterpolationHandler.StartInterpolationClientRpc")[1..4], [json!(0.5), json!(0.25), json!(-1)], "on down from where it stopped");
        // Up from half-way: the game's rule refuses it.
        g.ext.animals.interpolation.active.clear();
        g.wos.get_mut(&ELEVATOR).unwrap().setting = 1;
        g.net.out.clear();
        send_scene(&mut g, 1, "SceneInterpolationHandler.CheckInterpolatorActiveServerRpc", &[json!(ELEVATOR), json!(0.25)]);
        send_scene(&mut g, 1, "SceneInterpolationHandler.RegisterInterpolatorServerRpc", &[json!(ELEVATOR), json!(0.25)]);
        // Nothing to check, or no world object id: nothing.
        send_scene(&mut g, 1, "SceneInterpolationHandler.CheckInterpolatorActiveServerRpc", &[json!(ELEVATOR + 1), json!(0.25)]);
        send_scene(&mut g, 1, "SceneInterpolationHandler.StartInterpolationServerRpc", &[json!(0), json!(0.25), json!(1)]);
        assert!(g.net.out.is_empty());
    }
}
