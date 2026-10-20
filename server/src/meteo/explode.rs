//! SpaceCraft.Explode and Exploded, server side. An explosive (a dropped item with an Explode
//! component) that a client arms (SettingProxy setting = _explodeAtState) goes off after its timer:
//! its world object is destroyed, what its blast reaches explodes with it (Exploded objects are
//! removed after their lifetime, other explosives follow within 0.25 s), and its network object
//! despawns two lifetimes later. Damage, shake and effects are the clients' own.
//!
//! Approximations: Physics.OverlapSphere becomes "world objects with a network object whose
//! position is within the radius" (no colliders here; layer masks are not checked), and objects
//! of the planet's scene are never reached (the explodable groups are not among them).

use std::collections::{HashMap, HashSet};

use crate::clock::GameTime;
use crate::game::Game;
use crate::idmap::{FxHashMap, FxHashSet};

#[derive(Default, serde::Serialize, serde::Deserialize)]
pub struct Explode {
    /// Explode / Exploded parameters by group.
    explosives: FxHashMap<usize, Explosive>,
    explodables: HashMap<usize, Explodable>,
    /// Explosives in the world and their last seen setting: SettingProxy.OnSettingUpdate only
    /// reaches Explode for changes after the object spawned.
    seen: FxHashMap<i32, i32>,
    /// Explode._exploded.
    exploded: HashSet<i32>,
    /// Explode._explodedSet: what any blast already reached.
    hit: HashSet<i32>,
    pending: Vec<(GameTime, Step)>,
    next_scan: GameTime,
    /// How many world objects are of an explosive group and have a network object: the only ones
    /// `scan` looks at, so it does not run (and costs nothing) while there are none. Derived, not
    /// saved: `recount` makes it at load and after a hot update, and `net_changed` keeps it at every
    /// place a world object gains or loses its network object, is replaced or removed. In tests
    /// every scan time checks it against the world (`check`).
    #[serde(skip)]
    live: usize,
}

#[derive(Clone, Copy, serde::Serialize, serde::Deserialize)]
struct Explosive {
    at_state: i32,
    wait: f32,
    lifetime: f32,
    radius: f32,
    ty: u8,
}

#[derive(Clone, Copy, serde::Serialize, serde::Deserialize)]
struct Explodable {
    lifetime: f32,
    ty: u8,
    drop: bool,
}

#[derive(serde::Serialize, serde::Deserialize)]
enum Step {
    /// The explosive goes off, if this network object still stands for it (not picked up).
    GoOff(i32, u64),
    /// An Exploded object's lifetime is over: gone from every client and from the world.
    Remove(i32),
    /// Explode's NetworkObject.Despawn.
    Despawn(u64),
}

/// DataConfig.ExplosiveTypes.
fn explosive_type(name: Option<&str>) -> u8 {
    match name {
        Some("Default") => 2,
        Some("Toxic") => 4,
        _ => 0,
    }
}

pub fn init(game: &mut Game) {
    let f = |c: &crate::doc::Doc, k: &str| c[k].as_f64().unwrap_or(0.0) as f32;
    let mut e = Explode::default();
    for (i, g) in game.data.groups.iter().enumerate() {
        if let Some(c) = game.data.prefab_components(&g.id, "Explode").first() {
            let at_state = c["_explodeAtState"].as_i64().unwrap_or(3) as i32;
            e.explosives.insert(i, Explosive { at_state, wait: f(c, "_explosionWaitTimer"), lifetime: f(c, "_explosionLifeTime"), radius: f(c, "_explosionRadius"), ty: explosive_type(c["_explositionType"].as_str()) });
        }
        if let Some(c) = game.data.prefab_components(&g.id, "Exploded").first() {
            e.explodables.insert(i, Explodable { lifetime: f(c, "_explosionLifeTime"), ty: explosive_type(c["_explosiveType"].as_str()), drop: c["_instanciateAndDropGroup"].as_bool().unwrap_or(false) });
        }
    }
    game.ext.meteo.explode = e;
    recount(game);
}

/// `live` from the world objects (a load, or the game rebuilt from a snapshot).
pub fn recount(game: &mut Game) {
    let e = &game.ext.meteo.explode;
    let live = game.wos.values().filter(|w| w.net.is_some() && e.explosives.contains_key(&w.group)).count();
    game.ext.meteo.explode.live = live;
}

