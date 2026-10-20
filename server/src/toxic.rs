//! SpaceCraft.ToxicAreaHandler and ToxicArea, server side. Toxicity (a DLC planet) is covered in
//! toxic goo that players clean up by picking it up; the goo is made of scene world objects, each
//! with a fixed id, and a ToxicArea lists the ones it answers for. An area's progress is the
//! share of them already picked up (or destroyed): a player standing in an area gets toxic
//! (PlayerGaugesHandler.GaugesTick reads `tracked_area_progress`) until it is clean.
//!
//! What the server does:
//! - the areas of the planet's scenes (scenes/<planet>.json "toxicAreas"): `ToxicArea.Start`
//!   registers each under the integer part of its squared distance from the origin, and counts
//!   the scene objects the world already has as cleaned;
//! - a client that walks into an area (its own trigger) registers for the area's progress
//!   (`RegisterToAreaToxicProgressServerRpc`: the gauge reads it) and, to be told when it is
//!   clean, for its end (`RegisterToAreaToxicFinishedServerRpc`); the server answers with the
//!   two client calls `UpdateAreaToxicProgressClientRpc` (whenever the progress moved by more
//!   than 1 %) and `UpdateAreaToxicFinishedClientRpc` (once, when it reached 1);
//! - `scene::on_world_object_from_scene_retrieved` / `_destroyed` tell the areas which goo went.
//!
//! The volumes (`ToxicAreaVolumes`), the decal and the map marker are the client's pictures of
//! these two calls; nothing is saved of the areas: the cleaned goo is the world objects the save
//! holds. A player's toxic gauge is saved with the player (gauges.rs).
//!
//! Where the port differs from the game, because the server has no scene:
//! - the host game has the areas of the sectors loaded around its own player; every area of the
//!   planet's scenes is here (a remote player far from a host player would not be hurt there).
//! - the key is computed from the area's position in the dump, in single precision as the game
//!   does; a client whose own key differs by a rounding step (at most `KEY_SLACK`) is taken to
//!   mean the area that is that near.

use serde_json::{json, Value};

use crate::game::{Args, Game};
use crate::net::Target;

/// A client's key for an area may be off by this much from the one the dump gives: the position
/// in the dump is composed from the scene's transforms, which Unity does in a way that is not
/// always the same bit for bit; the keys of two areas are thousands apart.
const KEY_SLACK: i32 = 4;

/// What one client may have registered at a time, in each list. A game client registers for the
/// areas it stands in (a few) and, at load, for the end of every area of the planet (the DLC
/// planet has 33, a volume and a decal each); it unregisters as it leaves. Both RPCs are allowed
/// before PlayerReady, and each entry is looked at in every tick of every player, so a client that
/// goes on registering would slow the whole world and fill the snapshot.
const MAX_REGISTRATIONS: usize = 256;

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct Area {
    /// ToxicAreaHandler.GetToxicAreaId: the integer part of the squared length of the position.
    id: i32,
    /// ToxicArea._toxicTimeInSeconds: the most the gauge of a player in it can hold.
    time: f32,
    /// ToxicArea._toxicObjects.Count.
    total: i32,
    /// _toxicObjectIds: the scene objects still there.
    todo: Vec<i32>,
    /// _cleanedObjectCount.
    cleaned: i32,
}

impl Area {
    /// ToxicArea.GetCurrentProgress.
    fn progress(&self) -> f32 {
        self.cleaned as f32 / self.total as f32
    }
}

#[derive(Default, serde::Serialize, serde::Deserialize)]
pub struct State {
    /// The planet the areas below are of (None: not read yet). Not in a snapshot: after a hot update the
    /// areas are read again, the cleaned ones from the world objects as at a start.
    #[serde(skip)]
    planet: Option<usize>,
    #[serde(skip)]
    areas: Vec<Area>,
    /// ToxicAreaHandler._progressUpdates: (area key as the client sent it, client, last progress
    /// sent to it; -1 before the first).
    progress: Vec<(i32, u64, f32)>,
    /// _finishedUpdates: per client, the areas it waits for the end of, with how many times.
    finished: Vec<(u64, Vec<(i32, i32)>)>,
}

/// ToxicAreaHandler.GetToxicAreaId for a position: Mathf.FloorToInt(sqrMagnitude).
fn area_id(position: [f32; 3]) -> i32 {
    (position[0] * position[0] + position[1] * position[1] + position[2] * position[2]).floor() as i32
}

