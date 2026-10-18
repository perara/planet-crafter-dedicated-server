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

use crate::game::Game;
use crate::idmap::{FxHashMap, FxHashSet};

#[derive(Default)]
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
    pending: Vec<(f32, Step)>,
    next_scan: f32,
}

#[derive(Clone, Copy)]
struct Explosive {
    at_state: i32,
    wait: f32,
    lifetime: f32,
    radius: f32,
    ty: u8,
}

#[derive(Clone, Copy)]
struct Explodable {
    lifetime: f32,
    ty: u8,
    drop: bool,
}

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
    let f = |c: &serde_json::Value, k: &str| c[k].as_f64().unwrap_or(0.0) as f32;
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
}

/// Restarts an explosive's countdown (StopAllCoroutines, StartCoroutine(ProcessExplosion)).
fn arm(e: &mut Explode, wo: i32, net: u64, due: f32) {
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
    // ponytail: scans every world object 10 times a second; keep a set of dropped explosives
    // if worlds grow large enough for this to show.
    if now >= game.ext.meteo.explode.next_scan {
        game.ext.meteo.explode.next_scan = now + 0.1;
        scan(game);
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
            g.time = t;
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
}