/// A world object of `group` had a network object (`had`) and has one (`has`) now, or is gone
/// (`has` false): the count of explosives the scan must look at follows.
pub fn net_changed(game: &mut Game, group: usize, had: bool, has: bool) {
    let e = &mut game.ext.meteo.explode;
    if had != has && e.explosives.contains_key(&group) {
        e.live = if has { e.live + 1 } else { e.live.saturating_sub(1) };
    }
}

/// Restarts an explosive's countdown (StopAllCoroutines, StartCoroutine(ProcessExplosion)).
fn arm(e: &mut Explode, wo: i32, net: u64, due: GameTime) {
    e.pending.retain(|(_, s)| !matches!(s, Step::GoOff(w, _) if *w == wo));
    e.pending.push((due, Step::GoOff(wo, net)));
}

/// Settings that changed to the explosive's state arm it.
fn scan(game: &mut Game) {
    let now = game.time;
    let e = &mut game.ext.meteo.explode;
    let current: Vec<(i32, i32, u64, Explosive)> = game.wos.values().filter_map(|w| Some((w.id, w.setting, w.net?, *e.explosives.get(&w.group)?))).collect();
    let mut present = FxHashSet::default();
    for (id, setting, net, x) in current {
        present.insert(id);
        let previous = e.seen.insert(id, setting);
        if previous.is_some_and(|p| p != setting) && setting == x.at_state && !e.exploded.contains(&id) {
            arm(e, id, net, now + x.wait);
        }
    }
    e.seen.retain(|id, _| present.contains(id));
}

pub fn tick(game: &mut Game) {
    let now = game.time;
    if now >= game.ext.meteo.explode.next_scan {
        game.ext.meteo.explode.next_scan = now + 0.1;
        #[cfg(test)]
        check(game);
        // ponytail: with an explosive about, every world object is still looked at 10 times a
        // second; keep their ids in slot order if a world with many bombs lying about needs it.
        if game.ext.meteo.explode.live > 0 {
            scan(game);
        } else {
            // What a scan with no candidate leaves: nothing seen.
            game.ext.meteo.explode.seen.clear();
        }
    }
    let e = &mut game.ext.meteo.explode;
    if e.pending.iter().all(|(t, _)| *t > now) {
        return;
    }
    let (due, waiting): (Vec<_>, Vec<_>) = std::mem::take(&mut e.pending).into_iter().partition(|(t, _)| *t <= now);
    e.pending = waiting;
    for (_, step) in due {
        match step {
            Step::GoOff(wo, net) => go_off(game, wo, net),
            Step::Remove(wo) => {
                game.destroy_go_on_all_clients(wo, 0.0);
                game.destroy_world_object(wo, false);
            }
            Step::Despawn(net) => game.net.despawn(net),
        }
    }
}

/// Explode.ProcessExplosion after the countdown, server side.
fn go_off(game: &mut Game, wo: i32, net: u64) {
    let Some(w) = game.wos.get(&wo).filter(|w| w.net == Some(net)) else { return };
    let (position, x) = (w.position, game.ext.meteo.explode.explosives[&w.group]);
    let planet = game.planet_hash();
    game.ext.meteo.explode.exploded.insert(wo);
    game.ext.meteo.explode.hit.insert(wo);
    // Its network object stays for the blast's lifetime (Step::Despawn below).
    game.set_wo_net(wo, None);
    game.destroy_world_object(wo, false);
    let reached: Vec<(i32, usize, u64)> = game
        .wos
        .values()
        .filter(|t| t.planet_hash() == planet && t.position.iter().zip(position).map(|(a, b)| (a - b) * (a - b)).sum::<f32>().sqrt() <= x.radius)
        .filter_map(|t| Some((t.id, t.group, t.net?)))
        .collect();
    let now = game.time;
    for (target, group, target_net) in reached {
        if !game.ext.meteo.explode.hit.insert(target) {
            continue;
        }
        let e = &game.ext.meteo.explode;
        if e.explosives.get(&group).is_some_and(|o| o.ty & x.ty == o.ty) && !e.exploded.contains(&target) {
            let delay = game.rng.rangef(0.0, 0.25);
            arm(&mut game.ext.meteo.explode, target, target_net, now + delay);
        }
        let explodable = game.ext.meteo.explode.explodables.get(&group).copied();
        if let Some(d) = explodable.filter(|d| d.ty & x.ty == d.ty) {
            exploded(game, target, d);
        }
    }
    game.ext.meteo.explode.pending.push((now + 2.0 * x.lifetime, Step::Despawn(net)));
}