/// The planet's areas as ToxicArea.Start leaves them on a server: a scene object the world has
/// (and saves) is cleaned already, the others are still there. An area whose key is taken is not
/// registered (the game's Dictionary.Add throws before Start does anything else).
fn load(game: &Game) -> Vec<Area> {
    let mut areas: Vec<Area> = Vec::new();
    for v in game.scenes.planets.get(game.planet_id()).into_iter().flat_map(|s| &s.toxic_areas) {
        // ponytail: an area hidden at load never shows later (the Toxicity dump has none hidden).
        if !crate::water::shown_at_load(game, v) {
            continue;
        }
        let position = [0, 1, 2].map(|i| v["position"][i].as_f64().unwrap_or(0.0) as f32);
        let id = area_id(position);
        if areas.iter().any(|a| a.id == id) {
            eprintln!("toxic area at key {id}: another area has it, the game would not register this one");
            continue;
        }
        let ids: Vec<i32> = v["fields"]["_toxicObjects"].as_array().into_iter().flatten().filter_map(|o| o["worldObjectId"].as_i64()).map(|i| i as i32).collect();
        let mut todo = Vec::new();
        let mut cleaned = std::collections::HashSet::new();
        for id in &ids {
            if game.wos.get(id).is_some_and(|w| !w.dont_save) {
                cleaned.insert(*id);
            } else if !todo.contains(id) {
                todo.push(*id);
            }
        }
        areas.push(Area { id, time: v["fields"]["_toxicTimeInSeconds"].as_f64().unwrap_or(0.0) as f32, total: ids.len() as i32, todo, cleaned: cleaned.len() as i32 });
    }
    areas
}

/// The areas of the current planet, read when the world is on it for the first time (a start, a
/// hot update, a travel).
fn ensure(game: &mut Game) {
    if game.ext.toxic.planet != Some(game.planet) {
        let areas = load(game);
        game.ext.toxic.areas = areas;
        game.ext.toxic.planet = Some(game.planet);
    }
}

/// The area a client's key means.
fn find(areas: &[Area], key: i32) -> Option<&Area> {
    areas.iter().find(|a| a.id == key).or_else(|| {
        // abs_diff: a client's key is any number (i32::MIN minus an area's key overflows i32).
        let mut near = areas.iter().filter(|a| a.id.abs_diff(key) <= KEY_SLACK.unsigned_abs());
        near.next().filter(|_| near.next().is_none())
    })
}

/// (progress, toxic time) of the toxic area a client tracks (GetTrackedAreaByClient): the first
/// area it registered for that is in the scene.
pub fn tracked_area_progress(game: &Game, client: u64) -> Option<(f32, f32)> {
    let t = &game.ext.toxic;
    let area = t.progress.iter().filter(|(_, c, _)| *c == client).find_map(|(key, _, _)| find(&t.areas, *key))?;
    Some((area.progress(), area.time))
}

/// ToxicArea.OnWorldObjectFromSceneRetrieved, of every area: a scene object picked up or
/// disabled counts as cleaned once.
pub fn scene_object_removed(game: &mut Game, wo: i32) {
    for a in &mut game.ext.toxic.areas {
        if let Some(i) = a.todo.iter().position(|t| *t == wo) {
            a.todo.swap_remove(i);
            a.cleaned += 1;
        }
    }
}

/// NetworkManager.OnClientDisconnectCallback: what the client had registered goes.
pub fn client_left(game: &mut Game, client: u64) {
    let t = &mut game.ext.toxic;
    t.progress.retain(|(_, c, _)| *c != client);
    t.finished.retain(|(c, _)| *c != client);
}

/// ToxicAreaHandler.Update: tell each registered client of an area's progress and end.
pub fn tick(game: &mut Game) {
    ensure(game);
    let t = &mut game.ext.toxic;
    if t.progress.is_empty() && t.finished.is_empty() {
        return;
    }
    let mut sends: Vec<(u64, &str, Vec<Value>)> = Vec::new();
    for (key, client, last) in &mut t.progress {
        if let Some(area) = find(&t.areas, *key) {
            let progress = area.progress();
            if (progress - *last).abs() > 0.01 {
                sends.push((*client, "ToxicAreaHandler.UpdateAreaToxicProgressClientRpc", vec![json!(*key), json!(area.cleaned)]));
                *last = progress;
            }
        }
    }
    for (client, waits) in &mut t.finished {
        waits.retain(|(key, _)| {
            let done = find(&t.areas, *key).is_some_and(|a| (a.progress() - 1.0).abs() < f32::EPSILON);
            if done {
                sends.push((*client, "ToxicAreaHandler.UpdateAreaToxicFinishedClientRpc", vec![json!(*key)]));
            }
            !done
        });
    }
    t.finished.retain(|(_, waits)| !waits.is_empty());
    for (client, name, args) in sends {
        game.net.rpc_scene(Target::Only(client), name, &args);
    }
}

pub fn rpc(game: &mut Game, client: u64, name: &str, a: &Args) -> bool {
    if !name.starts_with("ToxicAreaHandler.") {
        return false;
    }
    ensure(game);
    match name {
        // GetCurrentPlanetToxicProgressServerRpc: cleaned objects, objects, areas clean, areas.
        "ToxicAreaHandler.GetCurrentPlanetToxicProgressServerRpc" => {
            let areas = &game.ext.toxic.areas;
            let sum = |f: &dyn Fn(&Area) -> i32| areas.iter().map(f).sum::<i32>();
            let args = [json!(sum(&|a| a.cleaned)), json!(sum(&|a| a.total)), json!(sum(&|a| i32::from(a.cleaned == a.total))), json!(areas.len())];
            game.net.rpc_scene(Target::Only(client), "ToxicAreaHandler.GetCurrentPlanetToxicProgressClientRpc", &args);
        }
        "ToxicAreaHandler.RegisterToAreaToxicProgressServerRpc" => {
            if game.ext.toxic.progress.iter().filter(|(_, c, _)| *c == client).count() >= MAX_REGISTRATIONS {
                game.limits.warn(client, "toxic area registrations over the limit, ignored");
            } else {
                game.ext.toxic.progress.push((a.i(0), client, -1.0));
            }
        }
        "ToxicAreaHandler.UnregisterToAreaToxicProgressServerRpc" => {
            let t = &mut game.ext.toxic;
            if let Some(i) = t.progress.iter().position(|(key, c, _)| *key == a.i(0) && *c == client) {
                t.progress.remove(i);
            }
        }
        "ToxicAreaHandler.RegisterToAreaToxicFinishedServerRpc" => {
            let t = &mut game.ext.toxic;
            let i = t.finished.iter().position(|(c, _)| *c == client).unwrap_or_else(|| {
                t.finished.push((client, Vec::new()));
                t.finished.len() - 1
            });
            let waits = &mut t.finished[i].1;
            let mut over = false;
            let room = waits.len() < MAX_REGISTRATIONS;
            match waits.iter_mut().find(|(key, _)| *key == a.i(0)) {
                Some((_, count)) if (*count as usize) < MAX_REGISTRATIONS => *count += 1,
                None if room => waits.push((a.i(0), 1)),
                _ => over = true,
            }
            if over {
                game.limits.warn(client, "toxic area registrations over the limit, ignored");
            }
        }
        "ToxicAreaHandler.UnregisterToAreaToxicFinishedServerRpc" => {
            let waits = game.ext.toxic.finished.iter_mut().find(|(c, _)| *c == client).map(|(_, w)| w);
            if let Some(waits) = waits {
                if let Some(i) = waits.iter().position(|(key, _)| *key == a.i(0)) {
                    if waits[i].1 == 1 {
                        waits.remove(i);
                    } else {
                        waits[i].1 -= 1;
                    }
                }
            }
            // The game keeps the client's empty list; nothing sees the difference.
            game.ext.toxic.finished.retain(|(_, waits)| !waits.is_empty());
        }
        _ => return false,
    }
    true
}

#[cfg(test)]
mod tests {
    use serde_json::{json, Value};

    use super::*;
    use crate::game::Game;
    use crate::net::Target;
    use crate::testkit::*;

    const PROGRESS: &str = "ToxicAreaHandler.UpdateAreaToxicProgressClientRpc";
    const FINISHED: &str = "ToxicAreaHandler.UpdateAreaToxicFinishedClientRpc";