/// Exploded.ProcessExplosion, server side.
fn exploded(game: &mut Game, target: i32, d: Explodable) {
    let Some(w) = game.wos.get_mut(&target) else { return };
    if !w.dont_save {
        w.dont_save = true;
        if d.drop {
            let (group, p) = (w.group, w.position);
            game.create_and_drop_on_floor(group, [p[0], p[1] + 1.0, p[2]], 0.0, true, true, 0.0, 0);
        }
    }
    game.ext.meteo.explode.pending.push((game.time + d.lifetime, Step::Remove(target)));
}

/// Whether the tests compare `live` with the world at every scan time (the benchmarks turn it off:
/// it looks at every world object, which is what `live` saves).
#[cfg(test)]
pub static CHECK_LIVE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// `live` is what a scan of the world would find.
#[cfg(test)]
fn check(game: &Game) {
    if CHECK_LIVE.load(std::sync::atomic::Ordering::Relaxed) {
        let e = &game.ext.meteo.explode;
        let found = game.wos.values().filter(|w| w.net.is_some() && e.explosives.contains_key(&w.group)).count();
        assert_eq!(e.live, found, "explode::live drifted from the world: a place that sets a world object's net, group or presence does not tell explode");
    }
}

#[cfg(test)]
mod tests {
    use crate::meteo::tests::game;

    #[test]
    fn an_armed_explosive_goes_off_and_takes_rocks_with_it() {
        let Some(mut g) = game() else { return };
        let (explosive, rock) = (g.data.group("Explosive").unwrap(), g.data.group("RockExplodable").unwrap());
        let planet = g.planet_hash();
        let place = |g: &mut crate::game::Game, group: usize, p: [f32; 3]| {
            let id = g.create_world_object(group, 0, true);
            let w = g.wos.get_mut(&id).unwrap();
            w.position = p;
            w.planet = planet;
            g.instantiate(id, false, 0);
            id
        };
        let bomb = place(&mut g, explosive, [10.0, 0.0, 10.0]);
        let near = place(&mut g, rock, [12.0, 0.0, 10.0]);
        let far = place(&mut g, rock, [30.0, 0.0, 10.0]);
        let bomb_net = g.wos[&bomb].net.unwrap();
        let at = |g: &mut crate::game::Game, t: f32| {
            g.time = crate::clock::GameTime::at(f64::from(t));
            super::tick(g);
        };
        at(&mut g, 1.0);
        g.wos.get_mut(&bomb).unwrap().setting = 3;
        at(&mut g, 1.2);
        at(&mut g, 6.0);
        assert!(g.wos.contains_key(&bomb), "5 s countdown");
        at(&mut g, 6.3);
        assert!(!g.wos.contains_key(&bomb) && g.net.objects.contains_key(&bomb_net));
        assert!(g.wos[&near].dont_save && !g.wos[&far].dont_save);
        at(&mut g, 16.4);
        assert!(!g.wos.contains_key(&near) && g.wos.contains_key(&far));
        at(&mut g, 26.4);
        assert!(!g.net.objects.contains_key(&bomb_net), "despawned two lifetimes later");
    }