    /// A toxic area of scene goo `ids` at `at`, in the scene of the planet the test world is on
    /// (the tests need no DLC data: the area is the dump's shape, written by hand). Returns the
    /// key a client registers it under.
    fn area(g: &mut Game, at: [f64; 3], time: f64, ids: &[i32]) -> i32 {
        let objects: Vec<Value> = ids.iter().map(|i| json!({"worldObjectId": i, "group": "GroupDataItem:ToxicGoo"})).collect();
        let dump = json!({"position": at, "active": true, "activators": [], "fields": {"_toxicTimeInSeconds": time, "_toxicObjects": objects}});
        let planet = g.planet_id().to_string();
        g.scenes.planets.get_mut(&planet).expect("the planet's scene").toxic_areas.push(dump);
        g.ext.toxic.planet = None;
        area_id(at.map(|v| v as f32))
    }

    fn pick_up(g: &mut Game, id: i32) {
        let (backpack, _) = backpack(g, 1);
        let goo = hash(g, "ToxicGoo");
        send_scene(g, 1, "InventoriesHandler.AddWorldObjectToInventoryServerRpc", &[json!(id), json!(goo), json!(backpack), json!(false)]);
    }

    /// The goo disabled in the scene (a cleaning tool): counts like a pick-up, needs no room.
    fn disable(g: &mut Game, id: i32) {
        let goo = hash(g, "ToxicGoo");
        send_scene(g, 1, "WorldObjectsHandler.DisableWorldObjectFromSceneServerRpc", &[json!(id), json!(goo), json!(false), json!([0.0, 0.0, 0.0])]);
    }

    fn toxic_value(g: &Game, player: u64) -> f32 {
        crate::gauges::values(g, player)[3]
    }

    /// A client (before PlayerReady too) cannot register without end: every entry is looked at in each
    /// tick, for every player, and is in the snapshot.
    #[test]
    fn a_client_cannot_register_for_toxic_areas_without_end() {
        // A whole tick of the game: it runs the RPCs that waited for their turn (a client's budget).
        fn step(g: &mut Game) {
            g.time += 0.2;
            g.tick();
        }
        let Some((mut g, _)) = world() else { return };
        for i in 0..5 * MAX_REGISTRATIONS as i32 {
            for name in ["ToxicAreaHandler.RegisterToAreaToxicProgressServerRpc", "ToxicAreaHandler.RegisterToAreaToxicFinishedServerRpc"] {
                send_scene(&mut g, 1, name, &[json!(i)]);
            }
            if i % 16 == 15 {
                step(&mut g);
            }
        }
        for _ in 0..200 {
            step(&mut g);
        }
        let t = &g.ext.toxic;
        assert_eq!(t.progress.len(), MAX_REGISTRATIONS);
        assert_eq!(t.finished.iter().map(|(_, w)| w.len()).sum::<usize>(), MAX_REGISTRATIONS);
        // The same key again counts up to the limit, not past it; a client that left takes its entries along.
        for _ in 0..2 * MAX_REGISTRATIONS {
            send_scene(&mut g, 1, "ToxicAreaHandler.RegisterToAreaToxicFinishedServerRpc", &[json!(7)]);
            step(&mut g);
        }
        assert!(g.ext.toxic.finished[0].1.iter().all(|(_, n)| *n as usize <= MAX_REGISTRATIONS));
        client_left(&mut g, 1);
        assert!(g.ext.toxic.progress.is_empty() && g.ext.toxic.finished.is_empty());
    }

    /// A planet without toxic areas answers zero progress (as the game does on Prime),
    /// registrations have nothing to report, and nobody is in an area.
    #[test]
    fn planets_without_toxic_areas_answer_zero_progress() {
        let Some((mut g, _)) = world() else { return };
        send_scene(&mut g, 1, "ToxicAreaHandler.GetCurrentPlanetToxicProgressServerRpc", &[]);
        assert_eq!(sent(&g, "ToxicAreaHandler.GetCurrentPlanetToxicProgressClientRpc").into_iter().map(|s| (s.to, s.args)).collect::<Vec<_>>(), vec![(Target::Only(1), vec![json!(0), json!(0), json!(0), json!(0)])]);
        g.net.out.clear();
        for name in ["ToxicAreaHandler.RegisterToAreaToxicProgressServerRpc", "ToxicAreaHandler.UnregisterToAreaToxicProgressServerRpc", "ToxicAreaHandler.RegisterToAreaToxicFinishedServerRpc", "ToxicAreaHandler.UnregisterToAreaToxicFinishedServerRpc"] {
            send_scene(&mut g, 1, name, &[json!(5)]);
        }
        tick(&mut g);
        assert!(sent(&g, PROGRESS).is_empty() && sent(&g, FINISHED).is_empty());
        assert_eq!(tracked_area_progress(&g, 1), None);
    }

    /// A client's key is any number: the area within the slack is found whatever the keys are, and
    /// i32::MIN is no neighbour of the area at the origin (its difference overflowed and read as near).
    #[test]
    fn a_key_far_from_every_area_finds_none_whatever_the_numbers() {
        let area = |id| Area { id, time: 1.0, total: 1, todo: vec![], cleaned: 0 };
        let areas = [area(0), area(1000), area(i32::MAX)];
        for key in [i32::MIN, i32::MIN + 1, -5, -4, 5, 997, 1004, 1005, i32::MAX - 5, i32::MAX - 4] {
            let found = find(&areas, key).map(|a| a.id);
            let near: Vec<i32> = areas.iter().map(|a| a.id).filter(|id| (*id as i64 - key as i64).abs() <= KEY_SLACK as i64).collect();
            assert_eq!(found, if near.len() == 1 { Some(near[0]) } else { None }, "key {key}");
        }
        assert_eq!(find(&areas, i32::MIN).map(|a| a.id), None);
        assert_eq!(find(&areas, 1002).map(|a| a.id), Some(1000));
        assert_eq!(find(&areas, i32::MAX).map(|a| a.id), Some(i32::MAX));
    }

    /// The key is the integer part of the squared length of the position, in single precision.
    #[test]
    fn an_area_is_known_by_the_integer_part_of_its_squared_distance() {
        assert_eq!(area_id([3.0, 4.0, 12.0]), 169);
        assert_eq!(area_id([-395.5, 446.8, -1204.5]), 1_806_870);
        assert_eq!(area_id([0.0, 0.0, 0.0]), 0);
    }

    /// A client that registered gets the area's progress at once and again whenever it moved by
    /// more than 1 %, scene goo picked up (or disabled in the scene) counts once, and the end is
    /// sent once, to the clients that asked for it.
    #[test]
    fn registered_clients_are_told_the_progress_and_the_end_of_an_area() {
        let Some((mut g, _)) = world() else { return };
        let ids: Vec<i32> = (900_001..=900_004).collect();
        let key = area(&mut g, [100.0, 5.0, 200.0], 30.0, &ids);
        send_scene(&mut g, 1, "ToxicAreaHandler.RegisterToAreaToxicProgressServerRpc", &[json!(key)]);
        send_scene(&mut g, 1, "ToxicAreaHandler.RegisterToAreaToxicFinishedServerRpc", &[json!(key)]);
        tick(&mut g);
        assert_eq!(sent(&g, PROGRESS).into_iter().map(|s| (s.to, s.args)).collect::<Vec<_>>(), vec![(Target::Only(1), vec![json!(key), json!(0)])], "the first look");
        assert_eq!(tracked_area_progress(&g, 1), Some((0.0, 30.0)));
        g.net.out.clear();
        tick(&mut g);
        assert!(sent(&g, PROGRESS).is_empty(), "nothing moved");

        pick_up(&mut g, 900_001);
        tick(&mut g);
        assert_eq!(one(&g, PROGRESS), vec![json!(key), json!(1)]);
        assert_eq!(tracked_area_progress(&g, 1), Some((0.25, 30.0)));
        // The same goo again, and goo of no area: nothing counts.
        g.net.out.clear();
        super::scene_object_removed(&mut g, 900_001);
        super::scene_object_removed(&mut g, 5);
        assert_eq!(g.ext.toxic.areas[0].cleaned, 1);

        // Disabled in the scene (a cleaning tool, the debug command) counts as picked up.
        disable(&mut g, 900_002);
        tick(&mut g);
        assert_eq!(one(&g, PROGRESS), vec![json!(key), json!(2)]);

        g.net.out.clear();
        pick_up(&mut g, 900_003);
        pick_up(&mut g, 900_004);
        tick(&mut g);
        assert_eq!(client_rpcs(&g, PROGRESS).into_iter().map(|(_, a)| a).collect::<Vec<_>>(), vec![vec![json!(key), json!(4)]], "one update for the two");
        assert_eq!(sent(&g, FINISHED).into_iter().map(|s| (s.to, s.args)).collect::<Vec<_>>(), vec![(Target::Only(1), vec![json!(key)])]);
        g.net.out.clear();
        tick(&mut g);
        assert!(sent(&g, FINISHED).is_empty() && sent(&g, PROGRESS).is_empty(), "the end is told once");
        assert_eq!(tracked_area_progress(&g, 1), Some((1.0, 30.0)));
    }