    /// The count that lets the scan be skipped follows every way an explosive gains or loses its
    /// network object: random worlds of bombs, rocks and iron that are made, dropped, picked up,
    /// destroyed, armed, left to go off, and rebuilt from a snapshot (a hot update), with the count
    /// compared to the world after every step (and at every scan time by `check`). And the bombs go
    /// off in slot order, as they did when every scan looked at the whole world.
    #[test]
    fn the_count_of_live_explosives_follows_the_world() {
        let Some(mut g) = game() else { return };
        let groups = [g.data.group("Explosive").unwrap(), g.data.group("RockExplodable").unwrap(), g.data.group("Iron").unwrap()];
        let planet = g.planet_hash();
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut roll = move |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % n
        };
        let found = |g: &crate::game::Game| g.wos.values().filter(|w| w.net.is_some() && g.ext.meteo.explode.explosives.contains_key(&w.group)).count();
        let (mut ids, mut now, mut restored) = (Vec::new(), 1.0f64, 0);
        for step in 0..1500 {
            match roll(9) {
                0 | 1 => {
                    let id = g.create_world_object(groups[roll(3) as usize], 0, true);
                    let w = g.wos.get_mut(&id).unwrap();
                    (w.position, w.planet) = ([10.0 + roll(40) as f32, 0.0, 10.0 + roll(40) as f32], planet);
                    ids.push(id);
                    if roll(10) < 7 {
                        g.instantiate(id, false, 0);
                    }
                }
                2 if !ids.is_empty() => {
                    g.instantiate(ids[roll(ids.len() as u64) as usize], false, 0);
                }
                3 if !ids.is_empty() => g.destroy_go_on_all_clients(ids[roll(ids.len() as u64) as usize], 0.0),
                4 if !ids.is_empty() => g.destroy_world_object(ids[roll(ids.len() as u64) as usize], false),
                5 if !ids.is_empty() => {
                    if let Some(w) = g.wos.get_mut(&ids[roll(ids.len() as u64) as usize]) {
                        w.setting = 3;
                    }
                }
                // A world object created again under an id that is taken: the old one is replaced.
                6 if !ids.is_empty() => {
                    let id = ids[roll(ids.len() as u64) as usize];
                    g.create_world_object(groups[roll(3) as usize], id, true);
                }
                7 => {
                    now += roll(40) as f64 / 10.0;
                    g.time = crate::clock::GameTime::at(now);
                    super::tick(&mut g);
                }
                8 if step % 50 == 0 => {
                    let text = crate::handoff::snapshot(&mut g).unwrap();
                    let dir = crate::test_data().unwrap();
                    let (n, d, s) = crate::tables(dir);
                    let scenes = crate::scene_tables(dir, &n, &d);
                    g = crate::handoff::restore(n, d, s, &text, scenes).unwrap();
                    restored += 1;
                }
                _ => {}
            }
            assert_eq!(g.ext.meteo.explode.live, found(&g), "after step {step}");
            // The set a scan of what stands in the scene looks at holds every world object that has a network object.
            assert!(g.wos.values().filter(|w| w.net.is_some()).all(|w| g.net_wos.contains(&w.id)), "net_wos after step {step}");
        }
        assert!(restored >= 3 && g.ext.meteo.explode.live <= g.wos.len());
    }

    /// Armed together, bombs go off in the order of their slots in the world, whether or not the
    /// scan was skipped on the way (it is while no explosive stands, and runs when one does).
    #[test]
    fn bombs_armed_in_one_scan_go_off_in_slot_order() {
        let Some(mut g) = game() else { return };
        let explosive = g.data.group("Explosive").unwrap();
        let planet = g.planet_hash();
        let at = |g: &mut crate::game::Game, t: f64| {
            g.time = crate::clock::GameTime::at(t);
            super::tick(g);
        };
        // A long quiet time with none about: the scan is skipped.
        for t in 1..50 {
            at(&mut g, f64::from(t) * 0.1);
        }
        assert_eq!(g.ext.meteo.explode.live, 0);
        // Three bombs dropped; the third takes the slot a filler freed before the others, so that
        // slot order is not the order of their ids.
        let iron = g.data.group("Iron").unwrap();
        let filler = g.create_world_object(iron, 0, true);
        let mut bombs = Vec::new();
        for i in 0..3 {
            if i == 2 {
                g.destroy_world_object(filler, false);
            }
            let id = g.create_world_object(explosive, 0, true);
            let w = g.wos.get_mut(&id).unwrap();
            (w.position, w.planet) = ([10.0 + 40.0 * i as f32, 0.0, 10.0], planet);
            g.instantiate(id, false, 0);
            bombs.push(id);
        }
        assert_eq!(g.ext.meteo.explode.live, 3);
        at(&mut g, 6.0);
        for id in &bombs {
            g.wos.get_mut(id).unwrap().setting = 3;
        }
        at(&mut g, 6.2);
        let order: Vec<(f64, i32)> = g.ext.meteo.explode.pending.iter().filter_map(|(t, s)| matches!(s, super::Step::GoOff(..)).then(|| (t.secs(), match s { super::Step::GoOff(w, _) => *w, _ => 0 }))).collect();
        let slots: Vec<i32> = g.wos.keys().copied().filter(|id| bombs.contains(id)).collect();
        assert_eq!(order.iter().map(|(_, w)| *w).collect::<Vec<_>>(), slots, "armed in slot order");
        assert_eq!(slots.first(), bombs.last(), "the last bomb has the first slot");
    }
}