    /// DestroyWorldObjectFromScene raises no event in the game: the goo it removes is not
    /// cleaned, and stays uncounted when it is picked up later (it is saved by then).
    #[test]
    fn goo_destroyed_in_the_scene_is_not_counted() {
        let Some((mut g, _)) = world() else { return };
        area(&mut g, [100.0, 5.0, 200.0], 30.0, &[900_001, 900_002]);
        tick(&mut g);
        let goo = hash(&g, "ToxicGoo");
        send_scene(&mut g, 1, "WorldObjectsHandler.DestroyWorldObjectFromSceneServerRpc", &[json!(900_001), json!(goo), json!(0.0)]);
        pick_up(&mut g, 900_001);
        tick(&mut g);
        assert_eq!(g.ext.toxic.areas[0].cleaned, 0);
        pick_up(&mut g, 900_002);
        assert_eq!(g.ext.toxic.areas[0].cleaned, 1);
    }

    /// Scene goo the world holds when the area starts (saved: not "dont save me") is cleaned
    /// already; unsaved goo and goo the world has not met are not. The totals answer the map.
    #[test]
    fn goo_the_world_already_has_is_cleaned_when_the_area_starts() {
        let Some((mut g, _)) = world() else { return };
        let goo = g.data.group("ToxicGoo").unwrap();
        g.create_world_object(goo, 900_001, true);
        g.create_world_object(goo, 900_002, false);
        let key = area(&mut g, [10.0, 0.0, 10.0], 20.0, &[900_001, 900_002, 900_003]);
        let other = area(&mut g, [500.0, 0.0, 500.0], 40.0, &[900_010]);
        g.create_world_object(goo, 900_010, true);
        g.ext.toxic.planet = None;
        send_scene(&mut g, 1, "ToxicAreaHandler.GetCurrentPlanetToxicProgressServerRpc", &[]);
        // cleaned objects, objects, areas clean, areas
        assert_eq!(one(&g, "ToxicAreaHandler.GetCurrentPlanetToxicProgressClientRpc"), vec![json!(2), json!(4), json!(1), json!(2)]);
        send_scene(&mut g, 1, "ToxicAreaHandler.RegisterToAreaToxicFinishedServerRpc", &[json!(other)]);
        send_scene(&mut g, 1, "ToxicAreaHandler.RegisterToAreaToxicProgressServerRpc", &[json!(key)]);
        g.net.out.clear();
        tick(&mut g);
        assert_eq!(sent(&g, FINISHED).len(), 1, "an area that is clean at the start ends at once");
        assert_eq!(one(&g, PROGRESS), vec![json!(key), json!(1)]);
    }

    /// A player in an area gets toxic at the area's pace until it is clean: the gauge fills by
    /// the share of goo left, a toxin filter slows it, the time of the area is its limit, and
    /// out of the area (or once clean) it drains ten times as fast.
    #[test]
    fn a_player_in_an_area_fills_the_toxic_gauge_until_it_is_clean() {
        let Some((mut g, ann)) = world() else { return };
        let ids: Vec<i32> = (900_001..=900_010).collect();
        let key = area(&mut g, [100.0, 5.0, 200.0], 30.0, &ids);
        let gauges = |g: &mut Game, seconds: f32| {
            for _ in 0..(seconds / 0.2).round() as i32 {
                g.time += 0.2;
                crate::gauges::tick(g);
            }
        };
        // Out of every area: nothing to drain.
        gauges(&mut g, 2.0);
        assert_eq!(toxic_value(&g, ann), 0.0);
        send_scene(&mut g, 1, "ToxicAreaHandler.RegisterToAreaToxicProgressServerRpc", &[json!(key)]);
        gauges(&mut g, 5.0);
        let v = toxic_value(&g, ann);
        assert!((v - 5.0).abs() < 0.01, "a gauge unit a second at full goo: {v}");
        assert_eq!(var(&g, ann, "PlayerGaugesHandler", "_toxicMaxValue"), 30.0f32.to_le_bytes());
        // Half of the goo gone: half the pace.
        for id in 900_001..=900_005 {
            disable(&mut g, id);
        }
        gauges(&mut g, 4.0);
        assert!((toxic_value(&g, ann) - (v + 2.0)).abs() < 0.01, "{}", toxic_value(&g, ann));
        // A filter of the equipment cuts the pace by its value (percent).
        let (_, equipment) = backpack(&g, 1);
        let filter = give(&mut g, "ToxinsFilter1", equipment);
        let percent = g.data.groups[g.wos[&filter].group].fields["value"].as_f64().unwrap() as f32;
        assert!(percent > 0.0 && percent < 100.0);
        let before = toxic_value(&g, ann);
        gauges(&mut g, 4.0);
        assert!((toxic_value(&g, ann) - (before + 4.0 * 0.5 * (1.0 - percent / 100.0))).abs() < 0.02, "{} after {before}", toxic_value(&g, ann));
        // The limit is the area's time.
        crate::gauges::values(&g, ann);
        g.net.set(ann, "PlayerGaugesHandler", "_toxicValue", crate::net::enc::float(29.9));
        gauges(&mut g, 2.0);
        assert_eq!(toxic_value(&g, ann), 30.0);
        // Leaving the area: ten units a second off.
        send_scene(&mut g, 1, "ToxicAreaHandler.UnregisterToAreaToxicProgressServerRpc", &[json!(key)]);
        gauges(&mut g, 1.0);
        assert!((toxic_value(&g, ann) - (30.0 - 10.0 * (1.0 + percent / 100.0))).abs() < 0.05, "{}", toxic_value(&g, ann));
        // A clean area is no area: registered or not, the gauge drains.
        send_scene(&mut g, 1, "ToxicAreaHandler.RegisterToAreaToxicProgressServerRpc", &[json!(key)]);
        for id in 900_006..=900_010 {
            disable(&mut g, id);
        }
        let before = toxic_value(&g, ann);
        gauges(&mut g, 0.4);
        assert!(toxic_value(&g, ann) < before || before == 0.0);
        assert_eq!(var(&g, ann, "PlayerGaugesHandler", "_toxicDecreaseValue"), 0.0f32.to_le_bytes());
    }

    /// Registrations count: the same area twice is two entries (one goes with each unregister),
    /// the end is waited for once per registration, and a client that leaves takes its own.
    #[test]
    fn registrations_are_counted_and_go_with_the_client() {
        let Some((mut g, ann)) = world() else { return };
        let key = area(&mut g, [100.0, 5.0, 200.0], 30.0, &[900_001, 900_002]);
        let bob = join(&mut g, 2, "Bob");
        for client in [1, 2] {
            for name in ["ToxicAreaHandler.RegisterToAreaToxicProgressServerRpc", "ToxicAreaHandler.RegisterToAreaToxicProgressServerRpc", "ToxicAreaHandler.RegisterToAreaToxicFinishedServerRpc", "ToxicAreaHandler.RegisterToAreaToxicFinishedServerRpc"] {
                send_scene(&mut g, client, name, &[json!(key)]);
            }
        }
        assert_eq!((g.ext.toxic.progress.len(), g.ext.toxic.finished.len()), (4, 2));
        send_scene(&mut g, 1, "ToxicAreaHandler.UnregisterToAreaToxicProgressServerRpc", &[json!(key)]);
        send_scene(&mut g, 1, "ToxicAreaHandler.UnregisterToAreaToxicFinishedServerRpc", &[json!(key)]);
        assert_eq!(g.ext.toxic.progress.iter().filter(|p| p.1 == 1).count(), 1);
        assert_eq!(g.ext.toxic.finished[0].1, vec![(key, 1)]);
        send_scene(&mut g, 1, "ToxicAreaHandler.UnregisterToAreaToxicFinishedServerRpc", &[json!(key)]);
        send_scene(&mut g, 1, "ToxicAreaHandler.UnregisterToAreaToxicFinishedServerRpc", &[json!(key)]);
        assert_eq!(g.ext.toxic.finished.iter().map(|f| f.0).collect::<Vec<_>>(), vec![2], "no wait left for client 1");
        g.disconnect(2);
        assert_eq!(g.ext.toxic.progress.iter().map(|p| p.1).collect::<Vec<_>>(), vec![1]);
        assert!(g.ext.toxic.finished.is_empty());
        let _ = (ann, bob);
    }

    /// A client's key may be a rounding step off the dump's; the nearest area is meant when it
    /// is the only one that near. Exact keys win; a key near two areas means none.
    #[test]
    fn a_key_a_rounding_step_off_still_means_the_area() {
        let a = |id| Area { id, time: 10.0, total: 2, todo: vec![1, 2], cleaned: 0 };
        let areas = [a(1000), a(1003), a(50_000)];
        assert_eq!(find(&areas, 1003).map(|x| x.id), Some(1003));
        assert_eq!(find(&areas, 1000).map(|x| x.id), Some(1000));
        assert_eq!(find(&areas, 1001).map(|x| x.id), None, "between two");
        assert_eq!(find(&areas, 50_002).map(|x| x.id), Some(50_000));
        assert_eq!(find(&areas, 50_010).map(|x| x.id), None);
    }

    /// The toxic areas read from an install's Toxicity hold together: keys of their own (the game
    /// would not register the second of two), at least one scene object each and none in two
    /// areas, a time to fill a gauge, and a place within the planets' span.
    #[test]
    fn the_toxic_areas_read_from_an_install_hold_together() {
        let Some((g, _)) = world() else { return };
        let grounds = dlc_grounds(&g, "the_toxic_areas_read_from_an_install_hold_together");
        let mut found = 0;
        let mut ids = std::collections::HashSet::new();
        let mut keys = std::collections::HashSet::new();
        for (planet, scene) in grounds {
            for (i, a) in scene.toxic_areas.iter().enumerate() {
                found += 1;
                let position = [0, 1, 2].map(|k| a["position"][k].as_f64().unwrap() as f32);
                assert!(position.iter().all(|c| c.abs() < 20_000.0), "{planet} area {i}: {position:?}");
                assert!(keys.insert(area_id(position)), "{planet} area {i}: its key is another area's");
                assert!(a["fields"]["_toxicTimeInSeconds"].as_f64().unwrap() > 0.0);
                let objects = a["fields"]["_toxicObjects"].as_array().unwrap();
                assert!(!objects.is_empty());
                for o in objects {
                    let id = o["worldObjectId"].as_i64().unwrap();
                    assert!(id > 0 && ids.insert(id), "{planet} area {i}: scene object {id} is not a scene id or is in two areas");
                    assert!(o["group"].as_str().and_then(|r| g.data.group_of_ref(r)).is_some());
                }
            }
        }
        assert!(g.scenes.planets.values().all(|s| s.toxic_areas.is_empty()) || found > 0);
    }

    /// A hot update keeps who registered for what; the areas are read again from the scene and
    /// the world objects, so a goo picked up before the flip is still cleaned after it.
    #[test]
    fn a_hot_update_keeps_the_registrations() {
        let Some((mut g, ann)) = world() else { return };
        let key = area(&mut g, [100.0, 5.0, 200.0], 30.0, &[900_001, 900_002]);
        send_scene(&mut g, 1, "ToxicAreaHandler.RegisterToAreaToxicProgressServerRpc", &[json!(key)]);
        send_scene(&mut g, 1, "ToxicAreaHandler.RegisterToAreaToxicFinishedServerRpc", &[json!(key)]);
        pick_up(&mut g, 900_001);
        tick(&mut g);
        g.net.out.clear();
        let (n, d, s, scenes) = crate::game::test_tables().unwrap();
        let snapshot = crate::handoff::snapshot(&mut g).unwrap();
        let mut h = crate::handoff::restore(n, d, s, &snapshot, scenes).unwrap();
        let planet = h.planet_id().to_string();
        h.scenes.planets.get_mut(&planet).unwrap().toxic_areas = g.scenes.planets[&planet].toxic_areas.clone();
        assert_eq!((h.ext.toxic.progress.clone(), h.ext.toxic.finished.clone()), (g.ext.toxic.progress.clone(), g.ext.toxic.finished.clone()));
        assert_eq!(tracked_area_progress(&h, 1), None, "the areas are read at the first tick");
        tick(&mut h);
        assert_eq!(tracked_area_progress(&h, 1), Some((0.5, 30.0)));
        assert!(sent(&h, PROGRESS).is_empty(), "the client was told 0.5 before the flip");
        let _ = ann;
    }
}
